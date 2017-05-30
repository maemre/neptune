use std::sync::atomic::*;
use crossbeam::sync::*;
use libc;

/// Concurrent, thread-safe stack implementation.  All accesses to
/// this data structure are blocking.  This data structure overrides
/// some of Rust's safety guarantees for sending raw pointers. Use it
/// at your own risk with raw pointers.
///
/// The internal structure is represented as a Treiber stack.
pub struct ConcurrentStack<T> {
    stack: TreiberStack<T>,
}

impl<T> ConcurrentStack<T> {
    pub fn new() -> Self {
        ConcurrentStack {
            stack: TreiberStack::new(),
        }
    }

    #[inline(always)]
    pub fn push(&self, value: T) {
        self.stack.push(value);
    }

    #[inline(always)]
    pub fn pop(&self) -> Option<T> {
        self.stack.try_pop()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// This is thread-unsafe so other threads are prevented accessing the
    /// stack during clear, which is guaranteed by `&mut self`.
    #[inline(always)]
    pub fn clear(&mut self) {
        self.stack = TreiberStack::new();
    }
}

// unsafe impl<T> Sync for ConcurrentStack<* mut T> {}
unsafe impl Sync for ConcurrentStack<* mut libc::c_void> {}
