//! Optional UI provenance; receipt clocks and stored/raw telemetry are unchanged.
//! A bounded sidecar avoids changing every TelemetryRow constructor. Conflicting
//! sources for the same exposed row identity are deliberately unknown.
use crate::types::TelemetryRow;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
type Key = (i64, String, String);
const MAX_ROWS: usize = 250_000;
const HISTORY_MS: i64 = 1_200_000;
static SOURCES: OnceLock<Mutex<BTreeMap<Key, Option<i64>>>> = OnceLock::new();
fn key(row: &TelemetryRow) -> Key {
    (
        row.timestamp_ms,
        row.data_type.clone(),
        row.sender_id.clone(),
    )
}

/// A received packet's source time on the purpose clock. A stamp from an
/// earlier epoch is first translated into the station's current time base
/// (network time now minus its SEDSnet age, across network clock steps); an
/// untranslatable one is not promoted. The result must then agree with the
/// purpose clock as before.
pub(crate) fn canonical_source_on(
    router: Option<&sedsnet::router::Router>,
    source: crate::network_stamp::Stamp,
    canonical_now: i64,
) -> Option<i64> {
    let current_base = match router {
        Some(router) if router.network_time_epoch() != source.epoch => {
            let now = router.network_time_ms().and_then(|now| i64::try_from(now).ok())?;
            match crate::network_stamp::age_on(router, source) {
                sedsnet::timesync::NetworkStampAge::Age(age) => now.saturating_sub(age),
                sedsnet::timesync::NetworkStampAge::NewerEpoch { since_ms } => {
                    now.saturating_sub(since_ms.min(i64::MAX as u64) as i64)
                }
                _ => return None,
            }
        }
        _ => source.ms,
    };
    canonical_source(current_base, canonical_now)
}

/// A stored source time (its epoch is not recorded) against the purpose clock.
pub(crate) fn canonical_source(source: i64, canonical_now: i64) -> Option<i64> {
    // Packet uptime/unknown epochs are never promoted to UTC. The source must
    // also agree with the actual purpose clock (including explicit model UTC).
    const YEAR_2020: i64 = 1_577_836_800_000;
    const YEAR_2100: i64 = 4_102_444_800_000;
    ((YEAR_2020..YEAR_2100).contains(&source)
        && canonical_now.saturating_sub(source) >= -5_000
        && canonical_now.saturating_sub(source) <= 3_600_000)
        .then_some(source)
}
pub(crate) fn note(row: &TelemetryRow, source: Option<i64>) {
    let Ok(mut sources) = SOURCES.get_or_init(Default::default).lock() else {
        return;
    };
    sources
        .entry(key(row))
        .and_modify(|old| {
            if *old != source {
                *old = None;
            }
        })
        .or_insert(source);
    let floor = row.timestamp_ms.saturating_sub(HISTORY_MS);
    while sources
        .first_key_value()
        .is_some_and(|(key, _)| key.0 < floor)
        || sources.len() > MAX_ROWS
    {
        sources.pop_first();
    }
}
pub(crate) fn source(row: &TelemetryRow) -> Option<i64> {
    SOURCES
        .get()?
        .lock()
        .ok()?
        .get(&key(row))
        .copied()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_evidenced_utc_is_exported_for_wall_or_paused_model_clocks() {
        let utc = 1_767_225_600_000;
        assert_eq!(canonical_source(1234, utc), None);
        assert_eq!(canonical_source(utc, utc), Some(utc));
        assert_eq!(canonical_source(utc - 3000, utc), Some(utc - 3000));
        assert_eq!(canonical_source(utc + 100_000, utc), None);
        assert_eq!(canonical_source(utc, utc + 3_600_001), None);
        // Wall time is intentionally absent: a paused/slow model is held.
        assert_eq!(canonical_source(utc + 200, utc + 200), Some(utc + 200));
    }
    /// After a 4 h network clock step back a packet stamped 300 ms before
    /// the step is placed 300 ms before now, not 4 h ahead (refused); a
    /// copy from an epoch the station no longer remembers is not promoted.
    #[test]
    fn received_sources_are_aged_by_epoch_across_a_step() {
        use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
        use crate::network_stamp::Stamp;
        let clock = Arc::new(AtomicU64::new(10_000));
        let input = clock.clone();
        let router = sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]).with_sender("STATION").with_timesync(
                sedsnet::timesync::TimeSyncConfig {
                    role: sedsnet::timesync::TimeSyncRole::Source,
                    priority: 50,
                    ..Default::default()
                },
            ),
            Box::new(move || input.load(Ordering::SeqCst)),
        );
        router.set_timesync_steps_allowed(true);
        let set = |ms: u64| {
            router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(ms)).unwrap()
        };
        set(1_800_000_000_000);
        let (ms, epoch) = router.network_time_ms_with_epoch().unwrap();
        let pre = Stamp::new(epoch, ms as i64);
        clock.fetch_add(300, Ordering::SeqCst);
        set(router.network_time_ms().unwrap() - 4 * 3_600_000);
        let now = router.network_time_ms().unwrap() as i64;
        assert_eq!(canonical_source_on(Some(&router), pre, now), Some(now - 300));
        // The raw comparison read it as hours from the purpose clock.
        assert_eq!(canonical_source(pre.ms, now), None);
        for i in 1..=5u64 {
            set(1_800_000_000_000 + i * 10_000_000);
        }
        let now = router.network_time_ms().unwrap() as i64;
        assert_eq!(canonical_source_on(Some(&router), pre, now), None);
    }
    #[test]
    fn ui_serializer_omits_unknown_and_retains_receipt_and_actual_source() {
        let row = TelemetryRow {
            timestamp_ms: 11,
            data_type: "provenance-test".into(),
            sender_id: "DAQ".into(),
            values: vec![Some(5.)], over_range: 0, over_range_low: 0,
        };
        assert!(
            serde_json::to_value(&row)
                .unwrap()
                .get("source_timestamp_ms")
                .is_none()
        );
        note(&row, Some(1_767_225_600_123));
        let wire = serde_json::to_value(&row).unwrap();
        assert_eq!(wire["timestamp_ms"], 11);
        assert_eq!(wire["source_timestamp_ms"], 1_767_225_600_123i64);
        note(&row, Some(1_767_225_600_124));
        assert!(
            serde_json::to_value(&row)
                .unwrap()
                .get("source_timestamp_ms")
                .is_none()
        );
    }
}
