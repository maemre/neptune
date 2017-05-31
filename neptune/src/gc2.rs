use libc;
use pages::*;
use std::mem;
use gc::*;
use c_interface::*;
use alloc;
use std::intrinsics;
use std::sync::atomic::*;
use std::sync::*;
use std::slice;
use std::ffi::CStr;
use std::ops::Range;
use util::*;
use std::env;
use std::cmp;
use concurrency::*;
use scoped_threadpool::Pool;
use crossbeam::sync::*;
use std::thread;
use std::collections::HashMap;

type BitVec = Vec<AtomicBool>;

const PARALLEL_SWEEP: bool = false;

const PURGE_FREED_MEMORY: bool = false;

const TAG_BITS: u8 = 2; // number of tag bits
const TAG_RANGE: Range<u8> = 0..TAG_BITS;
const GC_N_POOLS: usize = 41;
const JL_SMALL_BYTE_ALIGNMENT: usize = 16;

const GC_CLEAN: u8 = 0;
const GC_MARKED: u8 = 1;
const GC_OLD: u8 = 2;
const GC_OLD_MARKED: u8 = (GC_OLD | GC_MARKED);

const MAX_MARK_DEPTH: i32 = 40;

const DEFAULT_COLLECT_INTERVAL: isize = 5600 * 1024 * 8;
const MAX_COLLECT_INTERVAL: usize = 1250000000;

// offset for aligning data in page to 16 bytes (JL_SMALL_BYTE_ALIGNMENT) after tag.
pub const GC_PAGE_OFFSET: usize = (JL_SMALL_BYTE_ALIGNMENT - (SIZE_OF_JLTAGGEDVALUE % JL_SMALL_BYTE_ALIGNMENT));

pub static mut np_threads: Option<Pool> = None;

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

static GC_ALREADY_RUN: AtomicBool = AtomicBool::new(false);

/*
 * in julia/src/julia.h:
 *
 *   struct _jl_taggedvalue_bits {
 *     uintptr_t gc:2;
 *   };
 *
 *   struct _jl_taggedvalue_t {
 *      union {
 *          uintptr_t header;
 *          jl_taggedvalue_t *next;
 *          jl_value_t *type; // 16-byte-aligned
 *          struct_jl_taggedvalue_bits bits;
 *      };
 *      // jl_value_t value;
 *   };
 *
 * The tag is stored before the pointer, so if the user has a value 'v', to treat it
 * as a tagged value, Julia uses the following macro, subtracting the size of the
 * tag value struct itself from the pointer.
 *
 *  #define jl_astaggedvalue(v) \
 *    ((jl_taggedvalue_t*)((char*)(v) - sizeof(jl_taggedvalue_t)))
 *
 * The value itself is stored after the header, so they simply take the value pointer
 * and add the size of the header, to get the pointer to the value it stores
 */
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
    pub unsafe fn typ_mut(&mut self) -> &mut JlValue {
        mem::transmute(self)
    }
    // this is bits in Julia
    pub fn tag(&self) -> u8 {
        self.header.tag()
    }
    // this will panic if one tries to set bits higher than lowest TAG_BITS bits
    pub fn set_tag(&mut self, tag: u8) {
        self.header.set_tag(tag);
    }

    pub fn marked(&self) -> bool {
        self.header.marked()
    }

    pub fn set_marked(&mut self, flag: bool) {
        self.header.set_marked(flag);
    }

    pub fn old(&self) -> bool {
        self.header.old()
    }

    pub fn set_old(&mut self, flag: bool) {
        self.header.set_old(flag);
    }

    // read header with relaxed memory guarantees
    #[inline(always)]
    pub fn read_header(&self) -> libc::uintptr_t {
        self.header.load(Ordering::Relaxed)
    }

    /// Read header with no memory guarantee. this is not thread safe w.r.t. other GC threads!
    #[inline(always)]
    pub unsafe fn yolo_header(&mut self) -> libc::uintptr_t {
        self.header.get_mut().clone()
    }

    /// Set header in an unsafe manner. This is not thread-safe w.r.t. GC threads
    pub unsafe fn yolo_set_header(&mut self, header: usize) {
        *self.header.get_mut() = header;
    }

    // pointer to type of this value
    #[inline(always)]
    pub fn type_tag(&self) -> libc::uintptr_t {
        self.read_header().type_tag()
    }

    // bits used for GC etc.
    #[inline(always)]
    pub fn nontype_tag(&self) -> libc::uintptr_t {
        self.read_header().nontype_tag()
    }

    // accessors to get the associated value
    pub fn get_value(&self) -> &JlValue {
        unsafe {
            mem::transmute((self as * const JlTaggedValue).offset(1))
        }
    }

    pub fn mut_value(&mut self) -> &mut JlValue {
        unsafe {
            mem::transmute((self as * mut JlTaggedValue).offset(1))
        }
    }
}

unsafe fn jl_typeof(v: * const JlValue) -> * mut JlDatatype {
    (*as_jltaggedvalue(v)).type_tag() as * mut JlDatatype
}

trait GcTag {
    fn tag(&self) -> u8;
    fn set_tag(&mut self, tag: u8);
    fn marked(&self) -> bool;
    fn set_marked(&mut self, flag: bool);
    fn old(&self) -> bool;
    fn set_old(&mut self, flag: bool);
    fn type_tag(&self) -> libc::uintptr_t;
    fn nontype_tag(&self) -> libc::uintptr_t;
}

impl GcTag for usize {
    // this is bits in Julia
    #[inline(always)]
    fn tag(&self) -> u8 {
        self.get_bits(TAG_RANGE) as u8
    }

    #[inline(always)]
    fn set_tag(&mut self, tag: u8) {
        self.set_bits(TAG_RANGE, tag as usize);
    }

    #[inline(always)]
    fn marked(&self) -> bool {
        self.get_bit(0)
    }

    #[inline(always)]
    fn set_marked(&mut self, flag: bool) {
        self.set_bit(0, flag);
    }

    #[inline(always)]
    fn old(&self) -> bool {
        self.get_bit(1)
    }

    #[inline(always)]
    fn set_old(&mut self, flag: bool) {
        self.set_bit(1, flag);
    }


    // pointer to type of this value
    #[inline(always)]
    fn type_tag(&self) -> libc::uintptr_t {
        self & (!0x0f)
    }

    // bits used for GC etc.
    #[inline(always)]
    fn nontype_tag(&self) -> libc::uintptr_t {
        self & 0x0f
    }
}

impl GcTag for u8 {
    // this is bits in Julia
    #[inline(always)]
    fn tag(&self) -> u8 {
        self.get_bits(TAG_RANGE)
    }

    #[inline(always)]
    fn set_tag(&mut self, tag: u8) {
        self.set_bits(TAG_RANGE, tag);
    }

    #[inline(always)]
    fn marked(&self) -> bool {
        self.get_bit(0)
    }

    #[inline(always)]
    fn set_marked(&mut self, flag: bool) {
        self.set_bit(0, flag);
    }

    #[inline(always)]
    fn old(&self) -> bool {
        self.get_bit(1)
    }

    #[inline(always)]
    fn set_old(&mut self, flag: bool) {
        self.set_bit(1, flag);
    }

    // pointer to type of this value
    #[inline(always)]
    fn type_tag(&self) -> libc::uintptr_t {
        panic!("u8 has no type tag")
    }

    // bits used for GC etc.
    #[inline(always)]
    fn nontype_tag(&self) -> libc::uintptr_t {
        (self & 0x0f) as usize
    }
}

impl GcTag for AtomicUsize {
    #[inline(always)]
    fn tag(&self) -> u8 {
        self.load(Ordering::Relaxed).tag()
    }

    #[inline(always)]
    fn set_tag(&mut self, tag: u8) {
        self.get_mut().set_tag(tag)
    }

    #[inline(always)]
    fn marked(&self) -> bool {
        self.load(Ordering::Relaxed).marked()
    }

    #[inline(always)]
    fn set_marked(&mut self, flag: bool) {
        self.get_mut().set_marked(flag)
    }

    #[inline(always)]
    fn old(&self) -> bool {
        self.load(Ordering::Relaxed).old()
    }

    #[inline(always)]
    fn set_old(&mut self, flag: bool) {
        self.get_mut().set_old(flag)
    }

    #[inline(always)]
    fn type_tag(&self) -> libc::uintptr_t {
        self.load(Ordering::Relaxed).type_tag()
    }

    #[inline(always)]
    fn nontype_tag(&self) -> libc::uintptr_t {
        self.load(Ordering::Relaxed).nontype_tag()
    }
}

#[cfg(test)]
mod jltagged_value_tests {
    use super::*;

    #[test]
    fn test_create() {
        // Note: a JlValue is just a libc::c_void type (in c_interface.rs)
        unsafe {
            let i: *mut i64 = libc::malloc(mem::size_of::<i64>()) as *mut i64;
            *i = 42;
            assert_eq!(*i, 42);
            libc::free(i as *mut JlValue);
            // TODO finish test
            let v = JlTaggedValue { header: AtomicUsize::new(0) };
        }
    }

    #[test]
    fn test_next() {
    }

    #[test]
    fn test_next_mut() {
    }

    #[test]
    fn test_typ() {
    }

    #[test]
    fn test_tag() {
    }

    #[test]
    fn test_set_tag() {
    }
}

// A GC Pool used for pooled allocation
pub struct GcPool<'a> {
    freelist: Vec<&'a mut JlTaggedValue>, // list of free objects, a vec is more packed
    newpages: Vec<JlTaggedValue>, // list of chunks of free objects (an optimization...)
    osize: usize                  // size of each object in this pool, could've been u16
}

impl<'a> GcPool<'a> {
    pub fn new(size: usize) -> Self {
        GcPool {
            freelist: Vec::new(),
            newpages: Vec::new(), // optimization, currently unused
            osize: size,
        }
    }

    #[inline(always)]
    pub fn clear_freelist(&mut self) {
        // self.freelist.clear()
        self.freelist = Vec::new()
    }
}

#[repr(C)]
pub struct WeakRef {
    // JL_DATA_TYPE exists before the pointer
    pub value: * mut JlValue,
}

impl JlValueMarker for WeakRef {
}

#[repr(C)]
pub struct JlBinding<'a> { // Currently unused (easier to know size at certain moments)
    pub name: * mut JlSym,
    pub value: * mut JlValue,
    pub globalref: * mut JlValue,
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

impl<'a> JlValueMarker for JlBinding<'a> {
}

// Pool page metadata
#[repr(C)]
pub struct PageMeta<'a> {
    pub pool_n:     u8,   // idx of pool that owns this page
    // TODO: make following bools after transitioning to Rust
    pub has_marked: AtomicBool,   // whether any cell is marked in this page
    pub has_young:  AtomicBool,   // whether any live and young cells are in this page, before sweeping
    pub nold:       AtomicU16,  // #old objects
    pub prev_nold:  u16,  // #old object during previous sweep
    pub nfree:      u16,  // #free objects, invalid if pool that owns this page is allocating from it
    pub osize:      u16,  // size of each object in this page
    pub fl_begin_offset: u16, // offset of the first free object
    pub fl_end_offset:   u16, // offset of the last free object
    pub thread_n: u16, // thread id of the heap that owns this page
    pub data: Option<&'a mut [u8]>, // we are currently not using this, try removing it and see what breaks!
    pub ages: Option<Box<BitVec>>,
}

impl<'a> PageMeta<'a> {
    pub fn new() -> Self {
        PageMeta {
            pool_n:     0,
            has_marked: AtomicBool::new(false),
            has_young:  AtomicBool::new(false),
            nold:       AtomicU16::new(0),
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

    // similar to `reset_page` in Julia but doesn't add a pointer to page data
    // and doesn't do the newpages optimization
    #[inline(always)]
    pub fn reset(&mut self, poolIndex: u8) -> (usize, usize) {
        self.pool_n = poolIndex;
        // make sure that we have enough offset to fit a pointer, this can be
        // used for newpages optimization
        debug_assert!(GC_PAGE_OFFSET >= mem::size_of::<* mut libc::c_void>());
        let n_ages = PAGE_SZ / self.osize as usize;
        let mut ages = match self.ages.take() {
            None => {
                let bv = Box::new(BitVec::with_capacity(n_ages));

                bv
            }
            Some(mut ages) => {
                ages.clear();
                let capacity = ages.capacity();

                if capacity < n_ages {
                    ages.reserve_exact(n_ages - capacity);
                }
                ages
            }
        };

        for age in ages.iter_mut() {
            *age.get_mut() = false;
        }

        for _ in ages.len()..n_ages {
            ages.push(AtomicBool::new(false));
        }

        ages.shrink_to_fit(); // TODO: if this becomes a performance hog, we can drop it

        self.ages = Some(ages);

        let size = mem::size_of::<JlTaggedValue>() + self.osize as usize;
        // size of the data portion of the page, after aligning to 16 bytes after each tag
        let aligned_pg_size = PAGE_SZ - GC_PAGE_OFFSET;
        // padding to align the object to Julia's required alignment
        let padding = (size - JL_SMALL_BYTE_ALIGNMENT) % JL_SMALL_BYTE_ALIGNMENT;
        self.nfree = (aligned_pg_size / (size + padding) as usize) as u16;

        (size, padding)
    }
}

// Thread-local heap
// lifetimes don't mean anything yet
pub struct ThreadHeap<'a> {
    // pools
    pools: Vec<GcPool<'a>>, // This has size GC_N_POOLS!, could have been an array, but copy only implemented for simpler things, so use a vec
    // weak refs
    weak_refs: Vec<* mut WeakRef>,
    // malloc'd arrays
    mallocarrays: Vec<MallocArray>,
    mafreelist: Vec<MallocArray>,
    // big objects
    pub big_objects: Vec<&'a mut BigVal>,
    // remset
    rem_bindings: Vec<&'a mut JlBinding<'a>>,
    pub remset: Vec<* mut JlValue>,
    pub last_remset: Vec<* mut JlValue>,
    pub remset_nptr: usize,
}

impl<'a> ThreadHeap<'a> {
    pub fn new() -> Self {
        let mut pools = Vec::with_capacity(GC_N_POOLS);
        for size in GC_SIZE_CLASSES.iter() {
            pools.push(GcPool::new(*size));
        }

        ThreadHeap {
            pools: pools,
            weak_refs: Vec::new(),
            mallocarrays: Vec::new(),
            mafreelist: Vec::new(),
            big_objects: Vec::new(),
            rem_bindings: Vec::new(),
            remset: Vec::new(),
            last_remset: Vec::new(),
            remset_nptr: 0,
        }
    }
}

const BIG_OBJ_CACHE_SIZE: usize = 1024;

pub struct MarkCache {
    // thread-local statistics, will be merged into global during stop-the-world
    pub perm_scanned_bytes: usize,
    pub scanned_bytes: usize,
    pub nbig_obj: usize, // # of queued big objects to be moved to old gen.
    pub big_obj: [* mut BigVal; BIG_OBJ_CACHE_SIZE],
    // GC-thread local cache for remsets
    pub remset_nptr: usize,
    pub remset: Vec<* mut JlValue>,
    // secondary big object list for GC thread mark caches
    big_obj_list: Vec<* mut BigVal>,
}


/// to-be thread-local mark cache for GC threads. TODO: make this thread-local
pub static mut mark_caches: Option<HashMap<thread::ThreadId, MarkCache>> = None;

/// Get _GC thread-local_ mark cache used for marking. This function
/// does not provide access to Julia threads' mark caches. To access
/// those mark caches, use the coressponding JlTLS object or Gc2
/// object.
pub fn gc_cache<'a>() -> &'a mut MarkCache {
    let mc = unsafe { mark_caches.as_mut().unwrap() };
    let tid = thread::current().id();
    mc.entry(tid).or_insert_with(|| MarkCache::new())
}

impl MarkCache {
    pub fn new() -> Self {
        MarkCache {
            perm_scanned_bytes: 0,
            scanned_bytes: 0,
            nbig_obj: 0,
            big_obj: [::std::ptr::null_mut(); BIG_OBJ_CACHE_SIZE],
            remset_nptr: 0,
            remset: Vec::new(),
            big_obj_list: Vec::new(),
        }
    }

    pub fn setmark_buf(&mut self, o: * mut JlValue, mark_mode: u8, minsz: usize) {
        let buf = unsafe {
            &mut *as_mut_jltaggedvalue(o)
        };
        let tag = buf.read_header();

        if tag.marked() {
            return;
        }

        let mut bits = 0;

        if unsafe { intrinsics::likely(Marking::setmark_tag(buf, mark_mode, tag, &mut bits)) } && ! get_gc_verifying() {
            if minsz <= GC_MAX_SZCLASS {
                let maybe_meta = unsafe {
                    pg_mgr().find_pagemeta(o)
                };
                match maybe_meta {
                    Some(meta) => {
                        // object belongs to a pool managed by page manager
                        self.setmark_pool_(buf, bits, meta);
                        return;
                    }
                    None => ()
                }
            }
            // object doesn't belong to a pool
            self.setmark_big(buf, bits);
        }
    }

    // update metadata of the page the *marked* pool-allocated object lies in
    fn setmark_pool_(&mut self, o: * mut JlTaggedValue, mark_mode: u8, meta: &mut PageMeta) {
        if cfg!(feature="memdebug") {
            return self.setmark_big(o, mark_mode);
        }

        if mark_mode == GC_OLD_MARKED {
            self.perm_scanned_bytes += meta.osize as usize;
            meta.nold.fetch_add(1, Ordering::Relaxed);
        } else {
            self.scanned_bytes += meta.osize as usize;

            if get_mark_reset_age() != 0 {
                meta.has_young.store(true, Ordering::Relaxed);
                unsafe {
                    let page_begin = Page::of_raw(o).offset(GC_PAGE_OFFSET as isize);
                    let obj_id = page_begin.offset_to(mem::transmute::<* mut JlTaggedValue, * const u8>(o)).unwrap() as usize / meta.osize as usize;
                    // set age of the object in memory pool atomically
                    meta.ages.as_mut().unwrap()[obj_id / 8].fetch_and(true, Ordering::Relaxed);
                }
            }
        }
    }

    unsafe fn setmark_pool(&mut self, o: * mut JlTaggedValue, mark_mode: u8) {
        if cfg!(feature="memdebug") {
            return self.setmark_big(o, mark_mode);
        }

        let meta = pg_mgr().find_pagemeta(o).unwrap();
        self.setmark_pool_(o, mark_mode, meta);
    }

    /// Update metadata of the *marked* big object. This method should
    /// be called *only once* per object.
    fn setmark_big(&mut self, o: * mut JlTaggedValue, mark_mode: u8) {
        debug_assert!(unsafe { pg_mgr().find_pagemeta(o).is_none() }, "Tried to process marked pool-allocated object as marked big object");

        let hdr = unsafe{
            BigVal::from_mut_jltaggedvalue(&mut *o)
        };

        let nbytes = hdr.size(); // the size() method does untagging already

        if mark_mode == GC_OLD_MARKED {
            // object is old
            self.perm_scanned_bytes += nbytes;
            self.gc_queue_big_marked(hdr, false);
        } else {
            self.scanned_bytes += nbytes;
            // object may be young, may be old. however, if object's
            // age is 0 then it has to be young
            if get_mark_reset_age() != 0 && hdr.age() != 0 {
                // reset the age
                hdr.set_age(0);
                self.gc_queue_big_marked(hdr, true);
            }
        }

        // TODO: objprofile_count(jl_typeof(jl_valueof(o)), mark_mode == GC_OLD_MARKED, nbytes)
    }

    #[inline(always)]
    unsafe fn setmark(&mut self, o: * mut JlTaggedValue, mark_mode: u8, sz: usize) {
        if sz <= GC_MAX_SZCLASS {
            self.setmark_pool(o, mark_mode);
        } else {
            self.setmark_big(o, mark_mode);
        }
    }


    #[inline(always)]
    fn gc_queue_big_marked(&mut self, hdr: &mut BigVal, toyoung: bool) {
        let nentry = BIG_OBJ_CACHE_SIZE;
        let mut nobj = self.nbig_obj;

        if unsafe { intrinsics::unlikely(nobj >= nentry) } {
            self.sync_self_cache();
            nobj = 0;
        }

        if hdr.in_list == true {
            return;
        }

        let v = if toyoung {
            ((hdr as * mut BigVal as usize) | 1) as * mut BigVal
        } else {
            hdr as * mut BigVal
        };

        unsafe {
            hdr.in_list = false;
            hdr.slot = nobj;
        }

        self.big_obj[nobj] = v;
        self.nbig_obj = nobj + 1;
    }

    fn sync_self_cache(&mut self) {
        let nbig = self.nbig_obj;

        for i in 0..nbig {
            let ptr = self.big_obj[i].clone();
            let hdr = unsafe {
                &mut *((ptr as usize).clear_tag(1) as * mut BigVal)
            };

            Gc2::unlink_big_object(hdr);

            if ((ptr as usize) & 1) != 0 {
                // move to big_obj_list, a.k.a. "toyoung"
                hdr.slot = self.big_obj_list.len();
                hdr.tid = -2; // normally, we must remember where this one went.
                hdr.in_list = true;
                self.big_obj_list.push(hdr);
            } else {
                // move from `big_objects` to `big_objects_marked`
                unsafe {
                    // TODO: fix my attempt at making thread-safe
                    let mut bo: MutexGuard<Vec<* mut BigVal>> = big_objects_marked.as_mut().unwrap().lock().unwrap();
                    (*bo).push(hdr);
                    hdr.in_list = true;
                    hdr.slot = (*bo).len();
                    hdr.tid = -1;
                }
            }
        }

        self.nbig_obj = 0;
    }

    /// Synchronize caches without locking. Caller must guarantee that this is called in a single-threaded context.
    pub unsafe fn sync_cache_nolock(&mut self, local_obj_list: &mut Vec<&mut BigVal>, tid: i16) {
        let nbig = self.nbig_obj;

        for i in 0..nbig {
            let ptr = self.big_obj[i].clone();
            let hdr = unsafe {
                &mut *((ptr as usize).clear_tag(1) as * mut BigVal)
            };

            Gc2::unlink_big_object(hdr);

            if ((ptr as usize) & 1) != 0 {
                hdr.slot = local_obj_list.len();
                hdr.tid = tid;
                hdr.in_list = true;
                local_obj_list.push(hdr);
            } else {
                // move from `big_objects` to `big_objects_marked`
                unsafe {
                    // get the value without locking the global object. this is not thread-safe but ok.
                    let mut bo = big_objects_marked.as_mut().unwrap().get_mut().unwrap();
                    (*bo).push(hdr);
                    hdr.in_list = true;
                    hdr.slot = (*bo).len();
                    hdr.tid = -1;
                }
            }
        }

        self.nbig_obj = 0;

        unsafe {
            perm_scanned_bytes += self.perm_scanned_bytes;
            scanned_bytes += self.scanned_bytes;
        }

        self.perm_scanned_bytes = 0;
        self.scanned_bytes = 0;
    }

    /// Synchronize unmarked big objects
    pub fn sync_big_objects(&mut self, gc: &mut Gc2) {
        // simulate linking to that list
        let start = gc.heap.big_objects.len();
        gc.heap.big_objects.append(unsafe {
            mem::transmute::<&mut Vec<*mut BigVal>, &mut Vec<&mut BigVal>>(&mut self.big_obj_list)
        });
        for i in start..gc.heap.big_objects.len() {
            let ref mut hdr = gc.heap.big_objects[i];
            assert!(hdr.in_list);
            hdr.slot = i;
            hdr.tid = gc.tid;
        }
    }

    pub fn sync_remset(&mut self, gc: &mut Gc2) {
        gc.heap.remset.append(&mut self.remset);
        gc.heap.remset_nptr += self.remset_nptr;
        self.remset_nptr = 0;
    }
}

#[repr(C)]
pub struct GcFrame {
    nroots: usize,
    prev: * mut GcFrame,
    // actual roots appear here
}

/// Marking part of the garbage collector. Separation of this and the rest makes thread-safety easier to achieve.
pub struct Marking {
    // mark stack for marking on this thread
    mark_stack: ConcurrentStack<* mut JlValue>,
}

impl Marking {
    pub fn new() -> Self {
        Marking {
            mark_stack: ConcurrentStack::new(),
        }
    }

    pub fn mark_roots(&self) {
        // modules
        self.push_root(unsafe { (*jl_main_module).as_mut_jlvalue() }, MAX_MARK_DEPTH);
        self.push_root(unsafe { (*jl_internal_main_module).as_mut_jlvalue() }, MAX_MARK_DEPTH);

        // invisible builtin values
        if ! jl_an_empty_vec_any.is_null() {
            self.push_root(jl_an_empty_vec_any, MAX_MARK_DEPTH);
        }
        if ! jl_module_init_order.is_null() {
            self.push_root(unsafe { (*jl_module_init_order).as_mut_jlvalue() }, MAX_MARK_DEPTH);
        }
        let f = unsafe { jl_cfunction_list.unknown };
        self.push_root(f, MAX_MARK_DEPTH);
        self.push_root(unsafe { (*jl_anytuple_type_type).as_mut_jlvalue() }, MAX_MARK_DEPTH);
        self.push_root(jl_ANY_flag, MAX_MARK_DEPTH);

        for i in 0..N_CALL_CACHE {
            if ! call_cache[i].is_null() {
                self.push_root(call_cache[i], MAX_MARK_DEPTH);
            }
        }

        if ! jl_all_methods.is_null() {
            self.push_root(unsafe { (*jl_all_methods).as_mut_jlvalue() }, MAX_MARK_DEPTH);
        }

        // constants
        self.push_root(unsafe { (*jl_typetype_type).as_mut_jlvalue() }, MAX_MARK_DEPTH);
        self.push_root(unsafe { (*jl_emptytuple_type).as_mut_jlvalue() }, MAX_MARK_DEPTH);
    }

    pub fn walk_roots(&self) {
        debug_assert!(self.mark_stack.is_empty());

        // finished premark, mark remsets and thread local roots
        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * t.tl_gcs };
            self.mark_remset(tl_gc); // TODO: make this just tl_gc to separate marking even better
            self.mark_thread_local(tl_gc); // TODO: separate these from self
        }

        // walk the roots
        self.mark_roots();
        self.visit_mark_stack(); // this function processes all the pushed roots
    }

    #[inline(never)]
    pub fn mark_finalizers(&self, orig_marked_len: usize) {
        // mark remaining finalizers
        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * t.tl_gcs };
            // this is self, not t!
            self.mark_object_list(&mut t.finalizers, 0);
        }

        unsafe {
            // check only the remainder of finalizer_list_marked
            self.mark_object_list(&mut finalizer_list_marked, orig_marked_len);
        }

        // visit mark stack once before resetting mark_reset_age (in case of extra markings happened during finalizers?)
        self.visit_mark_stack();
        set_mark_reset_age(1);

        // reset the age and old bit for any unmarked objects
        // referenced by to_finalize list. Note that these objects
        // can't be accessed outside `to_finalize` since they are
        // still unmarked.
        self.mark_object_list(unsafe { &mut to_finalize }, 0);
        self.visit_mark_stack();

        set_mark_reset_age(0);
    }

    fn push_root(&self, e: *mut JlValue, d: i32) -> i32 {
        // N.B. Julia has `gc_findval` to interact with GDB for finding the gc-root for a value.
        // We should implement something similar for simpler debugging

        debug_assert!(! e.is_null());

        let o = unsafe { &mut *as_mut_jltaggedvalue(e) };
        // TODO: verify_val(v);
        let tag = o.read_header();
        if ! tag.marked() {
            let mut bits: u8 = 0;
            if unsafe { intrinsics::likely(Marking::setmark_tag(o, GC_MARKED, tag, &mut bits)) } {
                let tag = tag & !0xf;
                if ! get_gc_verifying() {
                    // self.mark_obj(e, tag, bits);
                }
                self.scan_obj(&e, d, tag, bits);
            }
            return (! (bits as usize).old()) as i32;
        }
        return (! tag.old()) as i32;
    }

    #[inline(always)]
    fn push_root_if_not_null<T: JlValueLike>(&self, p: * mut T, d: i32) {
        if ! p.is_null() {
            self.push_root(unsafe { (* p).as_mut_jlvalue() }, d);
        }
    }

    #[inline(always)]
    fn scan_obj3(&self, v: &* mut JlValue, d: i32, tag: usize) {
        self.scan_obj(v, d, tag & !15, (tag & 0xf) as u8);
    }

    // Julia's gc marks the object and recursively marks its children, queueing objecs
    // on mark stack when recursion depth is too great.
    fn scan_obj(&self, v: &*mut JlValue, _d: i32, tag: libc::uintptr_t, bits: u8) {
        let vt: *const JlDatatype = tag as *mut JlDatatype;
        let mut nptr = 0;
        let mut refyoung = 0;

        debug_assert!(! v.is_null());
        debug_assert_ne!(bits & GC_MARKED, 0);
        debug_assert_ne!(vt, jl_symbol_type); // should've checked in `gc_mark_obj`

        if vt == jl_weakref_type {
            return // don't mark weakrefs
        }

        if unsafe { (*(*vt).layout).npointers() == 0 } {
            return; // fast path for pointerless types
        }

        let d = _d + 1;
        if d >= MAX_MARK_DEPTH {
            // queue the root
            self.mark_stack.push(*v);
            return;
        }

        if vt == jl_simplevector_type {
            let vec = *v as *const JlSVec;
            let data = unsafe { np_jl_svec_data(*v) };
            let l = unsafe { (*vec).length };
            nptr += 1;
            let elements: &mut[* mut JlValue] = unsafe { slice::from_raw_parts_mut(data, l as usize) };
            let mut i = 0;
            for e in elements {
                if ! (*e).is_null() {
                    verify_parent!("svec", *v, e, format!("elem({})", i));
                    refyoung |= self.push_root(*e, d);
                }
                i += 1;
            }
        } else if unsafe { (*vt).name == jl_array_typename } {
            let a = unsafe {
                JlArray::from_jlvalue_mut(&mut **v)
            };
            let flags = a.flags.clone();
            if flags.how() == AllocStyle::HasOwnerPointer {
                let owner = a.data_owner_mut();
                refyoung |= self.push_root(owner, d);
            } else if flags.how() == AllocStyle::JlBuffer {
                let buf_ptr = unsafe {
                    mem::transmute::<* mut u8, * mut JlValue>((a.data as * mut u8).offset(- (a.offset as isize * a.elsize as isize)))
                };
                let val_buf = unsafe {
                    as_mut_jltaggedvalue(buf_ptr)
                };
                verify_parent!("array", *v, unsafe { mem::transmute(&val_buf) }, "buffer ('loc' addr is meaningless)");
                // N.B. In C there is the statement `(void)val_buf` here for some reason.
                gc_cache().setmark_buf(buf_ptr, bits, a.nbytes());
            }

            if flags.ptrarray() && ! a.data.is_null() {
                let l = a.length as usize;

                if l > 100000 && d > MAX_MARK_DEPTH - 10 {
                    // don't mark long arrays at hight depth to avoid copying
                    // the whole array into the mark queue, instead queue the
                    // array pointer.
                    self.mark_stack.push(*v);
                    return;
                } else {
                    nptr += l;
                    let data = unsafe {
                        slice::from_raw_parts(a.data as * const * mut JlValue, l)
                    };

                    // queue elements for marking
                    for i in 0..l {
                        let elt = data[i];
                        if ! elt.is_null() {
                            // N.B. I'm not sure about the &elt part
                            verify_parent!("array", *v, &elt, format!("elem({})", i));
                            refyoung |= self.push_root(elt, d);
                        }
                    }
                }
            }
        } else if vt == jl_module_type {
            // should increase nptr here, according to Julia's GC implementation
            refyoung |= self.mark_module(JlModule::from_jlvalue_mut(unsafe { &mut **v }), d, bits);
        } else if vt == jl_task_type {
            // same nptr increment thing
            self.gc_mark_task(JlTask::from_jlvalue_mut(unsafe { &mut **v }), d, bits);
            // tasks should always be remarked since Julia doesn't trigger the
            // write barrier for stores to stack slots, it does so only for
            // values on heap
            refyoung = 1;
        } else {
            let layout = unsafe {
                &*(*vt).layout
            };
            let nf = layout.nfields;
            let npointers = layout.npointers();
            nptr += ((npointers & 0xff) as usize) << (npointers & 0x300);

            for i in 0..nf {
                if unsafe { np_jl_field_isptr(vt, i as i32) != 0 } {
                    let slot = unsafe {
                        &*((*v as * mut u8).offset(np_jl_field_offset(vt, i as i32) as isize) as * mut * mut JlValue)
                    };
                    let fld = unsafe { *slot };
                    if ! fld.is_null() {
                        verify_parent!("object", *v, slot, format!("field({})", i));
                        refyoung |= self.push_root(fld, d);
                    }
                }
            }
        }

        // label 'ret:
        if bits == GC_OLD_MARKED && refyoung > 0 && ! get_gc_verifying() {
            // use marking thread's remset rather than Julia thread's remset for fast thread-safety
            // for now, these remsets are in gc_cache()
            gc_cache().remset_nptr += nptr;
            gc_cache().remset.push(*v);
        }
    }

    /// Update metadata of a marked object without scanning it
    fn mark_obj(&self, v: * mut JlValue, tag: usize, bits: u8) {
        debug_assert!(! v.is_null());
        debug_assert!((bits as usize).marked());

        let o: * mut JlTaggedValue = as_mut_jltaggedvalue(v);
        let vtref = tag.type_tag() as * const JlDatatype;
        let vt = unsafe { &mut * (vtref as * mut JlDatatype) };

        Marking::assert_datatype(vt);

        debug_assert!(vtref != jl_symbol_type);

        if vtref == jl_simplevector_type {
            let vec = v as * const JlSVec;
            let l = unsafe { (*vec).length };

            unsafe {
                gc_cache().setmark(o, bits, l * mem::size_of::<* const libc::c_void>() + mem::size_of::<JlSVec>());
            }

        } else if vt.name == jl_array_typename {
            let a = unsafe { &*(v as * const JlArray) };
            let ref flags = a.flags;

            if flags.pooled() {
                unsafe {
                    gc_cache().setmark_pool(o, bits);
                }
            } else {
                gc_cache().setmark_big(o, bits);
            }

            if flags.how() == AllocStyle::MallocBuffer {
                // array is malloc'd

                // In C:
                // objprofile_count(jl_malloc_tag, bits == GC_OLD_MARKED, a.nbytes())

                if bits == GC_OLD_MARKED {
                    gc_cache().perm_scanned_bytes += a.nbytes();
                } else {
                    gc_cache().scanned_bytes += a.nbytes();
                }
            }
        } else if vtref == jl_module_type {
            unsafe {
                gc_cache().setmark(o, bits, mem::size_of::<JlModule>());
            }
        } else if vtref == jl_task_type {
            unsafe {
                gc_cache().setmark(o, bits, mem::size_of::<JlTask>());
            }
        } else if vtref == jl_string_type {
            unsafe {
                // length of the string
                let len = *(v as * const usize);
                gc_cache().setmark(o, bits, len + mem::size_of::<usize>() + 1);
            }
        } else {
            unsafe {
                gc_cache().setmark(o, bits, vt.size as usize);
            }
        }
    }

    #[inline(always)]
    fn assert_datatype(vt: * mut JlDatatype) {
        if cfg!(debug_assertions) {
            unsafe {
                if intrinsics::unlikely(jl_typeof((*vt).as_jlvalue()) != jl_datatype_type) {
                    np_corruption_fail(vt);
                }
            }
        }
    }

    fn setmark_tag(o: &mut JlTaggedValue, mark_mode: u8, tag: usize, bits: &mut u8) -> bool {
        debug_assert!(! tag.marked());
        debug_assert!((mark_mode as usize).marked(), format!("Found mark_mode {} rather than a marked one", mark_mode));

        let (tag, mark_mode) = if get_mark_reset_age() != 0 {
            // reset the object's age to young, as if it is just allocated
            let mut t = tag.clone();
            t.set_tag(GC_MARKED);
            (t, GC_MARKED)
        } else {
            let mark_mode = if tag.old() {
                GC_OLD_MARKED
            } else {
                mark_mode
            };
            (tag | mark_mode as usize, mark_mode)
        };

        debug_assert!(tag & 0x3 == mark_mode as usize, format!("tag has mark bits {} but mark mode is {}", tag & 0x3, mark_mode));

        *bits = mark_mode;
        let old_tag = o.header.swap(tag, Ordering::Relaxed);
        // TODO: verify_val(jl_valueof(o)) !!!
        ! old_tag.marked()
    }

    fn mark_remset(&self, other: &mut Gc2) {
        for i in 0..other.heap.last_remset.len() {
            // cannot borrow array item because non-lexical borrowing hasn't landed to Rust yet
            let item = other.heap.last_remset[i].clone();
            let tag = unsafe { &*as_jltaggedvalue(item) };
            self.scan_obj3(&item, MAX_MARK_DEPTH, tag.read_header());
        }

        let mut n_bnd_refyoung = 0;

        for i in 0..other.heap.rem_bindings.len() {
            if other.heap.rem_bindings[i].value.is_null() {
                continue;
            }

            let is_young = self.push_root(other.heap.rem_bindings[i].value, MAX_MARK_DEPTH) != 0; // for lexical borrow

            if is_young {
                // reusing processed indices
                other.heap.rem_bindings.swap(i, n_bnd_refyoung);
                n_bnd_refyoung += 1;
            }
        }

        other.heap.rem_bindings.truncate(n_bnd_refyoung);
    }

    /// Visit all objects queued to the mark stack
    pub fn visit_mark_stack(&self) {
        let thread_pool = unsafe {
            np_threads.as_mut().unwrap()
        };

        // the outer loop is for the cases where the stack becomes
        // empty while we are synchronizing
        while ! self.mark_stack.is_empty() && ! Gc2::should_timeout() {
            // when the scope gets dropped, i.e. when this
            // function returns, the threads will join
            // automatically.
            thread_pool.scoped(|scope| {
                while ! self.mark_stack.is_empty() && ! Gc2::should_timeout() {
                    // casting to let Rust send this pointer over threads
                    let v = self.mark_stack.pop().unwrap() as usize;
                    let header = unsafe { &*as_jltaggedvalue(v as * mut JlValue) }.read_header();
                    debug_assert_ne!(header, 0);
                    scope.execute(move || {
                        self.scan_obj3(&(v as * mut JlValue), 0, header);
                    });
                }
            });
        }

        assert!(self.mark_stack.is_empty());
    }


    #[inline(always)]
    unsafe fn read_rt_stack<T>(addr: * mut T, offset: usize, lb: usize, ub: usize) -> usize {
        let a = addr as usize;
        // correct address if it is within bounds
        let real_addr = if a >= lb && a < ub {
            a + offset
        } else {
            a
        };
        *mem::transmute::<usize, * const usize>(real_addr)
    }

    fn mark_rt_stack(&self, sinit: * mut GcFrame, offset: usize, lb: usize, ub: usize, d: i32) {
        // leave all hope, ye who enter here
        // for that there is no more safety guarantees and only memory transmutation

        let mut s = sinit;

        while ! s.is_null() {
            let nroots = unsafe {
                Marking::read_rt_stack(&mut (&mut *s).nroots, offset, lb, ub)
            };
            let nr = nroots >> 1;
            let rts = unsafe {
                slice::from_raw_parts_mut((s as * mut * mut libc::c_void).offset(2) as * mut * mut * mut JlValue, nr)
            };

            if nroots & 1 != 0 {
                // stack is indirected
                for i in 0..nr {
                    unsafe {
                        // read stack slot
                        let slot: * mut * mut libc::c_void = mem::transmute(Marking::read_rt_stack(&mut rts[i], offset, lb, ub));
                        // read object itself
                        let obj: * mut libc::c_void = mem::transmute(Marking::read_rt_stack(slot, offset, lb, ub));

                        if ! obj.is_null() {
                            self.push_root(obj, d);
                        }
                    }
                }
            } else {
                // stack has no indirection
                for i in 0..nr {
                    // read object
                    let obj: * mut libc::c_void = unsafe {
                        mem::transmute(Marking::read_rt_stack(&mut rts[i], offset, lb, ub))
                    };
                    if ! obj.is_null() {
                        self.push_root(obj, d);
                    }
                }
            }

            unsafe {
                s = mem::transmute(Marking::read_rt_stack(&mut (*s).prev, offset, lb, ub));
            }
        }
    }

    pub fn mark_thread_local(&self, other: &mut Gc2) {
        let ref tls = other.tls;
        let m = tls.current_module.clone();
        let ct = tls.current_task.clone();
        let rt = tls.root_task.clone();
        let exn = tls.exception_in_transit.clone();
        let ta = tls.task_arg_in_transit.clone();

        self.push_root_if_not_null(m, MAX_MARK_DEPTH);
        self.push_root_if_not_null(ct, MAX_MARK_DEPTH);
        self.push_root_if_not_null(rt, MAX_MARK_DEPTH);
        self.push_root_if_not_null(exn, MAX_MARK_DEPTH);
        self.push_root_if_not_null(ta, MAX_MARK_DEPTH);
    }

    fn mark_module(&self, m: &mut JlModule, d: i32, bits: u8) -> i32 {
        let mut refyoung = 0;
        let mut table = unsafe {
            slice::from_raw_parts_mut(m.bindings.table, m.bindings.size)
        };

        let mut i = 1;

        while i < m.bindings.size {
            if ! HTable::is_not_found(table[i]) {
                let b = unsafe {
                    JlBinding::from_jlvalue_mut(&mut *table[i])
                };
                gc_cache().setmark_buf(b.as_mut_jlvalue(), bits, mem::size_of::<JlBinding>());
                let vb = as_mut_jltaggedvalue(b.as_mut_jlvalue());
                verify_parent!("module", m.as_jlvalue(), &unsafe { mem::transmute(vb) }, "binding_buff");

                if ! b.value.is_null() {
                    verify_parent!("module", m.as_jlvalue(), &b.value, format!("binding({})", CStr::from_ptr(np_jl_symbol_name(b.name)).to_str().unwrap()));
                    refyoung |= self.push_root(b.value, d);
                }

                if ! b.globalref.is_null() {
                    refyoung |= self.push_root(b.globalref, d);
                }
            }

            i += 2;
        }

        for using in m.usings.as_slice_mut() {
            refyoung |= self.push_root(*using, d);
        }

        if ! m.parent.is_null() {
            refyoung |= self.push_root(unsafe { (&mut *m.parent).as_mut_jlvalue() }, d);
        }

        refyoung
    }

    fn gc_mark_task(&self, ta: &mut JlTask, d: i32, bits: u8) {
        if ! ta.parent.is_null() {
            self.push_root(unsafe { (&mut *ta.parent).as_mut_jlvalue() }, d);
        }

        self.push_root(ta.tls, d);
        self.push_root(ta.consumers, d);
        self.push_root(ta.donenotify, d);
        self.push_root(ta.exception, d);

        if ! ta.backtrace.is_null() {
            self.push_root(ta.backtrace, d);
        }

        if ! ta.start.is_null() {
            self.push_root(ta.start, d);
        }

        if ! ta.result.is_null() {
            self.push_root(ta.result, d);
        }

        self.gc_mark_task_stack(ta, d, bits);
    }

    fn gc_mark_task_stack(&self, ta: &mut JlTask, d: i32, bits: u8) {
        unsafe {
            // TODO: make this thread-safe
            gc_scrub_record_task(ta);
        }

        let stkbuf = ta.stkbuf != usize::max_value() as * mut libc::c_void && ! ta.stkbuf.is_null();
        let tid = ta.tid;
        let ptls2 = unsafe {
            &mut get_all_tls()[tid as usize]
        };

        if stkbuf {
            if cfg!(copy_stacks) {
                gc_cache().setmark_buf(ta.stkbuf, bits, ta.bufsz);
            } else {
                if ta as * mut JlTask != ptls2.root_task {
                    // TODO: give it to the corresponding thread?
                    gc_cache().setmark_buf(ta.stkbuf, bits, ta.ssize);
                }
            }
        }

        if ta as * mut JlTask == ptls2.current_task {
            // TODO: give it to the corresponding thread?
            self.mark_rt_stack(&mut *ptls2.pgcstack, 0, 0, usize::max_value(), d);
        } else if stkbuf {
            let (offset, lb, ub) = if cfg!(copy_stacks) {
                let ub = ptls2.stackbase as usize;
                let lb = ub - ta.ssize;
                (ta.stkbuf as usize - lb, lb, ub)
            } else {
                (0, 0, usize::max_value())
            };
            // TODO: give it to the corresponding thread?
            self.mark_rt_stack(ta.gcstack, offset, lb, ub, d);
        }
    }

    fn mark_object_list(&self, list: * mut JlArrayList, start: usize) {
        let l = unsafe { &mut *list };
        let len = l.len;
        let items = l.as_slice_mut();
        let mut i = start;

        while i < len {
            let mut v = items[i];
            if unsafe { intrinsics::unlikely(v.is_null()) } {
                i += 1;
                continue;
            }

            let vp = v as usize;

            if (vp & 1) != 0 {
                v = vp.clear_tag(1) as * mut libc::c_void;
                i += 1;
                debug_assert!(i < len);
            }

            self.push_root(v, 0);

            i += 1;
        }
    }
}

// Thread-local GC data
// Lifetimes here don't have a meaning, yet
pub struct Gc2<'a> {
    /// heap for the current _Julia_ thread
    pub heap: ThreadHeap<'a>,
    /// mark cache for thread-local marks
    pub cache: MarkCache,
    // Age of the world, used for promotion
    world_age: usize,
    // parent pointer to thread-local storage for other fields
    // we can access stack base etc. from here
    tls: &'static mut JlTLS, // we are lying to Rust right now, we will use this in an unsafe manner
    tid: i16, // ID of the owning thread
    // Data structure for marking part of the GC
    pub marking: Marking
}

impl<'a> Gc2<'a> {
    pub fn new(tls: &'static mut JlTLS) -> Self {
       Gc2 {
           heap: ThreadHeap::new(),
           cache: MarkCache::new(),
           world_age: 0,
           tid: tls.tid,
           tls: tls,
           marking: Marking::new(),
        }
    }

    #[inline(always)]
    pub fn collect_small(&mut self) -> bool {
        self.collect(false)
    }

    #[inline(always)]
    pub fn collect_full(&mut self) -> bool {
        self.collect(true)
    }

    // allocate a Julia object
    // Semi-equivalent(?) to: julia/src/gc.c:jl_gc_alloc
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
            // Set type of v. we are the only owner so this is OK here.
            (*as_mut_jltaggedvalue(v)).yolo_set_header(typ as usize);
        }
        v
    }

    // Semi-equivalent(?) to: julia/src/gc.c:jl_gc_pool_alloc
    pub fn pool_alloc(&mut self, size: usize) -> &mut JlValue {
        let osize = size - mem::size_of::<JlTaggedValue>();

        debug_assert_eq!(self.tls.gc_state, GcState::GcNotRunning); // make sure that GC is not working.

        if cfg!(feature="memdebug") {
            return self.big_alloc(size);
        }

        unsafe {
            // Julia's GC also doesn't use atomic increments for this. TODO: maybe switch to atomic increments
            *gc_num.allocd.get_mut() += size as i64;
        }
        if unsafe { intrinsics::unlikely(unsafe { *gc_num.allocd.get_mut() } > 0) || debug_check_pool() } {
            if ! (cfg!(feature="run_only_once") && GC_ALREADY_RUN.load(Ordering::SeqCst)) {
                // println!("triggering periodic collection");
                unsafe {
                    jl_gc_collect(0);
                }
            }
        } else {
            unsafe {
                np_jl_gc_safepoint_(self.tls);
            }
        }

        unsafe {
            gc_num.poolalloc += 1;
        }

        let v = match self.find_pool(&osize) {
            Some(pool_index) => {
                // TODO: check if pool is full, see below...
                // TODO: I'm not sure how to use pool.newpages yet...
                //
                // We are not using newpages and adding new pages to freelist for now.
                // We can implement newpages as an optimization later on.
                // TODO: do extra bookkeeping about marking pagemetas etc.
                if let Some(v) = self.heap.pools[pool_index].freelist.pop() {
                    let pool = &self.heap.pools[pool_index];
                    let meta = unsafe {
                        pg_mgr().find_pagemeta(v).unwrap()
                    };
                    // just a sanity check:
                    debug_assert_eq!(meta.osize as usize, pool.osize);
                    *meta.has_young.get_mut() = true;
                    meta.nfree -= 1;
                    /*
                    if let Some(next) = pool.freelist.last() {
                        unsafe { // this unsafe is here because `unlikely` is marked unsafe in Rust
                            if intrinsics::unlikely(Page::of(v) != Page::of(next)) {
                                meta.nfree = 0;
                            }
                        }
                    }
                    */
                    v
                } else {
                    self.add_page(pool_index);
                    let ref mut pool = self.heap.pools[pool_index];
                    let v = pool.freelist.pop().unwrap();
                    let meta = unsafe {
                        pg_mgr().find_pagemeta(v).unwrap()
                    };
                    // just a sanity check:
                    debug_assert_eq!(meta.osize as usize, pool.osize);
                    *meta.has_young.get_mut() = true;
                    meta.nfree -= 1;
                    v
                }
            },
            None => {
                // size of the object is too large for any pool, should've used alloc
                panic!(format!("Allocation error: object size {} is too large for pool", size));
            }
        };
        jl_value_of_mut(v)
    }

    fn add_page(&mut self, poolIndex: usize) {
        // TODO: rewrite this after moving regions to page manager for safety
        // allocate page
        let regions = unsafe {
            REGIONS.as_mut().unwrap()
        };
        let page = pg_mgr().alloc_page(regions);
        let region = unsafe {
            neptune_find_region(page).unwrap()
        };
        // get page meta
        let i = region.index_of(page).unwrap();
        let meta = &mut region.meta[i];
        // set up page meta
        let pool = &mut self.heap.pools[poolIndex];
        meta.osize = pool.osize as u16;
        meta.thread_n = self.tid as u16;
        /* TODO: enable later on!
        meta.data = Some(&mut page.data);
         */
        let (size, padding) = meta.reset(poolIndex as u8);

        // add objects to freelist
        // pool.freelist.reserve(meta.nfree as usize);
        // println!("object size: {}, computed size: {}, # free objects: {}", meta.osize, size, meta.nfree);
        for i in 0..(meta.nfree as usize) {
            let v: &mut JlTaggedValue = unsafe {
                mem::transmute(&mut page.data[i * (size + padding) + GC_PAGE_OFFSET])
            };
            v.set_tag(0);
            pool.freelist.push(v);
        }
    }

    pub fn find_pool(&self, size: &usize) -> Option<usize> {
        if *size > GC_MAX_SZCLASS {
            return None;
        }
        GC_SIZE_CLASSES.binary_search(size)
            .map(|i| {
                Some(i)
            })
            .unwrap_or_else(|i| {
                if i >= GC_SIZE_CLASSES.len() {
                    None
                } else {
                    Some(i)
                }
            })
    }

    /// Allocate a big object with given size, including object tag (i.e. tagged type pointer).
    ///
    /// Note: Size includes the tag a nd the tag is not cleared!
    pub fn big_alloc(&mut self, size: usize) -> &mut JlValue {
        self.maybe_collect();
        let rawsz = mem::size_of::<BigVal>().checked_add(size)
            .expect(& format!("Cannot allocate a BigVal with size {} on this architecture", size));
        // align size to cache byte alignment
        let allocsz = llt_align(rawsz, JL_CACHE_BYTE_ALIGNMENT);

        if unsafe { intrinsics::unlikely(rawsz < size) } {
            panic!(format!("BigVal with size {} is too big to align to cache and use on this architecture", size));
        }

        let (bv, tv) = unsafe {
            let ptr = self.rust_alloc::<BigVal>(allocsz);
            (*ptr).tid = self.tid;
            (*ptr).in_list = true;
            (*ptr).slot = self.heap.big_objects.len();
            (*ptr).sz_or_age = size;
            (*ptr).set_age(0);
            let taggedvalue: &mut JlTaggedValue = (*ptr).mut_taggedvalue();
            (&mut *ptr, taggedvalue)
        };

        // update stats
        unsafe {
            gc_num.allocd.fetch_add(allocsz as i64, Ordering::SeqCst);
            gc_num.bigalloc += 1;
        }

        if cfg!(feature="memdebug") {
            // TODO: fill bigval with 0xEE
        }

        self.heap.big_objects.push(bv);
        jl_value_of_mut(tv)
    }

    pub unsafe fn rust_alloc<T>(&mut self, size: usize) -> * mut T {
        // we don't deal with ZSTs but just fail
        debug_assert_ne!(size, 0);
        let ptr = alloc::heap::allocate(size, 8);
        if ptr.is_null() {
            panic!("GC error: out of memory (OOM)!");
        }
        mem::transmute(ptr)
    }

    // free an unmanaged pointer
    pub unsafe fn rust_free<T>(ptr: * mut T, size: usize) {
        alloc::heap::deallocate(mem::transmute::<* mut T, * mut u8>(ptr), size, 8);
    }

    // keep track of array with malloc'd storage
    pub fn track_malloced_array(&mut self, a: * mut JlArray) {
        // N.B. This is *NOT* a GC safepoint due to heap mutation!!!
        debug_assert_eq!(unsafe { (*a).flags.how() }, AllocStyle::MallocBuffer);
        self.heap.mallocarrays.push(MallocArray::new(a));
    }

    #[inline(always)]
    pub fn maybe_collect(&mut self) -> bool {
        if unsafe { intrinsics::unlikely(unsafe { *gc_num.allocd.get_mut() } > 0) || debug_check_pool() } {
            if ! (cfg!(feature="run_only_once") && GC_ALREADY_RUN.load(Ordering::SeqCst)) {
                // println!("triggering periodic collection");
                unsafe {
                    jl_gc_collect(0);
                }
                return true;
            }
        }
        unsafe {
            np_jl_gc_safepoint_(self.tls);
        }
        false
    }

    pub fn collect(&mut self, full: bool) -> bool {
        let t0 = neptune_hrtime();
        let last_perm_scanned_bytes = unsafe { perm_scanned_bytes } as i64;

        Gc2::verify_remsets();

        assert!(unsafe { mark_caches.as_ref().unwrap().len() } <= unsafe { np_threads.as_ref().unwrap().thread_count() as usize });
        if cfg!(feature = "run_only_once") {
            if GC_ALREADY_RUN.swap(true, Ordering::SeqCst) {
                return false;
            }
        }

        // 1. fix GC bits of objects in the memset (a.k.a. premark)
        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * t.tl_gcs };
            tl_gc.premark();
        }

        // walk the roots
        self.marking.walk_roots();

        unsafe {
            // this is deliberately not thread-safe
            gc_num.since_sweep += (*gc_num.allocd.get_mut() + gc_num.interval as i64) as u64;
        }

        neptune_gc_settime_premark_end();
        neptune_gc_time_mark_pause(t0, unsafe { scanned_bytes }, unsafe { perm_scanned_bytes }); // TODO

        let actual_allocd = unsafe { gc_num.since_sweep } as i64;
        // walking roots is over, time for finalizers

        // check for objects to finalize
        let mut orig_marked_len = unsafe {
            finalizer_list_marked.len
        };

        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * t.tl_gcs };
            Gc2::sweep_finalizer_list(&mut t.finalizers); // these are confusingly called `sweep_finalizer_list`
        }

        if unsafe { prev_sweep_full } != 0 {
            unsafe {
                Gc2::sweep_finalizer_list(&mut finalizer_list_marked);
            }
            orig_marked_len = 0;
        }

        // mark finalizers
        self.marking.mark_finalizers(orig_marked_len);

        neptune_gc_settime_postmark_end();

        Gc2::verify_to_finalize();

        self.sync_caches();

        assert_eq!(unsafe { mark_caches.as_ref().unwrap().len() }, unsafe { np_threads.as_ref().unwrap().thread_count() as usize });

        let live_sz_ub: i64 = unsafe {
            live_bytes + actual_allocd
        };
        let live_sz_est: i64 = unsafe {
            (scanned_bytes + perm_scanned_bytes) as i64
        };
        let estimate_freed: i64 = live_sz_ub - live_sz_est;

        self.verify();

        // TODO: call gc_stats.*

        // make a collection/sweep decision based on statistics

        unsafe {
            gc_num.total_allocd += gc_num.since_sweep;
        }

        // we want to free ~70% if possible.
        let not_freed_enough = estimate_freed < 7 * (actual_allocd/10);
        let mut nptr = 0;
        nptr += unsafe {
            get_all_tls().iter().fold(0, |acc, &ref t| { acc + (&*t.tl_gcs).heap.remset_nptr })
        };

        // if there are many intergenerational pointers then quick (not full, only young gen) sweep is not so quick
        let large_frontier = nptr * mem::size_of::<* mut libc::c_void>() >= DEFAULT_COLLECT_INTERVAL as usize;
        let mut sweep_full = false;
        let mut recollect = false;

        unsafe {
            if (full || large_frontier ||
                ((not_freed_enough || promoted_bytes >= gc_num.interval as i64) &&
                 (promoted_bytes >= DEFAULT_COLLECT_INTERVAL as i64 || prev_sweep_full != 0)) ||
                gc_check_heap_size(live_sz_ub, live_sz_est) != 0) &&
                gc_num.pause > 1
            {
                gc_update_heap_size(live_sz_ub, live_sz_est);

                recollect = full;

                if large_frontier {
                    gc_num.interval = last_long_collect_interval;
                }

                if not_freed_enough || large_frontier {
                    if gc_num.interval < DEFAULT_COLLECT_INTERVAL as usize {
                        gc_num.interval = DEFAULT_COLLECT_INTERVAL as usize;
                    } else if gc_num.interval <= 2 * (MAX_COLLECT_INTERVAL / 5) {
                        gc_num.interval = 5 * (gc_num.interval / 2);
                    }
                }

                last_long_collect_interval = gc_num.interval;
                sweep_full = true;
            } else {
                gc_num.interval = DEFAULT_COLLECT_INTERVAL as usize / 2;
                // sweep_full = gc_sweep_always_full;
            }
        }
        if sweep_full {
            unsafe {
                perm_scanned_bytes = 0;
            }
        }

        unsafe {
            scanned_bytes = 0;
        }

        // println!("collection decisions: sweep_full = {}, recollect = {}", sweep_full, recollect);

        // sweep
        self.sweep(sweep_full);

        // writeback stats
        self.writeback_stats(t0, sweep_full, recollect, actual_allocd, estimate_freed);

        recollect
    }

    fn sync_cache(&mut self, cache: &mut MarkCache) {
        unsafe {
            cache.sync_cache_nolock(&mut self.heap.big_objects, self.tid);
        }
        cache.sync_big_objects(self);
        cache.sync_remset(self);
    }

    #[inline(never)]
    fn sync_caches(&mut self) {
        // flush everything in mark caches
        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * t.tl_gcs };
            self.sync_cache(&mut tl_gc.cache);
        }

        for cache in unsafe { mark_caches.as_mut().unwrap().values_mut() } {
            self.sync_cache(cache);
        }
    }

    fn unlink_big_object(b: &mut BigVal) {
        if ! b.in_list {
            return;
        }
        if b.tid < 0 {
            // this part may cause deadlocks if this is called while holding lock of big_objects_marked
            unsafe {
                let mut bo: MutexGuard<Vec<* mut BigVal>> = big_objects_marked.as_mut().unwrap().lock().unwrap();
                let b2 = bo.swap_remove(b.slot as usize);
                assert_eq!(b as * mut BigVal, b2);
                b.in_list = false;
                b.slot = 0;
            }
        } else {
            // This part may not be thread-safe. We may need a lock
            // for this one. However, this should be fine since this
            // method is only called when:
            // 1. Either GC is not running, during jl_gc_realloc_string
            // 2. Inside sync_cache_nolock, callers of which need to guarantee thread-safety anyways
            let gc = unsafe {
                &mut *get_all_tls()[b.tid as usize].tl_gcs
            };
            let b2 = gc.heap.big_objects.swap_remove(b.slot as usize);
            assert_eq!(b as * mut BigVal, b2 as * mut BigVal);
            b.slot = 0;
            b.in_list = false;
        };
    }

    fn verify(&mut self) {
        // TODO: implement
    }

    #[inline(always)]
    fn writeback_stats(&mut self,
                       t0: u64,
                       full: bool,
                       recollect: bool,
                       actual_allocd: i64,
                       estimate_freed: i64) {
        let gc_end_t = neptune_hrtime();
        let pause = gc_end_t - t0;
        unsafe {
            gc_final_pause_end(t0, gc_end_t);
        }
        Gc2::time_sweep_pause(gc_end_t, actual_allocd, estimate_freed, full);
        unsafe {
            gc_num.full_sweep += full as libc::c_int;
            prev_sweep_full = full as libc::c_int;
            *gc_num.allocd.get_mut() = - (gc_num.interval as i64);
            live_bytes += gc_num.since_sweep as i64 - gc_num.freed;
            gc_num.pause += (! recollect) as libc::c_int;
            gc_num.total_time += pause;
            gc_num.since_sweep = 0;
            gc_num.freed = 0;
        }
    }

    #[cfg(feature = "gc_time")]
    #[inline(always)]
    fn time_sweep_pause(gc_end_t: u64, actual_allocd: i64, estimate_freed: i64, sweep_full: bool) {
        unsafe {
            gc_time_sweep_pause(gc_end_t, actual_allocd, live_bytes, estimate_freed, sweep_full as libc::c_int);
        }
    }

    #[cfg(not(feature = "gc_time"))]
    #[inline(always)]
    fn time_sweep_pause(gc_end_t: u64, actual_allocd: i64, estimate_freed: i64, sweep_full: bool) {
    }


    fn premark(&mut self) {
        for item in self.heap.remset.iter() {
          // TODO import and call objprofile_count(..)
            unsafe {
                (*as_mut_jltaggedvalue(*item)).set_tag(GC_OLD_MARKED);
            }
        }

        for item in self.heap.rem_bindings.iter_mut() {
            unsafe {
                (*as_mut_jltaggedvalue((*item).as_mut_jlvalue())).set_tag(GC_OLD_MARKED);
            }
        }

        mem::swap(&mut self.heap.remset, &mut self.heap.last_remset);
        self.heap.remset.clear();
        self.heap.remset_nptr = 0;
    }

    /// Mark given object concurrent to program execution. This is confusingly called `jl_gc_setmark` in Julia.
    /// This function _should not_ be called from inside GC.
    pub fn mark_concurrently(&mut self, v: * mut JlValue) {
        let o = unsafe {
            &mut *as_mut_jltaggedvalue(v)
        };
        let tag = unsafe { o.yolo_header() };

        if ! tag.marked() {
            let mut bits: u8 = 0;
            unsafe {
                if intrinsics::likely(Marking::setmark_tag(o, GC_MARKED, tag, &mut bits) && ! get_gc_verifying()) {
                    self.cache.setmark_pool(o, bits);
                }
            }
        }
    }

    #[inline(always)]
    fn should_timeout() -> bool {
        false
    }

    fn clear_freelists(&mut self) {
        for pool in self.heap.pools.iter_mut() {
            pool.clear_freelist();
        }
    }

    fn sweep_finalizer_list(finalizers: &mut JlArrayList) {
        let listptr = finalizers as * mut JlArrayList;
        let mut len = finalizers.len;
        let mut i = 0;
        // new scope to make borrow checker happy
        {
            let mut items = finalizers.as_slice_mut();
            while i < len {
                let v0 = items[i].clone();
                let is_cptr = (v0 as usize).marked(); // c-pointers' value pointers (not taggedvalue pointers) are marked
                let v = (v0 as usize).clear_tag(1) as * mut libc::c_void;
                let mut dontIncrement = false;

                if unsafe { intrinsics::unlikely(v0.is_null()) } {
                    // remove from this list
                    if i < len - 2 {
                        items[i] = items[len - 2];
                        items[i + 1] = items[len - 1];
                    } else {
                        i += 2;
                    }
                    len -= 2;
                    continue;
                }

                let fin = items[i+1].clone();
                let isfreed = ! unsafe { &* as_jltaggedvalue(v) }.marked();
                let isold = unsafe {
                    listptr != (&mut finalizer_list_marked) as * mut JlArrayList &&
                        unsafe { &* as_jltaggedvalue(v) }.tag() == GC_OLD_MARKED &&
                        (is_cptr || unsafe { &* as_jltaggedvalue(fin) }.tag() == GC_OLD_MARKED)
                };

                if isfreed || isold {
                    // remove from this list
                    if i < len - 2 {
                        items[i] = items[len - 2];
                        items[i + 1] = items[len - 1];
                        // we do this instead of decrementing 2 because
                        // Rust checks for underflow and although
                        // temporary underflow is ok in this case, there
                        // is no easy way to tell that to Rust.
                        dontIncrement = true; // instead of just having i -= 2 here
                    }
                    len -= 2;
                }

                if isfreed {
                    if is_cptr {
                        // schedule finalizer to execute right away if it is native (non-Julia) code
                        unsafe {
                            np_call_finalizer(fin, v);
                        }
                        if unsafe { intrinsics::likely(! dontIncrement) } {
                            i += 2;
                        }
                        continue;
                    }

                    // this is schedule_finalization()
                    unsafe {
                        to_finalize.push(v);
                        to_finalize.push(fin);
                    }
                }

                if isold {
                    // the caller relies on the new objects to be pushed to the end of the list
                    unsafe {
                        finalizer_list_marked.push(v0);
                        finalizer_list_marked.push(fin);
                    }
                }

                if unsafe { intrinsics::likely(! dontIncrement) } {
                    i += 2;
                }
            }
        }

        finalizers.len = len; // truncate the finalizer list
    }

    /// Verify that to_finalize doesn't contain any tagged pointers
    fn verify_to_finalize() {
        if cfg!(debug_assertions) {
            let items = unsafe {
                to_finalize.as_slice()
            };

            for i in (0..items.len()).step_by(2) {
                if items[i].clone() as usize & 1 != 0 {
                    panic!(format!("to_finalize has tagged pointer at index {}!", i));
                }
            }
        }
    }

    // sweep the object pool memory page by page.
    //
    // N.B. in this code, a "chunk" refers to 32 contiguous pages that
    // correspond to an element of allocmap.
    fn sweep_pools(&mut self, full: bool) {
        self.clear_freelists();
        // TODO: get this from page manager
        let regions = unsafe { REGIONS.as_mut().unwrap() };
        let remaining_pages = Arc::new(AtomicUsize::new(pg_mgr().current_pg_count.load(Ordering::SeqCst))); // Arc+AtomicUsize in preparation for sharing among threads
        for ri in 0..regions.len() {

            let ref mut region = regions[ri];
            if remaining_pages.load(Ordering::SeqCst) == 0 {
                break;
            }
            // if #pages in region is not a multiple of 32, then we need to check one more
            // entry in allocmap
            let check_incomplete_chunk = (region.pg_cnt % 32 != 0) as usize;

            if PARALLEL_SWEEP {
                /*let mut pool = unsafe { np_threads.as_mut().unwrap() };
                pool.scoped(|scope| {
                    for i in 0..(region.pg_cnt as usize / 32 + check_incomplete_chunk) {
                        let rp = remaining_pages.clone();
                        let regions = unsafe { REGIONS.as_mut().unwrap() };
                        scope.execute(move || {
                            //println!("Thread executing sweep_pool_chunk()");
                            Gc2::sweep_pool_chunk(&mut regions[ri], i, &rp, full)
                        });
                    }
                });*/
            } else {
                for i in 0..(region.pg_cnt as usize / 32 + check_incomplete_chunk) {
                    Gc2::sweep_pool_chunk(region, i, &remaining_pages, full);
                }
            }

        }
    }

    fn sweep_pool_chunk(region: &mut Region, i: usize, remaining_pages: &Arc<AtomicUsize>, full: bool) {
        let mut bytes_freed = 0;
        let mut m = region.allocmap[i];
        let mut j = 0;
        while m != 0 {
            let pg_idx = 32 * i + j;
            // if current page is not allocated, skip
            if m & 1 == 0 {
                m >>= 1;
                j += 1;
                continue;
            }
            // whether current page should be freed completely
            let mut should_free = false;
            // if current page is to be swept
            // a page is to be swept if it contains young objects or we are
            // doing a full sweep
            if full || *region.meta[pg_idx].has_young.get_mut() {
                let meta = &mut region.meta[pg_idx];
                let size = mem::size_of::<JlTaggedValue>() + meta.osize as usize;
                let aligned_pg_size = PAGE_SZ - GC_PAGE_OFFSET;
                let padding = (size - JL_SMALL_BYTE_ALIGNMENT) % JL_SMALL_BYTE_ALIGNMENT;
                let n_obj = aligned_pg_size / (size + padding) as usize;
                let page = &mut region.pages[pg_idx];
                let mut nfree = 0;
                let mut has_young = false;

                for o_idx in 0..n_obj {
                    let o = unsafe {
                        mem::transmute::<&mut u8, &mut JlTaggedValue>(&mut page.data[o_idx * (size + padding) + GC_PAGE_OFFSET])
                    };
                    if ! o.marked() {
                        nfree += 1;
                    }
                }

                bytes_freed += (nfree - meta.nfree as usize) * meta.osize as usize;

                // reset #free objects
                meta.nfree = nfree as u16;
                *meta.nold.get_mut() = 0; // ???

                if nfree != n_obj {
                    // there are live objects in the page, return free objects to the corresponding free list
                    let tl_gc: &mut Gc2 = unsafe {
                        &mut *(get_all_tls()[meta.thread_n as usize].tl_gcs)
                    };
                    let freelist = &mut tl_gc.heap.pools[meta.pool_n as usize].freelist;
                    for o_idx in 0..n_obj {
                        let o = unsafe {
                            mem::transmute::<&mut u8, &mut JlTaggedValue>(&mut page.data[o_idx * (size + padding) + GC_PAGE_OFFSET])
                        };

                        let mut bits = o.tag();
                        if bits.marked() {
                            if *meta.ages.as_mut().unwrap()[o_idx].get_mut() || bits == GC_OLD_MARKED {
                                // object is old enough
                                if full || bits == GC_MARKED {
                                    bits = GC_OLD; // promote
                                }
                                meta.prev_nold += 1;
                            } else {
                                assert_eq!(bits, GC_MARKED, "meta.ages doesn't match the object's age");
                                bits = GC_CLEAN;
                                has_young = true;
                            }
                            // increment age, saturating
                            *meta.ages.as_mut().unwrap()[o_idx].get_mut() = true;

                            o.set_tag(bits);
                        } else {
                            freelist.push(o);
                        }
                    }
                    meta.has_marked.store(true, Ordering::Relaxed);
                } else {
                    // page doesn't have anything alive in it, mark it for freeing
                    // TODO: do lazy sweeping with resets etc.
                    should_free = true;
                    meta.has_marked.store(false, Ordering::Relaxed);
                }


                *meta.has_young.get_mut() = has_young;
            }

            // we free the page here to make borrow checker happy
            if should_free {
                // page is unused, free it. we are being a little bit more aggressive here
                // we need to tell Rust that moving regions here is safe somehow.
                pg_mgr().free_page_in_region(region, pg_idx);
            }

            if remaining_pages.fetch_sub(1, Ordering::SeqCst) - 1 == 0 {
                break;
            }
            m >>= 1;
            j += 1;
        }

        unsafe {
            // TODO: make this update atomic for thread-safety
            gc_num.freed += bytes_freed as i64;
        }
    }

    // sweep bigvals in all threads
    fn sweep_bigvals(&mut self, full: bool) {
        neptune_gc_time_big_start();
        for ptls in unsafe { get_all_tls() } {
            // get thread-local Gc
            let tl_gc = unsafe {
                &mut * (*ptls).tl_gcs
            };
            tl_gc.sweep_local_bigvals(full);
        }

        if full {
            // sweep old bigvals
            let mut bo: MutexGuard<Vec<* mut BigVal>> = unsafe {
                big_objects_marked.as_mut().unwrap().lock().unwrap()
            };
            let big_objects = unsafe {
                // make pointers managed, this trick is required to match type of self.heap.big_objects
                mem::transmute::<&mut Vec<* mut BigVal>, &mut Vec<& mut BigVal>>(&mut *bo)
            };

            Gc2::sweep_big_list(&mut *big_objects, full);

            // move all survivors from big_objects_marked to this thread's big_objects
            self.heap.big_objects.append(&mut *big_objects);
        }
        neptune_gc_time_big_end();
    }

    // sweep bigvals local to this thread
    fn sweep_local_bigvals(&mut self, full: bool) {
        Gc2::sweep_big_list(&mut self.heap.big_objects, full)
    }

    fn sweep_big_list(list: &mut Vec<& mut BigVal>, full: bool) {
        let mut nbig_obj = list.len();
        let mut i = 0;

        while i < nbig_obj {
            let mut bits = list[i].taggedvalue().tag();
            let old_bits: u8 = bits;

            if unsafe { bits.marked() } {
                if list[i].age() >= PROMOTE_AGE || bits == GC_OLD_MARKED {
                    if full || bits == GC_MARKED {
                        bits = GC_OLD;
                    }
                } else {
                    list[i].inc_age();
                    bits = GC_CLEAN;
                }
                list[i].mut_taggedvalue().set_tag(bits);
                i += 1;
            } else {
                let b = list.swap_remove(i);
                nbig_obj -= 1;

                let begin = b.taggedvalue().get_value() as * const JlValue as usize;

                unsafe {
                    gc_num.freed += b.allocd_size() as i64;
                }

                unsafe {
                    Gc2::rust_free(b as * mut BigVal, b.allocd_size());
                }
            }

            neptune_gc_time_count_big(old_bits as libc::c_int, bits as libc::c_int);
        }
    }

    fn sweep_weakrefs(&mut self) {
        let mut i = 0;
        while i < self.heap.weak_refs.len() {
            if unsafe { (* as_jltaggedvalue((&*self.heap.weak_refs[i]).as_jlvalue())).marked() } {
                let wr = unsafe { &mut *self.heap.weak_refs[i] };
                // weakref is alive
                if ! unsafe { (* as_jltaggedvalue(wr.value)).marked() } {
                    // however, referenced value is dead, so invalidate weakref
                    wr.value = jl_nothing;
                }
                i += 1;
            } else {
                // drop weakref
                self.heap.weak_refs.swap_remove(i);
            }
        }
    }

    #[inline(always)]
    fn sweep_remset(&mut self, full: bool) {
        if full {
            // this is a full sweep, clear remsets
            self.heap.remset.truncate(0);
            self.heap.rem_bindings.truncate(0);
        } else {
            // this is a quicksweep, mark objects in remset so that they will
            // not trigger the write barrier till next full sweep
            for v in self.heap.remset.iter_mut() {
                unsafe {
                    (*as_mut_jltaggedvalue(*v)).set_tag(GC_MARKED);
                }
            }

            for v in self.heap.rem_bindings.iter_mut() {
                unsafe {
                    (*as_mut_jltaggedvalue(v.as_mut_jlvalue())).set_tag(GC_MARKED);
                }
            }
        }
    }

    fn sweep_malloced_arrays(&mut self) {
        neptune_gc_time_mallocd_array_start();
        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * (*t).tl_gcs };
            tl_gc.sweep_local_malloced_arrays();
        }
        neptune_gc_time_mallocd_array_end();
    }

    fn sweep_local_malloced_arrays(&mut self) {
        let ref mut ma = self.heap.mallocarrays;

        let mut end = ma.len();
        let mut i = 0;
        while i < end {
            let tag = unsafe {
                &*as_jltaggedvalue((&*ma[i].a).as_jlvalue())
            };

            if tag.marked() {
                i += 1;
            } else {
                let a = unsafe {
                    &mut *ma.swap_remove(i).a
                };
                debug_assert_eq!(a.flags.how(), AllocStyle::MallocBuffer);
                Gc2::free_array(a);
                end -= 1;
            }

            neptune_gc_time_count_mallocd_array(tag.tag() as libc::c_int)
        }
    }

    fn free_array(a: &mut JlArray) {
        if a.flags.how() == AllocStyle::MallocBuffer {
            if PURGE_FREED_MEMORY {
                unsafe {
                    libc::memset(a.data, 0, a.length * a.elsize as usize);
                }
            }

            let d = unsafe {
                (a.data as * mut u8).offset(- (a.offset as isize * a.elsize as isize)) as * mut libc::c_void
            };

            // if a.flags().isaligned() {
            //     free_aligned(d);
            // } else {
            //     unsafe {
            //         libc::free(d);
            //     }
            // }
            unsafe {
                gc_num.freed += a.nbytes() as i64;
                libc::free(d); // on POSIX both cases compile down to free(3)
            }
        }
    }

    #[cfg(not(debug_assertions))]
    fn verify_module(&mut self, m: & mut JlModule) { }

    #[cfg(debug_assertions)]
    fn verify_module(&mut self, m: & mut JlModule) {
        let mut table = unsafe {
            slice::from_raw_parts_mut(m.bindings.table, m.bindings.size)
        };

        let mut i = 1;
        let len = table.len();

        // verify bindings
        while i < len {
            let entry = table[i];
            if ! HTable::is_not_found(entry) {
                let b = unsafe {
                    JlBinding::from_jlvalue_mut(&mut *table[i])
                };

                let bname = unsafe { (*b.name).sname().unwrap() };

                let vb = unsafe { &*as_mut_jltaggedvalue(b.as_mut_jlvalue()) };

                assert!(vb.marked(), format!("binding #{} is not marked!", bname));

                if ! b.value.is_null() {
                    let t = unsafe { &*as_jltaggedvalue((*b.value).as_jlvalue()) };

                    assert!(t.marked(), format!("value of binding #{} is not marked!", bname));
                }

                // println!("Binding {} is marked", bname);
            }

            i += 2;
        }

        for using in m.usings.as_slice() {
            assert!(unsafe { (&*as_jltaggedvalue(*using)).marked() }, "using is not marked!");
        }

        if ! m.parent.is_null() {
            assert!(unsafe { (&*as_jltaggedvalue((&*m.parent).as_jlvalue())).marked() }, "parent module is not marked!");
        }
    }

    fn scrub(&self) {
    }

    fn verify_tags(&mut self) {
        if cfg!(feature = "memfence") {
            // verify the freelist chains look valid

            for t in unsafe { get_all_tls() } {
                let gc = unsafe { &mut *t.tl_gcs };

                for p in gc.heap.pools.iter_mut() {
                    // for all fools, iterate its freelist
                    let mut last_page = Page::of_raw::<u8>(::std::ptr::null());
                    // TODO: have `allocated` and check it too, gc-debug.c:262

                    // TODO: fix for SegQueue
                    /*
                    for o in p.freelist.iter_mut() {
                        // and assert that freelist values aren't gc-marked
                        debug_assert!(! o.marked(), "There are marked objects in the freelists.");

                        // TODO: verify that freelist pages are ordered

                        let cur_page = Page::of::<JlTaggedValue>(o);

                        if last_page != cur_page {
                            // verify that the page metadata is correct
                            let meta = unsafe {
                                pg_mgr().find_pagemeta::<JlTaggedValue>(*o).expect("Pooled object doesn't belong to any memory region!")
                            };

                            debug_assert_eq!(p.osize, meta.osize as usize, "Pool and pagemeta object sizes don't match!");

                            last_page = cur_page;
                        }
                    }
                     */
                }
            }
        }
    }

    fn print_big_object(b: &BigVal) {
        let pb = b as * const BigVal as usize;
        let t = b.taggedvalue().tag();
        print!(" 0x{:x} [{}]", pb, t);
    }

    fn print_big_object_lists(full: bool) {
        println!("-------------------- after {} collection", if full { "full" } else { "quick" });
        for t in unsafe { get_all_tls() } {
            let gc = unsafe { &mut * t.tl_gcs };

            print!("big objects in t{}'s list:", t.tid);

            for b in gc.heap.big_objects.iter() {
                assert!(b.tid == gc.tid);
                // Gc2::print_big_object(b);
            }
            println!();

            print!("big objects in t{}'s cache:", t.tid);

            for i in 0..gc.cache.nbig_obj {
                Gc2::print_big_object(unsafe { &*gc.cache.big_obj[i] });
            }
            println!();

            print!("big objects in t{}'s cache's biglist:", t.tid);

            for i in 0..gc.cache.big_obj_list.len() {
                Gc2::print_big_object(unsafe { &*gc.cache.big_obj_list[i] });
            }
            println!();
        }

        for (t, c) in unsafe { mark_caches.as_ref().unwrap() } {

            print!("big objects in GC thread {:?}'s cache:", t);

            for i in 0..c.nbig_obj {
                Gc2::print_big_object(unsafe { &*c.big_obj[i] });
            }
            println!();

            print!("big objects in GC thread {:?}'s cache's biglist:", t);

            for i in 0..c.big_obj_list.len() {
                Gc2::print_big_object(unsafe { &*c.big_obj_list[i] });
            }
            println!();
        }

        print!("big_objects_marked: ");

        let bo = unsafe { big_objects_marked.as_mut().unwrap().lock().unwrap() };
        for b in (*bo).iter() {
            Gc2::print_big_object(unsafe { &**b });
        }
        println!();
        println!("--------------------");
    }

    fn print_object(b: & JlValue) {
        let pb = b as * const JlValue as usize;
        let t = unsafe { &*as_jltaggedvalue(b) }.tag();
        print!(" 0x{:x} [{}]", pb, t);
    }

    fn verify_remsets() {
        for t in unsafe { get_all_tls() } {
            let gc = unsafe { &mut * t.tl_gcs };
            assert!(gc.cache.remset.is_empty());
            /*
            print!("big objects in t{}'s cache's remset:", t.tid);

            for i in 0..gc.cache.remset.len() {
                Gc2::print_object(unsafe { &*gc.cache.remset[i] });
            }
            println!();*/
        }

        for (t, c) in unsafe { mark_caches.as_ref().unwrap() } {
            assert!(c.remset.is_empty());
            /*
            print!("big objects in GC thread {:?}'s cache's remset:", t);

            for i in 0..c.remset.len() {
                Gc2::print_object(unsafe { &*c.remset[i] });
            }
            println!();
             */
        }
    }

    fn sweep(&mut self, full: bool) {
        self.verify_module(unsafe { &mut *jl_core_module }); self.verify_module(unsafe { &mut *jl_main_module });

        // println!("sweeping weak refs");
        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * (*t).tl_gcs };
            tl_gc.sweep_weakrefs();
        }

        // println!("sweeping malloc'd arrays");
        self.sweep_malloced_arrays();

        // println!("sweeping bigvals");
        self.sweep_bigvals(full);

        // Gc2::print_big_object_lists(full);

        // println!("scrubbing");
        self.scrub();

        // println!("verifying tags");
        self.verify_tags();

        // println!("sweeping pools");
        self.sweep_pools(full);

        // Gc2::verify_remsets();
        // println!("sweeping remsets");
        for t in unsafe { get_all_tls() } {
            let tl_gc = unsafe { &mut * (*t).tl_gcs };
            tl_gc.sweep_remset(full);
        }
        // Gc2::verify_remsets();

        // println!("done sweeping")
    }

    // Functions for write barrier
    #[inline(always)]
    pub fn queue_root(&mut self, root: &mut JlValue) {
        let tag = as_managed_jltaggedvalue(root);
        debug_assert!(tag.tag() == GC_OLD_MARKED);

        // N.B. The modification of the tag is not atomic!
        // It should be ok since this is not a GC safepoint.
        tag.header.get_mut().set_tag(GC_MARKED);
        self.heap.remset.push(tag.mut_value()); // we use get_value instead of directly root to make borrow checker happy
        self.heap.remset_nptr += 1; // conservative, in case of root being a pointer
    }

    #[inline(always)]
    pub fn queue_binding(&mut self, binding: &'a mut JlBinding<'a>) {
        let tag = unsafe {
            &mut *as_mut_jltaggedvalue(binding.as_mut_jlvalue())
        };
        debug_assert!(tag.tag() == GC_OLD_MARKED);

        // N.B. The modification of the tag is not atomic!
        // It should be ok since this is not a GC safepoint.
        tag.header.get_mut().set_tag(GC_MARKED);

        self.heap.rem_bindings.push(binding);
    }

    #[inline(always)]
    pub fn push_weakref(&mut self, wr: &mut WeakRef) {
        self.heap.weak_refs.push(wr);
    }
}
