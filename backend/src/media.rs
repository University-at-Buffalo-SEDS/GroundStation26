//! Authenticated WebRTC signaling, camera discovery and persistent per-stage GLB assets.
#[path = "media/contract.rs"]
mod contract;
use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::{auth::Permission, state::AppState};

const MAX_MODEL: usize = 64 * 1024 * 1024;
type ApiResult<T> = Result<T, Response>;

struct MediaState {
    app: Arc<AppState>,
    client: reqwest::Client,
    api_url: String,
    webrtc_url: String,
    relay_password: String,
    model_write: Mutex<()>,
    models: PathBuf,
    presentation_write: Mutex<()>,
    tickets: Mutex<Vec<contract::Ticket>>,
}

pub fn router(app: Arc<AppState>) -> Router<Arc<AppState>> {
    let state = Arc::new(MediaState {
        app,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("media HTTP client"),
        api_url: std::env::var("GS_VIDEO_API_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9997".into()),
        webrtc_url: std::env::var("GS_VIDEO_WEBRTC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8889".into()),
        relay_password: std::env::var("GS_VIDEO_PASSWORD").unwrap_or_default(),
        model_write: Mutex::new(()),
        presentation_write: Mutex::new(()),
        tickets: Mutex::new(Vec::new()),
        models: std::env::var_os("GS_STAGE_MODELS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/stage_models")),
    });
    Router::new()
        .merge(contract::routes())
        .route("/api/video/streams", get(list_streams))
        .route(
            "/api/video/streams/{id}/whep",
            post(whep_offer).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/video/streams/{id}/whep/{session}",
            delete(whep_delete),
        )
        .route("/api/stage-models", get(list_models))
        .route(
            "/api/stage-models/{stage}/{name}",
            get(download_model)
                .put(upload_model)
                .layer(DefaultBodyLimit::max(MAX_MODEL)),
        )
        .with_state(state)
        .route("/media", get(|| async { Html(include_str!("media.html")) }))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, message.to_owned()).into_response()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn check_id(id: &str) -> ApiResult<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(error(
            StatusCode::BAD_REQUEST,
            "IDs must be 1–64 ASCII letters, digits, hyphens or underscores",
        ))
    }
}

async fn authorize(
    state: &MediaState,
    headers: &HeaderMap,
    permission: Permission,
) -> ApiResult<()> {
    crate::web::authorize_headers(&state.app, headers, permission)
        .await
        .map(|_| ())
}

fn upstream_error(err: reqwest::Error) -> Response {
    log::warn!("video relay: {err}");
    error(
        StatusCode::BAD_GATEWAY,
        "Video relay unavailable; check MediaMTX and video configuration",
    )
}

#[derive(Serialize)]
struct StreamInfo {
    id: String,
    live: bool,
}

async fn list_streams(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
) -> ApiResult<axum::Json<Vec<StreamInfo>>> {
    authorize(&state, &headers, Permission::ViewData).await?;
    let response = state
        .client
        .get(format!(
            "{}/v3/paths/list?itemsPerPage=1000",
            state.api_url.trim_end_matches('/')
        ))
        .basic_auth("groundstation", Some(&state.relay_password))
        .send()
        .await
        .map_err(upstream_error)?
        .error_for_status()
        .map_err(upstream_error)?;
    let body: serde_json::Value = response.json().await.map_err(upstream_error)?;
    let streams = body["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item["name"].as_str()?;
            valid_id(id).then(|| StreamInfo {
                id: id.into(),
                live: item["ready"].as_bool().unwrap_or(false),
            })
        })
        .collect();
    Ok(axum::Json(streams))
}

fn session_id(location: &str) -> Option<&str> {
    let id = location.rsplit('/').next()?;
    valid_id(id).then_some(id)
}

async fn whep_offer(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    Query(query): Query<contract::MediaQuery>,
    headers: HeaderMap,
    bytes: Bytes,
) -> ApiResult<Response> {
    contract::authorize_media(&state, &headers, &query, &format!("stream:{id}")).await?;
    check_id(&id)?;
    let upstream = state
        .client
        .post(format!(
            "{}/{id}/whep",
            state.webrtc_url.trim_end_matches('/')
        ))
        .basic_auth("groundstation", Some(&state.relay_password))
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(bytes)
        .send()
        .await
        .map_err(upstream_error)?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(error(status, "Camera playback unavailable"));
    }
    let session = upstream
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(session_id)
        .ok_or_else(|| error(StatusCode::BAD_GATEWAY, "Invalid video session response"))?
        .to_owned();
    let body = upstream.bytes().await.map_err(upstream_error)?;
    Ok((
        status,
        [
            (header::CONTENT_TYPE, "application/sdp".to_owned()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (
                header::LOCATION,
                format!("/api/video/streams/{id}/whep/{session}"),
            ),
        ],
        body,
    )
        .into_response())
}

async fn whep_delete(
    State(state): State<Arc<MediaState>>,
    Path((id, session)): Path<(String, String)>,
    Query(query): Query<contract::MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    contract::authorize_media(&state, &headers, &query, &format!("stream:{id}")).await?;
    check_id(&id)?;
    check_id(&session)?;
    let upstream = state
        .client
        .request(
            Method::DELETE,
            format!(
                "{}/{id}/whep/{session}",
                state.webrtc_url.trim_end_matches('/')
            ),
        )
        .basic_auth("groundstation", Some(&state.relay_password))
        .send()
        .await
        .map_err(upstream_error)?;
    Ok(upstream.status())
}

fn valid_glb(bytes: &[u8]) -> bool {
    if bytes.len() < 20 || &bytes[..4] != b"glTF" {
        return false;
    }
    let number =
        |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    if number(4) != 2 || number(8) != bytes.len() || &bytes[16..20] != b"JSON" {
        return false;
    }
    let mut offset = 12;
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            return false;
        }
        let len = number(offset);
        if len % 4 != 0 || len > bytes.len() - offset - 8 {
            return false;
        }
        if offset == 12
            && serde_json::from_slice::<serde_json::Value>(&bytes[20..20 + len]).is_err()
        {
            return false;
        }
        offset += 8 + len;
    }
    offset == bytes.len()
}

fn model_path(state: &MediaState, stage: &str, name: &str) -> ApiResult<PathBuf> {
    check_id(stage)?;
    check_id(name)?;
    Ok(state.models.join(stage).join(format!("{name}.glb")))
}

fn io_error(err: std::io::Error) -> Response {
    if err.kind() == std::io::ErrorKind::NotFound {
        error(StatusCode::NOT_FOUND, "Model not found")
    } else {
        log::error!("stage model storage: {err}");
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Model storage unavailable",
        )
    }
}

async fn upload_model(
    State(state): State<Arc<MediaState>>,
    Path((stage, name)): Path<(String, String)>,
    headers: HeaderMap,
    bytes: Bytes,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers, Permission::SendCommands).await?;
    let path = model_path(&state, &stage, &name)?;
    if !valid_glb(&bytes) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Expected a GLB version 2 file",
        ));
    }
    let _guard = state.model_write.lock().await;
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .map_err(io_error)?;
    let temporary = path.with_extension("upload");
    tokio::fs::write(&temporary, &bytes)
        .await
        .map_err(io_error)?;
    tokio::fs::rename(&temporary, &path)
        .await
        .map_err(io_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn download_model(
    State(state): State<Arc<MediaState>>,
    Path((stage, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authorize(&state, &headers, Permission::ViewData).await?;
    let path = model_path(&state, &stage, &name)?;
    let bytes = tokio::fs::read(path).await.map_err(io_error)?;
    Ok((
        [
            (header::CONTENT_TYPE, "model/gltf-binary"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Serialize)]
struct ModelInfo {
    stage: String,
    name: String,
    size_bytes: u64,
}

async fn list_models(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
) -> ApiResult<axum::Json<Vec<ModelInfo>>> {
    authorize(&state, &headers, Permission::ViewData).await?;
    let mut models = Vec::new();
    let mut stages = match tokio::fs::read_dir(&state.models).await {
        Ok(stages) => stages,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(axum::Json(models)),
        Err(err) => return Err(io_error(err)),
    };
    while let Some(stage) = stages.next_entry().await.map_err(io_error)? {
        let stage_id = stage.file_name().to_string_lossy().into_owned();
        if !valid_id(&stage_id) || !stage.file_type().await.map_err(io_error)?.is_dir() {
            continue;
        }
        let mut files = tokio::fs::read_dir(stage.path()).await.map_err(io_error)?;
        while let Some(file) = files.next_entry().await.map_err(io_error)? {
            let path = file.path();
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if path.extension().is_some_and(|ext| ext == "glb")
                && valid_id(&name)
                && file.file_type().await.map_err(io_error)?.is_file()
            {
                models.push(ModelInfo {
                    stage: stage_id.clone(),
                    name,
                    size_bytes: file.metadata().await.map_err(io_error)?.len(),
                });
            }
        }
    }
    models.sort_by(|a, b| (&a.stage, &a.name).cmp(&(&b.stage, &b.name)));
    Ok(axum::Json(models))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape_storage() {
        for id in ["", "..", "../stage", "a/b", "a\\b", "stage.glb", "é"] {
            assert!(!valid_id(id));
        }
        assert!(valid_id("stage-1_booster"));
        assert!(!valid_id(&"a".repeat(65)));
    }

    #[test]
    fn glb_container_validation() {
        let json = br#"{"asset":{"version":"2.0"}} "#;
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&((20 + json.len()) as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(json);
        assert!(valid_glb(&glb));
        glb.pop();
        assert!(!valid_glb(&glb));
        assert!(!valid_glb(b"not a model"));
    }

    #[test]
    fn whep_locations_are_rewritten_without_trusting_upstream_hosts() {
        assert_eq!(
            session_id("http://video:8889/front/whep/abc-123"),
            Some("abc-123")
        );
        assert_eq!(session_id("/front/whep/abc-123"), Some("abc-123"));
        assert_eq!(session_id("/front/whep/.."), None);
        assert_eq!(session_id("/front/whep/id?token=secret"), None);
    }
}
