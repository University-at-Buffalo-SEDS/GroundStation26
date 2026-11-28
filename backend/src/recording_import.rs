//! Importing a replay bundle (`POST /api/recordings/import`).
//!
//! An administrator uploads a `.gsreplay` file (`recording_bundle`, UI
//! `docs/replay-bundle.md`). The station checks the archive like the UI's
//! reader does (stored or deflated entries, no ZIP64 or encryption, safe
//! names, at most 192 MiB unpacked, every listed entry present with its size
//! and SHA-256) and recreates the recording from `data/telemetry.csv` as an
//! event capture of kind `imported`
//! (`recordings/events/imported/<stamp>_<slug>/recording.db` plus
//! `manifest.json`). It then appears in `GET /api/recordings` and every
//! `/api/recordings/{id}/...` route like any capture. The station never
//! writes to it again; deleting it is the usual administrator delete.
//!
//! The rows keep their ids, reception and source times, sender, type,
//! values (over-range markers become the `over_range` column again) and
//! payload, so the imported recording's CSV export equals the bundle's CSV
//! byte for byte. The recorded alerts come from `derived/alerts.json`.
//! Configuration entries (layout, calibration, view configuration) are not
//! installed: the importing station's own configuration labels the replay.
//!
//! An SD-resolved recording (`recording_sd`) imports from its two CSVs: the
//! receipt rows (`data/receipt_telemetry.csv`, `recording_bundle_sd`) become
//! `telemetry`, and the resolved rows with their provenance
//! (`data/telemetry.csv`) become `sd_replay_telemetry`, so the imported
//! recording's CSV, time-range rows and reports equal the source's. The SD
//! imports' manifests and calibration metadata and the resolution summary
//! come from `replay.json` (`sd_provenance`), the source names from the
//! bundled SD source list, and the original SD files from `sd/sources/` when
//! the bundle holds them. Not carried: the SD samples themselves (raw
//! records stay in each row's provenance; the unresolved ones only in the
//! bundle's `replay.json`) and the coverage segments, so the imported replay
//! does not mark where SD coverage ends. A bundle with a resolved CSV but no
//! receipt rows (made before they were bundled) is refused with 422.
use crate::{auth::Permission, recording_store as store, state::AppState};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::Path, sync::Arc};

/// Largest accepted upload.
pub(crate) const MAX_BODY: usize = 256 << 20;
/// Largest unpacked archive (the UI reader's limit).
const MAX_UNPACKED: usize = 192 << 20;
const CSV_PATH: &str = "data/telemetry.csv";
const ALERTS_PATH: &str = "derived/alerts.json";
const PLAIN_HEADER: &str = "id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json";
const PROVENANCE_HEADER: &str = "id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json,data_provenance";
const RESOLVED_HEADER: &str = "id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json,canonical_utc_ms,provenance_json";
/// Imports run one at a time so the duplicate check and the write agree.
static IMPORTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/recordings/import", post(import).layer(DefaultBodyLimit::max(MAX_BODY)))
}

#[derive(Debug)]
pub(crate) struct Refusal {
    pub status: StatusCode,
    pub message: String,
    /// The recording already imported from the same source (409).
    pub existing: Option<String>,
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Refusal {
    Refusal { status, message: message.into(), existing: None }
}
fn bad(message: impl Into<String>) -> Refusal {
    refuse(StatusCode::BAD_REQUEST, message)
}

async fn import(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.is_admin() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "Administrator access required to import a replay bundle"}))).into_response();
    }
    let root = crate::recording_export::directory(&state);
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    match import_bundle(&root, &body, principal.username.as_deref(), now_ms).await {
        Ok(imported) => (StatusCode::CREATED, Json(imported)).into_response(),
        Err(refusal) => {
            let mut body = json!({"error": refusal.message});
            if let Some(existing) = refusal.existing {
                body["id"] = existing.into();
            }
            (refusal.status, Json(body)).into_response()
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// A relative, `/`-separated name with no empty, `.` or `..` segment.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 512
        && name.chars().all(|c| !c.is_control() && c != '\\' && c != ':')
        && name.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The entries of a bundle archive, refusing anything the UI reader refuses.
pub(crate) fn unzip(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let malformed = || "Not a readable ZIP archive.".to_owned();
    let u16_at = |i: usize| bytes.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |i: usize| bytes.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if bytes.len() < 22 {
        return Err(malformed());
    }
    let lowest = bytes.len().saturating_sub(22 + 0xFFFF);
    let end = (lowest..=bytes.len() - 22).rev().find(|&i| u32_at(i) == Some(0x0605_4b50)).ok_or_else(malformed)?;
    let field = |i: usize| u16_at(end + i).ok_or_else(malformed);
    let (disk, cd_disk, on_disk, count) = (field(4)?, field(6)?, field(8)?, field(10)?);
    let cd_size = u32_at(end + 12).ok_or_else(malformed)?;
    let cd_offset = u32_at(end + 16).ok_or_else(malformed)?;
    if count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF || (end >= 20 && u32_at(end - 20) == Some(0x0706_4b50)) {
        return Err("ZIP64 archives are not read.".into());
    }
    if disk != 0 || cd_disk != 0 || on_disk != count || end + 22 + field(20)? != bytes.len() || cd_offset.checked_add(cd_size).is_none_or(|e| e > end) {
        return Err(malformed());
    }
    let mut files = BTreeMap::new();
    let mut unpacked = 0usize;
    let mut at = cd_offset;
    for _ in 0..count {
        if u32_at(at) != Some(0x0201_4b50) {
            return Err(malformed());
        }
        let get = |i: usize| u16_at(at + i).ok_or_else(malformed);
        let (flags, method, name_len, extra_len, comment_len) = (get(8)?, get(10)?, get(28)?, get(30)?, get(32)?);
        let crc = u32_at(at + 16).ok_or_else(malformed)?;
        let (packed, size, local) = (u32_at(at + 20).ok_or_else(malformed)?, u32_at(at + 24).ok_or_else(malformed)?, u32_at(at + 42).ok_or_else(malformed)?);
        let name_bytes = bytes.get(at + 46..at + 46 + name_len).ok_or_else(malformed)?;
        let name = std::str::from_utf8(name_bytes).map_err(|_| malformed())?.to_owned();
        let mut extra = bytes.get(at + 46 + name_len..at + 46 + name_len + extra_len).ok_or_else(malformed)?;
        while extra.len() >= 4 {
            let (id, len) = (u16::from_le_bytes([extra[0], extra[1]]), u16::from_le_bytes([extra[2], extra[3]]) as usize);
            if id == 0x0001 {
                return Err("ZIP64 archives are not read.".into());
            }
            extra = extra.get(4 + len..).unwrap_or_default();
        }
        at += 46 + name_len + extra_len + comment_len;
        if at > cd_offset + cd_size {
            return Err(malformed());
        }
        if packed == 0xFFFF_FFFF || size == 0xFFFF_FFFF || local == 0xFFFF_FFFF {
            return Err("ZIP64 archives are not read.".into());
        }
        if flags & 0x41 != 0 {
            return Err(format!("Entry {name} is encrypted."));
        }
        if !safe_name(&name) {
            return Err(format!("Entry name {name:?} is not a safe relative path."));
        }
        if !matches!(method, 0 | 8) {
            return Err(format!("Entry {name} uses ZIP method {method}; only stored and deflated entries are read."));
        }
        unpacked = unpacked.saturating_add(size);
        if unpacked > MAX_UNPACKED {
            return Err("The archive unpacks to more than 192 MiB.".into());
        }
        if u32_at(local) != Some(0x0403_4b50) || bytes.get(local + 30..local + 30 + name_len) != Some(name_bytes) {
            return Err(malformed());
        }
        let start = local + 30 + u16_at(local + 26).ok_or_else(malformed)? + u16_at(local + 28).ok_or_else(malformed)?;
        if start.checked_add(packed).is_none_or(|e| e > cd_offset) {
            return Err(malformed());
        }
        let body = &bytes[start..start + packed];
        let data = if method == 0 {
            if packed != size {
                return Err(malformed());
            }
            body.to_vec()
        } else {
            let mut data = Vec::with_capacity(size);
            flate2::read::DeflateDecoder::new(body).take(size as u64 + 1).read_to_end(&mut data).map_err(|_| malformed())?;
            data
        };
        if data.len() != size || crc32fast::hash(&data) as usize != crc {
            return Err(format!("Entry {name} is corrupt."));
        }
        if files.insert(name.clone(), data).is_some() {
            return Err(format!("Entry {name} appears twice."));
        }
    }
    Ok(files)
}

/// The bundle's manifest, after checking the format, the version and every
/// listed entry's size and hash.
pub(crate) fn verify(files: &BTreeMap<String, Vec<u8>>) -> Result<Value, Refusal> {
    let manifest: Value = files
        .get("manifest.json")
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .filter(Value::is_object)
        .ok_or_else(|| bad("Not a replay bundle: manifest.json is missing or unreadable."))?;
    if manifest["format"] != crate::recording_bundle::FORMAT {
        return Err(bad("Not a replay bundle: manifest format is not \"gsreplay\"."));
    }
    let version = manifest["version"].as_u64().ok_or_else(|| bad("The bundle's version is not an integer."))?;
    if version > crate::recording_bundle::VERSION {
        return Err(refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("The bundle is version {version}; this station reads version {} and older. Update the station.", crate::recording_bundle::VERSION)));
    }
    for entry in manifest["entries"].as_array().ok_or_else(|| bad("The bundle lists no entries."))? {
        let path = entry["path"].as_str().ok_or_else(|| bad("A listed entry has no path."))?;
        let bytes = files.get(path).ok_or_else(|| bad(format!("Listed entry {path} is missing.")))?;
        if entry["bytes"].as_u64() != Some(bytes.len() as u64) || entry["sha256"].as_str() != Some(sha256_hex(bytes).as_str()) {
            return Err(bad(format!("Entry {path} does not match its size or SHA-256.")));
        }
    }
    Ok(manifest)
}

/// One CSV record per call (RFC 4180 quoting, CRLF or LF line ends).
struct Records<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Records<'_> {
    fn next_record(&mut self) -> Option<Result<Vec<String>, String>> {
        if self.at >= self.bytes.len() {
            return None;
        }
        let (mut row, mut field, mut quoted, mut was_quoted) = (Vec::new(), Vec::new(), false, false);
        let finish = |field: &mut Vec<u8>| String::from_utf8(std::mem::take(field)).map_err(|_| "The CSV is not UTF-8.".to_owned());
        while self.at < self.bytes.len() {
            let c = self.bytes[self.at];
            self.at += 1;
            if quoted {
                if c == b'"' {
                    if self.bytes.get(self.at) == Some(&b'"') {
                        field.push(b'"');
                        self.at += 1;
                    } else {
                        quoted = false;
                    }
                } else {
                    field.push(c);
                }
                continue;
            }
            match c {
                b'"' if field.is_empty() && !was_quoted => (quoted, was_quoted) = (true, true),
                b',' => {
                    row.push(match finish(&mut field) { Ok(f) => f, Err(e) => return Some(Err(e)) });
                    was_quoted = false;
                }
                b'\r' if self.bytes.get(self.at) == Some(&b'\n') => {}
                b'\n' => {
                    row.push(match finish(&mut field) { Ok(f) => f, Err(e) => return Some(Err(e)) });
                    return Some(Ok(row));
                }
                _ => field.push(c),
            }
        }
        if quoted {
            return Some(Err("The CSV ends inside a quoted cell.".into()));
        }
        row.push(match finish(&mut field) { Ok(f) => f, Err(e) => return Some(Err(e)) });
        Some(Ok(row))
    }
}

/// A text cell as stored: the export prefixes `'` to text that a spreadsheet
/// would read as a formula (`recording_export::text_cell`).
fn stored_text(cell: String) -> String {
    match cell.strip_prefix('\'') {
        Some(rest) if rest.trim_start().starts_with(['=', '+', '-', '@']) => rest.to_owned(),
        _ => cell,
    }
}

/// values_json as stored: exported `"OVR"`/`"-OVR"` markers become null and
/// the packed `over_range` masks (`over_range::export_values_json` reversed).
fn stored_values(cell: &str) -> (Option<String>, Option<i64>) {
    if cell.is_empty() {
        return (None, None);
    }
    if !cell.contains("OVR\"") {
        return (Some(cell.to_owned()), None);
    }
    let Ok(mut values) = serde_json::from_str::<Vec<Value>>(cell) else {
        return (Some(cell.to_owned()), None);
    };
    let (mut mask, mut low) = (0u32, 0u32);
    for (i, value) in values.iter_mut().enumerate().take(32) {
        let marker = match value.as_str() {
            Some("OVR") => false,
            Some("-OVR") => true,
            _ => continue,
        };
        *value = Value::Null;
        mask |= 1 << i;
        if marker {
            low |= 1 << i;
        }
    }
    (serde_json::to_string(&values).ok(), crate::over_range::pack(mask, low))
}

fn parse_ms(cell: &str, what: &str, line: usize) -> Result<i64, Refusal> {
    cell.parse().map_err(|_| bad(format!("CSV line {line}: {what} {cell:?} is not an integer.")))
}

/// Folders of earlier imports from the same source recording and CSV.
fn already_imported(root: &Path, recording: &str, csv_sha: &str) -> Option<String> {
    let dir = store::events_dir(root).join(store::EventKind::Imported.id());
    std::fs::read_dir(dir).into_iter().flatten().flatten().find_map(|entry| {
        let manifest = store::read_manifest(&entry.path())?;
        let import = &manifest.details["import"];
        (import["source"]["recording"] == recording && import["csv_sha256"] == csv_sha).then_some(manifest.id)
    })
}

/// Checks a bundle and stores its recording; returns the import summary.
pub(crate) async fn import_bundle(root: &Path, bytes: &[u8], operator: Option<&str>, now_ms: i64) -> Result<Value, Refusal> {
    let files = unzip(bytes).map_err(bad)?;
    let manifest = verify(&files)?;
    let csv = files.get(CSV_PATH).ok_or_else(|| bad("The bundle has no data/telemetry.csv."))?;
    let csv_sha = sha256_hex(csv);
    let source = manifest["source"].clone();
    let recording = source["recording"].as_str().filter(|r| !r.is_empty()).unwrap_or("recording").to_owned();
    let bundle_sha = sha256_hex(bytes);

    let mut records = Records { bytes: csv, at: 0 };
    let header = records.next_record().transpose().map_err(bad)?.unwrap_or_default().join(",");
    // An SD-resolved recording: data/telemetry.csv is the resolved timeline,
    // and the receipt rows are their own entry.
    let mut resolved = None;
    let mut columns = header.split(',').count();
    if header == RESOLVED_HEADER || header == format!("{RESOLVED_HEADER},data_provenance") {
        let receipt = files.get(crate::recording_bundle_sd::RECEIPT_PATH).ok_or_else(|| {
            refuse(StatusCode::UNPROCESSABLE_ENTITY, "This bundle's recording was merged with board SD data, but the bundle does not hold the rows as received (it was made by an older station). Make the bundle again on an updated station to import it.")
        })?;
        resolved = Some(Resolved { rows: std::mem::replace(&mut records, Records { bytes: receipt, at: 0 }), columns: header.split(',').count(), sd: SdRecords::of(&files) });
        let receipt_header = records.next_record().transpose().map_err(bad)?.unwrap_or_default().join(",");
        if receipt_header != PLAIN_HEADER && receipt_header != PROVENANCE_HEADER {
            return Err(bad("data/receipt_telemetry.csv does not have the recording CSV header."));
        }
        columns = receipt_header.split(',').count();
    } else if header != PLAIN_HEADER && header != PROVENANCE_HEADER {
        return Err(if header.contains("canonical_utc_ms") {
            refuse(StatusCode::UNPROCESSABLE_ENTITY, "This bundle's resolved CSV does not have the SD-resolved recording CSV header.")
        } else {
            bad("data/telemetry.csv does not have the recording CSV header.")
        });
    }

    let _importing = IMPORTING.lock().await;
    if let Some(existing) = tokio::task::spawn_blocking({
        let (root, recording, csv_sha) = (root.to_path_buf(), recording.clone(), csv_sha.clone());
        move || already_imported(&root, &recording, &csv_sha)
    })
    .await
    .ok()
    .flatten()
    {
        return Err(Refusal { status: StatusCode::CONFLICT, message: format!("This recording was already imported as {existing}."), existing: Some(existing) });
    }

    let kind = store::EventKind::Imported;
    let kind_dir = store::events_dir(root).join(kind.id());
    std::fs::create_dir_all(&kind_dir).map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not create the import folder: {e}")))?;
    let start_hint = manifest["time_range"]["start_ms"].as_i64().unwrap_or(now_ms);
    let stem = recording.trim_end_matches(".db");
    let base = format!("{}_{}", store::stamp(start_hint), store::slug(stem));
    let (folder_name, folder) = store::unique_folder(&kind_dir, &base);
    std::fs::create_dir_all(&folder).map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not create the import folder: {e}")))?;
    let result = write_recording(&folder, &mut records, columns, files.get(ALERTS_PATH), resolved).await;
    let (rows, alerts, data_from, data_to) = match result {
        Ok(counts) => counts,
        Err(refusal) => {
            let _ = std::fs::remove_dir_all(&folder);
            return Err(refusal);
        }
    };

    let id = store::RecordingId::event_id(kind, &folder_name);
    let station = source["station_name"].as_str().filter(|s| !s.is_empty()).unwrap_or("another station");
    let label: String = format!("{stem} from {station}").chars().take(120).collect();
    let (from, to) = (data_from.unwrap_or(start_hint), data_to.unwrap_or(start_hint));
    let event_start = manifest["time_range"]["start_ms"].as_i64().unwrap_or(from);
    let event_end = manifest["time_range"]["end_ms"].as_i64().unwrap_or(to);
    let import = json!({
        "source": source,
        "csv_sha256": csv_sha,
        "bundle_sha256": bundle_sha,
        "bundle_created_ms": manifest["created_ms"],
        "generator": manifest["generator"],
        "mode": manifest["mode"],
        "recorded_mode": manifest["recorded_mode"],
        "imported_at_ms": now_ms,
    });
    let saved = store::Manifest {
        format: "seds-gs-event-capture".into(),
        version: 1,
        id: id.clone(),
        event_type: kind,
        title: kind.title().into(),
        label: label.clone(),
        event_start_ms: event_start,
        event_end_ms: event_end,
        window_start_ms: from.min(event_start),
        window_end_ms: to.max(event_end),
        pre_roll_s: 0,
        post_roll_s: 0,
        station_mode: manifest["recorded_mode"].as_str().unwrap_or("unknown").to_owned(),
        provenance: None,
        simulated: false,
        versions: json!({"groundstation_backend": env!("CARGO_PKG_VERSION"), "source_generator": manifest["generator"]}),
        operator: operator.map(str::to_owned),
        result: Some("imported".into()),
        rows: BTreeMap::from([("telemetry".to_owned(), rows), ("alerts".to_owned(), alerts)]),
        data_from_ms: data_from,
        data_to_ms: data_to,
        pre_roll_truncated: false,
        source_segments: Vec::new(),
        saved_at_ms: now_ms,
        details: json!({"import": import}),
        board_sd: Vec::new(),
    };
    let written = serde_json::to_vec_pretty(&saved)
        .map_err(std::io::Error::other)
        .and_then(|bytes| store::write_atomic(&folder.join(store::MANIFEST_FILE), &bytes));
    if let Err(e) = written {
        let _ = std::fs::remove_dir_all(&folder);
        return Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not save the import manifest: {e}")));
    }
    log::info!("imported replay bundle {recording} from {station} as {id} ({rows} rows)");
    Ok(json!({
        "id": id,
        "label": label,
        "rows": rows,
        "alerts": alerts,
        "start_ms": data_from,
        "end_ms": data_to,
        "source": saved.details["import"]["source"],
        "csv_sha256": saved.details["import"]["csv_sha256"],
    }))
}

/// Writes `recording.db` in `folder` from the CSV records (and the recorded
/// alerts); returns the row and alert counts and the reception-time range.
async fn write_recording(
    folder: &Path,
    records: &mut Records<'_>,
    columns: usize,
    alerts: Option<&Vec<u8>>,
    resolved: Option<Resolved<'_>>,
) -> Result<(i64, i64, Option<i64>, Option<i64>), Refusal> {
    let storage = |e: &dyn std::fmt::Display| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not write the imported recording: {e}"));
    let partial = folder.join(store::PARTIAL_FILE);
    let (db, db_path) = crate::telemetry_db::open_telemetry_db(&partial).await.map_err(|e| storage(&e))?;
    let written = async {
        let mut tx = db.begin().await.map_err(|e| storage(&e))?;
        let (mut rows, mut last_id, mut from, mut to) = (0i64, 0i64, None::<i64>, None::<i64>);
        let mut line = 1;
        while let Some(record) = records.next_record() {
            line += 1;
            let mut cells = record.map_err(bad)?;
            if cells.len() == 1 && cells[0].is_empty() && records.at >= records.bytes.len() {
                break;
            }
            if cells.len() != columns {
                return Err(bad(format!("CSV line {line} has {} cells, not {columns}.", cells.len())));
            }
            cells.truncate(7);
            let payload = stored_text(cells.pop().unwrap_or_default());
            let values = stored_text(cells.pop().unwrap_or_default());
            let data_type = stored_text(cells.pop().unwrap_or_default());
            let sender = stored_text(cells.pop().unwrap_or_default());
            let id = parse_ms(&cells[0], "id", line)?;
            let received = parse_ms(&cells[1], "received_timestamp_ms", line)?;
            let source = if cells[2].is_empty() { None } else { Some(parse_ms(&cells[2], "source_timestamp_ms", line)?) };
            if id <= last_id {
                return Err(bad(format!("CSV line {line}: row ids must increase.")));
            }
            if data_type.is_empty() {
                return Err(bad(format!("CSV line {line}: the row has no data type.")));
            }
            last_id = id;
            let (values, over_range) = stored_values(&values);
            sqlx::query("INSERT INTO telemetry(id,timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json,over_range) VALUES(?,?,?,?,?,?,?,?)")
                .bind(id)
                .bind(received)
                .bind(source)
                .bind((!sender.is_empty()).then_some(sender))
                .bind(data_type)
                .bind(values)
                .bind((!payload.is_empty()).then_some(payload))
                .bind(over_range)
                .execute(&mut *tx)
                .await
                .map_err(|e| storage(&e))?;
            rows += 1;
            from = Some(from.map_or(received, |f: i64| f.min(received)));
            to = Some(to.map_or(received, |t: i64| t.max(received)));
        }
        let mut alert_rows = 0i64;
        let recorded = alerts.and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
        for alert in recorded.as_ref().and_then(|a| a["alerts"].as_array()).into_iter().flatten() {
            let (Some(at), Some(severity), Some(message)) = (alert["timestamp_ms"].as_i64(), alert["severity"].as_str(), alert["message"].as_str()) else { continue };
            sqlx::query("INSERT INTO alerts(timestamp_ms,severity,message) VALUES(?,?,?)")
                .bind(at)
                .bind(severity)
                .bind(message)
                .execute(&mut *tx)
                .await
                .map_err(|e| storage(&e))?;
            alert_rows += 1;
        }
        if let Some(resolved) = resolved {
            write_resolved(&mut tx, resolved).await?;
        }
        tx.commit().await.map_err(|e| storage(&e))?;
        Ok((rows, alert_rows, from, to))
    }
    .await;
    crate::telemetry_db::close_and_finalize_sqlite(db, &db_path).await;
    let counts = written?;
    std::fs::rename(&partial, folder.join(store::CAPTURE_FILE)).map_err(|e| storage(&e))?;
    Ok(counts)
}

/// The SD-resolved part of a bundle: the resolved CSV's rows (header read)
/// and what describes the SD imports.
struct Resolved<'a> {
    rows: Records<'a>,
    columns: usize,
    sd: SdRecords,
}

/// The SD imports' descriptions a bundle carries.
#[derive(Default)]
struct SdRecords {
    /// `replay.json` `sd_provenance.resolution`.
    resolution: Option<Value>,
    /// `replay.json` `sd_provenance.imports`.
    imports: Vec<Value>,
    /// The SD source list's rows (`derived/sd_sources.json` and its later pages), by import id.
    sources: BTreeMap<i64, Value>,
    /// Original SD files (`sd/sources/{sha256}.csv`).
    files: BTreeMap<String, Vec<u8>>,
}

impl SdRecords {
    fn of(files: &BTreeMap<String, Vec<u8>>) -> Self {
        let json = |path: &str| files.get(path).and_then(|b| serde_json::from_slice::<Value>(b).ok());
        let provenance = json("replay.json").map(|r| r["sd_provenance"].clone()).unwrap_or(Value::Null);
        let mut out = SdRecords { resolution: provenance.get("resolution").cloned().filter(Value::is_object), imports: provenance["imports"].as_array().cloned().unwrap_or_default(), ..Default::default() };
        for (path, bytes) in files {
            if path == "derived/sd_sources.json" || (path.starts_with("derived/sd_sources/") && path.ends_with(".json")) {
                let listing = serde_json::from_slice::<Value>(bytes).unwrap_or(Value::Null);
                for source in listing["sources"].as_array().into_iter().flatten() {
                    if let Some(id) = source["import_id"].as_i64() {
                        out.sources.insert(id, source.clone());
                    }
                }
            }
            if let Some(sha) = path.strip_prefix("sd/sources/").and_then(|p| p.strip_suffix(".csv"))
                && sha.len() == 64
                && sha.bytes().all(|b| b.is_ascii_hexdigit())
                && sha256_hex(bytes) == sha
            {
                out.files.insert(sha.to_owned(), bytes.clone());
            }
        }
        out
    }
}

/// Recreates the resolved timeline and the SD import records (`recording_sd` tables).
async fn write_resolved(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, mut resolved: Resolved<'_>) -> Result<(), Refusal> {
    let storage = |e: &dyn std::fmt::Display| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not write the imported recording: {e}"));
    for statement in [
        "CREATE TABLE sd_replay_telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER NOT NULL,source_timestamp_ms INTEGER,data_type TEXT NOT NULL,sender_id TEXT,values_json TEXT,payload_json TEXT,provenance_json TEXT NOT NULL,over_range INTEGER)",
        "CREATE INDEX sd_replay_sample_origin ON sd_replay_telemetry(json_extract(provenance_json,'$.sd_sample_id'))",
        "CREATE INDEX sd_replay_time ON sd_replay_telemetry(timestamp_ms)",
        "CREATE INDEX sd_replay_series ON sd_replay_telemetry(data_type,sender_id,timestamp_ms)",
        "CREATE TABLE sd_replay_loadcell_units(id INTEGER PRIMARY KEY CHECK(id=1),unit TEXT NOT NULL)",
        "INSERT INTO sd_replay_loadcell_units VALUES(1,'adc_volts')",
        "CREATE TABLE sd_imports(id INTEGER PRIMARY KEY, file_sha256 TEXT NOT NULL, filename TEXT NOT NULL, board_id TEXT NOT NULL, boot_id TEXT NOT NULL, stream_id TEXT NOT NULL, launch_session TEXT, schema_version TEXT NOT NULL, config_json TEXT NOT NULL, metadata_json TEXT NOT NULL, manifest_json TEXT NOT NULL, UNIQUE(file_sha256,board_id,boot_id,stream_id))",
        "CREATE TABLE sd_samples(id INTEGER PRIMARY KEY, identity_key TEXT NOT NULL, content_sha256 TEXT NOT NULL, sensor TEXT NOT NULL, data_type TEXT, timestamp_ms INTEGER NOT NULL, monotonic_ms INTEGER NOT NULL, time_source TEXT NOT NULL, uptime_ticks TEXT NOT NULL, uptime_frequency_hz INTEGER NOT NULL, record_sequence TEXT NOT NULL, fields_json TEXT NOT NULL, UNIQUE(identity_key,content_sha256))",
        "CREATE TABLE sd_source_chunks(file_sha256 TEXT NOT NULL,offset INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(file_sha256,offset))",
    ] {
        sqlx::query(statement).execute(&mut **tx).await.map_err(|e| storage(&e))?;
    }
    let (mut line, mut last_id) = (1, 0i64);
    while let Some(record) = resolved.rows.next_record() {
        line += 1;
        let cells = record.map_err(bad)?;
        if cells.len() == 1 && cells[0].is_empty() && resolved.rows.at >= resolved.rows.bytes.len() {
            break;
        }
        if cells.len() != resolved.columns {
            return Err(bad(format!("data/telemetry.csv line {line} has {} cells, not {}.", cells.len(), resolved.columns)));
        }
        let mut cells = cells.into_iter();
        let mut next = || cells.next().unwrap_or_default();
        let (id, _received, source) = (next(), next(), next());
        let (sender, data_type, values, payload) = (stored_text(next()), stored_text(next()), stored_text(next()), stored_text(next()));
        let (canonical, provenance) = (next(), stored_text(next()));
        let id = parse_ms(&id, "id", line)?;
        if id <= last_id {
            return Err(bad(format!("data/telemetry.csv line {line}: row ids must increase.")));
        }
        last_id = id;
        let source = if source.is_empty() { None } else { Some(parse_ms(&source, "source_timestamp_ms", line)?) };
        if serde_json::from_str::<Value>(&provenance).map_or(true, |p| !matches!(p["origin"].as_str(), Some("sd" | "network"))) {
            return Err(bad(format!("data/telemetry.csv line {line}: the row has no SD or network provenance.")));
        }
        let (values, over_range) = stored_values(&values);
        sqlx::query("INSERT INTO sd_replay_telemetry VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(id)
            .bind(parse_ms(&canonical, "canonical_utc_ms", line)?)
            .bind(source)
            .bind(data_type)
            .bind((!sender.is_empty()).then_some(sender))
            .bind(values)
            .bind((!payload.is_empty()).then_some(payload))
            .bind(provenance)
            .bind(over_range)
            .execute(&mut **tx)
            .await
            .map_err(|e| storage(&e))?;
    }
    let sd = resolved.sd;
    if let Some(resolution) = &sd.resolution {
        sqlx::query("CREATE TABLE sd_resolution_manifest(id INTEGER PRIMARY KEY CHECK(id=1),manifest_json TEXT NOT NULL)").execute(&mut **tx).await.map_err(|e| storage(&e))?;
        sqlx::query("INSERT INTO sd_resolution_manifest VALUES(1,?)").bind(resolution.to_string()).execute(&mut **tx).await.map_err(|e| storage(&e))?;
    }
    for import in &sd.imports {
        let (Some(id), Some(sha)) = (import["id"].as_i64(), import["file_sha256"].as_str()) else { continue };
        let manifest = &import["manifest"];
        let listed = sd.sources.get(&id);
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let field = |name: &str| listed.and_then(|l| text(&l[name])).or_else(|| text(&manifest[name])).unwrap_or_default();
        sqlx::query("INSERT OR IGNORE INTO sd_imports VALUES(?,?,?,?,?,?,?,?,?,?,?)")
            .bind(id)
            .bind(sha)
            .bind(listed.and_then(|l| text(&l["filename"])).unwrap_or_else(|| format!("sd-{sha}.csv")))
            .bind(field("board_id"))
            .bind(field("boot_id"))
            .bind(field("stream_id"))
            .bind(listed.and_then(|l| text(&l["launch_session"])).or_else(|| text(&manifest["launch_session"])))
            .bind("imported-replay-bundle")
            .bind(manifest.get("config").map_or_else(|| "{}".to_owned(), Value::to_string))
            .bind(import["calibration_metadata"].to_string())
            .bind(manifest.to_string())
            .execute(&mut **tx)
            .await
            .map_err(|e| storage(&e))?;
    }
    for (sha, bytes) in &sd.files {
        for (n, chunk) in bytes.chunks(64 * 1024).enumerate() {
            sqlx::query("INSERT OR IGNORE INTO sd_source_chunks VALUES(?,?,?)").bind(sha).bind((n * 64 * 1024) as i64).bind(chunk).execute(&mut **tx).await.map_err(|e| storage(&e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "recording_import_tests.rs"]
mod tests;
