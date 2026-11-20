//! Recording store HTTP API: ring status, captures, settings, snapshots.
use super::*;
use crate::auth::Permission;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use std::sync::Arc;

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/recordings/store", get(store))
        .route("/api/recordings/settings", put(update_settings).post(update_settings))
        .route("/api/recordings/snapshot", post(snapshot))
        .route("/api/recordings/{id}", axum::routing::delete(delete_recording))
        .route("/api/recordings/{id}/delete", post(delete_recording))
        .route("/api/recordings/{id}/file", get(download_file))
        .route("/api/recordings/{id}/manifest", get(manifest))
}

fn root(state: &AppState) -> PathBuf {
    crate::recording_export::directory(state)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[derive(Serialize)]
struct CaptureEntry {
    id: String,
    event_type: EventKind,
    title: String,
    label: String,
    bytes: u64,
    manifest: Manifest,
    attachments: Vec<Attachment>,
}
#[derive(Serialize)]
struct Attachment {
    id: String,
    bytes: u64,
}
#[derive(Serialize)]
struct Group {
    event_type: EventKind,
    title: String,
    captures: Vec<CaptureEntry>,
}
#[derive(Serialize)]
struct StoreResponse {
    always_on: bool,
    ring: RingStatus,
    in_progress: Vec<CaptureBrief>,
    groups: Vec<Group>,
    captures_bytes: u64,
    legacy: Vec<Listed>,
    settings: Settings,
    is_admin: bool,
}

async fn store(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let root = root(&state);
    let listed = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || (list_captures(&root), list_legacy(&root), dir_bytes(&events_dir(&root))))
            .await
            .unwrap_or_default()
    };
    let (captures, legacy, captures_bytes) = listed;
    let mut groups: Vec<Group> = Vec::new();
    for kind in EventKind::ALL {
        let mut entries = Vec::new();
        for item in captures.iter().filter(|c| c.event_type == Some(kind) && c.kind == ListedKind::Capture) {
            let Some(folder) = item.path.parent() else { continue };
            let Some(manifest) = read_manifest(folder) else { continue };
            let attachments = captures
                .iter()
                .filter(|a| a.kind == ListedKind::Attachment && a.path.parent() == Some(folder))
                .map(|a| Attachment { id: a.id.clone(), bytes: a.bytes })
                .collect();
            entries.push(CaptureEntry {
                id: item.id.clone(),
                event_type: kind,
                title: kind.title().into(),
                label: manifest.label.clone(),
                bytes: item.bytes,
                manifest,
                attachments,
            });
        }
        if !entries.is_empty() {
            groups.push(Group { event_type: kind, title: kind.title().into(), captures: entries });
        }
    }
    no_store(
        Json(StoreResponse {
            always_on: state.recording_store.rolling_enabled(),
            ring: state.recording_store.ring_status(),
            in_progress: state.recording_store.briefs(),
            groups,
            captures_bytes,
            legacy,
            settings: state.recording_store.settings(),
            is_admin: principal.is_admin(),
        })
        .into_response(),
    )
}

#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API")]
fn require_admin(principal: &crate::auth::AuthPrincipal, what: &str) -> Result<(), Response> {
    if principal.is_admin() {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, format!("Administrator access required to {what}")).into_response())
    }
}

async fn update_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(settings): Json<Settings>,
) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if let Err(response) = require_admin(&principal, "change recording settings") {
        return response;
    }
    let settings = settings.normalized();
    if let Err(error) = save_settings(&root(&state), &settings) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Could not save recording settings: {error}")).into_response();
    }
    log::warn!(
        "recording settings changed by {}: ring {} MB, segment {} MB",
        principal.username.as_deref().unwrap_or("?"),
        settings.ring_budget_mb,
        settings.segment_mb
    );
    state.recording_store.set_settings(settings.clone());
    Json(settings).into_response()
}

#[derive(Deserialize)]
struct SnapshotRequest {
    #[serde(default)]
    label: String,
    #[serde(default = "default_before")]
    before_s: u32,
    #[serde(default)]
    after_s: u32,
}
fn default_before() -> u32 {
    120
}

async fn snapshot(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<SnapshotRequest>,
) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if !state.recording_store.rolling_enabled() {
        return (StatusCode::SERVICE_UNAVAILABLE, "The rolling recording is not running on this station.").into_response();
    }
    let label: String = request.label.trim().chars().filter(|c| !c.is_control()).take(80).collect();
    let label = if label.is_empty() { "Operator snapshot".to_owned() } else { label };
    let before = request.before_s.min(4 * 3600);
    let after = request.after_s.min(3600);
    let operator = principal.username.clone();
    match state.recording_store.manual(&label, before, after, operator.as_deref(), now_ms(), &station_mode_id()) {
        Some(serial) => {
            log::info!("manual capture \"{label}\" requested by {}", operator.as_deref().unwrap_or("?"));
            publish(&state);
            Json(serde_json::json!({"serial": serial, "label": label, "before_s": before, "after_s": after})).into_response()
        }
        None => (StatusCode::SERVICE_UNAVAILABLE, "The rolling recording is not running on this station.").into_response(),
    }
}

async fn delete_recording(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if let Err(response) = require_admin(&principal, "delete recordings") {
        return response;
    }
    let Some(parsed) = RecordingId::parse(&id) else {
        return (StatusCode::BAD_REQUEST, "Invalid recording id").into_response();
    };
    if matches!(parsed, RecordingId::Segment(_)) {
        return (
            StatusCode::CONFLICT,
            "Rolling segments are managed by the ring budget. Save a manual capture to keep this data.",
        )
            .into_response();
    }
    let path = match crate::recording_export::recording_path(&root(&state), &id).await {
        Ok(path) => path,
        Err(status) => return (status, "Recording not found. Refresh the list.").into_response(),
    };
    let who = principal.username.clone().unwrap_or_else(|| "?".into());
    let removed = match &parsed {
        RecordingId::Event { file: None, .. } => match path.parent() {
            Some(folder) => tokio::fs::remove_dir_all(folder).await,
            None => Err(std::io::Error::other("no capture folder")),
        },
        _ => {
            remove_sqlite_files(&path);
            Ok(())
        }
    };
    match removed {
        Ok(()) => {
            log::warn!("recording {id} deleted by administrator {who}");
            state.add_notification(format!("Recording {id} was deleted by {who}."));
            Json(serde_json::json!({"deleted": id})).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Could not delete: {error}")).into_response(),
    }
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let path = match crate::recording_export::recording_path(&root(&state), &id).await {
        Ok(path) => path,
        Err(status) => return (status, "Recording not found. Refresh the list.").into_response(),
    };
    let active = state.recording_status_snapshot().db_path.is_some_and(|p| {
        Path::new(&p).file_name() == path.file_name() && matches!(RecordingId::parse(&id), Some(RecordingId::Segment(_)))
    });
    if active || !crate::telemetry_db::sqlite_sidecars_present(&path.to_string_lossy()).is_empty() {
        return (
            StatusCode::CONFLICT,
            "This recording is still being written. Use CSV export, or save a manual capture and download that.",
        )
            .into_response();
    }
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let stream = tokio_util_stream(file);
    no_store(
        (
            [
                (header::CONTENT_TYPE, "application/vnd.sqlite3".to_owned()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{id}\"")),
            ],
            Body::from_stream(stream),
        )
            .into_response(),
    )
}

fn tokio_util_stream(
    file: tokio::fs::File,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    use tokio::io::AsyncReadExt;
    futures::stream::try_unfold(file, |mut file| async move {
        let mut buf = vec![0u8; 64 * 1024];
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some((buf, file)))
    })
}

async fn manifest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    if !matches!(RecordingId::parse(&id), Some(RecordingId::Event { file: None, .. })) {
        return (StatusCode::BAD_REQUEST, "Only event captures have a manifest").into_response();
    }
    let path = match crate::recording_export::recording_path(&root(&state), &id).await {
        Ok(path) => path,
        Err(status) => return status.into_response(),
    };
    match path.parent().and_then(read_manifest) {
        Some(manifest) => no_store(Json(manifest).into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
