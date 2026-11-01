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

pub(crate) fn canonical_source(source: i64, canonical_now: i64) -> Option<i64> {
    // Packet uptime/unknown epochs are never promoted to UTC. The source must
    // also agree with the actual purpose clock (including explicit model UTC).
    const YEAR_2020: i64 = 1_577_836_800_000;
    const YEAR_2100: i64 = 4_102_444_800_000;
    (source >= YEAR_2020
        && source < YEAR_2100
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
    #[test]
    fn ui_serializer_omits_unknown_and_retains_receipt_and_actual_source() {
        let row = TelemetryRow {
            timestamp_ms: 11,
            data_type: "provenance-test".into(),
            sender_id: "DAQ".into(),
            values: vec![Some(5.)],
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
