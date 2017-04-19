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
#define DEFAULT_REGION_PG_COUNT (16 * 8 * 4096) // 8 GB
#else
#define DEFAULT_REGION_PG_COUNT (8 * 4096) // 512 MB
#endif
#define MIN_REGION_PG_COUNT 64 // 1 MB

static int region_pg_cnt = DEFAULT_REGION_PG_COUNT;
static jl_mutex_t pagealloc_lock;
static size_t current_pg_count = 0;

void jl_gc_init_page(void)
{
  neptune_init_page_mgr();
}

#ifndef MAP_NORESERVE // not defined in POSIX, FreeBSD, etc.
#define MAP_NORESERVE (0)
#endif

// Try to allocate a memory block for a region with `pg_cnt` pages.
// Return `NULL` if allocation failed. Result is aligned to `GC_PAGE_SZ`.
static char *jl_gc_try_alloc_region(int pg_cnt)
{
    const size_t pages_sz = sizeof(jl_gc_page_t) * pg_cnt;
    const size_t freemap_sz = sizeof(uint32_t) * pg_cnt / 32;
    const size_t meta_sz = sizeof(jl_gc_pagemeta_t) * pg_cnt;
    size_t alloc_size = pages_sz + freemap_sz + meta_sz;
#ifdef _OS_WINDOWS_
    char *mem = (char*)VirtualAlloc(NULL, alloc_size + GC_PAGE_SZ,
                                    MEM_RESERVE, PAGE_READWRITE);
    if (mem == NULL)
        return NULL;
#else
    if (GC_PAGE_SZ > jl_page_size)
        alloc_size += GC_PAGE_SZ;
    char *mem = (char*)mmap(0, alloc_size, PROT_READ | PROT_WRITE,
                            MAP_NORESERVE | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mem == MAP_FAILED)
        return NULL;
#endif
    if (GC_PAGE_SZ > jl_page_size) {
        // round data pointer up to the nearest gc_page_data-aligned
        // boundary if mmap didn't already do so.
        mem = (char*)gc_page_data(mem + GC_PAGE_SZ - 1);
    }
    return mem;
}

// Allocate the memory for a `region_t`. Starts with `region_pg_cnt` number
// of pages. Decrease 4x every time so that there are enough space for a few.
// more regions (or other allocations). The final page count is recorded
// and will be used as the starting count next time. If the page count is
// smaller `MIN_REGION_PG_COUNT` a `jl_memory_exception` is thrown.
// Assume `pagealloc_lock` is acquired, the lock is released before the
// exception is thrown.
static void jl_gc_alloc_region(region_t *region)
{
    int pg_cnt = region_pg_cnt;
    char *mem = NULL;
    while (1) {
        if (__likely((mem = jl_gc_try_alloc_region(pg_cnt))))
            break;
        if (pg_cnt >= MIN_REGION_PG_COUNT * 4) {
            pg_cnt /= 4;
            region_pg_cnt = pg_cnt;
        }
        else if (pg_cnt > MIN_REGION_PG_COUNT) {
            region_pg_cnt = pg_cnt = MIN_REGION_PG_COUNT;
        }
        else {
            JL_UNLOCK_NOGC(&pagealloc_lock);
            jl_throw(jl_memory_exception);
        }
    }
    const size_t pages_sz = sizeof(jl_gc_page_t) * pg_cnt;
    const size_t allocmap_sz = sizeof(uint32_t) * pg_cnt / 32;
    region->pages = (jl_gc_page_t*)mem;
    region->allocmap = (uint32_t*)(mem + pages_sz);
    region->meta = (jl_gc_pagemeta_t*)(mem + pages_sz +allocmap_sz);
    region->lb = 0;
    region->ub = 0;
    region->pg_cnt = pg_cnt;
#ifdef _OS_WINDOWS_
    VirtualAlloc(region->allocmap, pg_cnt / 8, MEM_COMMIT, PAGE_READWRITE);
    VirtualAlloc(region->meta, pg_cnt * sizeof(jl_gc_pagemeta_t),
                 MEM_COMMIT, PAGE_READWRITE);
#endif
}

NOINLINE void *jl_gc_alloc_page(void)
{
  return neptune_alloc_page(regions);
}

void jl_gc_free_page(void *p)
{
  neptune_free_page(regions, jl_page_size, p);
}

#ifdef __cplusplus
}
#endif
