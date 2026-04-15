use anyhow::Result;
use sqlx::sqlite::SqliteConnection;
use sqlx::{Connection, Row, SqlitePool};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::time::Duration;

pub const RECORDING_BUFFER_WINDOW_MS: i64 = 2 * 60 * 1000;
pub const DEFAULT_TELEMETRY_DB_FILENAME: &str = "groundstation.db";

#[derive(Debug, Clone)]
pub enum DbWrite {
    FlightState {
        timestamp_ms: i64,
        state_code: i64,
    },
    Telemetry {
        timestamp_ms: i64,
        data_type: String,
        sender_id: String,
        values_json: Option<String>,
        payload_json: String,
    },
    Alert {
        timestamp_ms: i64,
        severity: String,
        message: String,
    },
}

impl DbWrite {
    pub fn timestamp_ms(&self) -> i64 {
        match self {
            Self::FlightState { timestamp_ms, .. }
            | Self::Telemetry { timestamp_ms, .. }
            | Self::Alert { timestamp_ms, .. } => *timestamp_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingMode {
    Idle,
    Recording,
    Paused,
}

#[derive(Debug, Clone, Copy)]
pub enum RecordingCommand {
    StartNow,
    StartWithRecent,
    Pause,
    Stop,
}

#[derive(Debug, Clone)]
pub enum DbQueueItem {
    Write(DbWrite),
    Control(RecordingCommand),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LaunchClockMsg {
    pub kind: LaunchClockKind,
    pub anchor_timestamp_ms: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchClockKind {
    Idle,
    TMinus,
    TPlus,
}

impl LaunchClockMsg {
    pub fn idle() -> Self {
        Self {
            kind: LaunchClockKind::Idle,
            anchor_timestamp_ms: None,
            duration_ms: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordingStatusMsg {
    pub mode: RecordingModeWire,
    pub db_path: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingModeWire {
    Idle,
    Recording,
    Paused,
}

impl From<RecordingMode> for RecordingModeWire {
    fn from(value: RecordingMode) -> Self {
        match value {
            RecordingMode::Idle => Self::Idle,
            RecordingMode::Recording => Self::Recording,
            RecordingMode::Paused => Self::Paused,
        }
    }
}

pub fn ensure_sqlite_db_file(path: &Path) -> Result<String> {
    if !path.exists() {
        fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))?;
        fs::write(path, b"")?;
        println!("Created empty DB file: {}", path.display());
    }
    Ok(fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string())
}

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn env_i64(name: &str, default: i64, min: i64, max: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

pub async fn apply_sqlite_pragmas(db: &SqlitePool) {
    let synchronous = std::env::var("GS_SQLITE_SYNCHRONOUS")
        .unwrap_or_else(|_| "NORMAL".to_string())
        .to_uppercase();
    let synchronous = match synchronous.as_str() {
        "OFF" | "NORMAL" | "FULL" | "EXTRA" => synchronous,
        _ => "NORMAL".to_string(),
    };

    let busy_timeout_ms = env_i64("GS_SQLITE_BUSY_TIMEOUT_MS", 5_000, 100, 120_000);
    let wal_autocheckpoint = env_i64("GS_SQLITE_WAL_AUTOCHECKPOINT", 1_000, 100, 100_000);
    let cache_kib = env_i64("GS_SQLITE_CACHE_SIZE_KIB", 32 * 1024, 1024, 512 * 1024);
    let cache_pages = -cache_kib;

    let pragmas = [
        "PRAGMA journal_mode=WAL;".to_string(),
        format!("PRAGMA synchronous={synchronous};"),
        "PRAGMA temp_store=MEMORY;".to_string(),
        format!("PRAGMA busy_timeout={busy_timeout_ms};"),
        format!("PRAGMA wal_autocheckpoint={wal_autocheckpoint};"),
        format!("PRAGMA cache_size={cache_pages};"),
    ];

    for stmt in pragmas {
        if let Err(err) = sqlx::query(&stmt).execute(db).await {
            eprintln!("SQLite pragma failed ({stmt}): {err}");
        }
    }
}

pub async fn ensure_telemetry_schema(db: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            data_type    TEXT    NOT NULL,
            sender_id    TEXT,
            values_json  TEXT,
            payload_json TEXT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_timestamp_ms ON telemetry (timestamp_ms);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_data_type_timestamp_ms ON telemetry (data_type, timestamp_ms DESC);",
    )
    .execute(db)
    .await?;

    let cols = sqlx::query("PRAGMA table_info(telemetry)")
        .fetch_all(db)
        .await?;
    let has_values_json = cols
        .iter()
        .any(|row| row.get::<String, _>("name") == "values_json");
    if !has_values_json {
        sqlx::query("ALTER TABLE telemetry ADD COLUMN values_json TEXT")
            .execute(db)
            .await?;
    }
    let has_payload_json = cols
        .iter()
        .any(|row| row.get::<String, _>("name") == "payload_json");
    if !has_payload_json {
        sqlx::query("ALTER TABLE telemetry ADD COLUMN payload_json TEXT")
            .execute(db)
            .await?;
    }
    let has_sender_id = cols
        .iter()
        .any(|row| row.get::<String, _>("name") == "sender_id");
    if !has_sender_id {
        sqlx::query("ALTER TABLE telemetry ADD COLUMN sender_id TEXT")
            .execute(db)
            .await?;
    }

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS alerts (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            severity     TEXT    NOT NULL,
            message      TEXT    NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS flight_state (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            f_state      INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

pub async fn open_telemetry_db(path: &Path) -> Result<(SqlitePool, String)> {
    let db_path = ensure_sqlite_db_file(path)?;
    let db = SqlitePool::connect(&format!("sqlite://{}", db_path)).await?;
    apply_sqlite_pragmas(&db).await;
    ensure_telemetry_schema(&db).await?;
    Ok((db, db_path))
}

pub fn session_db_path(base_dir: &Path, started_at_ms: i64) -> PathBuf {
    base_dir.join(format!("groundstation_{started_at_ms}.db"))
}

async fn exec_pragma_with_retry(
    db: &SqlitePool,
    stmt: &str,
    retries: usize,
    delay_ms: u64,
) -> Result<(), sqlx::Error> {
    for attempt in 0..retries {
        match sqlx::query(stmt).execute(db).await {
            Ok(_) => return Ok(()),
            Err(err) => {
                if attempt + 1 >= retries {
                    return Err(err);
                }
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }
    Ok(())
}

pub async fn flush_sqlite_journals(db: &SqlitePool) {
    let retries = env_usize("GS_SQLITE_SHUTDOWN_PRAGMA_RETRIES", 8, 1, 60);
    let delay_ms = env_i64("GS_SQLITE_SHUTDOWN_PRAGMA_RETRY_DELAY_MS", 120, 10, 5_000) as u64;

    if let Err(err) =
        exec_pragma_with_retry(db, "PRAGMA wal_checkpoint(TRUNCATE);", retries, delay_ms).await
    {
        eprintln!("SQLite wal_checkpoint(TRUNCATE) failed after {retries} attempts: {err}");
    }
    if let Err(err) = exec_pragma_with_retry(db, "PRAGMA optimize;", retries, delay_ms).await {
        eprintln!("SQLite PRAGMA optimize failed after {retries} attempts: {err}");
    }
}

pub async fn finalize_sqlite_after_pool_close(db_path: &str) {
    let url = format!("sqlite://{db_path}");
    let mut conn = match SqliteConnection::connect(&url).await {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("Failed to reopen SQLite DB for finalization ({db_path}): {err}");
            return;
        }
    };

    for stmt in [
        "PRAGMA busy_timeout=5000;",
        "PRAGMA wal_checkpoint(TRUNCATE);",
        "PRAGMA optimize;",
    ] {
        if let Err(err) = sqlx::query(stmt).execute(&mut conn).await {
            eprintln!("SQLite finalization pragma failed ({stmt}): {err}");
        }
    }

    let retries = env_usize("GS_SQLITE_SHUTDOWN_PRAGMA_RETRIES", 8, 1, 60);
    let retry_delay_ms = env_i64("GS_SQLITE_SHUTDOWN_PRAGMA_RETRY_DELAY_MS", 120, 10, 5_000) as u64;
    for attempt in 0..retries {
        match sqlx::query("PRAGMA journal_mode=DELETE;")
            .execute(&mut conn)
            .await
        {
            Ok(_) => break,
            Err(err) => {
                if attempt + 1 >= retries {
                    eprintln!(
                        "SQLite finalization pragma failed (PRAGMA journal_mode=DELETE;): {err}"
                    );
                } else {
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                }
            }
        }
    }

    if let Err(err) = conn.close().await {
        eprintln!("Failed closing SQLite finalization connection: {err}");
    }
}

pub async fn remove_sqlite_sidecars(db_path: &str) {
    let retries = env_usize("GS_SQLITE_SIDECAR_DELETE_RETRIES", 12, 1, 120);
    let retry_delay_ms = env_i64("GS_SQLITE_SIDECAR_DELETE_DELAY_MS", 100, 10, 2_000) as u64;
    for suffix in [".wal", ".shm", "-wal", "-shm", "-journal", ".journal"] {
        let sidecar = format!("{db_path}{suffix}");
        for attempt in 0..retries {
            match fs::remove_file(&sidecar) {
                Ok(()) => break,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                Err(err) => {
                    if attempt + 1 >= retries {
                        eprintln!("Failed removing SQLite sidecar {sidecar}: {err}");
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                }
            }
        }
    }
}

pub fn sqlite_sidecars_present(db_path: &str) -> Vec<String> {
    [".wal", ".shm", "-wal", "-shm", "-journal", ".journal"]
        .into_iter()
        .map(|suffix| format!("{db_path}{suffix}"))
        .filter(|p| Path::new(p).exists())
        .collect()
}

pub async fn close_and_finalize_sqlite(db: SqlitePool, db_path: &str) {
    flush_sqlite_journals(&db).await;
    db.close().await;
    finalize_sqlite_after_pool_close(db_path).await;
    remove_sqlite_sidecars(db_path).await;
    let lingering = sqlite_sidecars_present(db_path);
    if !lingering.is_empty() {
        eprintln!(
            "WARNING: SQLite sidecar files still present after shutdown cleanup: {}",
            lingering.join(", ")
        );
    }
}

pub fn prune_recent_writes(buffer: &mut VecDeque<DbWrite>, newest_ts_ms: i64) {
    let cutoff = newest_ts_ms.saturating_sub(RECORDING_BUFFER_WINDOW_MS);
    while let Some(front) = buffer.front() {
        if front.timestamp_ms() < cutoff {
            buffer.pop_front();
        } else {
            break;
        }
    }
}
