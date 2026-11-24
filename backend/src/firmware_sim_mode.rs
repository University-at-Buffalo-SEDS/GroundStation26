//! Firmware simulation (SITL) mode: this station runs against the real firmware of every board, compiled for the
//! host by FirmwareSimulator's native engine, with a simulated plant and flight (docs/firmware-sim.md).
//!
//! Like Simulation, it is a start-up property. The station mode supervisor (`station_modes.py`) starts the native
//! bay (`tools/firmware_sim.py` cache, FirmwareSimulator `scripts/gs_bay.py`) before the backend and passes:
//!
//! - `GS_FIRMWARE_SIM=1` and `GS_FIRMWARE_SIM_DIR` (the bay's run directory: `status.json`, `control.json`,
//!   `endpoints.json`, board logs);
//! - the bay's endpoints as the simulator serial overrides (`GS_SIMULATED_SERIAL_PTY`, `GS_AV_BAY_SERIAL_PORT`,
//!   `GS_FILL_PICO_DISCOVERY_PORT`) and a comms config of its own, so no physical link is ever opened;
//! - isolated state, configuration and recording paths under the firmware-sim root, so simulated launches,
//!   calibrations and proofs never reach the hardware installation.
//!
//! The mode is never entered while real hardware is connected: the switch into it refuses while any board was heard
//! recently on this (hardware) station ([`hardware_connected`]). Raspberry Pi builds and RRLS stations do not offer
//! it (the boards are built and simulated on a development machine).
use crate::{auth::{Permission, StreamRole}, state::AppState, types::Board};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub(crate) const ENV: &str = "GS_FIRMWARE_SIM";
/// DAC validation targets (daq_dac::model) the simulated plant can patch the DAQ's DAC output onto.
pub(crate) const PATCH_TARGETS: [&str; 9] =
    ["mcp_ch2", "mcp_ch3", "mcp_ch4", "mcp_ch5", "hvain1", "hvain2", "p6_diff", "loadcell_kg1000", "loadcell_kg50"];
pub(crate) const DIR_ENV: &str = "GS_FIRMWARE_SIM_DIR";
/// A board heard within this long means hardware is attached to this station.
pub(crate) const HARDWARE_RECENT: Duration = Duration::from_secs(30);

/// This process talks to simulated firmware (fixed at start).
pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var(ENV).as_deref() == Ok("1"))
}

/// Set by the supervisor (`station_modes.py`) when the bay publishes its virtual clock and the backend preloads
/// FirmwareSimulator's simclock to follow it.
pub(crate) const CLOCK_ENV: &str = "GS_STATION_CLOCK";
pub(crate) const CLOCK_BAY: &str = "firmware_sim_bay";

/// The clock this process runs on: `firmware_sim_bay` (every clock read and timed wait follows the simulated bay's
/// virtual clock, so network time and report freshness are judged on the boards' own time base and a bay slowed by
/// host load slows the station with it) or `wall`.
pub(crate) fn clock_source() -> &'static str {
    clock_source_for(enabled(), std::env::var(CLOCK_ENV).ok().as_deref(), std::env::var("SIMCLOCK_FILE").ok().as_deref())
}

pub(crate) fn clock_source_for(firmware_sim: bool, declared: Option<&str>, simclock_file: Option<&str>) -> &'static str {
    if firmware_sim && declared == Some(CLOCK_BAY) && simclock_file.is_some_and(|f| !f.is_empty()) { CLOCK_BAY } else { "wall" }
}

/// Why this process must not start: a simulated clock outside firmware simulation (a preload or page inherited by
/// a hardware station). Only the firmware-sim start may follow the bay's clock; every other mode is on the wall.
pub(crate) fn foreign_clock_reason(firmware_sim: bool, simclock_file: Option<&str>, ld_preload: Option<&str>) -> Option<String> {
    if firmware_sim {
        return None;
    }
    let preloaded = ld_preload.is_some_and(|p| p.split([' ', ':']).any(|lib| lib.rsplit('/').next().is_some_and(|n| n.contains("simclock"))));
    let page = simclock_file.is_some_and(|f| !f.is_empty());
    (preloaded || page).then(|| {
        "the simulated-firmware station clock (simclock) is set but this station is not in firmware simulation; \
         refusing to run a hardware station on a simulated clock"
            .to_string()
    })
}

/// Startup check: exit before touching any link when [`foreign_clock_reason`] applies.
pub(crate) fn refuse_foreign_clock() {
    if let Some(reason) = foreign_clock_reason(
        enabled(),
        std::env::var("SIMCLOCK_FILE").ok().as_deref(),
        std::env::var("LD_PRELOAD").ok().as_deref(),
    ) {
        log::error!("{reason}");
        eprintln!("{reason}");
        std::process::exit(2);
    }
}

/// Recording provenance for captures and exports.
pub(crate) fn provenance() -> &'static str {
    if enabled() {
        "firmware_sim"
    } else if crate::flight_sim::sim_mode_enabled() {
        "internal_flight_sim"
    } else {
        "hardware"
    }
}

fn run_dir() -> Option<PathBuf> {
    std::env::var_os(DIR_ENV).map(PathBuf::from)
}

/// The firmware-sim build cache (tools/firmware_sim.py).
pub(crate) fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GS_FIRMWARE_SIM_CACHE") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache").join("seds-firmware-sim"))
        .unwrap_or_else(|| PathBuf::from(".cache/seds-firmware-sim"))
}

pub(crate) fn manifest() -> Option<Value> {
    serde_json::from_slice(&std::fs::read(cache_dir().join("manifest.json")).ok()?).ok()
}

/// Why this station cannot offer firmware simulation at all (None: it can).
pub(crate) fn unavailable_reason(rrls: bool) -> Option<String> {
    if rrls {
        return Some("Firmware simulation runs on the primary station only.".into());
    }
    let manifest = manifest();
    if cfg!(feature = "raspberry_pi")
        && manifest.as_ref().and_then(|m| m.get("host_abi")).and_then(Value::as_str) != Some("armhf")
    {
        return Some("This Raspberry Pi station needs an armhf firmware-sim bundle (docs/firmware-sim.md, Raspberry Pi).".into());
    }
    match manifest {
        Some(m)
            if m.get("boards").and_then(Value::as_object).is_some_and(|b| {
                ["flight", "rf", "power", "gateway", "actuator", "valve", "daq"].iter().all(|n| b.contains_key(*n))
            }) =>
        {
            None
        }
        Some(_) => Some("The firmware simulation cache is incomplete. Run: python3 tools/firmware_sim.py build".into()),
        None => Some("No firmware simulation build on this machine. Run: python3 tools/firmware_sim.py build".into()),
    }
}

/// Boards heard within `recent` on this station's links, newest first: real hardware, unless this process is
/// itself the firmware simulation.
pub(crate) fn boards_heard(state: &AppState, recent: Duration) -> Vec<(Board, Duration)> {
    let status = state.board_status.lock().unwrap_or_else(|e| e.into_inner());
    let mut heard: Vec<(Board, Duration)> = status
        .iter()
        .filter(|(board, _)| **board != Board::GroundStation)
        .filter_map(|(board, s)| s.last_seen_instant.map(|at| (*board, at.elapsed())))
        .filter(|(_, age)| *age <= recent)
        .collect();
    heard.sort_by_key(|(_, age)| *age);
    heard
}

/// Why entering firmware simulation must be refused now: real boards are talking to this station.
pub(crate) fn hardware_connected(state: &AppState) -> Option<String> {
    if enabled() {
        return None;
    }
    let heard = boards_heard(state, HARDWARE_RECENT);
    if heard.is_empty() {
        return None;
    }
    let names: Vec<String> = heard
        .iter()
        .map(|(board, age)| format!("{} {:.0} s ago", board.as_str(), age.as_secs_f64()))
        .collect();
    Some(format!(
        "Real hardware is connected: {} heard on this station's links ({}). Firmware simulation is never entered while boards are attached; disconnect them and wait {} s.",
        if heard.len() == 1 { "a board was" } else { "boards were" },
        names.join(", "),
        HARDWARE_RECENT.as_secs()
    ))
}

fn read_json(path: PathBuf) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Where the committed IGT replay datasets live (`GS_IGT_DATA_DIR`, else
/// `test-data/igt`).
pub(crate) fn igt_data_dir() -> PathBuf {
    std::env::var_os("GS_IGT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test-data/igt"))
}

/// IGT replay datasets (`test-data/igt/<set>/IGT/*.CSV`) the simulated FC SD card can carry.
pub(crate) fn igt_datasets() -> Vec<String> {
    let dir = igt_data_dir();
    let mut sets: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("IGT").is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    sets.sort();
    sets
}

/// The current plant control (conditions, loads, faults) for recording metadata.
pub(crate) fn current_control() -> Option<Value> {
    run_dir().and_then(|d| read_json(d.join("control.json")))
}

/// `GET /api/firmware-sim`: whether this station runs simulated firmware, the firmware revisions, the bay's
/// pace, nodes and plant, and the current control (plant inputs and faults).
pub(crate) fn status_json() -> Value {
    let dir = run_dir();
    let status = dir.as_ref().and_then(|d| read_json(d.join("status.json")));
    let age_ms = status
        .as_ref()
        .and_then(|s| s.get("updated_unix_ms").and_then(Value::as_u64))
        .map(|at| (crate::telemetry_task::get_current_timestamp_ms()).saturating_sub(at));
    json!({
        "active": enabled(),
        "banner": enabled().then_some("SIMULATED FIRMWARE"),
        "provenance": provenance(),
        "available_reason": if enabled() { None } else { unavailable_reason(crate::rrls::is_rrls()) },
        "manifest": manifest(),
        "bay": status,
        "bay_status_age_ms": age_ms,
        "bay_running": age_ms.is_some_and(|age| age < 5_000),
        "station_clock": clock_source(),
        "control": dir.as_ref().and_then(|d| read_json(d.join("control.json"))).unwrap_or_else(|| json!({})),
        "igt_datasets": igt_datasets(),
        "sd_card": dir.as_ref().and_then(|d| read_json(d.join("sdcard.json"))),
        "bay_refused": dir.as_ref().and_then(|d| std::fs::read_to_string(d.join("refused.txt")).ok()),
        "run_dir": dir.map(|d| d.display().to_string()),
    })
}

/// Validated plant/fault control document (unknown keys refused).
pub(crate) fn validate_control(body: &Value) -> Result<Value, String> {
    let obj = body.as_object().ok_or("control must be an object")?;
    let mut out = serde_json::Map::new();
    for (key, value) in obj {
        match key.as_str() {
            "plant" => {
                let plant = value.as_object().ok_or("plant must be an object")?;
                for (k, v) in plant {
                    if !matches!(k.as_str(), "qd_connected" | "nitrogen_bottle_open" | "nitrous_bottle_open") || !v.is_boolean() {
                        return Err(format!("unknown or non-boolean plant input {k}"));
                    }
                }
                out.insert(key.clone(), value.clone());
            }
            "faults" => {
                let faults = value.as_object().ok_or("faults must be an object")?;
                for (k, v) in faults {
                    match k.as_str() {
                        "dead_tank_pt" | "dead_chamber_pt" | "radio_drop" | "kg1000_supply_dip" | "kg1000_open_lead"
                        | "kg50_open_lead" | "p10_inputs_swapped" | "fc_sd_missing" | "gps_no_fix"
                            if v.is_boolean() => {}
                        "kg50_bypass" | "kg1000_bypass" if v.as_f64().is_some_and(|x| (0.0..=1.0).contains(&x)) => {}
                        "kg50_drift_kg_per_c" | "kg1000_drift_kg_per_c" if v.as_f64().is_some_and(|x| x.abs() <= 5.0) => {}
                        "stuck_valves" => {
                            let valves = v.as_object().ok_or("stuck_valves must be an object")?;
                            for (name, state) in valves {
                                if !matches!(name.as_str(), "nitrogen" | "nitrous" | "vent" | "dump" | "pilot")
                                    || !matches!(state.as_str(), Some("open" | "closed"))
                                {
                                    return Err(format!("stuck valve {name} must be nitrogen|nitrous|vent|dump|pilot = open|closed"));
                                }
                            }
                        }
                        _ => return Err(format!("unknown fault {k}")),
                    }
                }
                out.insert(key.clone(), value.clone());
            }
            "conditions" => {
                let c = value.as_object().ok_or("conditions must be an object")?;
                for (k, v) in c {
                    if k == "wind_levels" {
                        // winds aloft (the apogee planner's weather snapshot)
                        let levels = v.as_array().ok_or("wind_levels must be a list")?;
                        if levels.len() > 64
                            || !levels.iter().all(|l| {
                                l.get("alt_m").and_then(Value::as_f64).is_some_and(|x| (-500.0..=30_000.0).contains(&x))
                                    && l.get("speed_mps").and_then(Value::as_f64).is_some_and(|x| (0.0..=80.0).contains(&x))
                                    && l.get("from_deg").and_then(Value::as_f64).is_some_and(|x| (0.0..=360.0).contains(&x))
                            })
                        {
                            return Err("wind_levels: up to 64 of {alt_m, speed_mps 0-80, from_deg 0-360}".into());
                        }
                        continue;
                    }
                    let x = v.as_f64().ok_or_else(|| format!("condition {k} must be a number"))?;
                    let ok = match k.as_str() {
                        "ambient_c" => (-30.0..=50.0).contains(&x),
                        "wind_mps" => (0.0..=40.0).contains(&x),
                        "wind_from_deg" => (0.0..=360.0).contains(&x),
                        "gust_mps" => (0.0..=20.0).contains(&x),
                        _ => return Err(format!("unknown condition {k}")),
                    };
                    if !ok {
                        return Err(format!("condition {k} out of range"));
                    }
                }
                out.insert(key.clone(), value.clone());
            }
            "igt" => {
                let i = value.as_object().ok_or("igt must be an object")?;
                for (k, v) in i {
                    match (k.as_str(), v) {
                        ("dataset", Value::Null) => {}
                        ("dataset", Value::String(name)) if igt_datasets().iter().any(|d| d == name) => {}
                        ("dataset", _) => return Err(format!("unknown IGT dataset (have: {})", igt_datasets().join(", "))),
                        _ => return Err(format!("unknown igt key {k}")),
                    }
                }
                out.insert(key.clone(), value.clone());
            }
            "patch" => {
                // DAQ DAC output -> DAQ input (the DAC validation targets the plant can route)
                let p = value.as_object().ok_or("patch must be an object")?;
                for (k, v) in p {
                    match k.as_str() {
                        "dac1" | "dac2" => {
                            if !(v.is_null() || v.as_str().is_some_and(|t| PATCH_TARGETS.contains(&t))) {
                                return Err(format!("{k} must be null or one of {}", PATCH_TARGETS.join(", ")));
                            }
                        }
                        "dac1_attenuation" | "dac2_attenuation" => {
                            if !v.as_f64().is_some_and(|x| x > 0.0 && x <= 1.0) {
                                return Err(format!("{k} must be in (0, 1]"));
                            }
                        }
                        _ => return Err(format!("unknown patch key {k}")),
                    }
                }
                out.insert(key.clone(), value.clone());
            }
            "loads" => {
                let l = value.as_object().ok_or("loads must be an object")?;
                for (k, v) in l {
                    let x = v.as_f64().ok_or_else(|| format!("load {k} must be a number"))?;
                    let max = match k.as_str() {
                        "kg50_kg" => 60.0,
                        "kg1000_kg" => 1200.0,
                        _ => return Err(format!("unknown load cell {k}")),
                    };
                    if !(0.0..=max).contains(&x) {
                        return Err(format!("load {k} out of range"));
                    }
                }
                out.insert(key.clone(), value.clone());
            }
            _ => return Err(format!("unknown control section {key}")),
        }
    }
    Ok(Value::Object(out))
}

/// Replaces the plant `conditions` (ambient, wind) in the run's control file, keeping its other sections; used to
/// rehearse an apogee plan's weather (docs/apogee-planner.md section 6). The caller checks the permission.
pub(crate) fn set_conditions(conditions: Value) -> Result<Value, String> {
    if !enabled() {
        return Err("This station is not running simulated firmware.".into());
    }
    let dir = run_dir().ok_or("No firmware simulation run directory.")?;
    let mut control = read_json(dir.join("control.json")).unwrap_or_else(|| json!({}));
    control
        .as_object_mut()
        .ok_or("The current control file is not an object.")?
        .insert("conditions".into(), conditions);
    let control = validate_control(&control)?;
    write_control(&dir, &control).map_err(|e| format!("could not write the control file: {e}"))?;
    Ok(control)
}

fn write_control(dir: &std::path::Path, control: &Value) -> std::io::Result<()> {
    let tmp = dir.join("control.json.pending");
    std::fs::write(&tmp, serde_json::to_vec_pretty(control).unwrap_or_default())
        .and_then(|_| std::fs::rename(&tmp, dir.join("control.json")))
}

/// Top-level sections of `update` replace those of `current`.
pub(crate) fn merge_control(current: Option<Value>, update: Value) -> Value {
    let mut merged = current.filter(Value::is_object).unwrap_or_else(|| json!({}));
    if let (Some(m), Some(u)) = (merged.as_object_mut(), update.as_object()) {
        for (k, v) in u {
            m.insert(k.clone(), v.clone());
        }
    }
    merged
}

async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(status_json()).into_response()
}

async fn post_control(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if principal.anonymous || !principal.roles.contains(&StreamRole::Admin) {
        return (StatusCode::FORBIDDEN, "Firmware simulation controls need an administrator.").into_response();
    }
    if !enabled() {
        return (StatusCode::CONFLICT, "This station is not running simulated firmware.").into_response();
    }
    let control = match validate_control(&body) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let Some(dir) = run_dir() else {
        return (StatusCode::CONFLICT, "No firmware simulation run directory.").into_response();
    };
    // sections replace the current ones; sections not sent are kept (the DAC page sends only "patch", the
    // apogee planner only "conditions")
    let control = merge_control(read_json(dir.join("control.json")), control);
    match write_control(&dir, &control) {
        Ok(()) => {
            log::warn!("Firmware simulation control set by {}: {control}", principal.username.as_deref().unwrap_or("?"));
            Json(json!({"control": control})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not write the control file: {e}")).into_response(),
    }
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/firmware-sim", get(get_status))
        .route("/api/firmware-sim/control", post(post_control))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outside firmware simulation the station is on the wall clock, whatever the environment says; a simclock
    /// preload or page there refuses the start.
    #[test]
    fn only_firmware_simulation_follows_the_bay_clock() {
        assert_eq!(clock_source_for(false, None, None), "wall");
        assert_eq!(clock_source_for(false, Some(CLOCK_BAY), Some("/dev/shm/x/station-clock.page")), "wall");
        assert_eq!(clock_source_for(true, None, None), "wall", "a bay without a clock page: wall");
        assert_eq!(clock_source_for(true, Some(CLOCK_BAY), Some("/dev/shm/x/station-clock.page")), CLOCK_BAY);
        assert_eq!(foreign_clock_reason(false, None, None), None);
        assert_eq!(foreign_clock_reason(false, None, Some("/usr/lib/libjemalloc.so")), None);
        assert!(foreign_clock_reason(false, None, Some("/cache/bin/simclock.so")).is_some());
        assert!(foreign_clock_reason(false, Some("/dev/shm/page"), None).is_some());
        assert_eq!(foreign_clock_reason(true, Some("/dev/shm/page"), Some("/cache/bin/simclock.so")), None);
        // this test process itself: not firmware sim, so the wall clock
        assert_eq!(clock_source(), "wall");
        let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        assert!(crate::telemetry_task::get_current_timestamp_ms().abs_diff(wall) < 5_000);
    }

    #[test]
    fn control_sections_merge() {
        let merged = merge_control(Some(json!({"patch": {"dac1": "hvain1"}, "loads": {"kg50_kg": 5.0}})), json!({"loads": {"kg50_kg": 10.0}}));
        assert_eq!(merged, json!({"patch": {"dac1": "hvain1"}, "loads": {"kg50_kg": 10.0}}));
    }

    #[tokio::test]
    async fn real_boards_heard_recently_refuse_firmware_simulation() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let state = export_state(&root.0, false).await;
        assert_eq!(hardware_connected(&state), None, "no board heard: no hardware");
        let set = |board: Board, ago: Duration| {
            let mut status = state.board_status.lock().unwrap();
            status.get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now() - ago);
        };
        set(Board::GroundStation, Duration::ZERO);
        assert_eq!(hardware_connected(&state), None, "the station itself is not hardware");
        set(Board::ValveBoard, HARDWARE_RECENT + Duration::from_secs(5));
        assert_eq!(hardware_connected(&state), None, "heard long ago");
        set(Board::FlightComputer, Duration::from_secs(2));
        let reason = hardware_connected(&state).expect("a board heard 2 s ago is attached hardware");
        assert!(reason.contains("Flight Computer") && !reason.contains("Valve Board"), "{reason}");
    }

    #[test]
    fn control_validation_accepts_plant_inputs_and_faults_only() {
        let ok = json!({"plant": {"qd_connected": false}, "faults": {"stuck_valves": {"vent": "closed"}, "dead_tank_pt": true,
                        "kg50_bypass": 0.2, "kg1000_supply_dip": true},
                        "conditions": {"ambient_c": 35.0, "wind_mps": 9.0, "wind_from_deg": 300.0,
                                       "wind_levels": [{"alt_m": 0.0, "speed_mps": 4.0, "from_deg": 300.0}, {"alt_m": 3000.0, "speed_mps": 15.0, "from_deg": 270.0}]},
                        "loads": {"kg50_kg": 10.0},
                        "patch": {"dac1": "hvain1", "dac2": null, "dac1_attenuation": 0.5}, "igt": {"dataset": "nominal_10k"}});
        assert_eq!(validate_control(&ok).unwrap(), ok);
        for bad in [
            json!({"plant": {"qd_connected": "no"}}),
            json!({"plant": {"tank_pressure_psi": 500}}),
            json!({"faults": {"stuck_valves": {"igniter": "open"}}}),
            json!({"faults": {"stuck_valves": {"vent": "half"}}}),
            json!({"faults": {"fire": true}}),
            json!({"flight": {"apogee": 1}}),
            json!({"conditions": {"ambient_c": 90.0}}),
            json!({"loads": {"kg50_kg": 500.0}}),
            json!({"faults": {"kg50_bypass": 2.0}}),
            json!({"patch": {"dac1": "sar1"}}),
            json!({"igt": {"dataset": "../../etc"}}),
            json!({"patch": {"dac1_attenuation": 0.0}}),
            json!([1]),
        ] {
            assert!(validate_control(&bad).is_err(), "{bad}");
        }
    }
}
