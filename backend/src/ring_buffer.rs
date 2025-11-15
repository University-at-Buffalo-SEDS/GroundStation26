use std::collections::VecDeque;

pub struct RingBuffer<T> {
    max: usize,
    buf: VecDeque<T>,
}

impl<T> RingBuffer<T> {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            buf: VecDeque::with_capacity(max),
        }
    }

    pub fn push(&mut self, item: T) {
        if self.buf.len() == self.max {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    
    pub fn recent(&self, n: usize) -> Vec<&T> {
        self.buf.iter().rev().take(n).collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }
}
