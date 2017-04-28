#ifndef NEPTUNE_PREDEF_H
#define NEPTUNE_PREDEF_H
#define NEPTUNE 0xC60D

// Allocator entry points
jl_value_t *neptune_alloc(tl_gcs_t * gc, size_t sz, void *typ);
jl_value_t *neptune_pool_alloc(tl_gcs_t * gc, size_t size);
jl_value_t *neptune_big_alloc(tl_gcs_t * gc, size_t size);

#endif // NEPTUNE_PREDEF_H
