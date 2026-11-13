//! The existing DB worker remains the sole owner of recording pools.
use super::*;
use crate::fill_recording::{EnsureRequest, Error, Receipt};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) struct ActiveRecording {
    pub(super) pool: sqlx::SqlitePool,
    pub(super) path: String,
}

pub(super) fn revoke(state: &AppState) {
    crate::gse::revoke_fill_recording(state);
}

async fn discard(pool: Option<sqlx::SqlitePool>, path: &Path) {
    if let Some(pool) = pool {
        pool.close().await;
    }
    // The caller reserved this exact new basename before SQLite could open it.
    for path in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        let _ = tokio::fs::remove_file(path).await;
    }
}

fn reserve(recordings_dir: &Path, instance: u64) -> Result<PathBuf, Error> {
    let base = session_db_path(recordings_dir, get_current_timestamp_ms() as i64);
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or(Error::Unavailable)?;
    let path = recordings_dir.join(format!("{stem}_{instance}_{}.db", rand::random::<u128>()));
    for suffix in ["-wal", "-shm"] {
        if PathBuf::from(format!("{}{suffix}", path.display())).exists() {
            return Err(Error::Storage("recording reservation collision".into()));
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(
        options
            .open(&path)
            .map_err(|e| Error::Storage(e.to_string()))?,
    );
    Ok(path)
}

enum PreparedRecording {
    Existing { instance: u64, path: String },
    New { instance: u64, path: PathBuf, pool: sqlx::SqlitePool },
}
impl PreparedRecording {
    fn instance(&self) -> u64 {
        match self { Self::Existing {instance,..} | Self::New {instance,..} => *instance }
    }
}
/// At most one fair, resumable publication stage in the sole DB actor.
/// Neither a Busy retry nor an actor yield creates a new request/file/instance.
struct PendingEnsure {
    request: Arc<EnsureRequest>,
    prepared: Option<PreparedRecording>,
    publication: Option<crate::fill_recording::Publication>,
}
enum EnsurePoll { PendingBusy, Ready(Result<Receipt, Error>) }
impl PendingEnsure {
    fn new(request: Arc<EnsureRequest>) -> Self {
        Self { request, prepared: None, publication: None }
    }
    async fn poll(
        &mut self,
        state: &Arc<AppState>,
        recordings_dir: &Path,
        active: &mut Option<ActiveRecording>,
        mode: &mut RecordingMode,
    ) -> EnsurePoll {
        let result = self.attempt(state, recordings_dir, active, mode).await;
        match result { Err(Error::PendingBusy) => EnsurePoll::PendingBusy, result => EnsurePoll::Ready(result) }
    }
    async fn attempt(
        &mut self,
        state: &Arc<AppState>,
        recordings_dir: &Path,
        active: &mut Option<ActiveRecording>,
        mode: &mut RecordingMode,
    ) -> Result<Receipt, Error> {
        if !self.request.pending() { return Err(Error::Stale); }
        // One nonwaiting inspection attempt; the actor, not this future, owns
        // the next fair slice. Permissions are rechecked on every clear attempt.
        self.request.origin.authorize_once(state, self.request.command).await?;
        if !self.request.pending() { return Err(Error::Stale); }
        if self.prepared.is_none() {
            if let Some(owner) = active.as_ref() {
                if *mode != RecordingMode::Recording { return Err(Error::Unavailable); }
                let current = state.fill_recording.ready_instance();
                let instance = if current == 0 { state.fill_recording.reserve_instance()? } else { current };
                self.prepared = Some(PreparedRecording::Existing { instance, path: owner.path.clone() });
            } else {
                let instance = state.fill_recording.reserve_instance()?;
                let path = reserve(recordings_dir, instance)?;
                let pool = match crate::telemetry_db::open_reserved_telemetry_db(&path).await {
                    Ok(pool) => pool,
                    Err(error) => { discard(None, &path).await; return Err(Error::Storage(error.to_string())); }
                };
                self.prepared = Some(PreparedRecording::New { instance, path, pool });
                #[cfg(test)]
                if !test_hooks::after_open(state) { return Err(Error::Stale); }
            }
            self.publication = Some(crate::fill_recording::Publication::new(
                self.request.clone(), self.prepared.as_ref().unwrap().instance()));
        }
        self.request.origin.authorize_once(state, self.request.command).await?;
        if !self.request.pending() { return Err(Error::Stale); }
        let publication = self.publication.as_mut().ok_or(Error::Unavailable)?;
        match self.prepared.as_ref().ok_or(Error::Unavailable)? {
            PreparedRecording::Existing { instance, path } => {
                if *mode != RecordingMode::Recording || active.as_ref().is_none_or(|owner| owner.path != *path) {
                    return Err(Error::Unavailable);
                }
                crate::gse::with_fill_recording_publication(state, &self.request.origin, publication)?;
                Ok(Receipt { instance: *instance, path: path.clone() })
            }
            PreparedRecording::New { instance, path, pool } => {
                if active.is_some() { return Err(Error::Unavailable); }
                let path_string = path.to_string_lossy().into_owned();
                let old_pool = state.telemetry_db_pool();
                let old_path = state.telemetry_db_path();
                let old_status = state.recording_status_snapshot();
                let old_mode = *mode;
                // Same existing installation/rollback boundary: no actor write
                // can occur between these RAM assignments and final admission.
                state.replace_telemetry_db(pool.clone(), path_string.clone());
                *active = Some(ActiveRecording { pool: pool.clone(), path: path_string.clone() });
                *mode = RecordingMode::Recording;
                if let Err(error) = crate::gse::with_fill_recording_publication(state, &self.request.origin, publication) {
                    *active = None;
                    *mode = old_mode;
                    state.replace_telemetry_db(old_pool, old_path);
                    state.set_recording_status(old_status);
                    // Busy retains THIS staged pool/path/one-shot plan; the
                    // actor can flush its old owner before the next attempt.
                    return Err(error);
                }
                state.set_recording_status(RecordingStatusMsg { mode: RecordingModeWire::Recording, db_path: Some(path_string.clone()) });
                old_pool.close().await;
                Ok(Receipt { instance: *instance, path: path_string })
            }
        }
    }
    async fn finish(self, result: Result<Receipt, Error>) {
        if !self.publication.as_ref().is_some_and(|p| p.applied())
            && let Some(PreparedRecording::New { path, pool, .. }) = self.prepared {
                discard(Some(pool), &path).await;
            }
        self.request.reply(result);
    }
}

/// Direct actor fixture adapter. Production drives PendingEnsure fairly from
/// run(); no fresh deadline or publication owner is created by this loop.
#[cfg(test)]
pub(super) async fn ensure(
    state: &Arc<AppState>, request: Arc<EnsureRequest>, recordings_dir: &Path,
    active: &mut Option<ActiveRecording>, mode: &mut RecordingMode,
) {
    let mut stage = PendingEnsure::new(request);
    loop {
        match stage.poll(state, recordings_dir, active, mode).await {
            EnsurePoll::PendingBusy => tokio::time::sleep(Duration::from_millis(5)).await,
            EnsurePoll::Ready(result) => { stage.finish(result).await; return; }
        }
    }
}

/// Actual production DB loop, shared with source-bound host fixtures. There is
/// one receiver and one recording owner; tests do not synthesize readiness.
pub(crate) async fn run(
    state: Arc<AppState>,
    mut db_rx: tokio::sync::mpsc::Receiver<DbQueueItem>,
    db_batch_max: usize,
    db_batch_wait_ms: u64,
    mut db_shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    log::info!("telemetry db worker started");

    let placeholder_path = state.placeholder_db_path.clone();
    let recordings_dir = std::path::Path::new(&placeholder_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let mut recent_writes: VecDeque<DbWrite> = VecDeque::new();
    let mut active_recording: Option<ActiveRecording> = None;
    let mut mode = RecordingMode::Idle;
    let mut last_recording_end_ms: Option<i64> = None;

    let mut pending: Vec<DbWrite> = Vec::with_capacity(db_batch_max);
    let mut condition_store = crate::alert_condition_runtime::initialize(&state).await;
    let mut pending_conditions = Vec::with_capacity(64);
    let mut deferred_control_carry: Option<DbQueueItem> = None;
    let mut diagnostics = recording_diagnostics::Collector::new();
    let mut diagnostic_tick = tokio::time::interval(Duration::from_millis(100));
    diagnostic_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutting_down = false;
    let mut pending_ensure: Option<PendingEnsure> = None;
    let mut ensure_retry_at = tokio::time::Instant::now();
    loop {
        let diagnostic_path = if !shutting_down && pending.is_empty() && matches!(mode, RecordingMode::Recording) {
            active_recording.as_ref().map(|a| a.path.as_str())
        } else { None };
        for write in diagnostics.poll(&state, diagnostic_path).await {
            recent_writes.push_back(write.clone());
            prune_recent_writes(&mut recent_writes, write.timestamp_ms());
            pending.push(write);
        }
        let first = if shutting_down {
            match db_rx.try_recv() {
                Ok(item) => Some(item),
                Err(MpscTryRecvError::Empty | MpscTryRecvError::Disconnected) => {
                    if pending_ensure.is_some() { None } else { break; }
                }
            }
        } else {
            tokio::select! {
                _ = diagnostic_tick.tick() => None,
                recv = db_rx.recv() => {
                    match recv {
                        Some(item) => Some(item),
                        None if pending_ensure.is_some() => { shutting_down = true; None },
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(ensure_retry_at), if pending_ensure.is_some() => None,
                recv = db_shutdown_rx.recv() => {
                    match recv {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
                            shutting_down = true;
                            continue;
                        }
                    }
                }
            }
        };

        let mut deferred_control: Option<DbQueueItem> = deferred_control_carry.take();
        match first {
            Some(DbQueueItem::Write(write)) => {
                let newest_ts = write.timestamp_ms();
                recent_writes.push_back(write.clone());
                prune_recent_writes(&mut recent_writes, newest_ts);
                if matches!(mode, RecordingMode::Recording) {
                    pending.push(write);
                }
            }
            Some(DbQueueItem::Condition(work)) => {
                crate::alert_condition_runtime::stage(
                    &state,
                    &mut condition_store,
                    &mut pending_conditions,
                    work,
                )
                .await
            }
            Some(control @ (DbQueueItem::Control(_) | DbQueueItem::EnsureRecording(_))) => {
                deferred_control = Some(control)
            }
            None => {},
        }

        let deadline = tokio::time::Instant::now() + Duration::from_millis(db_batch_wait_ms);
        let mut ensure_batch_items = 0usize;
        while pending.len() < db_batch_max && deferred_control.is_none()
            && (pending_ensure.is_none() || ensure_batch_items < db_batch_max) {
            // Idle writes need not enter `pending`, but a waiting publication
            // must still get a bounded fair turn under a continuous input feed.
            ensure_batch_items = ensure_batch_items.saturating_add(1);
            match db_rx.try_recv() {
                Ok(DbQueueItem::Write(write)) => {
                    let newest_ts = write.timestamp_ms();
                    recent_writes.push_back(write.clone());
                    prune_recent_writes(&mut recent_writes, newest_ts);
                    if matches!(mode, RecordingMode::Recording) {
                        pending.push(write);
                    }
                }
                Ok(DbQueueItem::Condition(work)) => {
                    crate::alert_condition_runtime::stage(
                        &state,
                        &mut condition_store,
                        &mut pending_conditions,
                        work,
                    )
                    .await
                }
                Ok(control @ (DbQueueItem::Control(_) | DbQueueItem::EnsureRecording(_))) => {
                    deferred_control = Some(control);
                }
                Err(MpscTryRecvError::Disconnected) => break,
                Err(MpscTryRecvError::Empty) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    match tokio::time::timeout(remaining, db_rx.recv()).await {
                        Ok(Some(DbQueueItem::Write(write))) => {
                            let newest_ts = write.timestamp_ms();
                            recent_writes.push_back(write.clone());
                            prune_recent_writes(&mut recent_writes, newest_ts);
                            if matches!(mode, RecordingMode::Recording) {
                                pending.push(write);
                            }
                        }
                        Ok(Some(DbQueueItem::Condition(work))) => {
                            crate::alert_condition_runtime::stage(
                                &state,
                                &mut condition_store,
                                &mut pending_conditions,
                                work,
                            )
                            .await
                        }
                        Ok(Some(
                            control @ (DbQueueItem::Control(_) | DbQueueItem::EnsureRecording(_)),
                        )) => {
                            deferred_control = Some(control);
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            }
        }

        crate::alert_condition_runtime::flush(
            &state,
            &mut condition_store,
            &mut pending_conditions,
        )
        .await;
        let flush_ok =
            flush_recording_batch(active_recording.as_ref().map(|a| &a.pool), &mut pending).await;
        if !flush_ok {
            if let Some(stage) = pending_ensure.take() { stage.finish(Err(Error::Unavailable)).await; }
            recording_owner::revoke(&state);
            deferred_control_carry = deferred_control;
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        if let Some(control) = deferred_control {
            // Invalidate before any control/session preparation/close await.
            // A completed old worker cannot publish into a subsequent file.
            diagnostics.invalidate();
            if let DbQueueItem::EnsureRecording(request) = control {
                if pending_ensure.is_some() {
                    request.reply(Err(Error::Busy));
                } else {
                    pending_ensure = Some(PendingEnsure::new(request));
                    ensure_retry_at = tokio::time::Instant::now();
                }
                continue;
            }
            let DbQueueItem::Control(cmd) = control else {
                unreachable!("only controls deferred")
            };
            // A real recording control supersedes an unpublished preparation;
            // retain the control itself and clean only that original owned file.
            if let Some(stage) = pending_ensure.take() { stage.finish(Err(Error::Unavailable)).await; }
            match cmd {
                RecordingCommand::StartNow | RecordingCommand::StartWithRecent => {
                    let started_at_ms = get_current_timestamp_ms() as i64;
                    let target_path = session_db_path(&recordings_dir, started_at_ms);
                    match open_telemetry_db(&target_path).await {
                        Ok((new_pool, new_path)) => {
                            recording_owner::revoke(&state);
                            if let Some(active) = active_recording.take() {
                                log::info!("recording session closing db={}", active.path);
                                close_and_finalize_sqlite(active.pool, &active.path).await;
                                export_test_fire_csv_if_enabled(&state, &active.path).await;
                                last_recording_end_ms = Some(get_current_timestamp_ms() as i64);
                            }
                            log::info!("recording session started db={new_path}");
                            state.replace_telemetry_db(new_pool.clone(), new_path.clone());
                            state.set_recording_status(RecordingStatusMsg {
                                mode: RecordingModeWire::Recording,
                                db_path: Some(new_path.clone()),
                            });
                            active_recording = Some(ActiveRecording {
                                pool: new_pool,
                                path: new_path,
                            });
                            mode = RecordingMode::Recording;
                            if let Ok(instance) = state.fill_recording.reserve_instance() {
                                let _dispatch = crate::gse::recording_publication_gate();
                                state.fill_recording.publish_under_dispatch(instance);
                            }

                            if matches!(cmd, RecordingCommand::StartWithRecent)
                                && let Some(active) = &active_recording
                            {
                                let cutoff = started_at_ms.saturating_sub(120_000);
                                let min_ts = last_recording_end_ms
                                    .map(|prev| prev.max(cutoff))
                                    .unwrap_or(cutoff);
                                let backfill: Vec<DbWrite> = recent_writes
                                    .iter()
                                    .filter(|write| write.timestamp_ms() > min_ts)
                                    .cloned()
                                    .collect();
                                if let Err(err) =
                                    insert_db_batch_with_retry(&active.pool, &backfill).await
                                {
                                    eprintln!("Failed to backfill recent writes: {err}");
                                }
                            }
                        }
                        Err(err) => {
                            emit_warning(
                                &state,
                                format!("Failed to open recording DB session: {err}"),
                            );
                        }
                    }
                }
                RecordingCommand::Pause | RecordingCommand::Stop => {
                    recording_owner::revoke(&state);
                    if let Some(active) = active_recording.take() {
                        log::info!("recording session closing db={}", active.path);
                        close_and_finalize_sqlite(active.pool, &active.path).await;
                        let deleted = delete_empty_recording_if_needed(&active.path).await;
                        if !deleted {
                            export_test_fire_csv_if_enabled(&state, &active.path).await;
                        }
                        last_recording_end_ms = Some(get_current_timestamp_ms() as i64);
                    }
                    mode = if matches!(cmd, RecordingCommand::Pause) {
                        RecordingMode::Paused
                    } else {
                        RecordingMode::Idle
                    };
                    rotate_placeholder_db(&state, mode).await;
                    state.set_recording_status(RecordingStatusMsg {
                        mode: RecordingModeWire::from(mode),
                        db_path: None,
                    });
                }
                RecordingCommand::ResetAll => {
                    pending.clear();
                    recent_writes.clear();
                    let db = active_recording
                        .as_ref()
                        .map(|active| active.pool.clone())
                        .unwrap_or_else(|| state.telemetry_db_pool());
                    if let Err(err) = clear_telemetry_tables(&db).await {
                        log::error!("failed clearing telemetry tables during reset: {err}");
                    }
                }
            }
        }
        if ensure_retry_at <= tokio::time::Instant::now()
            && let Some(mut stage) = pending_ensure.take() {
            match stage.poll(&state, &recordings_dir, &mut active_recording, &mut mode).await {
                EnsurePoll::PendingBusy => {
                    pending_ensure = Some(stage);
                    ensure_retry_at = tokio::time::Instant::now() + Duration::from_millis(5);
                }
                EnsurePoll::Ready(result) => stage.finish(result).await,
            }
        }
    }

    drop(diagnostics);
    if let Some(stage) = pending_ensure.take() { stage.finish(Err(Error::Unavailable)).await; }
    recording_owner::revoke(&state);
    if let Some(active) = active_recording.take() {
        log::info!("recording worker finalizing active db={}", active.path);
        close_and_finalize_sqlite(active.pool, &active.path).await;
        let deleted = delete_empty_recording_if_needed(&active.path).await;
        if !deleted {
            export_test_fire_csv_if_enabled(&state, &active.path).await;
        }
    }
    if let Some(store) = condition_store {
        store.close().await;
    }
    log::info!("telemetry db worker stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fill_recording::{Authority, Captured, EnsureRequest};
    fn origin(state: &AppState) -> Arc<crate::fill_recording::Origin> {
        Captured::capture(state).bind(
            Authority::Physical {
                current: Arc::new(|| true),
            },
            None,
        )
    }
    fn root() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("gs-fill-recording-{}", rand::random::<u128>()));
        std::fs::create_dir(&root).unwrap();
        root
    }
    async fn request(
        state: &Arc<AppState>,
        directory: &Path,
        active: &mut Option<ActiveRecording>,
        mode: &mut RecordingMode,
    ) -> Result<Receipt, Error> {
        let (request, reply) = EnsureRequest::new(origin(state), "StartFill");
        ensure(state, request, directory, active, mode).await;
        reply.await.unwrap()
    }
    async fn finish(state: &AppState, active: Option<ActiveRecording>, directory: PathBuf) {
        revoke(state);
        if let Some(active) = active {
            active.pool.close().await;
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn actual_sqlite_ensure_reuses_one_active_pool_and_retired_receipt_cannot_rearm() {
        let state = crate::state::tests::test_app_state().await;
        let directory = root();
        let mut active = None;
        let mut mode = RecordingMode::Idle;
        let first = request(&state, &directory, &mut active, &mut mode)
            .await
            .unwrap();
        sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type,sender_id,payload_json) VALUES(1,'fixture','DAQ','[]')").execute(&active.as_ref().unwrap().pool).await.unwrap();
        let second = request(&state, &directory, &mut active, &mut mode)
            .await
            .unwrap();
        assert_eq!(first, second);
        assert!(state.fill_recording.current(&first));
        revoke(&state);
        assert!(!state.fill_recording.current(&first));
        let third = request(&state, &directory, &mut active, &mut mode)
            .await
            .unwrap();
        assert_eq!(third.path, first.path);
        assert_ne!(third.instance, first.instance);
        assert!(!state.fill_recording.current(&first));
        assert!(state.fill_recording.current(&third));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM telemetry")
            .fetch_one(&active.as_ref().unwrap().pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            std::fs::read_dir(&directory)
                .unwrap()
                .filter(|p| p
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "db"))
                .count(),
            1
        );
        finish(&state, active, directory).await;
    }
    #[tokio::test]
    async fn actual_owner_canceled_and_retired_requests_never_create_recording() {
        let state = crate::state::tests::test_app_state().await;
        let directory = root();
        let mut active = None;
        let mut mode = RecordingMode::Idle;
        let (request, reply) = EnsureRequest::new(origin(&state), "StartFill");
        drop(reply);
        ensure(&state, request, &directory, &mut active, &mut mode).await;
        let retired = origin(&state);
        retired.retire();
        let (request, reply) = EnsureRequest::new(retired, "StartFill");
        ensure(&state, request, &directory, &mut active, &mut mode).await;
        assert_eq!(reply.await.unwrap(), Err(Error::Stale));
        assert!(active.is_none());
        assert_eq!(mode, RecordingMode::Idle);
        assert_eq!(
            state.recording_status_snapshot().mode,
            RecordingModeWire::Idle
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        finish(&state, active, directory).await;
    }
    #[tokio::test]
    async fn actual_owner_storage_failure_is_typed_and_leaves_recording_idle() {
        let state = crate::state::tests::test_app_state().await;
        let directory = root();
        let blocker = directory.join("not-a-directory");
        std::fs::write(&blocker, b"keep").unwrap();
        let mut active = None;
        let mut mode = RecordingMode::Idle;
        assert!(matches!(
            request(&state, &blocker, &mut active, &mut mode).await,
            Err(Error::Storage(_))
        ));
        assert!(active.is_none());
        assert_eq!(mode, RecordingMode::Idle);
        assert_eq!(state.fill_recording.ready_instance(), 0);
        assert_eq!(
            state.recording_status_snapshot().mode,
            RecordingModeWire::Idle
        );
        assert_eq!(std::fs::read(&blocker).unwrap(), b"keep");
        finish(&state, active, directory).await;
    }
    #[tokio::test]
    async fn actual_stop_after_sqlite_open_prevents_publication_and_cleans_only_candidate() {
        let state = crate::state::tests::test_app_state().await;
        let directory = root();
        std::fs::write(directory.join("archive.db"), b"keep").unwrap();
        let weak = Arc::downgrade(&state);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let origin = Captured::capture(&state).bind(
            Authority::Physical {
                current: Arc::new(move || {
                    if observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                        weak.upgrade().unwrap().manual_abort_intent.request();
                    }
                    true
                }),
            },
            None,
        );
        let (request, reply) = EnsureRequest::new(origin, "StartFill");
        let mut active = None;
        let mut mode = RecordingMode::Idle;
        ensure(&state, request, &directory, &mut active, &mut mode).await;
        assert_eq!(reply.await.unwrap(), Err(Error::Stale));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        assert!(active.is_none());
        assert_eq!(state.fill_recording.ready_instance(), 0);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(directory.join("archive.db")).unwrap(),
            b"keep"
        );
        finish(&state, active, directory).await;
    }
}

/// A disposable host fixture runs the SAME production actor on its own runtime;
/// its Drop joins the owner before the fixture deletes its owned directory.
#[cfg(test)]
pub(crate) struct TestOwner {
    state: Arc<AppState>,
    thread: Option<std::thread::JoinHandle<()>>,
}
#[cfg(test)]
impl TestOwner {
    pub(crate) fn start(state: &mut Arc<AppState>, directory: &Path) -> Self {
        if !directory.exists() {
            std::fs::create_dir_all(directory).unwrap();
        }
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let exclusive = Arc::get_mut(state).expect("install fixture actor before sharing state");
        exclusive.db_queue_tx = tx;
        exclusive.placeholder_db_path = directory
            .join("groundstation.db")
            .to_string_lossy()
            .into_owned();
        let shutdown = state.shutdown_subscribe();
        let owned = state.clone();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run(owned, rx, 32, 1, shutdown));
        });
        Self {
            state: state.clone(),
            thread: Some(thread),
        }
    }
}
#[cfg(test)]
impl Drop for TestOwner {
    fn drop(&mut self) {
        self.state.request_shutdown();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use super::*;
    use std::sync::{Mutex, OnceLock, mpsc};
    struct Hook {
        arrived: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    static HOOKS: OnceLock<Mutex<std::collections::HashMap<usize, Hook>>> = OnceLock::new();
    pub(crate) fn hold_after_open(state: &Arc<AppState>) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (arrived, receive) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        assert!(
            HOOKS
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(
                    Arc::as_ptr(state) as usize,
                    Hook {
                        arrived,
                        release: wait
                    }
                )
                .is_none()
        );
        (receive, release)
    }
    struct AuthHook {
        arrived: mpsc::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    }
    static AUTH_HOOKS: OnceLock<Mutex<std::collections::HashMap<usize, AuthHook>>> =
        OnceLock::new();
    pub(crate) fn hold_http_auth(
        state: &Arc<AppState>,
    ) -> (mpsc::Receiver<()>, tokio::sync::oneshot::Sender<()>) {
        let (arrived, receive) = mpsc::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        assert!(
            AUTH_HOOKS
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(
                    Arc::as_ptr(state) as usize,
                    AuthHook {
                        arrived,
                        release: wait
                    }
                )
                .is_none()
        );
        (receive, release)
    }
    pub(crate) async fn before_http_auth(state: &Arc<AppState>) -> bool {
        let hook = AUTH_HOOKS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(state) as usize));
        let Some(hook) = hook else {
            return true;
        };
        let _ = hook.arrived.send(());
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(6), hook.release).await,
            Ok(Ok(()))
        )
    }
    pub(super) fn after_open(state: &Arc<AppState>) -> bool {
        let hook = HOOKS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(state) as usize));
        let Some(hook) = hook else {
            return true;
        };
        let _ = hook.arrived.send(());
        hook.release
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok()
    }
}

#[cfg(test)]
#[path = "recording_pending_tests.rs"]
mod pending_tests;
