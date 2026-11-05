//! Composition adapter. Live locks cover RAM only; the existing DB writer owns IO.
use crate::{
    alert_condition_journal::Store,
    alert_conditions::{
        Change, ConditionLink, ConditionState, JournalEntry, Key, ReadyError, Registry, Snapshot,
    },
    state::AppState,
    telemetry_db::DbQueueItem,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
#[derive(Clone, Debug)]
pub enum Work {
    Transition(JournalEntry),
    Checkpoint(Snapshot),
}
pub struct Runtime {
    registry: Mutex<Registry>,
    pub tx: broadcast::Sender<Snapshot>,
    repair_inflight: AtomicBool,
    last_repair: Mutex<Option<Instant>>,
    #[cfg(test)]
    test_directory: Option<OwnedTestDirectory>,
}
#[cfg(test)]
struct OwnedTestDirectory(std::path::PathBuf);
#[cfg(test)]
impl Drop for OwnedTestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Runtime {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16);
        Self {
            registry: Mutex::new(Registry::new(
                format!("{:032x}", rand::random::<u128>()),
                [Key::StationHeartbeatEnqueue],
            )),
            tx,
            repair_inflight: AtomicBool::new(false),
            last_repair: Mutex::new(None),
            #[cfg(test)]
            test_directory: None,
        }
    }
    #[cfg(test)]
    pub fn for_test() -> Self {
        let mut runtime = Self::new();
        let path = std::env::temp_dir().join(format!(
            "seds-condition-tests-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&path).expect("create isolated condition test directory");
        runtime.test_directory = Some(OwnedTestDirectory(path));
        runtime
    }
    #[cfg(test)]
    pub fn test_placeholder_path(&self) -> String {
        self.test_directory
            .as_ref()
            .expect("test directory owner")
            .0
            .join("groundstation.db")
            .display()
            .to_string()
    }
    pub(crate) fn authority(&self) -> String {
        self.registry.lock().unwrap().authority().to_owned()
    }
    pub fn snapshot(&self) -> Snapshot {
        self.registry.lock().unwrap().snapshot()
    }
    #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
    pub fn current_link(&self, key: Key) -> Option<ConditionLink> {
        self.registry.lock().unwrap().current_link(key)
    }
    pub fn current_state(&self, key: Key) -> Option<ConditionState> {
        self.registry.lock().unwrap().current_state(key)
    }
    fn broadcast(&self) {
        let _ = self.tx.send(self.snapshot());
    }
    fn failed(&self, entry: &JournalEntry, reason: String) {
        let changed = {
            self.registry
                .lock()
                .unwrap()
                .journal_failed(&entry.authority, entry.revision, reason)
        };
        if changed {
            self.broadcast();
        }
    }
}
/// Returns explicit correlation for an existing occurrence. A held restore may
/// not yet know the prior occurrence; then the ordinary event stays uncorrelated.
pub fn observe(
    state: &AppState,
    key: Key,
    condition: ConditionState,
    message: String,
    at_ms: i64,
) -> Option<ConditionLink> {
    let (change, link) = {
        let mut registry = state.alert_conditions.registry.lock().unwrap();
        let change = registry.observe(key, condition, message, at_ms);
        let link = registry.current_link(key);
        (change, link)
    };
    publish_change(state, change);
    link
}
/// Drain one atomically collected producer batch. Registry mutation is one
/// memory-only critical section; all serialization/journal publication follows.
pub(crate) fn observe_board_abort_edges(
    state: &AppState,
    authority: &str,
    edges: &[Option<crate::board_abort::AlertEdge>],
    incomplete: bool,
) {
    let (changes, errors) = {
        let mut registry = state.alert_conditions.registry.lock().unwrap();
        if !registry.has_authority(authority) { return; }
        let changes = if incomplete {
            vec![registry.board_abort_reporting_gap("observer edge queue overflow or ordinal exhaustion")]
        } else {
            edges.iter().flatten().map(|edge| registry.observe_board_abort_edge(*edge)).collect::<Vec<_>>()
        };
        (changes, registry.take_ready_errors())
    };
    for change in changes { publish_change(state, change); }
    publish_ready_errors(state, errors);
    request_repair(state);
}
fn publish_ready_errors(state: &AppState, errors: Vec<ReadyError>) {
    for error in errors {
        let accepted = state
            .alert_conditions
            .registry
            .lock()
            .unwrap()
            .accepts_ready_error(&error);
        if !accepted {
            log::warn!("Rejected late/foreign condition error receipt");
            continue;
        }
        crate::web::emit_error_linked_at(state, error.timestamp_ms, error.message, error.condition);
    }
}
fn publish_change(state: &AppState, change: Change) {
    if change.changed {
        state.alert_conditions.broadcast();
    }
    if let Some(entry) = change.journal
        && let Err(error) = state
            .db_queue_tx
            .try_send(DbQueueItem::Condition(Work::Transition(entry.clone())))
    {
        state.alert_conditions.failed(
            &entry,
            format!("Condition journal queue refused transition: {error}"),
        );
    }
}
/// Called from the existing heartbeat cadence. Coalesced and monotonic backoff;
/// no extra task, SDK traffic, or recursive alert is created by journal failure.
pub fn request_repair(state: &AppState) {
    if !state
        .alert_conditions
        .registry
        .lock()
        .unwrap()
        .needs_repair()
    {
        return;
    }
    let snapshot = state.alert_conditions.snapshot();
    {
        let mut last = state.alert_conditions.last_repair.lock().unwrap();
        if last.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return;
        }
        if state
            .alert_conditions
            .repair_inflight
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    if state
        .db_queue_tx
        .try_send(DbQueueItem::Condition(Work::Checkpoint(snapshot)))
        .is_err()
    {
        state
            .alert_conditions
            .repair_inflight
            .store(false, Ordering::Release);
    }
}
/// Initialize inside the existing writer task. Producers can stage evidence
/// during this await; Registry::restore applies it after loading old records.
pub async fn initialize(state: &AppState) -> Option<Store> {
    initialize_with_restore_gate(state, std::future::ready(())).await
}
// The production gate is immediately ready. Tests can hold the actual writer
// restoration while raw board publication/control continues independently.
async fn initialize_with_restore_gate<F: std::future::Future<Output = ()>>(
    state: &AppState,
    before_restore: F,
) -> Option<Store> {
    let authority = state.alert_conditions.snapshot().authority;
    let path = std::path::Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("alert-conditions.sqlite3");
    let result = async {
        let store = Store::open(&path, authority.clone()).await?;
        before_restore.await;
        let rows = store.restore().await?;
        let integrity = store.restore_integrity().await?;
        let (staged, errors) = {
            let mut registry = state.alert_conditions.registry.lock().unwrap();
            if !registry.has_authority(&authority) {
                return Err("Late condition restore for superseded runtime".into());
            }
            registry.restore_integrity(integrity)?;
            let staged = registry.restore(rows)?;
            (staged, registry.take_ready_errors())
        };
        // No await between removing the bounded receipts and publishing them.
        // Task cancellation during later journal IO cannot silently lose history.
        publish_ready_errors(state, errors);
        for entry in staged {
            let result = store.append(std::slice::from_ref(&entry)).await;
            match result {
                Ok(()) => {
                    state
                        .alert_conditions
                        .registry
                        .lock()
                        .unwrap()
                        .journal_succeeded(&entry.authority, entry.revision);
                }
                Err(reason) => state.alert_conditions.failed(&entry, reason),
            }
        }
        let integrity = state
            .alert_conditions
            .registry
            .lock()
            .unwrap()
            .integrity_marker();
        if let Some(marker) = integrity {
            persist_integrity_for_scope(state, &authority, store.persist_integrity(&marker)).await;
        }
        Ok::<_, String>(store)
    }
    .await;
    match result {
        Ok(store) => {
            state.alert_conditions.broadcast();
            Some(store)
        }
        Err(reason) => {
            let mut registry = state.alert_conditions.registry.lock().unwrap();
            if registry.has_authority(&authority) {
                registry.restore_failed(format!("Condition journal restore failed: {reason}"));
            }
            drop(registry);
            state.alert_conditions.broadcast();
            None
        }
    }
}
// Carry the original store/runtime scope across the await. A completion from
// an old writer cannot mark a replacement runtime's integrity gap durable.
async fn persist_integrity_for_scope<F: std::future::Future<Output = Result<(), String>>>(
    state: &AppState,
    authority: &str,
    operation: F,
) -> bool {
    let result = operation.await;
    let mut registry = state.alert_conditions.registry.lock().unwrap();
    match result {
        Ok(()) => registry.integrity_saved(authority),
        Err(reason) => {
            let current = registry.has_authority(authority);
            registry.integrity_failed(
                authority,
                format!("Condition integrity persistence failed: {reason}"),
            );
            current
        }
    }
}
/// Invoke for each Condition queue item, independent of RecordingMode. Never
/// call this while a registry/GSE/board-status/actuator admission lock is held.
pub async fn write(state: &AppState, store: &mut Option<Store>, work: Work) {
    match work {
        Work::Transition(entry) => {
            let result = if let Some(store) = store {
                store.append(std::slice::from_ref(&entry)).await
            } else {
                Err("Condition journal unavailable".into())
            };
            match result {
                Ok(()) => {
                    let changed = {
                        state
                            .alert_conditions
                            .registry
                            .lock()
                            .unwrap()
                            .journal_succeeded(&entry.authority, entry.revision)
                    };
                    if changed {
                        state.alert_conditions.broadcast();
                    }
                }
                Err(reason) => state.alert_conditions.failed(&entry, reason),
            }
        }
        Work::Checkpoint(requested) => {
            if requested.authority != state.alert_conditions.snapshot().authority {
                return;
            }
            if store.is_none() {
                *store = initialize(state).await;
            }
            let snapshot = state.alert_conditions.snapshot();
            if snapshot.authority != requested.authority {
                return;
            }
            if let Some(store) = store {
                match store.checkpoint(&snapshot).await {
                    Ok(()) => {
                        state
                            .alert_conditions
                            .registry
                            .lock()
                            .unwrap()
                            .checkpoint_succeeded(&snapshot.authority, snapshot.revision);
                    }
                    Err(reason) => {
                        let mut registry = state.alert_conditions.registry.lock().unwrap();
                        if registry.has_authority(&snapshot.authority) {
                            registry.restore_failed(format!(
                                "Condition journal checkpoint failed: {reason}"
                            ));
                        }
                    }
                }
            }
            state
                .alert_conditions
                .repair_inflight
                .store(false, Ordering::Release);
            state.alert_conditions.broadcast();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn actual_appstate_factories_own_distinct_persistent_test_paths_until_writer_teardown() {
        let first = crate::state::tests::test_app_state().await;
        let second = crate::state::tests::test_app_state().await;
        assert_ne!(first.placeholder_db_path, second.placeholder_db_path);
        assert_ne!(
            first.alert_conditions.snapshot().authority,
            second.alert_conditions.snapshot().authority
        );
        let first_dir = std::path::Path::new(&first.placeholder_db_path)
            .parent()
            .unwrap()
            .to_owned();
        let second_dir = std::path::Path::new(&second.placeholder_db_path)
            .parent()
            .unwrap()
            .to_owned();
        let mut first_store = initialize(&first).await;
        let second_store = initialize(&second).await;
        assert!(first_store.is_some() && second_store.is_some());
        let writer_owner = first.clone();
        drop(first);
        assert!(
            first_dir.exists(),
            "writer retains actual AppState directory owner"
        );
        observe(
            &writer_owner,
            Key::StationHeartbeatEnqueue,
            ConditionState::Resolved,
            "Enqueue recovered".into(),
            1,
        );
        observe(
            &writer_owner,
            Key::GatewayLowVoltage,
            ConditionState::Active,
            "Low".into(),
            2,
        );
        let snapshot = writer_owner.alert_conditions.snapshot();
        write(&writer_owner, &mut first_store, Work::Checkpoint(snapshot)).await;
        assert!(writer_owner.alert_conditions.snapshot().complete);
        first_store.unwrap().close().await;
        drop(writer_owner);
        assert!(
            !first_dir.exists(),
            "owned test directory cleaned after writer close"
        );
        second_store.unwrap().close().await;
        drop(second);
        assert!(!second_dir.exists());
    }
}

/// Share the existing writer's bounded collection turn. Each condition batch
/// commits once, then publishes one durability update outside the registry lock.
pub async fn flush(state: &AppState, store: &mut Option<Store>, pending: &mut Vec<JournalEntry>) {
    if pending.is_empty() {
        return;
    }
    let result = if let Some(store) = store {
        store.append(pending).await
    } else {
        Err("Condition journal unavailable".into())
    };
    let changed = {
        let mut registry = state.alert_conditions.registry.lock().unwrap();
        let mut changed = false;
        for entry in pending.iter() {
            changed |= match &result {
                Ok(()) => registry.journal_succeeded(&entry.authority, entry.revision),
                Err(reason) => {
                    registry.journal_failed(&entry.authority, entry.revision, reason.clone())
                }
            };
        }
        changed
    };
    pending.clear();
    if changed {
        state.alert_conditions.broadcast();
    }
}
pub async fn stage(
    state: &AppState,
    store: &mut Option<Store>,
    pending: &mut Vec<JournalEntry>,
    work: Work,
) {
    match work {
        Work::Transition(entry) => {
            if pending.len() == 64 {
                flush(state, store, pending).await;
            }
            pending.push(entry);
        }
        checkpoint @ Work::Checkpoint(_) => {
            flush(state, store, pending).await;
            write(state, store, checkpoint).await;
        }
    }
}

#[cfg(test)]
mod board_abort_bridge_tests {
    use super::*;
    #[tokio::test]
    async fn actual_board_abort_publisher_correlates_acknowledged_error_and_only_strong_exit_resolves_it()
     {
        use crate::{
            alert_conditions::BoardKey,
            types::{Board, FlightState},
        };
        let state = crate::state::tests::test_app_state().await;
        let mut store = initialize(&state).await;
        observe(
            &state,
            Key::StationHeartbeatEnqueue,
            ConditionState::Resolved,
            "Enqueue recovered".into(),
            1,
        );
        let mut errors = state.errors_tx.subscribe();
        let service = tokio::spawn(crate::board_abort::publish_service(
            state.clone(),
            state.shutdown_subscribe(),
        ));
        let first_at = crate::telemetry_task::get_current_timestamp_ms();
        let abort_seen = Instant::now();
        state
            .board_abort
            .observe_abort(Board::DaqBoard, abort_seen, first_at);
        let event = tokio::time::timeout(Duration::from_secs(3), errors.recv())
            .await
            .unwrap()
            .unwrap();
        let link = event.condition.expect("typed Abort occurrence correlation");
        assert_eq!(link.condition_key, Key::BoardAbort(BoardKey::Daq).wire());
        state.acknowledge_alerts_through(i64::MAX, i64::MAX);
        assert_eq!(
            state
                .alert_conditions
                .current_state(Key::BoardAbort(BoardKey::Daq)),
            Some(ConditionState::Active)
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
        let after = Instant::now();
        let revision = state.board_abort.query_revision(Board::DaqBoard);
        // A source reset/invalid boot or unproven reply cannot clear the bridge.
        state.board_abort.observe_status(
            Board::DaqBoard,
            0,
            0,
            1,
            Some(FlightState::Idle as u8),
            false,
            true,
            after,
            revision,
            Instant::now(),
            first_at + 1,
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            state
                .alert_conditions
                .current_state(Key::BoardAbort(BoardKey::Daq)),
            Some(ConditionState::Active)
        );
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
        tokio::time::timeout(Duration::from_secs(3), async {
            while state
                .alert_conditions
                .current_state(Key::BoardAbort(BoardKey::Daq))
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let resolved = state
            .alert_conditions
            .snapshot()
            .records
            .into_iter()
            .find(|record| record.occurrence_id == link.condition_occurrence_id)
            .unwrap();
        assert_eq!(resolved.state, ConditionState::Resolved);
        assert_eq!(resolved.raised_at_ms, first_at as i64);
        assert!(state.board_abort.masks().0 == 0);
        write(
            &state,
            &mut store,
            Work::Checkpoint(state.alert_conditions.snapshot()),
        )
        .await;
        assert!(state.alert_conditions.snapshot().complete);
        // A new Abort remains active even though its immutable Error event's
        // timestamp is older than the user's already-advanced ack cutoff.
        state
            .board_abort
            .observe_abort(Board::DaqBoard, Instant::now(), first_at + 3);
        let recurring = tokio::time::timeout(Duration::from_secs(3), errors.recv())
            .await
            .unwrap()
            .unwrap()
            .condition
            .unwrap();
        assert_ne!(
            recurring.condition_occurrence_id,
            link.condition_occurrence_id
        );
        assert_eq!(
            state
                .alert_conditions
                .current_state(Key::BoardAbort(BoardKey::Daq)),
            Some(ConditionState::Active)
        );
        state.request_shutdown();
        service.await.unwrap();
        store.unwrap().close().await;
    }
}

#[cfg(test)]
#[path = "alert_startup_correlation_tests.rs"]
mod startup_correlation_tests;

#[cfg(test)]
#[path = "alert_delayed_exit_tests.rs"]
mod delayed_exit_tests;

#[cfg(test)]
#[path = "alert_edge_chronology_tests.rs"]
mod edge_chronology_tests;
