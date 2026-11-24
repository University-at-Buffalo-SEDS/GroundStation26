//! Cross-repo contract fixtures.
//!
//! Serializes real backend HTTP responses and websocket frames, produced by
//! the actual handlers and types, into `backend/tests/contract-fixtures/`.
//! The UI (Seds-Ground-Station-Frontend) decodes the same files with its own
//! types, so a JSON shape change on either side fails a test instead of
//! shipping (for example the station mode challenge without `"version": 2`).
//!
//! The test fails when a checked-in fixture differs from what the backend now
//! produces. After an intended change, regenerate and commit the files:
//!
//! ```text
//! GS_UPDATE_CONTRACT_FIXTURES=1 cargo test -p groundstation_backend contract_fixtures
//! ```
//!
//! then copy them into the UI's snapshot (see `docs/backend/contract-fixtures.md`).
//!
//! Values that change from run to run (random tickets, boot epochs, clock
//! readings, process-wide counters) are replaced by fixed placeholders of the
//! same JSON type; everything else is exactly what the backend sent.
use crate::recording_export::tests::{TestDirectory, export_state};
use crate::runtime_mode::test_support::thread_mode;
use crate::state::AppState;
use crate::station_mode::Mode;
use crate::station_mode_switch::{Config, RestartSignal, Switch};
use crate::web::{AlertDto, WsOutMsg};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const UPDATE_ENV: &str = "GS_UPDATE_CONTRACT_FIXTURES";
/// Fixed clock reading used for alert events in the fixtures.
const T0: i64 = 1_767_225_600_000;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("contract-fixtures")
}

/// Collects fixtures by relative path, then checks or rewrites them all.
#[derive(Default)]
struct Fixtures(BTreeMap<String, Value>);
impl Fixtures {
    fn add(&mut self, name: &str, value: Value) {
        assert!(
            self.0.insert(format!("{name}.json"), value).is_none(),
            "duplicate fixture {name}"
        );
    }
    fn finish(self) {
        let dir = fixture_dir();
        let update = std::env::var(UPDATE_ENV).as_deref() == Ok("1");
        let mut stale = Vec::new();
        for (name, value) in &self.0 {
            let text = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
            let path = dir.join(name);
            if update {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                    std::fs::write(&path, text).unwrap();
                }
            } else if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                stale.push(format!("{name} differs from the backend's current output"));
            }
        }
        for file in json_files(&dir) {
            if !self.0.contains_key(&file) {
                if update {
                    std::fs::remove_file(dir.join(&file)).unwrap();
                } else {
                    stale.push(format!("{file} is no longer produced by the backend"));
                }
            }
        }
        assert!(
            stale.is_empty(),
            "contract fixtures are stale:\n  {}\nRegenerate with `{UPDATE_ENV}=1 cargo test -p groundstation_backend contract_fixtures`, \
             review the diff, then refresh the UI snapshot (docs/backend/contract-fixtures.md).",
            stale.join("\n  ")
        );
    }
}

fn json_files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "json") {
                let relative = path.strip_prefix(root).unwrap();
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Replaces every occurrence of each volatile text (whole value or inside a
/// longer string) with its placeholder.
fn replace_text(value: &mut Value, pairs: &[(String, &str)]) {
    match value {
        Value::String(text) => {
            for (from, to) in pairs {
                if !from.is_empty() && text.contains(from.as_str()) {
                    *text = text.replace(from.as_str(), to);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| replace_text(v, pairs)),
        Value::Object(map) => map.values_mut().for_each(|v| replace_text(v, pairs)),
        _ => {}
    }
}

/// Replaces the value of every field named in `keys` (at any depth) with a
/// fixed value of the same JSON type. Null stays null.
fn pin_fields(value: &mut Value, keys: &[&str]) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(|v| pin_fields(v, keys)),
        Value::Object(map) => {
            for (key, field) in map.iter_mut() {
                if keys.contains(&key.as_str()) {
                    match field {
                        Value::Number(n) if n.is_f64() => *field = json!(1.0),
                        Value::Number(_) => *field = json!(1),
                        // Keep the format the UI validates (hex ids, base64).
                        Value::String(text) => {
                            *field = if !text.is_empty() && text.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
                                json!("a5".repeat(text.len()).split_at(text.len()).0)
                            } else if key.ends_with("_b64") {
                                json!("Zml4dHVyZQ==")
                            } else {
                                json!(format!("fixture-{key}"))
                            }
                        }
                        Value::Bool(_) | Value::Null => {}
                        other => pin_fields(other, keys),
                    }
                } else {
                    pin_fields(field, keys);
                }
            }
        }
        _ => {}
    }
}

/// Users and signed-in sessions shared by every fixture request.
async fn station(root: &Path) -> Arc<AppState> {
    let state = export_state(root, false).await;
    std::fs::write(
        root.join("auth.json"),
        json!({"users":[
            {"username":"administrator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true},
             "command_access":{"allowed_commands":["Abort","StationMode"]}},
            {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"isolation","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true},
             "command_access":{"allowed_commands":["ValveSelfTest"]}}
        ]})
        .to_string(),
    )
    .unwrap();
    for name in ["administrator", "operator", "viewer", "isolation"] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
            .bind(format!("{name}-token"))
            .bind(name)
            .execute(&state.auth_db)
            .await
            .unwrap();
    }
    state
}

async fn call(
    app: axum::Router,
    method: &str,
    path: &str,
    user: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(user) = user {
        request = request.header("Authorization", format!("Bearer {user}-token"));
    }
    if body.is_some() {
        request = request.header("Content-Type", "application/json");
    }
    let request = request
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22).await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("{method} {path} returned non-JSON {status}: {}", String::from_utf8_lossy(&bytes)));
    (status, value)
}

async fn get_ok(app: &axum::Router, path: &str, user: Option<&str>) -> Value {
    let (status, value) = call(app.clone(), "GET", path, user, None).await;
    assert_eq!(status, StatusCode::OK, "GET {path}: {value}");
    value
}

/// Station mode V2: capability snapshot, review challenge, confirmation
/// receipt, the snapshot after the in-process switch, and the websocket
/// `StationModeChanged` frame.
async fn station_modes(root: &Path, state: &Arc<AppState>, out: &mut Fixtures) {
    let dir = root.join("station-mode");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("instance-id"), "contractfixture0001").unwrap();
    let config = Config {
        rrls: false,
        supervised: true,
        flight_sim: false,
        dir,
        simulation: false,
        flight_model: false,
        firmware: false,
        firmware_unavailable: None,
        restart_delay: Duration::ZERO,
        switch_delay: Duration::ZERO,
        station_resets: false,
        stand_in_links: || 0,
        hardware_connected: |_| None,
    };
    let switch = Arc::new(Switch::new(config, Arc::new(RestartSignal::default())));
    let app = crate::station_mode_switch::routes_with(Some(switch)).with_state(state.clone());
    let before = get_ok(&app, "/api/station-modes", Some("operator")).await;
    let binding = json!({
        "backend_instance": before["backend_instance"],
        "principal_context": before["principal_context"],
        "authority_id": before["authority_id"],
        "revision": before["revision"],
        "policy_epoch": before["policy_epoch"],
        "current_id": before["current"]["id"],
        "target_id": "hitl",
    });
    let (status, challenge) =
        call(app.clone(), "POST", "/api/station-modes/prepare", Some("operator"), Some(binding)).await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    // A Simulation change restarts instead of switching in place.
    let restart_binding = json!({
        "backend_instance": before["backend_instance"],
        "principal_context": before["principal_context"],
        "authority_id": before["authority_id"],
        "revision": before["revision"],
        "policy_epoch": before["policy_epoch"],
        "current_id": before["current"]["id"],
        "target_id": "testing-simulation",
    });
    let (status, restart_challenge) = call(
        app.clone(),
        "POST",
        "/api/station-modes/prepare",
        Some("operator"),
        Some(restart_binding),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{restart_challenge}");
    // The restart review replaced the in-place one; review again to confirm.
    let (status, challenge2) = call(
        app.clone(),
        "POST",
        "/api/station-modes/prepare",
        Some("operator"),
        Some(json!({
            "backend_instance": before["backend_instance"],
            "principal_context": before["principal_context"],
            "authority_id": before["authority_id"],
            "revision": before["revision"],
            "policy_epoch": before["policy_epoch"],
            "current_id": before["current"]["id"],
            "target_id": "hitl",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{challenge2}");
    let mut confirmation = challenge2.clone();
    let object = confirmation.as_object_mut().unwrap();
    for key in ["current", "target", "summary", "required_confirmation", "expires_in_ms"] {
        object.remove(key);
    }
    // Nothing is typed: Confirm sends the ticket and binding only.
    assert!(challenge2["required_confirmation"].is_null(), "{challenge2}");
    let (status, accepted) =
        call(app.clone(), "POST", "/api/station-modes/confirm", Some("operator"), Some(confirmation)).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let mut after = Value::Null;
    for _ in 0..200 {
        after = get_ok(&app, "/api/station-modes", Some("operator")).await;
        if after["transition"]["state"] != "pending" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(after["transition"]["state"], "applied", "{after}");
    let change = crate::station_mode_switch::last_change().expect("switch announced");
    let frame = serde_json::to_value(WsOutMsg::StationModeChanged(change)).unwrap();

    // Random per process or per review; keep links between files intact.
    let boot = |epoch: &Value| epoch.as_str().unwrap().split(':').next().unwrap().to_owned();
    let pairs = vec![
        (boot(&before["policy_epoch"]), "boot-fixture"),
        (challenge["ticket"].as_str().unwrap().to_owned(), "fixture-ticket-1"),
        (challenge["transition_id"].as_str().unwrap().to_owned(), "switch-fixture-1"),
        (restart_challenge["ticket"].as_str().unwrap().to_owned(), "fixture-ticket-2"),
        (restart_challenge["transition_id"].as_str().unwrap().to_owned(), "switch-fixture-2"),
        (challenge2["ticket"].as_str().unwrap().to_owned(), "fixture-ticket-3"),
        (challenge2["transition_id"].as_str().unwrap().to_owned(), "switch-fixture-3"),
    ];
    for (name, mut value) in [
        ("station-modes/snapshot", before),
        ("station-modes/prepare-in-process", challenge),
        ("station-modes/prepare-restart", restart_challenge),
        ("station-modes/confirm-accepted", accepted),
        ("station-modes/snapshot-after-switch", after),
        ("websocket/station-mode-changed", frame),
    ] {
        replace_text(&mut value, &pairs);
        // Process-wide counter bumped by every in-process switch.
        pin_fields(&mut value, &["layout_revision"]);
        out.add(name, value);
    }
}

/// `GET /api/vehicle/geometry`: the built-in design's drawn geometry (docs/vehicle-geometry.md).
async fn vehicle_geometry(app: &axum::Router, out: &mut Fixtures) {
    out.add("vehicle/geometry", get_ok(app, crate::vehicle_geometry::PATH, Some("viewer")).await);
}

/// `GET /api/station_mode` (operating mode metadata).
async fn operating_mode(app: &axum::Router, out: &mut Fixtures) {
    out.add("station-mode/status", get_ok(app, "/api/station_mode", Some("viewer")).await);
}

/// `GET /api/board-recovery/status` for a viewer and an operator.
async fn board_recovery(app: &axum::Router, out: &mut Fixtures) {
    for user in ["viewer", "operator"] {
        let mut value = get_ok(app, "/api/board-recovery/status", Some(user)).await;
        pin_fields(&mut value, &["generated_at_ms", "server_time_ms", "now_ms", "age_ms", "expires_at_ms", "instance", "backend_instance", "context"]);
        out.add(&format!("board-recovery/status-{user}"), value);
    }
}

/// The state-split flight state: `GET /flightstate` and the websocket
/// `FlightState` frame in a fill sub-phase (network Fill plus `fill_phase`)
/// and in flight (network InFlight plus the FC's detailed `fc_phase`).
async fn flight_state(app: &axum::Router, state: &Arc<AppState>, out: &mut Fixtures) {
    let before = state.local_flight_state_snapshot();
    *state.state.lock().unwrap() = crate::types::FlightState::NitrousFill;
    out.add("flight-state/get-fill", get_ok(app, "/flightstate", Some("viewer")).await);
    *state.state.lock().unwrap() = crate::types::FlightState::InFlight;
    crate::fc_phase::observe(state, crate::types::FlightState::Descent as u8, crate::fc_phase::Source::FcStateReport, 0);
    let mut frame = serde_json::to_value(WsOutMsg::FlightState(crate::web::FlightStateWire::new(
        state.local_flight_state_snapshot(), state))).unwrap();
    frame["data"]["fc_phase"]["age_ms"] = json!(0);
    out.add("websocket/flight-state-in-flight", frame);
    *state.state.lock().unwrap() = before;
}

/// `GET /api/action_policy` and the websocket `ActionPolicy` frame, with a
/// network-variable output that has a `network_note`.
async fn action_policy(app: &axum::Router, state: &Arc<AppState>, out: &mut Fixtures) {
    // Network-variable outputs read process-wide state that other tests
    // change; the fixture shows them as the station does before the network
    // has carried any value (the case that sets `network_note`).
    let view = crate::network_variables::NetworkOutputView { requested: true, confirmed: None, readback_seen: false };
    let note = crate::sequences::network_output_note(view);
    assert!(note.is_some());
    let mut policy = get_ok(app, "/api/action_policy", Some("viewer")).await;
    let mut network_outputs = 0;
    for control in policy["controls"].as_array_mut().unwrap() {
        if crate::sequences::network_output_view(control["cmd"].as_str().unwrap()).is_some() {
            control["requested"] = json!(view.requested);
            control["actuated"] = json!(view.confirmed);
            control["network_note"] = json!(note);
            network_outputs += 1;
        }
    }
    assert!(network_outputs > 0, "the policy lists network-variable outputs: {policy}");
    out.add("action-policy/get", policy);
    let mut snapshot = state.action_policy_snapshot();
    snapshot.controls.retain(|control| crate::sequences::network_output_view(&control.cmd).is_none());
    // A QD retraction the station sent and the actuator has not yet reported.
    if let Some(qd) = snapshot.controls.iter_mut().find(|control| control.cmd == "RetractPlumbing") {
        qd.progress = Some(crate::sequences::ControlProgress {
            state: crate::sequences::ControlProgressState::Running,
            confirmed: false,
            remaining_ms: Some(31_000),
            message: Some("Retraction command sent; running, not confirmed by the actuator.".into()),
        });
    }
    snapshot.controls.push(crate::sequences::ActionControl {
        disabled_reason: None,
        cmd: "ToggleAvBayUnderglow".into(),
        enabled: true,
        blink: crate::sequences::BlinkMode::None,
        actuated: view.confirmed,
        requested: Some(view.requested),
        network_note: note,
        progress: None,
    });
    out.add("websocket/action-policy", serde_json::to_value(WsOutMsg::ActionPolicy(snapshot)).unwrap());
}

/// Alerts: `GET /api/alerts`, `POST /api/alerts/ack`, the websocket
/// `AlertAckState` frame and `POST /api/alerts/history/clear`.
async fn alerts(app: &axum::Router, state: &Arc<AppState>, out: &mut Fixtures) {
    crate::alert_condition_runtime::restore_empty_for_test(state);
    state.clear_alert_history_through(T0);
    for (offset, severity) in [(1_000, "warning"), (2_000, "error")] {
        state.cache_recent_alert(AlertDto {
            timestamp_ms: T0 + offset,
            severity: severity.into(),
            message: format!("Contract fixture {severity}"),
            condition: None,
        });
    }
    // Cleared events are never returned.
    state.cache_recent_alert(AlertDto {
        timestamp_ms: T0 - 1_000,
        severity: "error".into(),
        message: "Cleared before the fixture".into(),
        condition: None,
    });
    out.add("alerts/list", get_ok(app, "/api/alerts?minutes=20", Some("viewer")).await);
    let (status, ack) = call(
        app.clone(),
        "POST",
        "/api/alerts/ack",
        Some("operator"),
        Some(json!({"warning_timestamp_ms": T0 + 1_000, "error_timestamp_ms": T0 + 2_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    out.add("alerts/ack", ack);
    out.add(
        "websocket/alert-ack-state",
        serde_json::to_value(WsOutMsg::AlertAckState(state.alert_ack_state_snapshot())).unwrap(),
    );
    let (status, mut cleared) = call(
        app.clone(),
        "POST",
        "/api/alerts/history/clear",
        Some("operator"),
        Some(json!({"confirm": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    pin_fields(&mut cleared, &["cleared_at_ms", "cleared_through_ms", "at_ms", "timestamp_ms"]);
    out.add("alerts/history-clear", cleared);
}

/// `/api/network_diagnostics` `radio_tx_budget`: the radio writer budget's own
/// diagnostics, as the route embeds them.
fn radio_tx_budget(out: &mut Fixtures) {
    let budget = crate::telemetry_task::TxQueueBudget::new(4096);
    let mut diagnostics = budget.diagnostics();
    // Ordered-exhaustion counts are process-wide, and other tests drive real
    // exhaustions through the router worker's queue service; the fixture
    // shows a station that has seen none.
    diagnostics["ordered_exhausted_by_type"] = json!({});
    diagnostics["unordered_exhausted_by_type"] = json!({});
    diagnostics["silent_owners_retired"] = json!({
        "retired_ordered": 0, "retired_unordered": 0,
        "completed_ordered": 0, "completed_unordered": 0,
    });
    out.add("network-diagnostics/radio-tx-budget", diagnostics);
}

/// `GET /api/radio/modem`. The route reports process-wide modem state that
/// other tests change, so the fixture is the same `Status` type filled with a
/// completed check (a real `Report` and an RF board block decoded from
/// BOARD_HEALTH words); the route's own reply must have the same fields.
async fn radio_modem(app: &axum::Router, out: &mut Fixtures) {
    use crate::comms::rfd_modem::{self, Mode, Outcome, Policy, RegisterRow, Report, Trigger};
    let mut report = Report::skipped(Trigger::Startup, Mode::Fix, "Radio modem settings match the profile");
    report.outcome = Outcome::Matched;
    report.identity = Some("RFD900x".into());
    report.registers = vec![
        RegisterRow { number: 5, name: Some("GLOBAL_RETRIES".into()), observed: Some(10), expected: Some(10), policy: Policy::Fix, status: "match" },
        RegisterRow { number: 3, name: Some("NETID".into()), observed: Some(25), expected: None, policy: Policy::Info, status: "info" },
    ];
    report.warnings = vec!["Contract fixture warning".into()];
    report.finished_unix_ms = 1;
    // RF board BOARD_HEALTH: matched check, ATI5 names, fix mode.
    let mut words = vec![0u32; 24];
    (words[0], words[1], words[2], words[4]) = (1, 1 | 8, 9, 1000);
    words[13] = 1 | (1 << 4) | (2 << 10) | (0x04 << 16) | (10 << 24);
    words[15] = 3968 | (50 << 16);
    words[22] = 32768 | (64 << 16) | (10 << 24);
    let rf_board = rfd_modem::decode_rf_status(&words);
    assert!(rf_board.is_some());
    let status = rfd_modem::Status {
        available: true,
        profile_name: Some("deployed".into()),
        opt_in_profile: false,
        rolled_out_all_nodes: true,
        configured_mode: Mode::Fix,
        running: false,
        reconfiguration_allowed: true,
        blocked_reason: None,
        needs_attention: report.needs_attention(),
        report: Some(report),
        rf_board,
        receive_quality: crate::comms::link_quality::Snapshot {
            good_frames_last_minute: 120,
            crc_failures_last_minute: 2,
            good_frames_total: 5000,
            crc_failures_total: 7,
        },
    };
    let value = serde_json::to_value(status).unwrap();
    let live = get_ok(app, "/api/radio/modem", Some("viewer")).await;
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&live), keys(&value), "the route serves this type");
    out.add("radio/modem", value);
}

/// `GET /api/rrls/status`. The route reads the process-wide recovery radio
/// (installed by other tests), so the fixtures are its `Status` type for a
/// station without a recovery radio and for an RRLS that is relaying.
async fn rrls_status(app: &axum::Router, out: &mut Fixtures) {
    let live = get_ok(app, "/api/rrls/status", None).await;
    let idle = crate::rrls::Status {
        enabled: false,
        role: None,
        forwarding: false,
        relay_requested: false,
        direct_age_ms: None,
        queued_frames: 0,
        dropped_frames: 0,
        forwarded_frames: 0,
        relayed_frames: 0,
        relay_envelope_bytes: 0,
        settings_framing: None,
        recovery_budget: None,
        relay_decision: None,
        pacing_warning: None,
    };
    let idle = serde_json::to_value(idle).unwrap();
    for key in idle.as_object().unwrap().keys() {
        assert!(live.get(key).is_some(), "the route serves this type: {key} missing from {live}");
    }
    out.add("rrls/status", idle);
    let relaying = crate::rrls::Status {
        enabled: true,
        role: Some(crate::rrls::Role::Rrls),
        forwarding: true,
        relay_requested: true,
        direct_age_ms: Some(1_500),
        queued_frames: 2,
        dropped_frames: 0,
        forwarded_frames: 40,
        relayed_frames: 12,
        relay_envelope_bytes: 960,
        settings_framing: None,
        recovery_budget: None,
        relay_decision: None,
        pacing_warning: None,
    };
    out.add("rrls/status-relaying", serde_json::to_value(relaying).unwrap());
}

/// `GET /api/auth/session` (signed in and anonymous) and
/// `POST /api/auth/challenge`.
async fn auth(app: &axum::Router, out: &mut Fixtures) {
    let mut session = get_ok(app, "/api/auth/session", Some("operator")).await;
    pin_fields(&mut session, &["expires_at_ms", "created_at_ms", "server_time_ms", "station_id", "fingerprint"]);
    out.add("auth/session-operator", session);
    let mut anonymous = get_ok(app, "/api/auth/session", None).await;
    pin_fields(&mut anonymous, &["expires_at_ms", "created_at_ms", "server_time_ms", "station_id", "fingerprint"]);
    out.add("auth/session-anonymous", anonymous);
    let (status, mut challenge) =
        call(app.clone(), "POST", "/api/auth/challenge", None, Some(json!({"username":"operator"}))).await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    pin_fields(&mut challenge, &["challenge_id", "server_nonce_b64", "expires_at_ms", "expires_in_ms", "salt_b64"]);
    out.add("auth/challenge", challenge);
}

/// `GET /api/board_abort` in HITL with every board aborted: each active error
/// is marked dismissible, and the UI keys a dismissal by board and abort time
/// (`timestamp_ms`), so a dismissed Abort stays hidden until the board aborts
/// again.
async fn board_abort_hitl(out: &mut Fixtures) {
    use crate::types::Board;
    let _mode = thread_mode(Mode::Hitl);
    let root = TestDirectory::new();
    let state = station(&root.0).await;
    let app = crate::web::router(state.clone(), String::new());
    let boards = [
        Board::RFBoard,
        Board::FlightComputer,
        Board::PowerBoard,
        Board::ActuatorBoard,
        Board::ValveBoard,
        Board::GatewayBoard,
        Board::DaqBoard,
    ];
    for (i, board) in boards.into_iter().enumerate() {
        state.board_abort.observe_abort(board, std::time::Instant::now(), T0 as u64 + 1_000 * i as u64);
    }
    let value = get_ok(&app, "/api/board_abort", Some("viewer")).await;
    let errors = value["active_errors"].as_array().unwrap();
    assert_eq!(errors.len(), boards.len(), "{value}");
    assert!(errors.iter().all(|e| e["dismissible"] == true), "{value}");
    out.add("board-abort/status-hitl", value);
}

/// The FC's Kalman filter status: the `FcEstimator` frame after the filter
/// started at T+0.3 s, and `/api/fc_estimator` before any FC report.
async fn fc_estimator(app: &axum::Router, out: &mut Fixtures) {
    use crate::fc_estimator::{Context, TAG, Tracker};
    let tracker = Tracker::default();
    let launch = std::time::Instant::now();
    let flight = Context { launched: true, expected: true };
    tracker.tick(launch, flight);
    let mut words = [0u32; 24];
    words[0] = 1;
    words[2] = 11_301;
    words[4] = 1000;
    words[13] = (TAG << 24) | (3 << 20) | 62;
    words[15] = crate::fc_estimator::word15(11_051, (0xFB4 << 2) | 1); // tracking, -0.76 hPa
    tracker.observe_barometer(&[0.0, 100_320.0, 21.5], launch);
    tracker.observe(&words, launch + Duration::from_millis(550), Some(T0 as u64 + 1_250), flight);
    let status = tracker.status(launch + Duration::from_millis(2_550));
    out.add("websocket/fc-estimator", serde_json::to_value(WsOutMsg::FcEstimator(status)).unwrap());
    let mut value = get_ok(app, "/api/fc_estimator", Some("viewer")).await;
    pin_fields(&mut value, &["report_age_ms", "running_for_ms"]);
    out.add("fc-estimator/get", value);
}

/// `POST /api/gse/self-test-confirmation`: the isolation checkbox's request as
/// the station decodes it (the UI checks that it sends exactly this), and the
/// station's reply.
async fn self_test_confirmation(out: &mut Fixtures) {
    let _mode = thread_mode(Mode::Normal);
    let root = TestDirectory::new();
    let state = station(&root.0).await;
    let app = crate::web::router(state.clone(), String::new());
    let (method, path, body) = ("POST", "/api/gse/self-test-confirmation", json!({"confirmed": true}));
    let (status, reply) = call(app.clone(), method, path, Some("isolation"), Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(state.gse.lock().unwrap().engine.config.dry_self_test_confirmed, "the station took the confirmation");
    out.add("requests/gse-self-test-confirmation", json!({"method": method, "path": path, "body": body, "reply": reply}));
}

/// A portable replay file as the app saves it: the actual
/// `GET /api/recordings/{id}/replay/export` body for a recording of the IGT
/// `nominal_10k` mock flight (thinned to 0.5 Hz), plus the configuration the
/// app attaches from the same station (`layout`, `boards`, `view_config`,
/// `vehicle_geometry`, `valve_mappings`, `mode`). The UI opens this with no station connected
/// (offline replay).
/// Telemetry rows of the IGT `nominal_10k` mock flight, thinned to 0.5 Hz.
fn nominal_10k_rows() -> Vec<(i64, &'static str, &'static str, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-data/igt/nominal_10k/IGT");
    let read = |name: &str| -> Vec<Vec<f64>> {
        std::fs::read_to_string(dir.join(name)).unwrap().lines().skip(1)
            .map(|l| l.split(',').map(|v| v.parse().unwrap()).collect()).collect()
    };
    let (baro, gps) = (read("BARO0.CSV"), read("GPS0.CSV"));
    let at = |rows: &[Vec<f64>], t: f64| rows.iter().find(|r| r[0] >= t).unwrap_or(rows.last().unwrap()).clone();
    let mut rows: Vec<(i64, &str, &str, Value)> = Vec::new();
    // FlightState codes: Armed 6, Launch 7, Ascent 8, Coast 9, Descent 11, Landed 13.
    for (t, code) in [(0, 6), (10_000, 7), (10_200, 8), (17_300, 9), (36_600, 11), (204_000, 13)] {
        rows.push((t, "FC", "FLIGHT_STATE", json!([code])));
    }
    for t in (0..=236_000i64).step_by(2_000) {
        let (b, g) = (at(&baro, t as f64), at(&gps, t as f64));
        rows.push((t, "FC", "GPS_DATA", json!([g[1], g[2], g[3]])));
        rows.push((t, "FC", "BAROMETER_DATA", json!([b[1], b[2], b[3]])));
        let burning = (10_000..17_300).contains(&t);
        rows.push((t, "VB", "VALVE_STATE", json!([burning as u8, 0, 0, (t == 10_000) as u8, 0, 0, 1])));
        rows.push((t, "VB", "OX_TANK_PRESSURE", json!([if t < 10_000 { 745.0 } else if burning { 745.0 - (t - 10_000) as f64 * 0.08 } else { 30.0 }])));
        rows.push((t, "PB", "BATTERY_VOLTAGE", json!([7.9 - t as f64 * 1e-6])));
    }
    rows.sort_by_key(|r| r.0);
    rows
}

async fn insert_rows(db: &sqlx::SqlitePool, rows: Vec<(i64, &str, &str, Value)>) {
    for (t, sender, data_type, values) in rows {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(?,NULL,?,?,?,'[]')")
            .bind(T0 + t).bind(sender).bind(data_type).bind(values.to_string()).execute(db).await.unwrap();
    }
}

/// A self-contained replay bundle (`GET /api/recordings/{id}/bundle`) of a
/// recording of the same flight, the recording's own
/// `GET /api/recordings/{id}/csv`, and the bundle manifest. The UI opens the
/// bundle with no station and checks that its CSV export equals that CSV.
async fn replay_bundle(root: &Path, state: &Arc<AppState>, app: &axum::Router, out: &mut Fixtures) {
    let id = "groundstation_recording_2026-01-01_00-00-00_000.db";
    let (db, _) = crate::telemetry_db::open_telemetry_db(&root.join(id)).await.unwrap();
    insert_rows(&db, nominal_10k_rows()).await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("Authorization", "Bearer viewer-token".parse().unwrap());
    // Loading a calibration file without a 50 kg input stamp stamps it with the
    // wall clock; pin it so the bundle's calibration entry is reproducible.
    state.loadcell_calibration.lock().unwrap().kg50_input_stamp_ms = T0 as u64;
    let options = crate::recording_bundle::Options { tiles: true, tiles_max_mb: Some(1), ..Default::default() };
    let bundle = crate::recording_bundle::build(state.clone(), &headers, id, &options, T0).await.unwrap();
    let again = crate::recording_bundle::build(state.clone(), &headers, id, &options, T0).await.unwrap();
    assert_eq!(bundle, again, "a bundle is reproducible");
    let request = Request::builder().uri(format!("/api/recordings/{id}/csv")).header("Authorization", "Bearer viewer-token").body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let csv = axum::body::to_bytes(response.into_body(), 1 << 24).await.unwrap().to_vec();
    let request = Request::builder().uri(format!("/api/recordings/{id}/bundle")).header("Authorization", "Bearer viewer-token").body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "the bundle route answers");
    assert_eq!(response.headers()["content-type"], crate::recording_bundle::MEDIA_TYPE);
    db.close().await;
    let manifest = bundle_manifest(&bundle);
    let csv_sha: String = ring::digest::digest(&ring::digest::SHA256, &csv).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let listed = manifest["entries"].as_array().unwrap().iter().find(|e| e["path"] == "data/telemetry.csv").unwrap();
    assert_eq!(listed["sha256"], csv_sha, "the bundle's CSV is the recording's own CSV export");
    // The recording's trajectory with its flight-phase segmentation.
    let upper = nominal_10k_rows().len();
    let trajectory = get_ok(app, &format!("/api/recordings/{id}/replay/trajectory?upper_id={upper}"), Some("viewer")).await;
    out.add("replay-trajectory/flight", trajectory);
    out.add("replay-bundle/flight.manifest", manifest);
    binary_fixture("replay-bundle/flight.gsreplay", &bundle);
    binary_fixture("replay-bundle/flight.csv", &csv);
}

/// Recovery search: start with a target area, a searcher's opt-in and track
/// upload, the status with its `search_coverage` overlay, the refusals a
/// client must handle, and an administrator's clear. Clock readings and the
/// random session / opt-in ids are pinned.
async fn recovery_search(app: &axum::Router, out: &mut Fixtures) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let frame = crate::recovery_search::coverage::Frame::new(31.758, -102.046);
    let geo = |x: f64, y: f64| {
        let (lat, lon) = frame.to_geo(x, y);
        [(lat * 1e7).round() / 1e7, (lon * 1e7).round() / 1e7]
    };
    let target = json!({"polygon": [geo(0.0, 0.0), geo(200.0, 0.0), geo(200.0, 100.0), geo(0.0, 100.0)], "source": "los_prediction", "label": "95% landing area"});
    let (status, refused) = call(app.clone(), "POST", "/api/recovery_search/start", Some("viewer"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mut fixtures: Vec<(&str, Value)> = vec![("recovery-search/refused-start-viewer", refused)];
    let (status, started) = call(app.clone(), "POST", "/api/recovery_search/start", Some("operator"), Some(json!({"swath_width_m": 30, "target": target}))).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let session_id = started["session_id"].as_str().unwrap().to_owned();
    fixtures.push(("recovery-search/started", started));
    let opt = json!({"device_id": "fixture-phone", "device_name": "viewer's iPhone", "platform": "ios", "sharing": true});
    let (status, opted) = call(app.clone(), "POST", "/api/recovery_search/opt_in", Some("viewer"), Some(opt)).await;
    assert_eq!(status, StatusCode::OK, "{opted}");
    let opt_in_id = opted["opt_in_id"].as_str().unwrap().to_owned();
    fixtures.push(("recovery-search/opt-in", opted));
    let points: Vec<Value> = (0..=24)
        .map(|i| {
            let p = geo(100.0, -10.0 + 5.0 * i as f64);
            json!({"seq": i + 1, "t_ms": now + i * 1000, "lat": p[0], "lon": p[1], "acc_m": 4.0})
        })
        .collect();
    let batch = json!({"v": 1, "session_id": session_id, "opt_in_id": opt_in_id, "device_id": "fixture-phone", "points": points});
    let (status, accepted) = call(app.clone(), "POST", "/api/recovery_search/track", Some("viewer"), Some(batch.clone())).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    fixtures.push(("recovery-search/track-accepted", accepted));
    let (_, duplicate) = call(app.clone(), "POST", "/api/recovery_search/track", Some("viewer"), Some(batch.clone())).await;
    fixtures.push(("recovery-search/track-duplicate", duplicate));
    let mut stranger = batch.clone();
    stranger["device_id"] = "fixture-other".into();
    let (status, not_opted) = call(app.clone(), "POST", "/api/recovery_search/track", Some("viewer"), Some(stranger)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    fixtures.push(("recovery-search/refused-not-opted-in", not_opted));
    fixtures.push(("recovery-search/status", get_ok(app, "/api/recovery_search?device_id=fixture-phone", Some("viewer")).await));
    let (status, stopped) = call(app.clone(), "POST", "/api/recovery_search/stop", Some("operator"), None).await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    fixtures.push(("recovery-search/stopped", stopped));
    let (status, refused) = call(app.clone(), "DELETE", "/api/recovery_search/tracks", Some("operator"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    fixtures.push(("recovery-search/refused-clear-operator", refused));
    let (status, cleared) = call(app.clone(), "DELETE", "/api/recovery_search/tracks", Some("administrator"), None).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    fixtures.push(("recovery-search/cleared", cleared));
    // The clear forgot the stopped session's devices: a late batch is refused.
    let (status, _) = call(app.clone(), "POST", "/api/recovery_search/track", Some("viewer"), Some(batch)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let pins = [(session_id.clone(), "rs-fixture-session"), (opt_in_id.clone(), "fixture-opt-in")];
    for (name, mut value) in fixtures {
        replace_text(&mut value, &pins);
        pin_fields(&mut value, &["started_ms", "stopped_ms", "set_ms", "opted_in_ms", "opted_out_ms", "first_ms", "last_ms", "cleared_ms", "late_upload_until_ms"]);
        out.add(name, value);
    }
}

/// `manifest.json`, the first (deflated or stored) entry of a bundle.
fn bundle_manifest(bundle: &[u8]) -> Value {
    let u16_at = |i: usize| u16::from_le_bytes([bundle[i], bundle[i + 1]]) as usize;
    let u32_at = |i: usize| u32::from_le_bytes([bundle[i], bundle[i + 1], bundle[i + 2], bundle[i + 3]]) as usize;
    assert_eq!(u32_at(0), 0x0403_4b50);
    let (method, size, name_len, extra) = (u16_at(8), u32_at(18), u16_at(26), u16_at(28));
    assert_eq!(&bundle[30..30 + name_len], b"manifest.json");
    let body = &bundle[30 + name_len + extra..30 + name_len + extra + size];
    let text = if method == 8 {
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::DeflateDecoder::new(body), &mut out).unwrap();
        out
    } else {
        String::from_utf8(body.to_vec()).unwrap()
    };
    serde_json::from_str(&text).unwrap()
}

/// A non-JSON fixture: written under `GS_UPDATE_CONTRACT_FIXTURES=1`, compared byte for byte otherwise.
fn binary_fixture(name: &str, bytes: &[u8]) {
    let path = fixture_dir().join(name);
    if std::env::var(UPDATE_ENV).as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    } else {
        assert!(std::fs::read(&path).ok().as_deref() == Some(bytes), "{name} differs from the backend's current output; regenerate with {UPDATE_ENV}=1");
    }
}

async fn replay_export(app: &axum::Router, out: &mut Fixtures) {
    let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)").execute(&db).await.unwrap();
    insert_rows(&db, nominal_10k_rows()).await;
    let mut file = crate::recording_replay::test_export_from(&db).await.unwrap();
    file["recording"] = "nominal_10k.db".into();
    file["mode"] = "flight".into();
    file["layout"] = get_ok(app, "/api/layout", Some("viewer")).await;
    file["boards"] = get_ok(app, "/api/boards", Some("viewer")).await["boards"].clone();
    file["view_config"] = get_ok(app, "/api/vehicle_visualization", Some("viewer")).await;
    file["vehicle_geometry"] = get_ok(app, crate::vehicle_geometry::PATH, Some("viewer")).await;
    file["valve_mappings"] = json!([
        {"label": "Pilot valve", "data_type": "VALVE_STATE", "index": 0, "true_label": "Open", "false_label": "Closed"},
        {"label": "Normally-open valve", "data_type": "VALVE_STATE", "index": 1, "true_label": "Open", "false_label": "Closed"},
        {"label": "Dump valve", "data_type": "VALVE_STATE", "index": 2, "true_label": "Open", "false_label": "Closed"}
    ]);
    file["configuration_provenance"] = "Station configuration at export; not a claim of historical capture-time configuration.".into();
    out.add("replay-export/flight", file);
}

/// Flight plans: a computed plan as the station stores and serves it, the list
/// (with the request defaults), the plan bundle's manifest; and the
/// loss-of-signal prediction for a simulated cut and the idle live view.
async fn flight_plans(state: &Arc<AppState>, app: &axum::Router, out: &mut Fixtures) {
    use crate::flight_plan::{DispersionRequest, Request, WeatherRequest};
    let request = Request {
        kind: Some("flight".into()),
        label: Some("8.5 kg nominal".into()),
        fill_kg: Some(8.5),
        supercharge_psi: Some(750.0),
        tank_temp_c: Some(15.0),
        motor_model: Some("scaling".into()),
        weather: Some(WeatherRequest {
            source: Some("manual".into()),
            manual: Some(crate::apogee_planner::weather::ManualWeather { surface_temp_c: 20.0, wind_mps: 4.0, wind_from_deg: 300.0, gust_mps: 1.0, ..Default::default() }),
        }),
        site: Some(crate::apogee_planner::weather::Site { lat_deg: 31.758, lon_deg: -102.046, alt_msl_m: 890.0 }),
        dispersion: Some(DispersionRequest { samples: Some(20), seed: Some(1) }),
        ..Default::default()
    };
    let mut plan = crate::flight_plan::compute(state, &request, Some("operator".into()), T0).unwrap();
    // Timings are wall-clock readings.
    plan["compute_ms"] = json!(0.0);
    plan["dispersion"]["compute_ms"] = json!(0.0);
    crate::flight_plan::save(&crate::flight_plan::directory(state), &plan).unwrap();
    let id = plan["id"].as_str().unwrap().to_string();
    out.add("flight-plans/list", get_ok(app, "/api/flight_plans", Some("viewer")).await);
    out.add("flight-plans/plan", get_ok(app, &format!("/api/flight_plans/{id}"), Some("viewer")).await);
    let (status, refused) = call(app.clone(), "POST", "/api/flight_plans/run", Some("viewer"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    out.add("flight-plans/run-forbidden", refused);
    let headers = {
        let mut h = axum::http::HeaderMap::new();
        h.insert("Authorization", "Bearer viewer-token".parse().unwrap());
        h
    };
    let bundle = crate::recording_bundle::build_plan(state.clone(), &headers, &plan, T0).await.unwrap();
    out.add("flight-plans/bundle-manifest", bundle_manifest(&bundle));
    let body = json!({
        "last_fix": {"t_ms": T0, "lat_deg": 31.7595, "lon_deg": -102.041, "alt_msl_m": 2500.0, "ve_mps": 3.5, "vn_mps": -1.0, "vu_mps": -25.0},
        "phase": "drogue", "elapsed_s": 12.0, "site": {"lat_deg": 31.758, "lon_deg": -102.046, "alt_msl_m": 890.0}, "samples": 16,
    });
    let (status, mut simulated) = call(app.clone(), "POST", "/api/los_prediction/simulate", Some("viewer"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{simulated}");
    // The station position is the station GPS when one is known (process-wide): pin it to the launch site.
    simulated["station"] = json!({"lat_deg": 31.758, "lon_deg": -102.046});
    simulated["from_station"] = Value::Null;
    out.add("los-prediction/simulate", simulated);
    // The live view reads process-wide state other tests drive: record its idle shape.
    let mut live = get_ok(app, "/api/los_prediction", Some("viewer")).await;
    for (k, v) in [("state", json!("idle")), ("comparison", Value::Null), ("history", json!([])), ("settings", json!({"enabled": true, "timeout_s": 5.0})), ("flight_state", json!("Idle"))] {
        live[k] = v;
    }
    for k in ["last_fix", "signal_lost_ms", "since_fix_s", "phase", "predicted_now", "landing", "station", "from_station", "wind_source", "overlay"] {
        live[k] = Value::Null;
    }
    out.add("los-prediction/idle", live);
}

/// Terrain (no DEM cached on the test station, plus a synthetic DEM grid
/// through the real serializer) and the live flight phases of the nominal
/// 10k flight (`GET /api/flight_phases`, assembled from fixed inputs).
async fn terrain_and_phases(app: &axum::Router, out: &mut Fixtures) {
    out.add("terrain/heightmap-unavailable", get_ok(app, "/api/terrain/heightmap?lat=31.758&lon=-102.046&extent_m=6000&size=65", Some("viewer")).await);
    out.add("terrain/status-empty", get_ok(app, "/api/terrain/status?lat=31.758&lon=-102.046&radius_m=1000", Some("viewer")).await);
    let (fx, fy) = crate::terrain::tile_xy(31.758, -102.046, 10);
    let png = crate::terrain::encode_terrarium(256, 256, |x, y| 850.0 + x as f32 * 0.5 + y as f32 * 0.25);
    let temp = crate::terrain::tests::temp_store("fixture", &[((10, fx as u32, fy as u32), png)]).await;
    let map = crate::terrain::build_heightmap(&temp.store, 31.758, -102.046, 2_000.0, 17).await.unwrap();
    out.add("terrain/heightmap", serde_json::to_value(map).unwrap());
    let rows = nominal_10k_rows();
    let reports: Vec<(i64, u8)> = rows.iter().filter(|r| r.2 == "FLIGHT_STATE").map(|r| (T0 + r.0, r.3[0].as_u64().unwrap() as u8)).collect();
    let fc: Vec<(i64, f64, f64, f64)> = rows
        .iter()
        .filter(|r| r.2 == "GPS_DATA" && r.0 % 10_000 == 0)
        .map(|r| (T0 + r.0, r.3[0].as_f64().unwrap(), r.3[1].as_f64().unwrap(), r.3[2].as_f64().unwrap()))
        .collect();
    let live = crate::flight_phases::assemble(Some(T0), &reports, Vec::new(), fc);
    out.add("flight-phases/live", serde_json::to_value(live).unwrap());
}

#[tokio::test]
async fn contract_fixtures_match_the_backend() {
    let _mode = thread_mode(Mode::Normal);
    let root = TestDirectory::new();
    let state = station(&root.0).await;
    let app = crate::web::router(state.clone(), String::new());
    let mut out = Fixtures::default();
    station_modes(&root.0, &state, &mut out).await;
    // The switch above went to HITL; read the rest in Normal mode.
    drop(_mode);
    let _mode = thread_mode(Mode::Normal);
    operating_mode(&app, &mut out).await;
    board_recovery(&app, &mut out).await;
    flight_state(&app, &state, &mut out).await;
    action_policy(&app, &state, &mut out).await;
    alerts(&app, &state, &mut out).await;
    radio_tx_budget(&mut out);
    radio_modem(&app, &mut out).await;
    rrls_status(&app, &mut out).await;
    auth(&app, &mut out).await;
    fc_estimator(&app, &mut out).await;
    vehicle_geometry(&app, &mut out).await;
    replay_export(&app, &mut out).await;
    replay_bundle(&root.0, &state, &app, &mut out).await;
    flight_plans(&state, &app, &mut out).await;
    recovery_search(&app, &mut out).await;
    terrain_and_phases(&app, &mut out).await;
    drop(_mode);
    board_abort_hitl(&mut out).await;
    self_test_confirmation(&mut out).await;
    out.finish();
}
