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
    len: AtomicUsize,
}

impl<T> ConcurrentStack<T> {
    pub fn new() -> Self {
        ConcurrentStack {
            stack: TreiberStack::new(),
            len: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, value: T) {
        self.stack.push(value);
    }

    pub fn pop(&self) -> Option<T> {
        self.stack.try_pop()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::SeqCst)
    }

    /// This is thread-unsafe so other threads are prevented accessing the
    /// stack during clear, which is guaranteed by `&mut self`.
    #[inline(always)]
    pub fn clear(&mut self) {
        self.len.store(0, Ordering::SeqCst);
        self.stack = TreiberStack::new();
    }
}

// unsafe impl<T> Sync for ConcurrentStack<* mut T> {}
unsafe impl Sync for ConcurrentStack<* mut libc::c_void> {}
