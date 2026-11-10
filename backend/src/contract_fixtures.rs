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
        restart_delay: Duration::ZERO,
        switch_delay: Duration::ZERO,
        station_resets: false,
        stand_in_links: || 0,
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
    snapshot.controls.push(crate::sequences::ActionControl {
        disabled_reason: None,
        cmd: "ToggleAvBayUnderglow".into(),
        enabled: true,
        blink: crate::sequences::BlinkMode::None,
        actuated: view.confirmed,
        requested: Some(view.requested),
        network_note: note,
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
    out.add("network-diagnostics/radio-tx-budget", budget.diagnostics());
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
    action_policy(&app, &state, &mut out).await;
    alerts(&app, &state, &mut out).await;
    radio_tx_budget(&mut out);
    radio_modem(&app, &mut out).await;
    rrls_status(&app, &mut out).await;
    auth(&app, &mut out).await;
    drop(_mode);
    board_abort_hitl(&mut out).await;
    self_test_confirmation(&mut out).await;
    out.finish();
}
