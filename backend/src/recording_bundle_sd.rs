//! Replay-bundle entries of an SD-resolved recording (`recording_sd`): what a
//! client needs to produce the station's exports of it offline
//! (`docs/backend/replay-bundle.md`, "SD-resolved recordings").
//!
//! An SD-resolved recording has two timelines. Its CSV (`data/telemetry.csv`,
//! `GET /api/recordings/{id}/csv`) and its reports read the resolved
//! timeline (`sd_replay_telemetry`: canonical UTC, every row with its
//! `provenance_json`); its time-range CSV reads the rows as received
//! (`main.telemetry`). The bundle already holds the first and the SD
//! provenance (`replay.json` `sd_provenance`: the resolution summary, each
//! SD import's manifest and calibration metadata, per-row provenance and the
//! SD samples left unresolved). This adds:
//!
//! - `data/receipt_telemetry.csv`: the receipt rows, as the recording's CSV
//!   would be without the resolved view (no route). Omitted, and listed as
//!   missing with status 507, above [`MAX_RECEIPT_BYTES`].
//! - `derived/sd_sources/after_{n}.json`: further pages of the SD source
//!   list (`GET /api/recordings/{id}/sd/sources?after_id={n}`; the first page
//!   is `derived/sd_sources.json`).
//! - With `sd_files=true`: the original SD files, byte for byte
//!   (`sd/sources/{sha256}.csv`, `GET /api/recordings/{id}/sd/sources/{sha256}`),
//!   in listing order within `sd_files_max_mb` (default 16, at most 64).
//!
//! The manifest's `sd` object reports what was included and its size.
use axum::{Router, http::HeaderMap};
use serde_json::{Value, json};

/// Largest receipt-row CSV a bundle holds (the bundle's per-entry ceiling).
pub(crate) const MAX_RECEIPT_BYTES: usize = 96 << 20;
pub(crate) const RECEIPT_PATH: &str = "data/receipt_telemetry.csv";
const DEFAULT_FILES_BUDGET_MB: u32 = 16;
const MAX_FILES_BUDGET_MB: u32 = 64;
/// At most this many source-list pages (128 sources each).
const MAX_SOURCE_PAGES: usize = 64;

pub(crate) struct Added {
    pub path: String,
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub route: Option<String>,
}

pub(crate) struct SdEntries {
    pub entries: Vec<Added>,
    pub missing: Vec<Value>,
    /// The manifest's `sd` object.
    pub summary: Value,
}

/// Whether the recording at `path` has a resolved view, and its row count.
async fn resolved_rows(path: &std::path::Path) -> Option<i64> {
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(path).read_only(true).create_if_missing(false);
    let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(options).await.ok()?;
    let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM main.sqlite_master WHERE type='table' AND name='sd_replay_telemetry'")
        .fetch_one(&db)
        .await
        .unwrap_or(0)
        > 0;
    let rows = if exists { sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM main.sd_replay_telemetry").fetch_one(&db).await.ok() } else { None };
    db.close().await;
    rows
}

/// The entries for recording `id` stored at `path`; `None` when it is not SD-resolved.
pub(crate) async fn entries(
    router: &Router,
    headers: &HeaderMap,
    id: &str,
    path: &std::path::Path,
    files: bool,
    files_max_mb: Option<u32>,
) -> Option<SdEntries> {
    let resolved = resolved_rows(path).await?;
    let mut out = SdEntries { entries: Vec::new(), missing: Vec::new(), summary: Value::Null };
    let receipt = match crate::recording_export::receipt_csv(path, MAX_RECEIPT_BYTES).await {
        Ok(Some((csv, rows))) => {
            let summary = json!({"path": RECEIPT_PATH, "included": true, "rows": rows, "bytes": csv.len()});
            out.entries.push(Added { path: RECEIPT_PATH.into(), bytes: csv, media_type: "text/csv", route: None });
            summary
        }
        Ok(None) => {
            out.missing.push(json!({"path": RECEIPT_PATH, "status": 507}));
            json!({"path": RECEIPT_PATH, "included": false, "reason": format!("larger than {} MiB", MAX_RECEIPT_BYTES >> 20)})
        }
        Err(error) => {
            log::warn!("bundle receipt rows of {id}: {error}");
            out.missing.push(json!({"path": RECEIPT_PATH, "status": 500}));
            json!({"path": RECEIPT_PATH, "included": false, "reason": "unreadable"})
        }
    };

    // Every page of the source list; the first is the bundle's derived/sd_sources.json.
    let base = format!("/api/recordings/{id}/sd/sources");
    let mut sources = Vec::new();
    let mut page = crate::recording_bundle::fetch(router, headers, &base).await.ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let mut pages = 1;
    while let Some(listing) = page.take() {
        sources.extend(listing["sources"].as_array().cloned().unwrap_or_default());
        let Some(after) = listing["next_id"].as_i64().filter(|_| listing["has_more"] == true) else { break };
        if pages >= MAX_SOURCE_PAGES {
            break;
        }
        let route = format!("{base}?after_id={after}");
        match crate::recording_bundle::fetch(router, headers, &route).await {
            Ok(bytes) => {
                let value = serde_json::from_slice::<Value>(&bytes).ok();
                let bytes = value.as_ref().and_then(|v| serde_json::to_vec(v).ok()).unwrap_or(bytes);
                out.entries.push(Added { path: format!("derived/sd_sources/after_{after}.json"), bytes, media_type: "application/json", route: Some(route) });
                page = value;
                pages += 1;
            }
            Err(status) => out.missing.push(json!({"path": format!("derived/sd_sources/after_{after}.json"), "route": route, "status": status})),
        }
    }

    let budget = (files_max_mb.unwrap_or(DEFAULT_FILES_BUDGET_MB).min(MAX_FILES_BUDGET_MB) as usize) << 20;
    let total: u64 = sources.iter().filter_map(|s| s["bytes"].as_u64()).sum();
    let (mut used, mut count, mut complete) = (0usize, 0u64, true);
    if files {
        for source in &sources {
            let Some(sha) = source["file_sha256"].as_str().filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())) else { continue };
            let size = source["bytes"].as_u64().unwrap_or(0) as usize;
            if used + size > budget {
                complete = false;
                continue;
            }
            let route = format!("{base}/{sha}");
            match crate::recording_bundle::fetch(router, headers, &route).await {
                Ok(bytes) if used + bytes.len() <= budget => {
                    used += bytes.len();
                    count += 1;
                    out.entries.push(Added { path: format!("sd/sources/{sha}.csv"), bytes, media_type: "text/csv", route: Some(route) });
                }
                Ok(_) => complete = false,
                Err(status) => {
                    complete = false;
                    out.missing.push(json!({"path": format!("sd/sources/{sha}.csv"), "route": route, "status": status}));
                }
            }
        }
    }
    out.summary = json!({
        "resolved": true,
        "resolved_rows": resolved,
        "receipt_csv": receipt,
        "sources": {"listed": sources.len(), "bytes": total},
        "files": {"requested": files, "included": count, "bytes": used, "budget_bytes": if files { budget } else { 0 }, "complete": files && complete && count as usize == sources.len()},
    });
    Some(out)
}
