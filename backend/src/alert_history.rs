//! Operator clearing of warning/error history, with a durable audit trail.
//! Active conditions, board Abort latches and abort history are never touched:
//! only resolved condition occurrences leave, and older events are hidden.
//!
//! The shared warning/error acknowledgements are saved beside the audit and
//! restored with it, so a station restart does not reset them. An
//! acknowledgement covers events stamped at or before its time; one saved
//! ahead of the clock the station restarts with is cut back to the restart
//! time, so it never covers an alert raised after the restart.
use crate::state::AppState;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

const AUDIT_FILE: &str = "alert-history-clears.jsonl";
const ACK_FILE: &str = "alert-acknowledgements.json";
/// Entries returned to viewers; the file itself keeps every clear.
const AUDIT_VIEW_LIMIT: usize = 50;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClearEntry {
    pub cleared_at_ms: i64,
    /// Events at or before this station time are hidden from history.
    pub cleared_through_ms: i64,
    pub cleared_by: String,
    pub resolved_conditions_cleared: usize,
}

static CLEAR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn audit_path(state: &AppState) -> PathBuf {
    data_dir(state).join(AUDIT_FILE)
}

fn data_dir(state: &AppState) -> &Path {
    Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct SavedAcks {
    warning_ack_timestamp_ms: i64,
    error_ack_timestamp_ms: i64,
}

/// Serialises acknowledgement file updates across threads.
static ACK_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn read_acks(path: &Path) -> Result<Option<SavedAcks>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_acks(path: &Path, acks: SavedAcks) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(&acks).map_err(|error| error.to_string())?;
    let mut file = std::fs::File::create(&temporary).map_err(|error| error.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| error.to_string())?;
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Saves the current acknowledgements. They only move forward, so the file
/// keeps the newer of what it holds and the current state, whatever order
/// concurrent saves run in.
fn save_acks_blocking_at(path: &Path, current: SavedAcks) -> Result<(), String> {
    let _serial = ACK_WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // An unreadable file is replaced by the current state.
    let saved = read_acks(path).unwrap_or(None);
    let base = saved.unwrap_or_default();
    let merged = SavedAcks {
        warning_ack_timestamp_ms: base.warning_ack_timestamp_ms.max(current.warning_ack_timestamp_ms),
        error_ack_timestamp_ms: base.error_ack_timestamp_ms.max(current.error_ack_timestamp_ms),
    };
    if saved == Some(merged) {
        return Ok(());
    }
    write_acks(path, merged)
}

fn current_acks(state: &AppState) -> SavedAcks {
    let ack = state.alert_ack_state_snapshot();
    SavedAcks {
        warning_ack_timestamp_ms: ack.warning_ack_timestamp_ms,
        error_ack_timestamp_ms: ack.error_ack_timestamp_ms,
    }
}

/// Saves the shared acknowledgements after they change, from a thread that
/// may block (the GPIO acknowledge button).
pub fn save_acks_blocking(state: &AppState) {
    if let Err(error) = save_acks_blocking_at(&data_dir(state).join(ACK_FILE), current_acks(state)) {
        log::warn!("Alert acknowledgements were not saved: {error}");
    }
}

/// Saves the shared acknowledgements after they change.
pub async fn save_acks(state: &AppState) {
    let path = data_dir(state).join(ACK_FILE);
    let current = current_acks(state);
    match tokio::task::spawn_blocking(move || save_acks_blocking_at(&path, current)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::warn!("Alert acknowledgements were not saved: {error}"),
        Err(error) => log::warn!("Alert acknowledgements were not saved: {error}"),
    }
}

/// Reapply the saved acknowledgements after a restart, cut back to the
/// restart time: an alert raised from now on is never covered by them.
fn restore_acks_blocking(path: &Path, now_ms: i64) -> Result<Option<SavedAcks>, String> {
    let _serial = ACK_WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(saved) = read_acks(path)? else {
        return Ok(None);
    };
    let restored = SavedAcks {
        warning_ack_timestamp_ms: saved.warning_ack_timestamp_ms.min(now_ms),
        error_ack_timestamp_ms: saved.error_ack_timestamp_ms.min(now_ms),
    };
    if restored != saved {
        log::warn!(
            "Saved alert acknowledgements were ahead of the station clock; cut back to {now_ms}"
        );
        write_acks(path, restored)?;
    }
    Ok(Some(restored))
}

fn read_entries(path: &Path) -> Result<Vec<ClearEntry>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(entry) => Some(entry),
            Err(error) => {
                log::warn!("Skipping unreadable alert history audit line: {error}");
                None
            }
        })
        .collect())
}

fn append(path: &Path, entry: &ClearEntry) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut line = serde_json::to_string(entry).map_err(|error| error.to_string())?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

/// Reapply the latest clear after a restart so cleared events stay hidden.
pub async fn restore(state: &AppState) {
    let path = audit_path(state);
    match tokio::task::spawn_blocking(move || read_entries(&path)).await {
        Ok(Ok(entries)) => {
            if let Some(cutoff) = entries.iter().map(|entry| entry.cleared_through_ms).max() {
                state.clear_alert_history_through(cutoff);
            }
        }
        Ok(Err(error)) => log::warn!("Cannot read alert history audit: {error}"),
        Err(error) => log::warn!("Cannot read alert history audit: {error}"),
    }
    let path = data_dir(state).join(ACK_FILE);
    let now_ms = crate::telemetry_task::get_current_timestamp_ms() as i64;
    match tokio::task::spawn_blocking(move || restore_acks_blocking(&path, now_ms)).await {
        Ok(Ok(Some(acks))) => {
            state.acknowledge_alerts_through(acks.warning_ack_timestamp_ms, acks.error_ack_timestamp_ms);
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => log::warn!("Cannot read saved alert acknowledgements: {error}"),
        Err(error) => log::warn!("Cannot read saved alert acknowledgements: {error}"),
    }
}

/// Newest first.
pub async fn recent_entries(state: &AppState) -> Result<Vec<ClearEntry>, String> {
    let path = audit_path(state);
    let mut entries = tokio::task::spawn_blocking(move || read_entries(&path))
        .await
        .map_err(|error| error.to_string())??;
    entries.reverse();
    entries.truncate(AUDIT_VIEW_LIMIT);
    Ok(entries)
}

/// The audit entry is written first; without it nothing is cleared.
pub async fn clear(state: &AppState, cleared_by: String) -> Result<ClearEntry, (StatusCode, String)> {
    let _serial = CLEAR_LOCK.lock().await;
    let Some(resolved) = crate::alert_condition_runtime::resolved_history_len(state) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Condition history is still loading; try again shortly".into(),
        ));
    };
    let now_ms = crate::telemetry_task::get_current_timestamp_ms() as i64;
    let newest_event = state
        .recent_alerts_snapshot()
        .iter()
        .map(|alert| alert.timestamp_ms)
        .max()
        .unwrap_or(0);
    let entry = ClearEntry {
        cleared_at_ms: now_ms,
        cleared_through_ms: now_ms
            .max(newest_event)
            .max(state.alert_ack_state_snapshot().history_cleared_through_ms),
        cleared_by,
        resolved_conditions_cleared: resolved,
    };
    let path = audit_path(state);
    let record = entry.clone();
    let written = tokio::task::spawn_blocking(move || append(&path, &record))
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
    if let Err(error) = written {
        log::warn!("Cannot record alert history clear: {error}");
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Cannot record the audit entry, so history was not cleared".into(),
        ));
    }
    if let Err(error) = crate::alert_condition_runtime::clear_resolved_history(state) {
        log::warn!("Alert history clear kept condition history: {error}");
    }
    state.clear_alert_history_through(entry.cleared_through_ms);
    log::info!(
        "Alert history cleared by {} through {} ({} resolved conditions)",
        entry.cleared_by,
        entry.cleared_through_ms,
        entry.resolved_conditions_cleared
    );
    Ok(entry)
}
