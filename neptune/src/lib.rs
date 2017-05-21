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

extern crate libc;
extern crate bit_field;
extern crate core;
extern crate alloc;
extern crate threadpool;

//#[cfg(test)]
//mod tests;

mod gc;
pub mod pages;
pub mod util;

#[macro_use]
pub mod c_interface;

pub mod gc2;
