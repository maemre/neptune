#ifndef NEPTUNE_PREDEF_H
#define NEPTUNE_PREDEF_H
#define NEPTUNE 0xC60D

// Thread-local initialization
tl_gcs_t *neptune_init_thread_local_gc(jl_ptls_t ptls);

// Allocator entry points
jl_value_t *neptune_alloc(tl_gcs_t * gc, size_t sz, void *typ);
jl_value_t *neptune_pool_alloc(tl_gcs_t * gc, size_t size);
jl_value_t *neptune_big_alloc(tl_gcs_t * gc, size_t size);

// GC
int neptune_gc_collect(tl_gcs_t *gc, bool_t full);

#endif // NEPTUNE_PREDEF_H
