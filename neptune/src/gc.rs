use libc::*;
use bit_field::BitField;
use pages::*;
use util::*;
use std::mem;
use std::env;
use std::num;
use c_interface::*;
use threadpool::ThreadPool;

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
    pub allocd:         i64,
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
            allocd:         0,
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

    // Find page with given data pointer
    pub fn index_of_raw(&self, data: * const u8) -> Option<usize> {
        // for (i, p) in self.pages.iter().enumerate() {
        //     if p.data.as_ptr() == data {
        //         return Some(i);
        //     }
        // }
        // None
        // optimization of above with pointer arithmetic:
        let offset = data as usize - self.pages.as_ptr() as usize;
        if offset >= self.pg_cnt as usize * PAGE_SZ {
            // data is not in the region
            None
        } else {
            Some(offset >> PAGE_LG2) // get the page id from offset
        }
    }
}

// Pool page metadata
#[repr(C)]
pub struct PageMeta<'a> {
    pub pool_n:     u8,   // idx of pool that owns this page
    // TODO: make following bools after transitioning to Rust
    pub has_marked: u8,   // whether any cell is marked in this page
    pub has_young:  u8,   // whether any live and young cells are in this page, before sweeping
    pub nold:       u16,  // #old objects
    pub prev_nold:  u16,  // #old object during previous sweep
    pub nfree:      u16,  // #free objects, invalid if pool that owns this page is allocating from it
    pub osize:      u16,  // size of each object in this page
    pub fl_begin_offset: u16, // offset of the first free object
    pub fl_end_offset:   u16, // offset of the last free object
    pub thread_n: u16, // thread id of the heap that owns this page
    pub data: Option<Box<&'a mut [u8]>>,
    pub ages: Option<Box<&'a mut [u8]>>,
}

impl<'a> PageMeta<'a> {
    pub fn new() -> Self {
        PageMeta {
            pool_n:     0,
            has_marked: 0,
            has_young:  0,
            nold:       0,
            prev_nold:  0,
            nfree:      0,
            osize:      0,
            fl_begin_offset: 0,
            fl_end_offset:   0,
            thread_n: 0,
            data: None,
            ages: None,
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
    pub finalizer_list_marked: Vec<Finalizer<'a>>,
    pub to_finalize: Vec<Finalizer<'a>>, // make sure that this doesn't have tagged pointers by refining the type
    pub lazy_freed_pages: i64,
    pub page_mgr: PageMgr,
    pub page_size: usize,
    pub thread_pool: ThreadPool,
}

// GC implementation

impl<'a> Gc<'a> {
    pub fn new(page_size: usize) -> Gc<'a> {
        // create regions
        let mut regions = Vec::with_capacity(REGION_COUNT);
        for _ in 0..REGION_COUNT {
            regions.push(Region::new());
        }
        // create thread pool
        let nthreads = match ::std::env::var("NEPTUNE_THREADS").map_err(GcInitError::Env).and_then(|nthreads| {
            nthreads.parse::<usize>().map_err(GcInitError::Parse)
        }) {
            Ok(0) => panic!("Garbage collector cannot work with 0 worker threads! Set NEPTUNE_THREADS to a positive number."),
            Ok(n) => n,
            Err(GcInitError::Env(env::VarError::NotPresent)) => 1, // if no environment variable given, assume 1
            Err(_) => panic!("Expected environment variable NEPTUNE_THREADS to be defined as a positive number.")
        };

        // create global GC object
        Gc {
            gc_num: GcNum::new(),
            last_long_collect_interval: 0,
            regions: regions,
            big_objects_marked: Vec::new(),
            finalizer_list_marked: Vec::new(),
            to_finalize: Vec::new(),
            lazy_freed_pages: 0,
            page_mgr: PageMgr::new(),
            page_size: page_size, // equivalent of jl_page_size, size of OS' pages
            thread_pool: ThreadPool::new(nthreads),
        }
    }

    pub fn schedule_finalization(&mut self, o: Option<&'a JlValue>, f: uintptr_t) {
        self.to_finalize.push(Finalizer::new(o, f));
    }

    // move run_finalizer to C side
    // pub fn run_finalizer(tls: Option<JlTLS>, obj: &JlValue, ff: &Option<JlValue>)

    // if `need_sync` then `list` is the `finalizers` list of another thread
    pub fn finalize_object<'b>(&mut self, list: &mut Vec<Finalizer<'b>>, o: Option<&'b JlValue>, copied_list: &mut Vec<Finalizer<'b>>, need_sync: bool) {
        // make sure that this is atomic by checking need_sync
        for i in 1..list.len() {
            let v = list[i].obj;
            let mut should_move = false;
            if o.map(|n| n as *const JlValue as usize) == (v.as_ref().map(|n| (n as *const &JlValue as usize).clear_tag(1))) {
                should_move = true;
                let f = list[i].fun;
                // function is an actual function, cast the pointer
                if v.as_ref().map(|n| (n as *const &JlValue as usize) & 1).unwrap_or(0) != 0 {
                    // this works because of null pointer optimization on Option<T>
                    let f: fn(Option<&JlValue>) -> *const c_void = unsafe { mem::transmute(f) };
                    f(o);
                } else {
                    copied_list.push(Finalizer::new(o, f));
                }
            }
            if should_move || v.is_none() {
                // TODO: make sure that these updates are atomic by enforcing rules on vecs if need_sync
                list.swap_remove(i);
            }
        }
    }

    // ???
}

type JlValue = c_void;

pub struct Finalizer<'a> {
    obj: Option<&'a JlValue>,
    fun: uintptr_t,
}

impl<'a> Finalizer<'a> {
    pub fn new(o: Option<&'a JlValue>, fun: uintptr_t) -> Self {
        Finalizer { obj: o, fun: fun }
    }
}

// representation of big objects
#[repr(C)]
pub struct BigVal {
    //next: Box<BigVal>,
    //prev: Box<BigVal>,
    szOrAge: usize, // unpack this union via methods
    padding: [u8; 32], // to align to 64 bits
    headerOrBits: usize, // unpack this union via methods
    // object data is here
}

impl BigVal {
    pub fn new(s: usize, h: usize) -> Self {
        BigVal { szOrAge: s, padding: [0; 32], headerOrBits: h }
    }

    pub unsafe fn taggedvalue(&self) -> &JlTaggedValue {
        let ptr: * const Self = self;
        mem::transmute(ptr.offset(1))
    }

    pub unsafe fn mut_taggedvalue(&mut self) -> &mut JlTaggedValue {
        let ptr: * mut Self = self;
        mem::transmute(ptr.offset(1))
    }

    #[inline(always)]
    pub fn size(&self) -> usize {
        self.szOrAge
    }

    #[inline(always)]
    pub fn set_size(&mut self, size: usize) {
        self.szOrAge = size;
    }
}

// list of malloc'd arrays
#[repr(C)]
pub struct MallocArray {
    a: Box<JlArray>,
    next: Option<Box<MallocArray>>
}

impl MallocArray {
    pub fn new(a: Box<JlArray>) -> Self {
        MallocArray {
            a: a,
            next: None,
        }
    }
}
