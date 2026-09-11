//! Wire schemas match Seds-Ground-Station-Frontend/docs/backend-api.md.
use super::*;
use serde::Deserialize;
use std::{collections::BTreeMap, time::Instant};
#[path = "program.rs"]
mod program;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Broadcast {
    delay_seconds: u32,
    label: String,
    featured_stream_id: String,
    hidden_stream_ids: Vec<String>,
    layout: String,
    revision: u64,
}

impl Default for Broadcast {
    fn default() -> Self {
        Self {
            delay_seconds: 10,
            label: String::new(),
            featured_stream_id: String::new(),
            hidden_stream_ids: Vec::new(),
            layout: "hero".into(),
            revision: 0,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Presentation {
    title: String,
    stream_labels: BTreeMap<String, String>,
    stats: Vec<BroadcastStat>,
    broadcast: Broadcast,
    vehicle: Vehicle,
}

#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    data_type: String,
    #[serde(default)]
    sender_id: Option<String>,
    #[serde(default)]
    index: usize,
    #[serde(default = "one")]
    scale: f32,
    #[serde(default)]
    offset: f32,
}
fn one() -> f32 {
    1.0
}
fn precision() -> usize {
    1
}

#[derive(Clone, Serialize, Deserialize)]
struct BroadcastStat {
    label: String,
    binding: Binding,
    #[serde(default)]
    unit: String,
    #[serde(default = "precision")]
    precision: usize,
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
struct Vehicle {
    motions: Vec<Motion>,
    title: String,
    model_url: String,
    renderer_url: String,
    model_alt: String,
    camera_orbit: String,
    phase_animations: BTreeMap<String, String>,
    attitude: Attitude,
    stages: Vec<Stage>,
    ground_systems: Vec<Component>,
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

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route("/api/live_streams", get(live_streams))
        .route("/api/live_streams/control", post(control_broadcast))
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
                    include_bytes!("../../assets/model-viewer.min.js").as_slice(),
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
                    include_bytes!("../../assets/models/gse-site.glb").as_slice(),
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
                    include_bytes!("../../assets/models/vehicle.glb").as_slice(),
                )
            }),
        )
        .route("/api/media-assets/models/{stage}/{name}", get(model_asset))
}

async fn read_presentation(state: &MediaState) -> ApiResult<Presentation> {
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
    if presentation.stats.is_empty() {
        presentation.stats = default_stats();
    }
    Ok(presentation)
}

async fn write_presentation(state: &MediaState, value: &Presentation) -> ApiResult<()> {
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
}

async fn ticket(state: &MediaState, headers: &HeaderMap, resource: &str) -> ApiResult<String> {
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
    Ok(headers)
}

pub(super) async fn authorize_preview(state: &MediaState, headers: &HeaderMap) -> ApiResult<()> {
    let p = crate::web::authorize_headers(&state.app, headers, Permission::ViewData).await?;
    if p.can_manage_stream() || p.permissions.send_commands {
        Ok(())
    } else {
        Err(error(
            StatusCode::FORBIDDEN,
            "Live preview requires operator or stream-master access",
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
        let ticket = ticket(&state, &headers, &format!("stream:{}", stream.id)).await?;
        feeds.push(serde_json::json!({
            "id": stream.id, "label": presentation.stream_labels.get(&stream.id).unwrap_or(&stream.id),
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
            "can_preview_live": principal.can_manage_stream() || principal.permissions.send_commands,
        })),
    )
        .into_response())
}

async fn control_broadcast(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(mut broadcast): axum::Json<Broadcast>,
) -> ApiResult<axum::Json<Broadcast>> {
    let principal =
        crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await?;
    if !principal.can_manage_stream() {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Stream-master permission required",
        ));
    }
    if broadcast.label.len() > 200
        || !(3..=60).contains(&broadcast.delay_seconds)
        || !["hero", "grid"].contains(&broadcast.layout.as_str())
        || broadcast.hidden_stream_ids.len() > 1000
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Invalid broadcast label, layout or camera list",
        ));
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
    broadcast.revision = presentation
        .broadcast
        .revision
        .checked_add(1)
        .ok_or_else(|| error(StatusCode::CONFLICT, "Broadcast revision exhausted"))?;
    presentation.broadcast = broadcast.clone();
    write_presentation(&state, &presentation).await?;
    Ok(axum::Json(broadcast))
}

async fn vehicle(State(state): State<Arc<MediaState>>, headers: HeaderMap) -> ApiResult<Response> {
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
                        ("ParachuteDeploy".into(), 1.0),
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
                        ("Descent".into(), 1.0),
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
    Ok(([(header::CACHE_CONTROL, "no-store")], axum::Json(vehicle)).into_response())
}

async fn save_vehicle(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    axum::Json(vehicle): axum::Json<Vehicle>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers, Permission::SendCommands).await?;
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
        "vehicle-renderer.js" => include_bytes!("../../assets/three/vehicle-renderer.js"),
        "three.module.js" => include_bytes!("../../assets/three/three.module.js"),
        "three.core.js" => include_bytes!("../../assets/three/three.core.js"),
        "GLTFLoader.js" => include_bytes!("../../assets/three/GLTFLoader.js"),
        "BufferGeometryUtils.js" => include_bytes!("../../assets/three/BufferGeometryUtils.js"),
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
        Html(include_str!("player.html")),
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
    use super::*;
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
