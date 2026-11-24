//! What a replay bundle carries so a client can review a recording and make
//! its motor and flight exports for any window with no station
//! (`docs/backend/replay-bundle.md`, "Review exports offline"):
//!
//! - `derived/review_inputs.json`: the rows a review reads from the source
//!   for its whole span (`data::read_pool` without a range: telemetry rows,
//!   the message log, the station's flight states), the DAQ launch file rows
//!   of a plain capture, and the capture's manifest context (label, mode,
//!   provenance, applied plan and the planner's motor for it). A client
//!   clips them to the window (`Data::clip`) and runs the same review.
//! - `models/hybrid_motor/*.json`: the hybrid motor model's spec, CEA table
//!   and grain profile (`apogee_planner::hybrid`), for the burn report's
//!   predicted curve; only when the recording has load cells and a tank PT.
//!
//! The UI's offline answers equal the station's (`?raster=false`) byte for
//! byte; `contract_fixtures` checks custom windows of a flight and a test
//! fire.
use super::data::{Data, read_pool};
use super::{motor, source_context};
use crate::burn_report::merge::Channel;
use crate::recording_replay::sources::{self, Source};
use crate::state::AppState;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const INPUTS_PATH: &str = "derived/review_inputs.json";
pub(crate) const MODEL_DIR: &str = "models/hybrid_motor";
pub(crate) const FORMAT: &str = "seds-gs-review-inputs";

/// Row types whose payload a review reads (the station-mode context, the
/// maxima's flight anchor); other rows are sent without their payload.
const PAYLOAD_TYPES: &[&str] = &["GS_STATION_MODE", "GS_FLIGHT_MAXIMA"];

/// A value as the inputs carry it: a number, null, or the over-range marker.
fn value(v: &Option<f64>) -> Value {
    match v {
        Some(v) if v.is_infinite() => crate::over_range::csv_marker(*v < 0.0).into(),
        Some(v) => json!(v),
        None => Value::Null,
    }
}

/// The inputs document of one source's whole span.
pub(crate) fn document(id: &str, source: &Source, data: &Data, context: &super::SourceContext) -> Value {
    let mut types: BTreeMap<&str, usize> = BTreeMap::new();
    let mut senders: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &data.rows {
        let n = types.len();
        types.entry(&r.data_type).or_insert(n);
        let n = senders.len();
        senders.entry(&r.sender).or_insert(n);
    }
    fn names<'a>(map: &BTreeMap<&'a str, usize>) -> Vec<&'a str> {
        let mut list = vec![""; map.len()];
        for (name, i) in map {
            list[*i] = name;
        }
        list
    }
    let rows: Vec<Value> = data
        .rows
        .iter()
        .map(|r| {
            let mut row = json!([r.t_ms, types[r.data_type.as_str()], senders[r.sender.as_str()], r.values.iter().map(value).collect::<Vec<_>>()]);
            if PAYLOAD_TYPES.contains(&r.data_type.as_str())
                && let (Some(list), Some(payload)) = (row.as_array_mut(), &r.payload)
            {
                list.push(payload.as_str().into());
            }
            row
        })
        .collect();
    let plan = context.plan.clone();
    json!({
        "format": FORMAT,
        "version": 1,
        "id": id,
        "source_kind": source.kind(),
        "label": context.label,
        "mode": context.mode,
        "provenance": context.provenance,
        "simulated": context.simulated,
        "plan": plan,
        "planner_prediction": plan.as_ref().and_then(motor::planner_prediction),
        "station_software": format!("groundstation_backend {}", env!("CARGO_PKG_VERSION")),
        "types": names(&types),
        "senders": names(&senders),
        "rows": rows,
        "messages": data.messages.iter().map(|(t, m)| json!([t, m])).collect::<Vec<_>>(),
        "station_states": data.station_states.iter().map(|(t, c)| json!([t, c])).collect::<Vec<_>>(),
        "extra_daq": context.extra_daq.iter().map(|(c, s)| json!([motor::channel_key(*c), s.t_ms, value(&Some(s.value))])).collect::<Vec<_>>(),
    })
}

/// One of this module's bundle entries (left out with `?review=false`).
pub(crate) fn is_entry(path: &str) -> bool {
    path == INPUTS_PATH || path.strip_prefix(MODEL_DIR).is_some_and(|rest| rest.starts_with('/'))
}

/// The hybrid model's predicted curve can apply: load cells and a tank PT.
fn needs_model(data: &Data, context: &super::SourceContext) -> bool {
    let inputs = motor::inputs(data, context.extra_daq.clone());
    inputs.channels.contains_key(&Channel::Tank) && (inputs.channels.contains_key(&Channel::Kg50) || inputs.channels.contains_key(&Channel::Kg1000))
}

/// The bundle entries (path, bytes, media type) of one recording; none when
/// the source cannot be read (the bundle then has no custom-window exports).
pub(crate) async fn bundle_entries(state: &Arc<AppState>, id: &str) -> Vec<(String, Vec<u8>, &'static str)> {
    let Some(source) = Source::parse(id) else { return Vec::new() };
    let Ok(pool) = sources::open(state, id).await else { return Vec::new() };
    let data = read_pool(&pool, None, None).await;
    if matches!(source, Source::File(_)) {
        pool.close().await;
    }
    let Ok(data) = data else { return Vec::new() };
    let root = crate::recording_export::directory(state);
    let context = source_context(&root, &source).await;
    let document = document(id, &source, &data, &context);
    let mut out = vec![(INPUTS_PATH.to_owned(), serde_json::to_vec(&document).unwrap_or_default(), "application/json")];
    if needs_model(&data, &context) {
        use crate::apogee_planner::hybrid;
        for (name, text) in [("hybrid_motor.json", hybrid::SPEC_JSON), ("sweet_v6_cea.json", hybrid::CEA_JSON), ("sweet_v6_grain.json", hybrid::GRAIN_JSON)] {
            out.push((format!("{MODEL_DIR}/{name}"), text.as_bytes().to_vec(), "application/json"));
        }
    }
    out
}
