//! FIFO ownership for the two reusable asynchronous capture slots.
use std::collections::VecDeque;

#[derive(Default)]
pub(crate) struct ReadbackQueue {
    pending: VecDeque<usize>,
}

impl ReadbackQueue {
    const CAPACITY: usize = 2;

    pub(crate) fn next_free(&self) -> Option<usize> {
        (0..Self::CAPACITY).find(|index| !self.pending.contains(index))
    }
    pub(crate) fn submitted(&mut self, index: usize) {
        assert!(index < Self::CAPACITY && !self.pending.contains(&index));
        self.pending.push_back(index);
    }
    pub(crate) fn front(&self) -> Option<usize> {
        self.pending.front().copied()
    }
    pub(crate) fn completed(&mut self) -> Option<usize> {
        self.pending.pop_front()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_slots_are_never_reused_and_completed_frames_keep_order() {
        let mut queue = ReadbackQueue::default();
        assert_eq!(queue.next_free(), Some(0));
        queue.submitted(0);
        assert_eq!(queue.next_free(), Some(1));
        queue.submitted(1);
        assert_eq!(queue.next_free(), None);
        assert_eq!(queue.front(), Some(0));
        assert_eq!(queue.completed(), Some(0));
        assert_eq!(queue.next_free(), Some(0));
        queue.submitted(0);
        assert_eq!(queue.completed(), Some(1));
        assert_eq!(queue.completed(), Some(0));
        assert!(queue.is_empty());
    }
}
