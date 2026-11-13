//! One admitted original network-UTC owner mapped once into local model time.
//! The caller owns freshness/permission/epoch admission; retries never remap it.
#[derive(Debug)]
pub(super) struct Owner {
    pub source_utc: u64,
    pub start_model_ms: u64,
    pub stop_model_ms: u64,
    completed: bool,
}
impl Owner {
    pub fn admitted(source: u64, observed_utc: u64, receipt_model_ms: u64) -> Option<Self> {
        if source == 0 || source == u64::MAX || observed_utc == 0 || observed_utc == u64::MAX {
            return None;
        }
        let start_remaining = source.checked_add(4250)?.saturating_sub(observed_utc);
        let stop_remaining = source.checked_add(14250)?.saturating_sub(observed_utc);
        Some(Self {
            source_utc: source,
            completed: false,
            start_model_ms: receipt_model_ms.checked_add(start_remaining)?,
            stop_model_ms: receipt_model_ms.checked_add(stop_remaining)?,
        })
    }
    pub fn on_at(&mut self, model_ms: u64) -> bool {
        if self.completed {
            return false;
        }
        if model_ms >= self.stop_model_ms {
            self.completed = true;
            return false;
        }
        model_ms >= self.start_model_ms
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_network_wall_offset_is_mapped_once_not_subtracted_directly() {
        crate::mode_suite!(Testing);
        let mut o = Owner::admitted(10000, 12000, 1_793_000_000_000).unwrap();
        assert_eq!(o.source_utc, 10000);
        assert_eq!(
            (o.start_model_ms, o.stop_model_ms),
            (1_793_000_002_250, 1_793_000_012_250)
        );
        assert!(!o.on_at(o.start_model_ms - 1));
        assert!(o.on_at(o.start_model_ms));
        assert!(!o.on_at(o.stop_model_ms));
    }
    #[test]
    fn a_late_observation_cannot_restart_an_expired_pulse() {
        crate::mode_suite!(Testing);
        let mut o = Owner::admitted(10000, 25000, 80000).unwrap();
        assert_eq!(o.start_model_ms, o.stop_model_ms);
        assert!(!o.on_at(80000));
    }
    #[test]
    fn a_completed_owner_cannot_resurrect_after_local_clock_rollback() {
        crate::mode_suite!(Testing);
        let mut o = Owner::admitted(10000, 10000, 1000000).unwrap();
        let start = o.start_model_ms;
        let stop = o.stop_model_ms;
        assert!(o.on_at(start));
        assert!(!o.on_at(stop));
        for clock in [start, start + 1, stop - 1, stop + 1, start] {
            assert!(
                !o.on_at(clock),
                "completed pulse resurrected without new admission"
            );
        }
        assert_eq!(o.source_utc, 10000);
    }
}
