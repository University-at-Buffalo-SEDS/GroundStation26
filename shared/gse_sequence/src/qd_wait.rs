//! QD timing uses a monotonic clock and a fresh positive command confirmation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QdStatus {
    #[default]
    NotStarted,
    AwaitingAck,
    Settling {
        remaining_ms: u64,
    },
    Ready,
    AckTimedOut,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct QdWait {
    sent_ms: Option<u64>,
    ack_ms: Option<u64>,
    queued: bool,
    timed_out: bool,
}
impl QdWait {
    pub const SETTLE_MS: u64 = 5_000;
    pub const ACK_TIMEOUT_MS: u64 = 5_000;
    pub fn dispatched(&mut self, now_ms: u64) {
        *self = Self {
            sent_ms: Some(now_ms),
            ..Self::default()
        };
    }
    pub fn confirm(&mut self, received_ms: u64) -> bool {
        let Some(sent) = self.sent_ms else {
            return false;
        };
        if self.ack_ms.is_some() || self.timed_out || received_ms < sent {
            return false;
        }
        if received_ms - sent >= Self::ACK_TIMEOUT_MS {
            self.timed_out = true;
            self.queued = false;
            return false;
        }
        self.ack_ms = Some(received_ms);
        true
    }
    pub fn status(&self, now_ms: u64) -> QdStatus {
        if self.timed_out {
            return QdStatus::AckTimedOut;
        }
        match (self.sent_ms, self.ack_ms) {
            (_, Some(ack)) => {
                let elapsed = now_ms.saturating_sub(ack);
                if elapsed < Self::SETTLE_MS {
                    QdStatus::Settling {
                        remaining_ms: Self::SETTLE_MS - elapsed,
                    }
                } else {
                    QdStatus::Ready
                }
            }
            (Some(sent), _) if now_ms.saturating_sub(sent) >= Self::ACK_TIMEOUT_MS => {
                QdStatus::AckTimedOut
            }
            (Some(_), _) => QdStatus::AwaitingAck,
            _ => QdStatus::NotStarted,
        }
    }
    pub fn queue_launch(&mut self, now_ms: u64) -> Result<(), &'static str> {
        match self.status(now_ms) {
            QdStatus::NotStarted => Err("Retract QD before requesting launch"),
            QdStatus::AckTimedOut => Err("QD command acknowledgement timed out"),
            _ => {
                self.queued = true;
                Ok(())
            }
        }
    }
    pub fn queued(&self) -> bool {
        self.queued
    }
    /// Consumes exactly one queued request. The caller must recheck every live
    /// launch prerequisite before dispatching; this timer grants no authority.
    pub fn take_ready_launch(&mut self, now_ms: u64) -> bool {
        match self.status(now_ms) {
            QdStatus::AckTimedOut => {
                self.queued = false;
                self.timed_out = true;
                false
            }
            QdStatus::Ready if self.queued => {
                self.queued = false;
                true
            }
            _ => false,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn five_seconds_begin_at_ack_and_duplicate_reports_do_not_restart_time() {
        let mut q = QdWait::default();
        q.dispatched(100);
        q.queue_launch(101).unwrap();
        assert!(q.confirm(1100));
        assert!(!q.confirm(2000));
        assert_eq!(q.status(6099), QdStatus::Settling { remaining_ms: 1 });
        assert!(!q.take_ready_launch(6099));
        assert!(q.take_ready_launch(6100));
        assert!(!q.take_ready_launch(6101));
    }
    #[test]
    fn missing_late_or_predispatch_ack_cannot_release_launch() {
        let mut q = QdWait::default();
        assert!(q.queue_launch(0).is_err());
        q.dispatched(100);
        q.queue_launch(101).unwrap();
        assert!(!q.confirm(99));
        assert!(!q.take_ready_launch(5100));
        assert!(!q.queued());
        assert!(!q.confirm(5101));
        assert!(!q.take_ready_launch(20000));
    }
    #[test]
    fn request_during_settling_is_queued_once_and_restart_never_replays_it() {
        let mut q = QdWait::default();
        q.dispatched(0);
        q.confirm(20);
        for _ in 0..10 {
            q.queue_launch(100).unwrap();
        }
        assert!(q.queued());
        assert!(q.take_ready_launch(5020));
        assert!(!q.take_ready_launch(5020));
        let mut restarted = QdWait::default();
        assert!(!restarted.take_ready_launch(100000));
        assert_eq!(restarted.status(100000), QdStatus::NotStarted);
    }
}
