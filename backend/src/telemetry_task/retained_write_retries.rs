//! Fixed local ownership for failed at-most-three-frame physical batches.
//! Moves existing Vec owners; no payload cloning or new accepted-byte budget.
#[derive(Debug)]
pub(super) struct Frame {
    pub(super) id: u64,
    pub(super) priority: u8,
    pub(super) bytes: Vec<u8>,
}
pub(super) struct Retained {
    slots: [Option<Frame>; 3],
    next_id: u64,
    saturated: bool,
}
impl Default for Retained {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            next_id: 0,
            saturated: false,
        }
    }
}
impl Retained {
    pub(super) fn blocked(&self) -> bool {
        self.saturated || self.slots.iter().any(Option::is_some)
    }
    pub(super) fn has_owner(&self) -> bool {
        self.slots.iter().any(Option::is_some)
    }
    pub(super) fn hold_new(&mut self, priority: u8, bytes: Vec<u8>) -> Result<(), (u8, Vec<u8>)> {
        let Some(index) = self.slots.iter().position(Option::is_none) else {
            self.saturated = true;
            return Err((priority, bytes));
        };
        let Some(id) = self.next_id.checked_add(1) else {
            self.saturated = true;
            return Err((priority, bytes));
        };
        self.next_id = id;
        self.slots[index] = Some(Frame {
            id,
            priority,
            bytes,
        });
        Ok(())
    }
    pub(super) fn take(&mut self) -> Option<Frame> {
        let frame = self.slots[0].take()?;
        for i in 1..3 {
            self.slots[i - 1] = self.slots[i].take();
        }
        Some(frame)
    }
    pub(super) fn restore(&mut self, frame: Frame) {
        assert!(self.slots[2].is_none());
        for i in (1..3).rev() {
            self.slots[i] = self.slots[i - 1].take();
        }
        self.slots[0] = Some(frame);
    }
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&Frame) -> bool) {
        let mut index = 0;
        while index < 3 {
            if self.slots[index].as_ref().is_some_and(|frame| !keep(frame)) {
                self.slots[index] = None;
                for next in index + 1..3 {
                    self.slots[next - 1] = self.slots[next].take();
                }
            } else {
                index += 1;
            }
        }
    }
    // Saturation never guesses an owner from bytes, addresses or elapsed time.
    // Only complete drain of every charged accepted frame can release it.
    pub(super) fn observe_complete_drain(&mut self, fully_drained: bool) {
        if fully_drained && !self.has_owner() {
            self.saturated = false;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_owned_retry_survives_unrelated_success_without_copying_payload() {
        let mut r = Retained::default();
        let bytes = vec![1; 64];
        let ptr = bytes.as_ptr();
        r.hold_new(0, bytes).unwrap();
        assert!(r.blocked());
        let first = r.take().unwrap();
        assert_eq!(first.bytes.as_ptr(), ptr);
        let id = first.id;
        r.restore(first);
        r.observe_complete_drain(false);
        assert!(r.blocked(), "unrelated success cannot release failed owner");
        let retry = r.take().unwrap();
        assert_eq!(retry.id, id);
        assert_eq!(retry.bytes.as_ptr(), ptr);
        drop(retry);
        assert!(
            !r.blocked(),
            "only moving exact owner through full success releases it"
        );
    }
    #[test]
    fn saturation_and_id_wrap_are_conservative_until_complete_drain() {
        let mut r = Retained::default();
        for n in 0..3 {
            r.hold_new(0, vec![n]).unwrap();
        }
        assert!(r.hold_new(0, vec![9]).is_err());
        while r.take().is_some() {}
        assert!(r.blocked());
        r.observe_complete_drain(false);
        assert!(r.blocked());
        r.observe_complete_drain(true);
        assert!(!r.blocked());
        r.next_id = u64::MAX;
        assert!(r.hold_new(0, vec![10]).is_err());
        assert!(r.blocked());
    }
}
