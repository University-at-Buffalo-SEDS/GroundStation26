//! Actual typed publication must retain an accepted exit even when delivery waits.
use super::*;
#[tokio::test]
async fn held_actual_board_abort_publisher_preserves_accepted_exit_after_proof_expiry() {
    use crate::{
        alert_conditions::BoardKey,
        types::{Board, FlightState},
    };
    let state = crate::state::tests::test_app_state().await;
    let store = initialize(&state).await;
    let mut errors = state.errors_tx.subscribe();
    let first_service = tokio::spawn(crate::board_abort::publish_service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let first_at = crate::telemetry_task::get_current_timestamp_ms();
    state
        .board_abort
        .observe_abort(Board::DaqBoard, Instant::now(), first_at);
    let event = tokio::time::timeout(Duration::from_secs(3), errors.recv())
        .await
        .unwrap()
        .unwrap();
    let occurrence = event.condition.unwrap().condition_occurrence_id;
    assert_eq!(
        state
            .alert_conditions
            .current_state(Key::BoardAbort(BoardKey::Daq)),
        Some(ConditionState::Active)
    );
    // Pause the actual consumer only; observation, ownership and proof aging
    // continue in the unchanged production Runtime. No record is edited.
    first_service.abort();
    assert!(first_service.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(2)).await;
    let after = Instant::now();
    let revision = state.board_abort.query_revision(Board::DaqBoard);
    state.board_abort.observe_status(
        Board::DaqBoard,
        123,
        2,
        2,
        Some(FlightState::Idle as u8),
        false,
        true,
        after,
        revision,
        Instant::now(),
        first_at + 2,
    );
    assert_eq!(
        state.board_abort.masks().0,
        0,
        "accepted strong exit must clear actual Abort"
    );
    tokio::time::sleep(crate::board_abort::PROOF_TTL + Duration::from_millis(100)).await;
    let daq_bit = 1 << 6;
    assert_eq!(
        state.board_abort.masks().1 & daq_bit,
        0,
        "expiry must not extend Launch proof"
    );
    assert!(
        state
            .board_abort
            .snapshot(daq_bit)
            .missing_required_state
            .iter()
            .any(|board| board == "DAQ")
    );
    let mut snapshots = state.board_abort_tx.subscribe();
    let resumed_service = tokio::spawn(crate::board_abort::publish_service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let reported = tokio::time::timeout(Duration::from_secs(3), snapshots.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        reported.active_errors.is_empty(),
        "actual resumed publisher sees accepted exit"
    );
    let record = state
        .alert_conditions
        .snapshot()
        .records
        .into_iter()
        .find(|record| record.occurrence_id == occurrence)
        .unwrap();
    assert_eq!(
        record.state,
        ConditionState::Resolved,
        "accepted exit chronology must not disappear because current Launch proof expired while publication was held"
    );
    assert_eq!(state.board_abort.masks().1 & daq_bit, 0);
    state.request_shutdown();
    resumed_service.await.unwrap();
    store.unwrap().close().await;
}
