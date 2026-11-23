//! What a replay id can name, and the read-only database each one is served from.
//!
//! - a saved file: a legacy session, a rolling segment, an event capture or an
//!   attachment beside it (`recording_store::RecordingId`), opened read only;
//! - `range.<start_ms>.<end_ms>`: a time range of the rolling store, across as
//!   many segments as it touches;
//! - `merge-sd.<capture id>`: an event capture with its board SD files
//!   (`board_sd/DAQ|FC/`, launch file and lead-up) merged in on network time;
//! - `sd-only.<capture id>`: those board SD files alone (with the capture's
//!   station-mode context rows, so the recorded mode is still known);
//! - `fire.<fire archive id>`: a fire archive entry: its capture (with board
//!   SD files) when the capture is still on disk, else its saved thrust curve.
//!
//! Composed sources are built once into a private in-memory database and kept
//! in a small cache. Nothing here writes a recording, the ring, a capture
//! folder or any live station state: replay only reads files.
use crate::burn_report::merge::{Channel, csv_fields};
use crate::recording_store::{self, RecordingId, board_sd};
use crate::state::AppState;
use axum::http::StatusCode;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Longest rolling-store range one replay may span.
pub(crate) const MAX_RANGE_MS: i64 = 6 * 3_600_000;
/// Rolling segments a range replay may read in total (database + WAL).
const MAX_RANGE_BYTES: u64 = 768 * 1024 * 1024;
/// Board SD rows merged into one replay.
const MAX_SD_ROWS: usize = 4_000_000;
const CACHE_ENTRIES: usize = 3;
const CACHE_IDLE: Duration = Duration::from_secs(15 * 60);

/// Sender of rows that came from a board's SD card rather than the radio
/// link, so the two never share a series.
pub(crate) fn sd_sender(board: &str) -> String {
    format!("{board}_SD")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    File(String),
    Range { start_ms: i64, end_ms: i64 },
    MergeSd(String),
    SdOnly(String),
    Fire(String),
}

impl Source {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        if let Some(rest) = id.strip_prefix("range.") {
            let (a, b) = rest.split_once('.')?;
            let digits = |s: &str| !s.is_empty() && s.len() <= 16 && s.bytes().all(|c| c.is_ascii_digit());
            if !digits(a) || !digits(b) {
                return None;
            }
            let (start_ms, end_ms) = (a.parse().ok()?, b.parse().ok()?);
            return (end_ms > start_ms).then_some(Self::Range { start_ms, end_ms });
        }
        let capture = |rest: &str| {
            matches!(RecordingId::parse(rest), Some(RecordingId::Event { file: None, .. })).then(|| rest.to_owned())
        };
        if let Some(rest) = id.strip_prefix("merge-sd.") {
            return capture(rest).map(Self::MergeSd);
        }
        if let Some(rest) = id.strip_prefix("sd-only.") {
            return capture(rest).map(Self::SdOnly);
        }
        if let Some(rest) = id.strip_prefix("fire.") {
            let ok = !rest.is_empty()
                && rest.len() <= 96
                && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            return ok.then(|| Self::Fire(rest.to_owned()));
        }
        RecordingId::parse(id).map(|_| Self::File(id.to_owned()))
    }
    /// `legacy`, `rolling`, `capture`, `attachment`, `range`, `merge_sd`,
    /// `sd_only` or `fire`.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::File(id) => match RecordingId::parse(id) {
                Some(RecordingId::Legacy(_)) => "legacy",
                Some(RecordingId::Segment(_)) => "rolling",
                Some(RecordingId::Event { file: None, .. }) => "capture",
                _ => "attachment",
            },
            Self::Range { .. } => "range",
            Self::MergeSd(_) => "merge_sd",
            Self::SdOnly(_) => "sd_only",
            Self::Fire(_) => "fire",
        }
    }
    /// The event capture this source belongs to, if any.
    pub(crate) fn capture_id(&self) -> Option<String> {
        match self {
            Self::File(id) => match RecordingId::parse(id)? {
                RecordingId::Event { kind, folder, .. } => Some(RecordingId::event_id(kind, &folder)),
                _ => None,
            },
            Self::MergeSd(id) | Self::SdOnly(id) => Some(id.clone()),
            Self::Range { .. } | Self::Fire(_) => None,
        }
    }
}

/// The capture folder of a capture id, resolved through the guarded lookup.
pub(crate) async fn capture_folder(root: &Path, capture_id: &str) -> Option<PathBuf> {
    let path = crate::recording_export::recording_path(root, capture_id).await.ok()?;
    path.parent().map(Path::to_path_buf)
}

/// Opens the replay database for `id`. Saved files are opened read only;
/// composed sources come from (or are built into) the in-memory cache.
pub(crate) async fn open(state: &AppState, id: &str) -> Result<SqlitePool, StatusCode> {
    let source = Source::parse(id).ok_or(StatusCode::BAD_REQUEST)?;
    let root = crate::recording_export::directory(state);
    match source {
        Source::File(id) => open_file(&root, &id).await,
        other => cached(&root, id, &other).await,
    }
}

async fn open_file(root: &Path, id: &str) -> Result<SqlitePool, StatusCode> {
    let path = crate::recording_export::recording_path(root, id).await?;
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path).read_only(true).create_if_missing(false))
        .await
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    crate::recording_sd::activate_resolved(&db).await.map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    Ok(db)
}

struct Entry {
    key: String,
    pool: SqlitePool,
    used: Instant,
}
static CACHE: tokio::sync::Mutex<Vec<Entry>> = tokio::sync::Mutex::const_new(Vec::new());

async fn cached(root: &Path, id: &str, source: &Source) -> Result<SqlitePool, StatusCode> {
    // Keyed by data directory too: tests and firmware-sim stations use their own.
    let key = format!("{}\u{0}{id}", root.display());
    let mut cache = CACHE.lock().await;
    cache.retain(|e| e.used.elapsed() < CACHE_IDLE);
    if let Some(entry) = cache.iter_mut().find(|e| e.key == key) {
        entry.used = Instant::now();
        return Ok(entry.pool.clone());
    }
    let pool = build(root, source).await?;
    if cache.len() >= CACHE_ENTRIES {
        cache.sort_by_key(|e| e.used);
        cache.remove(0);
    }
    cache.push(Entry { key, pool: pool.clone(), used: Instant::now() });
    Ok(pool)
}

/// A private in-memory database. One connection that never idles out: the
/// database lives exactly as long as the pool.
async fn memory_db() -> anyhow::Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        // A plain ":memory:" name (not the in-memory open flag), so ATTACH still opens files.
        .connect_with(SqliteConnectOptions::new().filename(":memory:"))
        .await?;
    crate::telemetry_db::ensure_telemetry_schema(&pool).await?;
    Ok(pool)
}

async fn build(root: &Path, source: &Source) -> Result<SqlitePool, StatusCode> {
    let result = match source {
        Source::Range { start_ms, end_ms } => build_range(root, *start_ms, *end_ms).await,
        Source::MergeSd(capture) => build_capture(root, capture, true, true).await,
        Source::SdOnly(capture) => build_capture(root, capture, false, true).await,
        Source::Fire(id) => build_fire(root, id).await,
        Source::File(_) => unreachable!("files are opened directly"),
    };
    result.map_err(|error| {
        log::info!("replay source unavailable: {error:#}");
        match error.downcast_ref::<SourceError>() {
            Some(SourceError::NotFound) => StatusCode::NOT_FOUND,
            Some(SourceError::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
            Some(SourceError::Invalid) => StatusCode::BAD_REQUEST,
            None => StatusCode::UNPROCESSABLE_ENTITY,
        }
    })
}

#[derive(Debug)]
enum SourceError {
    NotFound,
    TooLarge,
    Invalid,
}
impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "replay source not found",
            Self::TooLarge => "replay source too large",
            Self::Invalid => "invalid replay source",
        })
    }
}
impl std::error::Error for SourceError {}

async fn build_range(root: &Path, start: i64, end: i64) -> anyhow::Result<SqlitePool> {
    if start < 0 || end <= start || end - start > MAX_RANGE_MS {
        return Err(SourceError::Invalid.into());
    }
    let rolling = recording_store::rolling_dir(root);
    let segments = recording_store::segments_for_window(&recording_store::scan_segments(&rolling), start, end);
    if segments.is_empty() {
        return Err(SourceError::NotFound.into());
    }
    if segments.iter().map(|s| s.bytes).sum::<u64>() > MAX_RANGE_BYTES {
        return Err(SourceError::TooLarge.into());
    }
    let db = memory_db().await?;
    recording_store::copy_window(&db, &rolling, start, end).await?;
    Ok(db)
}

async fn attach(db: &SqlitePool, path: &Path) -> Result<(), sqlx::Error> {
    sqlx::query("ATTACH DATABASE ? AS src").bind(path.to_string_lossy().into_owned()).execute(db).await.map(|_| ())
}
async fn detach(db: &SqlitePool) {
    let _ = sqlx::query("DETACH DATABASE src").execute(db).await;
}
async fn src_has(db: &SqlitePool, table: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM src.sqlite_master WHERE type='table' AND name=?")
        .bind(table)
        .fetch_one(db)
        .await
        .unwrap_or(0)
        > 0
}

/// Copies a capture's rows (keeping their ids and load-cell unit mark) or,
/// without `rows`, only its station-mode context rows.
async fn copy_capture(db: &SqlitePool, path: &Path, rows: bool) -> anyhow::Result<()> {
    attach(db, path).await?;
    let result: anyhow::Result<()> = async {
        if !rows {
            sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) SELECT timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json FROM src.telemetry WHERE data_type IN ('GS_STATION_MODE','GS_IGT') ORDER BY id")
                .execute(db).await?;
            return Ok(());
        }
        sqlx::query("INSERT INTO telemetry(id,timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) SELECT id,timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json FROM src.telemetry ORDER BY id")
            .execute(db).await?;
        sqlx::query("INSERT INTO alerts(timestamp_ms,severity,message,condition_json) SELECT timestamp_ms,severity,message,condition_json FROM src.alerts ORDER BY id").execute(db).await?;
        sqlx::query("INSERT INTO flight_state(timestamp_ms,f_state) SELECT timestamp_ms,f_state FROM src.flight_state ORDER BY id").execute(db).await?;
        sqlx::query("INSERT OR IGNORE INTO messages(id,timestamp_ms,message,action_label,action_cmd) SELECT id,timestamp_ms,message,action_label,action_cmd FROM src.messages").execute(db).await?;
        if src_has(db, "calibration_history").await {
            sqlx::query("CREATE TABLE IF NOT EXISTS calibration_history (timestamp_ms INTEGER NOT NULL, sender_id TEXT NOT NULL, sensor_id TEXT NOT NULL, config_json TEXT NOT NULL, PRIMARY KEY(timestamp_ms,sender_id,sensor_id))").execute(db).await?;
            sqlx::query("INSERT OR REPLACE INTO calibration_history SELECT timestamp_ms,sender_id,sensor_id,config_json FROM src.calibration_history").execute(db).await?;
        }
        // Row ids are kept, so the capture's own load-cell unit mark applies.
        let first: i64 = if src_has(db, "loadcell_units").await {
            sqlx::query_scalar("SELECT first_volts_id FROM src.loadcell_units WHERE id=1").fetch_optional(db).await?.unwrap_or(i64::MAX)
        } else {
            i64::MAX
        };
        sqlx::query("UPDATE loadcell_units SET first_volts_id=? WHERE id=1").bind(first).execute(db).await?;
        Ok(())
    }
    .await;
    detach(db).await;
    result
}

/// One telemetry row converted from a board SD CSV line.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SdRow {
    pub timestamp_ms: i64,
    pub data_type: String,
    pub sender_id: String,
    pub values: Vec<f32>,
}

fn sd_type_name(board: &str, sensor: &str) -> String {
    let clean: String = sensor
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .take(64)
        .collect();
    format!("{board}_SD_{clean}")
}

/// A board SD CSV row (`timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,
/// raw_value,calibrated_value,time_source,...`) as a replay row. Only rows on
/// network time can be aligned with the station's telemetry; the DAQ's load
/// cells and chamber PT become the station's own data types (sender `DAQ_SD`),
/// every other sensor `<BOARD>_SD_<SENSOR>`. `Err(())` marks a data row on the
/// board's local clock.
pub(crate) fn sd_row(board: &str, line: &str) -> Option<Result<SdRow, ()>> {
    if line.is_empty() || line.starts_with('#') || line.starts_with("timestamp_ms") {
        return None;
    }
    let fields = csv_fields(line);
    if fields.len() < 8 {
        return None;
    }
    if fields[7].trim() != "network" {
        return Some(Err(()));
    }
    let timestamp_ms = fields[0].trim().parse::<i64>().ok()?;
    let sensor = fields[2].trim();
    let parse = |text: &str| -> Vec<f32> {
        text.split(|c: char| c.is_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .take(64)
            .collect()
    };
    let channel = (board == "DAQ").then(|| Channel::ALL.into_iter().find(|c| c.daq_sensor() == sensor)).flatten();
    let values = match channel {
        // The burn report's reading of the same row: value, else calibrated.
        Some(_) => [3usize, 6]
            .iter()
            .filter_map(|i| fields.get(*i).and_then(|f| f.trim().parse::<f32>().ok()))
            .find(|v| v.is_finite())
            .map(|v| vec![v])
            .unwrap_or_default(),
        None => {
            let v = parse(&fields[3]);
            if v.is_empty() { parse(&fields[6]) } else { v }
        }
    };
    if values.is_empty() {
        return None;
    }
    let data_type = match channel {
        Some(c) => c.gs_type().0.to_owned(),
        None => sd_type_name(board, sensor),
    };
    Some(Ok(SdRow { timestamp_ms, data_type, sender_id: sd_sender(board), values }))
}

/// Network-time rows of one board SD file, and how many data rows were on the
/// board's local clock (not alignable, skipped).
pub(crate) fn read_sd_file(board: &str, path: &Path, limit: usize) -> anyhow::Result<(Vec<SdRow>, u64)> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let (mut rows, mut local) = (Vec::new(), 0u64);
    for line in reader.lines() {
        match sd_row(board, line?.trim_end()) {
            Some(Ok(row)) => {
                if rows.len() >= limit {
                    return Err(SourceError::TooLarge.into());
                }
                rows.push(row);
            }
            Some(Err(())) => local += 1,
            None => {}
        }
    }
    Ok((rows, local))
}

async fn insert_sd_rows(db: &SqlitePool, rows: &[SdRow]) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    for row in rows {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json) VALUES(?,?,?,?,?)")
            .bind(row.timestamp_ms)
            .bind(row.timestamp_ms)
            .bind(&row.data_type)
            .bind(&row.sender_id)
            .bind(serde_json::to_string(&row.values)?)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The board SD files attached to a capture: (board, file name, path).
pub(crate) fn attached_sd_files(capture_db: &Path) -> Vec<(String, String, PathBuf)> {
    let Some(manifest) = capture_db.parent().and_then(recording_store::read_manifest) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in &manifest.board_sd {
        for name in entry.lead_up_file.iter().chain(std::iter::once(&entry.file)) {
            if let Some(path) = board_sd::attached_path(capture_db, &entry.board, name) {
                out.push((entry.board.clone(), name.clone(), path));
            }
        }
    }
    out
}

async fn build_capture(root: &Path, capture: &str, rows: bool, sd: bool) -> anyhow::Result<SqlitePool> {
    let path = crate::recording_export::recording_path(root, capture).await.map_err(|_| SourceError::NotFound)?;
    let db = memory_db().await?;
    copy_capture(&db, &path, rows).await?;
    if sd {
        let files = attached_sd_files(&path);
        if !rows && files.is_empty() {
            return Err(SourceError::NotFound.into());
        }
        let mut budget = MAX_SD_ROWS;
        for (board, _name, file) in files {
            let (sd_rows, _local) = {
                let file = file.clone();
                tokio::task::spawn_blocking(move || read_sd_file(&board, &file, budget)).await??
            };
            budget = budget.saturating_sub(sd_rows.len());
            insert_sd_rows(&db, &sd_rows).await?;
        }
    }
    Ok(db)
}

/// Data type of the thrust curve rows a fire archive entry without its
/// capture replays from.
pub(crate) const THRUST_TYPE: &str = "BURN_REPORT_THRUST";

/// The thrust curve's time zero (network ms) for a fire archive entry: its
/// burn report's ignition reference, else midnight UTC of its date.
pub(crate) fn fire_t0(root: &Path, record: &crate::fire_archive::FireRecord) -> i64 {
    if let Some(report) = record.burn_report_id.as_deref().and_then(|id| crate::burn_report::find(root, id)) {
        return report.ignition_ms;
    }
    static FORMAT: &[time::format_description::FormatItem<'static>] = time::macros::format_description!("[year]-[month]-[day]");
    time::Date::parse(&record.date, FORMAT)
        .map(|d| d.midnight().assume_utc().unix_timestamp() * 1000)
        .unwrap_or(0)
}

async fn build_fire(root: &Path, id: &str) -> anyhow::Result<SqlitePool> {
    let record = crate::fire_archive::get(root, id).ok_or(SourceError::NotFound)?;
    if let Some(capture) = record.capture_id.as_deref()
        && matches!(RecordingId::parse(capture), Some(RecordingId::Event { file: None, .. }))
        && crate::recording_export::recording_path(root, capture).await.is_ok()
    {
        return build_capture(root, capture, true, true).await;
    }
    let curve = crate::fire_archive::curve(root, id).unwrap_or_default();
    if curve.is_empty() {
        return Err(SourceError::NotFound.into());
    }
    let t0 = fire_t0(root, &record);
    let db = memory_db().await?;
    // A fire is a stand burn: test fire unless the archive says simulated.
    let mode = serde_json::json!({"mode": "test_fire", "igt_active": false, "source": "fire_archive",
        "provenance": if record.simulated { "simulated" } else { "hardware" }});
    let first = curve.first().map(|p| t0 + (p[0] * 1000.0) as i64).unwrap_or(t0);
    sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type,sender_id,payload_json) VALUES(?,'GS_STATION_MODE','GS',?)")
        .bind(first.min(t0))
        .bind(mode.to_string())
        .execute(&db)
        .await?;
    let rows: Vec<SdRow> = curve
        .iter()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .map(|p| SdRow {
            timestamp_ms: t0 + (p[0] * 1000.0).round() as i64,
            data_type: THRUST_TYPE.into(),
            sender_id: "GS".into(),
            values: vec![p[1] as f32],
        })
        .collect();
    insert_sd_rows(&db, &rows).await?;
    Ok(db)
}

#[cfg(test)]
pub(crate) async fn clear_cache() {
    CACHE.lock().await.clear();
}
