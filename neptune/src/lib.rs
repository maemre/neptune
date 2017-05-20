//! Crate attributes:
// for having options as statics:
#![feature(drop_types_in_const)]
// for having rust allocator:
#![feature(alloc)]
#![feature(heap_api)]
// for having likely/unlikely intrinsics:
#![feature(core_intrinsics)]

extern crate libc;
extern crate bit_field;
extern crate core;
extern crate alloc;
extern crate threadpool;

//#[cfg(test)]
//mod tests;

mod gc;
mod pages;
mod util;

#[macro_use]
pub mod c_interface;

mod gc2;
