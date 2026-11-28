//! ADC over-range markers (docs/data-pipeline.md "Over-range values").
//!
//! The DAQ publishes a value whose whole publish bin was beyond the
//! MCP3564R's full scale as +Inf (above +full scale) or -Inf (below -full
//! scale); NaN keeps its old meaning (no data). Inside the station's packet
//! pipeline such a value travels as `Some(±Inf)` ("marked" values). It never
//! reaches arithmetic: a [`TelemetryRow`](crate::types::TelemetryRow) carries
//! it as `None` in `values` plus a bit in `over_range` (and `over_range_low`
//! when below -full scale), and a recorded row stores JSON null in
//! `values_json` plus the packed masks in the `over_range` column.

/// Data types whose DAQ floats may carry an over-range marker.
pub const DAQ_OVER_RANGE_TYPES: [&str; 5] =
    ["KG1000", "DAQ_LOADCELL_CHAMBER", "KG50", "DAQ_KG50_SELECTED", "DAQ_SDADC_VOLTAGES"];

/// In-flight values_json marker for an over-range value (see
/// [`marked_values_json`]); never stored.
const HIGH_MARK: &str = "ovr";
const LOW_MARK: &str = "-ovr";

/// True when `data_type` from `sender` may carry ±Inf over-range markers.
pub fn daq_type_carries_markers(sender: &str, data_type: &str) -> bool {
    crate::types::canonical_sender_id(sender.trim()) == "DAQ" && DAQ_OVER_RANGE_TYPES.contains(&data_type)
}

/// Turns every `Some(±Inf)` into `None`, returning (mask, low mask).
pub fn split(values: &mut [Option<f32>]) -> (u32, u32) {
    let (mut mask, mut low) = (0u32, 0u32);
    for (i, value) in values.iter_mut().enumerate() {
        if let Some(v) = *value
            && v.is_infinite()
        {
            *value = None;
            if i < 32 {
                mask |= 1 << i;
                if v < 0.0 {
                    low |= 1 << i;
                }
            }
        }
    }
    (mask, low)
}

/// Puts the markers back (for code that recomputes derived rows).
pub fn mark(values: &mut [Option<f32>], mask: u32, low: u32) {
    for (i, value) in values.iter_mut().enumerate().take(32) {
        if mask & (1 << i) != 0 {
            *value = Some(if low & (1 << i) != 0 { f32::NEG_INFINITY } else { f32::INFINITY });
        }
    }
}

/// The `over_range` column value: `mask | low << 32`, None when nothing is
/// over range.
pub fn pack(mask: u32, low: u32) -> Option<i64> {
    (mask != 0).then(|| (u64::from(mask) | (u64::from(low & mask) << 32)) as i64)
}

pub fn unpack(packed: Option<i64>) -> (u32, u32) {
    match packed {
        Some(p) => {
            let p = p as u64;
            let mask = p as u32;
            (mask, (p >> 32) as u32 & mask)
        }
        None => (0, 0),
    }
}

/// values_json for marked values: an over-range value is written as the
/// string "ovr"/"-ovr". Only the database writer sees this form; it stores
/// null plus the `over_range` column ([`split_values_json`]).
pub fn marked_values_json(values: &[Option<f32>]) -> Option<String> {
    let json: Vec<serde_json::Value> = values
        .iter()
        .map(|v| match v {
            Some(v) if v.is_infinite() => serde_json::Value::from(if *v > 0.0 { HIGH_MARK } else { LOW_MARK }),
            Some(v) => serde_json::Number::from_f64(f64::from(*v)).map_or(serde_json::Value::Null, serde_json::Value::Number),
            None => serde_json::Value::Null,
        })
        .collect();
    serde_json::to_string(&json).ok()
}

/// Stored form of an in-flight values_json: markers become null and the
/// packed masks are returned. Untouched (and None) when there is no marker.
pub fn split_values_json(values_json: Option<&str>) -> (Option<std::borrow::Cow<'_, str>>, Option<i64>) {
    let Some(json) = values_json else {
        return (None, None);
    };
    if !json.contains("ovr\"") {
        return (Some(json.into()), None);
    }
    let Ok(mut values) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return (Some(json.into()), None);
    };
    let (mut mask, mut low) = (0u32, 0u32);
    for (i, value) in values.iter_mut().enumerate() {
        let marker = match value.as_str() {
            Some(HIGH_MARK) => Some(false),
            Some(LOW_MARK) => Some(true),
            _ => None,
        };
        if let Some(is_low) = marker {
            *value = serde_json::Value::Null;
            if i < 32 {
                mask |= 1 << i;
                if is_low {
                    low |= 1 << i;
                }
            }
        }
    }
    (serde_json::to_string(&values).ok().map(Into::into), pack(mask, low))
}

/// A known-mass/zero capture averages about 200 samples of the live stream
/// in the client (CALIBRATION_CAPTURE_TARGET_SAMPLES); at the slowest DAQ
/// publish rate (20 Hz) that is 10 s. Without an explicit capture start the
/// station checks this far back.
pub const CAPTURE_LOOKBACK_MS: i64 = 10_000;

/// Over-range samples of `sensor` (value 0) from the DAQ at or after `since_ms`.
pub fn count_in<'a>(rows: impl IntoIterator<Item = &'a crate::types::TelemetryRow>, sensor: &str, since_ms: i64) -> usize {
    rows.into_iter()
        .filter(|r| {
            r.timestamp_ms >= since_ms
                && r.data_type == sensor
                && crate::types::canonical_sender_id(r.sender_id.trim()) == "DAQ"
                && r.is_over_range(0)
        })
        .count()
}

/// The operator reason a capture is refused for `n` over-range samples.
pub fn capture_refusal(sensor: &str, n: usize) -> String {
    format!(
        "{sensor} over range during the capture ({n} sample{}); fix the input and repeat",
        if n == 1 { "" } else { "s" }
    )
}

/// values_json for an export: each over-range position (null in the stored
/// JSON) becomes the string "OVR" / "-OVR".
pub fn export_values_json(values_json: &str, packed: Option<i64>) -> String {
    let (mask, low) = unpack(packed);
    if mask == 0 {
        return values_json.to_string();
    }
    let Ok(mut values) = serde_json::from_str::<Vec<serde_json::Value>>(values_json) else {
        return values_json.to_string();
    };
    for (i, value) in values.iter_mut().enumerate().take(32) {
        if mask & (1 << i) != 0 {
            *value = csv_marker(low & (1 << i) != 0).into();
        }
    }
    serde_json::to_string(&values).unwrap_or_else(|_| values_json.to_string())
}

/// CSV cell for an over-range value.
pub fn csv_marker(low: bool) -> &'static str {
    if low { "-OVR" } else { "OVR" }
}

/// Whether `table` (in the main schema of `db`) has an `over_range` column.
/// Recordings made before over-range support do not.
pub async fn table_has_column(db: &sqlx::SqlitePool, table: &str) -> bool {
    use sqlx::Row;
    let Ok(rows) = sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA table_info({})",
        if table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') { table } else { "telemetry" }
    )))
    .fetch_all(db)
    .await
    else {
        return false;
    };
    rows.iter().any(|r| r.try_get::<String, _>("name").ok().as_deref() == Some("over_range"))
}

/// Whether `schema.telemetry` (e.g. an attached "src") has the column.
pub async fn schema_has_column(db: &sqlx::SqlitePool, schema: &str, table: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pragma_table_info(?, ?) WHERE name='over_range'")
        .bind(table)
        .bind(schema)
        .fetch_one(db)
        .await
        .unwrap_or(0)
        > 0
}

/// Copy-column expression for an `INSERT .. SELECT` from `schema.telemetry`
/// into a main telemetry table: `(", over_range", ", over_range")` when both
/// have the column, otherwise empty strings.
pub async fn copy_columns(db: &sqlx::SqlitePool, schema: &str) -> (&'static str, &'static str) {
    if schema_has_column(db, schema, "telemetry").await && schema_has_column(db, "main", "telemetry").await {
        (",over_range", ",over_range")
    } else {
        ("", "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_mark_and_pack_round_trip() {
        let mut values = vec![Some(1.0), Some(f32::INFINITY), None, Some(f32::NEG_INFINITY)];
        let (mask, low) = split(&mut values);
        assert_eq!((mask, low), (0b1010, 0b1000));
        assert_eq!(values, vec![Some(1.0), None, None, None]);
        assert_eq!(unpack(pack(mask, low)), (mask, low));
        assert_eq!(pack(0, 0), None);
        assert_eq!(unpack(pack(1 << 31, 1 << 31)), (1 << 31, 1 << 31));
        mark(&mut values, mask, low);
        assert_eq!(values[1], Some(f32::INFINITY));
        assert_eq!(values[3], Some(f32::NEG_INFINITY));
    }

    #[test]
    fn a_capture_counts_over_range_samples_of_its_sensor_only() {
        use crate::types::TelemetryRow;
        let row = |t, ty: &str, sender: &str, v: f32| TelemetryRow::marked(t, ty.into(), sender.into(), vec![Some(v)]);
        let rows = [
            row(900, "KG1000", "DAQ", f32::INFINITY),
            row(1_000, "KG1000", "DAQ", f32::INFINITY),
            row(1_010, "KG1000", "DAQ", 0.2),
            row(1_020, "KG1000", "DAQ", f32::NEG_INFINITY),
            row(1_030, "KG50", "DAQ", f32::INFINITY),
            row(1_040, "KG1000", "VB", f32::INFINITY),
        ];
        assert_eq!(count_in(rows.iter(), "KG1000", 1_000), 2);
        assert_eq!(count_in(rows.iter(), "KG50", 0), 1);
        assert_eq!(capture_refusal("KG1000", 2), "KG1000 over range during the capture (2 samples); fix the input and repeat");
        assert_eq!(capture_refusal("KG50", 1), "KG50 over range during the capture (1 sample); fix the input and repeat");
    }

    #[test]
    fn in_flight_values_json_stores_null_and_the_column() {
        let json = marked_values_json(&[Some(2.5), Some(f32::NEG_INFINITY), None]).unwrap();
        assert_eq!(json, r#"[2.5,"-ovr",null]"#);
        let (stored, packed) = split_values_json(Some(&json));
        assert_eq!(stored.as_deref(), Some("[2.5,null,null]"));
        assert_eq!(unpack(packed), (0b10, 0b10));
        let (stored, packed) = split_values_json(Some("[1.0]"));
        assert_eq!((stored.as_deref(), packed), (Some("[1.0]"), None));
        assert_eq!(marked_values_json(&[Some(1.0)]).unwrap(), "[1.0]");
        assert_eq!(export_values_json("[1.5,null,null]", pack(0b110, 0b100)), r#"[1.5,"OVR","-OVR"]"#);
        assert_eq!(export_values_json("[1.5]", None), "[1.5]");
    }
}
