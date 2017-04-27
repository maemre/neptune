//! Crate attributes:
#![feature(drop_types_in_const)]
#![feature(alloc)]
#![feature(heap_api)]

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
