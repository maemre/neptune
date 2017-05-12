// C interface for the garbage collector, C side needs to call
// appropriate functions with a Gc object since Using a static mutable
// object is unsafe in Rust because of "life after main" and
// destructor order.

use gc::*;
use gc2::*;
use libc::c_int;
use libc::c_void;
use libc::c_uint;
use libc::uintptr_t;
use libc;
use pages::*;
use core::slice;
use std::mem;
use core::ops::Deref;
use core::ops::DerefMut;
use core;

pub type JlJmpBuf = libc::c_void; // we cannot use long jumps in Rust anyways

// temporary, TODO: reify
pub type JlValue = libc::c_void;
pub type JlTask = libc::c_void;
pub type JlModule = libc::c_void;

// This is a marker trait for data structures that are allocated as JlValue, if
// a data structure implements this then it promises its memory layout to be
// same as a JlValue and promises that it has a tag for GC, runtime type tag
// etc.
pub trait JlValueMarker {
}

// This trait provides casting to JlValue for the types that implement it
pub trait JlValueLike {
    // extract JlValue representation of this struct
    fn as_jlvalue(&self) -> &JlValue;

    fn as_mut_jlvalue(&mut self) -> &mut JlValue;
}

// Automatic derivation of JlValue casting for types that implement JlValueMarker
impl<T> JlValueLike for T where T: Sized+JlValueMarker {
    fn as_mut_jlvalue(&mut self) -> &mut JlValue {
        unsafe {
            mem::transmute(self)
        }
    }

    fn as_jlvalue(&self) -> &JlValue {
        unsafe {
            mem::transmute(self)
        }
    }
}

pub unsafe fn as_jltaggedvalue(v: * const JlValue) -> * const JlTaggedValue {
    mem::transmute::<* const JlValue, * const JlTaggedValue>(v).offset(-1)
}

pub unsafe fn as_mut_jltaggedvalue(v: * mut JlValue) -> * mut JlTaggedValue {
    mem::transmute::<* mut JlValue, * mut JlTaggedValue>(v).offset(-1)
}

pub struct JlDatatypeLayout {
  nfields: u32,
  alignment: u32, // TODO 9 bits
  haspadding: u32, // TODO 1 bit
  npointers: u32, // TODO 20 bits
  fielddesc_type: u32 // TODO 2 bits
}

pub struct JlSVec {
  //JL_DATA_TYPE
  length: usize,
}

pub struct JlDatatype {
  //JL_DATA_TYPE
  pub name: String,
  pub super_t: *const JlDatatype,
  pub parameters: JlSVec, // TODO
  pub types: JlSVec, // TODO
  pub instance: JlValue,  // for singletons
  pub layout: *const JlDatatypeLayout,
  pub size: i32,
  pub ninitialized: i32,
  pub uid: u32,
  pub stract: u8,
  pub mutabl: u8,
  // memoized properties
  pub struct_decl: *mut c_void,  //llvm::Type*
  pub ditype: *mut c_void, // llvm::MDNode* to be used as llvm::DIType(ditype)
  pub depth: u32,
  pub hasfreetypevars: u8,
  pub isleaftype: u8,
}

// this is actually just the tag
pub struct JlTaggedValue {
    pub header: libc::uintptr_t
}
// this is actually mem::size_of::<JlTaggedValue>(), we cannot make it a static const
// because `size_of` is not yet constant in Rust unfortunately.
// ACHTUNG: update this if JlTaggedValue is ever changed!
pub const SIZE_OF_JLTAGGEDVALUE: usize = 8;

extern {
    pub fn gc_final_count_page(pg_cnt: usize);
    pub fn jl_gc_wait_for_the_world(); // wait for the world to stop

    // mark boxed caches, which don't contain any pointers hence are terminal nodes
    pub fn jl_mark_box_caches(ptls: &mut JlTLS);

    // set type of a value by setting the tag
    pub fn np_jl_set_typeof(v: &mut JlValue, typ: * const c_void);

    // list of global threads, declared in julia/src/threading.c
    //pub static jl_all_ts_states: *mut JlTLS;
    //pub static jl_n_threads: u32;
    // TODO I'm not sure if this is legal, but it compiles for now
    pub static jl_all_tls_states: Vec<* mut JlTLS>;

    pub static jl_page_size: usize;

    // jl_nothing is a value inhabiting bottom, similar to NULL. It is used for
    // invalidating weak references so its type should match weak reference
    // types.
    pub static jl_nothing: * mut JlValue;
}

pub fn jl_value_of(t: &JlTaggedValue) -> &JlValue {
    unsafe {
        mem::transmute((t as * const JlTaggedValue).offset(1))
    }
}

pub fn jl_value_of_mut(t: &mut JlTaggedValue) -> &mut JlValue {
    unsafe {
        mem::transmute((t as * mut JlTaggedValue).offset(1))
    }
}

pub fn gc_init<'a>(page_size: usize) -> Box<Gc<'a>> {
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
    pub rem_bindings: JlArrayList, // TODO what are these?
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
    // pointer to thread-local GC-related stuff, lifetime is meaningless!
    pub tl_gcs: * mut Gc2<'static>,
}

type JlPTLS<'a> = Option<&'a JlTLS>; // this is just a pointer to thread-local state

// Note: We represent sig_atomic_t as c_int since C99 standard says so.
pub type sig_atomic_t = c_int;

#[repr(u8)]
pub enum GcState {
    Waiting = 1, // thread is waiting for GC
    Safe = 2, // thread is running unmanaged code that can be executed simultaneously with GC
}

// expose page manager
static mut PAGE_MGR: Option<PageMgr> = None;

// julia's GC's regions are slightly different, using naked pointers etc.
#[repr(C)]
pub struct JlRegion<'a> {
    pub pages: * mut Page,
    pub allocmap: * mut u32,
    pub meta: * mut PageMeta<'a>,
    pub pg_cnt: c_uint,
    pub lb: c_uint,
    pub ub: c_uint
}

impl<'a> JlRegion<'a> {
    pub fn to_region(&mut self) -> Region<'a> {
        let pages: &mut [Page] = if self.pages as * const u8 == core::ptr::null() {
            assert!(self.pg_cnt == 0, "page array cannot be null if region is not empty!");
            &mut []
        } else {
            unsafe { slice::from_raw_parts_mut(self.pages, self.pg_cnt as usize) }
        };
        let allocmap: &mut [u32] = if self.allocmap as * const u8 == core::ptr::null() {
            assert!(self.pg_cnt == 0, "alloc map cannot be null if region is not empty!");
            &mut []
        } else {
            unsafe { slice::from_raw_parts_mut(self.allocmap, self.pg_cnt as usize / 32) }
        };
        let meta: &mut [PageMeta] = if self.meta as * const PageMeta == core::ptr::null() {
            assert!(self.pg_cnt == 0, "pagemeta array cannot be null if region is not empty!");
            &mut []
        } else {
            unsafe { slice::from_raw_parts_mut(self.meta, self.pg_cnt as usize) }
        };
        Region {
            pages: pages,
            allocmap: allocmap,
            meta: meta,
            pg_cnt: self.pg_cnt,
            lb: self.lb,
            ub: self.ub,
        }
    }
    // update self using information from region
    pub fn update(&mut self, region: Region<'a>) {
        self.pages = region.pages.as_mut_ptr();
        self.allocmap = region.allocmap.as_mut_ptr();
        self.meta = region.meta.as_mut_ptr();
        self.pg_cnt = region.pg_cnt;
        self.lb = region.lb;
        self.ub = region.ub;
    }
}

pub struct JlRegionArray<'a> {
    regions: * mut JlRegion<'a>
}

impl<'a> JlRegionArray<'a> {
    pub fn new(regions: * mut JlRegion<'a>) -> Self {
        JlRegionArray { regions: regions }
    }
}

impl<'a> Deref for JlRegionArray<'a> {
    type Target = [JlRegion<'a>];

    fn deref(&self) -> &[JlRegion<'a>] {
        unsafe { slice::from_raw_parts(self.regions, REGION_COUNT) }
    }
}

impl<'a> DerefMut for JlRegionArray<'a> {
    fn deref_mut(&mut self) -> &mut [JlRegion<'a>] {
        unsafe { slice::from_raw_parts_mut(self.regions, REGION_COUNT) }
    }
}

//------------------------------------------------------------------------------
// Page manager

#[no_mangle]
pub unsafe extern fn neptune_init_page_mgr() {
    println!("page offset: {}", GC_PAGE_OFFSET);

    PAGE_MGR = Some(PageMgr::new());
    REGIONS = Some(Vec::with_capacity(REGION_COUNT));
    let regions = REGIONS.as_mut().unwrap();
    for i in 0..REGION_COUNT {
        regions.push(Region::new()); // initialize regions
    }
}

#[no_mangle]
pub unsafe extern fn neptune_alloc_page<'a>() -> * mut u8 {
    // if PAGE_MGR is uninitialized, we're better off crashing anyways
    PAGE_MGR.as_mut().unwrap().alloc_page(&mut REGIONS.as_mut().unwrap()).data.as_mut_ptr()
}

#[no_mangle]
pub unsafe extern fn neptune_free_page<'a>(data: * const u8) {
    PAGE_MGR.as_mut().unwrap().free_page(REGIONS.as_mut().unwrap().as_mut_slice(), data);
}

//------------------------------------------------------------------------------
// Region related exports

// NB. I'm not happy with this being 'static The solution seems like
// moving everything to Rust. Objects in the boundary will still have
// static lifetime probably, since Rust cannot reason about lifetimes
// crossing languages.
pub static mut REGIONS: Option<Vec<Region<'static>>> = None;

#[no_mangle]
pub unsafe extern fn neptune_get_region(i: usize) -> &'static mut Region<'static> {
    &mut REGIONS.as_mut().unwrap()[i]
}

// Find region given pointer is in
// NOTE: This works because of null-pointer optimization on Option<&T>
#[no_mangle]
pub unsafe extern fn neptune_find_region(ptr: * const Page) -> Option<&'static mut Region<'static>> {
    let mut regions = REGIONS.as_mut().unwrap();
    for i in 0..regions.len() {
        let begin = regions[i].pages.as_ptr();
        // pointer arithmetic to find end of region
        let end = begin.offset(regions[i].pg_cnt as isize);
        if ptr >= begin && ptr <= end {
            return Some(&mut regions[i]);
        }
    }
    None
}

#[no_mangle]
pub unsafe extern fn neptune_get_pages<'a>(region: &'a mut Region<'a>) -> * mut Page {
    region.pages.as_mut_ptr()
}

#[no_mangle]
pub unsafe extern fn neptune_get_allocmap<'a>(region: &'a mut Region<'a>) -> * mut u32 {
    region.allocmap.as_mut_ptr()
}

#[no_mangle]
pub unsafe extern fn neptune_get_pagemeta<'a>(region: &'a mut Region<'a>) -> * mut PageMeta<'a> {
    region.meta.as_mut_ptr()
}

#[no_mangle]
pub extern fn neptune_get_lb<'a>(region: &mut Region<'a>) -> u32 {
    region.lb
}

#[no_mangle]
pub extern fn neptune_set_lb<'a>(region: &mut Region<'a>, lb: u32) {
    region.lb = lb;
}

#[no_mangle]
pub extern fn neptune_get_ub<'a>(region: &mut Region<'a>) -> u32 {
    region.ub
}

#[no_mangle]
pub extern fn neptune_set_ub<'a>(region: &mut Region<'a>, ub: u32) {
    region.ub = ub;
}

#[no_mangle]
pub extern fn neptune_get_pgcnt<'a>(region: &mut Region<'a>) -> u32 {
    region.pg_cnt
}

//------------------------------------------------------------------------------
// GC entry points

#[no_mangle]
pub extern fn neptune_alloc<'gc, 'a>(gc: &'gc mut Gc2<'a>, size: usize, typ: * const libc::c_void) -> &'gc mut JlValue {
    gc.alloc(size, typ)
}

#[no_mangle]
pub extern fn neptune_pool_alloc<'gc, 'a>(gc: &'gc mut Gc2<'a>, size: usize) -> &'gc mut JlValue {
    gc.pool_alloc(size)
}

#[no_mangle]
pub extern fn neptune_big_alloc<'gc, 'a>(gc: &'gc mut Gc2<'a>, size: usize) -> &'gc mut JlValue {
    gc.big_alloc(size)
}

#[no_mangle]
pub extern fn neptune_init_thread_local_gc<'a>(tls: &'static JlTLS,
                                               stack: &'static GcFrame) -> Box<Gc2<'a>> {
    let pg_mgr = unsafe {
        PAGE_MGR.as_mut().unwrap()
    };
    Box::new(Gc2::new(tls, stack, pg_mgr))
}

// Corresponds to _jl_gc_collect
#[no_mangle]
pub extern fn neptune_gc_collect<'gc, 'a>(gc: &'gc mut Gc2<'a>, full: bool) -> bool {
    gc.collect(full)
}
