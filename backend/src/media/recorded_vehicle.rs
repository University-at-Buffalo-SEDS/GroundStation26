//! Audience-only saved-flight scene. Tickets authorize only the currently
//! selected recording; query parameters cannot select a different database.
use super::*;
use crate::recording_replay;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedReplayPlan {
    pub recording_id: String,
    pub upper_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub position_ms: u64,
    pub anchor_server_ms: u64,
    pub speed: f64,
    pub paused: bool,
    pub loop_playback: bool,
}
impl RecordedReplayPlan {
    fn valid(&self) -> bool {
        !self.recording_id.is_empty()
            && self.recording_id.len() <= 256
            && (0..=9_007_199_254_740_991).contains(&self.upper_id)
            && self.start_ms >= 0
            && self.end_ms <= 8_640_000_000_000_000
            && self.end_ms >= self.start_ms
            && self.position_ms <= self.end_ms.saturating_sub(self.start_ms) as u64
            && self.speed.is_finite()
            && (0.25..=4.).contains(&self.speed)
    }
    pub(super) fn elapsed(&self, now: u64) -> u64 {
        let duration = self.end_ms.saturating_sub(self.start_ms) as u64;
        let elapsed = if self.paused {
            0.
        } else {
            now.saturating_sub(self.anchor_server_ms) as f64 * self.speed
        };
        let value = self
            .position_ms
            .saturating_add(elapsed.max(0.).min(u64::MAX as f64) as u64);
        if self.loop_playback && duration > 0 {
            value % duration
        } else {
            value.min(duration)
        }
    }
}

pub(super) async fn validate_plan(
    state: &MediaState,
    plan: &mut RecordedReplayPlan,
) -> ApiResult<()> {
    if !plan.valid() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Invalid recorded replay selection or playback controls",
        ));
    }
    let saved = recording_replay::saved_snapshot(&state.app, &plan.recording_id)
        .await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Selected recording is unavailable"))?;
    let (Some(first), Some(last)) = (saved.start_ms, saved.end_ms) else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Selected recording has no saved samples",
        ));
    };
    if plan.upper_id == 0 && plan.start_ms == 0 && plan.end_ms == 0 {
        plan.upper_id = saved.upper_id;
        plan.start_ms = first;
        plan.end_ms = last;
    }
    if plan.upper_id > saved.upper_id
        || plan.start_ms < first
        || plan.end_ms > last
        || !plan.valid()
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Recorded replay selection exceeds saved recording bounds",
        ));
    }
    if plan.anchor_server_ms == 0 {
        plan.anchor_server_ms = contract::program_now_ms();
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct SceneCache {
    track: Option<(String, i64, serde_json::Value)>,
}
#[derive(Deserialize)]
struct WindowQuery {
    recording_id: String,
    after_id: i64,
    start_ms: i64,
    end_ms: i64,
    upper_id: i64,
}

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route("/api/media-assets/recorded-vehicle", get(page))
        .route("/api/media-assets/recorded-vehicle/state", get(scene_state))
        .route("/api/media-assets/recorded-vehicle/window", get(window))
        .route(
            "/api/media-assets/recorded-vehicle/trajectory",
            get(trajectory),
        )
        .route("/assets/three/saved-replay-player.js", get(player_asset))
}
fn no_store(value: impl Serialize) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}
async fn selected(state: &MediaState) -> ApiResult<(RecordedReplayPlan, serde_json::Value)> {
    let presentation = contract::read_presentation(state).await?;
    let plan = presentation
        .broadcast
        .recorded_replay
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "No recorded flight replay selected"))?;
    if !plan.valid() {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Saved replay controls are invalid",
        ));
    }
    let config = serde_json::to_value(&presentation.vehicle).map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Replay configuration unavailable",
        )
    })?;
    // Window filters use saved configured bindings. The scene endpoint
    // separately normalizes model URLs into narrow media capabilities.
    Ok((plan, config))
}
async fn page(
    State(state): State<Arc<MediaState>>,
    Query(query): Query<contract::MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    contract::authorize_media(&state, &headers, &query, "program").await?;
    selected(&state).await?;
    let html = gs_browser_assets::asset_bytes("RECORDED_VEHICLE_HTML").ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            "Selected frontend has no recorded flight scene",
        )
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response())
}
async fn player_asset() -> ApiResult<Response> {
    let bytes = gs_browser_assets::asset_bytes("SAVED_REPLAY_PLAYER_JS")
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Replay player asset unavailable"))?;
    Ok(([(header::CONTENT_TYPE, "text/javascript")], bytes).into_response())
}
async fn scene_state(
    State(state): State<Arc<MediaState>>,
    Query(query): Query<contract::MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    contract::authorize_media(&state, &headers, &query, "program").await?;
    let (plan, _) = selected(&state).await?;
    let config = contract::recorded_vehicle_config(&state, &headers, &query).await?;
    let now = contract::program_now_ms();
    Ok(no_store(
        serde_json::json!({"replay_position_ms":plan.elapsed(now),"plan":plan,"config":config,"server_now_ms":now}),
    ))
}
fn requested_types(config: &serde_json::Value) -> Vec<String> {
    let mut types = BTreeSet::from([
        "GPS_DATA".to_owned(),
        "GPS".to_owned(),
        "ROCKET_GPS".to_owned(),
        "FLIGHT_STATE".to_owned(),
    ]);
    fn visit(value: &serde_json::Value, out: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::Object(values) => {
                if let Some(ty) = values.get("data_type").and_then(|v| v.as_str()) {
                    if !ty.is_empty() && ty.len() <= 128 {
                        out.insert(ty.to_owned());
                    }
                }
                for value in values.values() {
                    visit(value, out);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, out);
                }
            }
            _ => {}
        }
    }
    visit(config, &mut types);
    types.into_iter().collect()
}
async fn window(
    State(state): State<Arc<MediaState>>,
    Query(query): Query<contract::MediaQuery>,
    Query(window): Query<WindowQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    contract::authorize_media(&state, &headers, &query, "program").await?;
    let (plan, config) = selected(&state).await?;
    if window.recording_id != plan.recording_id
        || window.upper_id != plan.upper_id
        || window.start_ms < plan.start_ms
        || window.end_ms > plan.end_ms
    {
        return Err(error(
            StatusCode::CONFLICT,
            "Selected recorded replay changed; reload its snapshot",
        ));
    }
    let page = recording_replay::saved_window(
        &state.app,
        &plan.recording_id,
        plan.upper_id,
        window.after_id,
        window.start_ms,
        window.end_ms,
        requested_types(&config),
    )
    .await
    .map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "Invalid or unavailable saved replay window",
        )
    })?;
    let mut value = serde_json::to_value(page).map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Replay serialization failed",
        )
    })?;
    value["recording_id"] = serde_json::json!(plan.recording_id);
    Ok(no_store(value))
}
#[derive(Deserialize)]
struct TrajectoryQuery {
    recording_id: String,
    upper_id: i64,
}
async fn trajectory(
    State(state): State<Arc<MediaState>>,
    Query(query): Query<contract::MediaQuery>,
    Query(expected): Query<TrajectoryQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    contract::authorize_media(&state, &headers, &query, "program").await?;
    let (plan, _) = selected(&state).await?;
    if expected.recording_id != plan.recording_id || expected.upper_id != plan.upper_id {
        return Err(error(
            StatusCode::CONFLICT,
            "Selected recorded replay changed; reload its snapshot",
        ));
    }
    let mut cache = state.recorded_vehicle_cache.lock().await;
    if let Some((id, upper, value)) = &cache.track {
        if id == &plan.recording_id && *upper == plan.upper_id {
            return Ok(no_store(value.clone()));
        }
    }
    let value = recording_replay::saved_trajectory(&state.app, &plan.recording_id, plan.upper_id)
        .await
        .map_err(|_| {
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Selected saved trajectory is unavailable",
            )
        })?;
    let mut value = serde_json::to_value(value).map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Trajectory serialization failed",
        )
    })?;
    value["recording_id"] = serde_json::json!(plan.recording_id);
    cache.track = Some((plan.recording_id, plan.upper_id, value.clone()));
    Ok(no_store(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan() -> RecordedReplayPlan {
        RecordedReplayPlan {
            recording_id: "one.db".into(),
            upper_id: 10,
            start_ms: 1000,
            end_ms: 11000,
            position_ms: 2000,
            anchor_server_ms: 50000,
            speed: 1.,
            paused: false,
            loop_playback: false,
        }
    }

    async fn media_state(root: &std::path::Path, allow: bool) -> Arc<MediaState> {
        Arc::new(MediaState {
            app: crate::recording_export::tests::export_state(root, allow).await,
            client: reqwest::Client::new(),
            api_url: String::new(),
            webrtc_url: String::new(),
            hls_url: String::new(),
            relay_password: String::new(),
            models: root.to_path_buf(),
            recordings: root.to_path_buf(),
            model_write: Mutex::new(()),
            voice: Arc::new(crate::voice::VoiceHub::default()),
            presentation_write: Mutex::new(()),
            tickets: Mutex::new(Vec::new()),
            replays: Mutex::new(Default::default()),
            program_history: Mutex::new(Default::default()),
            hls_segments: Mutex::new(Default::default()),
            preview_sessions: Mutex::new(Vec::new()),
            publishers: Mutex::new(Vec::new()),
            recorded_vehicle_cache: Mutex::new(Default::default()),
        })
    }
    async fn insert(db: &sqlx::SqlitePool, time: i64, kind: &str, values: &str) {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,sender_id,data_type,values_json) VALUES (?,'FC',?,?)")
            .bind(time).bind(kind).bind(values).execute(db).await.unwrap();
    }
    #[tokio::test]
    async fn audience_capability_is_selected_snapshot_only_and_revocation_is_rechecked() {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;
        let root = crate::recording_export::tests::TestDirectory::new();
        let state = media_state(&root.0, true).await;
        let id = "groundstation_recording_1.db";
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id))
            .await
            .unwrap();
        insert(&db, 1000, "GPS_DATA", "[43,-78,300]").await;
        insert(&db, 2000, "GPS_DATA", "[43.001,-78,4000]").await;
        let mut plan = plan();
        plan.recording_id = id.into();
        plan.upper_id = 0;
        plan.start_ms = 0;
        plan.end_ms = 0;
        plan.position_ms = 0;
        plan.anchor_server_ms = 0;
        validate_plan(&state, &mut plan).await.unwrap();
        assert_eq!((plan.upper_id, plan.start_ms, plan.end_ms), (2, 1000, 2000));
        assert!(plan.anchor_server_ms > 0);
        let mut presentation: contract::Presentation =
            serde_json::from_value(serde_json::json!({})).unwrap();
        presentation.broadcast.recorded_replay = Some(plan.clone());
        tokio::fs::write(
            root.0.join("_presentation.json"),
            serde_json::to_vec(&presentation).unwrap(),
        )
        .await
        .unwrap();
        let ticket = contract::ticket(&state, &HeaderMap::new(), "program")
            .await
            .unwrap();
        let app = routes().with_state(state.clone());
        let request = |path: String| Request::builder().uri(path).body(Body::empty()).unwrap();
        let state_url = format!("/api/media-assets/recorded-vehicle/state?ticket={ticket}");
        let response = app
            .clone()
            .oneshot(request(state_url.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 100000).await.unwrap()).unwrap();
        assert_eq!(body["plan"]["recording_id"], id);
        let window_url = format!(
            "/api/media-assets/recorded-vehicle/window?ticket={ticket}&recording_id={id}&upper_id=2&after_id=0&start_ms=1000&end_ms=2000"
        );
        let response = app
            .clone()
            .oneshot(request(window_url.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 100000).await.unwrap()).unwrap();
        assert_eq!(body["rows"].as_array().unwrap().len(), 2);
        assert_eq!(
            app.clone()
                .oneshot(request(
                    window_url.replace(id, "groundstation_recording_2.db")
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            app.clone()
                .oneshot(request(window_url.replace("upper_id=2", "upper_id=3")))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            app.clone()
                .oneshot(request(window_url.replace("end_ms=2000", "end_ms=9000")))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let track_url = format!(
            "/api/media-assets/recorded-vehicle/trajectory?ticket={ticket}&recording_id={id}&upper_id=2"
        );
        let response = app
            .clone()
            .oneshot(request(track_url.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let before = to_bytes(response.into_body(), 100000).await.unwrap();
        insert(&db, 3000, "GPS_DATA", "[43.002,-78,9000]").await;
        let response = app.clone().oneshot(request(track_url)).await.unwrap();
        assert_eq!(
            to_bytes(response.into_body(), 100000).await.unwrap(),
            before,
            "newlyappendeddata cannotchangefrozenreplay"
        );
        let mut invalid = plan.clone();
        invalid.recording_id = "../outside.db".into();
        assert!(validate_plan(&state, &mut invalid).await.is_err());
        state.tickets.lock().await.clear();
        assert_eq!(
            app.clone()
                .oneshot(request(state_url))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let denied = routes().with_state(media_state(&root.0, false).await);
        assert!(matches!(
            denied
                .oneshot(request("/api/media-assets/recorded-vehicle/state".into()))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
        assert!(!root.0.join("outside.db").exists());
    }
    #[tokio::test]
    async fn uploaded_replay_models_receive_only_their_own_media_capability() {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;
        let root = crate::recording_export::tests::TestDirectory::new();
        let state = media_state(&root.0, true).await;
        std::fs::create_dir(root.0.join("stage-1")).unwrap();
        std::fs::write(
            root.0.join("stage-1/rocket.glb"),
            gs_browser_assets::VEHICLE_MODEL,
        )
        .unwrap();
        let id = "groundstation_recording_1.db";
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id))
            .await
            .unwrap();
        insert(&db, 1000, "GPS_DATA", "[43,-78,300]").await;
        insert(&db, 2000, "GPS_DATA", "[43,-78,4000]").await;
        let mut p = plan();
        p.recording_id = id.into();
        p.upper_id = 2;
        p.start_ms = 1000;
        p.end_ms = 2000;
        p.position_ms = 0;
        std::fs::write(root.0.join("_presentation.json"),serde_json::to_vec(&serde_json::json!({"broadcast":{"recorded_replay":p},"vehicle":{"model_url":"/api/stage-models/stage-1/rocket"}})).unwrap()).unwrap();
        let program = contract::ticket(&state, &HeaderMap::new(), "program")
            .await
            .unwrap();
        let app = contract::routes().merge(routes()).with_state(state);
        let request = |path: String| Request::builder().uri(path).body(Body::empty()).unwrap();
        let response = app
            .clone()
            .oneshot(request(format!(
                "/api/media-assets/recorded-vehicle/state?ticket={program}"
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 100000).await.unwrap()).unwrap();
        let url = body["config"]["model_url"].as_str().unwrap();
        assert!(url.starts_with("/api/media-assets/models/stage-1/rocket?ticket="));
        assert!(!url.ends_with(&program));
        assert_eq!(
            app.clone()
                .oneshot(request(format!(
                    "/api/media-assets/models/stage-1/rocket?ticket={program}"
                )))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = app.oneshot(request(url.into())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            &to_bytes(response.into_body(), 1000000).await.unwrap()[..4],
            b"glTF"
        );
    }
    #[tokio::test]
    async fn stream_manager_selection_is_persisted_and_stale_or_viewer_writes_are_rejected() {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;
        let root = crate::recording_export::tests::TestDirectory::new();
        let mut state = media_state(&root.0, false).await;
        let users = root.0.join("operator-users.json");
        std::fs::write(&users,serde_json::to_vec(&serde_json::json!({"anonymous":{"view_data":false},"users":[
            {"username":"operator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"viewer","roles":["stream_viewer"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}
        ]})).unwrap()).unwrap();
        let inner = Arc::get_mut(&mut state).unwrap();
        Arc::get_mut(&mut inner.app).unwrap().auth = Arc::new(crate::auth::AuthManager::new(users));
        for name in ["operator", "viewer"] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,allowed_commands_json,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,0,'[]',0,9999999999999)")
                .bind(format!("fixture-{name}")).bind(name).execute(&state.app.auth_db).await.unwrap();
        }
        let id = "groundstation_recording_1.db";
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id))
            .await
            .unwrap();
        insert(&db, 1000, "GPS_DATA", "[43,-78,300]").await;
        insert(&db, 2000, "GPS_DATA", "[43,-78,4000]").await;
        let mut selection = plan();
        selection.recording_id = id.into();
        selection.upper_id = 0;
        selection.start_ms = 0;
        selection.end_ms = 0;
        selection.position_ms = 0;
        selection.anchor_server_ms = 0;
        selection.paused = true;
        let mut broadcast: serde_json::Value =
            serde_json::to_value(contract::Broadcast::default()).unwrap();
        broadcast["layout"] = "recorded-replay".into();
        broadcast["recorded_replay"] = serde_json::to_value(selection).unwrap();
        let app = contract::routes().merge(routes()).with_state(state.clone());
        let request = |name: &str, body: serde_json::Value| {
            Request::builder()
                .method("POST")
                .uri("/api/live_streams/control")
                .header(header::AUTHORIZATION, format!("Bearer fixture-{name}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request("viewer", broadcast.clone()))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = app
            .clone()
            .oneshot(request("operator", broadcast.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let saved: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 100000).await.unwrap()).unwrap();
        assert_eq!(saved["revision"], 1);
        assert_eq!(saved["recorded_replay"]["upper_id"], 2);
        assert_eq!(saved["recorded_replay"]["start_ms"], 1000);
        assert_eq!(
            app.clone()
                .oneshot(request("operator", broadcast))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT,
            "oldoperatorpagecannotoverwriteanewprogramme"
        );
        let persisted: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(root.0.join("_presentation.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            persisted["broadcast"]["recorded_replay"],
            saved["recorded_replay"]
        );
        let restarted = media_state(&root.0, true).await;
        let replay = selected(&restarted).await.unwrap().0;
        assert_eq!(replay.recording_id, id);
        assert_eq!(replay.upper_id, 2);
        assert!(replay.paused);
        assert!(!root.0.join("_presentation.upload").exists());
    }
    #[test]
    fn recorded_clock_pause_speed_seek_and_loop_are_bounded() {
        let mut p = plan();
        assert!(p.valid());
        assert_eq!(p.elapsed(51000), 3000);
        assert_eq!(p.elapsed(49000), 2000);
        p.paused = true;
        assert_eq!(p.elapsed(90000), 2000);
        p.paused = false;
        p.speed = 4.;
        assert_eq!(p.elapsed(52000), 10000);
        p.loop_playback = true;
        assert_eq!(p.elapsed(53000), 4000);
        p.speed = f64::NAN;
        assert!(!p.valid());
    }
    #[test]
    fn client_cannot_expand_saved_window_types() {
        let config = serde_json::json!({"attitude":{"roll":{"data_type":"ROLL"}},"motions":[{"binding":{"data_type":"EULER_ANGLES"}}]});
        let types = requested_types(&config);
        assert!(types.contains(&"ROLL".into()));
        assert!(types.contains(&"GPS_DATA".into()));
        assert_eq!(types.len(), 6);
    }
}
