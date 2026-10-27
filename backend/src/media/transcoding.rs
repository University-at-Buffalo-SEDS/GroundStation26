//! Authenticated native workers encode remotely; this station only remuxes bytes.
use super::*;
use axum::{Json, extract::Request};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Instant;
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
}
#[derive(Default)]
pub(super) struct Registry {
    hosts: Vec<Host>,
    selected: Option<String>,
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
    });
    Ok(Json(serde_json::json!({"id":id,"test_bytes":TEST_BYTES})))
}
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
        serde_json::json!({"passed":host.passed,"capacity":host.capacity,"encoding_ms":host.encoding_ms,"upload_bps":host.upload_bps,"download_bps":host.download_bps}),
    ))
}
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
async fn heartbeat(
    State(state): State<Arc<MediaState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(offer): Json<Offer>,
) -> ApiResult<Json<serde_json::Value>> {
    let owner = manager(&state, &headers).await?;
    let (selected, capacity) = {
        let mut registry = state.transcoding.lock().await;
        let selected = registry.selected.as_deref() == Some(&id);
        let host = registry
            .hosts
            .iter_mut()
            .find(|h| h.id == id && h.owner == owner)
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "Test this host again"))?;
        if offer.offered && !host.passed {
            return Err(error(
                StatusCode::CONFLICT,
                "Host has not passed qualification",
            ));
        }
        host.offered = offer.offered;
        host.last_seen = Instant::now();
        (selected && eligible(host), host.capacity)
    };
    let streams = if selected {
        fetch_streams_raw(&state).await?.0
    } else {
        Vec::new()
    };
    let jobs:Vec<_>=streams.iter().filter(|s|s.live&&!s.id.starts_with("transcoded-")&&s.id.len()<=53).take(capacity).map(|s|serde_json::json!({"id":s.id,"input":format!("/api/video/transcode/input/{}/index.m3u8",s.id),"output":format!("/api/video/transcode/output/{}/{}",id,s.id)})).collect();
    Ok(Json(serde_json::json!({"selected":selected,"jobs":jobs})))
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
async fn output(
    State(state): State<Arc<MediaState>>,
    Path((host_id, id)): Path<(String, String)>,
    headers: HeaderMap,
    request: Request,
) -> ApiResult<StatusCode> {
    let owner = manager(&state, &headers).await?;
    check_id(&id)?;
    {
        let registry = state.transcoding.lock().await;
        if registry.selected.as_deref() != Some(&host_id)
            || !registry
                .hosts
                .iter()
                .any(|h| h.id == host_id && h.owner == owner && eligible(h))
        {
            return Err(error(StatusCode::CONFLICT, "Host is no longer selected"));
        }
    }
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
    let mut child = tokio::process::Command::new("ffmpeg")
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
        .stderr(if cfg!(test) {
            std::process::Stdio::inherit()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "FFmpeg remuxer is unavailable",
            )
        })?;
    let mut input = child.stdin.take().unwrap();
    let mut stream = request.into_body().into_data_stream();
    let mut lease = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            chunk=stream.next()=>{match chunk{Some(Ok(bytes))=>input.write_all(&bytes).await.map_err(|_|error(StatusCode::BAD_GATEWAY,"Remuxer stopped"))?,Some(Err(_))=>break,None=>break}},
            _=lease.tick()=>{manager(&state,&headers).await?;let registry=state.transcoding.lock().await;if registry.selected.as_deref()!=Some(&host_id)||!registry.hosts.iter().any(|h|h.id==host_id&&eligible(h)){break;}}
        }
    }
    let _ = input.shutdown().await;
    drop(input);
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    Ok(StatusCode::NO_CONTENT)
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
            Json(Offer { offered: true }),
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
}
