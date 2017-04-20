#ifndef NEPTUNE_H
#define NEPTUNE_H
#define NEPTUNE 0xC60D

#include "gc.h"

// page manager
void neptune_init_page_mgr(void);
void * neptune_alloc_page(void);
void neptune_free_page(size_t gc_page_size, void * page);

#endif // NEPTUNE_H
