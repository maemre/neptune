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

#endif // NEPTUNE_H
