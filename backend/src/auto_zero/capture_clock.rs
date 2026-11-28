//! Procedure capture time only; calibration receipts and authentication stay wall-based.
use std::time::{Duration, Instant};
#[derive(Clone, Copy)]
pub(super) struct CaptureClock {
    wall: Instant,
    model_ns: Option<u128>,
    valid: bool,
}
impl CaptureClock {
    pub(super) fn now() -> Self {
        let sampled = crate::simulation_clock_ms();
        // The shared SDK clock intentionally clamps/reset-caches its floor.
        // A capture cannot turn that uncertainty into a renewed valid budget.
        // This reads the explicit model file: never call it RAM-only.
        let (model_ns, valid) = match sampled {
            None => (None, true),
            Some(floor) => {
                let raw = crate::SIMULATION_CLOCK_PATH
                    .get()
                    .and_then(|path| path.as_ref())
                    .and_then(|path| std::fs::read_to_string(path).ok());
                observed_model(
                    floor,
                    raw.as_deref(),
                    crate::cached_simulation_clock_sample().is_some(),
                )
            }
        };
        Self {
            wall: Instant::now(),
            model_ns,
            valid,
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
    pub(super) fn is_valid(self) -> bool {
        self.valid
    }
    pub(super) fn invalidate(&mut self) {
        self.valid = false;
    }
    pub(super) fn same_domain(self, other: Self) -> bool {
        self.model_ns.is_some() == other.model_ns.is_some()
    }
    fn comparable(self, other: Self) -> bool {
        self.valid && other.valid && self.same_domain(other)
    }
    pub(super) fn at_or_after(self, earlier: Self) -> bool {
        if !self.comparable(earlier) {
            return false;
        }
        match (self.model_ns, earlier.model_ns) {
            (Some(now), Some(before)) => now >= before,
            _ => self.wall >= earlier.wall,
        }
    }
    pub(super) fn elapsed(self) -> Duration {
        Self::now().duration_since(self)
    }
    /// The ORIGINAL capture domain must remain comparable and monotonic.
    /// Model clock reset/loss is uncertainty, never a fresh acquisition budget.
    pub(super) fn within(self, window: Duration) -> bool {
        self.within_at(Self::now(), window)
    }
    fn within_at(self, now: Self, window: Duration) -> bool {
        now.at_or_after(self) && now.duration_since(self) < window
    }
    pub(super) fn earlier(self, duration: Duration) -> Self {
        Self {
            wall: self.wall - duration,
            model_ns: self
                .model_ns
                .map(|ns| ns.saturating_sub(duration.as_nanos())),
            valid: self.valid,
        }
    }
    #[cfg(test)]
    pub(super) fn later(self, duration: Duration) -> Self {
        Self {
            wall: self.wall + duration,
            model_ns: self
                .model_ns
                .map(|ns| ns.saturating_add(duration.as_nanos())),
            valid: self.valid,
        }
    }
}
fn observed_model(floor: u64, raw: Option<&str>, shared_valid: bool) -> (Option<u128>, bool) {
    let raw = raw
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|raw| *raw != u64::MAX);
    let valid = shared_valid && raw.is_some_and(|raw| raw >= floor);
    (Some(u128::from(raw.unwrap_or(floor)) * 1_000_000), valid)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stamp(wall: Instant, ms: u64) -> CaptureClock {
        CaptureClock {
            wall,
            model_ns: Some(u128::from(ms) * 1_000_000),
            valid: true,
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
            valid: true,
        };
        let later = CaptureClock {
            wall: wall + Duration::from_millis(123),
            model_ns: None,
            valid: true,
        };
        assert_eq!(
            later.duration_since(first),
            later.wall.duration_since(first.wall)
        );
        assert!(later.at_or_after(first));
        assert!(!first.at_or_after(later));
        assert_eq!(later.earlier(Duration::from_millis(123)).wall, wall);
    }
    #[test]
    fn original_seven_second_boundary_and_clock_domain_are_never_renewed() {
        let wall = Instant::now();
        let start = stamp(wall, 10000);
        let window = Duration::from_secs(7);
        assert!(start.within_at(stamp(wall, 16999), window));
        assert!(!start.within_at(stamp(wall, 17000), window));
        assert!(!start.within_at(stamp(wall, 17001), window));
        assert!(!start.within_at(stamp(wall, 9999), window));
        let unknown = CaptureClock {
            valid: false,
            ..stamp(wall, 11000)
        };
        assert!(!start.within_at(unknown, window));
        assert!(!unknown.within_at(stamp(wall, 11000), window));
        assert!(!unknown.earlier(Duration::from_secs(1)).valid);
        assert!(!unknown.later(Duration::from_secs(1)).valid);
        let wall_only = CaptureClock {
            wall,
            model_ns: None,
            valid: true,
        };
        assert!(!start.within_at(wall_only, window));
        assert!(!wall_only.within_at(start, window));
    }
    #[test]
    fn malformed_truncated_missing_reset_and_out_of_range_model_samples_stay_invalid() {
        for raw in [
            None,
            Some(""),
            Some(" "),
            Some("bad"),
            Some("-1"),
            Some("9999"),
            Some("18446744073709551615"),
            Some("18446744073709551616"),
        ] {
            let (domain, valid) = observed_model(10000, raw, true);
            assert!(
                domain.is_some(),
                "invalid model must not become wall authority"
            );
            assert!(!valid, "{raw:?}");
        }
        assert!(observed_model(10000, Some("10000\n"), true).1);
        assert!(!observed_model(10000, Some("10001"), false).1);
        assert!(
            observed_model(0, Some("0"), true).1,
            "model boot tick zero is valid"
        );
    }

    #[test]
    fn invalid_capture_receipts_remain_diagnostic_but_never_reappear_as_eligible() {
        let wall = Instant::now();
        let mut runtime = crate::auto_zero::Runtime::testing(true);
        let original = stamp(wall, 10000);
        for id in 1..=200 {
            runtime.observe_captured("KG1000", 0.1, Some(id), stamp(wall, 10000 + id * 25));
        }
        assert_eq!(runtime.samples("KG1000", original).len(), 200);
        runtime.pending = true;
        let generation = runtime.generation;
        let bad = CaptureClock {
            valid: false,
            ..stamp(wall, 15001)
        };
        runtime.observe_captured("KG1000", 0.9, Some(999), bad);
        assert!(!runtime.pending && runtime.generation != generation);
        assert_eq!(
            runtime.kg1000.len(),
            200,
            "raw capture diagnostics retained, not promoted"
        );
        assert!(runtime.samples("KG1000", original).is_empty());
        runtime.observe_captured("KG1000", 0.9, Some(999), stamp(wall, 15002));
        assert!(
            runtime.samples("KG1000", original).is_empty(),
            "same invalid owner never becomes valid on clock recovery"
        );
        let recovered = stamp(wall, 15002);
        for id in 1..=200 {
            runtime.observe_captured("KG1000", 0.2, Some(2000 + id), stamp(wall, 15002 + id * 25));
        }
        assert_eq!(runtime.samples("KG1000", recovered), vec![0.2; 200]);
        assert!(
            !original.within_at(stamp(wall, 20002), Duration::from_secs(7)),
            "recovering data does not renew the old request"
        );
    }
    #[test]
    fn a_new_capture_domain_cannot_reuse_old_domain_receipts_or_future_freshness() {
        let wall = Instant::now();
        let mut runtime = crate::auto_zero::Runtime::testing(true);
        let ordinary = CaptureClock {
            wall,
            model_ns: None,
            valid: true,
        };
        for id in 1..=200 {
            runtime.observe_captured("KG1000", 0.1, Some(id), ordinary);
        }
        let model = stamp(wall, 10000);
        for id in 1..=200 {
            runtime.observe_captured("KG1000", 0.2, Some(1000 + id), stamp(wall, 10000 + id * 25));
        }
        assert!(runtime.samples("KG1000", ordinary).is_empty());
        assert_eq!(runtime.samples("KG1000", model), vec![0.2; 200]);
        assert!(
            !model.at_or_after(stamp(wall, 10001)),
            "future receipt is not a fresh observation"
        );
    }
}
