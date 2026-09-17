//! Scan local recording contents, not filename dates (the Pi may boot without RTC).
use crate::{
    auth::Permission,
    recording_export::{directory, recording_path, text_cell, valid_id},
    state::AppState,
};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Deserialize)]
pub struct Range {
    pub start_ms: i64,
    pub end_ms: i64,
}
impl Range {
    fn valid(self) -> bool {
        self.start_ms >= 0 && self.end_ms > self.start_ms
    }
}
struct Snapshot {
    path: PathBuf,
    id: String,
    upper: i64,
}
async fn open(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .read_only(true)
                .create_if_missing(false),
        )
        .await
}
async fn snapshots(root: &Path, range: Range) -> anyhow::Result<Vec<Snapshot>> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut result = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_id(&id) {
            continue;
        }
        let meta = entry.file_type().await?;
        if !meta.is_file() || meta.is_symlink() {
            continue;
        }
        let path = recording_path(root, &id)
            .await
            .map_err(|e| anyhow::anyhow!("Recording changed during export: {id}: {e}"))?;
        let db = open(&path).await?;
        let upper: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id),0) FROM telemetry WHERE timestamp_ms >= ? AND timestamp_ms < ?")
            .bind(range.start_ms).bind(range.end_ms).fetch_one(&db).await?;
        db.close().await;
        if upper > 0 {
            result.push(Snapshot { path, id, upper });
        }
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}
async fn page(
    db: &SqlitePool,
    snapshot: &Snapshot,
    range: Range,
    last: i64,
) -> Result<(String, i64), sqlx::Error> {
    let rows = sqlx::query("SELECT id,timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json FROM telemetry WHERE id > ? AND id <= ? AND timestamp_ms >= ? AND timestamp_ms < ? ORDER BY id LIMIT 512")
        .bind(last).bind(snapshot.upper).bind(range.start_ms).bind(range.end_ms).fetch_all(db).await?;
    let mut csv = String::new();
    let mut next = last;
    for row in rows {
        next = row.try_get("id")?;
        let received: i64 = row.try_get("timestamp_ms")?;
        let source: Option<i64> = row.try_get("source_timestamp_ms")?;
        let sender: Option<String> = row.try_get("sender_id")?;
        let ty: String = row.try_get("data_type")?;
        let values: Option<String> = row.try_get("values_json")?;
        let payload: Option<String> = row.try_get("payload_json")?;
        csv.push_str(&format!(
            "{},{next},{received},{},{},{},{},{}\r\n",
            text_cell(&snapshot.id),
            source.map(|s| s.to_string()).unwrap_or_default(),
            text_cell(sender.as_deref().unwrap_or("")),
            text_cell(&ty),
            text_cell(values.as_deref().unwrap_or("")),
            text_cell(payload.as_deref().unwrap_or(""))
        ));
    }
    Ok((csv, next))
}
pub async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(range): Query<Range>,
) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    if !range.valid() {
        return (
            StatusCode::BAD_REQUEST,
            "Choose an end time later than the start time.",
        )
            .into_response();
    }
    let files = match snapshots(&directory(&state), range).await {
        Ok(v) if v.is_empty() => return (StatusCode::NOT_FOUND, "No recorded telemetry matches this time range. Check the GroundStation clock or download an individual session.").into_response(),
        Ok(v) => v,
        Err(e) => { log::error!("Date-range export: {e}"); return (StatusCode::INTERNAL_SERVER_ERROR, "Could not read all local recordings; no partial export was started.").into_response(); }
    };
    let stream = futures::stream::try_unfold(
        (files, 0usize, None::<SqlitePool>, 0i64, false),
        move |(files, mut index, mut db, mut last, started)| async move {
            if !started {
                return Ok::<_,std::io::Error>(Some(("recording,id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json\r\n".into(),(files,index,db,last,true))));
            }
            while index < files.len() {
                if db.is_none() {
                    db = Some(
                        open(&files[index].path)
                            .await
                            .map_err(std::io::Error::other)?,
                    );
                }
                let (chunk, next) = page(db.as_ref().unwrap(), &files[index], range, last)
                    .await
                    .map_err(std::io::Error::other)?;
                if next != last {
                    return Ok(Some((chunk, (files, index, db, next, true))));
                }
                db.take().unwrap().close().await;
                index += 1;
                last = 0;
            }
            Ok(None)
        },
    );
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"telemetry-{}-{}.csv\"",
                    range.start_ms, range.end_ms
                ),
            ),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn finds_rows_across_files_not_filename_dates_and_bounds_active_recordings() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        for id in [
            "groundstation_recording_1970-01-01.db",
            "groundstation_recording_2099-01-01.db",
        ] {
            let db = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(root.0.join(id))
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            crate::telemetry_db::ensure_telemetry_schema(&db)
                .await
                .unwrap();
            for _ in 0..515 {
                sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(150,'RAW')")
                    .execute(&db)
                    .await
                    .unwrap();
            }
            db.close().await;
        }
        std::fs::write(root.0.join("unrelated.db"), "not a database").unwrap();
        let range = Range {
            start_ms: 100,
            end_ms: 200,
        };
        let files = snapshots(&root.0, range).await.unwrap();
        assert_eq!(files.len(), 2);
        let writable = SqlitePoolOptions::new()
            .connect_with(SqliteConnectOptions::new().filename(&files[0].path))
            .await
            .unwrap();
        sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(150,'APPENDED')")
            .execute(&writable)
            .await
            .unwrap();
        writable.close().await;
        for file in &files {
            let db = open(&file.path).await.unwrap();
            let (first, last) = page(&db, file, range, 0).await.unwrap();
            let (second, last) = page(&db, file, range, last).await.unwrap();
            assert_eq!(first.lines().count(), 512);
            assert_eq!(second.lines().count(), 3);
            assert!(!second.contains("APPENDED"));
            assert!(page(&db, file, range, last).await.unwrap().0.is_empty());
            db.close().await;
        }
        assert!(
            snapshots(
                &root.0,
                Range {
                    start_ms: 200,
                    end_ms: 300
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
        let state = export_state(&root.0, false).await;
        assert_eq!(
            download(State(state), HeaderMap::new(), Query(range))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let state = export_state(&root.0, true).await;
        let response = download(State(state.clone()), HeaderMap::new(), Query(range)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 10_000_000)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap().lines().count(),
            1032
        );
        std::fs::write(root.0.join("groundstation_recording_123.db"), "corrupt").unwrap();
        assert_eq!(
            download(State(state), HeaderMap::new(), Query(range))
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    #[tokio::test]
    async fn range_is_half_open_and_keeps_all_duplicate_samples() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::telemetry_db::ensure_telemetry_schema(&db)
            .await
            .unwrap();
        for time in [99, 100, 100, 199, 200] {
            sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(?,'RAW')")
                .bind(time)
                .execute(&db)
                .await
                .unwrap();
        }
        let s = Snapshot {
            path: PathBuf::new(),
            id: "recording".into(),
            upper: 5,
        };
        let (csv, next) = page(
            &db,
            &s,
            Range {
                start_ms: 100,
                end_ms: 200,
            },
            0,
        )
        .await
        .unwrap();
        assert_eq!(csv.lines().count(), 3);
        assert_eq!(next, 4);
        assert!(
            !Range {
                start_ms: 200,
                end_ms: 100
            }
            .valid()
        );
        assert!(
            page(
                &db,
                &s,
                Range {
                    start_ms: 100,
                    end_ms: 200
                },
                next
            )
            .await
            .unwrap()
            .0
            .is_empty()
        );
    }
}
