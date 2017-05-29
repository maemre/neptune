//! Crate attributes:
// for having options as statics:
#![feature(drop_types_in_const)]
// for having rust allocator:
#![feature(alloc)]
#![feature(heap_api)]
// for having likely/unlikely intrinsics:
#![feature(core_intrinsics)]
// for having atomic u16, u32 etc.
#![feature(integer_atomics)]
// for computing offsets between objects on a page
#![feature(offset_to)]
// to support const fns in globals and to enable const fn optimizations:
#![feature(const_fn)]
// to iterate over ranges with steps different than 1
#![feature(step_by)]

extern crate libc;
extern crate core;
extern crate alloc;
extern crate scoped_threadpool;
extern crate crossbeam;

//#[cfg(test)]
//mod tests;

mod concurrency;
mod gc;
pub mod pages;
pub mod util;

#[macro_use]
pub mod c_interface;

pub mod gc2;
