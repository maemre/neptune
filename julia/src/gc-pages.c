// This file is a part of Julia. License is MIT: http://julialang.org/license

#include "gc.h"
#include "neptune.h"
#ifndef _OS_WINDOWS_
#  include <sys/resource.h>
#endif

#ifdef __cplusplus
extern "C" {
#endif

// A region is contiguous storage for up to DEFAULT_REGION_PG_COUNT naturally aligned GC_PAGE_SZ pages
// It uses a very naive allocator (see jl_gc_alloc_page & jl_gc_free_page)
#if defined(_P64)
#define DEFAULT_REGION_PG_COUNT (4 * 8 * 4096) // 2 GB, easier to debug
#else
#define DEFAULT_REGION_PG_COUNT (8 * 4096) // 512 MB
#endif
#define MIN_REGION_PG_COUNT 64 // 1 MB

void jl_gc_init_page(void)
{
  neptune_init_page_mgr();
}


void *jl_gc_alloc_page(void)
{
  return neptune_alloc_page();
}

void jl_gc_free_page(void *p)
{
  neptune_free_page(p);
}

#ifdef __cplusplus
}
#endif
