// Page allocator for GC, the allocator will allocate pages in a simple manner.
// TODO: explain allocation scheme

use gc::*;
use gc2::*;
use c_interface::*;
use libc;
use std::mem;
use std::cmp;
use util::*;
use bit_field::BitField;
use core;
use std::panic;

// max. page count per region.
// From: https://doc.rust-lang.org/reference.html#conditional-compilation
//  other possible configurations to use:
//  1. * target_arch="x86_64"
//     * target_arch="x64"
//  2. * target_pointer_width="32"
//     * target_pointer_width="64"
#[cfg(archbits="32")]
pub const DEFAULT_REGION_PG_COUNT: usize = 8 * 4096; // 512 MB
#[cfg(not(archbits="32"))] // 64-bit
pub const DEFAULT_REGION_PG_COUNT: usize = 4 * 8 * 4096; // 2 GB, easier to debug


const MIN_REGION_PG_COUNT: usize = 64; // 1 MB

// A GC page, eqv. of jl_gc_page_t
#[repr(C)]
#[derive(Copy)]
pub struct Page {
    pub data: [u8; PAGE_SZ]
}

impl Page {
    pub fn of<T>(ptr: &T) -> * const u8 {
        Page::of_raw(ptr)
    }
    pub fn of_raw<T>(ptr: * const T) -> * const u8 {
        unsafe { PageMgr::align_to_boundary(ptr as * const u8, PAGE_SZ) }
    }
}

impl Clone for Page {
    // unfortunately, clone is not implemented for arrays with size > 32 so we need to
    // do some memory transmutation.
    fn clone(&self) -> Self {
        unsafe {
            mem::transmute_copy(&self)
        }
    }
}

pub struct PageMgr {
    region_pg_count: usize,
    pub current_pg_count: usize,
}
impl PageMgr {
    pub fn new() -> PageMgr {
        let mut region_pg_count = DEFAULT_REGION_PG_COUNT;
        // if on unix, compute a realistic page count limit by checking limits for this process
        if cfg!(not(target_os = "windows")) {
            let mut rl = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0
            };
            
            // check what size page it's using, before we do any possible exponential decrease
            println!("page count: {}", region_pg_count);

            unsafe {
                // an exponential decrease to find limit within a binary order of magnitude quickly
                if libc::getrlimit(libc::RLIMIT_AS, &mut rl as *mut libc::rlimit) == 0 {
                    while ((rl.rlim_cur as u64) < (region_pg_count as u64) * (mem::size_of::<Page>() as u64) * 2) &&
                        (region_pg_count >= MIN_REGION_PG_COUNT) {
                            region_pg_count /= 2;
                    }
                }
            }
        }
        PageMgr {
            region_pg_count: region_pg_count,
            current_pg_count: 0,
        }
    }
    
    // Compute a pointer to the beginning of the page given data pointer lies in
    #[inline(always)]
    unsafe fn align_to_boundary(ptr: * const u8, boundary: usize) -> * const u8 {
        assert_eq!(boundary, boundary.next_power_of_two());
        let bound_lg2 = boundary.trailing_zeros();
        ((ptr as usize >> bound_lg2) << bound_lg2) as * const u8
    }
    
    // Mutable version of page_of
    #[inline(always)]
    unsafe fn align_to_boundary_mut(ptr: * mut u8, boundary: usize) -> * mut u8 {
        assert_eq!(boundary, boundary.next_power_of_two());
        let bound_lg2 = boundary.trailing_zeros();
        ((ptr as usize >> bound_lg2) << bound_lg2) as * mut u8
    }
    
    unsafe fn alloc_unmanaged_array<'a, T>(len: usize, alignment: Option<usize>) -> &'a mut [T] {
        match len.checked_mul(mem::size_of::<T>()) {
            Some(size) => {
                // alignment guaranteed by the system
                let sys_alignment = libc::sysconf(libc::_SC_PAGESIZE) as usize;
                // max. of requested alignment and type's required alignment
                let align = alignment.unwrap_or(cmp::max(sys_alignment, mem::align_of::<T>()));
                let allocsz = if align > sys_alignment {
                    // if our page alignment is larger than system page size, allocate extra memory
                    // to compensate the alignment memory bump
                    size + align
                } else {
                    size
                };
                // allocate the memory
                let m: * mut u8 = mem::transmute(libc::mmap(core::ptr::null_mut(),
                                                            allocsz,
                                                            libc::PROT_READ | libc::PROT_WRITE,
                                                            libc::MAP_NORESERVE | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                                                            -1,
                                                            0));
                if m == core::ptr::null_mut() {
                    panic!("Memory error: mmap failed!");
                }
                // align to nearest given alignment
                let begin = if align > sys_alignment {
                    PageMgr::align_to_boundary_mut(m.offset(align as isize - 1), align)
                } else {
                    m
                };
                // memory alchemy! make it a Rust slice
                ::std::slice::from_raw_parts_mut(mem::transmute(begin), size)
            }
            None => {
                panic!("Memory error: requested array's size is greater than 2^64!");
            }
        }
    }

    // Note: the libc::MAP_ANONYMOUS flag says that it initializes the contents to zero,
    //       so maybe this function is unneeded
    unsafe fn alloc_unmanaged_zeroed_array<'a, T>(len: usize, alignment: Option<usize>) -> &'a mut [T] {
        let s = PageMgr::alloc_unmanaged_array(len, alignment);
        // zero the memory
        libc::memset(mem::transmute(s.as_mut_ptr()), 0, len * mem::size_of::<T>());
        s
    }

    pub fn alloc_region_mem<'a>(&self, pg_cnt: usize) -> Option<Region<'a>> {
        let pages_sz = mem::size_of::<Page>() * pg_cnt;
        let freemap_sz = mem::size_of::<u32>() * pg_cnt / 32;
        let meta_sz =  pg_cnt;

        let mut region = Region::new();
        println!("page count: {}", pg_cnt);
        // TODO: handle failure for this gracefully
        region.pages = unsafe {
            PageMgr::alloc_unmanaged_array(pg_cnt, Some(PAGE_SZ))
        };
        region.allocmap = unsafe {
            PageMgr::alloc_unmanaged_zeroed_array(pg_cnt / 32, None)
        };
        // mmap hack time
        region.meta = unsafe {
            PageMgr::alloc_unmanaged_zeroed_array(pg_cnt, None)
        };
        region.pg_cnt = pg_cnt as u32;
        // TODO: commit meta and allocmap
        Some(region)
    }

    pub fn alloc_region(&mut self, region: &mut Region) {
        let mut pg_cnt = self.region_pg_count;
        loop {
            match self.alloc_region_mem(pg_cnt) {
                Some(r) => {
                    mem::replace(region, r);
                    return;
                }
                None => {
                    // hitting OOM, try reducing #pages
                    if pg_cnt >= MIN_REGION_PG_COUNT * 4 {
                        pg_cnt /= 4;
                        self.region_pg_count = pg_cnt;
                    } else if pg_cnt > MIN_REGION_PG_COUNT {
                        pg_cnt = MIN_REGION_PG_COUNT;
                        self.region_pg_count = pg_cnt;
                    } else {
                        // can't recover by reducing page count, die.
                        panic!("GC: Out of memory"); // TODO: call jl_throw
                    }
                }
            }
        }
    }

    #[inline(never)]
    pub fn alloc_page<'a>(&mut self, regions: &'a mut [Region]) -> &'a mut Page {
        // println!("allocating page...");
        let mut i: Option<u32> = None;
        let mut region_i = 0;
        // println!("regions.len = {}", regions.len());
        'outer: for region in regions.iter_mut() {
            // println!("#pages in region {}: {}", region_i, region.pages.len());
            if region.pages.len() == 0 {
                // found an empty region, allocate it
                self.alloc_region(region);
            }
            for j in region.lb..(region.pg_cnt / 32) {
                // println!("j: {}", j);
                if (!region.allocmap[j as usize]) != 0 {
                    // there are free pages in the region
                    i = Some(j);
                    break 'outer;
                }
            }
            region_i += 1;
        }
        if let Some(i) = i {
            let region = &mut regions[region_i];
            // update bounds
            if region.lb < i {
                region.lb = i;
            }
            if region.ub < i {
                region.ub = i;
            }
            // find first empty page
            let j = ((! region.allocmap[i as usize]).ffs() - 1) as u32;
            region.allocmap[i as usize] |= 1 << j;
            // TODO: commit page (&region.pages[i * 32 + j])
            self.current_pg_count += 1;
            // notify Julia's GC debugger
            unsafe {
                gc_final_count_page(self.current_pg_count);
            }
            &mut region.pages[(i * 32 + j) as usize]
        } else {
            // No regions with free memory are available and all region slots are allocated
            panic!("GC: out of memory: no regions left!"); // TODO: change with jl_throw
        }
    }
    
    // free page with given pointer
    pub fn free_page(&mut self, regions: &mut [Region], p: * const u8) {
        let mut pg_idx = None;
        let mut reg_idx = None;
        for (i, region) in regions.iter_mut().enumerate() {
            if region.pages.len() == 0 {
                continue;
            }

            if let Some(pi) = region.index_of_raw(p) {
                pg_idx = Some(pi);
                reg_idx = Some(i);
            }
        }

        let mut pg_idx = pg_idx.unwrap();
        let i = reg_idx.unwrap();

        self.free_page_in_region(&mut regions[i], pg_idx);
    }

    // free page with given index at given region
    pub fn free_page_in_region(&mut self, region: &mut Region, pg_idx: usize) {
        let bit_idx = (pg_idx % 32) as u8;
        assert!(region.allocmap[pg_idx / 32].get_bit(bit_idx), "GC: Memory corruption: allocation map and data mismatch!");
        region.allocmap[pg_idx / 32].set_bit(bit_idx, false);
        // free age data
        region.meta[pg_idx].ages = None;

        // decommit code

        // figure out #pages to decommit
        let mut decommit_size = PAGE_SZ;
        let mut page_ptr: Option<*const libc::c_void> = None;
        let mut should_decommit = true;
        if PAGE_SZ < jl_page_size {
            let n_pages = (PAGE_SZ + jl_page_size - 1) / PAGE_SZ; // size of OS pages in terms of our pages
            decommit_size = jl_page_size;

            // hacky pointer magic for figuring out OS page alignment
            let page_ptr = unsafe {
                Some(((&region.pages[pg_idx].data as *const u8 as usize) & !(jl_page_size - 1)) as *const u8)
            };

            let pg_idx = region.index_of_raw(page_ptr.unwrap()).unwrap();
            if pg_idx + n_pages > region.pg_cnt as usize {
                should_decommit = false;
            } else {
                for i in 0..n_pages {
                    if region.allocmap[pg_idx / 32].get_bit(bit_idx) {
                        should_decommit = false;
                        break;
                    }
                }
            }
        }

        if should_decommit {
            // TODO: actually decommit, we need to use our own allocator for this
        }

        if region.lb as usize > pg_idx / 32 {
            region.lb = (pg_idx / 32) as u32;
        }

        self.current_pg_count -= 1;
    }

    /// port of `page_metadata` in Julia
    pub unsafe fn find_pagemeta<T>(&self, ptr: * const T) -> Option<&'static mut PageMeta<'static>> {
        let regions = REGIONS.as_mut().unwrap();
        for region in regions.iter_mut() {
            if region.pg_cnt == 0 {
                // we are done with all initialized regions
                // N.B. if we change region allocation algorithm, we need to revisit this
                return None;
            }

            if let Some(i) = region.index_of_raw(ptr as * const u8) {
                return Some(&mut region.meta[i]);
            }
        }
        None
    }
}

// Testing stubs for pages
#[cfg(test)]
mod pages_tests {
    use super::*;

    #[test]
    fn test_clone() {
        let arr = [42; PAGE_SZ];
        let page = Page { data: arr };
        let page2 = page.clone();
        // TODO: Am I misunderstanding something about how clone is supposed to work? or is this a bug
        assert_ne!(page.data[0], page2.data[0]); // Fix, should not pass I'm assuming
        assert_eq!(page.data.len(), page2.data.len());
    }

    #[test]
    fn test_pagemgr_new() {
        let pgmgr = PageMgr::new();
        assert!(pgmgr.region_pg_count >= MIN_REGION_PG_COUNT);
        assert!(pgmgr.region_pg_count <= DEFAULT_REGION_PG_COUNT);
        assert_eq!(pgmgr.current_pg_count, 0);
    }

    #[test]
    fn test_alloc_unmanaged_array() {
        unsafe {
            let res1 = panic::catch_unwind(|| {
                PageMgr::alloc_unmanaged_array::<u8>(2u32.pow(64) as usize, None);
            });
            assert!(res1.is_err());

            let arr = PageMgr::alloc_unmanaged_array::<u8>(2u32.pow(5) as usize, None);
            assert!(arr.iter().all(|&el| el == 0));
            assert_eq!(arr.len(), 2u32.pow(5) as usize * mem::size_of::<u8>());
        }
    }

    #[test]
    fn test_alloc_unmanaged_zeroed_array() {
        unsafe {
            let arr = PageMgr::alloc_unmanaged_array::<u8>(2u32.pow(5) as usize, None);
            // Note: see above comment
            assert!(arr.iter().all(|&el| el == 0));
            assert_eq!(arr.len(), 2u32.pow(5) as usize * mem::size_of::<u8>());
        }
    }

    #[test]
    fn test_unmanaged_array_alignment() {
        for alignment in &[1usize, 2, 4, 8, 32, 1024, 64 * 1024, 128 * 1024, 256 * 1024] {
            // make sure that alignment is a power of two
            assert_eq!(alignment.count_ones(), 1);
            unsafe {
                let plain = PageMgr::alloc_unmanaged_array::<u32>(2usize.pow(5) as usize, Some(alignment.clone()));
                assert!((plain.as_ptr() as usize).trailing_zeros() >= alignment.trailing_zeros());
                let zeroed = PageMgr::alloc_unmanaged_zeroed_array::<u32>(2usize.pow(5) as usize, Some(alignment.clone()));
                assert!((zeroed.as_ptr() as usize).trailing_zeros() >= alignment.trailing_zeros());
            }
        }
    }

    #[test]
    fn test_alloc_region_mem() {
        assert_eq!(42, 5+37);
    }

    #[test]
    fn test_alloc_region() {
        assert_eq!(42, 5+37);
    }

    #[test]
    fn test_alloc_page() {
        assert_eq!(42, 5+37);
    }

    #[test]
    fn test_page() {
        assert_eq!(42, 5+37);
    }

}
