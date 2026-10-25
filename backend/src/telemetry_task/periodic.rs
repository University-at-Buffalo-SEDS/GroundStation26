//! Protocol producers follow the shared simulator clock without catch-up bursts.
//! Ordinary deployments keep their existing wall-clock timer cadence and UTC.
use std::sync::OnceLock;

pub(super) struct ProtocolCadence {
    period_ms: u64,
    next_virtual_ms: Option<u64>,
}
impl ProtocolCadence {
    pub(super) fn new(period_ms: u64) -> Self {
        Self {
            period_ms,
            next_virtual_ms: None,
        }
    }
    pub(super) fn due(&mut self, virtual_ms: Option<u64>) -> bool {
        let Some(now) = virtual_ms else {
            return true;
        };
        if self.next_virtual_ms.is_some_and(|next| now < next) {
            return false;
        }
        self.next_virtual_ms = Some(now.saturating_add(self.period_ms));
        true
    }
}

pub(super) fn source_utc_ms(
    wall_ms: u64,
    virtual_ms: Option<u64>,
    configured_epoch_ms: Option<u64>,
    epoch: &OnceLock<u64>,
) -> u64 {
    let Some(now) = virtual_ms else {
        return wall_ms;
    };
    if let Some(configured_epoch) = configured_epoch_ms {
        assert!(
            (315_532_800_000..4_354_819_200_000).contains(&configured_epoch),
            "simulation UTC epoch is outside the supported calendar range"
        );
        return configured_epoch.saturating_add(now);
    }
    // Anchor one UTC epoch when the source first starts. Subsequent simulator
    // pauses/slow execution must not move network UTC faster than MCU time.
    epoch
        .get_or_init(|| wall_ms.saturating_sub(now))
        .saturating_add(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_and_slow_simulation_preserve_the_production_period() {
        let mut cadence = ProtocolCadence::new(500);
        let mut count = 0;
        for now in 0..=1000 {
            // Hundreds of wall callbacks during every paused virtual tick.
            for _ in 0..100 {
                count += usize::from(cadence.due(Some(now)));
            }
        }
        assert_eq!(count, 3); // startup,500ms,1000ms; no wall-time inflation.
    }
    #[test]
    fn idle_jump_does_not_replay_missed_ticks_and_wall_timers_are_unchanged() {
        let mut cadence = ProtocolCadence::new(500);
        assert!(cadence.due(Some(0)));
        assert!(cadence.due(Some(10000)));
        assert!(!cadence.due(Some(10000)));
        assert!(!cadence.due(Some(10499)));
        assert!(cadence.due(Some(10500)));
        assert!(cadence.due(None));
        assert!(cadence.due(None));
    }
    #[test]
    fn simulator_utc_advances_only_with_mcu_time_and_production_utc_stays_live() {
        let epoch = OnceLock::new();
        assert_eq!(source_utc_ms(100000, Some(50), None, &epoch), 100000);
        assert_eq!(source_utc_ms(900000, Some(50), None, &epoch), 100000);
        assert_eq!(source_utc_ms(900001, Some(1050), None, &epoch), 101000);
        assert_eq!(source_utc_ms(900002, None, None, &epoch), 900002);
    }
    #[test]
    fn explicit_shared_epoch_survives_two_host_starts_and_process_restart() {
        let configured = 1_767_225_600_000;
        let gs = OnceLock::new();
        let rrls = OnceLock::new();
        let restarted_gs = OnceLock::new();
        let at = 30_000;
        assert_eq!(
            source_utc_ms(1_800_000_000_000, Some(at), Some(configured), &gs),
            configured + at
        );
        assert_eq!(
            source_utc_ms(1_800_000_000_500, Some(at), Some(configured), &rrls),
            configured + at
        );
        assert_eq!(
            source_utc_ms(1_800_001_200_000, Some(at), Some(configured), &restarted_gs),
            configured + at
        );
        assert_eq!(
            source_utc_ms(
                1_800_001_201_000,
                Some(at + 1_000),
                Some(configured),
                &restarted_gs
            ),
            configured + at + 1_000
        );
        // Production ignores a simulator epoch even if one was provided.
        assert_eq!(
            source_utc_ms(1_800_001_202_000, None, Some(0), &gs),
            1_800_001_202_000
        );
    }
}
