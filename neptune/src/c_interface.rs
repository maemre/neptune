// C interface for the garbage collector, C side needs to call
// appropriate functions with a Gc object since Using a static mutable
// object is unsafe in Rust because of "life after main" and
// destructor order.

use gc::*;
use gc2::*;
use libc::c_int;
use libc::c_void;
use libc::c_uint;
use libc::c_char;
use libc::uintptr_t;
use libc;
use pages::*;
use core::slice;
use std::mem;
use core::ops::Deref;
use core::ops::DerefMut;
use core;
use std::sync::atomic::*;
use std::ffi::CString;
use std::ffi::CStr;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use util::*;
use concurrency::*;
use std::sync::*;
use std::env;
use scoped_threadpool::Pool;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct jmp_buf {
    _data: [u64; 25]
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigjmp_buf {
    _data: [u64; 25]
}

pub type JlJmpBuf = sigjmp_buf;

pub type JlValue = libc::c_void;
pub type JlFunction = JlValue;

// temporary, TODO: reify
pub type JlHandler = libc::c_void;
pub type JlTypeMapEntry = libc::c_void;

#[repr(C)]
pub struct JlSym {
    left: * mut JlSym,
    right: * mut JlSym,
    hash: usize,
}

impl JlSym {
    pub fn name(&self) -> &CStr {
        unsafe {
            CStr::from_ptr((self as * const JlSym).offset(1) as * const c_char)
        }
    }

    pub fn sname(&self) -> Option<&str> {
        self.name().to_str().ok()
    }
}

impl JlValueMarker for JlSym {
}

#[repr(C)]
pub struct JlModule {
    pub name: * mut JlSym,
    pub parent: * mut JlModule,
    pub bindings: HTable,
    pub usings: JlArrayList, // modules with all bindings potentially imported
    pub istopmod: u8,
    pub uuid: u64,
    pub counter: u32,
}

#[repr(C)]
pub struct JlTask {
    pub parent: * mut JlTask,
    pub tls: * mut JlValue,
    pub state: * mut JlSym,
    pub consumers: * mut JlValue,
    pub donenotify: * mut JlValue,
    pub result: * mut JlValue,
    pub exception: * mut JlValue,
    pub backtrace: * mut JlValue,
    pub start: * mut JlFunction,
    pub ctx: JlJmpBuf,
    pub bufsz: usize,
    pub stkbuf: * mut c_void,

    // hidden fields:
    pub ssize: usize,
    started: usize, // this is actually a bool

    // current exception handler
    pub eh: JlHandler,
    // saved gc stack top for context switches
    pub gcstack: * mut GcFrame,
    // current module, or NULL if this task has not set one
    pub current_module: * mut JlModule,
    // current world age
    pub world_age: usize,

    // id of owning thread
    // does not need to be defined until the task runs
    pub tid: i16,
    // This is statically initialized when the task is not holding any locks
    pub locks: JlArrayList,
    pub timing_stack: * mut JlTimingBlock,
}

#[repr(C)]
pub struct JlTVar {
    name: * mut JlSym,
    lb: * mut JlValue,
    ub: * mut JlValue,
}

impl JlValueMarker for JlTVar {
}

#[repr(C)]
pub struct JlUnionAll {
    var: * mut JlTVar,
    body: * mut JlValue,
}

impl JlValueMarker for JlUnionAll {
}

#[repr(C)]
#[cfg(debug_assertions)]
pub struct JlTimingBlock { // typedef in julia.h
    prev: * mut JlTimingBlock,
    total: u64,
    t0: u64,
    owner: c_int,
    running: u8,
}

#[repr(C)]
#[cfg(not(debug_assertions))]
pub struct JlTimingBlock { // typedef in julia.h
    prev: * mut JlTimingBlock,
    total: u64,
    t0: u64,
    owner: c_int,
}

// Representations of internal hashtables used by Julia
pub const HT_N_INLINE: usize = 32;

#[repr(C)]
pub struct HTable {
    pub size: usize,
    pub table: * mut * mut c_void,
    pub _space: [* mut c_void; HT_N_INLINE],
}

impl HTable {
    #[inline(always)]
    pub fn is_not_found(entry: * mut c_void) -> bool {
        entry as usize == 1
    }
}

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

    fn from_jlvalue(v: &JlValue) -> &Self;

    fn from_jlvalue_mut(v: &mut JlValue) -> &mut Self;
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

    fn from_jlvalue(v: &JlValue) -> &Self {
        unsafe {
            mem::transmute(v)
        }
    }

    fn from_jlvalue_mut(v: &mut JlValue) -> &mut Self {
        unsafe {
            mem::transmute(v)
        }
    }
}

impl JlValueLike for JlValue {
    fn as_mut_jlvalue(&mut self) -> &mut JlValue {
        self
    }

    fn as_jlvalue(&self) -> &JlValue {
        self
    }

    fn from_jlvalue(v: &JlValue) -> &Self {
        v
    }

    fn from_jlvalue_mut(v: &mut JlValue) -> &mut Self {
        v
    }
}

impl JlValueMarker for JlModule {
}

impl JlValueMarker for JlTask {
}

pub fn as_jltaggedvalue(v: * const JlValue) -> * const JlTaggedValue {
    unsafe {
        mem::transmute::<* const JlValue, * const JlTaggedValue>(v).offset(-1)
    }
}

pub fn as_mut_jltaggedvalue(v: * mut JlValue) -> * mut JlTaggedValue {
    unsafe {
        mem::transmute::<* mut JlValue, * mut JlTaggedValue>(v).offset(-1)
    }
}

pub fn as_managed_jltaggedvalue(v: &mut JlValue) -> &mut JlTaggedValue {
    unsafe {
        &mut *mem::transmute::<* mut JlValue, * mut JlTaggedValue>(v).offset(-1)
    }
}

// Note: this is actually a union with the shape:
//
// ```
// union jl_typemap_t {
//     struct _jl_typemap_level_t *node;
//     struct _jl_typemap_entry_t *leaf;
//     struct _jl_value_t *unknown; // nothing
// };
// ```
//
// We can add accessors to other interpretations of this union later
// on if necessary.
#[repr(C)]
pub struct JlTypeMap {
    pub unknown: * mut JlValue,
}

#[repr(C)]
pub struct JlDatatypeLayout {
    pub nfields: u32,
    bits: u32, // these will correspond to the bitfields
}

impl JlDatatypeLayout {
    #[inline(always)]
    pub fn alignment(&self) -> u32 {
        self.bits.get_bits(0..9)
    }

    #[inline(always)]
    pub fn set_alignment(&mut self, alignment: u32) {
        self.bits.set_bits(0..9, alignment);
    }

    #[inline(always)]
    pub fn haspadding(&self) -> bool {
        self.bits.get_bit(9)
    }

    #[inline(always)]
    pub fn set_haspadding(&mut self, haspadding: bool) {
        self.bits.set_bit(9, haspadding);
    }

    #[inline(always)]
    pub fn npointers(&self) -> u32 {
        self.bits.get_bits(10..30)
    }

    #[inline(always)]
    pub fn set_npointers(&mut self, npointers: u32) {
        self.bits.set_bits(10..30, npointers);
    }

    #[inline(always)]
    pub fn fielddesc_type(&self) -> u32 {
        self.bits.get_bits(30..32)
    }

    #[inline(always)]
    pub fn set_fielddesc_type(&mut self, fielddesc_type: u32) {
        self.bits.set_bits(30..32, fielddesc_type);
    }
}

#[repr(C)]
pub struct JlSVec {
    //JL_DATA_TYPE
    pub length: usize,
}

// Might not be correct, might be needed, might be incomplete
#[repr(C)]
pub struct JlDatatype {
    //JL_DATA_TYPE
    pub name: *const JlTypename,
    pub super_t: *const JlDatatype,
    pub parameters: JlSVec,
    pub types: JlSVec,
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

impl JlValueMarker for JlDatatype {
}

pub struct JlTypename {
    //JL_DATA_TYPE
    pub name: *mut c_void, // jl_sym_t
    pub module: *mut c_void, // jl_module_t
    names: *mut JlSVec,  // jl_svec_t field names
    wrapper: *mut JlValue,
    cache: *mut JlSVec,        // sorted array
    linearcache: *mut JlSVec,  // unsorted array
    hash: i32, // inptr_t
    mt: *mut c_void, // struct _jl_methtable_t
}

#[derive(Clone)]
#[repr(C)]
pub struct JlArrayFlags {
    pub flags: u16 // how:2, ndims:10, pooled:1, ptarray:1, isshared:1, isaligned:1 TODO not sure about order
}

impl JlArrayFlags {
    pub fn how(&self) -> AllocStyle {
        // following cast works because AllocStyle is represented as a u16!
        unsafe {
            mem::transmute::<u16, AllocStyle>(self.flags.get_bits(0..2))
        }
    }
    pub fn ndims(&self) -> u16 {self.flags.get_bits(2..12)}
    pub fn pooled(&self) -> bool {self.flags.get_bit(12)}
    pub fn ptrarray(&self) -> bool {self.flags.get_bit(13)}
    pub fn ishared(&self) -> bool {self.flags.get_bit(14)}
    pub fn isaligned(&self) -> bool {self.flags.get_bit(15)}
}

#[derive(PartialEq, Debug)]
#[repr(u16)]
pub enum AllocStyle {
    Inlined = 0,
    JlBuffer = 1,
    MallocBuffer = 2,
    HasOwnerPointer = 3,
}

#[repr(C)]
pub struct JlArray {
    //JL_DATA_TYPE
    pub data: *mut c_void, // void *
    pub length: usize, // size_t
    pub flags: JlArrayFlags,
    pub elsize: u16,
    pub offset: u32,
    pub nrows: usize,
    pub maxsize_ncols: usize, // size_t
}

impl JlArray {
    // imitate ncols in union in C
    #[inline(always)]
    pub fn ncols(&self) -> usize {
        self.maxsize_ncols
    }

    #[inline(always)]
    pub fn set_ncols(&mut self, ncols: usize) {
        self.maxsize_ncols = ncols;
    }

    pub fn nbytes(&self) -> usize {
        if self.ndims() == 1 {
            self.elsize as usize * self.maxsize_ncols as usize + (self.elsize == 1) as usize
        } else {
            self.elsize as usize * self.length as usize
        }
    }

    #[inline(always)]
    pub fn ndims(&self) -> u16 {
        self.flags.ndims()
    }

    #[inline(always)]
    pub fn data_owner_offset(&self) -> isize {
        (mem::size_of::<JlArray>() + mem::size_of::<usize>() * (self.ndimwords())) as isize
    }

    #[inline(always)]
    pub fn data_owner_mut(&mut self) -> &mut JlValue {
        unsafe {
            *(mem::transmute::<* mut JlArray, * mut u8>(self as * mut JlArray).offset(self.data_owner_offset()) as * mut &mut JlValue)
        }
    }

    #[inline(always)]
    pub fn data_owner(&self) -> &JlValue {
        unsafe {
            *(mem::transmute::<* const JlArray, * const u8>(self as * const JlArray).offset(self.data_owner_offset()) as * const &JlValue)
        }
    }

    #[inline(always)]
    pub fn ndimwords(&self) -> usize {
        self.ndims().saturating_sub(2) as usize
    }
}

impl JlValueMarker for JlArray {
}

// this is actually just the tag
pub struct JlTaggedValue {
    pub header: AtomicUsize
}
// this is actually mem::size_of::<JlTaggedValue>(), we cannot make it a static const
// because `size_of` is not yet constant in Rust unfortunately.
// ACHTUNG: update this if JlTaggedValue is ever changed!
pub const SIZE_OF_JLTAGGEDVALUE: usize = 8;

pub const N_CALL_CACHE: usize = 4096; // from options.h

pub const PROMOTE_AGE: usize = 1;

pub const JL_CACHE_BYTE_ALIGNMENT: usize = 64;

extern {
    pub fn arraylist_push(a: * mut JlArrayList, e: * mut c_void);

    pub fn gc_final_count_page(pg_cnt: usize);
    pub fn gc_final_pause_end(t0: u64, tend: u64);
    pub fn gc_time_sweep_pause(gc_end_t: u64, actual_allocd: i64, live_bytes: i64, estimate_freed: i64, sweep_full: c_int);
    pub fn jl_gc_wait_for_the_world(); // wait for the world to stop

    // access to julia's timing functions for profiling
    pub fn jl_hrtime() -> u64;

    pub fn gc_settime_premark_end();
    pub fn gc_time_mark_pause(t0: u64, scanned_bytes: usize, perm_scanned_bytes: usize);
    pub fn gc_settime_postmark_end();

    pub fn gc_time_mallocd_array_start();
    pub fn gc_time_count_mallocd_array(bits: c_int);
    pub fn gc_time_mallocd_array_end();

    pub fn gc_time_big_start();
    pub fn gc_time_count_big(old_bits: c_int, bits: c_int);
    pub fn gc_time_big_end();

    // mark boxed caches, which don't contain any pointers hence are terminal nodes
    pub fn jl_mark_box_caches(ptls: &mut JlTLS);

    pub fn jl_gc_collect(full: c_int);

    // to print types for debugging
    pub fn jl_(v: * const JlValue);

    #[cfg(gc_debug_env)]
    pub fn gc_scrub_record_task(ta: * mut JlTask);

    #[cfg(gc_debug_env)]
    pub fn gc_debug_check_pool() -> c_int;

    pub fn gc_check_heap_size(sz_ub: i64, sz_est: i64) -> c_int;
    pub fn gc_update_heap_size(sz_ub: i64, sz_est: i64);

    // set type of a value by setting the tag
    pub fn np_jl_svec_data(v: * mut JlValue) -> * mut * mut JlValue;
    pub fn np_jl_field_isptr(st: * const JlDatatype, i: c_int) -> c_int;
    pub fn np_jl_field_offset(st: * const JlDatatype, i: c_int) -> u32;
    pub fn np_jl_symbol_name(sym: * const JlSym) -> * const c_char;
    pub fn np_jl_gc_safepoint_(ptls: * mut JlTLS);

    pub fn np_corruption_fail(vt: * mut JlDatatype) -> !;
    pub fn np_verify_parent(ty: * const c_char, o: * const JlValue, slot: * const * mut JlValue, msg: * const c_char);
    pub fn np_call_finalizer(fin: * mut c_void, p: * mut c_void);

    // list of global threads, declared in julia/src/threading.c
    pub static jl_n_threads: u32;
    pub static jl_all_tls_states: * mut &'static mut JlTLS;

    pub static jl_page_size: usize;

    // jl_nothing is a value inhabiting bottom, similar to NULL. It is used for
    // invalidating weak references so its type should match weak reference
    // types.
    pub static jl_nothing: * mut JlValue;
    /* From julia/src/jltypes.c */
    pub static jl_any_type: *const JlDatatype;
    pub static jl_type_type: *const c_void;
    pub static jl_symbol_type: *const JlDatatype;
    pub static jl_weakref_type: *const JlDatatype;
    pub static jl_simplevector_type: *const JlDatatype;
    pub static jl_array_typename: *const JlTypename;
    pub static jl_typename: *const JlTypename;
    pub static jl_module_type: * const JlDatatype;
    pub static jl_task_type: * const JlDatatype;
    pub static jl_string_type: * const JlDatatype;
    pub static jl_emptytuple_type: * mut JlDatatype;
    pub static jl_datatype_type: * mut JlDatatype;

    pub static jl_core_module: * mut JlModule;
    pub static jl_main_module: * mut JlModule;
    pub static jl_internal_main_module: * mut JlModule;

    pub static jl_typetype_type: * mut JlUnionAll;
    pub static jl_anytuple_type_type: * mut JlUnionAll;
    pub static jl_all_methods: * mut JlArray;
    pub static jl_module_init_order: * mut JlArray;

    pub static jl_cfunction_list: JlTypeMap;
    pub static jl_an_empty_vec_any: * mut JlValue;
    pub static jl_ANY_flag: * mut JlValue;

    #[cfg(gc_verify)]
    pub static gc_verifying: libc::c_int;

    pub static mark_reset_age: AtomicI32; // assuming c_int = i32

    pub static call_cache: [* mut JlTypeMapEntry; N_CALL_CACHE];

    pub static mut gc_num: GcNum;
    pub static mut live_bytes: i64;
    pub static mut promoted_bytes: i64;
    pub static mut prev_sweep_full: libc::c_int;
    pub static mut perm_scanned_bytes: usize; // static int found in gc.c
    pub static mut scanned_bytes: usize; // static int found in gc.c
    pub static mut last_long_collect_interval: usize;

    pub static mut finalizer_list_marked: JlArrayList;
    pub static mut to_finalize: JlArrayList;
}

#[inline(always)]
pub fn neptune_gc_settime_premark_end() {
    unsafe { gc_settime_premark_end() }
}

#[inline(always)]
#[cfg(feature = "gc_time")]
pub fn neptune_gc_time_mark_pause(t0: u64, sb: usize, psb: usize) {
    unsafe { gc_time_mark_pause(t0, sb, psb) }
}

#[inline(always)]
#[cfg(not(feature = "gc_time"))]
pub fn neptune_gc_time_mark_pause(t0: u64, sb: usize, psb: usize) {
}

#[inline(always)]
pub fn neptune_gc_settime_postmark_end() {
    unsafe { gc_settime_postmark_end() }
}

#[inline(always)]
#[cfg(feature = "gc_time")]
pub fn neptune_gc_time_mallocd_array_start() {
    unsafe { gc_time_mallocd_array_start() }
}

#[inline(always)]
#[cfg(not(feature = "gc_time"))]
pub fn neptune_gc_time_mallocd_array_start() {
}

#[inline(always)]
#[cfg(feature = "gc_time")]
pub fn neptune_gc_time_mallocd_array_end() {
    unsafe { gc_time_mallocd_array_end() }
}

#[inline(always)]
#[cfg(not(feature = "gc_time"))]
pub fn neptune_gc_time_mallocd_array_end() {
}

#[inline(always)]
#[cfg(feature = "gc_time")]
pub fn neptune_gc_time_big_start() {
    unsafe { gc_time_big_start() }
}

#[inline(always)]
#[cfg(not(feature = "gc_time"))]
pub fn neptune_gc_time_big_start() {
}

#[inline(always)]
#[cfg(feature = "gc_time")]
pub fn neptune_gc_time_big_end() {
    unsafe { gc_time_big_end() }
}

#[inline(always)]
#[cfg(not(feature = "gc_time"))]
pub fn neptune_gc_time_big_end() {
}

#[inline(always)]
#[cfg(feature = "gc_time")]
pub fn neptune_gc_time_count_big(old_bits: c_int, bits: c_int) {
    unsafe { gc_time_count_big(old_bits, bits) }
}

#[inline(always)]
#[cfg(not(feature = "gc_time"))]
pub fn neptune_gc_time_count_big(old_bits: c_int, bits: c_int) {
}

#[inline(always)]
#[cfg(feature = "gc_time")]
pub fn neptune_gc_time_count_mallocd_array(bits: c_int) {
    unsafe { gc_time_count_mallocd_array(bits) }
}

#[inline(always)]
#[cfg(not(feature = "gc_time"))]
pub fn neptune_gc_time_count_mallocd_array(bits: c_int) {
}

#[inline(always)]
#[cfg(debug_env)]
pub fn debug_check_pool() -> bool {
    gc_debug_check_pool() != 0
}

#[inline(always)]
#[cfg(not(debug_env))]
pub fn debug_check_pool() -> bool {
    false
}

#[inline(always)]
pub fn get_mark_reset_age() -> i32 {
    mark_reset_age.load(Ordering::Relaxed)
}

#[inline(always)]
pub fn set_mark_reset_age(n: i32) {
    mark_reset_age.store(n, Ordering::Relaxed);
}

#[inline(always)]
pub fn neptune_hrtime() -> u64 {
    unsafe { jl_hrtime() }
}

#[inline(always)]
#[cfg(not(gc_debug_env))]
pub fn gc_scrub_record_task(t: * mut JlTask) {
}

#[inline(always)]
pub unsafe fn verify_parent_<T: Into<Vec<u8>>>(ty: &str, o: * const JlValue, slot: &* mut JlValue, msg: T) {
        np_verify_parent(CString::new(ty).unwrap().into_raw(),
                         o,
                         slot as * const * mut JlValue,
                         CString::new(msg).unwrap().into_raw())
}

#[macro_export]
#[cfg(gc_verify)]
macro_rules! verify_parent {
    ($ty: expr, $o: expr, $slot: expr, $msg: expr) => (verify_parent_($ty, $o, $slot, $msg));
}

#[macro_export]
#[cfg(not(gc_verify))]
macro_rules! verify_parent {
    ($ty: expr, $o: expr, $slot: expr, $msg: expr) => ();
}

// Wrapper for getting all thread states in a safer manner by constructing a
// slice hence allowing for proper bounds checks
#[inline(always)]
pub unsafe fn get_all_tls<'a>() -> &'a mut [&'static mut JlTLS] {
    ::std::slice::from_raw_parts_mut(jl_all_tls_states, jl_n_threads as usize)
}

#[cfg(gc_verify)]
#[inline(always)]
pub fn get_gc_verifying() -> bool {
    gc_verifying != 0
}

#[cfg(not(gc_verify))]
#[inline(always)]
pub fn get_gc_verifying() -> bool {
    false
}

#[inline(always)]
pub fn llt_align(x: usize, sz: usize) -> usize {
    ((x as isize + sz as isize - 1) & - (sz as isize)) as usize
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

// Clean up all the memory, the Gc object passed becomes unusable.
// Unfortunately, C cannot tell this.
pub extern fn gc_drop(gc: Box<Gc>) {
}

const AL_N_INLINE: usize = 29;

pub struct JlArrayList {
    pub len: usize,
    pub max: usize,
    pub items: *mut *mut c_void,
    pub _space: [*mut c_void; AL_N_INLINE],
}

// some helper methods for reading raw data from arraylists
impl JlArrayList {
    pub fn as_slice(&self) -> &[* mut c_void] {
        unsafe {
            slice::from_raw_parts(self.items, self.len)
        }
    }

    pub fn as_slice_mut(&mut self) -> &mut [* mut c_void] {
        unsafe {
            slice::from_raw_parts_mut(self.items, self.len)
        }
    }

    pub fn push(&mut self, e: * mut c_void) {
        unsafe {
            arraylist_push(self, e);
        }
    }
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
    pub exception_in_transit: * mut JlValue,
    pub safepoint: usize, // volatile, TODO: represent volatility
    pub gc_state: GcState, // volatile
    pub in_finalizer: u8, // volatile
    pub disable_gc: u8,
    pub defer_signal: sig_atomic_t, // ???
    pub current_module: * mut JlModule,
    pub current_task: * mut JlTask, // volatile
    pub root_task: * mut JlTask,
    pub task_arg_in_transit: * mut JlValue, // volatile
    pub stackbase: *const c_void,
    pub stack_lo: *const u8,
    pub stack_hi: *const u8,
    pub jmp_target: Option<&'static JlJmpBuf>, // volatile
    pub base_ctx: JlJmpBuf, // base context of stack
    pub safe_restore: Option<&'static JlJmpBuf>,
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
    pub finalizers_inhibited: c_int,
    pub finalizers: JlArrayList,
    // pointer to thread-local GC-related stuff, lifetime is meaningless!
    pub tl_gcs: * mut Gc2<'static>,
}

type JlPTLS<'a> = Option<&'a JlTLS>; // this is just a pointer to thread-local state

// Note: We represent sig_atomic_t as c_int since C99 standard says so.
pub type sig_atomic_t = c_int;

#[derive(PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum GcState {
    GcNotRunning = 0, // GC is not running
    Waiting = 1, // thread is waiting for GC
    Safe = 2, // thread is running unmanaged code that can be executed simultaneously with GC
}

// expose page manager
static mut PAGE_MGR: Option<Mutex<PageMgr>> = None;

// Expose the global page manager. Trying to make thread-safe
// via a mutex; won't do much until we have actual threading used in sweep_pools(), etc
// where this would help.
#[inline(always)]
pub fn pg_mgr<'a>() -> MutexGuard<'a, PageMgr> {
    unsafe {
        PAGE_MGR.as_ref().unwrap().lock().unwrap()
    }
}

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
// Global GC objects
pub static mut big_objects_marked: Option<Box<Mutex<Vec<* mut BigVal>>>> = None;

//------------------------------------------------------------------------------
// Page manager

#[no_mangle]
pub unsafe extern fn neptune_init_page_mgr() {
    println!("page offset: {}", GC_PAGE_OFFSET);

    PAGE_MGR = Some(Mutex::new(PageMgr::new()));
    REGIONS = Some(Vec::with_capacity(REGION_COUNT));
    let regions = REGIONS.as_mut().unwrap();
    for i in 0..REGION_COUNT {
        regions.push(Region::new()); // initialize regions
    }
}

#[no_mangle]
pub unsafe extern fn neptune_alloc_page<'a>() -> * mut u8 {
    // if PAGE_MGR is uninitialized, we're better off crashing anyways
    pg_mgr().alloc_page(&mut REGIONS.as_mut().unwrap()).data.as_mut_ptr()
}

#[no_mangle]
pub unsafe extern fn neptune_free_page<'a>(data: * const u8) {
    pg_mgr().free_page(REGIONS.as_mut().unwrap().as_mut_slice(), data);
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
pub extern fn neptune_init_gc() {
    unsafe {
        big_objects_marked = Some(Box::new(Mutex::new(Vec::new())));
        mark_caches = Some(HashMap::new());
    }

    assert_eq!(mem::size_of::<BigVal>(), 56, "BigVal+TaggedValue should align to 64 bytes!");
    // create thread pool for parallelizing marking and sweeping
    let num_threads = match ::std::env::var("NEPTUNE_THREADS").map_err(GcInitError::Env).and_then(|nthreads| {
        nthreads.parse::<u32>().map_err(GcInitError::Parse)
    }) {
        Ok(0) => panic!("Garbage collector cannot work with 0 worker threads! Set NEPTUNE_THREADS to a positive number."),
        Ok(n) => n,
        Err(GcInitError::Env(env::VarError::NotPresent)) => 1, // if no environment variable given, assume single_threaded (1)
        Err(_) => panic!("Expected environment variable NEPTUNE_THREADS to be defined as a positive number.")
    };
    println!("Starting neptune with {} threads", num_threads);
    unsafe { np_threads = Some(Pool::new(num_threads)) };
}

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
pub extern fn neptune_init_thread_local_gc<'a>(tls: &'static mut JlTLS) -> Box<Gc2<'a>> {
    println!("{} {}", mem::size_of::<JlSVec>(), mem::size_of::<JlTask>());
    Box::new(Gc2::new(tls))
}

// Corresponds to _jl_gc_collect
#[no_mangle]
pub extern fn neptune_gc_collect<'gc, 'a>(gc: &'gc mut Gc2<'a>, full: c_int) -> c_int {
    gc.collect(full != 0) as c_int
}

// Tracking malloc'd data
#[no_mangle]
pub unsafe extern fn jl_gc_track_malloced_array(tls: &'static mut JlTLS, a: * mut JlArray) {
    (*tls.tl_gcs).track_malloced_array(a);
}

#[no_mangle]
pub extern fn neptune_visit_mark_stack(gc: &mut Gc2) {
    gc.marking.visit_mark_stack();
}

#[no_mangle]
pub extern fn neptune_mark_roots(gc: &mut Gc2) {
    gc.marking.mark_roots();
}

#[no_mangle]
pub extern fn neptune_mark_thread_local(gc: &mut Gc2, gc2: &mut Gc2) {
    gc.marking.mark_thread_local(gc2);
}

#[no_mangle]
pub extern fn neptune_setmark_buf(gc: &mut Gc2, o: * mut JlValue, mark_mode: u8, minsz: usize) {
    gc.cache.setmark_buf(o, mark_mode, minsz);
}

#[no_mangle]
pub extern fn neptune_exit_hook() {
}

//----------------------------------------------------------------------------------
// Write barrier entry points
#[no_mangle]
pub extern fn neptune_queue_root(gc: &mut Gc2, root: &mut JlValue) {
    gc.queue_root(root);
}

#[no_mangle]
pub extern fn neptune_queue_binding<'a>(gc: &mut Gc2<'a>, binding: &'a mut JlBinding<'a>) {
    gc.queue_binding(binding);
}

#[no_mangle]
pub unsafe extern fn jl_gc_setmark(tls: &mut JlTLS, v: * mut JlValue) {
    let gc = &mut *tls.tl_gcs;
    gc.mark_concurrently(v);
}

//----------------------------------------------------------------------------------
// Push objects to heap for becoming managed

#[no_mangle]
pub extern fn neptune_push_weakref(gc: &mut Gc2, wr: &mut WeakRef) {
    gc.push_weakref(wr);
}

#[no_mangle]
pub unsafe extern fn neptune_push_big_object<'a>(gc: &mut Gc2<'a>, b: &'a mut BigVal) {
    gc.heap.big_objects.push(b);
}

//----------------------------------------------------------------------------------
// Access to stats for gc_time
#[no_mangle]
pub unsafe extern fn neptune_remset_len_(gc: &mut Gc2, last_remset: u8) -> usize {
    if last_remset != 0 {
        gc.heap.last_remset.len()
    } else {
        gc.heap.remset.len()
    }
}

#[no_mangle]
pub unsafe extern fn neptune_remset_nptr(gc: &mut Gc2) -> usize {
    gc.heap.remset_nptr
}

//------------------------------------------------------------------------------
// Access to GC cache

#[no_mangle]
pub extern fn neptune_log_perm_scanned_bytes(gc: &mut Gc2, newly_scanned_bytes: usize) {
    gc.cache.perm_scanned_bytes += newly_scanned_bytes;
}
