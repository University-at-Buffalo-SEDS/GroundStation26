//! Reporting chronology is separate from expiring launch readiness.
use super::*;
use crate::alert_conditions::{BoardKey, Record};
use crate::board_abort::{AlertEdge, AlertEdgeKind, AlertEpisode};
use crate::types::{Board, FlightState};
use std::sync::Arc;

fn edge(ordinal: u64, kind: AlertEdgeKind, episode: Option<AlertEpisode>, at_ms: u64) -> AlertEdge {
    AlertEdge { ordinal, board: BoardKey::Daq, episode, kind, at_ms }
}
fn episode(ordinal: u64) -> AlertEpisode { AlertEpisode { ordinal, first_abort_ms: 1234 } }
fn registry() -> Registry {
    let mut value = Registry::new("current".into(), [Key::BoardAbort(BoardKey::Daq)]);
    value.restore(Vec::new()).unwrap();
    value
}
fn prior_record() -> Record {
    let mut prior = Registry::new("prior".into(), []);
    prior.restore(Vec::new()).unwrap();
    prior.observe(Key::BoardAbort(BoardKey::Daq), ConditionState::Active, "DAQ Board reports Abort".into(), 1234).journal.unwrap().record
}
#[test]
fn same_utc_recurrence_and_late_old_exit_cannot_clear_new_occurrence() {
    let mut value = registry();
    value.observe_board_abort_edge(edge(1, AlertEdgeKind::Raised, Some(episode(1)), 2000));
    let original = value.current_link(Key::BoardAbort(BoardKey::Daq)).unwrap().condition_occurrence_id;
    value.observe_board_abort_edge(edge(2, AlertEdgeKind::AcceptedExit, Some(episode(1)), 2100));
    value.observe_board_abort_edge(edge(3, AlertEdgeKind::Raised, Some(episode(3)), 2200));
    let current = value.current_link(Key::BoardAbort(BoardKey::Daq)).unwrap().condition_occurrence_id;
    assert_ne!(current, original);
    value.observe_board_abort_edge(edge(4, AlertEdgeKind::AcceptedExit, Some(episode(1)), 2300));
    assert_eq!(value.current_state(Key::BoardAbort(BoardKey::Daq)), Some(ConditionState::Active));
    assert_eq!(value.current_link(Key::BoardAbort(BoardKey::Daq)).unwrap().condition_occurrence_id, current);
    assert!(value.snapshot().history_integrity.is_some());
    assert!(value.snapshot().records.iter().any(|record| record.occurrence_id == original && record.state == ConditionState::Resolved));
}
#[test]
fn ordered_bootstrap_edges_keep_transient_history_and_new_same_utc_episode() {
    let mut value = Registry::new("current".into(), []);
    for event in [edge(1, AlertEdgeKind::Raised, Some(episode(1)), 2000), edge(2, AlertEdgeKind::AcceptedExit, Some(episode(1)), 2100), edge(3, AlertEdgeKind::Raised, Some(episode(3)), 2200)] { value.observe_board_abort_edge(event); }
    let entries = value.restore(Vec::new()).unwrap();
    assert_eq!(entries.iter().map(|entry| entry.record.state).collect::<Vec<_>>(), [ConditionState::Active, ConditionState::Resolved, ConditionState::Active]);
    assert_eq!(entries[0].record.occurrence_id, entries[1].record.occurrence_id);
    assert_ne!(entries[1].record.occurrence_id, entries[2].record.occurrence_id);
    assert_eq!(entries[0].record.raised_at_ms, 1234);
    let errors = value.take_ready_errors();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].condition.as_ref().unwrap().condition_occurrence_id, entries[0].record.occurrence_id);
    assert_eq!(errors[1].condition.as_ref().unwrap().condition_occurrence_id, entries[2].record.occurrence_id);
}
#[test]
fn bound_restored_exit_preserves_exact_original_id_and_time() {
    let prior = prior_record();
    let mut value = Registry::new("current".into(), []);
    value.restore(vec![prior.clone()]).unwrap();
    value.observe_board_abort_edge(edge(1, AlertEdgeKind::Raised, Some(episode(1)), 2000));
    value.observe_board_abort_edge(edge(2, AlertEdgeKind::AcceptedExit, Some(episode(1)), 2100));
    let record = value.snapshot().records.into_iter().find(|row| row.occurrence_id == prior.occurrence_id).unwrap();
    assert_eq!(record.state, ConditionState::Resolved);
    assert_eq!(record.raised_at_ms, 1234);
    assert_eq!(record.resolved_at_ms, Some(2100));
}
#[test]
fn unbound_affirmative_exit_resolves_only_explicit_restored_occurrence() {
    let prior = prior_record();
    let mut value = Registry::new("current".into(), []);
    value.restore(vec![prior.clone()]).unwrap();
    assert_eq!(value.current_state(Key::BoardAbort(BoardKey::Daq)), Some(ConditionState::Unknown));
    value.observe_board_abort_edge(edge(1, AlertEdgeKind::AcceptedExit, None, 2100));
    assert!(value.snapshot().records.iter().any(|record| record.occurrence_id == prior.occurrence_id && record.state == ConditionState::Resolved));
    value.observe_board_abort_edge(edge(2, AlertEdgeKind::Raised, Some(episode(2)), 2200));
    value.observe_board_abort_edge(edge(3, AlertEdgeKind::AcceptedExit, None, 2300));
    assert_eq!(value.current_state(Key::BoardAbort(BoardKey::Daq)), Some(ConditionState::Active));
    assert!(value.snapshot().history_integrity.is_some());
}
#[test]
fn missing_or_exhausted_chronology_is_visible_and_late_batch_cannot_clear() {
    let mut value = registry();
    value.observe_board_abort_edge(edge(1, AlertEdgeKind::Raised, Some(episode(1)), 2000));
    value.observe_board_abort_edge(edge(3, AlertEdgeKind::Raised, Some(episode(3)), 2200));
    value.observe_board_abort_edge(edge(2, AlertEdgeKind::AcceptedExit, Some(episode(1)), 2100));
    assert_eq!(value.current_state(Key::BoardAbort(BoardKey::Daq)), Some(ConditionState::Active));
    assert!(value.snapshot().history_integrity.is_some());
    value.observe_board_abort_edge(edge(u64::MAX, AlertEdgeKind::AcceptedExit, Some(episode(1)), 2300));
    assert!(!value.snapshot().complete);
}
fn accept_exit(runtime: &crate::board_abort::Runtime, at_ms: u64) {
    let after = Instant::now();
    runtime.observe_status(Board::DaqBoard, 123, 2, at_ms, Some(FlightState::Idle as u8), false, true, after, runtime.query_revision(Board::DaqBoard), Instant::now(), at_ms);
}
#[tokio::test]
async fn observer_weak_boot_and_old_query_cannot_emit_an_exit() {
    let runtime = crate::board_abort::Runtime::default();
    runtime.observe_abort(Board::DaqBoard, Instant::now(), 1234);
    let revision = runtime.query_revision(Board::DaqBoard);
    for (boot, epoch, strong, query_revision) in [(0, 2, true, revision), (123, 0, true, revision), (123, 2, false, revision), (123, 2, true, revision.saturating_sub(1))] {
        runtime.observe_status(Board::DaqBoard, boot, epoch, 99, Some(FlightState::Idle as u8), false, strong, Instant::now(), query_revision, Instant::now(), 2000);
    }
    let (events, gap) = runtime.take_alert_edges();
    assert!(!gap);
    assert_eq!(events.iter().flatten().count(), 1);
    assert_eq!(runtime.masks().0 & 64, 64);
}
#[tokio::test]
async fn observer_overflow_reports_unknown_without_changing_launch_truth() {
    let state = crate::state::tests::test_app_state().await;
    let store = initialize(&state).await;
    for index in 0..34 {
        state.board_abort.observe_abort(Board::DaqBoard, Instant::now(), 1000 + index * 2);
        accept_exit(&state.board_abort, 1001 + index * 2);
    }
    let before = state.board_abort.masks();
    assert_eq!(before.0 & 64, 0);
    assert_eq!(before.1 & 64, 64);
    let authority = state.alert_conditions.snapshot().authority;
    let (events, gap) = state.board_abort.take_alert_edges();
    assert!(gap);
    observe_board_abort_edges(&state, &authority, &events, gap);
    assert!(state.alert_conditions.snapshot().history_integrity.is_some());
    assert_eq!(state.board_abort.masks(), before);
    store.unwrap().close().await;
}
#[tokio::test]
async fn old_authority_batch_cannot_mutate_replacement_reporting_scope() {
    let state = crate::state::tests::test_app_state().await;
    let store = initialize(&state).await;
    let old = state.alert_conditions.snapshot().authority;
    state.board_abort.observe_abort(Board::DaqBoard, Instant::now(), 1234);
    let (events, gap) = state.board_abort.take_alert_edges();
    let mut replacement = Registry::new("replacement".into(), []);
    replacement.restore(Vec::new()).unwrap();
    *state.alert_conditions.registry.lock().unwrap() = replacement;
    let before = state.alert_conditions.snapshot();
    observe_board_abort_edges(&state, &old, &events, gap);
    assert_eq!(state.alert_conditions.snapshot(), before);
    assert_eq!(state.board_abort.masks().0 & 64, 64);
    store.unwrap().close().await;
}
#[tokio::test]
async fn actual_registry_only_sqlite_restore_resolves_prior_id_after_accepted_status() {
    let state = crate::state::tests::test_app_state().await;
    let directory = std::path::Path::new(&state.placeholder_db_path).parent().unwrap();
    let path = directory.join("alert-conditions.sqlite3");
    let prior = prior_record();
    let seed = Store::open(&path, "prior".into()).await.unwrap();
    seed.append(&[JournalEntry { authority: "prior".into(), revision: 1, record: prior.clone() }]).await.unwrap();
    seed.close().await;
    let store = initialize(&state).await;
    assert_eq!(state.alert_conditions.current_state(Key::BoardAbort(BoardKey::Daq)), Some(ConditionState::Unknown));
    assert_eq!(state.board_abort.masks().0, 0);
    accept_exit(&state.board_abort, 2100);
    let authority = state.alert_conditions.snapshot().authority;
    let (events, gap) = state.board_abort.take_alert_edges();
    assert!(events.iter().flatten().all(|edge| edge.episode.is_none()));
    observe_board_abort_edges(&state, &authority, &events, gap);
    let record = state.alert_conditions.snapshot().records.into_iter().find(|record| record.occurrence_id == prior.occurrence_id).unwrap();
    assert_eq!(record.state, ConditionState::Resolved);
    assert_eq!(record.raised_at_ms, 1234);
    store.unwrap().close().await;
}
#[tokio::test]
async fn actual_raw_only_history_restore_retains_first_time_until_real_accepted_exit() {
    let mut state = crate::state::tests::test_app_state().await;
    let directory = std::path::Path::new(&state.placeholder_db_path).parent().unwrap().to_owned();
    let path = directory.join("raw-only-board-history");
    let identity = [61; 16];
    let mut history = crate::board_abort_history::Store::provision_uncertain(&path, identity, 64).unwrap();
    let initial = history.begin_session(64).unwrap();
    history.save(crate::board_abort_history::Snapshot {
        revision: initial.snapshot.revision + 1, actual_abort: 64, unknown: 64, enrolled: 64,
        first_abort_ms: [0, 0, 0, 0, 0, 0, 1234],
    }).unwrap();
    drop(history);
    let changed = Arc::new(tokio::sync::Notify::new());
    let (restored, publisher, _owner) = crate::board_abort_history_writer::Owner::open(&path, identity, changed.clone()).unwrap();
    Arc::get_mut(&mut state).unwrap().board_abort = Arc::new(crate::board_abort::Runtime::restored(restored, publisher, changed));
    let store = initialize(&state).await;
    let authority = state.alert_conditions.snapshot().authority;
    let (events, gap) = state.board_abort.take_alert_edges();
    observe_board_abort_edges(&state, &authority, &events, gap);
    let active = state.alert_conditions.snapshot().records.into_iter().find(|record| record.key == Key::BoardAbort(BoardKey::Daq).wire()).unwrap();
    assert_eq!(active.raised_at_ms, 1234);
    assert_eq!(active.state, ConditionState::Active);
    assert_eq!(state.board_abort.masks().0 & 64, 64);
    accept_exit(&state.board_abort, 2100);
    let (events, gap) = state.board_abort.take_alert_edges();
    observe_board_abort_edges(&state, &authority, &events, gap);
    let resolved = state.alert_conditions.snapshot().records.into_iter().find(|record| record.occurrence_id == active.occurrence_id).unwrap();
    assert_eq!(resolved.state, ConditionState::Resolved);
    assert_eq!(resolved.raised_at_ms, 1234);
    store.unwrap().close().await;
}
#[test]
fn authenticated_mirror_enqueues_only_its_accepted_strong_exit() {
    let runtime = crate::board_abort::Runtime::default();
    runtime.observe_authenticated_mirror(64, 0, [u16::MAX; 7], 0);
    runtime.observe_authenticated_mirror(0, 64, [u16::MAX; 7], 0);
    let (events, gap) = runtime.take_alert_edges();
    assert!(!gap);
    assert_eq!(events.iter().flatten().count(), 1);
    assert_eq!(runtime.masks().0 & 64, 64);
    let mut ages = [u16::MAX; 7]; ages[6] = 0;
    runtime.observe_authenticated_mirror(0, 64, ages, 0);
    let (events, gap) = runtime.take_alert_edges();
    assert!(!gap);
    let event = events.iter().flatten().next().unwrap();
    assert_eq!(event.kind, AlertEdgeKind::AcceptedExit);
    assert!(event.episode.is_some());
    assert_eq!(runtime.masks().0 & 64, 0);
}
#[test]
fn zero_first_abort_time_is_unknown_not_a_fabricated_occurrence() {
    let prior = prior_record();
    let mut value = Registry::new("current".into(), []);
    value.restore(vec![prior.clone()]).unwrap();
    let unknown = AlertEpisode { ordinal: 1, first_abort_ms: 0 };
    value.observe_board_abort_edge(edge(1, AlertEdgeKind::Raised, Some(unknown), 2000));
    assert_eq!(value.current_state(Key::BoardAbort(BoardKey::Daq)), Some(ConditionState::Unknown));
    assert!(value.snapshot().history_integrity.is_some());
    assert!(value.snapshot().records.iter().all(|record| record.raised_at_ms != 0));
    assert!(value.snapshot().records.iter().any(|record| record.occurrence_id == prior.occurrence_id));
}
#[test]
fn inverted_collected_batches_remain_incomplete_and_cannot_clear_newer_abort() {
    let runtime = crate::board_abort::Runtime::default();
    runtime.observe_abort(Board::DaqBoard, Instant::now(), 1234);
    accept_exit(&runtime, 2100);
    let (older, gap) = runtime.take_alert_edges();
    assert!(!gap);
    runtime.observe_abort(Board::DaqBoard, Instant::now(), 1234);
    let (newer, gap) = runtime.take_alert_edges();
    assert!(!gap);
    let mut value = registry();
    for event in newer.into_iter().flatten() { value.observe_board_abort_edge(event); }
    for event in older.into_iter().flatten() { value.observe_board_abort_edge(event); }
    assert!(value.snapshot().history_integrity.is_some());
    assert!(!value.snapshot().complete);
    assert!(!value.snapshot().records.iter().any(|record| record.state == ConditionState::Resolved));
    assert_eq!(runtime.masks().0 & 64, 64);
    assert_eq!(runtime.masks().1 & 64, 0);
}
#[tokio::test]
async fn registry_only_restore_held_exit_then_same_utc_new_abort_keeps_exact_chronology() {
    let state = crate::state::tests::test_app_state().await;
    let path = std::path::Path::new(&state.placeholder_db_path).parent().unwrap().join("alert-conditions.sqlite3");
    let prior = prior_record();
    let seed = Store::open(&path, "prior".into()).await.unwrap();
    seed.append(&[JournalEntry { authority: "prior".into(), revision: 1, record: prior.clone() }]).await.unwrap();
    seed.close().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let task = tokio::spawn({
        let state = state.clone(); let entered = entered.clone(); let release = release.clone();
        async move { initialize_with_restore_gate(&state, async move { entered.notify_one(); release.acquire().await.unwrap().forget(); }).await }
    });
    entered.notified().await;
    assert_eq!(state.board_abort.masks().0, 0);
    accept_exit(&state.board_abort, 2100);
    let authority = state.alert_conditions.snapshot().authority;
    let (events, gap) = state.board_abort.take_alert_edges();
    observe_board_abort_edges(&state, &authority, &events, gap);
    state.board_abort.observe_abort(Board::DaqBoard, Instant::now(), 1234);
    let (events, gap) = state.board_abort.take_alert_edges();
    observe_board_abort_edges(&state, &authority, &events, gap);
    release.add_permits(1);
    let store = task.await.unwrap().unwrap();
    let snapshot = state.alert_conditions.snapshot();
    let previous = snapshot.records.iter().find(|record| record.occurrence_id == prior.occurrence_id).unwrap();
    assert_eq!(previous.state, ConditionState::Resolved);
    assert_eq!(previous.raised_at_ms, 1234);
    let current = snapshot.records.iter().find(|record| record.key == prior.key && record.state == ConditionState::Active).unwrap();
    assert_eq!(current.raised_at_ms, 1234);
    assert_ne!(current.occurrence_id, prior.occurrence_id);
    assert_eq!(state.board_abort.masks().0 & 64, 64);
    assert_eq!(state.board_abort.masks().1 & 64, 0);
    store.close().await;
}
