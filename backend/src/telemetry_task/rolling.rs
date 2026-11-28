//! The DB actor's always-on rolling segment owner.
use super::recording_owner::ActiveRecording;
use super::*;
use crate::recording_store;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(2);
const DISK_WARNING_INTERVAL: Duration = Duration::from_secs(600);

pub(super) struct Rolling {
    pub(super) root: PathBuf,
    dir: PathBuf,
    last_maintenance: tokio::time::Instant,
    last_disk_warning: Option<tokio::time::Instant>,
    /// Latest calibration per (sender, sensor): written into every new
    /// segment so each segment (and each capture) is self-contained.
    calibration: HashMap<(String, String), DbWrite>,
}

impl Rolling {
    /// Opens the ring: recovers segment journals, resumes pending captures,
    /// and opens a fresh segment for this run.
    pub(super) async fn start(state: &Arc<AppState>, root: PathBuf) -> (Self, Option<ActiveRecording>) {
        let dir = recording_store::rolling_dir(&root);
        if let Err(error) = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::create_dir_all(recording_store::events_dir(&root)))
        {
            emit_warning(state, format!("Could not create the rolling recording folders: {error}"));
        }
        if let Err(error) = crate::telemetry_db::recover_sqlite_sidecars_in_dir(&dir).await {
            log::warn!("rolling segment recovery: {error}");
        }
        state.recording_store.set_settings(recording_store::load_settings(&root));
        let pending = recording_store::load_pending(&root);
        if !pending.is_empty() {
            let last = last_data_ms(&dir).await.unwrap_or_else(|| get_current_timestamp_ms() as i64);
            log::info!("resuming {} pending event capture(s) after restart", pending.len());
            state.recording_store.restore(pending, last);
            // State-keyed captures continue only while that state still holds.
            let code = state.local_flight_state_snapshot() as i64;
            recording_store::observe_flight_state(state, code);
        }
        let mut rolling = Self {
            root,
            dir,
            last_maintenance: tokio::time::Instant::now(),
            last_disk_warning: None,
            calibration: HashMap::new(),
        };
        let active = rolling.open_segment(state).await;
        (rolling, active)
    }

    pub(super) async fn open_segment(&mut self, state: &Arc<AppState>) -> Option<ActiveRecording> {
        let path = recording_store::new_segment_path(&self.dir, get_current_timestamp_ms() as i64);
        match open_telemetry_db(&path).await {
            Ok((pool, path)) => {
                let mut rows = crate::igt::recording_start_rows(state);
                rows.extend(self.calibration.values().cloned());
                if let Err(err) = insert_db_batch_with_retry(&pool, &rows).await {
                    log::error!("failed to write segment start rows: {err}");
                }
                state.replace_telemetry_db(pool.clone(), path.clone());
                state.set_recording_status(RecordingStatusMsg::new(RecordingModeWire::Recording, Some(path.clone())));
                log::info!("rolling recording segment opened db={path}");
                Some(ActiveRecording { pool, path })
            }
            Err(err) => {
                emit_warning(state, format!("Failed to open a rolling recording segment: {err}"));
                None
            }
        }
    }

    /// Calibration carry-forward and event triggers for every accepted write.
    pub(super) fn observe(&mut self, state: &AppState, write: &DbWrite) {
        match write {
            DbWrite::Calibration { sender_id, sensor_id, .. } => {
                self.calibration.insert((sender_id.clone(), sensor_id.clone()), write.clone());
            }
            DbWrite::FlightState { state_code, .. } => {
                recording_store::observe_flight_state(state, *state_code);
            }
            _ => {}
        }
    }

    /// Closes the current segment and opens the next. The fill-recording
    /// instance stays published: the recording never stopped.
    pub(super) async fn rotate(&mut self, state: &Arc<AppState>, active: &mut Option<ActiveRecording>) {
        let Some(next) = self.open_segment(state).await else {
            return;
        };
        if let Some(old) = active.replace(next) {
            log::info!("rolling recording segment closing db={}", old.path);
            // The test-fire CSV is written per event capture, not per segment.
            close_and_finalize_sqlite(old.pool, &old.path).await;
        }
    }

    pub(super) fn maintenance_due(&self) -> bool {
        self.last_maintenance.elapsed() >= MAINTENANCE_INTERVAL
    }

    /// Size-based rotation, the ring budget and the low-disk policy.
    pub(super) async fn maintain(
        &mut self,
        state: &Arc<AppState>,
        active: &mut Option<ActiveRecording>,
        rotation_allowed: bool,
    ) {
        self.last_maintenance = tokio::time::Instant::now();
        let settings = state.recording_store.settings();
        if rotation_allowed
            && let Some(current) = active.as_ref()
            && recording_store::sqlite_bytes(Path::new(&current.path)) >= settings.segment_bytes()
        {
            self.rotate(state, active).await;
        }
        if active.is_none() && rotation_allowed {
            // A failed open is retried here; writes wait in `pending`.
            *active = self.open_segment(state).await;
        }
        let free = crate::recording_sd::free_disk(&self.root).ok();
        let active_path = active.as_ref().map(|a| PathBuf::from(&a.path));
        recording_store::maintain_ring(state, &self.root, active_path.as_deref(), free);
        let status = state.recording_store.ring_status();
        if let Some(warning) = status.warning {
            let due = self
                .last_disk_warning
                .is_none_or(|at| at.elapsed() >= DISK_WARNING_INTERVAL);
            if due {
                self.last_disk_warning = Some(tokio::time::Instant::now());
                emit_warning(state, warning);
            }
        } else {
            self.last_disk_warning = None;
        }
    }
}

/// Newest row time in the ring (the moment a crash or restart cut it off).
async fn last_data_ms(dir: &Path) -> Option<i64> {
    let newest = recording_store::scan_segments(dir).pop()?;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&newest.path)
        .read_only(true)
        .create_if_missing(false);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .ok()?;
    let max: Option<i64> = sqlx::query_scalar("SELECT MAX(timestamp_ms) FROM telemetry")
        .fetch_one(&pool)
        .await
        .ok()
        .flatten();
    pool.close().await;
    max.or(Some(newest.start_ms))
}
