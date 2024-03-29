#ifndef NEPTUNE_H
#define NEPTUNE_H

#ifndef NEPTUNE_PREDEF_H
#include "neptune_predef.h"
#endif

#include "gc.h"

// page manager
void neptune_init_page_mgr(void);
void * neptune_alloc_page(void);
void neptune_free_page(void * page);

// write barrier 
void neptune_queue_root(tl_gcs_t *gc, jl_value_t * root);
void neptune_queue_binding(tl_gcs_t *gc, jl_binding_t * binding);

// to push object to heap
void neptune_push_weakref(tl_gcs_t *gc, jl_weakref_t *wr);
void neptune_push_big_object(tl_gcs_t *gc, bigval_t *b);

// external marking stuff
void neptune_visit_mark_stack(tl_gcs_t *gc);
void neptune_mark_roots(tl_gcs_t *gc);
void neptune_mark_thread_local(tl_gcs_t *gc, tl_gcs_t *gc2);

// get stats about the heap
size_t neptune_remset_len_(tl_gcs_t *gc, uint8_t last_remset);
size_t neptune_remset_nptr(tl_gcs_t *gc);

#define neptune_remset_len(ptls) neptune_remset_len_(ptls->tl_gcs, 0)
#define neptune_last_remset_len(ptls) neptune_remset_len_(ptls->tl_gcs, 1)

// initializetion of GC-side globals
void neptune_init_gc(void);

// exit hook
void neptune_exit_hook(void);

// logs perm. scanned bytes to corresponding GC cache
void neptune_log_perm_scanned_bytes(tl_gcs_t *gc, size_t perm_scanned_bytes);

#endif // NEPTUNE_H
