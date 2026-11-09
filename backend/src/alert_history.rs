//! Operator clearing of warning/error history, with a durable audit trail.
//! Active conditions, board Abort latches and abort history are never touched:
//! only resolved condition occurrences leave, and older events are hidden.
use crate::state::AppState;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

const AUDIT_FILE: &str = "alert-history-clears.jsonl";
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
    Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(AUDIT_FILE)
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
