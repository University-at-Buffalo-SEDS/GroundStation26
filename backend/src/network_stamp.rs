//! Received `(time_epoch, timestamp)` stamps judged against the station's
//! network clock (SEDSnet 6).
//!
//! Every packet carries the time-base epoch its timestamp was taken in. A
//! node never steps back inside an epoch and epochs only grow, so:
//!
//! - ordering: one sender's stamps (and stamps of different senders in the
//!   network's shared epochs) order by `(epoch, timestamp)` across steps in
//!   either direction ([`Stamp`] derives exactly that order);
//! - freshness: the age of a stamp comes from the router
//!   (`network_stamp_age_ms`), which translates it across the steps the
//!   station remembers. A raw timestamp is never compared with the station's
//!   clock across a step.
//!
//! Much of the station's receive path computes ages as `receipt - source`
//! with the source's raw timestamp. [`receipt_ms_for`] supplies that receipt
//! reading in the source's own time base (the source timestamp plus its true
//! age), so those differences are the router's age whatever steps happened.
use sedsnet::{packet::Packet, router::Router, timesync::NetworkStampAge};

/// Unix ms of 2020-01-01: stamps below it are boot counters, not UTC.
pub(crate) const UTC_2020_MS: i64 = 1_577_836_800_000;

/// A network stamp: the epoch its timestamp was taken in and the timestamp.
/// The derived order is `(epoch, ms)`, SEDSnet's
/// `timesync::compare_network_stamps`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub(crate) struct Stamp {
    pub(crate) epoch: u16,
    pub(crate) ms: i64,
}

impl Stamp {
    pub(crate) const fn new(epoch: u16, ms: i64) -> Self {
        Self { epoch, ms }
    }

    /// A stamp in epoch 0 (no step seen yet), as every stamp read before
    /// SEDSnet 6 was.
    #[cfg(test)]
    pub(crate) const fn raw(ms: i64) -> Self {
        Self { epoch: 0, ms }
    }

    pub(crate) fn of(packet: &Packet) -> Self {
        Self {
            epoch: packet.time_epoch(),
            ms: i64::try_from(packet.timestamp()).unwrap_or(i64::MAX),
        }
    }
}

/// A stamp `ms` later in the same epoch.
impl std::ops::Add<i64> for Stamp {
    type Output = Stamp;
    fn add(self, ms: i64) -> Stamp {
        Stamp::new(self.epoch, self.ms.saturating_add(ms))
    }
}

/// A stamp `ms` earlier in the same epoch.
impl std::ops::Sub<i64> for Stamp {
    type Output = Stamp;
    fn sub(self, ms: i64) -> Stamp {
        Stamp::new(self.epoch, self.ms.saturating_sub(ms))
    }
}

/// The station router used for time judgements (set at boot).
fn station_router() -> Option<&'static Router> {
    crate::telemetry_task::network_time_router()
}

/// Age of `stamp` on `router`'s network clock.
pub(crate) fn age_on(router: &Router, stamp: Stamp) -> NetworkStampAge {
    match u64::try_from(stamp.ms) {
        Ok(ms) => router.network_stamp_age_ms(stamp.epoch, ms),
        Err(_) => NetworkStampAge::OlderEpoch,
    }
}

/// Whether `stamp` is at most `max_age_ms` old and at most `max_ahead_ms`
/// ahead on `router`'s network clock (`Router::network_stamp_is_fresh`).
pub(crate) fn is_fresh_on(router: &Router, stamp: Stamp, max_age_ms: u64, max_ahead_ms: u64) -> bool {
    age_on(router, stamp).is_fresh(max_age_ms, max_ahead_ms)
}

/// The receipt reading to subtract `stamp.ms` from: the station's network
/// time now, expressed in the stamp's own time base, so that
/// `receipt - stamp.ms` is the stamp's true age.
///
/// - same or translatable epoch: `stamp.ms + age`;
/// - newer epoch (the station is re-anchoring onto it): `stamp.ms + since`,
///   the most the stamp can be old;
/// - an older epoch the station can no longer translate: a reading far past
///   every freshness window (stale);
/// - no network time: `fallback` (the station's receipt clock), as before
///   network time existed.
///
/// A boot-counter stamp (below 2020) is not UTC and has no network age:
/// callers treat it as received now, so the station's reading is returned.
pub(crate) fn receipt_ms_on(router: Option<&Router>, stamp: Stamp, fallback: i64) -> i64 {
    let Some(router) = router else {
        return fallback;
    };
    if stamp.ms < UTC_2020_MS {
        return router
            .network_time_ms()
            .and_then(|now| i64::try_from(now).ok())
            .unwrap_or(fallback);
    }
    match age_on(router, stamp) {
        NetworkStampAge::Age(age) => stamp.ms.saturating_add(age),
        NetworkStampAge::NewerEpoch { since_ms } => {
            stamp.ms.saturating_add(i64::try_from(since_ms).unwrap_or(i64::MAX))
        }
        NetworkStampAge::OlderEpoch => stamp.ms.saturating_add(STALE_AGE_MS),
        NetworkStampAge::NoTime => fallback,
    }
}

/// An age beyond every freshness window and history horizon (one year).
const STALE_AGE_MS: i64 = 365 * 24 * 3_600_000;

/// [`receipt_ms_on`] with the station router; `fallback` when it is not set.
pub(crate) fn receipt_ms_for(stamp: Stamp, fallback: i64) -> i64 {
    receipt_ms_on(station_router(), stamp, fallback)
}

/// [`receipt_ms_for`] for a received packet.
pub(crate) fn receipt_ms_for_packet(packet: &Packet, fallback: i64) -> i64 {
    receipt_ms_for(Stamp::of(packet), fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    fn station(clock: &Arc<AtomicU64>) -> Router {
        let input = clock.clone();
        Router::new_with_clock(
            sedsnet::router::RouterConfig::new([])
                .with_sender("STATION")
                .with_timesync(sedsnet::timesync::TimeSyncConfig {
                    role: sedsnet::timesync::TimeSyncRole::Source,
                    priority: 50,
                    ..Default::default()
                }),
            Box::new(move || input.load(Ordering::SeqCst)),
        )
    }

    fn set(router: &Router, ms: u64) {
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(ms))
            .unwrap();
    }

    #[test]
    fn stamps_order_by_epoch_then_timestamp() {
        // A write after a 4 h step back is newer than every pre-step write.
        let before = Stamp::new(3, 1_800_000_000_000);
        let after = Stamp::new(4, 1_800_000_000_000 - 4 * 3_600_000);
        assert!(after > before);
        assert!(Stamp::new(4, 10) < Stamp::new(4, 11));
        assert_eq!(
            before.cmp(&after),
            sedsnet::timesync::compare_network_stamps((3, before.ms as u64), (4, after.ms as u64))
        );
    }

    /// A report stamped 300 ms before a 4 h step back (or a 3.8 h step
    /// forward) is 300 ms old after it, not 4 h ahead or 3.8 h stale; a report
    /// stamped in the new epoch is judged directly.
    #[test]
    fn receipt_reading_gives_the_true_age_across_a_step() {
        for delta in [-14_400_000i64, 13_700_000] {
            let clock = Arc::new(AtomicU64::new(10_000));
            let router = station(&clock);
            router.set_timesync_steps_allowed(true);
            set(&router, 1_800_000_000_000);
            clock.fetch_add(1_000, Ordering::SeqCst);
            let (ms, epoch) = router.network_time_ms_with_epoch().unwrap();
            let pre = Stamp::new(epoch, ms as i64);
            clock.fetch_add(300, Ordering::SeqCst);
            let now = router.network_time_ms().unwrap() as i64;
            set(&router, (now + delta) as u64);
            assert!(router.network_time_epoch() > epoch, "{delta}: stepped");
            let receipt = receipt_ms_on(Some(&router), pre, 0);
            assert!((receipt - pre.ms - 300).abs() <= 2, "{delta}: age {}", receipt - pre.ms);
            assert!(is_fresh_on(&router, pre, 2_000, 100));
            let (ms, epoch) = router.network_time_ms_with_epoch().unwrap();
            let post = Stamp::new(epoch, ms as i64 - 50);
            assert_eq!(receipt_ms_on(Some(&router), post, 0) - post.ms, 50);
            assert!(post > pre, "{delta}: the post-step report orders after the pre-step one");
        }
    }

    /// DAQ_LAUNCH_LOG_STATUS and valve reports through the receive-path
    /// arithmetic (`receipt - source`): after an operator step (3.8 h
    /// forward or 4 h back) a report stamped in the new epoch is accepted; a
    /// copy stamped 6 s before the step is judged 6 s old and refused as a
    /// valve report (5 s window), never as hours ahead or behind.
    #[test]
    fn launch_log_and_valve_reports_across_an_operator_step() {
        for delta in [13_700_000i64, -14_400_000] {
            let clock = Arc::new(AtomicU64::new(10_000));
            let router = station(&clock);
            router.set_timesync_steps_allowed(true);
            set(&router, 1_800_000_000_000);
            let (ms, epoch) = router.network_time_ms_with_epoch().unwrap();
            let pre_step = Stamp::new(epoch, ms as i64);
            clock.fetch_add(6_000, Ordering::SeqCst);
            let now = router.network_time_ms().unwrap() as i64;
            set(&router, (now + delta) as u64);
            let (ms, epoch) = router.network_time_ms_with_epoch().unwrap();
            let post_step = Stamp::new(epoch, ms as i64 - 30);

            let receipt = receipt_ms_on(Some(&router), post_step, 0);
            assert_eq!(
                crate::daq_launch_log::source_age_ms(post_step.ms as u64, receipt as u64),
                Some(30),
                "{delta}: new-epoch launch-log status accepted"
            );
            assert!(
                crate::gse::source_age_for_test(post_step, receipt).is_some(),
                "{delta}: new-epoch valve report accepted"
            );

            let receipt = receipt_ms_on(Some(&router), pre_step, 0);
            assert!((receipt - pre_step.ms - 6_000).abs() <= 2, "{delta}: aged 6 s");
            assert!(crate::gse::source_age_for_test(pre_step, receipt).is_none());
            assert_eq!(
                crate::valve_report_health::classify_refusal(pre_step.ms, receipt),
                crate::valve_report_health::Refusal::ClockSkew {
                    source_minus_station_ms: pre_step.ms - receipt
                }
            );
            // The raw comparison this replaces read the copy as hours off.
            assert!((pre_step.ms - now - delta).abs() > 3_600_000);
        }
    }

    #[test]
    fn untranslatable_or_unknown_stamps() {
        let clock = Arc::new(AtomicU64::new(10_000));
        let router = station(&clock);
        assert_eq!(receipt_ms_on(Some(&router), Stamp::raw(1_800_000_000_000), 77), 77, "no time");
        assert_eq!(receipt_ms_on(None, Stamp::raw(1_800_000_000_000), 77), 77, "no router");
        router.set_timesync_steps_allowed(true);
        set(&router, 1_800_000_000_000);
        // Five steps: epoch 0 is no longer remembered.
        for i in 1..=5u64 {
            set(&router, 1_800_000_000_000 + i * 10_000_000);
        }
        let old = Stamp::raw(router.network_time_ms().unwrap() as i64);
        assert!(!is_fresh_on(&router, old, 60_000, 1_000));
        assert!(receipt_ms_on(Some(&router), old, 0) - old.ms > 3_600_000);
        // Boot counters keep the station's reading (treated as received now).
        let now = router.network_time_ms().unwrap() as i64;
        assert_eq!(receipt_ms_on(Some(&router), Stamp::new(9, 1234), 0), now);
    }
}
