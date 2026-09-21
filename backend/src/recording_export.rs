//! Read-only, bounded-memory CSV downloads in every operating mode.
use crate::{auth::Permission, state::AppState};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::stream;
use serde::Serialize;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

const HEADER: &str =
    "id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json\r\n";
const PAGE_SIZE: i64 = 512;

#[derive(Serialize)]
struct Recording {
    id: String,
    bytes: u64,
    active: bool,
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/recordings", get(list))
        .route("/api/recordings/report", get(crate::recording_report::report))
        .route("/api/recordings/csv", get(crate::recording_range::download))
        .route("/api/system/time", get(crate::system_clock::status).post(crate::system_clock::sync))
        .route("/api/recordings/{id}/csv", get(download))
}

pub(crate) fn directory(state: &AppState) -> PathBuf {
    FsPath::new(&state.placeholder_db_path)
        .parent()
        .unwrap_or(FsPath::new("."))
        .to_path_buf()
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.strip_prefix("groundstation_recording_")
        .and_then(|s| s.strip_suffix(".db"))
        .is_some_and(|stamp| {
            !stamp.is_empty()
                && stamp
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'-' || b == b'_')
        })
}

pub(crate) async fn recording_path(root: &FsPath, id: &str) -> Result<PathBuf, StatusCode> {
    if !valid_id(id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let candidate = root.join(id);
    let meta = tokio::fs::symlink_metadata(&candidate)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(StatusCode::NOT_FOUND);
    }
    let resolved = tokio::fs::canonicalize(candidate)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if resolved.parent() != Some(root.as_path()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(resolved)
}

async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    let mut entries = match tokio::fs::read_dir(directory(&state)).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return recording_list_response(Vec::new());
        }
        Err(e) => {
            log::error!("listing recordings: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let active = state.recording_status_snapshot().db_path.and_then(|p| {
        FsPath::new(&p)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
    });
    let mut recordings = Vec::new();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                log::error!("reading recording entry: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_id(&id) {
            continue;
        }
        if let Ok(meta) = tokio::fs::symlink_metadata(entry.path()).await {
            if meta.is_file() && !meta.file_type().is_symlink() {
                recordings.push(Recording {
                    active: active.as_deref() == Some(&id),
                    id,
                    bytes: meta.len(),
                });
            }
        }
    }
    recordings.sort_by(|a, b| b.id.cmp(&a.id));
    recording_list_response(recordings)
}

fn recording_list_response(recordings: Vec<Recording>) -> Response {
    let mut response = Json(recordings).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    let path = match recording_path(&directory(&state), &id).await {
        Ok(path) => path,
        Err(status) => {
            return (
                status,
                "Recording not found or invalid recording name. Refresh the recording list.",
            )
                .into_response();
        }
    };
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false);
    let db = match SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
    {
        Ok(db) => db,
        Err(e) => {
            log::error!("opening CSV recording: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not open this recording for export.",
            )
                .into_response();
        }
    };
    let upper = match sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(id), 0) FROM telemetry")
        .fetch_one(&db)
        .await
    {
        Ok(upper) => upper,
        Err(e) => {
            log::error!("reading CSV recording: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "This recording has no readable telemetry table.",
            )
                .into_response();
        }
    };
    let body = stream::try_unfold((db, 0i64, false), move |(db, last, started)| async move {
        if !started {
            return Ok::<_, std::io::Error>(Some((HEADER.to_owned(), (db, last, true))));
        }
        let (chunk, next) = csv_page(&db, last, upper)
            .await
            .map_err(std::io::Error::other)?;
        if next == last {
            Ok(None)
        } else {
            Ok(Some((chunk, (db, next, true))))
        }
    });
    let filename = id.trim_end_matches(".db");
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}.csv\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

pub(crate) fn text_cell(value: &str) -> String {
    // Quote CSV syntax and neutralize spreadsheet formulas in textual metadata.
    let formula = value.trim_start().starts_with(['=', '+', '-', '@']);
    format!(
        "\"{}{}\"",
        if formula { "'" } else { "" },
        value.replace('"', "\"\"")
    )
}

async fn csv_page(db: &SqlitePool, last: i64, upper: i64) -> Result<(String, i64), sqlx::Error> {
    let rows = sqlx::query("SELECT id, timestamp_ms, source_timestamp_ms, sender_id, data_type, values_json, payload_json FROM telemetry WHERE id > ? AND id <= ? ORDER BY id LIMIT ?")
        .bind(last).bind(upper).bind(PAGE_SIZE).fetch_all(db).await?;
    let mut chunk = String::new();
    let mut next = last;
    for row in rows {
        next = row.try_get("id")?;
        let received: i64 = row.try_get("timestamp_ms")?;
        let source: Option<i64> = row.try_get("source_timestamp_ms")?;
        let sender: Option<String> = row.try_get("sender_id")?;
        let ty: String = row.try_get("data_type")?;
        let values: Option<String> = row.try_get("values_json")?;
        let payload: Option<String> = row.try_get("payload_json")?;
        chunk.push_str(&format!(
            "{next},{received},{},{},{},{},{}\r\n",
            source.map(|s| s.to_string()).unwrap_or_default(),
            text_cell(sender.as_deref().unwrap_or("")),
            text_cell(&ty),
            text_cell(values.as_deref().unwrap_or("")),
            text_cell(payload.as_deref().unwrap_or(""))
        ));
    }
    Ok((chunk, next))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct TestDirectory(pub PathBuf);
    impl TestDirectory {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "gs-csv-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) async fn export_state(root: &FsPath, allow: bool) -> Arc<AppState> {
        let mut state = crate::state::tests::test_app_state().await;
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        let auth_path = root.join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::json!({
                "version":1, "anonymous":{"view_data":allow,"send_commands":false}, "users":[]
            })
            .to_string(),
        )
        .unwrap();
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.placeholder_db_path = root.join("placeholder.db").to_string_lossy().into_owned();
        inner.auth = Arc::new(crate::auth::AuthManager::new(auth_path));
        state
    }

    #[tokio::test]
    async fn downloads_require_view_permission_without_exposing_files() {
        let root = TestDirectory::new();
        let state = export_state(&root.0, false).await;
        for response in [
            list(State(state.clone()), HeaderMap::new()).await,
            download(
                State(state),
                HeaderMap::new(),
                Path("groundstation_recording_1.db".into()),
            )
            .await,
        ] {
            assert!(matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ));
        }
        assert!(!root.0.join("groundstation_recording_1.db").exists());
    }

    #[tokio::test]
    async fn lists_sessions_and_downloads_a_finite_csv_snapshot() {
        let root = TestDirectory::new();
        let id = "groundstation_recording_2026-09-16_12-00-00_000.db";
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id))
            .await
            .unwrap();
        sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(10,'RAW')")
            .execute(&db)
            .await
            .unwrap();
        let state = export_state(&root.0, true).await;
        let response = list(State(state.clone()), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let entries: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(entries.as_array().unwrap().len(), 1);
        assert_eq!(entries[0]["id"], id);
        let response = download(State(state.clone()), HeaderMap::new(), Path(id.into())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/csv; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            response.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains(".csv")
        );
        // A live recording can keep writing, but the download must terminate.
        sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(20,'LATER')")
            .execute(&db)
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let csv = std::str::from_utf8(&body).unwrap();
        assert!(csv.starts_with(HEADER));
        assert!(csv.contains("RAW"));
        assert!(!csv.contains("LATER"));
        assert_eq!(
            download(State(state), HeaderMap::new(), Path("../auth.json".into()))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        db.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_and_missing_recordings_are_rejected() {
        let root = TestDirectory::new();
        let id = "groundstation_recording_1.db";
        std::os::unix::fs::symlink(root.0.join("auth.json"), root.0.join(id)).unwrap();
        assert_eq!(
            recording_path(&root.0, id).await.unwrap_err(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            recording_path(&root.0, "groundstation_recording_2.db")
                .await
                .unwrap_err(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn filenames_cannot_escape_recordings_or_select_other_databases() {
        assert!(valid_id(
            "groundstation_recording_2026-09-16_12-34-56_789.db"
        ));
        for id in [
            "../auth.db",
            "groundstation.db",
            "groundstation_recording_../auth.db",
            "groundstation_recording_.db",
            "groundstation_recording_1.db-wal",
        ] {
            assert!(!valid_id(id), "{id}");
        }
    }
    #[test]
    fn csv_escapes_quotes_newlines_and_spreadsheet_formulas() {
        assert_eq!(text_cell("a,\"b\"\nc"), "\"a,\"\"b\"\"\nc\"");
        assert_eq!(text_cell("=SUM(A1)"), "\"\x27=SUM(A1)\"");
        assert_eq!(text_cell("[0.001,-570,null]"), "\"[0.001,-570,null]\"");
    }
    #[tokio::test]
    async fn nullable_metadata_and_empty_recordings_export_cleanly() {
        let db = crate::telemetry_db::open_in_memory_telemetry_db()
            .await
            .unwrap();
        assert_eq!(csv_page(&db, 0, 0).await.unwrap(), (String::new(), 0));
        sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(42,'RAW')")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            csv_page(&db, 0, 1).await.unwrap().0,
            "1,42,,\"\",\"RAW\",\"\",\"\"\r\n"
        );
    }
    #[tokio::test]
    async fn exports_all_senders_and_types_without_decimation_or_calibration() {
        let db = crate::telemetry_db::open_in_memory_telemetry_db()
            .await
            .unwrap();
        for i in 1..=515i64 {
            sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(?,?,?,?,?,?)")
                .bind(i).bind(if i == 1 { None } else { Some(i*10) }).bind(if i%2 == 0 { "DAQ" } else { "FC" })
                .bind(if i%2 == 0 { "KG1000" } else { "BAROMETER" }).bind("[0.001,-570,null]").bind("[1,2,3]").execute(&db).await.unwrap();
        }
        let (first, id) = csv_page(&db, 0, 514).await.unwrap();
        assert_eq!(id, 512);
        assert_eq!(first.lines().count(), 512);
        assert!(first.starts_with("1,1,,\"FC\",\"BAROMETER\",\"[0.001,-570,null]\""));
        let (last, id) = csv_page(&db, id, 514).await.unwrap();
        assert_eq!(id, 514);
        assert_eq!(last.lines().count(), 2);
        assert!(csv_page(&db, id, 514).await.unwrap().0.is_empty());
    }
}
