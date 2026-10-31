//! Wire schemas match Seds-Ground-Station-Frontend/docs/backend-api.md.
use super::*;
use serde::Deserialize;
use std::{collections::BTreeMap, time::Instant};
#[path = "program.rs"]
pub(crate) mod program;
#[path = "replay.rs"]
pub(crate) mod replay;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Broadcast {
    delay_seconds: u32,
    comms_audio_enabled: bool,
    label: String,
    featured_stream_id: String,
    hidden_stream_ids: Vec<String>,
    layout: String,
    tiles: Vec<CameraTile>,
    pub(super) revision: u64,
    replay: Option<replay::Plan>,
    #[serde(default = "legacy_rundown")]
    rundown: super::rundown::Rundown,
    commentary_audio_enabled: bool,
    interview: Interview,
    pub(super) recorded_replay: Option<super::recorded_vehicle::RecordedReplayPlan>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraTile { stream_id:String, x:f64, y:f64, width:f64, height:f64 }
fn valid_tiles(tiles:&[CameraTile])->bool {
 let mut ids=std::collections::HashSet::new();
 tiles.len()<=8 && tiles.iter().all(|t|valid_id(&t.stream_id)&&ids.insert(&t.stream_id)&&[t.x,t.y,t.width,t.height].iter().all(|v|v.is_finite())&&t.x>=0.0&&t.y>=0.0&&t.width>=10.0&&t.height>=10.0&&t.x+t.width<=100.001&&t.y+t.height<=100.001)
}

fn legacy_rundown() -> super::rundown::Rundown {
    super::rundown::Rundown {
        flight_phase_version: 0,
        ..Default::default()
    }
}

impl Default for Broadcast {
    fn default() -> Self {
        Self {
            delay_seconds: 10,
            comms_audio_enabled: false,
            label: String::new(),
            featured_stream_id: String::new(),
            hidden_stream_ids: Vec::new(),
            layout: "hero".into(),
            tiles: Vec::new(),
            revision: 0,
            replay: None,
            rundown: Default::default(),
            commentary_audio_enabled: false,
            interview: Default::default(),
            recorded_replay: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Interview {
    main_stream_id: String,
    pip_stream_id: String,
    main_audio: bool,
    pip_audio: bool,
}
impl Default for Interview {
    fn default() -> Self {
        Self {
            main_stream_id: String::new(),
            pip_stream_id: String::new(),
            main_audio: true,
            pip_audio: false,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Presentation {
    title: String,
    pub(super) stream_labels: BTreeMap<String, String>,
    stats: Vec<BroadcastStat>,
    pub(super) broadcast: Broadcast,
    pub(super) vehicle: Vehicle,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Binding {
    pub(super) data_type: String,
    #[serde(default)]
    pub(super) sender_id: Option<String>,
    #[serde(default)]
    pub(super) index: usize,
    #[serde(default = "one")]
    pub(super) scale: f32,
    #[serde(default)]
    pub(super) offset: f32,
}
fn require_current_phase_version(version: u8) -> ApiResult<()> {
    match version {
        crate::types::FLIGHT_PHASE_VERSION => Ok(()),
        0 => Err(error(
            StatusCode::BAD_REQUEST,
            "Reload the updated UI before editing flight-phase settings",
        )),
        _ => Err(error(StatusCode::BAD_REQUEST, "Unsupported flight phase version")),
    }
}

fn one() -> f32 {
    1.0
}
fn precision() -> usize {
    1
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct BroadcastStat {
    pub(super) label: String,
    pub(super) binding: Binding,
    #[serde(default)]
    pub(super) unit: String,
    #[serde(default = "precision")]
    pub(super) precision: usize,
}

fn default_stats() -> Vec<BroadcastStat> {
    use crate::types::Board;
    [
        (
            "Altitude",
            "GPS_DATA",
            Some(Board::RFBoard.sender_id()),
            2,
            "m",
        ),
        (
            "Latitude",
            "GPS_DATA",
            Some(Board::RFBoard.sender_id()),
            0,
            "°",
        ),
        (
            "Longitude",
            "GPS_DATA",
            Some(Board::RFBoard.sender_id()),
            1,
            "°",
        ),
        (
            "Tank pressure",
            "PRESSURE_TRANSDUCER_CALIBRATED",
            None,
            0,
            "psi",
        ),
        ("Fill mass", "LOADCELL_WEIGHT_KG", None, 0, "kg"),
        ("Fill level", "LOADCELL_FILL_PERCENT", None, 0, "%"),
    ]
    .into_iter()
    .map(|(label, data_type, sender, index, unit)| BroadcastStat {
        label: label.into(),
        binding: Binding {
            data_type: data_type.into(),
            sender_id: sender.map(str::to_owned),
            index,
            scale: 1.0,
            offset: 0.0,
        },
        unit: unit.into(),
        precision: if unit == "°" { 5 } else { 1 },
    })
    .collect()
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Vehicle {
    flight_phase_version: u8,
    motions: Vec<Motion>,
    title: String,
    model_url: String,
    ground_model_url: String,
    renderer_url: String,
    model_alt: String,
    camera_orbit: String,
    phase_animations: BTreeMap<String, String>,
    attitude: Attitude,
    stages: Vec<Stage>,
    ground_systems: Vec<Component>,
}

impl Vehicle {
    fn normalize_flight_phases(&mut self) -> Result<(), &'static str> {
        let version = self.flight_phase_version;
        crate::types::normalize_phase_map(&mut self.phase_animations, version)?;
        for motion in &mut self.motions {
            crate::types::normalize_phase_map(&mut motion.phase_values, version)?;
        }
        self.flight_phase_version = crate::types::FLIGHT_PHASE_VERSION;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Motion {
    node: String,
    transform: String,
    axis: [f32; 3],
    from: f32,
    to: f32,
    #[serde(default)]
    binding: Option<Binding>,
    #[serde(default)]
    phase_values: BTreeMap<String, f32>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Attitude {
    roll: Option<Binding>,
    pitch: Option<Binding>,
    yaw: Option<Binding>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Stage {
    id: String,
    label: String,
    #[serde(default)]
    separation: Option<Binding>,
    #[serde(default)]
    components: Vec<Component>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Component {
    id: String,
    label: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    binding: Option<Binding>,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    min: Option<f32>,
    #[serde(default)]
    max: Option<f32>,
    #[serde(default)]
    active_threshold: Option<f32>,
}

pub(super) async fn capture_program_history(state: Arc<MediaState>) {
    program::capture_history(state).await;
}

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route("/api/live_streams", get(live_streams))
        .route("/api/live_streams/control", post(control_broadcast))
        .merge(replay::routes())
        .merge(program::routes())
        .route("/assets/three/{file}", get(three_asset))
        .route(
            "/api/vehicle_visualization",
            get(vehicle).put(save_vehicle).post(save_vehicle_post),
        )
        .route("/api/media-assets/streams/{id}", get(player))
        .route(
            "/assets/model-viewer.min.js",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "text/javascript"),
                        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                    ],
                    gs_browser_assets::MODEL_VIEWER_JS,
                )
            }),
        )
        .route(
            "/assets/models/gse-site.glb",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "model/gltf-binary"),
                        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                    ],
                    gs_browser_assets::GSE_SITE_MODEL,
                )
            }),
        )
        .route(
            "/assets/models/vehicle.glb",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "model/gltf-binary"),
                        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                    ],
                    gs_browser_assets::VEHICLE_MODEL,
                )
            }),
        )
        .route("/api/media-assets/models/{stage}/{name}", get(model_asset))
}

pub(super) async fn read_presentation(state: &MediaState) -> ApiResult<Presentation> {
    let mut presentation: Presentation =
        match tokio::fs::read(state.models.join("_presentation.json")).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| {
                log::error!("media presentation: {err}");
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Invalid media presentation configuration",
                )
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Presentation::default()),
            Err(err) => Err(io_error(err)),
        }?;
    presentation.vehicle.normalize_flight_phases()
        .map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR, message))?;
    presentation.broadcast.rundown.normalize_flight_phases()
        .map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR, message))?;
    presentation
        .broadcast
        .rundown
        .validate()
        .map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR, message))?;
    if let Some(plan) = &presentation.broadcast.replay {
        if !replay::available(state, plan).await {
            presentation.broadcast.replay = None;
        }
    }
    if presentation.stats.is_empty() {
        presentation.stats = default_stats();
    }
    if cfg!(feature = "test_fire_mode") && !presentation.stats.iter().any(|s| s.label == "Thrust") {
        presentation.stats.push(BroadcastStat {
            label: "Thrust".into(),
            unit: "N".into(),
            precision: 1,
            binding: Binding {
                data_type: "LOADCELL_WEIGHT_KG".into(),
                sender_id: None,
                index: 0,
                scale: 9.80665,
                offset: 0.0,
            },
        });
    }
    Ok(presentation)
}

/// Normalize the same saved model contract as the operator replay without
/// putting a login token into an audience scene or uploaded-model URL.
pub(super) async fn recorded_vehicle_config(
    state: &Arc<MediaState>,
    headers: &HeaderMap,
    query: &MediaQuery,
) -> ApiResult<serde_json::Value> {
    let headers = ticket_headers(state, headers, query, "program").await?;
    let vehicle = load_vehicle(state.clone(), headers).await?;
    serde_json::to_value(vehicle).map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Saved replay configuration unavailable",
        )
    })
}

pub(super) fn program_now_ms() -> u64 {
    program::now_ms()
}

pub(super) async fn initialize_voice_broadcast(state: &MediaState) {
    let _guard = state.presentation_write.lock().await;
    if let Ok(presentation) = read_presentation(state).await {
        state
            .voice
            .set_mix(
                presentation.broadcast.comms_audio_enabled,
                presentation.broadcast.commentary_audio_enabled,
            )
            .await;
    }
}

pub(super) async fn write_presentation(state: &MediaState, value: &Presentation) -> ApiResult<()> {
    tokio::fs::create_dir_all(&state.models)
        .await
        .map_err(io_error)?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid media configuration"))?;
    let temp = state.models.join("_presentation.upload");
    tokio::fs::write(&temp, bytes).await.map_err(io_error)?;
    tokio::fs::rename(&temp, state.models.join("_presentation.json"))
        .await
        .map_err(io_error)
}

/// A narrow media capability, never the user's login token. Rechecked against
/// normal session authorization on every use; stable across frontend polling.
pub(super) struct Ticket {
    id: String,
    authorization: Option<axum::http::HeaderValue>,
    resource: String,
    expires: Instant,
}

#[derive(Default, Deserialize)]
pub(super) struct MediaQuery {
    ticket: Option<String>,
    session: Option<String>,
    playback_ms: Option<u64>,
}

pub(super) async fn ticket(
    state: &MediaState,
    headers: &HeaderMap,
    resource: &str,
) -> ApiResult<String> {
    let authorization = headers.get(header::AUTHORIZATION).cloned();
    let mut tickets = state.tickets.lock().await;
    tickets.retain(|ticket| ticket.expires > Instant::now());
    if let Some(existing) = tickets.iter().find(|t| {
        t.authorization == authorization
            && t.resource == resource
            && t.expires > Instant::now() + Duration::from_secs(60)
    }) {
        return Ok(existing.id.clone());
    }
    if tickets.len() >= 4096 {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many active media viewers",
        ));
    }
    let mut bytes = [0u8; 32];
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot create media access ticket",
            )
        })?;
    use base64::Engine;
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    tickets.push(Ticket {
        id: id.clone(),
        authorization,
        resource: resource.into(),
        expires: Instant::now() + Duration::from_secs(3600),
    });
    Ok(id)
}

pub(super) async fn authorize_media(
    state: &MediaState,
    headers: &HeaderMap,
    query: &MediaQuery,
    resource: &str,
) -> ApiResult<()> {
    let headers = ticket_headers(state, headers, query, resource).await?;
    authorize(state, &headers, Permission::ViewData).await
}

async fn ticket_headers(
    state: &MediaState,
    headers: &HeaderMap,
    query: &MediaQuery,
    resource: &str,
) -> ApiResult<HeaderMap> {
    let Some(id) = &query.ticket else {
        return Ok(headers.clone());
    };
    let mut headers = HeaderMap::new();
    {
        let tickets = state.tickets.lock().await;
        let ticket = tickets
            .iter()
            .find(|t| &t.id == id && t.resource == resource && t.expires > Instant::now())
            .ok_or_else(|| {
                error(
                    StatusCode::UNAUTHORIZED,
                    "Media access expired; refresh the dashboard",
                )
            })?;
        if let Some(value) = &ticket.authorization {
            headers.insert(header::AUTHORIZATION, value.clone());
        }
    }
    authorize(state,&headers,Permission::ViewData).await?;
    if let Some(id)=&query.ticket {if let Some(ticket)=state.tickets.lock().await.iter_mut().find(|t|&t.id==id&&t.resource==resource){ticket.expires=Instant::now()+Duration::from_secs(3600);}}
    Ok(headers)
}

#[derive(Deserialize)]
pub(super) struct CameraName {name:String}
pub(super) async fn rename_camera(State(state):State<Arc<MediaState>>,Path(id):Path<String>,headers:HeaderMap,axum::Json(body):axum::Json<CameraName>)->ApiResult<axum::Json<serde_json::Value>>{
 let principal=crate::web::authorize_headers(&state.app,&headers,Permission::ViewData).await?;if !principal.can_manage_stream(){return Err(error(StatusCode::FORBIDDEN,"Stream-manager access required"));}
 check_id(&id)?;let id=id.strip_prefix("transcoded-").unwrap_or(&id).to_owned();let name=body.name.trim();if name.is_empty()||name.len()>256||name.chars().count()>80||name.chars().any(char::is_control){return Err(error(StatusCode::BAD_REQUEST,"Choose a camera name of 1–80 characters"));}
 let _guard=state.presentation_write.lock().await;let mut presentation=read_presentation(&state).await?;presentation.stream_labels.insert(id.clone(),name.to_owned());write_presentation(&state,&presentation).await?;Ok(axum::Json(serde_json::json!({"id":id,"name":name})))
}

pub(super) async fn authorize_preview(state: &MediaState, headers: &HeaderMap) -> ApiResult<()> {
    let p = crate::web::authorize_headers(&state.app, headers, Permission::ViewData).await?;
    if p.can_manage_stream() {
        Ok(())
    } else {
        Err(error(
            StatusCode::FORBIDDEN,
            "Live preview requires stream-manager access",
        ))
    }
}
pub(super) async fn authorize_preview_media(
    state: &MediaState,
    headers: &HeaderMap,
    query: &MediaQuery,
    resource: &str,
) -> ApiResult<()> {
    authorize_preview(
        state,
        &ticket_headers(state, headers, query, resource).await?,
    )
    .await
}

pub(super) async fn preview_owner(
    state: &MediaState,
    headers: &HeaderMap,
    query: &MediaQuery,
    resource: &str,
) -> ApiResult<String> {
    let headers = ticket_headers(state, headers, query, resource).await?;
    authorize_preview(state, &headers).await?;
    let p = crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await?;
    Ok(p.username.unwrap_or_default())
}

async fn live_streams(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let principal =
        crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await?;
    let presentation = read_presentation(&state).await?;
    // Receiver outages still return a valid contract so Mission can show Vehicle.
    let streams = match list_streams(State(state.clone()), headers.clone()).await {
        Ok(streams) => streams.0,
        Err(_) => Vec::new(),
    };
    let mut feeds = Vec::new();
    let program_ticket = ticket(&state, &headers, "program").await?;
    let program_url = format!("/api/media-assets/program?ticket={program_ticket}");
    for stream in streams {
        let logical=stream.id.strip_prefix("transcoded-").unwrap_or(&stream.id);
        let ticket = ticket(&state, &headers, &format!("stream:{}", stream.id)).await?;
        feeds.push(serde_json::json!({
            "id": logical, "label": presentation.stream_labels.get(logical).unwrap_or(&stream.name),
            "url": format!("/api/media-assets/streams/{}?ticket={ticket}", stream.id),
            "kind": "webrtc", "poster_url": "", "online": stream.live,
        }));
    }
    let default_id = if presentation.broadcast.featured_stream_id.is_empty() {
        feeds
            .iter()
            .find(|s| s["online"] == true)
            .and_then(|s| s["id"].as_str())
            .unwrap_or("")
            .to_owned()
    } else {
        presentation.broadcast.featured_stream_id.clone()
    };
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(serde_json::json!({
            "title": presentation.title, "default_stream_id": default_id, "streams": feeds,
            "stats": presentation.stats, "broadcast": presentation.broadcast,
            "program_url": program_url,
            "can_manage_stream": principal.can_manage_stream(),
            "can_publish_camera": principal.can_publish_camera(),
            "can_preview_live": principal.can_manage_stream(),
        })),
    )
        .into_response())
}

pub(super) async fn control_broadcast(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(broadcast): axum::Json<Broadcast>,
) -> ApiResult<axum::Json<Broadcast>> {
    control_broadcast_inner(state, headers, broadcast, false).await
}

pub(super) async fn control_recorded_broadcast(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(broadcast): axum::Json<Broadcast>,
) -> ApiResult<axum::Json<Broadcast>> {
    control_broadcast_inner(state, headers, broadcast, true).await
}

async fn control_broadcast_inner(
    state: Arc<MediaState>,
    headers: HeaderMap,
    mut broadcast: Broadcast,
    edit_recorded_replay: bool,
) -> ApiResult<axum::Json<Broadcast>> {
    let principal =
        crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await?;
    if !principal.can_manage_stream() {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Stream-master permission required",
        ));
    }
    require_current_phase_version(broadcast.rundown.flight_phase_version)?;
    broadcast.rundown.normalize_flight_phases()
        .map_err(|message| error(StatusCode::BAD_REQUEST, message))?;
    broadcast
        .rundown
        .validate()
        .map_err(|message| error(StatusCode::BAD_REQUEST, message))?;
    if broadcast.label.len() > 200
        || !(3..=60).contains(&broadcast.delay_seconds)
        || ![
            "hero",
            "grid",
            "tiles",
            "model",
            "grid-model",
            "interview",
            "map",
            "recorded-replay",
        ]
        .contains(&broadcast.layout.as_str())
        || !valid_tiles(&broadcast.tiles)
        || (broadcast.layout=="tiles" && broadcast.tiles.is_empty())
        || broadcast.hidden_stream_ids.len() > 1000
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Invalid broadcast label, layout or camera list",
        ));
    }
    if broadcast.layout == "interview"
        || broadcast
            .rundown
            .profiles
            .values()
            .any(|p| p.scene == "interview")
    {
        if broadcast.interview.main_stream_id.is_empty()
            || broadcast.interview.pip_stream_id.is_empty()
            || broadcast.interview.main_stream_id == broadcast.interview.pip_stream_id
        {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Choose different interview and picture-in-picture camera/participant feeds",
            ));
        }
    }
    for id in [
        &broadcast.interview.main_stream_id,
        &broadcast.interview.pip_stream_id,
    ] {
        if !id.is_empty() {
            check_id(id)?;
        }
    }
    if edit_recorded_replay {
        if let Some(plan) = broadcast.recorded_replay.as_mut() {
            super::recorded_vehicle::validate_plan(&state, plan).await?;
        }
    }
    if !broadcast.featured_stream_id.is_empty() {
        check_id(&broadcast.featured_stream_id)?;
    }
    for id in &broadcast.hidden_stream_ids {
        check_id(id)?;
    }
    broadcast.hidden_stream_ids.sort();
    broadcast.hidden_stream_ids.dedup();
    let _guard = state.presentation_write.lock().await;
    let mut presentation = read_presentation(&state).await?;
    if broadcast.revision != presentation.broadcast.revision {
        return Err(error(
            StatusCode::CONFLICT,
            "Broadcast changed; refresh before editing",
        ));
    }
    if edit_recorded_replay {
        // An automatic refresh can extend a snapshot while an explicit seek or
        // pause request is in flight. Keep those newly saved bounds while
        // applying the requested playback position and controls atomically.
        if let (Some(requested), Some(current)) = (
            broadcast.recorded_replay.as_mut(),
            presentation.broadcast.recorded_replay.as_ref(),
        ) {
            super::recorded_vehicle::preserve_grown_snapshot(requested, current);
        }
    } else {
        // Scene/rundown/audio edits never own the saved replay clock. A draft
        // may predate automatic growth without being a stale operator edit.
        broadcast.recorded_replay = presentation.broadcast.recorded_replay.clone();
    }
    replay::prepare(&state, &mut broadcast, &presentation.broadcast).await?;
    broadcast.revision = presentation
        .broadcast
        .revision
        .checked_add(1)
        .ok_or_else(|| error(StatusCode::CONFLICT, "Broadcast revision exhausted"))?;
    presentation.broadcast = broadcast.clone();
    write_presentation(&state, &presentation).await?;
    state
        .voice
        .set_mix(
            broadcast.comms_audio_enabled,
            broadcast.commentary_audio_enabled,
        )
        .await;
    Ok(axum::Json(broadcast))
}

async fn vehicle(State(state): State<Arc<MediaState>>, headers: HeaderMap) -> ApiResult<Response> {
    let vehicle = load_vehicle(state, headers).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], axum::Json(vehicle)).into_response())
}

async fn load_vehicle(state: Arc<MediaState>, headers: HeaderMap) -> ApiResult<Vehicle> {
    authorize(&state, &headers, Permission::ViewData).await?;
    let mut vehicle = read_presentation(&state).await?.vehicle;
    let models = list_models(State(state.clone()), headers.clone()).await?.0;
    for model in &models {
        if !vehicle.stages.iter().any(|stage| stage.id == model.stage) {
            vehicle.stages.push(Stage {
                id: model.stage.clone(),
                label: model.stage.clone(),
                separation: None,
                components: Vec::new(),
            });
        }
    }
    if vehicle.renderer_url.is_empty() {
        vehicle.renderer_url = "/assets/three/vehicle-renderer.js".into();
    }
    if vehicle.title.is_empty() {
        vehicle.title = "Rocket".into();
    }
    // The pad scene is independent of which uploaded rocket model is selected.
    if vehicle.ground_model_url.is_empty() {
        vehicle.ground_model_url = "/assets/models/gse-site.glb".into();
    }
    if vehicle.model_url.is_empty()
        && let Some(model) = models.first()
    {
        vehicle.model_url = format!("/api/stage-models/{}/{}", model.stage, model.name);
    }
    if vehicle.model_url.is_empty() || vehicle.model_url == "/assets/models/vehicle.glb" {
        vehicle.model_url = "/assets/models/vehicle.glb".into();
        vehicle.model_alt = "Single-stage rocket with aft fins".into();
        vehicle.motions.retain(|motion| {
            !matches!(motion.node.as_str(), "booster-stage" | "sustainer-stage")
                && !motion.node.starts_with("airbrake-")
        });
        if vehicle.motions.is_empty() {
            vehicle.motions = vec![
                Motion {
                    node: "motor-flame".into(),
                    transform: "visible".into(),
                    axis: [0.0, 1.0, 0.0],
                    from: 0.0,
                    to: 1.0,
                    binding: None,
                    phase_values: BTreeMap::from([("*".into(), 0.0), ("Ascent".into(), 1.0)]),
                },
                Motion {
                    node: "drogue-parachute".into(),
                    transform: "scale".into(),
                    axis: [1.0, 1.0, 1.0],
                    from: 0.001,
                    to: 1.0,
                    binding: None,
                    phase_values: BTreeMap::from([
                        ("*".into(), 0.0),
                        ("Descent".into(), 1.0),
                    ]),
                },
                Motion {
                    node: "main-parachute".into(),
                    transform: "scale".into(),
                    axis: [1.0, 1.0, 1.0],
                    from: 0.001,
                    to: 1.0,
                    binding: None,
                    phase_values: BTreeMap::from([
                        ("*".into(), 0.0),
                        ("Reefing".into(), 1.0),
                        ("Landed".into(), 1.0),
                        ("Recovery".into(), 1.0),
                    ]),
                },
            ];
        }
        vehicle.stages = vec![Stage {
            id: "stage-1".into(),
            label: "Single stage".into(),
            separation: None,
            components: vec![],
        }];
    }
    if let Some(path) = vehicle.model_url.strip_prefix("/api/stage-models/") {
        let (stage, name) = path
            .split_once('/')
            .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Invalid model URL"))?;
        model_path(&state, stage, name)?;
        let ticket = ticket(&state, &headers, &format!("model:{stage}:{name}")).await?;
        vehicle.model_url = format!("/api/media-assets/models/{stage}/{name}?ticket={ticket}");
    }
    Ok(vehicle)
}

async fn save_vehicle(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(mut vehicle): axum::Json<Vehicle>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers, Permission::SendCommands).await?;
    require_current_phase_version(vehicle.flight_phase_version)?;
    vehicle.normalize_flight_phases()
        .map_err(|message| error(StatusCode::BAD_REQUEST, message))?;
    if !vehicle.ground_model_url.is_empty()
        && vehicle.ground_model_url != "/assets/models/gse-site.glb"
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Select the bundled ground site model",
        ));
    }
    if vehicle.motions.len() > 128
        || vehicle.motions.iter().any(|m| {
            m.node.is_empty()
                || m.node.len() > 128
                || !["rotate", "translate", "scale", "visible"].contains(&m.transform.as_str())
                || !m.from.is_finite()
                || !m.to.is_finite()
                || m.axis.iter().any(|x| !x.is_finite())
                || m.phase_values.values().any(|x| !x.is_finite())
        })
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Invalid model motion mapping",
        ));
    }
    if !vehicle.model_url.is_empty()
        && !["/assets/models/vehicle.glb", "/assets/models/gse-site.glb"]
            .contains(&vehicle.model_url.as_str())
    {
        let path = vehicle
            .model_url
            .strip_prefix("/api/stage-models/")
            .ok_or_else(|| {
                error(
                    StatusCode::BAD_REQUEST,
                    "Select a stored /api/stage-models/STAGE/NAME URL",
                )
            })?;
        let (stage, name) = path
            .split_once('/')
            .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Invalid model URL"))?;
        tokio::fs::metadata(model_path(&state, stage, name)?)
            .await
            .map_err(io_error)?;
    }
    let mut ids = std::collections::HashSet::new();
    for stage in &vehicle.stages {
        check_id(&stage.id)?;
        if !ids.insert(&stage.id) {
            return Err(error(StatusCode::BAD_REQUEST, "Duplicate stage ID"));
        }
    }
    let _guard = state.presentation_write.lock().await;
    let mut presentation = read_presentation(&state).await?;
    presentation.vehicle = vehicle;
    write_presentation(&state, &presentation).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn save_vehicle_post(
    state: State<Arc<MediaState>>,
    headers: HeaderMap,
    body: axum::Json<Vehicle>,
) -> ApiResult<axum::Json<serde_json::Value>> {
    save_vehicle(state, headers, body).await?;
    Ok(axum::Json(serde_json::json!({"saved":true})))
}

async fn three_asset(Path(file): Path<String>) -> ApiResult<Response> {
    let bytes: &'static [u8] = match file.as_str() {
        "vehicle-renderer.js" => gs_browser_assets::VEHICLE_RENDERER_JS,
        "replay-scene.js" => {
            gs_browser_assets::asset_bytes("REPLAY_SCENE_JS").ok_or_else(|| {
                error(
                    StatusCode::NOT_FOUND,
                    "Renderer asset unavailable in selected frontend",
                )
            })?
        }
        "software-renderer.js" => gs_browser_assets::SOFTWARE_RENDERER_JS,
        "three.module.js" => gs_browser_assets::THREE_MODULE_JS,
        "three.core.js" => gs_browser_assets::THREE_CORE_JS,
        "GLTFLoader.js" => gs_browser_assets::GLTF_LOADER_JS,
        "BufferGeometryUtils.js" => gs_browser_assets::BUFFER_GEOMETRY_UTILS_JS,
        _ => return Err(error(StatusCode::NOT_FOUND, "Unknown renderer asset")),
    };
    Ok((
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        bytes,
    )
        .into_response())
}

async fn player(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    check_id(&id)?;
    authorize_preview_media(&state, &headers, &query, &format!("stream:{id}")).await?;
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(gs_browser_assets::PLAYER_HTML),
    )
        .into_response())
}

async fn model_asset(
    State(state): State<Arc<MediaState>>,
    Path((stage, name)): Path<(String, String)>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authorize_media(&state, &headers, &query, &format!("model:{stage}:{name}")).await?;
    let bytes = tokio::fs::read(model_path(&state, &stage, &name)?)
        .await
        .map_err(io_error)?;
    Ok((
        [
            (header::CONTENT_TYPE, "model/gltf-binary"),
            (header::CACHE_CONTROL, "no-store"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_vehicle_phase_maps_upgrade_once_without_changing_chute_identity() {
        let mut vehicle: super::Vehicle = serde_json::from_value(serde_json::json!({
            "phase_animations": {"ParachuteDeploy":"drogue", "Descent":"main"},
            "motions": [{"node":"drogue", "transform":"visible", "axis":[0,1,0],
                "from":0, "to":1, "phase_values":{"ParachuteDeploy":1}},
                {"node":"main", "transform":"visible", "axis":[0,1,0],
                "from":0, "to":1, "phase_values":{"Descent":1}}]
        })).unwrap();
        vehicle.normalize_flight_phases().unwrap();
        assert_eq!(vehicle.phase_animations["Descent"], "drogue");
        assert_eq!(vehicle.phase_animations["Reefing"], "main");
        assert_eq!(vehicle.motions[0].phase_values["Descent"], 1.0);
        assert_eq!(vehicle.motions[1].phase_values["Reefing"], 1.0);
        let once = serde_json::to_value(&vehicle).unwrap();
        vehicle.normalize_flight_phases().unwrap();
        assert_eq!(serde_json::to_value(vehicle).unwrap(), once);
    }
    use super::*;
    #[test]
    fn saved_tiles_reject_duplicate_ids_and_out_of_canvas_geometry() {
        let tile = CameraTile {stream_id:"front".into(),x:0.0,y:0.0,width:65.0,height:100.0};
        assert!(valid_tiles(std::slice::from_ref(&tile)));
        assert!(!valid_tiles(&[tile.clone(),tile.clone()]));
        for bad in [CameraTile{x:60.0,..tile.clone()},CameraTile{width:0.0,..tile.clone()},CameraTile{y:f64::NAN,..tile.clone()},CameraTile{stream_id:"../front".into(),..tile.clone()}] {
            assert!(!valid_tiles(&[bad]));
        }
    }

    #[tokio::test]
    async fn selected_frontend_replay_asset_is_served_or_explicitly_absent() {
        let result = three_asset(Path("replay-scene.js".into())).await;
        match gs_browser_assets::asset_bytes("REPLAY_SCENE_JS") {
            Some(expected) => {
                let response = result.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CONTENT_TYPE], "text/javascript");
                let body = axum::body::to_bytes(response.into_body(), expected.len() + 1)
                    .await
                    .unwrap();
                assert_eq!(body.as_ref(), expected);
            }
            None => assert_eq!(result.err().unwrap().status(), StatusCode::NOT_FOUND),
        }
    }
    #[tokio::test]
    async fn unknown_renderer_asset_remains_not_found() {
        assert_eq!(
            three_asset(Path("unlisted.js".into()))
                .await
                .err()
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn vehicle_defaults_preserve_frontend_contract() {
        let config: Vehicle = serde_json::from_str(r#"{"stages":[{"id":"booster","label":"Stage 1","components":[{"id":"motor","label":"Motor","binding":{"data_type":"THRUST"}}]}]}"#).unwrap();
        assert_eq!(
            config.stages[0].components[0]
                .binding
                .as_ref()
                .unwrap()
                .scale,
            1.0
        );
        let wire = serde_json::to_value(config).unwrap();
        for key in [
            "model_url",
            "ground_model_url",
            "renderer_url",
            "phase_animations",
            "attitude",
            "stages",
            "ground_systems",
        ] {
            assert!(wire.get(key).is_some());
        }
    }
    #[test]
    fn dashboard_defaults_have_real_telemetry_channels() {
        let stats = default_stats();
        assert_eq!(stats.len(), 6);
        assert_eq!(stats[0].binding.data_type, "GPS_DATA");
        assert_eq!(stats[0].binding.sender_id.as_deref(), Some("RF"));
        assert_eq!(stats[0].binding.index, 2);
        assert_eq!(
            stats[3].binding.data_type,
            crate::loadcell::DERIVED_PRESSURE_TRANSDUCER_CALIBRATED_DATA_TYPE
        );
        assert_eq!(
            stats[4].binding.data_type,
            crate::loadcell::DERIVED_WEIGHT_DATA_TYPE
        );
        assert_eq!(
            stats[5].binding.data_type,
            crate::loadcell::DERIVED_FILL_PERCENT_DATA_TYPE
        );
    }
}
