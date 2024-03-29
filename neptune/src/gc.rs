use libc::*;
use pages::*;
use gc2::*;
use util::*;
use std::mem;
use std::env;
use std::num;
use c_interface::*;
use std::sync::atomic::*;

// Errors that can be encountered during Gc initialization
#[derive(Debug)]
pub enum GcInitError {
    Parse(num::ParseIntError),
    Env(env::VarError),
}

// max. # of regions
pub const REGION_COUNT: usize = 32768; // 2^48 / 8G

pub const PAGE_LG2: usize = 14; // log_2(PAGE_SZ)
pub const PAGE_SZ: usize = 1 << PAGE_LG2; // 16k

// can we just use Rust threading instead of mutexes for these?
// static jl_mutex_t finalizers_lock;
// static jl_mutex_t gc_cache_lock;

// GC stats. This is equivalent of jl_gc_num_t in Julia
#[repr(C)]
pub struct GcNum {
    pub allocd:         AtomicI64,
    pub deferred_alloc: i64,
    pub freed:          i64,
    pub malloc:         u64,
    pub realloc:        u64,
    pub poolalloc:      u64,
    pub bigalloc:       u64,
    pub freecall:       u64,
    pub total_time:     u64,
    pub total_allocd:   u64,
    pub since_sweep:    u64,
    pub interval:       usize,
    pub pause:          c_int,
    pub full_sweep:     c_int
}

impl GcNum {
    fn new() -> GcNum {
        GcNum {
            allocd:         AtomicI64::new(0),
            deferred_alloc: 0,
            freed:          0,
            malloc:         0,
            realloc:        0,
            poolalloc:      0,
            bigalloc:       0,
            freecall:       0,
            total_time:     0,
            total_allocd:   0,
            since_sweep:    0,
            interval:       0,
            pause:          0,
            full_sweep:     0,
        }
    }
}

// A GC region, equivalent of region_t
#[repr(C)]
pub struct Region<'a> {
    pub pages: &'a mut [Page],
    pub allocmap: &'a mut [u32],
    pub meta: &'a mut [PageMeta<'a>],
    pub pg_cnt: c_uint,
    pub lb: c_uint,
    pub ub: c_uint
}

impl<'a> Region<'a> {
    pub fn new() -> Region<'a> {
        Region {
            pages: &mut [],
            allocmap: &mut [],
            meta: &mut [],
            pg_cnt: 0,
            lb: 0,
            ub: 0,
        }
    }

    pub fn index_of(&self, page: &Page) -> Option<usize> {
        self.index_of_raw(page.data.as_ptr())
    }

    /// Find page containing given data pointer.
    pub fn index_of_raw(&self, data: * const u8) -> Option<usize> {
        // for (i, p) in self.pages.iter().enumerate() {
        //     if p.data.as_ptr() == data {
        //         return Some(i);
        //     }
        // }
        // None
        // optimization of above with pointer arithmetic:
        let offset = data as isize - self.pages.as_ptr() as isize;
        if offset < 0 || offset >= self.pg_cnt as isize * PAGE_SZ as isize {
            // data is not in the region
            None
        } else {
            Some(offset as usize >> PAGE_LG2) // get the page id from offset
        }
    }
}

pub struct Gc<'a> {
    // gc stats
    pub gc_num: GcNum,
    // collect interval???
    pub last_long_collect_interval: usize,
    // GC regions
    pub regions: Vec<Region<'a>>, // this has size REGION_COUNT, but couldn't be an array since Region doesn't implement copy
    // list of marked big objects, not per thread
    pub big_objects_marked: Vec<BigVal>,
    // list of marked finalizers for object that need to be finalized in last mark phase
    pub lazy_freed_pages: i64,
    pub page_mgr: PageMgr,
    pub page_size: usize,
}

// GC implementation

impl<'a> Gc<'a> {
    pub fn new(page_size: usize) -> Gc<'a> {
        // create regions
        let mut regions = Vec::with_capacity(REGION_COUNT);
        for _ in 0..REGION_COUNT {
            regions.push(Region::new());
        }

        // create global GC object
        Gc {
            gc_num: GcNum::new(),
            last_long_collect_interval: 0,
            regions: regions,
            big_objects_marked: Vec::new(),
            lazy_freed_pages: 0,
            page_mgr: PageMgr::new(),
            page_size: page_size, // equivalent of jl_page_size, size of OS' pages
        }
    }
}

// representation of big objects
#[repr(C)]
pub struct BigVal {
    // use uintptr_t here to denote the void pointers. we are not
    // using them so they should be safe to send over channels to
    // threads.
    next: uintptr_t, // unused
    prev: uintptr_t, // unused
    pub sz_or_age: usize, // unpack this union via methods
    // if this bigval belongs to any thread's big object list, which one. -1 denotes big_objects_marked. Invalid if in_list is false
    pub tid: i16,
    // is this object in cache
    pub in_list: bool,
    // which slot of the list/cache this object is in, for deletion purposes
    pub slot: usize,
    padding: [u64; 8 - 6], // to align to 64 bits when included the taggedvalue below
    // taggedvalue is here (this is header union in bigval_t)
    // object data is here
}

impl BigVal {
    #[inline(always)]
    pub fn true_size() -> usize {
        mem::size_of::<BigVal>() + mem::size_of::<JlTaggedValue>()
    }

    pub fn allocd_size(&self) -> usize {
        llt_align(self.size() + BigVal::true_size(), JL_CACHE_BYTE_ALIGNMENT)
    }

    pub fn taggedvalue(&self) -> &JlTaggedValue {
        let ptr: * const Self = self;
        unsafe { mem::transmute(ptr.offset(1)) }
    }

    pub fn mut_taggedvalue(&mut self) -> &mut JlTaggedValue {
        let ptr: * mut Self = self;
        unsafe { mem::transmute(ptr.offset(1)) }
    }

    #[inline(always)]
    pub fn size(&self) -> usize {
        self.sz_or_age.get_bits(2..64) << 2
    }

    #[inline(always)]
    pub fn set_size(&mut self, size: usize) {
        debug_assert_eq!(size & 3, 0);
        self.sz_or_age.set_bits(2..64, size >> 2);
    }

    #[inline(always)]
    pub fn age(&self) -> usize {
        // subject to change based on endianness
        self.sz_or_age.get_bits(0..2)
    }

    #[inline(always)]
    pub fn set_age(&mut self, age: usize) {
        self.sz_or_age.set_bits(0..2, age);
    }

    /// Increment age while saturating it when it reaches the promotion age
    #[inline(always)]
    pub fn inc_age(&mut self) {
        let age = self.sz_or_age.get_bits(0..2);
        if age < PROMOTE_AGE {
            self.sz_or_age.set_bits(0..2, age + 1);
        }
    }

    pub unsafe fn from_mut_jltaggedvalue(t: &mut JlTaggedValue) -> &mut Self {
        &mut *mem::transmute::<* mut JlTaggedValue, * mut BigVal>(t).offset(-1)
    }

    pub unsafe fn from_jltaggedvalue(t: & JlTaggedValue) -> & Self {
        &*mem::transmute::<* const JlTaggedValue, * const BigVal>(t).offset(-1)
    }
}

// list of malloc'd arrays
#[repr(C)]
pub struct MallocArray {
    pub a:* mut JlArray,
    pub next: Option<Box<MallocArray>>
}

impl MallocArray {
    pub fn new(a:* mut JlArray) -> Self {
        MallocArray {
            a: a,
            next: None,
        }
    }
}
