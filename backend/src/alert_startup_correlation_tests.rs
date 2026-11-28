//! Actual AppState/SQLite/publisher integration, compiled by the parent cohort.
use super::*;
use crate::{
    alert_conditions::{BoardKey, ConditionState},
    types::{Board, FlightState},
};
use std::sync::Arc;

#[tokio::test]
async fn actual_main_order_held_sqlite_restore_correlates_restored_abort_after_strong_clear() {
    let main = include_str!("main.rs");
    assert!(
        main.find("state.board_abort.publish_initial_history()")
            .unwrap()
            < main
                .find("let mut tt = tokio::spawn(telemetry_task(")
                .unwrap()
    );
    assert!(main.find("let mut tt = tokio::spawn(telemetry_task(").unwrap()<main.find("let _board_abort_publications = tokio::spawn(crate::board_abort::publish_service(").unwrap());
    let mut state = crate::state::tests::test_app_state().await;
    let directory = std::path::Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap()
        .to_owned();
    let history_path = directory.join("actual-board-history");
    let identity = [53; 16];
    let first_abort = 1234;
    // Build actual durable Abort history, then bootstrap the exact restored
    // Runtime/pending-error constructor used by main. It is not physical proof.
    let mut history =
        crate::board_abort_history::Store::provision_uncertain(&history_path, identity, 64)
            .unwrap();
    let saved = history.begin_session(64).unwrap();
    history
        .save(crate::board_abort_history::Snapshot {
            revision: saved.snapshot.revision + 1,
            actual_abort: 64,
            unknown: 64,
            enrolled: 64,
            first_abort_ms: [0, 0, 0, 0, 0, 0, first_abort],
        })
        .unwrap();
    drop(history);
    let changed = Arc::new(tokio::sync::Notify::new());
    let (restored, publisher, history_owner) =
        crate::board_abort_history_writer::Owner::open(&history_path, identity, changed.clone())
            .unwrap();
    assert_eq!(restored.snapshot.actual_abort & 64, 64);
    Arc::get_mut(&mut state).unwrap().board_abort = Arc::new(
        crate::board_abort::Runtime::restored(restored, publisher, changed),
    );
    let (db_tx, mut db_rx) = tokio::sync::mpsc::channel(128);
    Arc::get_mut(&mut state).unwrap().db_queue_tx = db_tx;
    let path = directory.join("alert-conditions.sqlite3");
    let prior_store = Store::open(&path, "prior".into()).await.unwrap();
    let mut prior = Registry::new("prior".into(), []);
    prior.restore(Vec::new()).unwrap();
    let entry = prior
        .observe(
            Key::BoardAbort(BoardKey::Daq),
            ConditionState::Active,
            "DAQ Board reports Abort".into(),
            first_abort as i64,
        )
        .journal
        .unwrap();
    let original_id = entry.record.occurrence_id.clone();
    prior_store.append(&[entry]).await.unwrap();
    prior_store.close().await;
    observe(
        &state,
        Key::StationHeartbeatEnqueue,
        ConditionState::Resolved,
        "Enqueue recovered".into(),
        1200,
    );
    state.board_abort.publish_initial_history();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let initializing = tokio::spawn({
        let state = state.clone();
        let entered = entered.clone();
        let release = release.clone();
        async move {
            initialize_with_restore_gate(&state, async move {
                entered.notify_one();
                release.acquire().await.unwrap().forget();
            })
            .await
        }
    });
    entered.notified().await;
    let mut raw = state.board_abort_tx.subscribe();
    let mut errors = state.errors_tx.subscribe();
    let service = tokio::spawn(crate::board_abort::publish_service(
        state.clone(),
        state.shutdown_subscribe(),
    ));
    let active = tokio::time::timeout(Duration::from_secs(3), raw.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.active_errors.len(), 1);
    assert!(
        errors.try_recv().is_err(),
        "raw Abort remains visible while only correlated history waits"
    );
    let query = Instant::now();
    let revision = state.board_abort.query_revision(Board::DaqBoard);
    // Invalid/source-reset evidence still cannot clear the raw authority.
    state.board_abort.observe_status(
        Board::DaqBoard,
        0,
        0,
        7,
        Some(FlightState::Idle as u8),
        false,
        true,
        query,
        revision,
        Instant::now(),
        1500,
    );
    assert_ne!(state.board_abort.masks().0 & 64, 0);
    let clear_query = Instant::now();
    state.board_abort.observe_status(
        Board::DaqBoard,
        42,
        3,
        8,
        Some(FlightState::Idle as u8),
        false,
        true,
        clear_query,
        revision,
        Instant::now(),
        1600,
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.board_abort.masks().0 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let released_at = crate::telemetry_task::get_current_timestamp_ms() as i64;
    release.add_permits(1);
    let mut store = initializing.await.unwrap();
    assert!(store.is_some());
    let error = tokio::time::timeout(Duration::from_secs(3), errors.recv())
        .await
        .unwrap()
        .unwrap();
    let link = error
        .condition
        .expect("restored startup error must never become opaque merely due to writer delay");
    assert_eq!(link.condition_occurrence_id, original_id);
    assert!(error.timestamp_ms < released_at);
    let mut queued_legacy_error = false;
    while let Ok(item) = db_rx.try_recv() {
        match item {
            crate::telemetry_db::DbQueueItem::Condition(work) => {
                write(&state, &mut store, work).await
            }
            crate::telemetry_db::DbQueueItem::Write(crate::telemetry_db::DbWrite::Alert {
                condition: Some(condition),
                severity,
                timestamp_ms,
                ..
            }) if severity == "error" && condition.condition_occurrence_id == original_id => {
                assert_eq!(timestamp_ms, error.timestamp_ms);
                queued_legacy_error = true;
            }
            _ => {}
        }
    }
    assert!(
        queued_legacy_error,
        "actual legacy writer queue retains structured correlation"
    );
    let snapshot = state.alert_conditions.snapshot();
    assert!(snapshot.complete);
    let resolved = snapshot
        .records
        .iter()
        .find(|r| r.occurrence_id == original_id)
        .unwrap();
    assert_eq!(resolved.state, ConditionState::Resolved);
    assert_eq!(resolved.raised_at_ms, first_abort as i64);
    assert!(state.board_abort.masks().0 == 0);
    let durable = store.as_ref().unwrap().restore().await.unwrap();
    assert!(
        durable
            .iter()
            .any(|r| r.occurrence_id == original_id && r.state == ConditionState::Resolved)
    );
    // No acknowledgment was issued. The actual client reducer is additionally
    // exercised by the tiny source-extracted RED->GREEN integration fixture.
    state.request_shutdown();
    service.await.unwrap();
    store.unwrap().close().await;
    drop(history_owner);
    drop(state);
    assert!(!directory.exists());
}

#[tokio::test]
async fn actual_sqlite_integrity_write_completion_from_old_scope_cannot_mark_new_gap_saved() {
    let state = crate::state::tests::test_app_state().await;
    let scope = state.alert_conditions.snapshot().authority;
    let directory = std::path::Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap()
        .to_owned();
    let store = Store::open(&directory.join("alert-conditions.sqlite3"), scope.clone())
        .await
        .unwrap();
    let marker = crate::alert_conditions::IntegrityMarker {
        kind: crate::alert_conditions::IntegrityKind::BootstrapHistoryIncomplete,
        reason: "Old scope history incomplete".into(),
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let operation = tokio::spawn({
        let state = state.clone();
        let entered = entered.clone();
        let release = release.clone();
        let scope = scope.clone();
        async move {
            let result = persist_integrity_for_scope(&state, &scope, async {
                store.persist_integrity(&marker).await?;
                entered.notify_one();
                release.acquire().await.unwrap().forget();
                Ok(())
            })
            .await;
            store.close().await;
            result
        }
    });
    entered.notified().await;
    let mut next = Registry::new("replacement-scope".into(), []);
    for n in 0..65 {
        next.observe(
            Key::BoardAbort(BoardKey::Daq),
            if n % 2 == 0 {
                ConditionState::Active
            } else {
                ConditionState::Resolved
            },
            "State".into(),
            n,
        );
    }
    assert!(next.needs_repair());
    *state.alert_conditions.registry.lock().unwrap() = next;
    release.add_permits(1);
    assert!(!operation.await.unwrap());
    assert!(
        state
            .alert_conditions
            .registry
            .lock()
            .unwrap()
            .needs_repair()
    );
    assert!(!state.alert_conditions.snapshot().complete);
    assert!(
        state
            .alert_conditions
            .snapshot()
            .history_integrity
            .is_some()
    );
    state.request_shutdown();
    drop(state);
    assert!(!directory.exists());
}
