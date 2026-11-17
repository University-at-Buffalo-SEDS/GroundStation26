//! Recover station over the real HTTP route: it clears the station's own
//! Abort latch only once every present board has been recovered, refuses with
//! the reason otherwise, sends nothing to any board, journals the action and
//! moves the station-state broadcast the RRLS shows.
use super::*;
use crate::board_recovery::tests::{HttpFixture, http, http_fixture};
use crate::station_mode::Mode;
use crate::types::FlightState;
use std::time::Instant;

async fn fixture() -> HttpFixture {
    let f = http_fixture().await;
    *f.state.launch_history.lock().unwrap() =
        crate::launch_history::Store::open(f.directory.join("clock.json"), false, None, 10_000, 0).unwrap();
    let router = Arc::new(sedsnet::router::Router::new(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
    ));
    crate::telemetry_task::refresh_host_network_time(&router, crate::telemetry_task::get_current_timestamp_ms());
    assert!(f.state.topology_router.set(router).is_ok());
    // After an operator Abort: the station is Aborted and its latch is set.
    *f.state.state.lock().unwrap() = FlightState::Aborted;
    f.state.set_abort_indicator_latched(true);
    f
}

async fn post(f: &HttpFixture, token: Option<&str>, body: Value) -> (StatusCode, Value) {
    let response = http(f.state.clone(), "POST", PATH, token, body).await;
    let code = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn recover(f: &HttpFixture) -> (StatusCode, Value) {
    post(f, Some("operator"), json!({"confirm": true})).await
}

fn present(state: &AppState, board: Board) {
    state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(Instant::now());
}

/// A fresh correlated readback in `network_state` with the board's safe outputs.
fn readback(state: &AppState, board: Board, network_state: u8) {
    let now = Stamp::now();
    let (flags, outputs) = match safe_output_mask(board) {
        Some(mask) => (STATE_VALID | OUTPUTS_VALID | 2 | 0x04 | 0x20, mask),
        None => (STATE_VALID | 0x04 | 0x20, 0),
    };
    state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()] = Some(Observation {
        received: now,
        observed_after: now,
        value: Readback { result: 0, state: Some(network_state), flags, nonce: 1, boot: 1, epoch: 2, uptime: 1, outputs, issuer: 1 },
    });
}

/// The board's Abort row, then its per-board recovery: a validated reply that
/// proves it out of Abort.
fn aborted_then_recovered(state: &AppState, board: Board) {
    state.board_abort.observe_abort(board, Instant::now(), 100);
    let revision = state.board_abort.query_revision(board);
    let after = Instant::now();
    state.board_abort.observe_status(board, 1, 2, 1, Some(1), false, true, after, revision, after, 200);
    readback(state, board, 1);
}

/// Reliable traffic a board would receive: recovery requests, Aborts, commands.
fn board_traffic_pending(state: &AppState) -> Vec<&'static str> {
    let router = state.topology_router.get().unwrap();
    ["BOARD_RECOVERY_REQUEST", "ABORT", "VALVE_COMMAND", "ACTUATOR_COMMAND", "FLIGHT_COMMAND"]
        .into_iter()
        .filter(|name| {
            crate::abort_retirement::try_has_pending_reliable_tx_type(router, crate::telemetry_schema::data_type(name))
                .unwrap_or(true)
        })
        .collect()
}

#[tokio::test]
async fn clears_the_latch_once_every_present_board_is_recovered_and_sends_nothing_to_boards() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    crate::station_state::reset_for_test();
    let f = fixture().await;
    let state = f.state.clone();
    for board in [Board::ActuatorBoard, Board::ValveBoard, Board::FlightComputer] {
        present(&state, board);
        aborted_then_recovered(&state, board);
    }
    assert_eq!(crate::station_state::StationState::capture(&state).flight_state, FlightState::Aborted);
    let view = state.action_policy_snapshot().station_recovery.expect("shown while latched");
    assert_eq!(view.disabled_reason, None);
    let mut flight = state.state_tx.subscribe();
    assert!(board_traffic_pending(&state).is_empty());
    let queries = state.board_recovery.inner.lock().unwrap().queries.clone();

    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["recovered"], true);
    assert_eq!(body["from_state"], FlightState::Aborted as u8);
    assert!(!state.abort_indicator_latched());
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    // Fill-side presses are no longer dropped by the latch.
    for name in ["NormallyOpen", "Dump", "Pilot", "Nitrogen", "Nitrous", "Igniter"] {
        assert_eq!(crate::gse::abort_latch_output_refusal(&state, name), None, "{name}");
    }
    // No board reset, recovery request, query, Abort or command.
    assert!(board_traffic_pending(&state).is_empty(), "{:?}", board_traffic_pending(&state));
    assert_eq!(state.board_recovery.coordinator.operation(), None);
    assert!(state.board_recovery.inner.lock().unwrap().queries.iter().zip(queries.iter())
        .all(|(now, before)| now.as_ref().map(|q| q.nonce) == before.as_ref().map(|q| q.nonce)));
    // Only the station's own FLIGHT_STATE, now Startup.
    let cached = state.topology_router.get().unwrap()
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap().unwrap();
    assert_eq!(cached.payload(), &[FlightState::Startup as u8]);
    // The station-state broadcast the RRLS shows follows at once: the worker
    // wakes on this publication and captures the new state.
    assert_eq!(flight.try_recv().unwrap().state, FlightState::Startup);
    let broadcast = crate::station_state::StationState::capture(&state);
    assert_eq!(broadcast.flight_state, FlightState::Startup);
    let decoded = crate::station_state::StationState::decode(&broadcast.encode()).unwrap();
    assert_eq!(decoded.flight_state, FlightState::Startup);
    // Journaled under the operator's name.
    let history = state.launch_history.lock().unwrap();
    let record = history.station_recoveries().last().unwrap();
    assert_eq!((record.actor.as_str(), record.from_state.as_deref(), record.to_state.as_deref()),
        ("operator", Some("Aborted"), Some("Startup")));
    drop(history);
    // Nothing left to recover: hidden from the policy, refused if repeated.
    assert_eq!(state.action_policy_snapshot().station_recovery, None);
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("not latched"), "{body}");
    // The traffic probe does see a command owed to a board.
    let router = state.topology_router.get().unwrap();
    let kind = crate::telemetry_schema::data_type("VALVE_COMMAND");
    router.tx_queue(crate::telemetry_task::controller_packet(router, kind, &[1]).unwrap()).unwrap();
    assert!(board_traffic_pending(&state).contains(&"VALVE_COMMAND"));
}

#[tokio::test]
async fn hitl_returns_the_station_to_idle_like_reset_all_boards() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Hitl);
    let f = fixture().await;
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(f.state.local_flight_state_snapshot(), FlightState::Idle);
    assert!(!f.state.abort_indicator_latched());
}

#[tokio::test]
async fn refuses_while_a_present_board_still_reports_abort_and_keeps_the_latch() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    let state = f.state.clone();
    present(&state, Board::ValveBoard);
    state.board_abort.observe_abort(Board::ValveBoard, Instant::now(), 100);
    let reason = state.action_policy_snapshot().station_recovery.unwrap().disabled_reason.unwrap();
    assert!(reason.contains("VB still reports Abort"), "{reason}");
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("VB still reports Abort"), "{body}");
    assert!(state.abort_indicator_latched());
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
    assert!(state.launch_history.lock().unwrap().station_recoveries().is_empty());
    // Its per-board recovery done, the same request succeeds.
    aborted_then_recovered(&state, Board::ValveBoard);
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn an_absent_aborted_board_does_not_hold_the_station() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    // RF aborted, then left the network long ago.
    let long_ago = Instant::now() - Duration::from_secs(60);
    f.state.board_abort.observe_abort(Board::RFBoard, long_ago, 100);
    assert_ne!(f.state.board_abort.masks().0, 0);
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn refuses_a_present_board_whose_readback_is_not_healthy_idle() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    let state = f.state.clone();
    present(&state, Board::ValveBoard);
    readback(&state, Board::ValveBoard, FlightState::Aborted as u8);
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("VB last read back Abort"), "{body}");
    readback(&state, Board::ValveBoard, FlightState::PreFill.network() as u8);
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("not healthy in Startup or Idle"), "{body}");
    assert!(state.abort_indicator_latched());
}

#[tokio::test]
async fn refuses_while_a_board_holds_the_launch_latch() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    observe_board_state_for_test(&f.state, Board::ActuatorBoard, crate::types::NetworkFlightState::Launch as u8);
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::CONFLICT);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("AB still reports Launch") && message.contains("Reset all boards"), "{body}");
    assert!(f.state.abort_indicator_latched());
}

#[tokio::test]
async fn refuses_while_a_recovery_operation_is_in_progress() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    f.state.board_recovery.coordinator.0.lock().unwrap().operation = Some(json!({"id":"x","completed":false}));
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("being prepared or is running"), "{body}");
    assert!(f.state.abort_indicator_latched());
    f.state.board_recovery.coordinator.0.lock().unwrap().operation = Some(json!({"id":"x","completed":true}));
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn refuses_while_host_admission_is_closed_by_a_recovery() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    f.state.operator_admission_barrier.store(7, Ordering::Release);
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(f.state.abort_indicator_latched());
    assert!(block_reason(&f.state, true).unwrap().contains("unfinished board recovery"));
    f.state.operator_admission_barrier.store(0, Ordering::Release);
}

#[tokio::test]
async fn requires_an_operator_and_an_explicit_confirmation() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    assert_eq!(post(&f, None, json!({"confirm": true})).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(post(&f, Some("viewer"), json!({"confirm": true})).await.0, StatusCode::FORBIDDEN);
    let (code, body) = post(&f, Some("operator"), json!({})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("no board is reset"), "{body}");
    assert!(f.state.abort_indicator_latched());
}

#[tokio::test]
async fn an_abort_between_the_check_and_the_commit_keeps_the_station_aborted() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    let state = &f.state;
    let basis = crate::gse::StationRecoveryBasis::capture(state);
    assert_eq!(block_reason(state, true), None);
    // A board Abort lands after the checks.
    state.board_abort.observe_abort(Board::ValveBoard, Instant::now(), 300);
    assert!(crate::gse::commit_station_recovery(state, basis, FlightState::Startup).is_err());
    assert!(state.abort_indicator_latched());
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
}

#[tokio::test]
async fn the_board_recovery_status_reports_the_station_latch_and_its_reason() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    present(&f.state, Board::ValveBoard);
    f.state.board_abort.observe_abort(Board::ValveBoard, Instant::now(), 100);
    let response = http(f.state.clone(), "GET", "/api/board-recovery/status", Some("operator"), Value::Null).await;
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let station = &value["station_recovery"];
    assert_eq!(station["latched"], true, "{value}");
    assert_eq!(station["available"], false);
    assert!(station["blocked_reason"].as_str().unwrap().contains("VB still reports Abort"), "{station}");
    assert_eq!(station["target_state_label"], "Startup");
}

/// The recovered state is the station's durable FLIGHT_STATE, which is what a
/// restart restores (main.rs builds the station state from it), and the
/// journal entry survives a reopen. Own process: the network-variable cache
/// is process-global.
#[tokio::test]
async fn the_recovered_state_and_its_journal_entry_survive_a_restart() {
    if crate::operator_queue::isolated_case(module_path!(), "the_recovered_state_and_its_journal_entry_survive_a_restart") {
        return;
    }
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Normal);
    let f = fixture().await;
    let router = f.state.topology_router.get().unwrap().clone();
    crate::network_variables::set_flight_state(&router, FlightState::Aborted as u8).unwrap();
    assert_eq!(crate::types::u8_to_flight_state(crate::network_variables::flight_state()), Some(FlightState::Aborted));
    let (code, body) = recover(&f).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["published"], true);
    assert_eq!(crate::types::u8_to_flight_state(crate::network_variables::flight_state()), Some(FlightState::Startup));
    let history = f.state.launch_history.lock().unwrap();
    if let Some((writer, ticket)) = history.waiter() {
        writer.wait(ticket).unwrap();
    }
    drop(history);
    let reopened = crate::launch_history::Store::open(f.directory.join("clock.json"), false, None, 11_000, 0).unwrap();
    assert_eq!(reopened.station_recoveries().len(), 1);
    assert_eq!(reopened.station_recoveries()[0].to_state.as_deref(), Some("Startup"));
    assert_eq!(reopened.restored_recovery_barrier().0, 0, "evidence only: it never closes admission");
}
