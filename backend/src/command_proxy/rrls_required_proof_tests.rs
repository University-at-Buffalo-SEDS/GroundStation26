//! Source-bound signed presentation regressions. These never command hardware.
use super::*;
use crate::{board_abort, types::Board};
use crate::{primary_mirror, state::AppState};
use std::sync::Arc;
use std::time::Instant;

fn child(test_name: &str, role: &str) -> bool {
    if std::env::var("GS_RRLS_PROOF_REGRESSION_CHILD").as_deref() == Ok(test_name) {
        assert_eq!(crate::rrls::is_rrls(), role == "rrls");
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env("GS_RRLS_PROOF_REGRESSION_CHILD", test_name)
        .env("GS_RRLS_ROLE", role)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "child failed: {stdout}{stderr}");
    assert_eq!(
        stdout
            .matches("test result: ok. 1 passed; 0 failed; 0 ignored")
            .count(),
        1,
        "role-isolated child must execute exactly the intended one test: {stdout}{stderr}"
    );
    assert!(!stdout.contains("running 0 tests"));
    print!("{stdout}");
    true
}

// The signed application observation is deliberately built independently of
// this RRLS executable's local mode. Every byte passes the production decoder.
fn observation(required: u8, proven: u8, enforced: bool, test_fire: bool) -> Vec<u8> {
    let mut bytes = vec![0; 88];
    bytes[0] = 1;
    bytes[57] = 5;
    bytes[59] = required;
    bytes[60] = proven;
    for i in 0..7 {
        bytes[61 + 2 * i..63 + 2 * i].copy_from_slice(
            &if proven & (1 << i) != 0 {
                0u16
            } else {
                u16::MAX
            }
            .to_le_bytes(),
        );
    }
    bytes[75..83].copy_from_slice(&19u64.to_le_bytes());
    bytes[84] = 0; // Not configured: no fabricated motion timer/readiness.
    bytes[85..87].copy_from_slice(&u16::MAX.to_le_bytes());
    bytes[87] = 2 | u8::from(enforced) | (u8::from(test_fire) << 2);
    bytes
}

fn signed_view(state: &Arc<AppState>, bytes: Vec<u8>, mode: Mode, transit_ms: u64) {
    let (_root, p, r, mut primary, mut rr, pv, rv) = super::tests::connected();
    primary.observation = bytes.clone();
    let query = rr.query_observe(&r, rv).unwrap();
    let Received::Reply(reply) = primary.receive(&p, pv, mode, &query).unwrap() else {
        panic!("signed primary response");
    };
    let mut unsigned = reply.clone();
    *unsigned.last_mut().unwrap() ^= 1;
    assert!(rr.receive(&r, rv, Mode::Normal, &unsigned).is_err());
    let Received::Capability {
        boot,
        mode: actual_mode,
        observation,
    } = rr.receive(&r, rv, Mode::Normal, &reply).unwrap()
    else {
        panic!("authenticated current response");
    };
    assert_eq!(actual_mode, mode);
    assert_eq!(observation, bytes);
    assert!(accept_presentation(
        state,
        &observation,
        transit_ms,
        boot,
        rv.session
    ));
}

fn required_ids(mask: u8) -> Vec<String> {
    board_abort::BOARDS
        .into_iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, board)| board.sender_id().to_string())
        .collect()
}

#[tokio::test]
async fn normal_rrls_signed_hitl_off_snapshot_has_no_local_requirements() {
    crate::mode_suite!(Normal);
    if child(
        "command_proxy::rrls_required_proof_tests::normal_rrls_signed_hitl_off_snapshot_has_no_local_requirements",
        "rrls",
    ) {
        return;
    }
    assert_eq!(crate::station_mode::local_mode(), Mode::Normal);
    let state = crate::state::tests::test_app_state().await;
    assert!(
        state.board_state_proof_enforced(),
        "local normal policy is mandatory"
    );
    signed_view(&state, observation(0, 0, false, false), Mode::Hitl, 0);
    assert_eq!(primary_mirror::board_required_state_mask(&state), Some(0));
    assert_eq!(
        primary_mirror::board_history_required_scope(&state),
        Some(0)
    );
    let snapshot = board_abort::snapshot_for_state(&state);
    assert!(snapshot.required_boards.is_empty());
    assert!(snapshot.missing_required_state.is_empty());
    assert!(snapshot.active_errors.is_empty());
    assert!(
        primary_mirror::capture_launch_origin(&state).is_some(),
        "signed primary HITL OFF proof is not overridden by the local normal executable"
    );
}

#[tokio::test]
async fn normal_rrls_signed_test_fire_snapshot_requires_ground_daq_only() {
    crate::mode_suite!(Normal);
    if child(
        "command_proxy::rrls_required_proof_tests::normal_rrls_signed_test_fire_snapshot_requires_ground_daq_only",
        "rrls",
    ) {
        return;
    }
    let state = crate::state::tests::test_app_state().await;
    assert!(
        !state
            .av_bay_comms_connected
            .load(std::sync::atomic::Ordering::Acquire)
    );
    signed_view(&state, observation(0x78, 0, true, true), Mode::TestFire, 0);
    let missing = board_abort::snapshot_for_state(&state);
    assert_eq!(missing.required_boards, required_ids(0x78));
    assert_eq!(missing.missing_required_state, required_ids(0x78));
    assert!(primary_mirror::capture_launch_origin(&state).is_none());
    signed_view(
        &state,
        observation(0x78, 0x78, true, true),
        Mode::TestFire,
        0,
    );
    let ready = board_abort::snapshot_for_state(&state);
    assert_eq!(ready.required_boards, required_ids(0x78));
    assert!(ready.missing_required_state.is_empty());
    assert!(primary_mirror::capture_launch_origin(&state).is_some());
    assert!(
        !ready
            .required_boards
            .contains(&Board::FlightComputer.sender_id().into())
    );
    assert!(
        !ready
            .required_boards
            .contains(&Board::RFBoard.sender_id().into())
    );
    assert!(
        !ready
            .required_boards
            .contains(&Board::PowerBoard.sender_id().into())
    );
}

#[tokio::test]
async fn normal_rrls_unknown_legacy_stale_revoked_and_known_abort_stay_closed() {
    crate::mode_suite!(Normal);
    if child(
        "command_proxy::rrls_required_proof_tests::normal_rrls_unknown_legacy_stale_revoked_and_known_abort_stay_closed",
        "rrls",
    ) {
        return;
    }
    for case in [
        "unknown",
        "old57",
        "old87",
        "stale",
        "revoked",
        "reconnected",
    ] {
        let state = crate::state::tests::test_app_state().await;
        let mut bytes = observation(0, 0, false, false);
        match case {
            "unknown" => bytes[87] = 0,
            "old57" => bytes.truncate(57),
            "old87" => {
                bytes.truncate(87);
                bytes[57] = 4;
            }
            _ => {}
        }
        signed_view(
            &state,
            bytes,
            Mode::Hitl,
            if case == "stale" { 3_000 } else { 0 },
        );
        if matches!(case, "revoked" | "reconnected") {
            invalidate_presentation(&state); // Actual connection-change hook.
        }
        assert_eq!(
            primary_mirror::board_required_state_mask(&state),
            None,
            "{case}"
        );
        let snapshot = board_abort::snapshot_for_state(&state);
        assert_eq!(snapshot.required_boards, required_ids(0x7f), "{case}");
        assert_eq!(
            snapshot.missing_required_state,
            required_ids(0x7f),
            "{case}"
        );
        assert!(
            primary_mirror::capture_launch_origin(&state).is_none(),
            "{case}"
        );
    }
    let state = crate::state::tests::test_app_state().await;
    signed_view(&state, observation(0, 0, false, false), Mode::Hitl, 0);
    let manual_before = state.manual_abort_intent.epoch();
    let observed_before = state.deferred_abort.manual_epoch();
    state
        .board_abort
        .observe_abort(Board::FlightComputer, Instant::now(), 123456);
    signed_view(&state, observation(0, 0, false, false), Mode::Hitl, 0);
    let snapshot = board_abort::snapshot_for_state(&state);
    assert_eq!(snapshot.active_errors.len(), 1);
    assert_eq!(
        snapshot.active_errors[0].board,
        Board::FlightComputer.sender_id()
    );
    assert!(primary_mirror::capture_launch_origin(&state).is_none());
    assert!(
        board_abort::blocked_message(&state)
            .unwrap()
            .contains(Board::FlightComputer.as_str())
    );
    assert_eq!(state.manual_abort_intent.epoch(), manual_before);
    assert_eq!(state.deferred_abort.manual_epoch(), observed_before);
}

// Never built: no build combined HITL and test fire, and runtime modes are
// mutually exclusive, so this combination cannot occur.
#[cfg(any())]
#[tokio::test]
async fn hitl_test_fire_enabled_board_proof_requires_daq_without_av_bay() {
    if child(
        "command_proxy::rrls_required_proof_tests::hitl_test_fire_enabled_board_proof_requires_daq_without_av_bay",
        "primary",
    ) {
        return;
    }
    let state = crate::state::tests::test_app_state().await;
    assert!(!state.board_state_proof_enforced());
    assert_eq!(board_abort::required_mask(&state), 0);
    state.toggle_hitl_safety_interlock(3);
    assert!(state.board_state_proof_enforced());
    assert!(state.board_required_for_progression(Board::DaqBoard));
    assert_eq!(board_abort::required_mask(&state), 0x78);
    assert!(
        !state
            .av_bay_comms_connected
            .load(std::sync::atomic::Ordering::Acquire)
    );
    for board in [Board::FlightComputer, Board::RFBoard, Board::PowerBoard] {
        assert!(!state.board_required_for_progression(board));
    }
    state.toggle_hitl_safety_interlock(3);
    assert_eq!(board_abort::required_mask(&state), 0);
    state
        .board_abort
        .observe_abort(Board::FlightComputer, Instant::now(), 123456);
    assert!(
        board_abort::blocked_message(&state).is_some(),
        "known Abort is not an optional proof"
    );
}
