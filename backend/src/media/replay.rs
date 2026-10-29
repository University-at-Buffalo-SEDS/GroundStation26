//! Bounded, encoded replay clips. Never retain a second decoded video in RAM.
use super::*;
use std::collections::HashSet;
use tokio::io::AsyncWriteExt;

const CLIP_LIMIT: u64 = 256 * 1024 * 1024;
const FILE_LIMIT: u64 = 8 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct Camera {
    pub id: String,
    #[serde(default = "center")]
    pub x: f32,
    #[serde(default = "center")]
    pub y: f32,
    #[serde(default = "one")]
    pub zoom: f32,
}
fn center() -> f32 {
    50.0
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct Plan {
    #[serde(default)]
    pub id: String,
    pub duration_seconds: u32,
    pub speed: f32,
    pub cameras: Vec<Camera>,
    #[serde(default)]
    pub started_at_ms: u64,
    #[serde(default)]
    pub source_end_ms: u64,
}
impl Plan {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=20).contains(&self.duration_seconds)
            || ![0.25, 0.5, 1.0].contains(&self.speed)
            || self.cameras.is_empty()
            || self.cameras.len() > 8
        {
            return Err("Choose 1–8 cameras, 1–20 seconds and 0.25x, 0.5x or 1x playback");
        }
        let mut ids = HashSet::new();
        for c in &self.cameras {
            if !valid_id(&c.id)
                || !ids.insert(&c.id)
                || !c.x.is_finite()
                || !c.y.is_finite()
                || !(0.0..=100.0).contains(&c.x)
                || !(0.0..=100.0).contains(&c.y)
                || !c.zoom.is_finite()
                || !(1.0..=3.0).contains(&c.zoom)
            {
                return Err("Invalid replay camera or framing");
            }
        }
        Ok(())
    }
    pub fn active(&self, now: u64) -> bool {
        self.started_at_ms > 0
            && now >= self.started_at_ms
            && (now - self.started_at_ms) as f64 * (self.speed as f64)
                < self.duration_seconds as f64 * 1000.0
    }
    pub fn target(&self, now: u64) -> u64 {
        self.source_end_ms
            .saturating_sub(self.duration_seconds as u64 * 1000)
            .saturating_add(
                ((now.saturating_sub(self.started_at_ms)) as f64 * self.speed as f64) as u64,
            )
            .min(self.source_end_ms)
    }
}
#[derive(Clone)]
struct Session {
    plan: Plan,
    dir: PathBuf,
    files: HashSet<(String, String)>,
    telemetry: Vec<(u64, serde_json::Value)>,
}
#[derive(Default)]
pub(crate) struct ReplayStore {
    sessions: Vec<Session>,
}

pub(super) async fn available(state: &MediaState, plan: &Plan) -> bool {
    plan.active(program::now_ms())
        && state
            .replays
            .lock()
            .await
            .sessions
            .iter()
            .any(|s| s.plan.id == plan.id)
}

pub(super) async fn telemetry(
    state: &MediaState,
    id: &str,
    target: u64,
) -> Option<serde_json::Value> {
    let store = state.replays.lock().await;
    store
        .sessions
        .iter()
        .find(|s| s.plan.id == id)?
        .telemetry
        .iter()
        .rev()
        .find(|(t, _)| *t <= target && target.saturating_sub(*t) < 2000)
        .map(|(_, v)| v.clone())
}

// Require continuous, timestamped coverage of the whole requested interval.
fn clip_playlist(text: &str, start: u64, end: u64) -> Result<(String, Vec<String>), ()> {
    let (text, entries) = program::delayed_playlist(text, end.saturating_add(2000))?;
    let ends: std::collections::HashMap<_, _> = entries.into_iter().collect();
    let mut out = vec![
        "#EXTM3U".to_owned(),
        "#EXT-X-VERSION:7".into(),
        "#EXT-X-TARGETDURATION:30".into(),
        "#EXT-X-PLAYLIST-TYPE:VOD".into(),
    ];
    let mut files = Vec::new();
    let mut duration = 0.0;
    let mut pending = Vec::new();
    let mut covered = None;
    let mut last = 0;
    for line in text.lines() {
        if line.starts_with("#EXT-X-MAP:") {
            let name = line
                .split_once("URI=\"")
                .ok_or(())?
                .1
                .split_once('"')
                .ok_or(())?
                .0;
            let local = program::parse_hls_uri(name)?;
            files.push(name.to_string());
            out.push(line.replace(name, local));
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            duration = v
                .split(',')
                .next()
                .ok_or(())?
                .parse::<f64>()
                .map_err(|_| ())?;
            pending.push(line.to_string());
        } else if line.starts_with("#EXT-X-PROGRAM-DATE-TIME:") {
            pending.push(line.to_string());
        } else if line == "#EXT-X-DISCONTINUITY" {
            return Err(());
        } else if !line.is_empty() && !line.starts_with('#') {
            let local = program::parse_hls_uri(line)?;
            let stop = *ends.get(local).ok_or(())?;
            let begin = stop
                .checked_sub((duration * 1000.0).round() as u64)
                .ok_or(())?;
            if stop > start && begin < end {
                if covered.is_some() {
                    if begin > last + 5 || stop <= last {
                        return Err(());
                    }
                } else {
                    if begin > start {
                        return Err(());
                    }
                    covered = Some(begin);
                    if !pending
                        .iter()
                        .any(|v: &String| v.starts_with("#EXT-X-PROGRAM-DATE-TIME:"))
                    {
                        let date = time::OffsetDateTime::from_unix_timestamp_nanos(
                            begin as i128 * 1_000_000,
                        )
                        .map_err(|_| ())?
                        .format(&time::format_description::well_known::Rfc3339)
                        .map_err(|_| ())?;
                        out.push(format!("#EXT-X-PROGRAM-DATE-TIME:{date}"));
                    }
                }
                out.append(&mut pending);
                out.push(local.into());
                files.push(line.into());
                last = stop;
            } else {
                pending.clear();
            }
        }
    }
    if covered.is_none() || last < end {
        return Err(());
    }
    out.push("#EXT-X-ENDLIST".into());
    files.sort();
    files.dedup();
    let names = files
        .iter()
        .map(|f| program::parse_hls_uri(f))
        .collect::<Result<HashSet<_>, _>>()?;
    if names.len() != files.len() {
        return Err(());
    }
    Ok((out.join("\n") + "\n", files))
}
async fn upstream(state: &MediaState, camera: &str, file: &str) -> ApiResult<reqwest::Response> {
    let response = state
        .client
        .get(format!(
            "{}/{}/{}",
            state.hls_url.trim_end_matches('/'),
            camera,
            file
        ))
        .basic_auth("groundstation", Some(&state.relay_password))
        .send()
        .await
        .map_err(upstream_error)?;
    if !response.status().is_success() {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Replay footage is not available yet",
        ));
    }
    Ok(response)
}
async fn playlist(state: &MediaState, camera: &str, file: &str) -> ApiResult<String> {
    let mut response = upstream(state, camera, file).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(upstream_error)? {
        if bytes.len() + chunk.len() > 512 * 1024 {
            return Err(error(StatusCode::BAD_GATEWAY, "Playlist too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| error(StatusCode::BAD_GATEWAY, "Invalid HLS playlist"))
}
pub(super) async fn prepare(
    state: &MediaState,
    broadcast: &mut Broadcast,
    previous: &Broadcast,
) -> ApiResult<()> {
    let Some(plan) = broadcast.replay.as_mut() else {
        return Ok(());
    };
    plan.validate()
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    if let Some(old) = &previous.replay {
        if !plan.id.is_empty() && plan.id == old.id {
            // Reframe and reorder only footage captured for this session.
            if plan.duration_seconds != old.duration_seconds
                || plan.speed != old.speed
                || plan
                    .cameras
                    .iter()
                    .any(|c| !old.cameras.iter().any(|o| o.id == c.id))
            {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "Start a new replay to change duration, speed or camera sources",
                ));
            }
            plan.started_at_ms = old.started_at_ms;
            plan.source_end_ms = old.source_end_ms;
            return Ok(());
        }
    }
    let now = program::now_ms();
    plan.source_end_ms = now.saturating_sub(broadcast.delay_seconds as u64 * 1000 + 2500);
    plan.id = format!("r{now}-{}", previous.revision);
    // Slow motion must not age its source telemetry out of the live ring.
    let telemetry = {
        let history = state.program_history.lock().await;
        let start = plan
            .source_end_ms
            .saturating_sub(plan.duration_seconds as u64 * 1000 + 2000);
        history
            .iter()
            .filter(|(t, _)| *t >= start && *t <= plan.source_end_ms + 2000)
            .take(128)
            .cloned()
            .collect()
    };

    let root = state.models.join("_replays");
    let dir = root.join(&plan.id);
    tokio::fs::create_dir_all(&dir).await.map_err(io_error)?;
    let result=async {
        let mut files=HashSet::new();let mut total=0u64;
        for camera in &plan.cameras {
            let mut text=playlist(state,&camera.id,"index.m3u8").await?;
            if !text.contains("#EXTINF:") {
                let child=text.lines().find(|line|!line.is_empty()&&!line.starts_with('#')).ok_or_else(||error(StatusCode::SERVICE_UNAVAILABLE,"Replay playlist is empty"))?;
                program::parse_hls_uri(child).map_err(|_|error(StatusCode::BAD_GATEWAY,"Unsafe playlist URI"))?;
                text=playlist(state,&camera.id,child).await?;
            }
            let (clip,assets)=clip_playlist(&text,plan.source_end_ms.saturating_sub(plan.duration_seconds as u64*1000),plan.source_end_ms)
                .map_err(|_|error(StatusCode::CONFLICT,"Not enough continuous buffered footage for this replay; shorten the clip or wait"))?;
            let camera_dir=dir.join(&camera.id);tokio::fs::create_dir_all(&camera_dir).await.map_err(io_error)?;
            tokio::fs::write(camera_dir.join("index.m3u8"),clip).await.map_err(io_error)?;
            files.insert((camera.id.clone(),"index.m3u8".into()));
            for file in assets {
                let mut response=upstream(state,&camera.id,&file).await?;
                if response.content_length().is_some_and(|n|n>FILE_LIMIT || n>CLIP_LIMIT.saturating_sub(total)) {
                    return Err(error(StatusCode::PAYLOAD_TOO_LARGE,"Replay is too large. Select fewer cameras or a shorter clip."));
                }
                let local=program::parse_hls_uri(&file).map_err(|_|error(StatusCode::BAD_GATEWAY,"Unsafe replay asset"))?.to_owned();
                let mut output=tokio::fs::File::create(camera_dir.join(&local)).await.map_err(io_error)?;let mut size=0u64;
                while let Some(chunk)=response.chunk().await.map_err(upstream_error)? {
                    size+=chunk.len() as u64;total+=chunk.len() as u64;
                    if size>FILE_LIMIT || total>CLIP_LIMIT {return Err(error(StatusCode::PAYLOAD_TOO_LARGE,"Replay is too large. Select fewer cameras or a shorter clip."));}
                    output.write_all(&chunk).await.map_err(io_error)?;
                }
                output.flush().await.map_err(io_error)?;files.insert((camera.id.clone(),local));
            }
        }
        Ok::<_,Response>(files)
    }.await;
    let files = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(e);
        }
    };
    plan.started_at_ms = program::now_ms();
    let keep = {
        let mut store = state.replays.lock().await;
        store.sessions.push(Session {
            plan: plan.clone(),
            dir: dir.clone(),
            files,
            telemetry,
        });
        while store.sessions.len() > 2 {
            store.sessions.remove(0);
        }
        store
            .sessions
            .iter()
            .map(|s| s.plan.id.clone())
            .collect::<HashSet<_>>()
    };
    // Bound disk usage across sessions and clean clips abandoned by a restart.
    let mut dirs = tokio::fs::read_dir(root).await.map_err(io_error)?;
    while let Some(entry) = dirs.next_entry().await.map_err(io_error)? {
        if entry.file_type().await.map_err(io_error)?.is_dir()
            && !keep.contains(&entry.file_name().to_string_lossy().to_string())
        {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
    Ok(())
}
pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new().route("/api/media-assets/replay/{session}/{id}/{file}", get(asset))
}
async fn asset(
    State(state): State<Arc<MediaState>>,
    Path((session, id, file)): Path<(String, String, String)>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    check_id(&id)?;
    check_id(&session)?;
    let headers = ticket_headers(&state, &headers, &query, &format!("stream:{id}")).await?;
    authorize(&state, &headers, Permission::ViewData).await?;
    let path = {
        let store = state.replays.lock().await;
        let clip = store
            .sessions
            .iter()
            .find(|s| {
                s.plan.id == session
                    && program::now_ms().saturating_sub(s.plan.started_at_ms) < 120000
            })
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "Replay expired"))?;
        if !clip.files.contains(&(id.clone(), file.clone())) {
            return Err(error(StatusCode::NOT_FOUND, "Unknown replay segment"));
        }
        clip.dir.join(&id).join(&file)
    };
    if file != "index.m3u8" {
        // Stream encoded files in bounded chunks, including HTTP range support.
        let mut request = axum::http::Request::new(axum::body::Body::empty());
        *request.headers_mut() = headers;
        let mut response = tower_http::services::ServeFile::new(path)
            .try_call(request)
            .await
            .map_err(io_error)?
            .into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "video/mp4".parse().unwrap());
        return Ok(response);
    }
    let mut bytes = tokio::fs::read(path).await.map_err(io_error)?;
    let content = if file == "index.m3u8" {
        let token = ticket(&state, &headers, &format!("stream:{id}")).await?;
        bytes = program::rewrite_playlist(
            std::str::from_utf8(&bytes)
                .map_err(|_| error(StatusCode::BAD_GATEWAY, "Invalid replay playlist"))?,
            &token,
        )
        .map_err(|_| error(StatusCode::BAD_GATEWAY, "Unsafe replay playlist"))?
        .into_bytes();
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp4"
    };
    Ok((
        [
            (header::CONTENT_TYPE, content),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan() -> Plan {
        Plan {
            id: "clip".into(),
            duration_seconds: 20,
            speed: 0.25,
            cameras: vec![Camera {
                id: "pad".into(),
                x: 50.,
                y: 50.,
                zoom: 1.,
            }],
            started_at_ms: 1000,
            source_end_ms: 30000,
        }
    }
    #[test]
    fn replay_clock_bounds_and_frames() {
        let mut p = plan();
        assert!(p.validate().is_ok());
        assert_eq!(p.target(5000), 11000);
        assert!(p.active(80999));
        assert!(!p.active(81000));
        p.duration_seconds = 21;
        assert!(p.validate().is_err());
        p.duration_seconds = 20;
        p.cameras[0].zoom = f32::NAN;
        assert!(p.validate().is_err());
        p.cameras[0].zoom = 1.;
        p.cameras.push(p.cameras[0].clone());
        assert!(p.validate().is_err());
    }
    #[test]
    fn retained_playlist_requires_whole_clip_and_ends() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-PROGRAM-DATE-TIME:2026-10-20T00:00:00Z\n#EXTINF:1,\na.mp4\n#EXTINF:1,\nb.mp4\n#EXTINF:1,\nc.mp4\n";
        let t = 1792454400000u64;
        let (clip, files) = clip_playlist(text, t + 1000, t + 3000).unwrap();
        assert!(!clip.contains("a.mp4"));
        assert!(clip.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(files, vec!["b.mp4", "c.mp4", "init.mp4"]);
        assert!(clip_playlist(text, t - 1000, t + 3000).is_err());
        assert!(clip_playlist(text, t, t + 4000).is_err());
    }
    #[tokio::test]
    async fn clips_survive_upstream_loss_and_require_scoped_authorization() {
        let dir = std::env::temp_dir().join(format!("gs-replay-test-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let users = dir.join("users.json");
        std::fs::write(&users,serde_json::to_vec(&serde_json::json!({"anonymous":{"view_data":false},"users":[{"username":"viewer","roles":["stream_viewer"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}]})).unwrap()).unwrap();
        let mut app = crate::state::tests::test_app_state().await;
        Arc::get_mut(&mut app).unwrap().auth = Arc::new(crate::auth::AuthManager::new(users));
        crate::ensure_auth_sessions_table(&app.auth_db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,allowed_commands_json,created_at_ms,expires_at_ms) VALUES('replay-viewer','viewer','session',1,0,'[]',0,9999999999999)").execute(&app.auth_db).await.unwrap();
        let upstream=Router::new().route("/{id}/{file}",get(|Path((id,file)):Path<(String,String)>,headers:HeaderMap,Query(query):Query<std::collections::HashMap<String,String>>|async move {
            assert_eq!(headers[header::AUTHORIZATION],"Basic Z3JvdW5kc3RhdGlvbjpzZWNyZXQ=");
            if file!="index.m3u8" {assert_eq!(query.get("session").map(String::as_str),Some("fixture"));}
            if id=="missing" {return StatusCode::NOT_FOUND.into_response();}
            if file=="index.m3u8" {return "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100000\nmedia.m3u8?session=fixture\n".into_response();}
            if file=="media.m3u8" {
                let start=program::now_ms()/1000*1000-40000;
                let mut text=String::from("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4?session=fixture\"\n");
                for i in 0..40 {let time=time::OffsetDateTime::from_unix_timestamp_nanos((start+i*1000) as i128*1000000).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap();text.push_str(&format!("#EXT-X-PROGRAM-DATE-TIME:{time}\n#EXTINF:1,\nseg{i}.m4s?session=fixture\n"));}
                return text.into_response();
            }
            format!("encoded:{id}:{file}").into_response()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let state = Arc::new(MediaState {
            app,
            client: reqwest::Client::new(),
            api_url: String::new(),
            webrtc_url: String::new(),
            hls_url: format!("http://{address}"),
            relay_password: "secret".into(),
            models: dir.clone(),
            recordings: dir.clone(),
            model_write: Mutex::new(()),
            voice: Arc::new(crate::voice::VoiceHub::default()),
            presentation_write: Mutex::new(()),
            tickets: Mutex::new(Vec::new()),
            replays: Mutex::new(Default::default()),
            program_history: Mutex::new(Default::default()),
            hls_segments: Mutex::new(Default::default()),
            preview_sessions: Mutex::new(Vec::new()),
            publishers: Mutex::new(Vec::new()),
            publish_offer: Mutex::new(()),
            transcoding: Mutex::default(),
            recorded_vehicle_cache: Mutex::new(Default::default()),
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer replay-viewer".parse().unwrap(),
        );
        let previous = Broadcast {
            delay_seconds: 3,
            hidden_stream_ids: vec!["hidden".into()],
            ..Default::default()
        };
        let mut broadcast = previous.clone();
        let mut p = plan();
        p.id.clear();
        p.cameras = vec![
            Camera {
                id: "hidden".into(),
                x: 20.,
                y: 70.,
                zoom: 2.,
            },
            Camera {
                id: "pad".into(),
                x: 50.,
                y: 50.,
                zoom: 1.,
            },
        ];
        broadcast.replay = Some(p);
        assert_eq!(
            super::super::control_broadcast(
                State(state.clone()),
                headers.clone(),
                axum::Json(broadcast.clone())
            )
            .await
            .err()
            .unwrap()
            .status(),
            StatusCode::FORBIDDEN,
            "viewers cannot control replay"
        );
        {
            let mut history = state.program_history.lock().await;
            let now = program::now_ms();
            for age in (0..90).rev() {
                history.push_back((now - age * 1000, serde_json::json!({"frozen":true})));
            }
        }
        prepare(&state, &mut broadcast, &previous).await.unwrap();
        let session = broadcast.replay.clone().unwrap();
        assert!(available(&state, &session).await);
        state.program_history.lock().await.clear();
        assert_eq!(
            telemetry(&state, &session.id, session.source_end_ms - 10000)
                .await
                .unwrap()["frozen"],
            true,
            "replay telemetry survives live-ring eviction"
        );
        assert!(state.replays.lock().await.sessions[0].telemetry.len() <= 128);

        let store = state.replays.lock().await;
        let clip = &store.sessions[0];
        assert!(clip.files.iter().any(|(id, _)| id == "hidden"));
        assert!(clip.files.iter().any(|(id, _)| id == "pad"));
        drop(store);
        let mut failed = broadcast.clone();
        failed.replay.as_mut().unwrap().id.clear();
        failed.replay.as_mut().unwrap().cameras[0].id = "missing".into();
        assert!(prepare(&state, &mut failed, &broadcast).await.is_err());
        assert_eq!(
            state.replays.lock().await.sessions.len(),
            1,
            "failed capture preserves the current clip"
        );
        task.abort(); // Retained clips no longer depend on the rolling HLS source.
        let token = ticket(&state, &headers, "stream:hidden").await.unwrap();
        let response = asset(
            State(state.clone()),
            Path((session.id.clone(), "hidden".into(), "index.m3u8".into())),
            Query(MediaQuery {
                ticket: Some(token.clone()),
                ..Default::default()
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("#EXT-X-ENDLIST"));
        assert!(text.contains("ticket="));
        assert!(!text.contains("session="));
        let response = asset(
            State(state.clone()),
            Path((session.id.clone(), "hidden".into(), "init.mp4".into())),
            Query(MediaQuery {
                ticket: Some(token.clone()),
                ..Default::default()
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            b"encoded:hidden:init.mp4"
        );
        assert!(
            asset(
                State(state.clone()),
                Path((session.id.clone(), "pad".into(), "init.mp4".into())),
                Query(MediaQuery {
                    ticket: Some(token),
                    ..Default::default()
                }),
                HeaderMap::new()
            )
            .await
            .is_err(),
            "ticket cannot cross cameras"
        );
        assert!(
            asset(
                State(state.clone()),
                Path((session.id, "hidden".into(), "../users.json".into())),
                Query(MediaQuery::default()),
                headers
            )
            .await
            .is_err()
        );
        let mut range_headers = HeaderMap::new();
        range_headers.insert(
            header::AUTHORIZATION,
            "Bearer replay-viewer".parse().unwrap(),
        );
        range_headers.insert(header::RANGE, "bytes=0-6".parse().unwrap());
        let clip_id = state.replays.lock().await.sessions[0].plan.id.clone();
        let ranged = asset(
            State(state.clone()),
            Path((clip_id, "hidden".into(), "init.mp4".into())),
            Query(MediaQuery::default()),
            range_headers,
        )
        .await
        .unwrap();
        assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            axum::body::to_bytes(ranged.into_body(), 32)
                .await
                .unwrap()
                .as_ref(),
            b"encoded"
        );
        *state.app.state.lock().unwrap() = crate::types::FlightState::Launch;
        let presentation = read_presentation(&state).await.unwrap();
        let snapshot = program::telemetry_snapshot(&state.app, &presentation, program::now_ms());
        let fill = snapshot["stats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["label"] == "Fill mass");
        assert_eq!(fill, cfg!(feature = "test_fire_mode"));
        assert_eq!(snapshot["test_fire_mode"], cfg!(feature = "test_fire_mode"));
        if cfg!(feature = "test_fire_mode") {
            let thrust = presentation
                .stats
                .iter()
                .find(|s| s.label == "Thrust")
                .unwrap();
            assert_eq!(thrust.unit, "N");
            assert!((thrust.binding.scale - 9.80665).abs() < 0.00001);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
