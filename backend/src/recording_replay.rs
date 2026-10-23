//! Bounded, read-only replay windows from a selected recording. Never live caches.
use crate::{auth::Permission, recording_export, state::AppState};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::sync::Arc;
const PAGE: i64 = 512;
const MAX_WINDOW_MS: i64 = 30_000;

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub upper_id: i64,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
}
#[derive(Debug, Deserialize)]
pub struct Window {
    pub upper_id: i64,
    pub after_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub types: String,
}
impl Window {
    fn types(&self) -> anyhow::Result<Vec<String>> {
        anyhow::ensure!(
            self.upper_id >= 0 && self.after_id >= 0 && self.after_id <= self.upper_id,
            "invalid replay cursor"
        );
        anyhow::ensure!(
            self.start_ms >= 0
                && self.end_ms >= self.start_ms
                && self
                    .end_ms
                    .checked_sub(self.start_ms)
                    .is_some_and(|n| n <= MAX_WINDOW_MS),
            "replay window must be at most30 seconds"
        );
        anyhow::ensure!(self.types.len() <= 8192, "replay type filter too large");
        let types: Vec<String> = serde_json::from_str(&self.types)?;
        anyhow::ensure!(
            !types.is_empty()
                && types.len() <= 32
                && types.iter().all(|v| !v.is_empty() && v.len() <= 128),
            "invalid replay type filter"
        );
        Ok(types)
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Sample {
    pub id: i64,
    pub timestamp_ms: i64,
    pub source_timestamp_ms: Option<i64>,
    pub sender_id: String,
    pub data_type: String,
    pub values: Vec<Option<f32>>,
    pub phase: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub upper_id: i64,
    pub next_id: i64,
    pub has_more: bool,
    pub rows: Vec<Sample>,
    pub seeds: Vec<Sample>,
}
async fn open(state: &AppState, id: &str) -> Result<SqlitePool, StatusCode> {
    let path = recording_export::recording_path(&recording_export::directory(state), id).await?;
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .read_only(true)
                .create_if_missing(false),
        )
        .await
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)
}
async fn snapshot_from(db: &SqlitePool) -> anyhow::Result<Snapshot> {
    let row=sqlx::query("SELECT COALESCE(MAX(id),0) AS upper_id, MIN(timestamp_ms) AS start_ms, MAX(timestamp_ms) AS end_ms FROM telemetry").fetch_one(db).await?;
    Ok(Snapshot {
        upper_id: row.try_get("upper_id")?,
        start_ms: row.try_get("start_ms")?,
        end_ms: row.try_get("end_ms")?,
    })
}
async fn page_from(db: &SqlitePool, window: &Window) -> anyhow::Result<Page> {
    let types = window.types()?;
    let rows=sqlx::query("SELECT id,timestamp_ms,source_timestamp_ms,substr(COALESCE(sender_id,''),1,257) AS sender_id,data_type,substr(COALESCE(values_json,'[]'),1,32769) AS values_json FROM telemetry WHERE id > ? AND id <= ? AND timestamp_ms >= ? AND timestamp_ms <= ? AND data_type IN (SELECT value FROM json_each(?)) ORDER BY id LIMIT ?")
        .bind(window.after_id).bind(window.upper_id).bind(window.start_ms).bind(window.end_ms).bind(serde_json::to_string(&types)?).bind(PAGE+1).fetch_all(db).await?;
    let has_more = rows.len() > PAGE as usize;
    let mut samples = Vec::new();
    let mut next_id = window.after_id;
    for row in rows.into_iter().take(PAGE as usize) {
        let sample = decode(row)?;
        next_id = sample.id;
        samples.push(sample);
    }
    let mut seeds = Vec::new();
    if window.after_id == 0 {
        let rows = sqlx::query("SELECT id,timestamp_ms,source_timestamp_ms,substr(COALESCE(sender_id,''),1,257) AS sender_id,data_type,substr(COALESCE(values_json,'[]'),1,32769) AS values_json FROM (SELECT *,ROW_NUMBER() OVER(PARTITION BY data_type,sender_id ORDER BY timestamp_ms DESC,id DESC) AS rank FROM telemetry WHERE id <= ? AND timestamp_ms < ? AND data_type IN (SELECT value FROM json_each(?))) WHERE rank=1 LIMIT 257")
            .bind(window.upper_id).bind(window.start_ms).bind(serde_json::to_string(&types)?).fetch_all(db).await?;
        anyhow::ensure!(rows.len() <= 256, "too many replay source series");
        for row in rows {
            seeds.push(decode(row)?);
        }
    }

    Ok(Page {
        upper_id: window.upper_id,
        next_id,
        has_more,
        rows: samples,
        seeds,
    })
}
fn decode(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<Sample> {
    let raw: String = row.try_get("values_json")?;
    let sender: String = row.try_get("sender_id")?;
    let data_type: String = row.try_get("data_type")?;
    anyhow::ensure!(
        raw.len() <= 32768 && sender.len() <= 256,
        "oversized recorded replay sample"
    );
    let values: Vec<Option<f32>> = serde_json::from_str(&raw)?;
    anyhow::ensure!(
        values.len() <= 1024 && values.iter().flatten().all(|v| v.is_finite()),
        "invalid recorded replay values"
    );
    let phase = if data_type == "FLIGHT_STATE" && crate::types::canonical_sender_id(&sender) == "FC"
    {
        values
            .first()
            .copied()
            .flatten()
            .filter(|v| *v >= 0. && *v <= 255. && v.fract() == 0.)
            .and_then(|v| crate::types::u8_to_flight_state(v as u8))
            .map(|v| format!("{v:?}"))
    } else {
        None
    };
    Ok(Sample {
        id: row.try_get("id")?,
        timestamp_ms: row.try_get("timestamp_ms")?,
        source_timestamp_ms: row.try_get("source_timestamp_ms")?,
        sender_id: sender,
        data_type,
        values,
        phase,
    })
}

fn json<T: Serialize>(value: T) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
pub async fn snapshot(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    let db = match open(&state, &id).await {
        Ok(db) => db,
        Err(code) => return code.into_response(),
    };
    match snapshot_from(&db).await {
        Ok(value) => json(value),
        Err(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Recording has no readable telemetry table",
        )
            .into_response(),
    }
}
pub async fn window(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(window): Query<Window>,
) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    if let Err(e) = window.types() {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    let db = match open(&state, &id).await {
        Ok(db) => db,
        Err(code) => return code.into_response(),
    };
    match page_from(&db, &window).await {
        Ok(value) => json(value),
        Err(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Recording contains unreadable replay samples",
        )
            .into_response(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn replay_http_routes_authorize_and_read_only_the_selected_recording() {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;
        let root = recording_export::tests::TestDirectory::new();
        let id = "groundstation_recording_1.db";
        let path = format!("/api/recordings/{id}/replay");
        let denied = recording_export::routes()
            .with_state(recording_export::tests::export_state(&root.0, false).await);
        let response = denied
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
        assert!(!root.0.join(id).exists());
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id))
            .await
            .unwrap();
        insert(&db, 1000, "ATTITUDE", "[1,2,3]").await;
        let app = recording_export::routes()
            .with_state(recording_export::tests::export_state(&root.0, true).await);
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let saved: Snapshot =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        insert(&db, 1000, "ATTITUDE", "[99,99,99]").await;
        let url = format!(
            "{path}/window?upper_id={}&after_id=0&start_ms=0&end_ms=2000&types=%5B%22ATTITUDE%22%5D",
            saved.upper_id
        );
        let response = app
            .clone()
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let page: Page =
            serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].values[0], Some(1.));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/recordings/auth.json/replay")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        db.close().await;
    }
    async fn db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::telemetry_db::ensure_telemetry_schema(&db)
            .await
            .unwrap();
        db
    }
    async fn insert(db: &SqlitePool, time: i64, kind: &str, value: &str) {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json) VALUES (?,?, 'FC',?,?)").bind(time).bind(time-10).bind(kind).bind(value).execute(db).await.unwrap();
    }
    fn query(upper: i64) -> Window {
        Window {
            upper_id: upper,
            after_id: 0,
            start_ms: 0,
            end_ms: 30000,
            types: "[\"ATTITUDE\"]".into(),
        }
    }
    #[tokio::test]
    async fn active_recording_append_cannot_enter_frozen_snapshot() {
        let db = db().await;
        insert(&db, 1000, "ATTITUDE", "[1,2,3]").await;
        let saved = snapshot_from(&db).await.unwrap();
        insert(&db, 1000, "ATTITUDE", "[9,9,9]").await;
        let page = page_from(&db, &query(saved.upper_id)).await.unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].values[0], Some(1.));
        assert!(!page.has_more);
    }
    #[tokio::test]
    async fn filtered_cursor_pages_preserve_saved_rows_across_clock_reversal() {
        let db = db().await;
        for n in 0..515 {
            insert(&db, 2000 - n, "ATTITUDE", "[1,null,3]").await;
            insert(&db, 2000 - n, "IGNORED", "[99]").await;
        }
        let saved = snapshot_from(&db).await.unwrap();
        let mut q = query(saved.upper_id);
        let first = page_from(&db, &q).await.unwrap();
        assert_eq!(first.rows.len(), 512);
        assert!(first.has_more);
        q.after_id = first.next_id;
        let last = page_from(&db, &q).await.unwrap();
        assert_eq!(last.rows.len(), 3);
        assert!(!last.has_more);
        assert!(last.rows[0].id > first.rows.last().unwrap().id);
        assert!(first.rows[0].timestamp_ms > last.rows[0].timestamp_ms);
        assert_eq!(last.rows[0].values[1], None);
    }
    #[test]
    fn invalid_or_unbounded_requests_are_rejected() {
        for q in [
            Window {
                upper_id: -1,
                ..query(1)
            },
            Window {
                after_id: 2,
                ..query(1)
            },
            Window {
                end_ms: 30001,
                ..query(1)
            },
            Window {
                types: "[]".into(),
                ..query(1)
            },
            Window {
                types: "not json".into(),
                ..query(1)
            },
        ] {
            assert!(q.types().is_err());
        }
    }
    #[tokio::test]
    async fn corrupt_values_fail_without_replacing_them_with_live_or_zero_data() {
        let db = db().await;
        insert(&db, 1000, "ATTITUDE", "[1e300]").await;
        assert!(page_from(&db, &query(1)).await.is_err());
    }
    #[tokio::test]
    async fn empty_recording_has_no_invented_time_or_pose() {
        let db = db().await;
        let saved = snapshot_from(&db).await.unwrap();
        assert_eq!(saved.upper_id, 0);
        assert!(saved.start_ms.is_none());
        assert!(page_from(&db, &query(0)).await.unwrap().rows.is_empty());
    }
    #[tokio::test]
    async fn window_seeds_last_saved_state_per_source_without_reading_new_live_rows() {
        let db = db().await;
        insert(&db, 1000, "ATTITUDE", "[1,2,3]").await;
        insert(&db, 2000, "ATTITUDE", "[4,5,6]").await;
        let saved = snapshot_from(&db).await.unwrap();
        insert(&db, 2500, "ATTITUDE", "[99,99,99]").await;
        let q = Window {
            start_ms: 3000,
            end_ms: 4000,
            ..query(saved.upper_id)
        };
        let page = page_from(&db, &q).await.unwrap();
        assert!(page.rows.is_empty());
        assert_eq!(page.seeds.len(), 1);
        assert_eq!(page.seeds[0].values[0], Some(4.));
        assert_eq!(page.seeds[0].timestamp_ms, 2000);
    }
}
