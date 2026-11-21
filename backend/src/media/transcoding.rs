//! Authenticated native workers encode remotely; this station only remuxes bytes.
use super::*;
use axum::{Json, extract::Request};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Instant};
use tokio::sync::watch;
use tokio::io::AsyncWriteExt;

const TEST_BYTES: usize = 4 * 1024 * 1024;
const LEASE: Duration = Duration::from_secs(15);
#[derive(Clone, Serialize)]
struct Host {
    id: String,
    name: String,
    passed: bool,
    offered: bool,
    capacity: usize,
    encoding_ms: u64,
    upload_bps: u64,
    download_bps: u64,
    #[serde(skip)]
    owner: String,
    #[serde(skip)]
    upload_bytes: usize,
    #[serde(skip)]
    downloaded: bool,
    #[serde(skip)]
    last_seen: Instant,
    #[serde(skip)]
    generation: String,
    #[serde(skip)]
    previously_closed: Option<String>,
    #[serde(skip)]
    generation_closed: bool,
    #[serde(skip)]
    fenced: bool,
}
#[derive(Default)]
pub(super) struct Registry {
    hosts: Vec<Host>,
    selected: Option<String>,
    uploads: HashMap<String, ActiveUpload>,
}
#[derive(Deserialize)]
struct Name {
    name: String,
}
#[derive(Deserialize)]
struct Report {
    encoding_ms: u64,
    download_bps: u64,
}
#[derive(Deserialize)]
struct Offer {
    offered: bool,
    #[serde(default)]
    generation: Option<String>,
}
#[derive(Deserialize)]
struct Selection {
    host_id: Option<String>,
}

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route("/api/video/transcode/hosts", get(hosts).post(select))
        .route("/api/video/transcode/test", post(begin))
        .route("/api/video/transcode/test/{id}/download", get(download))
        .route(
            "/api/video/transcode/test/{id}/upload",
            post(upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/video/transcode/test/{id}/complete", post(complete))
        .route("/api/video/transcode/host/{id}", post(heartbeat))
        .route(
            "/api/video/transcode/input/{id}/{file}",
            get(contract::program::raw_hls),
        )
        .route(
            "/api/video/transcode/output/{host}/{id}",
            post(output).layer(DefaultBodyLimit::disable()),
        )
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn manager(state: &MediaState, headers: &HeaderMap) -> ApiResult<String> {
    let p = crate::web::authorize_headers(&state.app, headers, Permission::ViewData).await?;
    if !p.can_manage_stream() {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Stream-manager access required",
        ));
    }
    p.username
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Sign in"))
}
fn eligible(h: &Host) -> bool {
    h.passed && h.offered && h.last_seen.elapsed() < LEASE
}
fn qualification(encoding_ms: u64, up: u64, down: u64) -> usize {
    if encoding_ms == 0 || encoding_ms > 2500 || up < 12_000_000 || down < 12_000_000 {
        return 0;
    }
    (2400 / encoding_ms)
        .min(up / 6_000_000)
        .min(down / 12_000_000)
        .clamp(1, 8) as usize
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn hosts(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    manager(&state, &headers).await?;
    let registry = state.transcoding.lock().await;
    Ok(Json(
        serde_json::json!({"selected":registry.selected,"hosts":registry.hosts.iter().map(|h|serde_json::json!({"id":h.id,"name":h.name,"passed":h.passed,"online":eligible(h),"capacity":h.capacity,"encoding_ms":h.encoding_ms,"upload_bps":h.upload_bps,"download_bps":h.download_bps})).collect::<Vec<_>>()}),
    ))
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn begin(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    Json(body): Json<Name>,
) -> ApiResult<Json<serde_json::Value>> {
    let owner = manager(&state, &headers).await?;
    let name = body.name.trim();
    if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
        return Err(error(StatusCode::BAD_REQUEST, "Choose a short host name"));
    }
    let mut registry = state.transcoding.lock().await;
    registry
        .hosts
        .retain(|h| h.last_seen.elapsed() < Duration::from_secs(3600));
    if registry.hosts.len() >= 32 {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many candidate hosts",
        ));
    }
    let id = format!("{:032x}", rand::random::<u128>());
    registry.hosts.push(Host {
        id: id.clone(),
        name: name.into(),
        owner,
        passed: false,
        offered: false,
        capacity: 0,
        encoding_ms: 0,
        upload_bps: 0,
        download_bps: 0,
        upload_bytes: 0,
        downloaded: false,
        last_seen: Instant::now(),
        generation: format!("{:032x}", rand::random::<u128>()),
        previously_closed: None,
        generation_closed: false,
        fenced: false,
    });
    Ok(Json(serde_json::json!({"id":id,"test_bytes":TEST_BYTES})))
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn download(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let owner = manager(&state, &headers).await?;
    {
        let mut registry = state.transcoding.lock().await;
        let host = registry
            .hosts
            .iter_mut()
            .find(|h| h.id == id && h.owner == owner)
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "Unknown test"))?;
        host.downloaded = true;
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_ENCODING, "identity"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        vec![0u8; TEST_BYTES],
    )
        .into_response())
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn upload(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> ApiResult<StatusCode> {
    let owner = manager(&state, &headers).await?;
    let start = Instant::now();
    if !state
        .transcoding
        .lock()
        .await
        .hosts
        .iter()
        .any(|h| h.id == id && h.owner == owner)
    {
        return Err(error(StatusCode::NOT_FOUND, "Unknown test"));
    }
    let mut stream = request.into_body().into_data_stream();
    let mut bytes = 0;
    while let Some(chunk) = tokio::time::timeout(
        Duration::from_secs(20).saturating_sub(start.elapsed()),
        stream.next(),
    )
    .await
    .map_err(|_| error(StatusCode::REQUEST_TIMEOUT, "Network test timed out"))?
    {
        bytes += chunk
            .map_err(|_| error(StatusCode::BAD_REQUEST, "Upload interrupted"))?
            .len();
        if bytes > TEST_BYTES {
            return Err(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Test upload too large",
            ));
        }
    }
    if bytes != TEST_BYTES {
        return Err(error(StatusCode::BAD_REQUEST, "Incomplete network test"));
    }
    let mut registry = state.transcoding.lock().await;
    let host = registry
        .hosts
        .iter_mut()
        .find(|h| h.id == id && h.owner == owner)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Unknown test"))?;
    host.upload_bytes = bytes;
    host.upload_bps = (bytes as f64 * 8.0 / start.elapsed().as_secs_f64().max(0.001)) as u64;
    Ok(StatusCode::NO_CONTENT)
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn complete(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(report): Json<Report>,
) -> ApiResult<Json<serde_json::Value>> {
    let owner = manager(&state, &headers).await?;
    let mut registry = state.transcoding.lock().await;
    let host = registry
        .hosts
        .iter_mut()
        .find(|h| h.id == id && h.owner == owner)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Unknown test"))?;
    host.encoding_ms = report.encoding_ms;
    host.download_bps = report.download_bps;
    host.capacity = if host.upload_bytes == TEST_BYTES && host.downloaded {
        qualification(report.encoding_ms, host.upload_bps, report.download_bps)
    } else {
        0
    };
    host.passed = host.capacity > 0;
    host.last_seen = Instant::now();
    Ok(Json(
        serde_json::json!({"passed":host.passed,"capacity":host.capacity,"encoding_ms":host.encoding_ms,"upload_bps":host.upload_bps,"download_bps":host.download_bps,"generation":host.generation}),
    ))
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn select(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    Json(selection): Json<Selection>,
) -> ApiResult<Json<serde_json::Value>> {
    manager(&state, &headers).await?;
    let mut registry = state.transcoding.lock().await;
    if let Some(id) = &selection.host_id
        && !registry.hosts.iter().any(|h| &h.id == id && eligible(h))
    {
        return Err(error(
            StatusCode::CONFLICT,
            "Host must pass both tests and be available",
        ));
    }
    registry.selected = selection.host_id;
    Ok(Json(serde_json::json!({"selected":registry.selected})))
}
// The registry retains completion observers even when an HTTP handler drops.
// Only the owned relay can publish Clean after actual child/group reap.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UploadCompletion { Pending, Clean, Unknown }
struct ActiveUpload {
    host: String,
    generation: String,
    camera: String,
    cancelled: watch::Sender<bool>,
    completion: watch::Receiver<UploadCompletion>,
}
struct CompletionGuard(watch::Sender<UploadCompletion>);
impl CompletionGuard {
    fn finish(&self, known: bool) {
        self.0.send_replace(if known { UploadCompletion::Clean } else { UploadCompletion::Unknown });
    }
}
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        let pending = *self.0.borrow() == UploadCompletion::Pending;
        if pending { self.0.send_replace(UploadCompletion::Unknown); }
    }
}
async fn completion_known(mut observer: watch::Receiver<UploadCompletion>) -> bool {
    loop {
        let state = *observer.borrow_and_update();
        match state {
            UploadCompletion::Clean => return true,
            UploadCompletion::Unknown => return false,
            UploadCompletion::Pending => {},
        }
        if observer.changed().await.is_err() { return false; }
    }
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API")]
async fn heartbeat(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(offer): Json<Offer>,
) -> ApiResult<Json<serde_json::Value>> {
    let owner = manager(&state, &headers).await?;
    let (selected, capacity, generation, stopped) = {
        let mut registry = state.transcoding.lock().await;
        let selected = registry.selected.as_deref() == Some(&id);
        let host = registry.hosts.iter_mut().find(|h| h.id == id && h.owner == owner)
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "Test this host again"))?;
        if let Some(original) = offer.generation.as_deref() {
            if original != host.generation {
                // Lost acknowledgement is retryable only before the new offer.
                if !offer.offered && !host.offered && host.previously_closed.as_deref() == Some(original) {
                    return Ok(Json(serde_json::json!({"selected":false,"jobs":[],"cleanup_complete":true,"stopped_generation":original,"generation":host.generation})));
                }
                return Err(error(StatusCode::CONFLICT, "video_processing_generation_stale"));
            }
            host.fenced = true;
        } else if host.fenced {
            return Err(error(StatusCode::CONFLICT, "video_processing_generation_required"));
        }
        if offer.offered && (!host.passed || host.generation_closed) {
            return Err(error(StatusCode::CONFLICT, "Host is not qualified or its generation is closed"));
        }
        host.offered = offer.offered;
        host.last_seen = Instant::now();
        if !offer.offered && offer.generation.is_some() { host.generation_closed = true; }
        (selected && eligible(host), host.capacity, host.generation.clone(), !offer.offered && host.fenced)
    };
    if stopped {
        let observers = {
            let registry = state.transcoding.lock().await;
            registry.uploads.values().filter(|upload| upload.host == id && upload.generation == generation)
                .map(|upload| { upload.cancelled.send_replace(true); upload.completion.clone() }).collect::<Vec<_>>()
        };
        let known = tokio::time::timeout(Duration::from_secs(5), async {
            for observer in observers {
                if !completion_known(observer).await { return false; }
            }
            true
        }).await.unwrap_or(false);
        if !known { return Err(error(StatusCode::BAD_GATEWAY, "video_processing_remote_completion_unverified")); }
        // Reauthenticate the same caller before acknowledging quiescence.
        if manager(&state, &headers).await? != owner {
            return Err(error(StatusCode::FORBIDDEN, "Host owner changed"));
        }
        let mut registry = state.transcoding.lock().await;
        let host = registry.hosts.iter_mut().find(|h| h.id == id && h.owner == owner)
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "Test this host again"))?;
        if host.generation != generation || !host.generation_closed {
            return Err(error(StatusCode::CONFLICT, "video_processing_generation_stale"));
        }
        host.previously_closed = Some(generation.clone());
        host.generation = format!("{:032x}", rand::random::<u128>());
        host.generation_closed = false;
        return Ok(Json(serde_json::json!({"selected":false,"jobs":[],"cleanup_complete":true,"stopped_generation":generation,"generation":host.generation})));
    }
    let streams = if selected { fetch_streams_raw(&state).await?.0 } else { Vec::new() };
    let jobs: Vec<_> = streams.iter().filter(|s| s.live && !s.id.starts_with("transcoded-") && s.id.len() <= 53).take(capacity)
        .map(|s| serde_json::json!({"id":s.id,"input":format!("/api/video/transcode/input/{}/index.m3u8",s.id),"output":format!("/api/video/transcode/output/{}/{}",id,s.id),"generation":if offer.generation.is_some(){Some(&generation)}else{None}})).collect();
    Ok(Json(serde_json::json!({"selected":selected,"jobs":jobs,"generation":generation})))
}
pub(super) async fn choose(state: &MediaState, streams: Vec<StreamInfo>) -> Vec<StreamInfo> {
    let registry = state.transcoding.lock().await;
    let active = registry
        .selected
        .as_ref()
        .is_some_and(|id| registry.hosts.iter().any(|h| &h.id == id && eligible(h)));
    streams
        .iter()
        .filter(|s| !s.id.starts_with("transcoded-"))
        .map(|s| {
            let encoded = format!("transcoded-{}", s.id);
            if active && let Some(output) = streams.iter().find(|v| v.id == encoded && v.live) {
                return StreamInfo {
                    id: output.id.clone(),
                    name: s.name.clone(),
                    live: true,
                };
            }
            StreamInfo {
                id: s.id.clone(),
                name: s.name.clone(),
                live: s.live,
            }
        })
        .collect()
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn output(
    State(state): State<Arc<MediaState>>,
    Path((host_id, id)): Path<(String, String)>,
    headers: HeaderMap,
    request: Request,
) -> ApiResult<StatusCode> {
    let owner = manager(&state, &headers).await?;
    check_id(&id)?;
    if id.starts_with("transcoded-") || id.len() > 53 {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid source camera"));
    }
    let ingest =
        std::env::var("GS_VIDEO_RTSP_URL").unwrap_or_else(|_| "rtsp://127.0.0.1:8554".into());
    let mut destination = reqwest::Url::parse(&ingest).map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Invalid local video receiver",
        )
    })?;
    destination
        .set_username("camera")
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "Invalid receiver"))?;
    destination
        .set_password(Some(&state.relay_password))
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "Invalid receiver"))?;
    destination.set_path(&format!("/transcoded-{id}"));
    let format = if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| value.split(';').next() == Some("video/x-matroska"))
    {
        "matroska"
    } else {
        "mpegts"
    };
    let mut command = tokio::process::Command::new("ffmpeg");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    // Reserve before spawn under the same registry lock used by Stop. A
    // queued upload cannot enter after its generation has been closed.
    let (generation, reservation, revoked) = {
        let mut registry = state.transcoding.lock().await;
        let host = registry.hosts.iter().find(|h| h.id == host_id && h.owner == owner && eligible(h) && !h.generation_closed)
            .filter(|_| registry.selected.as_deref() == Some(&host_id))
            .ok_or_else(|| error(StatusCode::CONFLICT, "Host is no longer selected"))?;
        let supplied = headers.get("X-Station-Transcode-Generation").and_then(|v| v.to_str().ok());
        if (host.fenced && supplied != Some(host.generation.as_str())) || supplied.is_some_and(|v| v != host.generation) {
            return Err(error(StatusCode::CONFLICT, "video_processing_generation_stale"));
        }
        let generation = host.generation.clone();
        let capacity = host.capacity;
        registry.uploads.retain(|_, upload| *upload.completion.borrow() != UploadCompletion::Clean);
        if registry.uploads.len() >= 256 || registry.uploads.values().filter(|upload| upload.host == host_id).count() >= capacity {
            return Err(error(StatusCode::TOO_MANY_REQUESTS, "video_processing_upload_capacity"));
        }
        if registry.uploads.values().any(|upload| upload.camera == id) {
            return Err(error(StatusCode::CONFLICT, "video_processing_previous_upload_not_reaped"));
        }
        let (cancelled, revoked) = watch::channel(false);
        let (completion, observer) = watch::channel(UploadCompletion::Pending);
        registry.uploads.insert(format!("{:032x}", rand::random::<u128>()), ActiveUpload {
            host: host_id.clone(), generation: generation.clone(), camera: id.clone(), cancelled, completion: observer,
        });
        (generation, CompletionGuard(completion), revoked)
    };
    let spawned = command
        .args([
            "-hide_banner",
            "-loglevel",
            if cfg!(test) { "warning" } else { "quiet" },
            "-nostdin",
            // Both helper formats have known codecs and one-second keyframes.
            // Bound probing so an available helper does not add a long startup wait.
            "-analyzeduration",
            "1000000",
            "-probesize",
            "1048576",
            "-f",
            format,
            "-i",
            "pipe:0",
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c",
            "copy",
            "-f",
            "rtsp",
            "-rtsp_transport",
            "tcp",
            destination.as_str(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(if cfg!(test) && std::env::var_os("GS_VIDEO_COMPLETION_TEST").is_some() {
            std::process::Stdio::null()
        } else if cfg!(test) {
            std::process::Stdio::inherit()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true)
        .spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(_) => {
            reservation.finish(true);
            return Err(error(StatusCode::SERVICE_UNAVAILABLE, "FFmpeg remuxer is unavailable"));
        }
    };
    // The HTTP future may be dropped at any await. Its sender then closes,
    // while this owned task retains the child until kill/wait completes.
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
    let authority = UploadAuthority { host_id, generation, headers, revoked, completion: reservation };
    let relay = tokio::spawn(relay_output(state, authority, request, child, cancelled));
    let result = relay.await.map_err(|_| {
        error(StatusCode::BAD_GATEWAY, "video_remux_task_failed")
    })?;
    drop(cancel_on_drop);
    result.map_err(|response| *response)
}

async fn terminate_remux(child: &mut tokio::process::Child) -> Result<(), &'static str> {
    // Signal only our still-unreaped child and its own verified Unix group.
    // An already-reaped child has no id, so a recycled pid is never signalled.
    #[cfg(unix)]
    let group_failed = if let Some(pid) = child.id() {
        // SAFETY: process_group(0) was applied before spawning this owned child.
        // Recheck the live group before any negative-pid signal.
        unsafe {
            let group = libc::getpgid(pid as libc::pid_t);
            if group != pid as libc::pid_t || group == libc::getpgrp() {
                true
            } else if libc::kill(-group, libc::SIGKILL) != 0 {
                std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            } else {
                false
            }
        }
    } else { false };
    #[cfg(not(unix))]
    let group_failed = false;
    // start_kill can fail even when wait observes a completed child. Always
    // perform the bounded wait and classify both results; never use kill? here.
    let killed = child.start_kill();
    let waited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    match waited {
        Ok(Ok(_)) if !group_failed => Ok(()),
        Ok(Ok(_)) => Err("video_remux_group_stop_failed"),
        Ok(Err(_)) if killed.is_err() => Err("video_remux_kill_and_wait_failed"),
        Ok(Err(_)) => Err("video_remux_wait_failed"),
        Err(_) => Err("video_remux_stop_timeout"),
    }
}

async fn tracked_terminate(child: &mut tokio::process::Child, known: &mut bool) -> Result<(), &'static str> {
    let result = terminate_remux(child).await;
    if result.is_err() { *known = false; }
    result
}

fn natural_remux_completion(status: std::process::ExitStatus) -> Result<StatusCode, &'static str> {
    if status.success() { Ok(StatusCode::NO_CONTENT) } else { Err("video_remux_failed") }
}

async fn original_processing_assignment(state: &MediaState, headers: &HeaderMap, host_id: &str, generation: &str) -> Result<(), Box<Response>> {
    manager(state,headers).await.map_err(Box::new)?;
    let registry=state.transcoding.lock().await;
    if registry.selected.as_deref()!=Some(host_id) || !registry.hosts.iter().any(|host|host.id==host_id&&eligible(host)&&!host.generation_closed&&host.generation==generation) {
        return Err(Box::new(error(StatusCode::CONFLICT,"video_processing_assignment_revoked")));
    }
    Ok(())
}
enum RemuxFinalization {
    Exited(std::io::Result<std::process::ExitStatus>),
    Refused(Box<Response>),
    Cancelled,
    TimedOut,
}

struct UploadAuthority {
    host_id: String,
    generation: String,
    headers: HeaderMap,
    revoked: watch::Receiver<bool>,
    completion: CompletionGuard,
}
async fn relay_output(
    state: Arc<MediaState>,
    authority: UploadAuthority,
    request: Request,
    mut child: tokio::process::Child,
    mut cancelled: tokio::sync::oneshot::Receiver<()>,
) -> Result<StatusCode, Box<Response>> {
    let UploadAuthority { host_id, generation, headers, mut revoked, completion } = authority;
    let mut cleanup_known = true;
    let outcome: ApiResult<StatusCode> = async {
        #[cfg(unix)]
        {
            let valid = child.id().is_some_and(|pid| {
                // SAFETY: this pid is held by our unreaped Child, never a saved id.
                unsafe { libc::getpgid(pid as libc::pid_t) == pid as libc::pid_t
                    && pid as libc::pid_t != libc::getpgrp() }
            });
            if !valid {
                let cleanup = tracked_terminate(&mut child, &mut cleanup_known).await;
                return Err(error(StatusCode::BAD_GATEWAY, cleanup.err().unwrap_or("video_remux_group_unverified")));
            }
        }
        let Some(mut input) = child.stdin.take() else {
            let cleanup = tracked_terminate(&mut child, &mut cleanup_known).await;
            return Err(error(StatusCode::BAD_GATEWAY, cleanup.err().unwrap_or("video_remux_pipe_missing")));
        };
        let mut stream = request.into_body().into_data_stream();
        let mut lease = tokio::time::interval(Duration::from_secs(3));
        let mut caller_dropped = false;
        let transfer = 'relay: loop {
            tokio::select! {
                biased;
                _ = &mut cancelled => { caller_dropped = true; break Ok(()); }
                _ = revoked.changed() => { break Err(error(StatusCode::CONFLICT,"video_processing_generation_revoked")); }
                status = child.wait() => {
                    break match status {
                        Ok(status) if status.success() => Err(error(StatusCode::BAD_GATEWAY, "video_remux_ended_before_input")),
                        Ok(_) => Err(error(StatusCode::BAD_GATEWAY, "video_remux_failed")),
                        Err(_) => Err(error(StatusCode::BAD_GATEWAY, "video_remux_wait_failed")),
                    };
                }
                _ = lease.tick() => {
                    let authorized = tokio::select! {
                        biased;
                        _ = &mut cancelled => { caller_dropped = true; break Ok(()); }
                _ = revoked.changed() => { break Err(error(StatusCode::CONFLICT,"video_processing_generation_revoked")); }
                        authorized = original_processing_assignment(&state,&headers,&host_id,&generation) => authorized,
                    };
                    if let Err(response)=authorized { break Err(*response); }
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            let write = input.write_all(&bytes);
                            tokio::pin!(write);
                            loop {
                            tokio::select! {
                                biased;
                                _ = &mut cancelled => { caller_dropped = true; break 'relay Ok(()); }
                                _ = revoked.changed() => { break 'relay Err(error(StatusCode::CONFLICT,"video_processing_generation_revoked")); }
                                status = child.wait() => {
                                    break 'relay match status {
                                        Ok(status) if status.success() => Err(error(StatusCode::BAD_GATEWAY, "video_remux_ended_before_input")),
                                        Ok(_) => Err(error(StatusCode::BAD_GATEWAY, "video_remux_failed")),
                                        Err(_) => Err(error(StatusCode::BAD_GATEWAY, "video_remux_wait_failed")),
                                    };
                                }
                                _ = lease.tick() => {
                                    let authorized = tokio::select! {
                                        biased;
                                        _ = &mut cancelled => { caller_dropped = true; break 'relay Ok(()); }
                                _ = revoked.changed() => { break 'relay Err(error(StatusCode::CONFLICT,"video_processing_generation_revoked")); }
                                        authorized = original_processing_assignment(&state,&headers,&host_id,&generation) => authorized,
                                    };
                                    if let Err(response)=authorized { break 'relay Err(*response); }
                                }
                                written = std::future::poll_fn(|context| {
                                    std::future::Future::poll(write.as_mut(),context)
                                }) => {
                                    if written.is_err() {
                                        break 'relay Err(error(StatusCode::BAD_GATEWAY, "video_remux_input_failed"));
                                    }
                                    break;
                                }
                            }
                            }
                        }
                        Some(Err(_)) => break Err(error(StatusCode::BAD_REQUEST, "video_upload_interrupted")),
                        None => break Ok(()),
                    }
                }
            }
        };
        // Closing stdin sends actual EOF. Neither EOF nor accepted input bytes
        // establish that the copy-remux process succeeded.
        drop(input);
        if caller_dropped || transfer.is_err() {
            let cleanup = tracked_terminate(&mut child, &mut cleanup_known).await;
            if let Err(code) = cleanup {
                return Err(error(StatusCode::BAD_GATEWAY, code));
            }
            transfer?;
            // No HTTP caller remains to receive this cancellation outcome. It must
            // never be mistaken for successful natural encoder completion.
            return Err(error(StatusCode::REQUEST_TIMEOUT, "video_upload_cancelled"));
        }
        let completion = {
            let deadline=tokio::time::sleep(Duration::from_secs(5));tokio::pin!(deadline);
            let wait=child.wait();tokio::pin!(wait);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancelled => break RemuxFinalization::Cancelled,
                    _ = revoked.changed() => break RemuxFinalization::Refused(Box::new(error(StatusCode::CONFLICT,"video_processing_generation_revoked"))),
                    _ = &mut deadline => break RemuxFinalization::TimedOut,
                    _ = lease.tick() => {
                        let authorized=tokio::select! {
                            biased;
                            _ = &mut cancelled => break RemuxFinalization::Cancelled,
                    _ = revoked.changed() => break RemuxFinalization::Refused(Box::new(error(StatusCode::CONFLICT,"video_processing_generation_revoked"))),
                            _ = &mut deadline => break RemuxFinalization::TimedOut,
                            authorized=original_processing_assignment(&state,&headers,&host_id,&generation)=>authorized,
                        };
                        if let Err(response)=authorized { break RemuxFinalization::Refused(response); }
                    }
                    status = std::future::poll_fn(|context| {
                        std::future::Future::poll(wait.as_mut(),context)
                    }) => {
                        // Revalidate the original caller/assignment before the
                        // sole success response, within the SAME deadline.
                        if status.as_ref().is_ok_and(|status|status.success()) {
                            let authorized=tokio::select! {
                                biased;
                                _ = &mut cancelled => break RemuxFinalization::Cancelled,
                    _ = revoked.changed() => break RemuxFinalization::Refused(Box::new(error(StatusCode::CONFLICT,"video_processing_generation_revoked"))),
                                _ = &mut deadline => break RemuxFinalization::TimedOut,
                                authorized=original_processing_assignment(&state,&headers,&host_id,&generation)=>authorized,
                            };
                            if let Err(response)=authorized { break RemuxFinalization::Refused(response); }
                        }
                        break RemuxFinalization::Exited(status);
                    }
                }
            }
        };
        match completion {
            RemuxFinalization::Exited(Ok(status)) => natural_remux_completion(status).map_err(|code|error(StatusCode::BAD_GATEWAY,code)),
            RemuxFinalization::Exited(Err(_)) => {
                let cleanup=tracked_terminate(&mut child, &mut cleanup_known).await;
                Err(error(StatusCode::BAD_GATEWAY,cleanup.err().unwrap_or("video_remux_wait_failed")))
            }
            RemuxFinalization::Refused(response) => {
                if let Err(code)=tracked_terminate(&mut child, &mut cleanup_known).await { return Err(error(StatusCode::BAD_GATEWAY,code)); }
                Err(*response)
            }
            RemuxFinalization::Cancelled => {
                match tracked_terminate(&mut child, &mut cleanup_known).await {
                    Ok(())=>Err(error(StatusCode::REQUEST_TIMEOUT,"video_upload_cancelled")),
                    Err(code)=>Err(error(StatusCode::BAD_GATEWAY,code)),
                }
            }
            RemuxFinalization::TimedOut => {
                match tracked_terminate(&mut child, &mut cleanup_known).await {
                    Ok(())=>Err(error(StatusCode::GATEWAY_TIMEOUT,"video_remux_timeout")),
                    Err(code)=>Err(error(StatusCode::BAD_GATEWAY,code)),
                }
            }
        }

    }.await;
    completion.finish(cleanup_known);
    outcome.map_err(Box::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualification_requires_compute_and_both_network_directions() {
        assert_eq!(qualification(1000, 30_000_000, 30_000_000), 2);
        assert_eq!(qualification(3000, 100_000_000, 100_000_000), 0);
        assert_eq!(qualification(1000, 1_000_000, 30_000_000), 0);
        assert_eq!(qualification(1000, 30_000_000, 1_000_000), 0);
        assert_eq!(qualification(0, 30_000_000, 30_000_000), 0);
    }
    #[tokio::test]
    async fn worker_selection_requires_actual_network_test_offer_and_live_lease() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let dir = TestDirectory::new();
        let app = export_state(&dir.0, true).await;
        std::fs::write(dir.0.join("auth.json"),serde_json::json!({"users":[{"username":"manager","roles":["stream_master"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},{"username":"publisher","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"camera_publish":true}}]}).to_string()).unwrap();
        for name in ["manager", "publisher"] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,0,0,9999999999999)").bind(format!("worker-{name}")).bind(name).execute(&app.auth_db).await.unwrap();
        }
        let state = Arc::new(MediaState {
            app,
            client: reqwest::Client::new(),
            api_url: String::new(),
            webrtc_url: String::new(),
            hls_url: String::new(),
            relay_password: "fixture-only".into(),
            model_write: Mutex::new(()),
            models: dir.0.clone(),
            recordings: dir.0.clone(),
            voice: Arc::new(crate::voice::VoiceHub::default()),
            presentation_write: Mutex::new(()),
            replays: Mutex::default(),
            preview_sessions: Mutex::new(Vec::new()),
            recorded_vehicle_cache: Mutex::default(),
            tickets: Mutex::new(Vec::new()),
            hls_segments: Mutex::new(std::collections::HashMap::new()),
            program_history: Mutex::new(std::collections::VecDeque::new()),
            publishers: Mutex::new(Vec::new()),
            publish_offer: Mutex::new(()),
            transcoding: Mutex::default(),
        });
        let headers = |name: &str| {
            let mut h = HeaderMap::new();
            h.insert(
                header::AUTHORIZATION,
                format!("Bearer worker-{name}").parse().unwrap(),
            );
            h
        };
        assert_eq!(
            begin(
                State(state.clone()),
                headers("publisher"),
                Json(Name {
                    name: "Forbidden".into()
                })
            )
            .await
            .unwrap_err()
            .status(),
            StatusCode::FORBIDDEN
        );
        let response = begin(
            State(state.clone()),
            headers("manager"),
            Json(Name {
                name: "Worker".into(),
            }),
        )
        .await
        .unwrap();
        let id = response.0["id"].as_str().unwrap().to_owned();
        let report = complete(
            State(state.clone()),
            Path(id.clone()),
            headers("manager"),
            Json(Report {
                encoding_ms: 1000,
                download_bps: 30_000_000,
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            report.0["passed"], false,
            "claimed rates cannot replace the actual network test"
        );
        assert_eq!(
            select(
                State(state.clone()),
                headers("manager"),
                Json(Selection {
                    host_id: Some(id.clone())
                })
            )
            .await
            .unwrap_err()
            .status(),
            StatusCode::CONFLICT
        );
        download(State(state.clone()), Path(id.clone()), headers("manager"))
            .await
            .unwrap();
        let request = Request::new(axum::body::Body::from(vec![0u8; TEST_BYTES]));
        upload(
            State(state.clone()),
            Path(id.clone()),
            headers("manager"),
            request,
        )
        .await
        .unwrap();
        let weak = complete(
            State(state.clone()),
            Path(id.clone()),
            headers("manager"),
            Json(Report {
                encoding_ms: 1000,
                download_bps: 1_000_000,
            }),
        )
        .await
        .unwrap();
        assert_eq!(weak.0["passed"], false);
        let good = complete(
            State(state.clone()),
            Path(id.clone()),
            headers("manager"),
            Json(Report {
                encoding_ms: 1000,
                download_bps: 30_000_000,
            }),
        )
        .await
        .unwrap();
        assert_eq!(good.0["passed"], true);
        assert!(
            select(
                State(state.clone()),
                headers("manager"),
                Json(Selection {
                    host_id: Some(id.clone())
                })
            )
            .await
            .is_err(),
            "qualification does not automatically offer the host"
        );
        let _ = heartbeat(
            State(state.clone()),
            Path(id.clone()),
            headers("manager"),
            Json(Offer { offered: true, generation: None }),
        )
        .await
        .unwrap();
        let _ = select(
            State(state.clone()),
            headers("manager"),
            Json(Selection {
                host_id: Some(id.clone()),
            }),
        )
        .await
        .unwrap();
        // This portable fixture has no MediaMTX API service. Keep host
        // generation controls unselected through the real manager route;
        // selected/live generation uploads are tested by the actual HTTP harness.
        let _ = select(State(state.clone()), headers("manager"), Json(Selection { host_id: None })).await.unwrap();
        // Actual authenticated route admission: close a fenced generation,
        // reject a late upload before spawning, then explicitly reoffer.
        let generation = state.transcoding.lock().await.hosts[0].generation.clone();
        let _ = heartbeat(State(state.clone()), Path(id.clone()), headers("manager"),
            Json(Offer { offered: true, generation: Some(generation.clone()) })).await.unwrap();
        let stopped = heartbeat(State(state.clone()), Path(id.clone()), headers("manager"),
            Json(Offer { offered: false, generation: Some(generation.clone()) })).await.unwrap();
        assert_eq!(stopped.0["cleanup_complete"], true);
        assert_eq!(stopped.0["stopped_generation"], generation);
        let next = stopped.0["generation"].as_str().unwrap().to_owned();
        assert_ne!(next, generation);
        let mut stale_headers = headers("manager");
        stale_headers.insert("X-Station-Transcode-Generation", generation.parse().unwrap());
        assert_eq!(output(State(state.clone()), Path((id.clone(), "front".into())), stale_headers,
            Request::new(axum::body::Body::empty())).await.unwrap_err().status(), StatusCode::CONFLICT);
        assert!(state.transcoding.lock().await.uploads.is_empty(), "late upload was rejected before reservation/spawn");
        let _ = heartbeat(State(state.clone()), Path(id.clone()), headers("manager"),
            Json(Offer { offered: true, generation: Some(next.clone()) })).await.unwrap();
        assert_eq!(heartbeat(State(state.clone()), Path(id.clone()), headers("manager"),
            Json(Offer { offered: false, generation: Some(generation) })).await.unwrap_err().status(), StatusCode::CONFLICT,
            "old close cannot stop the new offered generation");
        // Retain an actual completion observer whose owner vanished without a
        // reap acknowledgement. Unknown must never be transformed into clean.
        let (cancelled, _revoked) = watch::channel(false);
        let (finished, completion) = watch::channel(UploadCompletion::Pending);
        let unknown = CompletionGuard(finished);
        state.transcoding.lock().await.uploads.insert("unknown-owner".into(), ActiveUpload {
            host: id.clone(), generation: next.clone(), camera: "front".into(), cancelled, completion,
        });
        drop(unknown);
        assert_eq!(heartbeat(State(state.clone()), Path(id.clone()), headers("manager"),
            Json(Offer { offered: false, generation: Some(next.clone()) })).await.unwrap_err().status(), StatusCode::BAD_GATEWAY);
        assert_eq!(heartbeat(State(state.clone()), Path(id.clone()), headers("manager"),
            Json(Offer { offered: true, generation: Some(next) })).await.unwrap_err().status(), StatusCode::CONFLICT);
        state.transcoding.lock().await.hosts[0].last_seen =
            Instant::now() - LEASE - Duration::from_secs(1);
        let chosen = choose(
            &state,
            vec![
                StreamInfo {
                    id: "front".into(),
                    name: "Pad".into(),
                    live: true,
                },
                StreamInfo {
                    id: "transcoded-front".into(),
                    name: "Pad".into(),
                    live: true,
                },
            ],
        )
        .await;
        assert_eq!(chosen.len(), 1);
        assert_eq!(
            chosen[0].id, "front",
            "expired workers fall back to the original source"
        );
    }
    #[test]
    #[cfg(any(unix, windows))]
    fn natural_completion_requires_successful_process_exit() {
        #[cfg(unix)] use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)] use std::os::windows::process::ExitStatusExt;
        let successful = std::process::ExitStatus::from_raw(0);
        #[cfg(unix)] let failed = std::process::ExitStatus::from_raw(256);
        #[cfg(windows)] let failed = std::process::ExitStatus::from_raw(1);
        assert_eq!(natural_remux_completion(successful), Ok(StatusCode::NO_CONTENT));
        assert_eq!(natural_remux_completion(failed), Err("video_remux_failed"));
        #[cfg(unix)] assert_eq!(natural_remux_completion(std::process::ExitStatus::from_raw(9)), Err("video_remux_failed"));
    }

}
