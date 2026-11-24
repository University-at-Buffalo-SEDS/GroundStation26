//! Live diagram topologies (docs/live-diagrams.md). The declarative files in
//! `backend/layout/diagrams/*.json` (or `GS_DIAGRAMS_DIR`) describe the fill
//! plumbing, the AV bay, the network and the stand; the dashboard draws them
//! and derives valve/flow state itself. Editing a file changes the drawing on
//! the next page load, without rebuilding the client.
//!
//! `GET /api/diagrams` (ViewData): `{"diagrams":[topology…],"errors":[{file,error}]}`.
//! A file that fails the structural check here is reported, not served; the
//! client validates fully again and falls back to its bundled copy.
use crate::auth::Permission;
use crate::state::AppState;
use crate::web::authorize_headers;
use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const PATH: &str = "/api/diagrams";
const SCHEMA: u64 = 1;
const MAX_FILE_BYTES: u64 = 512 * 1024;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route(PATH, get(get_diagrams))
}

pub fn diagrams_dir() -> PathBuf {
    std::env::var_os("GS_DIAGRAMS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("layout/diagrams"))
}

#[derive(Serialize, Debug, PartialEq)]
pub struct FileError {
    pub file: String,
    pub error: String,
}

#[derive(Serialize, Debug, Default)]
pub struct Reply {
    pub diagrams: Vec<Value>,
    pub errors: Vec<FileError>,
}

fn ids<'a>(value: &'a Value, list: &str) -> Vec<&'a str> {
    value[list]
        .as_array()
        .map(|items| items.iter().filter_map(|i| i["id"].as_str()).collect())
        .unwrap_or_default()
}

/// The structural rules the station can check without the client's renderer:
/// schema, id = file stem, unique ids, and every reference resolving.
pub fn check(stem: &str, value: &Value) -> Result<(), String> {
    if value["schema"].as_u64() != Some(SCHEMA) {
        return Err(format!("schema must be {SCHEMA}"));
    }
    if value["id"].as_str() != Some(stem) {
        return Err(format!("id must equal the file name `{stem}`"));
    }
    if value["title"].as_str().is_none_or(|t| t.trim().is_empty()) {
        return Err("title is required".into());
    }
    let mut seen = BTreeSet::new();
    for list in ["fluids", "frames", "nodes", "valves", "edges", "readings"] {
        for id in ids(value, list) {
            if !seen.insert(id) {
                return Err(format!("duplicate id `{id}`"));
            }
        }
    }
    let nodes: BTreeSet<&str> = ids(value, "nodes").into_iter().collect();
    let valves: BTreeSet<&str> = ids(value, "valves").into_iter().collect();
    for edge in value["edges"].as_array().into_iter().flatten() {
        let id = edge["id"].as_str().unwrap_or("?");
        for end in ["from", "to"] {
            let node = edge[end].as_str().unwrap_or("");
            if !nodes.contains(node) {
                return Err(format!("edge `{id}`: unknown node `{node}`"));
            }
        }
        for valve in edge["through"].as_array().into_iter().flatten() {
            let valve = valve.as_str().unwrap_or("");
            if !valves.contains(valve) {
                return Err(format!("edge `{id}`: unknown valve `{valve}`"));
            }
        }
        if edge["points"].as_array().is_none_or(|p| p.len() < 2) {
            return Err(format!("edge `{id}`: needs at least two points"));
        }
    }
    Ok(())
}

pub fn load(dir: &Path) -> Reply {
    let mut reply = Reply::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return reply;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    for path in files {
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let stem = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parsed = std::fs::metadata(&path)
            .map_err(|e| e.to_string())
            .and_then(|m| if m.len() > MAX_FILE_BYTES { Err("file too large".into()) } else { Ok(()) })
            .and_then(|()| std::fs::read(&path).map_err(|e| e.to_string()))
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string()))
            .map(|mut value| {
                crate::layout::canonicalize_legacy_type_names(&mut value);
                value
            })
            .and_then(|value| check(&stem, &value).map(|()| value));
        match parsed {
            Ok(value) => reply.diagrams.push(value),
            Err(error) => reply.errors.push(FileError { file, error }),
        }
    }
    reply
}

async fn get_diagrams(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let dir = diagrams_dir();
    let reply = tokio::task::spawn_blocking(move || load(&dir)).await.unwrap_or_default();
    Json(reply).into_response()
}

/// Data types the shipped diagrams read, refreshed at most every 10 s.
fn referenced_types() -> BTreeSet<String> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, BTreeSet<String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    if let Some((at, types)) = cache.as_ref()
        && at.elapsed() < Duration::from_secs(10)
    {
        return types.clone();
    }
    let mut types = BTreeSet::new();
    fn walk(value: &Value, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(t)) = map.get("telemetry") {
                    out.insert(t.clone());
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    for diagram in load(&diagrams_dir()).diagrams {
        walk(&diagram, &mut types);
    }
    *cache = Some((Instant::now(), types.clone()));
    types
}

/// "diagram-fill" -> the `fill` topology.
pub fn scene_topology(scene: &str) -> Option<Value> {
    let id = scene.strip_prefix("diagram-")?;
    load(&diagrams_dir()).diagrams.into_iter().find(|d| d["id"].as_str() == Some(id))
}

pub fn is_scene(scene: &str) -> bool {
    scene.strip_prefix("diagram-").is_some_and(|id| {
        (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

/// Drogue/main text, the same rule as the dashboard
/// (frontend `live_diagrams::pyro_text`): deployment from the FC phase, armed
/// from the network flight state, DRY-RUN from the ground-test report.
pub fn pyro_text(main: bool, flight_state: &str, fc_phase: Option<&str>, dry_run: Option<bool>) -> String {
    let phase = fc_phase.unwrap_or("");
    let fired_phases: &[&str] = if main { &["Reef", "Landed", "Recovery"] } else { &["Descent", "Reef", "Landed", "Recovery"] };
    let base = if fired_phases.iter().any(|p| phase.contains(p)) {
        format!("Fired (FC phase {})", phase.trim_end_matches(" (SIMULATED)"))
    } else if matches!(flight_state, "Launch" | "Ascent" | "Coast" | "Apogee" | "Descent" | "InFlight" | "Reefing") {
        "Armed by the FC in flight".into()
    } else {
        "Safe (FC arms after Launch)".into()
    };
    if dry_run == Some(true) { format!("{base} · DRY-RUN") } else { base }
}

/// The diagram inputs captured into each programme snapshot (only while a
/// diagram scene is selected), so the audience diagram is delayed exactly
/// like the video: latest rows of the referenced types with their age, board
/// freshness, valve controls (confirmed and requested) and station statuses.
pub fn program_inputs(app: &AppState, now: u64) -> Value {
    let types = referenced_types();
    let mut rows: Vec<Value> = Vec::new();
    {
        let mut seen = BTreeSet::new();
        let cache = app.recent_telemetry_cache.lock().unwrap();
        for row in cache.iter().rev() {
            let Some(age) = u64::try_from(row.timestamp_ms).ok().and_then(|at| now.checked_sub(at)) else {
                continue;
            };
            if age > 120_000 {
                break;
            }
            if types.contains(&row.data_type) && seen.insert((row.data_type.clone(), row.sender_id.clone())) {
                let mut entry = serde_json::json!({"t": row.data_type, "s": row.sender_id, "v": row.values, "age_ms": age});
                // ADC over range: value i is null in "v" and bit i set here
                // (same masks as TelemetryRow over_range/over_range_low).
                if row.over_range != 0 {
                    entry["over_range"] = row.over_range.into();
                    if row.over_range_low != 0 {
                        entry["over_range_low"] = row.over_range_low.into();
                    }
                }
                rows.push(entry);
            }
        }
    }
    let boards: Vec<Value> = app
        .board_status_snapshot(now)
        .boards
        .iter()
        .map(|b| serde_json::json!({"s": b.sender_id, "seen": b.seen, "age_ms": b.age_ms}))
        .collect();
    let controls: Vec<Value> = app
        .action_policy_snapshot()
        .controls
        .iter()
        .filter(|c| c.actuated.is_some() || c.requested.is_some() || c.progress.is_some())
        .map(|c| {
            let running = c.progress.as_ref().is_some_and(|p| {
                matches!(serde_json::to_value(&p.state).ok().and_then(|v| v.as_str().map(str::to_owned)).as_deref(), Some("starting" | "running"))
            });
            serde_json::json!({"cmd": c.cmd, "actuated": c.actuated, "requested": c.requested, "running": running})
        })
        .collect();
    let wire = crate::web::FlightStateWire::new(*app.state.lock().unwrap(), app);
    let network = format!("{:?}", wire.state);
    let fc_phase = wire.fc_phase.as_ref().map(|p| {
        if wire.simulated { format!("{} (SIMULATED)", p.label) } else { p.label.clone() }
    });
    let igt = app.igt.status();
    let igt_active = igt.igt_active;
    let dry = |main: bool| (igt.supported && igt_active).then_some(if main { igt.main_dry_run } else { igt.drogue_dry_run });
    let estimator = app.fc_estimator.snapshot();
    let provenance = if crate::raw_stream::station_simulated() {
        "SIMULATED".to_string()
    } else if igt_active && app.igt.simulated() {
        "IGT: FC flight data SIMULATED (replay)".into()
    } else if igt_active {
        "IGT: live fill and stand".into()
    } else {
        "Live".into()
    };
    let topology: serde_json::Map<String, Value> = app
        .network_topology_snapshot(now)
        .nodes
        .iter()
        .filter_map(|n| {
            let sender = n.sender_id.clone()?;
            Some((sender, serde_json::json!({"online": n.status != crate::types::NetworkTopologyStatus::Offline, "age_ms": n.age_ms})))
        })
        .collect();
    serde_json::json!({
        "rows": rows,
        "boards": boards,
        "controls": controls,
        "status": {
            "igt": igt_active,
            "flight_state": wire.label,
            "fill_phase": wire.fill_phase_label,
            "fc_phase": fc_phase,
            "estimator": (!estimator.label.is_empty()).then(|| estimator.label.trim_start_matches("FC estimator: ").to_string()),
            "estimator_rate_hz": estimator.rate_hz.map(|r| format!("{r:.0} cycles/s")),
            "igt_mode": igt_active.then(|| format!("{} · {}", igt.fc_mode.label(), if igt.live_pyro { "LIVE PYRO" } else { "pyro DRY-RUN" })),
            "pyro_drogue": pyro_text(false, &network, fc_phase.as_deref(), dry(false)),
            "pyro_main": pyro_text(true, &network, fc_phase.as_deref(), dry(true)),
            "fc_sd": igt.sd_ready.map(|ready| if ready { "Ready" } else if igt.sd_retry_pending { "Not ready (retry pending)" } else { "Not ready" }),
            "provenance": provenance,
            "topology": topology,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_names_and_pyro_text() {
        assert!(is_scene("diagram-fill") && is_scene("diagram-av_bay"));
        assert!(!is_scene("diagram-") && !is_scene("diagram-../x") && !is_scene("fill"));
        assert_eq!(scene_topology("diagram-stand").unwrap()["id"], "stand");
        assert!(scene_topology("diagram-missing").is_none());
        assert_eq!(pyro_text(false, "Fill", None, None), "Safe (FC arms after Launch)");
        assert_eq!(pyro_text(true, "InFlight", Some("Reefing (SIMULATED)"), Some(true)), "Fired (FC phase Reefing) · DRY-RUN");
        assert!(referenced_types().contains("VALVE_STATE"));
    }

    #[tokio::test]
    async fn program_inputs_carry_the_latest_referenced_rows_with_ages_and_statuses() {
        let state = crate::state::tests::test_app_state().await;
        let now = crate::telemetry_task::get_current_timestamp_ms();
        for (data_type, at) in [("VALVE_STATE", now - 3_000), ("VALVE_STATE", now - 1_000), ("NOT_IN_A_DIAGRAM", now)] {
            state.cache_recent_telemetry(crate::types::TelemetryRow {
                timestamp_ms: at as i64,
                data_type: data_type.into(),
                sender_id: "VB".into(),
                values: vec![Some(1.0), Some(0.0)], over_range: 0, over_range_low: 0,
            });
        }
        // An over-range chamber PT reaches the diagram as OVR, not a number.
        state.cache_recent_telemetry(crate::types::TelemetryRow::marked(
            now as i64 - 500,
            "COMBUSTION_CHAMBER_PRESSURE".into(),
            "DAQ".into(),
            vec![Some(f32::INFINITY)],
        ));
        let inputs = program_inputs(&state, now);
        let rows = inputs["rows"].as_array().unwrap();
        let chamber = rows.iter().find(|r| r["t"] == "COMBUSTION_CHAMBER_PRESSURE").expect("chamber row");
        assert_eq!((chamber["v"].clone(), chamber["over_range"].clone()), (serde_json::json!([null]), serde_json::json!(1)));
        assert!(chamber.get("over_range_low").is_none());
        let rows: Vec<_> = rows.iter().filter(|r| r["t"] != "COMBUSTION_CHAMBER_PRESSURE").collect();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["t"], "VALVE_STATE");
        assert_eq!(rows[0]["age_ms"], 1_000, "the newest row wins");
        for key in ["flight_state", "pyro_drogue", "pyro_main", "provenance", "topology"] {
            assert!(!inputs["status"][key].is_null(), "{key}");
        }
        assert_eq!(inputs["status"]["igt"], false);
    }

    #[test]
    fn shipped_topologies_load_without_errors() {
        let reply = load(&diagrams_dir());
        assert!(reply.errors.is_empty(), "{:?}", reply.errors);
        let ids: Vec<&str> = reply.diagrams.iter().filter_map(|d| d["id"].as_str()).collect();
        assert_eq!(ids, ["av_bay", "fill", "network", "stand"]);
    }

    #[test]
    fn broken_references_and_mismatched_ids_are_reported_not_served() {
        let dir = std::env::temp_dir().join(format!("gs26-diagrams-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fill: Value = serde_json::from_slice(&std::fs::read(diagrams_dir().join("fill.json")).unwrap()).unwrap();
        let mut broken = fill.clone();
        broken["edges"][0]["through"] = serde_json::json!(["nope"]);
        std::fs::write(dir.join("fill.json"), serde_json::to_vec(&broken).unwrap()).unwrap();
        std::fs::write(dir.join("renamed.json"), serde_json::to_vec(&fill).unwrap()).unwrap();
        std::fs::write(dir.join("garbage.json"), b"{").unwrap();
        let reply = load(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(reply.diagrams.is_empty());
        let errors: Vec<&str> = reply.errors.iter().map(|e| e.error.as_str()).collect();
        assert_eq!(reply.errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("unknown valve `nope`")));
        assert!(errors.iter().any(|e| e.contains("id must equal the file name `renamed`")));
    }
}
