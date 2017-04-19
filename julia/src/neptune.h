#ifndef NEPTUNE_H
#define NEPTUNE_H
#define NEPTUNE 1

void neptune_init_page_mgr(void);
void *neptune_alloc_page(region_t * gc_regions);
void neptune_free_page(region_t * gc_regions, size_t gc_page_size, void * page);

#endif // NEPTUNE_H
