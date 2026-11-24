//! BAROMETER_DATA is the FC's `baro { alt, prs, tmp }` struct sent whole
//! (FlightComputer26 fcstructs.h, distribution.c; schema "alt, prs, tmp").
//! A real FC-ordered frame must read back as altitude, pressure and
//! temperature in every station consumer and in every shipped layout.
use crate::telemetry_schema::baro;
use sedsnet::packet::Packet;
use std::collections::HashSet;

const ALT_M: f32 = 123.4;
const PRESSURE_PA: f32 = 98_765.0;
const TEMP_C: f32 = 21.5;

fn fc_frame(values: [f32; 3]) -> Vec<f32> {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let pkt = Packet::new(
        crate::telemetry_schema::data_type("BAROMETER_DATA"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "FC",
        1_000,
        std::sync::Arc::from(bytes.as_slice()),
    )
    .unwrap();
    pkt.data_as_f32().unwrap()
}

fn warnings(values: [f32; 3]) -> HashSet<&'static str> {
    let mut out = HashSet::new();
    crate::safety_task::check_baro_thresholds(&fc_frame(values), &mut out);
    out
}

#[test]
fn fc_ordered_frame_decodes_to_altitude_pressure_temperature() {
    let values = fc_frame([ALT_M, PRESSURE_PA, TEMP_C]);
    assert_eq!(values[baro::ALTITUDE_M], ALT_M);
    assert_eq!(values[baro::PRESSURE_PA], PRESSURE_PA);
    assert_eq!(values[baro::TEMPERATURE_C], TEMP_C);
}

#[test]
fn safety_thresholds_test_the_fields_they_name() {
    assert!(warnings([ALT_M, PRESSURE_PA, TEMP_C]).is_empty());
    // Real pad data (2026-11-09 recording) and a cold, drifted pad.
    assert!(warnings([-0.64, 98_221.9, 24.15]).is_empty());
    assert!(warnings([-12.0, 101_900.0, -15.0]).is_empty());
    assert_eq!(
        warnings([ALT_M, 20_000.0, TEMP_C]),
        HashSet::from(["Critical: Barometer pressure threshold exceeded!"])
    );
    assert_eq!(
        warnings([ALT_M, PRESSURE_PA, 90.0]),
        HashSet::from(["Critical: Barometer temperature threshold exceeded!"])
    );
    assert_eq!(
        warnings([-40.0, PRESSURE_PA, TEMP_C]),
        HashSet::from(["Critical: Barometer altitude threshold exceeded!"])
    );
}

#[test]
fn flight_maxima_takes_barometer_altitude() {
    let row = crate::types::TelemetryRow {
        timestamp_ms: 1_000,
        data_type: "BAROMETER_DATA".into(),
        sender_id: "FC".into(),
        values: fc_frame([ALT_M, PRESSURE_PA, TEMP_C]).into_iter().map(Some).collect(), over_range: 0, over_range_low: 0,
    };
    let samples = crate::flight_maxima::samples(&row);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].0, crate::flight_maxima::Quantity::Altitude);
    assert_eq!(samples[0].1, f64::from(ALT_M));
}

#[test]
fn fc_estimator_reference_reads_the_same_order() {
    let reference = crate::fc_estimator::reference_pressure(&fc_frame([ALT_M, PRESSURE_PA, TEMP_C]))
        .expect("plausible sample");
    // 123.4 m above the reference: about 1.47 kPa more than the reading.
    assert!((100_100.0..100_400.0).contains(&reference), "{reference}");
}

/// Every BAROMETER_DATA label in every shipped layout names the value at its index.
#[test]
fn layouts_label_barometer_channels_in_fc_order() {
    let values = fc_frame([ALT_M, PRESSURE_PA, TEMP_C]);
    let expected = |label: &str| -> f32 {
        let l = label.to_ascii_lowercase();
        if l.contains("alt") || l == "barometer" {
            ALT_M
        } else if l.contains("pressure") {
            PRESSURE_PA
        } else if l.contains("temp") {
            TEMP_C
        } else {
            panic!("unrecognized barometer label {label:?}")
        }
    };
    fn walk(v: &serde_json::Value, out: &mut Vec<(String, usize)>) {
        match v {
            serde_json::Value::Object(map) => {
                let baro = |key: &str| map.get(key).and_then(|v| v.as_str()) == Some("BAROMETER_DATA");
                if baro("id")
                    && let Some(channels) = map.get("channels").and_then(|c| c.as_array())
                {
                    for (i, c) in channels.iter().enumerate() {
                        out.push((c.as_str().unwrap().to_owned(), i));
                    }
                }
                if baro("data_type") {
                    if let Some(items) = map.get("items").and_then(|c| c.as_array()) {
                        for item in items {
                            out.push((item["label"].as_str().unwrap().to_owned(), item["index"].as_u64().unwrap() as usize));
                        }
                    }
                    if let (Some(label), Some(index)) = (map.get("label").and_then(|l| l.as_str()), map.get("index").and_then(|i| i.as_u64())) {
                        out.push((label.to_owned(), index as usize));
                    }
                }
                map.values().for_each(|v| walk(v, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("layout");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let layout: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut labels = Vec::new();
        walk(&layout, &mut labels);
        for (label, index) in labels {
            assert_eq!(values[index], expected(&label), "{} {label:?} index {index}", path.display());
            checked += 1;
        }
    }
    assert!(checked >= 6, "{checked}");
}
