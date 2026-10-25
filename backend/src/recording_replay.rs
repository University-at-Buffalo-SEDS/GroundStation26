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
#[cfg(all(test, any(feature = "hitl_mode", feature = "firmware_sim")))]
pub(crate) async fn test_page_from(db: &SqlitePool, window: &Window) -> anyhow::Result<Page> {
    page_from(db, window).await
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
const MAX_TRAJECTORY_POINTS: usize = 8192;
const MAX_TRAJECTORY_ROWS: i64 = 2_000_000;
const TRAJECTORY_GAP_MS: i64 = 5_000;

#[derive(Debug, Deserialize)]
pub struct TrajectoryQuery {
    pub upper_id: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrajectoryPoint {
    pub timestamp_ms: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: f64,
    pub break_before: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Trajectory {
    pub upper_id: i64,
    pub source_sender_id: Option<String>,
    pub points: Vec<TrajectoryPoint>,
    pub truncated: bool,
    pub sampled: bool,
}
#[derive(Clone)]
struct SegmentPoint {
    point: TrajectoryPoint,
    segment: u64,
}
#[derive(Default)]
struct TrajectorySampler {
    points: Vec<SegmentPoint>,
    segment: u64,
    previous_ms: Option<i64>,
    invalid_since_previous: bool,
    sampled: bool,
    truncated: bool,
}
impl TrajectorySampler {
    fn push(&mut self, timestamp_ms: i64, raw: &str) {
        let Some(mut point) = saved_gps_point(timestamp_ms, raw) else {
            self.invalid_since_previous = true;
            return;
        };
        point.break_before = self.previous_ms.is_none_or(|previous| {
            self.invalid_since_previous || timestamp_ms.saturating_sub(previous) > TRAJECTORY_GAP_MS
        });
        if point.break_before {
            self.segment = self.segment.saturating_add(1);
        }
        self.previous_ms = Some(timestamp_ms);
        self.invalid_since_previous = false;
        self.points.push(SegmentPoint {
            point,
            segment: self.segment,
        });
        if self.points.len() > MAX_TRAJECTORY_POINTS * 2 {
            self.compact(MAX_TRAJECTORY_POINTS);
        }
    }
    fn compact(&mut self, budget: usize) {
        use std::collections::BTreeSet;
        let len = self.points.len();
        if len <= budget {
            return;
        }
        self.sampled = true;
        let min = (0..len)
            .min_by(|&a, &b| {
                self.points[a]
                    .point
                    .altitude_m
                    .total_cmp(&self.points[b].point.altitude_m)
            })
            .unwrap();
        let max = (0..len)
            .max_by(|&a, &b| {
                self.points[a]
                    .point
                    .altitude_m
                    .total_cmp(&self.points[b].point.altitude_m)
            })
            .unwrap();
        let required = BTreeSet::from([0, len - 1, min, max]);
        let mut boundaries = required.clone();
        for i in 1..len {
            if self.points[i].segment != self.points[i - 1].segment {
                boundaries.insert(i - 1);
                boundaries.insert(i);
            }
        }
        let mut chosen = if boundaries.len() <= budget {
            boundaries
        } else {
            // Too many real gaps to represent every boundary. Omit whole
            // portions explicitly rather than joining across a missing segment.
            self.truncated = true;
            required
        };
        let optional: Vec<_> = (0..len).filter(|i| !chosen.contains(i)).collect();
        let slots = budget.saturating_sub(chosen.len());
        for i in 0..slots {
            chosen.insert(optional[i * optional.len() / slots]);
        }
        let mut kept = Vec::with_capacity(chosen.len());
        for (i, mut value) in self.points.drain(..).enumerate() {
            if chosen.contains(&i) {
                value.point.break_before = kept
                    .last()
                    .is_none_or(|previous: &SegmentPoint| previous.segment != value.segment);
                kept.push(value);
            }
        }
        self.points = kept;
    }
}
fn saved_gps_point(timestamp_ms: i64, raw: &str) -> Option<TrajectoryPoint> {
    if timestamp_ms < 0 || raw.len() > 32768 {
        return None;
    }
    let values: Vec<Option<f64>> = serde_json::from_str(raw).ok()?;
    if values.len() > 1024 {
        return None;
    }
    let latitude = *values.first()?.as_ref()?;
    let longitude = *values.get(1)?.as_ref()?;
    let altitude_m = *values.get(2)?.as_ref()?;
    if !latitude.is_finite()
        || !(-90.0..=90.0).contains(&latitude)
        || !longitude.is_finite()
        || !(-180.0..=180.0).contains(&longitude)
        || !altitude_m.is_finite()
        || !(-20_000.0..=10_000_000.0).contains(&altitude_m)
    {
        return None;
    }
    Some(TrajectoryPoint {
        timestamp_ms,
        latitude,
        longitude,
        altitude_m,
        break_before: false,
    })
}
async fn trajectory_from(db: &SqlitePool, upper_id: i64) -> anyhow::Result<Trajectory> {
    use futures::TryStreamExt;
    anyhow::ensure!(upper_id >= 0, "invalid replay snapshot");
    let mut radio = TrajectorySampler::default();
    let mut fc = TrajectorySampler::default();
    let mut rows = sqlx::query("SELECT timestamp_ms,sender_id,substr(COALESCE(values_json,'[]'),1,32769) AS values_json FROM telemetry WHERE id <= ? AND sender_id IN ('RF','FC') AND data_type IN ('GPS_DATA','GPS','ROCKET_GPS') ORDER BY timestamp_ms,id LIMIT ?")
        .bind(upper_id).bind(MAX_TRAJECTORY_ROWS + 1).fetch(db);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut count = 0;
    let mut truncated = false;
    loop {
        let row = match tokio::time::timeout_at(deadline, rows.try_next()).await {
            Ok(result) => match result? {
                Some(row) => row,
                None => break,
            },
            Err(_) => {
                truncated = true;
                break;
            }
        };
        count += 1;
        if count > MAX_TRAJECTORY_ROWS {
            truncated = true;
            break;
        }
        let sender: String = row.try_get("sender_id")?;
        let sampler = if sender == "RF" { &mut radio } else { &mut fc };
        sampler.push(
            row.try_get("timestamp_ms")?,
            &row.try_get::<String, _>("values_json")?,
        );
    }
    // Match antenna GPS's rocket source whitelist, preferring RF receiver fixes.
    // A single frozen source prevents mixing FC-relative and RF-MSL altitudes.
    let (mut selected, source) = if !radio.points.is_empty() {
        (radio, Some("RF"))
    } else if !fc.points.is_empty() {
        (fc, Some("FC"))
    } else {
        (radio, None)
    };
    selected.compact(MAX_TRAJECTORY_POINTS);
    Ok(Trajectory {
        upper_id,
        source_sender_id: source.map(str::to_owned),
        points: selected
            .points
            .into_iter()
            .map(|value| value.point)
            .collect(),
        truncated: truncated || selected.truncated,
        sampled: selected.sampled,
    })
}

pub async fn trajectory(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<TrajectoryQuery>,
) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    if query.upper_id < 0 {
        return (StatusCode::BAD_REQUEST, "Invalid replay snapshot").into_response();
    }
    let db = match open(&state, &id).await {
        Ok(db) => db,
        Err(code) => return code.into_response(),
    };
    match snapshot_from(&db).await {
        Ok(snapshot) if query.upper_id <= snapshot.upper_id => {}
        Ok(_) => {
            return (StatusCode::BAD_REQUEST, "Replay snapshot exceeds recording").into_response();
        }
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Recording has no readable telemetry table",
            )
                .into_response();
        }
    }
    match trajectory_from(&db, query.upper_id).await {
        Ok(value) => json(value),
        Err(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Recording contains unreadable trajectory data",
        )
            .into_response(),
    }
}

#[derive(Serialize)]
pub(crate) struct FlightSnapshot {
    pub(crate) upper_id: i64,
    pub(crate) start_ms: Option<i64>,
    pub(crate) end_ms: Option<i64>,
    flight_detected: bool,
}
async fn flight_snapshot_from(db: &SqlitePool) -> anyhow::Result<FlightSnapshot> {
    let snapshot = snapshot_from(db).await?;
    // Repeated phase samples are not repeated launches. Select the latest
    // actual saved transition into Ascent, with Launch as a pre-ascent fallback.
    let row=sqlx::query("WITH flight AS (SELECT id,timestamp_ms,CASE WHEN json_valid(values_json) THEN json_extract(values_json,'$[0]') END AS phase FROM telemetry WHERE id <= ? AND sender_id='FC' AND data_type='FLIGHT_STATE'), edges AS (SELECT *,LAG(phase) OVER(ORDER BY id) AS previous FROM flight) SELECT id,timestamp_ms FROM edges WHERE phase IN (7,8) AND (previous IS NULL OR previous != phase) ORDER BY id DESC LIMIT 1")
        .bind(snapshot.upper_id).fetch_optional(db).await?;
    let first_id = row
        .as_ref()
        .map(|row| row.try_get::<i64, _>("id"))
        .transpose()?;
    let start = row
        .map(|row| row.try_get::<i64, _>("timestamp_ms"))
        .transpose()?;
    let end = if let Some(start) = start {
        let prior: Option<i64> =
            sqlx::query_scalar("SELECT MAX(timestamp_ms) FROM telemetry WHERE id<?")
                .bind(first_id.unwrap())
                .fetch_one(db)
                .await?;
        anyhow::ensure!(
            prior.is_none_or(|time| time <= start),
            "Saved clock epochs overlap the selected flight"
        );
        let row=sqlx::query("SELECT MIN(timestamp_ms) AS landed FROM telemetry WHERE id<=? AND id>=? AND timestamp_ms>=? AND sender_id='FC' AND data_type='FLIGHT_STATE' AND CASE WHEN json_valid(values_json) THEN json_extract(values_json,'$[0]') IN (13,14) ELSE 0 END")
            .bind(snapshot.upper_id).bind(first_id.unwrap()).bind(start).fetch_one(db).await?;
        row.try_get::<Option<i64>, _>("landed")?
            .map(|end| end.saturating_add(5000).min(snapshot.end_ms.unwrap_or(end)))
            .or(snapshot.end_ms)
    } else {
        snapshot.end_ms
    };
    Ok(FlightSnapshot {
        upper_id: snapshot.upper_id,
        start_ms: start.or(snapshot.start_ms),
        end_ms: end,
        flight_detected: start.is_some(),
    })
}
pub async fn flight_snapshot(
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
    match flight_snapshot_from(&db).await {
        Ok(value) => json(value),
        Err(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Recording contains unreadable flight bounds",
        )
            .into_response(),
    }
}

/// Internal read-only entry points. Media callers authorize a narrow selected
/// replay capability before invoking these; recording IDs still use the same
/// guarded directory lookup as the authenticated recording APIs.
pub(crate) async fn saved_flight_snapshot(
    state: &AppState,
    id: &str,
) -> anyhow::Result<FlightSnapshot> {
    let db = open(state, id)
        .await
        .map_err(|status| anyhow::anyhow!("Recording unavailable ({status})"))?;
    flight_snapshot_from(&db).await
}
pub(crate) async fn saved_snapshot(state: &AppState, id: &str) -> anyhow::Result<Snapshot> {
    let db = open(state, id)
        .await
        .map_err(|status| anyhow::anyhow!("Recording unavailable ({status})"))?;
    snapshot_from(&db).await
}
pub(crate) async fn saved_window(
    state: &AppState,
    id: &str,
    upper_id: i64,
    after_id: i64,
    start_ms: i64,
    end_ms: i64,
    types: Vec<String>,
) -> anyhow::Result<Page> {
    let window = Window {
        upper_id,
        after_id,
        start_ms,
        end_ms,
        types: serde_json::to_string(&types)?,
    };
    window.types()?;
    let db = open(state, id)
        .await
        .map_err(|status| anyhow::anyhow!("Recording unavailable ({status})"))?;
    page_from(&db, &window).await
}
pub(crate) async fn saved_trajectory(
    state: &AppState,
    id: &str,
    upper_id: i64,
) -> anyhow::Result<Trajectory> {
    let db = open(state, id)
        .await
        .map_err(|status| anyhow::anyhow!("Recording unavailable ({status})"))?;
    trajectory_from(&db, upper_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn flight_selection_skips_fill_and_repeated_phase_samples_and_stops_after_landing() {
        let db = db().await;
        insert(&db, 1000, "ROLL", "[1]").await;
        insert(&db, 5000, "FLIGHT_STATE", "[7]").await;
        insert(&db, 6000, "FLIGHT_STATE", "[8]").await;
        insert(&db, 7000, "FLIGHT_STATE", "[8]").await;
        insert(&db, 10000, "FLIGHT_STATE", "[13]").await;
        insert(&db, 30000, "ROLL", "[99]").await;
        let bounds = flight_snapshot_from(&db).await.unwrap();
        assert!(bounds.flight_detected);
        assert_eq!(bounds.start_ms, Some(6000));
        assert_eq!(bounds.end_ms, Some(15000));
        insert(&db, 31000, "FLIGHT_STATE", "[7]").await;
        insert(&db, 32000, "FLIGHT_STATE", "[8]").await;
        let next = flight_snapshot_from(&db).await.unwrap();
        assert_eq!(next.start_ms, Some(32000));
        assert_eq!(next.end_ms, Some(32000));
        insert(&db, 1000, "FLIGHT_STATE", "[1]").await;
        insert(&db, 2000, "FLIGHT_STATE", "[8]").await;
        assert!(
            flight_snapshot_from(&db).await.is_err(),
            "overlappingwallclockepochs mustnotmixdifferentflights"
        );
    }
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
    #[test]
    fn trajectory_compaction_preserves_extrema_ends_and_true_gaps() {
        let mut sampler = TrajectorySampler::default();
        for n in 0..40_000_i64 {
            let time = n * 10 + if n >= 20_000 { 10_000 } else { 0 };
            let altitude = match n {
                12_345 => 99_999,
                23_456 => -20_000,
                _ => 100,
            };
            sampler.push(time, &format!("[43,-78,{altitude}]"));
            assert!(sampler.points.len() <= MAX_TRAJECTORY_POINTS * 2);
        }
        sampler.compact(MAX_TRAJECTORY_POINTS);
        let points: Vec<_> = sampler.points.iter().map(|p| &p.point).collect();
        assert!(points.len() <= MAX_TRAJECTORY_POINTS);
        assert_eq!(points.first().unwrap().timestamp_ms, 0);
        assert_eq!(points.last().unwrap().timestamp_ms, 409_990);
        assert!(points.iter().any(|p| p.altitude_m == 99_999.0));
        assert!(points.iter().any(|p| p.altitude_m == -20_000.0));
        assert!(points.iter().any(|p| p.timestamp_ms == 199_990));
        assert!(
            points
                .iter()
                .any(|p| p.timestamp_ms == 210_000 && p.break_before)
        );
        assert_eq!(points.iter().filter(|p| p.break_before).count(), 2);
        assert!(sampler.sampled);
        assert!(!sampler.truncated);
    }
    #[test]
    fn trajectory_decimation_does_not_invent_time_gaps() {
        let mut sampler = TrajectorySampler::default();
        for n in 0..1000 {
            sampler.push(n * 1000, "[0,0,100]");
        }
        sampler.compact(32);
        assert_eq!(sampler.points.len(), 32);
        assert!(
            sampler
                .points
                .windows(2)
                .any(
                    |pair| pair[1].point.timestamp_ms - pair[0].point.timestamp_ms
                        > TRAJECTORY_GAP_MS
                )
        );
        assert!(sampler.points[0].point.break_before);
        assert!(sampler.points[1..].iter().all(|p| !p.point.break_before));
        assert!(!sampler.truncated);
    }
    #[test]
    fn invalid_gps_breaks_saved_path_without_substituting_zero() {
        let mut sampler = TrajectorySampler::default();
        sampler.push(0, "[43,-78,100]");
        sampler.push(1000, "[null,-78,100]");
        sampler.push(2000, "[43,-78,101]");
        assert_eq!(sampler.points.len(), 2);
        assert!(sampler.points[1].point.break_before);
        for raw in [
            "[91,0,0]",
            "[0,181,0]",
            "[0,0,1e300]",
            "[0,0,null]",
            "[0,0]",
            "[\"43\",0,0]",
            "not json",
        ] {
            assert!(saved_gps_point(0, raw).is_none(), "{raw}");
        }
        assert!(saved_gps_point(-1, "[0,0,0]").is_none());
        assert!(saved_gps_point(0, "[0,0,0]").is_some());
    }
    #[test]
    fn pathological_gap_counts_truncate_without_drawing_false_connections() {
        let mut sampler = TrajectorySampler::default();
        for n in 0..1000 {
            sampler.push(n * 10_000, "[43,-78,100]");
        }
        sampler.compact(32);
        assert!(sampler.truncated);
        assert_eq!(sampler.points.len(), 32);
        assert_eq!(sampler.points.first().unwrap().point.timestamp_ms, 0);
        assert_eq!(sampler.points.last().unwrap().point.timestamp_ms, 9_990_000);
        assert!(sampler.points.iter().all(|p| p.point.break_before));
    }
    #[tokio::test]
    async fn saved_trajectory_uses_one_rocket_source_and_frozen_ids_in_time_order() {
        let db = db().await;
        for (sender, time, values) in [
            ("FC", 1000, "[43,-78,10]"),
            ("RRLS", 1000, "[44,-79,9999]"),
            ("GS", 1000, "[45,-80,9999]"),
            ("GB", 1000, "[46,-81,9999]"),
            ("RF", 3000, "[43,-78,300]"),
            ("RF", 2000, "[43,-78,200]"),
        ] {
            sqlx::query("INSERT INTO telemetry(timestamp_ms,sender_id,data_type,values_json) VALUES(?,?,'GPS_DATA',?)")
                .bind(time).bind(sender).bind(values).execute(&db).await.unwrap();
        }
        let upper = snapshot_from(&db).await.unwrap().upper_id;
        sqlx::query("INSERT INTO telemetry(timestamp_ms,sender_id,data_type,values_json) VALUES(2000,'RF','GPS_DATA','[43,-78,999]')").execute(&db).await.unwrap();
        let trajectory = trajectory_from(&db, upper).await.unwrap();
        assert_eq!(trajectory.upper_id, upper);
        assert_eq!(trajectory.source_sender_id.as_deref(), Some("RF"));
        assert_eq!(trajectory.points.len(), 2);
        assert_eq!(trajectory.points[0].timestamp_ms, 2000);
        assert_eq!(trajectory.points[0].altitude_m, 200.0);
        assert_eq!(trajectory.points[1].altitude_m, 300.0);
        assert!(!trajectory.sampled && !trajectory.truncated);
        let fc = trajectory_from(&db, 1).await.unwrap();
        assert_eq!(fc.source_sender_id.as_deref(), Some("FC"));
        assert_eq!(fc.points[0].altitude_m, 10.0);
        let empty = trajectory_from(&db, 0).await.unwrap();
        assert!(empty.points.is_empty());
        assert!(empty.source_sender_id.is_none());
    }
    #[tokio::test]
    async fn trajectory_http_authorizes_and_validates_frozen_recording() {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;
        let root = recording_export::tests::TestDirectory::new();
        let id = "groundstation_recording_2.db";
        let url = format!("/api/recordings/{id}/replay/trajectory?upper_id=0");
        let denied = recording_export::routes()
            .with_state(recording_export::tests::export_state(&root.0, false).await);
        let response = denied
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
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
        let app = recording_export::routes()
            .with_state(recording_export::tests::export_state(&root.0, true).await);
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let value: Trajectory =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(value.upper_id, 0);
        assert!(value.points.is_empty() && value.source_sender_id.is_none());
        for url in [
            format!("/api/recordings/{id}/replay/trajectory?upper_id=1"),
            format!("/api/recordings/{id}/replay/trajectory?upper_id=-1"),
            "/api/recordings/auth.json/replay/trajectory?upper_id=0".to_owned(),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        db.close().await;
    }
}
