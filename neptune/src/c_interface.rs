// C interface for the garbage collector, C side needs to call
// appropriate functions with a Gc object since Using a static mutable
// object is unsafe in Rust because of "life after main" and
// destructor order.

use gc::*;
use libc::c_int;
use libc::c_void;
use libc::uintptr_t;
use libc;

pub type JlJmpBuf = libc::c_void; // we cannot use long jumps in Rust anyways

// temporary, TODO: reify
pub type JlValue = libc::c_void;
pub type JlTask = libc::c_void;
pub type JlModule = libc::c_void;
pub type GcFrame = libc::c_void;

extern {
    pub fn gc_final_count_page(pg_cnt: usize);
    pub fn jl_gc_wait_for_the_world(); // wait for the world to stop
}

pub extern fn gc_init<'a>(page_size: usize) -> Box<Gc<'a>> {
    Box::new(Gc::new(page_size))
}

// Clean up all the memory, the Gc object passed becomes unusable.
// Unfortunately, C cannot tell this.
pub extern fn gc_drop(gc: Box<Gc>) {
}

// Cache of thread local change to global metadata during GC
// This were getting sync'd after marking in Julia GC
#[repr(C)]
pub struct GcMarkCache {
    pub perm_scanned_bytes: usize,
    pub scanned_bytes: usize,
    pub nbig_obj: usize,
    // array of queued big object to be moved between the young list
    // and the old list. We use low bit to track whether the object
    // should be moved so an object can and should be moved to this
    // list after mark bit is flipped to 1 atomically. This and the
    // sync after marking guarantee that single objects can only
    // appear once in the lists (the mark bit cannot be cleared
    // without sweeping).
    pub big_obj: [*const c_void; 1024],
}

const AL_N_INLINE: usize = 29;

pub struct JlArrayList {
    pub len: usize,
    pub max: usize,
    pub items: *mut *mut c_void,
    pub _space: [*mut c_void; AL_N_INLINE],
}

// Thread-local heap
pub struct JlThreadHeap {
    pub weak_refs: JlArrayList,
    pub mallocarrays: *mut MallocArray,
    pub mafreelist: *mut MallocArray,
    pub big_objects: *mut BigVal,
    pub rem_bindings: JlArrayList,
    pub _remset: [JlArrayList; 2],
    pub remset_nptr: c_int,
    pub remset: *mut JlArrayList,
    pub last_remset: *mut JlArrayList,
    pub norm_pools: [GcPool; 41],
}

pub struct GcPool {
    freelist: uintptr_t,
    newpages: uintptr_t,
    osize: u16,
}

// Julia's Thread-local states
#[repr(C)]
pub struct JlTLS {
    pub pgcstack: Box<GcFrame>,
    pub world_age: usize,
    // using Option instead of Box for values that can be null
    // this works thanks to null pointer optimization in Rust
    pub exception_in_transit: Option<JlValue>,
    pub safepoint: usize, // volatile, TODO: represent volatility
    pub gc_state: GcState, // volatile
    pub in_finalizer: u8, // volatile
    pub disable_gc: u8,
    pub defer_signal: sig_atomic_t, // ???
    pub current_module: Option<JlModule>,
    pub current_task: Option<JlTask>, // volatile
    pub root_task: Option<JlTask>,
    pub task_arg_in_transit: Option<JlValue>, // volatile
    pub stackbase: *const c_void,
    pub stack_lo: *const u8,
    pub stack_hi: *const u8,
    pub jmp_target: Option<JlJmpBuf>, // volatile
    pub base_ctx: Option<JlJmpBuf>, // base context of stack
    pub safe_restore: Option<JlJmpBuf>,
    pub tid: i16,
    pub bt_size: usize,
    pub bt_data: *const uintptr_t, // this is an array that is JL_MAX_BT_SIZE + 1 long
    // set by the sender, reset by the handler. Julia will handle signals for us.
    pub signal_request: sig_atomic_t, // volatile
    pub io_wait: sig_atomic_t, // volatile
    pub heap: JlThreadHeap,
    pub system_id: libc::pthread_t, // should remove this on Windows since Julia doesn't have it on Windows
    pub signal_stack: *const c_void, // should remove this on Windows since Julia doesn't have it on Windows
    pub in_pure_callback: c_int,
    pub finalizers: Vec<Finalizer<'static>>,
    pub gc_cache: GcMarkCache,
}

type JlPTLS = Option<JlTLS>; // this is just a pointer to thread-local state

// Note: We represent sig_atomic_t as c_int since C99 standard says so.
pub type sig_atomic_t = c_int;

#[repr(u8)]
pub enum GcState {
    Waiting = 1, // thread is waiting for GC
    Safe = 2, // thread is running unmanaged code that can be executed simultaneously with GC
}
