use std::sync::*;
use std::slice;

/// Concurrent, thread-safe stack implementation.
/// All accesses to this data structure are blocking.
pub struct ConcurrentStack<T> {
    vec: Arc<Mutex<Vec<T>>>,
}

impl<T> ConcurrentStack<T> {
    pub fn new() -> Self {
        ConcurrentStack {
            vec: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn push(&self, value: T) {
        self.vec.lock().unwrap().push(value);
    }

    pub fn pop(&self) -> Option<T> {
        self.vec.lock().unwrap().pop()
    }

    pub fn is_empty(&self) -> bool {
        self.vec.lock().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.vec.lock().unwrap().len()
    }

    pub fn truncate(&self, size: usize) {
        self.vec.lock().unwrap().truncate(size);
    }
/*
    pub fn iter_mut(&mut self) -> slice::IterMut<T> {
        self.vec.lock().unwrap().iter_mut()
    }

    pub fn iter(&self) -> slice::Iter<T> {
        self.vec.lock().unwrap().iter()
    }
*/
    #[inline(always)]
    pub fn clear(&self) {
        self.truncate(0);
    }
}
