use libc;
use pages::*;
use std::mem;
use gc::*;
use c_interface::JlValue;
use c_interface::*;
use bit_field::BitField;
use alloc;

// this is actually just the tag
struct JlTaggedValue {
    header: libc::uintptr_t
}

const TAG_BITS: u8 = 2; // number of tag bits
const GC_N_POOLS: usize = 41;

static GC_SIZE_CLASSES: [usize; GC_N_POOLS] = [
    // minimum platform alignment
    8,
    // increments of 16 till 256 bytes
    16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256,
    // rest is from Julia, according to formula:
    // size = (div(2^14-8,rng)÷16)*16; hcat(sz, (2^14-8)÷sz, 2^14-(2^14-8)÷sz.*sz)'
    
    // rng = 60:-4:32 (8 pools)
    272, 288, 304, 336, 368, 400, 448, 496,
    //   60,  56,  53,  48,  44,  40,  36,  33, /pool
    //   64, 256, 272, 256, 192, 384, 256,  16, bytes lost

    // rng = 30:-2:16 (8 pools)
    544, 576, 624, 672, 736, 816, 896, 1008,
    //   30,  28,  26,  24,  22,  20,  18,  16, /pool
    //   64, 256, 160, 256, 192,  64, 256, 256, bytes lost

    // rng = 15:-1:8 (8 pools)
    1088, 1168, 1248, 1360, 1488, 1632, 1808, 2032
    //    15,   14,   13,   12,   11,   10,    9,    8, /pool
    //    64,   32,  160,   64,   16,   64,  112,  128, bytes lost
];
const GC_MAX_SZCLASS: usize = 2032 - 8; // 8 is mem::size_of::<libc::uintptr_t>(), size_of isn't a const fn yet :(

impl JlTaggedValue {
    
    // implement union members by transmuting memory
    pub unsafe fn next(&self) -> * const JlTaggedValue {
        mem::transmute(self)
    }
    pub unsafe fn next_mut(&mut self) -> * mut JlTaggedValue {
        mem::transmute(self)
    }
    pub unsafe fn typ(&self) -> * const JlValue {
        mem::transmute(self)
    }
    // this is bits in Julia
    pub unsafe fn tag(&self) -> libc::uintptr_t {
        self.header.get_bits(0..TAG_BITS)
    }
    // this will panic if one tries to set bits higher than lowest TAG_BITS bits
    pub unsafe fn set_tag(&mut self, tag: u8) {
        self.header.set_bits(0..TAG_BITS, tag as usize);
    }
}

// A GC Pool used for pooled allocation
pub struct GcPool {
    freelist: Vec<JlTaggedValue>, // list of free objects, a vec is more packed
    newpages: Vec<JlTaggedValue>, // list of chunks of free objects
    osize: usize                  // size of objects in this pool, could've been u16
}

#[repr(C)]
pub struct WeakRef {
    // JL_DATA_TYPE exists before the pointer
    pub value: Option<Box<JlValue>>,
}

// JlSym is opaque to Rust because we don't care about its details
type JlSym = libc::c_void;

#[repr(C)]
pub struct JlBinding<'a> {
    pub name: * mut JlSym,
    pub value: &'a JlValue,
    pub globalref: &'a JlValue,
    pub owner: &'a JlModule,
    bitflags: u8
}

// implementation for bitfield access
impl<'a> JlBinding<'a> {
    pub fn constp(&self) -> bool {
        self.bitflags.get_bit(0)
    }
    pub fn exportp(&self) -> bool {
        self.bitflags.get_bit(1)
    }
    pub fn imported(&self) -> bool {
        self.bitflags.get_bit(2)
    }
    pub fn deprecated(&self) -> bool {
        self.bitflags.get_bit(3)
    }
    pub fn set_constp(&mut self, flag: bool) {
        self.bitflags.set_bit(0, flag);
    }
    pub fn set_exportp(&mut self, flag: bool) {
        self.bitflags.set_bit(1, flag);
    }
    pub fn set_imported(&mut self, flag: bool) {
        self.bitflags.set_bit(2, flag);
    }
    pub fn set_deprecated(&mut self, flag: bool) {
        self.bitflags.set_bit(3, flag);
    }
}

// Thread-local heap
// lifetimes don't mean anything yet
pub struct ThreadHeap<'a> {
    // pools
    pools: [GcPool; GC_N_POOLS],
    // weak refs
    weak_refs: Vec<WeakRef>,
    // malloc'd arrays
    mallocarrays: Vec<MallocArray>,
    mafreelist: Vec<MallocArray>,
    // big objects
    big_objects: BigVal, // TODO: use linked list
    // remset
    rem_bindings: Vec<JlBinding<'a>>,
    remset: Vec<* mut JlValue>,
    last_remset: Vec<* mut JlValue>,
    
}

pub struct GcMarkCache {
    // thread-local statistics, will be merged into global during stop-the-world
    perm_scanned_bytes: usize,
    scanned_bytes: usize,
    nbig_obj: usize, // # of queued big objects to be moved to old gen.
    big_obj: [* mut libc::c_void; 1024],
}

pub struct GcFrame {
    nroots: usize,
    // GC never deallocates frames, their lifetime is 'static from Rust's point of view
    prev: Option<&'static GcFrame>,
    // actual roots appear here
}

// Thread-local GC data
// Lifetimes here don't have a meaning, yet
pub struct Gc2<'a> {
    // heap for current thread
    heap: ThreadHeap<'a>,
    // handle for page manager
    pg_mgr: &'a PageMgr,
    // mark cache for thread-local marks
    cache: GcMarkCache,
    // Stack for GC roots
    gc_stack: &'static GcFrame,
    // Age of the world, used for promotion
    world_age: usize,
    // State of GC for this thread
    gc_state: GcState,
    in_finalizer: bool,
    disable_gc: bool,
    // Finalizers belong to here
    finalizers: Vec<Finalizer<'a>>,
    // Counter to disable finalizers on the current thread
    finalizers_inhibited: libc::c_int,
    // parent pointer to thread-local storage for other fields, if necessary
    // we can access stack base etc. from here (?)
    tls: &'static JlTLS,
}

impl<'a> Gc2<'a> {
    pub fn collect(&mut self, full: bool) {
    }

    #[inline(always)]
    pub fn collect_small(&mut self) {
        self.collect(false)
    }
    
    #[inline(always)]
    pub fn collect_full(&mut self) {
        self.collect(true)
    }
    
    // allocate a Julia object
    pub fn alloc(&mut self, size: usize, typ: * const libc::c_void) -> &mut JlValue {
        let allocsz = match size.checked_add(mem::size_of::<JlTaggedValue>()) {
            Some(s) => s,
            None => panic!("Memory error: requested object is too large to represent with native pointer size")
        };
        let v = if allocsz <= GC_MAX_SZCLASS + mem::size_of::<JlTaggedValue>() {
            self.pool_alloc(allocsz)
        } else {
            self.big_alloc(allocsz)
        };
        unsafe {
            jl_set_typeof(v, typ);
        }
        v
    }

    pub fn pool_alloc(&mut self, size: usize) -> &mut JlValue {
        self.rust_alloc(size)
    }

    pub fn find_pool(&self, size: &usize) -> Option<usize> {
        GC_SIZE_CLASSES.binary_search(size)
            .map(|i| Some(i))
            .unwrap_or_else(|idx| {
                let i = idx + 1;
                if i > GC_SIZE_CLASSES.len() {
                    None
                } else {
                    Some(i)
                }
            })
    }
    
    pub fn big_alloc(&mut self, size: usize) -> &mut JlValue {
        // TODO: actually handle this rather than piggybacking on Rust
        self.rust_alloc(size)
    }

    pub fn rust_alloc(&mut self, size: usize) -> &mut JlValue {
        unsafe {
            // we don't deal with ZSTs but just fail
            debug_assert_ne!(size, 0);
            let ptr = alloc::heap::allocate(size, 8);
            if ptr.is_null() {
                panic!("GC error: out of memory (OOM)!");
            }
            mem::transmute(ptr)
        }
    }
}