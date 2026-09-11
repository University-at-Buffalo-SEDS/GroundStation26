//! Wire schemas match Seds-Ground-Station-Frontend/docs/backend-api.md.
use super::*;
use serde::Deserialize;
use std::{collections::BTreeMap, time::Instant};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Broadcast {
    label: String,
    featured_stream_id: String,
    hidden_stream_ids: Vec<String>,
    layout: String,
    revision: u64,
}

impl Default for Broadcast {
    fn default() -> Self {
        Self {
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

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Vehicle {
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
        .route("/api/vehicle_visualization", get(vehicle).put(save_vehicle))
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
    }
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
    let Some(id) = &query.ticket else {
        return authorize(state, headers, Permission::ViewData).await;
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
    authorize(state, &headers, Permission::ViewData).await
}

async fn live_streams(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authorize(&state, &headers, Permission::ViewData).await?;
    let presentation = read_presentation(&state).await?;
    // Receiver outages still return a valid contract so Mission can show Vehicle.
    let streams = match list_streams(State(state.clone()), headers.clone()).await {
        Ok(streams) => streams.0,
        Err(_) => Vec::new(),
    };
    let mut feeds = Vec::new();
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
    if principal.anonymous
        || !(principal.session_type.as_deref() == Some("stream_master")
            || principal
                .command_access
                .allowed_commands
                .iter()
                .any(|cmd| cmd == "StreamControl"))
    {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Stream-master permission required",
        ));
    }
    if broadcast.label.len() > 200
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
        vehicle.renderer_url = "/assets/model-viewer.min.js".into();
    }
    if vehicle.title.is_empty() {
        vehicle.title = "Rocket".into();
    }
    if vehicle.model_url.is_empty()
        && let Some(model) = models.first()
    {
        vehicle.model_url = format!("/api/stage-models/{}/{}", model.stage, model.name);
    }
    if vehicle.model_url.is_empty() {
        vehicle.model_url = "/assets/models/vehicle.glb".into();
        vehicle.model_alt = "Minimal two-stage launch vehicle".into();
        vehicle.stages = vec![
            Stage {
                id: "booster".into(),
                label: "Booster".into(),
                separation: None,
                components: vec![],
            },
            Stage {
                id: "sustainer".into(),
                label: "Sustainer".into(),
                separation: None,
                components: vec![],
            },
        ];
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
    if !vehicle.model_url.is_empty() {
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

async fn player(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    check_id(&id)?;
    authorize_media(&state, &headers, &query, &format!("stream:{id}")).await?;
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
}
