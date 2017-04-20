//! Crate attributes:
#![feature(drop_types_in_const)]

extern crate libc;
extern crate bit_field;
extern crate core;

#[cfg(test)]
mod tests;

mod gc;
mod pages;
mod util;
pub mod c_interface;
