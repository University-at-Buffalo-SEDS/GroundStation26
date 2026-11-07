//! Read-only AB analog signal. Acknowledgment/command/continuity are unrelated.
//! The fitted 045B5 sensor is powered at3V3, outside its specified5V supply;
//! amperes are deliberately unavailable. Raw wire metadata remains recorded.
use std::time::{Duration, Instant};

const ADC_MAX: u32 = 4095;
const YEAR_2020: u64 = 1_577_836_800_000;
const YEAR_2100: u64 = 4_102_444_800_000;
const CURRENT_UNSUPPORTED: u32 = 8;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Waiting = 0,
    Fresh = 1,
    Stale = 2,
    AdcFault = 3,
    ReferenceUnavailable = 4,
    SourceClockUnavailable = 5,
    RetiredOwner = 6,
    AdcRail = 7,
    UnsupportedProfile = 8,
}
#[derive(Clone, Copy)]
struct Owner {
    boot: u64,
    sequence: u32,
    uptime: u64,
    original_sample_at: Option<Instant>,
    clock_known: bool,
}
#[derive(Default)]
pub(crate) struct Tracker {
    owner: Option<Owner>,
    reset_after: Option<Instant>,
}
pub(crate) struct Observation {
    pub capability: Option<u8>,
    pub boot: u64,
    pub sequence: u32,
    pub status: Status,
    pub total_sample_age_ms: Option<u64>,
    pub reference_age_ms: u32,
    pub signal: Option<Signal>,
}
pub(crate) struct Signal {
    pub raw_counts: Option<f32>,
    pub volts: Option<f32>,
    pub source_utc_ms: Option<i64>,
}
impl Tracker {
    /// Retain owner high water across UI/model resets: old boot/sequence data
    /// cannot become a new sample after the display cache is cleared.
    pub fn reset_reception(&mut self, now: Instant) {
        self.reset_after = Some(now);
    }
    pub fn observe(
        &mut self,
        source_is_actuator: bool,
        words: &[u32],
        published_utc: u64,
        canonical_now: u64,
        receipt: Instant,
    ) -> Option<Observation> {
        if !source_is_actuator || words.len() != 12 || words[0] != 1 || words[1] & !31 != 0 {
            return None;
        }
        let flags = words[1];
        let boot = u64::from(words[2]) | (u64::from(words[3]) << 32);
        let sequence = words[4];
        let uptime = u64::from(words[5]) | (u64::from(words[6]) << 32);
        let raw = words[7];
        let vdda = words[8];
        let sample_age = words[9];
        let ref_age = words[10];
        let clock_ok = (YEAR_2020..YEAR_2100).contains(&published_utc)
            && (YEAR_2020..YEAR_2100).contains(&canonical_now)
            && published_utc <= canonical_now.saturating_add(100);
        let mut total_age = clock_ok.then(|| {
            canonical_now
                .saturating_sub(published_utc)
                .saturating_add(u64::from(sample_age))
        });
        let source_utc = clock_ok
            .then(|| published_utc.checked_sub(u64::from(sample_age)))
            .flatten()
            .and_then(|v| i64::try_from(v).ok());
        let reference_total_age = clock_ok.then(|| {
            canonical_now
                .saturating_sub(published_utc)
                .saturating_add(u64::from(ref_age))
        });
        let mut result = Observation {
            capability: (words[11] == 1 && flags & CURRENT_UNSUPPORTED != 0).then_some(1),
            boot,
            sequence,
            status: Status::Waiting,
            total_sample_age_ms: total_age,
            reference_age_ms: reference_total_age
                .and_then(|age| u32::try_from(age).ok())
                .unwrap_or(ref_age),
            signal: None,
        };
        if words[11] != 1 || flags & CURRENT_UNSUPPORTED == 0 {
            result.status = Status::UnsupportedProfile;
            return Some(result);
        }
        if matches!(boot, 0 | u64::MAX) || sequence == 0 {
            return Some(result);
        }
        // Unknown network UTC still supplies a conservative latest-possible
        // sample birth bound: it cannot originate after its first receipt.
        let original_sample_at = receipt.checked_sub(Duration::from_millis(
            total_age.unwrap_or(u64::from(sample_age)),
        ));
        let new_owner = Owner {
            boot,
            sequence,
            uptime,
            original_sample_at,
            clock_known: clock_ok,
        };
        if let Some(old) = self.owner {
            let forward = sequence.wrapping_sub(old.sequence);
            if boot < old.boot
                || (boot == old.boot
                    && (forward >= 0x8000_0000
                        || uptime < old.uptime
                        || (forward == 0 && uptime != old.uptime)))
            {
                result.status = Status::RetiredOwner;
                return Some(result);
            }
        }
        if let Some(old) = self
            .owner
            .filter(|old| old.boot == boot && old.sequence == sequence)
            && let Some(age) = old
                .original_sample_at
                .and_then(|at| receipt.checked_duration_since(at))
        {
            // A newly stamped publication of one cached ADC sample may
            // never move its source age forward to the receipt clock.
            total_age = Some(
                total_age
                    .unwrap_or(0)
                    .max(age.as_millis().min(u128::from(u64::MAX)) as u64),
            );
            result.total_sample_age_ms = total_age;
        }
        if let Some(floor) = self.reset_after {
            let after_reset = self
                .owner
                .filter(|old| old.boot == boot && old.sequence == sequence)
                .and_then(|old| old.original_sample_at)
                .or(new_owner.original_sample_at)
                .is_some_and(|source| source >= floor);
            if !after_reset {
                // Even a rejected original must retain its birth upper-bound:
                // later clock synchronization/republication cannot relabel it
                // as a sample taken after this reset.
                if self
                    .owner
                    .is_none_or(|old| old.boot != boot || old.sequence != sequence)
                {
                    self.owner = Some(new_owner);
                }
                result.status = Status::RetiredOwner;
                return Some(result);
            }
        }
        let source_clock_known = clock_ok
            && self
                .owner
                .filter(|old| old.boot == boot && old.sequence == sequence)
                .is_none_or(|old| old.clock_known);
        let fresh_sample = flags & 1 != 0 && flags & 16 == 0 && raw <= ADC_MAX && sample_age <= 250;
        let reference_ok = flags & 2 != 0 && (1620..=3600).contains(&vdda) && ref_age <= 6000;
        result.status = if flags & 16 != 0 {
            Status::AdcFault
        } else if !fresh_sample {
            Status::Stale
        } else if !source_clock_known {
            Status::SourceClockUnavailable
        } else if total_age.is_none_or(|age| age > 1000) {
            Status::Stale
        } else if !reference_ok || reference_total_age.is_some_and(|age| age > 6000) {
            Status::ReferenceUnavailable
        } else if raw == 0 || raw == ADC_MAX {
            Status::AdcRail
        } else {
            Status::Fresh
        };
        let is_new = self
            .owner
            .is_none_or(|old| old.boot != boot || old.sequence != sequence);
        if is_new {
            self.owner = Some(new_owner);
            // Receipt delay changes live freshness, not the measured historical
            // signal. Its original sample UTC (when known) remains the row time.
            let raw_counts = fresh_sample.then_some(raw as f32);
            let volts = raw_counts
                .filter(|_| reference_ok)
                .map(|raw| raw * vdda as f32 / (ADC_MAX as f32 * 1000.0));
            result.signal = Some(Signal {
                raw_counts,
                volts,
                source_utc_ms: source_utc,
            });
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const UTC: u64 = 1_770_000_000_000;
    fn frame() -> [u32; 12] {
        [1, 1 | 2 | 8, 4, 0, 1, 500, 0, 2048, 3300, 0, 0, 1]
    }
    #[test]
    fn signal_is_not_firing_or_current_and_uses_actual_reference() {
        let mut t = Tracker::default();
        let at = Instant::now();
        let a = t.observe(true, &frame(), UTC, UTC, at).unwrap();
        assert_eq!(a.status, Status::Fresh);
        assert_eq!((a.boot, a.sequence), (4, 1));
        assert_eq!(a.capability, Some(1));
        assert_eq!(a.total_sample_age_ms, Some(0));
        assert_eq!(a.reference_age_ms, 0);
        let s = a.signal.unwrap();
        assert_eq!(s.raw_counts, Some(2048.0));
        assert!((s.volts.unwrap() - 1.6504).abs() < 0.001);
        let mut f = frame();
        f[4] = 2;
        f[5] += 50;
        f[8] = 3000;
        let s = t
            .observe(true, &f, UTC + 50, UTC + 50, at)
            .unwrap()
            .signal
            .unwrap();
        assert!((s.volts.unwrap() - 1.5004).abs() < 0.001);
    }
    #[test]
    fn rejects_bad_source_kind_shape_version_profile_and_nonfinite_routes() {
        let mut t = Tracker::default();
        let at = Instant::now();
        assert!(t.observe(false, &frame(), UTC, UTC, at).is_none());
        assert!(t.observe(true, &frame()[..11], UTC, UTC, at).is_none());
        let mut f = frame();
        f[0] = 2;
        assert!(t.observe(true, &f, UTC, UTC, at).is_none());
        f = frame();
        f[1] |= 32;
        assert!(t.observe(true, &f, UTC, UTC, at).is_none());
        f = frame();
        f[11] = 99;
        let a = t.observe(true, &f, UTC, UTC, at).unwrap();
        assert_eq!(a.status, Status::UnsupportedProfile);
        assert!(a.signal.is_none());
        f = frame();
        f[8] = 1;
        let a = t.observe(true, &f, UTC, UTC, at).unwrap();
        assert_eq!(a.status, Status::ReferenceUnavailable);
        assert_eq!(a.signal.unwrap().volts, None);
    }
    #[test]
    fn rails_zero_stale_fault_and_unsynced_are_not_fake_nominal() {
        for raw in [0, 4095] {
            let mut t = Tracker::default();
            let mut f = frame();
            f[7] = raw;
            assert_eq!(
                t.observe(true, &f, UTC, UTC, Instant::now())
                    .unwrap()
                    .status,
                Status::AdcRail
            );
        }
        let mut f = frame();
        f[1] |= 16;
        let a = Tracker::default()
            .observe(true, &f, UTC, UTC, Instant::now())
            .unwrap();
        assert_eq!(a.status, Status::AdcFault);
        assert_eq!(a.signal.unwrap().raw_counts, None);
        f = frame();
        let a = Tracker::default()
            .observe(true, &f, UTC, UTC + 5000, Instant::now())
            .unwrap();
        assert_eq!(a.status, Status::Stale);
        assert_eq!(a.signal.unwrap().source_utc_ms, Some(UTC as i64));
        let a = Tracker::default()
            .observe(true, &f, 500, UTC, Instant::now())
            .unwrap();
        assert_eq!(a.status, Status::SourceClockUnavailable);
        assert_eq!(a.signal.unwrap().source_utc_ms, None);
    }
    #[test]
    fn retries_do_not_become_new_samples_and_boot_replay_is_retired() {
        let mut t = Tracker::default();
        let at = Instant::now();
        let f = frame();
        assert!(t.observe(true, &f, UTC, UTC, at).unwrap().signal.is_some());
        assert!(
            t.observe(
                true,
                &f,
                UTC + 200,
                UTC + 200,
                at + Duration::from_millis(200)
            )
            .unwrap()
            .signal
            .is_none()
        );
        let duplicate = t
            .observe(
                true,
                &f,
                UTC + 5000,
                UTC + 5000,
                at + Duration::from_millis(5000),
            )
            .unwrap();
        assert_eq!(duplicate.status, Status::Stale);
        assert_eq!(duplicate.total_sample_age_ms, Some(5000));
        let mut newer = f;
        newer[2] = 5;
        newer[5] = 100;
        assert!(
            t.observe(true, &newer, UTC + 50, UTC + 50, at)
                .unwrap()
                .signal
                .is_some()
        );
        assert_eq!(
            t.observe(true, &f, UTC + 100, UTC + 100, at)
                .unwrap()
                .status,
            Status::RetiredOwner
        );
        newer[4] = 2;
        newer[5] = 99;
        assert_eq!(
            t.observe(true, &newer, UTC + 150, UTC + 150, at)
                .unwrap()
                .status,
            Status::RetiredOwner
        );
    }
    #[test]
    fn reset_floor_and_original_sample_age_survive_republication() {
        let mut t = Tracker::default();
        let at = Instant::now();
        let mut f = frame();
        t.reset_reception(at);
        f[9] = 100;
        assert_eq!(
            t.observe(true, &f, UTC, UTC, at).unwrap().status,
            Status::RetiredOwner
        );
        let a = t
            .observe(
                true,
                &f,
                UTC + 200,
                UTC + 200,
                at + Duration::from_millis(200),
            )
            .unwrap();
        assert_eq!(a.status, Status::RetiredOwner);
        assert!(a.signal.is_none());
        f[4] = 2;
        f[5] += 50;
        let fresh = t
            .observe(
                true,
                &f,
                UTC + 200,
                UTC + 200,
                at + Duration::from_millis(200),
            )
            .unwrap();
        assert!(fresh.signal.is_some());
        f[4] = 3;
        f[5] += 50;
        f[9] = 251;
        let a = t
            .observe(
                true,
                &f,
                UTC + 300,
                UTC + 300,
                at + Duration::from_millis(300),
            )
            .unwrap();
        assert_eq!(a.status, Status::Stale);
        assert_eq!(a.signal.unwrap().raw_counts, None);
    }
    #[test]
    fn known_pre_reset_sample_cannot_be_restamped_past_floor() {
        let mut tracker = Tracker::default();
        let start = Instant::now();
        let mut words = frame();
        words[9] = 50;
        assert!(
            tracker
                .observe(true, &words, UTC, UTC, start)
                .unwrap()
                .signal
                .is_some()
        );
        tracker.reset_reception(start + Duration::from_millis(10));
        words[9] = 0;
        let duplicate = tracker
            .observe(
                true,
                &words,
                UTC + 100,
                UTC + 100,
                start + Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(duplicate.status, Status::RetiredOwner);
        assert!(duplicate.signal.is_none());
        words[4] += 1;
        words[5] += 150;
        let next = tracker
            .observe(
                true,
                &words,
                UTC + 150,
                UTC + 150,
                start + Duration::from_millis(150),
            )
            .unwrap();
        assert_eq!(next.status, Status::Fresh);
        assert!(next.signal.is_some());
    }
    #[test]
    fn unsynced_cached_sample_cannot_gain_freshness_or_escape_reset() {
        let mut tracker = Tracker::default();
        let start = Instant::now();
        let words = frame();
        assert_eq!(
            tracker
                .observe(true, &words, 500, UTC, start)
                .unwrap()
                .status,
            Status::SourceClockUnavailable
        );
        assert_eq!(
            tracker
                .observe(
                    true,
                    &words,
                    UTC + 50,
                    UTC + 50,
                    start + Duration::from_millis(50)
                )
                .unwrap()
                .status,
            Status::SourceClockUnavailable
        );
        tracker.reset_reception(start + Duration::from_millis(60));
        assert_eq!(
            tracker
                .observe(
                    true,
                    &words,
                    UTC + 100,
                    UTC + 100,
                    start + Duration::from_millis(100)
                )
                .unwrap()
                .status,
            Status::RetiredOwner
        );
    }
    #[test]
    fn unsynced_first_receipt_after_reset_retains_earlier_advertised_birth() {
        let mut tracker = Tracker::default();
        let start = Instant::now();
        let mut words = frame();
        tracker.reset_reception(start);
        words[9] = 250;
        assert_eq!(
            tracker
                .observe(true, &words, 500, UTC, start + Duration::from_millis(100))
                .unwrap()
                .status,
            Status::RetiredOwner
        );
        words[9] = 0;
        let retry = tracker
            .observe(
                true,
                &words,
                UTC + 200,
                UTC + 200,
                start + Duration::from_millis(200),
            )
            .unwrap();
        assert_eq!(retry.status, Status::RetiredOwner);
        assert!(retry.signal.is_none());
        words[4] += 1;
        words[5] += 500;
        let fresh = tracker
            .observe(
                true,
                &words,
                UTC + 500,
                UTC + 500,
                start + Duration::from_millis(500),
            )
            .unwrap();
        assert_eq!(fresh.status, Status::Fresh);
        assert!(fresh.signal.is_some());
    }
}
