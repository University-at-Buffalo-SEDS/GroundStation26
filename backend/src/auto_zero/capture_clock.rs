//! Procedure capture time only; calibration receipts and authentication stay wall-based.
use std::time::{Duration, Instant};
#[derive(Clone, Copy)]
pub(super) struct CaptureClock {
    wall: Instant,
    model_ns: Option<u128>,
}
impl CaptureClock {
    pub(super) fn now() -> Self {
        Self {
            wall: Instant::now(),
            model_ns: crate::simulation_clock_ms().map(|ms| u128::from(ms) * 1_000_000),
        }
    }
    pub(super) fn duration_since(self, earlier: Self) -> Duration {
        match (self.model_ns, earlier.model_ns) {
            (Some(now), Some(before)) => {
                Duration::from_nanos(now.saturating_sub(before).min(u128::from(u64::MAX)) as u64)
            }
            _ => self.wall.duration_since(earlier.wall),
        }
    }
    pub(super) fn at_or_after(self, earlier: Self) -> bool {
        match (self.model_ns, earlier.model_ns) {
            (Some(now), Some(before)) => now >= before,
            _ => self.wall >= earlier.wall,
        }
    }
    pub(super) fn elapsed(self) -> Duration {
        Self::now().duration_since(self)
    }
    pub(super) fn earlier(self, duration: Duration) -> Self {
        Self {
            wall: self.wall - duration,
            model_ns: self
                .model_ns
                .map(|ns| ns.saturating_sub(duration.as_nanos())),
        }
    }
    #[cfg(test)]
    pub(super) fn later(self, duration: Duration) -> Self {
        Self {
            wall: self.wall + duration,
            model_ns: self
                .model_ns
                .map(|ns| ns.saturating_add(duration.as_nanos())),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn stamp(wall: Instant, ms: u64) -> CaptureClock {
        CaptureClock {
            wall,
            model_ns: Some(u128::from(ms) * 1_000_000),
        }
    }
    #[test]
    fn slow_model_uses_the_same_capture_window_without_wall_expiry() {
        let wall = Instant::now();
        let start = stamp(wall, 10000);
        let slow = stamp(wall + Duration::from_secs(60), 14000);
        assert!(
            slow.wall.duration_since(start.wall) >= Duration::from_secs(5),
            "old Instant capture expires incorrectly"
        );
        assert_eq!(slow.duration_since(start), Duration::from_secs(4));
        assert!(slow.duration_since(start) < Duration::from_secs(5));
        assert_eq!(
            stamp(wall + Duration::from_secs(90), 14000).duration_since(start),
            Duration::from_secs(4)
        );
        assert!(
            stamp(wall + Duration::from_secs(100), 15000).duration_since(start)
                >= Duration::from_secs(5)
        );
    }
    #[test]
    fn fresh_sample_and_capture_bounds_follow_paused_model_receipt() {
        let wall = Instant::now();
        let sample = stamp(wall, 10000);
        let paused = stamp(wall + Duration::from_secs(30), 10000);
        assert!(paused.duration_since(sample) < Duration::from_millis(250));
        assert!(
            !stamp(wall + Duration::from_secs(30), 10250)
                .duration_since(sample)
                .lt(&Duration::from_millis(250))
        );
        assert!(paused.at_or_after(sample));
        assert!(!sample.at_or_after(sample.later(Duration::from_nanos(1))));
        assert!(sample.at_or_after(paused.earlier(Duration::from_secs(5))));
    }
    #[test]
    fn ordinary_no_file_preserves_exact_instant_duration_and_order() {
        let wall = Instant::now();
        let first = CaptureClock {
            wall,
            model_ns: None,
        };
        let later = CaptureClock {
            wall: wall + Duration::from_millis(123),
            model_ns: None,
        };
        assert_eq!(
            later.duration_since(first),
            later.wall.duration_since(first.wall)
        );
        assert!(later.at_or_after(first));
        assert!(!first.at_or_after(later));
        assert_eq!(later.earlier(Duration::from_millis(123)).wall, wall);
    }
}
