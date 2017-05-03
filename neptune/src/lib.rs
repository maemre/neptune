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

//#[cfg(test)]
//mod tests;

mod gc;
mod gc2;
mod pages;
mod util;
pub mod c_interface;
