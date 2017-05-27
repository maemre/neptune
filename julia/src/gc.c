// This file is a part of Julia. License is MIT: http://julialang.org/license

#include "gc.h"
#include "neptune.h"

#ifdef __cplusplus
extern "C" {
#endif

// Protect all access to `finalizer_list_marked` and `to_finalize`.
// For accessing `ptls->finalizers`, the lock is needed if a thread
// is going to realloc the buffer (of its own list) or accessing the
// list of another thread
static jl_mutex_t finalizers_lock;

/**
 * Note about GC synchronization:
 *
 * When entering `jl_gc_collect()`, `jl_gc_running` is atomically changed from
 * `0` to `1` to make sure that only one thread can be running the GC. Other
 * threads that enters `jl_gc_collect()` at the same time (or later calling
 * from unmanaged code) will wait in `jl_gc_collect()` until the GC is finished.
 *
 * Before starting the mark phase the GC thread calls `jl_safepoint_gc_start()`
 * and `jl_gc_wait_for_the_world()`
 * to make sure all the thread are in a safe state for the GC. The function
 * activates the safepoint and wait for all the threads to get ready for the
 * GC (`gc_state != 0`). It also acquires the `finalizers` lock so that no
 * other thread will access them when the GC is running.
 *
 * During the mark and sweep phase of the GC, the threads that are not running
 * the GC should either be running unmanaged code (or code section that does
 * not have a GC critical region mainly including storing to the stack or
 * another object) or paused at a safepoint and wait for the GC to finish.
 * If a thread want to switch from running unmanaged code to running managed
 * code, it has to perform a GC safepoint check after setting the `gc_state`
 * flag (see `jl_gc_state_save_and_set()`. it is possible that the thread might
 * have `gc_state == 0` in the middle of the GC transition back before entering
 * the safepoint. This is fine since the thread won't be executing any GC
 * critical region during that time).
 *
 * The finalizers are run after the GC finishes in normal mode (the `gc_state`
 * when `jl_gc_collect` is called) with `jl_in_finalizer = 1`. (TODO:) When we
 * have proper support of GC transition in codegen, we should execute the
 * finalizers in unmanaged (GC safe) mode.
 */

jl_gc_num_t gc_num = {0,0,0,0,0,0,0,0,0,0,0,0,0,0};
size_t last_long_collect_interval;

// List of marked big objects.  Not per-thread.  Accessed only by master thread.
bigval_t *big_objects_marked = NULL;

// finalization
// `ptls->finalizers` and `finalizer_list_marked` might have tagged pointers.
// If an object pointer has the lowest bit set, the next pointer is an unboxed
// c function pointer.
// `to_finalize` should not have tagged pointers.
arraylist_t finalizer_list_marked;
arraylist_t to_finalize;

#define should_timeout() 0

#ifdef JULIA_ENABLE_THREADING
static void jl_gc_wait_for_the_world(void)
{
    for (int i = 0;i < jl_n_threads;i++) {
        jl_ptls_t ptls2 = jl_all_tls_states[i];
        // FIXME: The acquire load pairs with the release stores
        // in the signal handler of safepoint so we are sure that
        // all the stores on those threads are visible. However,
        // we're currently not using atomic stores in mutator threads.
        // We should either use atomic store release there too or use signals
        // to flush the memory operations on those threads.
        while (!ptls2->gc_state || !jl_atomic_load_acquire(&ptls2->gc_state)) {
            jl_cpu_pause(); // yield?
        }
    }
}
#else
static inline void jl_gc_wait_for_the_world(void)
{
}
#endif

// malloc wrappers, aligned allocation

#define malloc_cache_align(sz) jl_malloc_aligned(sz, JL_CACHE_BYTE_ALIGNMENT)
#define realloc_cache_align(p, sz, oldsz) jl_realloc_aligned(p, sz, oldsz, JL_CACHE_BYTE_ALIGNMENT)

static void schedule_finalization(void *o, void *f)
{
    arraylist_push(&to_finalize, o);
    arraylist_push(&to_finalize, f);
}

static void run_finalizer(jl_ptls_t ptls, jl_value_t *o, jl_value_t *ff)
{
    assert(!jl_typeis(ff, jl_voidpointer_type));
    jl_value_t *args[2] = {ff,o};
    JL_TRY {
        size_t last_age = jl_get_ptls_states()->world_age;
        jl_get_ptls_states()->world_age = jl_world_counter;
        jl_apply(args, 2);
        jl_get_ptls_states()->world_age = last_age;
    }
    JL_CATCH {
        jl_printf(JL_STDERR, "error in running finalizer: ");
        jl_static_show(JL_STDERR, ptls->exception_in_transit);
        jl_printf(JL_STDERR, "\n");
    }
}

// if `need_sync` is true, the `list` is the `finalizers` list of another
// thread and we need additional synchronizations
static void finalize_object(arraylist_t *list, jl_value_t *o,
                            arraylist_t *copied_list, int need_sync)
{
    // The acquire load makes sure that the first `len` objects are valid.
    // If `need_sync` is true, all mutations of the content should be limited
    // to the first `oldlen` elements and no mutation is allowed after the
    // new length is published with the `cmpxchg` at the end of the function.
    // This way, the mutation should not conflict with the owning thread,
    // which only writes to locations later than `len`
    // and will not resize the buffer without acquiring the lock.
    size_t len = need_sync ? jl_atomic_load_acquire(&list->len) : list->len;
    size_t oldlen = len;
    void **items = list->items;
    for (size_t i = 0; i < len; i += 2) {
        void *v = items[i];
        int move = 0;
        if (o == (jl_value_t*)gc_ptr_clear_tag(v, 1)) {
            void *f = items[i + 1];
            move = 1;
            if (gc_ptr_tag(v, 1)) {
                ((void (*)(void*))f)(o);
            }
            else {
                arraylist_push(copied_list, o);
                arraylist_push(copied_list, f);
            }
        }
        if (move || __unlikely(!v)) {
            if (i < len - 2) {
                items[i] = items[len - 2];
                items[i + 1] = items[len - 1];
                i -= 2;
            }
            len -= 2;
        }
    }
    if (oldlen == len)
        return;
    if (need_sync) {
        // The memset needs to be unconditional since the thread might have
        // already read the length.
        // The `memset` (like any other content mutation) has to be done
        // **before** the `cmpxchg` which publishes the length.
        memset(&items[len], 0, (oldlen - len) * sizeof(void*));
        jl_atomic_compare_exchange(&list->len, oldlen, len);
    }
    else {
        list->len = len;
    }
}

// The first two entries are assumed to be empty and the rest are assumed to
// be pointers to `jl_value_t` objects
static void jl_gc_push_arraylist(jl_ptls_t ptls, arraylist_t *list)
{
    void **items = list->items;
    items[0] = (void*)(((uintptr_t)list->len - 2) << 1);
    items[1] = ptls->pgcstack;
    ptls->pgcstack = (jl_gcframe_t*)items;
}

// Same assumption as `jl_gc_push_arraylist`. Requires the finalizers lock
// to be hold for the current thread and will release the lock when the
// function returns.
static void jl_gc_run_finalizers_in_list(jl_ptls_t ptls, arraylist_t *list)
{
    size_t len = list->len;
    jl_value_t **items = (jl_value_t**)list->items;
    jl_gc_push_arraylist(ptls, list);
    JL_UNLOCK_NOGC(&finalizers_lock);
    for (size_t i = 2;i < len;i += 2)
        run_finalizer(ptls, items[i], items[i + 1]);
    JL_GC_POP();
}

static void run_finalizers(jl_ptls_t ptls)
{
    // Racy fast path:
    // The race here should be OK since the race can only happen if
    // another thread is writing to it with the lock held. In such case,
    // we don't need to run pending finalizers since the writer thread
    // will flush it.
    if (to_finalize.len == 0)
        return;
    JL_LOCK_NOGC(&finalizers_lock);
    if (to_finalize.len == 0) {
        JL_UNLOCK_NOGC(&finalizers_lock);
        return;
    }
    arraylist_t copied_list;
    memcpy(&copied_list, &to_finalize, sizeof(copied_list));
    if (to_finalize.items == to_finalize._space) {
        copied_list.items = copied_list._space;
    }
    arraylist_new(&to_finalize, 0);
    // empty out the first two entries for the GC frame
    arraylist_push(&copied_list, copied_list.items[0]);
    arraylist_push(&copied_list, copied_list.items[1]);
    // This releases the finalizers lock.
    jl_gc_run_finalizers_in_list(ptls, &copied_list);
    arraylist_free(&copied_list);
}

JL_DLLEXPORT void jl_gc_enable_finalizers(jl_ptls_t ptls, int on)
{
    int old_val = ptls->finalizers_inhibited;
    int new_val = old_val + (on ? -1 : 1);
    ptls->finalizers_inhibited = new_val;
    if (!new_val && old_val && !ptls->in_finalizer) {
        ptls->in_finalizer = 1;
        run_finalizers(ptls);
        ptls->in_finalizer = 0;
    }
}

static void schedule_all_finalizers(arraylist_t *flist)
{
    void **items = flist->items;
    size_t len = flist->len;
    for(size_t i = 0; i < len; i+=2) {
        void *v = items[i];
        void *f = items[i + 1];
        if (__unlikely(!v))
            continue;
        if (!gc_ptr_tag(v, 1)) {
            schedule_finalization(v, f);
        }
        else {
            ((void (*)(void*))f)(gc_ptr_clear_tag(v, 1));
        }
    }
    flist->len = 0;
}

void jl_gc_run_all_finalizers(jl_ptls_t ptls)
{
    for (int i = 0;i < jl_n_threads;i++) {
        jl_ptls_t ptls2 = jl_all_tls_states[i];
        schedule_all_finalizers(&ptls2->finalizers);
    }
    schedule_all_finalizers(&finalizer_list_marked);
    run_finalizers(ptls);
}

static void gc_add_finalizer_(jl_ptls_t ptls, void *v, void *f)
{
    int8_t gc_state = jl_gc_unsafe_enter(ptls);
    arraylist_t *a = &ptls->finalizers;
    // This acquire load and the release store at the end are used to
    // synchronize with `finalize_object` on another thread. Apart from the GC,
    // which is blocked by entering a unsafe region, there might be only
    // one other thread accessing our list in `finalize_object`
    // (only one thread since it needs to acquire the finalizer lock).
    // Similar to `finalize_object`, all content mutation has to be done
    // between the acquire and the release of the length.
    size_t oldlen = jl_atomic_load_acquire(&a->len);
    if (__unlikely(oldlen + 2 > a->max)) {
        JL_LOCK_NOGC(&finalizers_lock);
        // `a->len` might have been modified.
        // Another possiblility is to always grow the array to `oldlen + 2` but
        // it's simpler this way and uses slightly less memory =)
        oldlen = a->len;
        arraylist_grow(a, 2);
        a->len = oldlen;
        JL_UNLOCK_NOGC(&finalizers_lock);
    }
    void **items = a->items;
    items[oldlen] = v;
    items[oldlen + 1] = f;
    jl_atomic_store_release(&a->len, oldlen + 2);
    jl_gc_unsafe_leave(ptls, gc_state);
}

STATIC_INLINE void gc_add_ptr_finalizer(jl_ptls_t ptls, jl_value_t *v, void *f)
{
    gc_add_finalizer_(ptls, (void*)(((uintptr_t)v) | 1), f);
}

JL_DLLEXPORT void jl_gc_add_finalizer_th(jl_ptls_t ptls, jl_value_t *v,
                                         jl_function_t *f)
{
    if (__unlikely(jl_typeis(f, jl_voidpointer_type))) {
        gc_add_ptr_finalizer(ptls, v, jl_unbox_voidpointer(f));
    }
    else {
        gc_add_finalizer_(ptls, v, f);
    }
}

JL_DLLEXPORT void jl_gc_add_ptr_finalizer(jl_ptls_t ptls, jl_value_t *v, void *f)
{
    gc_add_ptr_finalizer(ptls, v, f);
}

JL_DLLEXPORT void jl_finalize_th(jl_ptls_t ptls, jl_value_t *o)
{
    JL_LOCK_NOGC(&finalizers_lock);
    // Copy the finalizers into a temporary list so that code in the finalizer
    // won't change the list as we loop through them.
    // This list is also used as the GC frame when we are running the finalizers
    arraylist_t copied_list;
    arraylist_new(&copied_list, 0);
    arraylist_push(&copied_list, NULL); // GC frame size to be filled later
    arraylist_push(&copied_list, NULL); // pgcstack to be filled later
    // No need to check the to_finalize list since the user is apparently
    // still holding a reference to the object
    for (int i = 0;i < jl_n_threads;i++) {
        jl_ptls_t ptls2 = jl_all_tls_states[i];
        finalize_object(&ptls2->finalizers, o, &copied_list, ptls != ptls2);
    }
    finalize_object(&finalizer_list_marked, o, &copied_list, 0);
    if (copied_list.len > 2) {
        // This releases the finalizers lock.
        jl_gc_run_finalizers_in_list(ptls, &copied_list);
    }
    else {
        JL_UNLOCK_NOGC(&finalizers_lock);
    }
    arraylist_free(&copied_list);
}

// GC knobs and self-measurement variables
static int64_t last_gc_total_bytes = 0;

#ifdef _P64
#define default_collect_interval (5600*1024*sizeof(void*))
static size_t max_collect_interval = 1250000000UL;
#else
#define default_collect_interval (3200*1024*sizeof(void*))
static size_t max_collect_interval =  500000000UL;
#endif

// global variables for GC stats

// Resetting the object to a young object, this is used when marking the
// finalizer list to collect them the next time because the object is very
// likely dead. This also won't break the GC invariance since these objects
// are not reachable from anywhere else.
int mark_reset_age = 0;

/*
 * The state transition looks like :
 *
 * ([(quick)sweep] means either a sweep or a quicksweep)
 *
 * <-[(quick)sweep]-
 *                 |
 *     ---->  GC_OLD  <--[(quick)sweep && age>promotion]--
 *     |     |                                           |
 *     |     |  GC_MARKED (in remset)                    |
 *     |     |     ^            |                        |
 *     |   [mark]  |          [mark]                     |
 *     |     |     |            |                        |
 *     |     |     |            |                        |
 *  [sweep]  | [write barrier]  |                        |
 *     |     v     |            v                        |
 *     ----- GC_OLD_MARKED <----                         |
 *              |               ^                        |
 *              |               |                        |
 *              --[quicksweep]---                        |
 *                                                       |
 *  ========= above this line objects are old =========  |
 *                                                       |
 *  ----[new]------> GC_CLEAN ------[mark]-----------> GC_MARKED
 *                    |    ^                                   |
 *  <-[(quick)sweep]---    |                                   |
 *                         --[(quick)sweep && age<=promotion]---
 */

// A quick sweep is a sweep where `!sweep_full`
// It means we won't touch GC_OLD_MARKED objects (old gen).

// When a reachable object has survived more than PROMOTE_AGE+1 collections
// it is tagged with GC_OLD during sweep and will be promoted on next mark
// because at that point we can know easily if it references young objects.
// Marked old objects that reference young ones are kept in the remset.

// When a write barrier triggers, the offending marked object is both queued,
// so as not to trigger the barrier again, and put in the remset.


#define PROMOTE_AGE 1
// this cannot be increased as is without changing :
// - sweep_page which is specialized for 1bit age
// - the size of the age storage in region_t


int64_t scanned_bytes; // young bytes scanned while marking
int64_t perm_scanned_bytes; // old bytes scanned while marking
int prev_sweep_full = 1;

#define inc_sat(v,s) v = (v) >= s ? s : (v)+1

// Full collection heuristics
int64_t live_bytes = 0;
int64_t promoted_bytes = 0;

int64_t last_full_live_ub = 0;
int64_t last_full_live_est = 0;
// upper bound and estimated live object sizes
// This heuristic should be really unlikely to trigger.
// However, this should be simple enough to trigger a full collection
// when it's necessary if other heuristics are messed up.
// It is also possible to take the total memory available into account
// if necessary.
int gc_check_heap_size(int64_t sz_ub, int64_t sz_est)
{
    if (__unlikely(!last_full_live_ub || last_full_live_ub > sz_ub)) {
        last_full_live_ub = sz_ub;
    }
    else if (__unlikely(last_full_live_ub * 3 / 2 < sz_ub)) {
        return 1;
    }
    if (__unlikely(!last_full_live_est || last_full_live_est > sz_est)) {
        last_full_live_est = sz_est;
    }
    else if (__unlikely(last_full_live_est * 2 < sz_est)) {
        return 1;
    }
    return 0;
}

void gc_update_heap_size(int64_t sz_ub, int64_t sz_est)
{
    last_full_live_ub = sz_ub;
    last_full_live_est = sz_est;
}

#define should_collect() (__unlikely(gc_num.allocd>0))

static inline int maybe_collect(jl_ptls_t ptls)
{
    if (should_collect() || gc_debug_check_other()) {
        jl_gc_collect(0);
        return 1;
    }
    jl_gc_safepoint_(ptls);
    return 0;
}

// weak references

JL_DLLEXPORT jl_weakref_t *jl_gc_new_weakref_th(jl_ptls_t ptls,
                                                jl_value_t *value)
{
    jl_weakref_t *wr = (jl_weakref_t*)jl_gc_alloc(ptls, sizeof(void*),
                                                  jl_weakref_type);
    wr->value = value;  // NOTE: wb not needed here
    neptune_push_weakref(ptls->tl_gcs, wr);
    return wr;
}

// big value list

// Size includes the tag and the tag is not cleared!!
JL_DLLEXPORT jl_value_t *jl_gc_big_alloc(jl_ptls_t ptls, size_t sz)
{
  return neptune_big_alloc(ptls->tl_gcs, sz);
}

// tracking Arrays with malloc'd storage

void jl_gc_count_allocd(size_t sz)
{
    // This is **NOT** a GC safe point.
    gc_num.allocd += sz;
}

void jl_gc_reset_alloc_count(void)
{
    live_bytes += (gc_num.deferred_alloc + (gc_num.allocd + gc_num.interval));
    gc_num.allocd = -(int64_t)gc_num.interval;
    gc_num.deferred_alloc = 0;
}

// Size includes the tag and the tag is not cleared!!
JL_DLLEXPORT jl_value_t *jl_gc_pool_alloc(jl_ptls_t ptls, int pool_offset,
                                          int osize)
{
    // Use neptune's pool allocator
    return neptune_pool_alloc(ptls->tl_gcs, osize + sizeof(jl_taggedvalue_t));
}

int jl_gc_classify_pools(size_t sz, int *osize)
{
    if (sz > GC_MAX_SZCLASS)
        return -1;
    size_t allocsz = sz + sizeof(jl_taggedvalue_t);
    int klass = jl_gc_szclass(allocsz);
    *osize = jl_gc_sizeclasses[klass];
    // ACHTUNG: dirty hack
    // Note: resulting offset is _never used because of how our pool allocation works
    return 0; // (int)(intptr_t)(&((jl_ptls_t)0)->heap.norm_pools[klass]);
}

// sweep phase

int64_t lazy_freed_pages = 0;

JL_DLLEXPORT void jl_gc_queue_root(jl_value_t *ptr)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    neptune_queue_root(ptls->tl_gcs, ptr);
}

void gc_queue_binding(jl_binding_t *bnd)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    neptune_queue_binding(ptls->tl_gcs, bnd);
}

void visit_mark_stack(jl_ptls_t ptls)
{
  neptune_visit_mark_stack(ptls->tl_gcs);
}

extern jl_array_t *jl_module_init_order;
extern jl_typemap_entry_t *call_cache[N_CALL_CACHE];
extern jl_array_t *jl_all_methods;

// collector entry point and control
static volatile uint32_t jl_gc_disable_counter = 0;

JL_DLLEXPORT int jl_gc_enable(int on)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    int prev = !ptls->disable_gc;
    ptls->disable_gc = (on == 0);
    if (on && !prev) {
        // disable -> enable
        if (jl_atomic_fetch_add(&jl_gc_disable_counter, -1) == 1) {
            gc_num.allocd += gc_num.deferred_alloc;
            gc_num.deferred_alloc = 0;
        }
    }
    else if (prev && !on) {
        // enable -> disable
        jl_atomic_fetch_add(&jl_gc_disable_counter, 1);
        // check if the GC is running and wait for it to finish
        jl_gc_safepoint_(ptls);
    }
    return prev;
}
JL_DLLEXPORT int jl_gc_is_enabled(void)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return !ptls->disable_gc;
}

JL_DLLEXPORT int64_t jl_gc_total_bytes(void)
{
    // Sync this logic with `base/util.jl:GC_Diff`
    return (gc_num.total_allocd + gc_num.deferred_alloc +
            gc_num.allocd + gc_num.interval);
}
JL_DLLEXPORT uint64_t jl_gc_total_hrtime(void)
{
    return gc_num.total_time;
}
JL_DLLEXPORT jl_gc_num_t jl_gc_num(void)
{
    return gc_num;
}

JL_DLLEXPORT int64_t jl_gc_diff_total_bytes(void)
{
    int64_t oldtb = last_gc_total_bytes;
    int64_t newtb = jl_gc_total_bytes();
    last_gc_total_bytes = newtb;
    return newtb - oldtb;
}
void jl_gc_sync_total_bytes(void) {last_gc_total_bytes = jl_gc_total_bytes();}

static void jl_gc_mark_ptrfree(jl_ptls_t ptls)
{
    // Pointer-free objects, can be marked concurrently
    jl_mark_box_caches(ptls);
    jl_gc_setmark(ptls, (jl_value_t*)jl_emptysvec);
    jl_gc_setmark(ptls, jl_emptytuple);
    jl_gc_setmark(ptls, jl_true);
    jl_gc_setmark(ptls, jl_false);
}

// Only one thread should be running in this function
static int _jl_gc_collect(jl_ptls_t ptls, int full)
{
  return neptune_gc_collect(ptls->tl_gcs, full);
}

#ifdef NEPTUNE
#define GC_COLLECT(ptls, full) _jl_gc_collect(ptls, full)
#else
#define GC_COLLECT(ptls, full) neptune_gc_collect(ptls, full)
#endif

JL_DLLEXPORT void jl_gc_collect(int full)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    if (jl_gc_disable_counter) {
        gc_num.deferred_alloc += (gc_num.allocd + gc_num.interval);
        gc_num.allocd = -(int64_t)gc_num.interval;
        return;
    }
    gc_debug_print();

    int8_t old_state = jl_gc_state(ptls);
    ptls->gc_state = JL_GC_STATE_WAITING;
    // `jl_safepoint_start_gc()` makes sure only one thread can
    // run the GC.
    if (!jl_safepoint_start_gc()) {
        // Multithread only. See assertion in `safepoint.c`
        jl_gc_state_set(ptls, old_state, JL_GC_STATE_WAITING);
        return;
    }
    JL_TIMING(GC);
    // Now we are ready to wait for other threads to hit the safepoint,
    // we can do a few things that doesn't require synchronization.
    jl_gc_mark_ptrfree(ptls);
    // no-op for non-threading
    jl_gc_wait_for_the_world();

    // TODO: Here would be a good place to call our Rust garbage collection code,
    //       now that the threads have all stopped and reached a safe point
    if (!jl_gc_disable_counter) {
        JL_LOCK_NOGC(&finalizers_lock);
        if (GC_COLLECT(ptls, full)) {
          // TODO: determine what to needs to change in the rest of this block
            jl_gc_mark_ptrfree(ptls);
            int ret = GC_COLLECT(ptls, 0);
            (void)ret;
            assert(!ret);
        }
        JL_UNLOCK_NOGC(&finalizers_lock);
    }

    // no-op for non-threading
    jl_safepoint_end_gc();
    jl_gc_state_set(ptls, old_state, JL_GC_STATE_WAITING);

    // Only disable finalizers on current thread
    // Doing this on all threads is racy (it's impossible to check
    // or wait for finalizers on other threads without dead lock).
    if (!ptls->finalizers_inhibited) {
        int8_t was_in_finalizer = ptls->in_finalizer;
        ptls->in_finalizer = 1;
        run_finalizers(ptls);
        ptls->in_finalizer = was_in_finalizer;
    }
}

void mark_all_roots(jl_ptls_t ptls)
{
    for (size_t i = 0; i < jl_n_threads; i++)
      neptune_mark_thread_local(ptls->tl_gcs, jl_all_tls_states[i]->tl_gcs);
    neptune_mark_roots(ptls);
    jl_gc_mark_ptrfree(ptls);
}

// allocator entry points

JL_DLLEXPORT jl_value_t *(jl_gc_alloc)(jl_ptls_t ptls, size_t sz, void *ty)
{
    return jl_gc_alloc_(ptls, sz, ty);
}

// Per-thread initialization (when threading is fully implemented)
void jl_mk_thread_heap(jl_ptls_t ptls)
{
    arraylist_new(&ptls->finalizers, 0);
}

// System-wide initializations
void jl_gc_init(void)
{
    jl_gc_init_page();
    gc_debug_init();

    arraylist_new(&finalizer_list_marked, 0);
    arraylist_new(&to_finalize, 0);

    gc_num.interval = default_collect_interval;
    last_long_collect_interval = default_collect_interval;
    gc_num.allocd = -default_collect_interval;

#ifdef _P64
    // on a big memory machine, set max_collect_interval to totalmem * nthreads / ncores / 2
    size_t maxmem = (uv_get_total_memory() * jl_n_threads) / jl_cpu_cores() / 2;
    if (maxmem > max_collect_interval)
        max_collect_interval = maxmem;
#endif
}

JL_DLLEXPORT void *jl_gc_counted_malloc(size_t sz)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    sz += JL_SMALL_BYTE_ALIGNMENT;
    maybe_collect(ptls);
    gc_num.allocd += sz;
    gc_num.malloc++;
    void *b = malloc(sz);
    if (b == NULL)
        jl_throw(jl_memory_exception);
   return b;
}

JL_DLLEXPORT void *jl_gc_counted_calloc(size_t nm, size_t sz)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    nm += JL_SMALL_BYTE_ALIGNMENT;
    maybe_collect(ptls);
    gc_num.allocd += nm*sz;
    gc_num.malloc++;
    void *b = calloc(nm, sz);
    if (b == NULL)
        jl_throw(jl_memory_exception);
    return b;
}

JL_DLLEXPORT void jl_gc_counted_free(void *p, size_t sz)
{
    free(p);
    gc_num.freed += sz + JL_SMALL_BYTE_ALIGNMENT;
    gc_num.freecall++;
}

JL_DLLEXPORT void *jl_gc_counted_realloc_with_old_size(void *p, size_t old, size_t sz)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    old += JL_SMALL_BYTE_ALIGNMENT;
    sz += JL_SMALL_BYTE_ALIGNMENT;
    maybe_collect(ptls);
    if (sz < old)
       gc_num.freed += (old - sz);
    else
       gc_num.allocd += (sz - old);
    gc_num.realloc++;
    void *b = realloc(p, sz);
    if (b == NULL)
        jl_throw(jl_memory_exception);
    return b;
}

JL_DLLEXPORT void *jl_malloc(size_t sz)
{
    int64_t *p = (int64_t *)jl_gc_counted_malloc(sz);
    p[0] = sz;
    return (void *)(p + 2);
}

JL_DLLEXPORT void *jl_calloc(size_t nm, size_t sz)
{
    int64_t *p;
    size_t nmsz = nm*sz;
    p = (int64_t *)jl_gc_counted_calloc(nmsz, 1);
    p[0] = nmsz;
    return (void *)(p + 2);
}

JL_DLLEXPORT void jl_free(void *p)
{
    if (p != NULL) {
        int64_t *pp = (int64_t *)p - 2;
        size_t sz = pp[0];
        jl_gc_counted_free(pp, sz);
    }
}

JL_DLLEXPORT void *jl_realloc(void *p, size_t sz)
{
    int64_t *pp;
    size_t szold;
    if (p == NULL) {
        pp = NULL;
        szold = 0;
    }
    else {
        pp = (int64_t *)p - 2;
        szold = pp[0];
    }
    int64_t *pnew = (int64_t *)jl_gc_counted_realloc_with_old_size(pp, szold, sz);
    pnew[0] = sz;
    return (void *)(pnew + 2);
}

JL_DLLEXPORT void *jl_gc_managed_malloc(size_t sz)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    maybe_collect(ptls);
    size_t allocsz = LLT_ALIGN(sz, JL_CACHE_BYTE_ALIGNMENT);
    if (allocsz < sz)  // overflow in adding offs, size was "negative"
        jl_throw(jl_memory_exception);
    gc_num.allocd += allocsz;
    gc_num.malloc++;
    void *b = malloc_cache_align(allocsz);
    if (b == NULL)
        jl_throw(jl_memory_exception);
    return b;
}

static void *gc_managed_realloc_(jl_ptls_t ptls, void *d, size_t sz, size_t oldsz,
                                 int isaligned, jl_value_t *owner, int8_t can_collect)
{
    if (can_collect)
        maybe_collect(ptls);

    size_t allocsz = LLT_ALIGN(sz, JL_CACHE_BYTE_ALIGNMENT);
    if (allocsz < sz)  // overflow in adding offs, size was "negative"
        jl_throw(jl_memory_exception);

    if (jl_astaggedvalue(owner)->bits.gc == GC_OLD_MARKED) {
        ptls->gc_cache.perm_scanned_bytes += allocsz - oldsz;
        live_bytes += allocsz - oldsz;
    }
    else if (allocsz < oldsz)
        gc_num.freed += (oldsz - allocsz);
    else
        gc_num.allocd += (allocsz - oldsz);
    gc_num.realloc++;

    void *b;
    if (isaligned)
        b = realloc_cache_align(d, allocsz, oldsz);
    else
        b = realloc(d, allocsz);
    if (b == NULL)
        jl_throw(jl_memory_exception);

    return b;
}

JL_DLLEXPORT void *jl_gc_managed_realloc(void *d, size_t sz, size_t oldsz,
                                         int isaligned, jl_value_t *owner)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return gc_managed_realloc_(ptls, d, sz, oldsz, isaligned, owner, 1);
}

jl_value_t *jl_gc_realloc_string(jl_value_t *s, size_t sz)
{
    size_t len = jl_string_len(s);
    if (sz <= len) return s;
    jl_taggedvalue_t *v = jl_astaggedvalue(s);
    size_t strsz = len + sizeof(size_t) + 1;
    if (strsz <= GC_MAX_SZCLASS ||
        // TODO: because of issue #17971 we can't resize old objects
        gc_marked(v->bits.gc)) {
        // pool allocated; can't be grown in place so allocate a new object.
        jl_value_t *snew = jl_alloc_string(sz);
        memcpy(jl_string_data(snew), jl_string_data(s), len);
        return snew;
    }
    size_t newsz = sz + sizeof(size_t) + 1;
    size_t offs = offsetof(bigval_t, header);
    size_t allocsz = LLT_ALIGN(newsz + offs, JL_CACHE_BYTE_ALIGNMENT);
    if (allocsz < sz)  // overflow in adding offs, size was "negative"
        jl_throw(jl_memory_exception);
    bigval_t *hdr = bigval_header(v);
    jl_ptls_t ptls = jl_get_ptls_states();
    maybe_collect(ptls); // don't want this to happen during jl_gc_managed_realloc
    gc_big_object_unlink(hdr);
    // TODO: this is not safe since it frees the old pointer. ideally we'd like
    // the old pointer to be left alone if we can't grow in place.
    // for now it's up to the caller to make sure there are no references to the
    // old pointer.
    bigval_t *newbig =
        (bigval_t*)gc_managed_realloc_(ptls, hdr, allocsz, LLT_ALIGN(strsz+offs, JL_CACHE_BYTE_ALIGNMENT),
                                       1, s, 0);
    newbig->sz = allocsz;
    newbig->age = 0;
    neptune_push_big_object(ptls->tl_gcs, newbig);
    jl_value_t *snew = jl_valueof(&newbig->header);
    *(size_t*)snew = sz;
    return snew;
}

// Perm gen allocator
// 2M pool
#define GC_PERM_POOL_SIZE (2 * 1024 * 1024)
// 20k limit for pool allocation. At most 1% fragmentation
#define GC_PERM_POOL_LIMIT (20 * 1024)
jl_mutex_t gc_perm_lock = {0, 0};
static char *gc_perm_pool = NULL;
static size_t gc_perm_size = 0;

// **NOT** a safepoint
void *jl_gc_perm_alloc_nolock(size_t sz)
{
    // The caller should have acquired `gc_perm_lock`
#ifndef MEMDEBUG
    if (__unlikely(sz > GC_PERM_POOL_LIMIT))
#endif
        return malloc(sz);
    sz = LLT_ALIGN(sz, JL_SMALL_BYTE_ALIGNMENT);
    if (__unlikely(sz > gc_perm_size)) {
#ifdef _OS_WINDOWS_
        void *pool = VirtualAlloc(NULL,
                                  GC_PERM_POOL_SIZE + JL_SMALL_BYTE_ALIGNMENT,
                                  MEM_COMMIT, PAGE_READWRITE);
        if (__unlikely(pool == NULL))
            return NULL;
        pool = (void*)LLT_ALIGN((uintptr_t)pool, JL_SMALL_BYTE_ALIGNMENT);
#else
        void *pool = mmap(0, GC_PERM_POOL_SIZE, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (__unlikely(pool == MAP_FAILED))
            return NULL;
#endif
        gc_perm_pool = (char*)pool;
        gc_perm_size = GC_PERM_POOL_SIZE;
    }
    assert(((uintptr_t)gc_perm_pool) % JL_SMALL_BYTE_ALIGNMENT == 0);
    void *p = gc_perm_pool;
    gc_perm_size -= sz;
    gc_perm_pool += sz;
    return p;
}

// **NOT** a safepoint
void *jl_gc_perm_alloc(size_t sz)
{
#ifndef MEMDEBUG
    if (__unlikely(sz > GC_PERM_POOL_LIMIT))
#endif
        return malloc(sz);
    JL_LOCK_NOGC(&gc_perm_lock);
    void *p = jl_gc_perm_alloc_nolock(sz);
    JL_UNLOCK_NOGC(&gc_perm_lock);
    return p;
}

JL_DLLEXPORT void jl_gc_add_finalizer(jl_value_t *v, jl_function_t *f)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    jl_gc_add_finalizer_th(ptls, v, f);
}

JL_DLLEXPORT void jl_finalize(jl_value_t *o)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    jl_finalize_th(ptls, o);
}

JL_DLLEXPORT jl_weakref_t *jl_gc_new_weakref(jl_value_t *value)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return jl_gc_new_weakref_th(ptls, value);
}

JL_DLLEXPORT jl_value_t *jl_gc_allocobj(size_t sz)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return jl_gc_alloc(ptls, sz, NULL);
}

JL_DLLEXPORT jl_value_t *jl_gc_alloc_0w(void)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return jl_gc_alloc(ptls, 0, NULL);
}

JL_DLLEXPORT jl_value_t *jl_gc_alloc_1w(void)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return jl_gc_alloc(ptls, sizeof(void*), NULL);
}

JL_DLLEXPORT jl_value_t *jl_gc_alloc_2w(void)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return jl_gc_alloc(ptls, sizeof(void*) * 2, NULL);
}

JL_DLLEXPORT jl_value_t *jl_gc_alloc_3w(void)
{
    jl_ptls_t ptls = jl_get_ptls_states();
    return jl_gc_alloc(ptls, sizeof(void*) * 3, NULL);
}

#ifdef __cplusplus
}
#endif
