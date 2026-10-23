//! Viewer program uses retained HLS; live previews remain WebRTC.
use super::*;
use crate::auth::StreamRole;
use axum::Json;

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route("/api/media-assets/program", get(page))
        .route(
            "/assets/program-art.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    gs_browser_assets::PROGRAM_ART_JS,
                )
            }),
        )
        .route("/api/media-assets/program/state", get(program_state))
        .route("/api/media-assets/program/audio", get(program_audio))
        .route(
            "/assets/program-audio.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    gs_browser_assets::PROGRAM_AUDIO_JS,
                )
            }),
        )
        .route("/api/media-assets/hls/{id}/{file}", get(hls))
        .route("/api/stream-roles", get(roles).post(set_role))
        .route("/api/dashboard_status", get(dashboard_status))
        .route(
            "/assets/hls.min.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    gs_browser_assets::HLS_JS,
                )
            }),
        )
}
async fn page(
    State(state): State<Arc<MediaState>>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authorize_media(&state, &headers, &query, "program").await?;
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(gs_browser_assets::PROGRAM_HTML),
    )
        .into_response())
}
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn program_t_clock(clock: &crate::telemetry_db::LaunchClockMsg, now: i64) -> Option<String> {
    use crate::telemetry_db::LaunchClockKind;
    let (sign, delta) = match clock.kind {
        LaunchClockKind::Idle => (
            "−",
            clock
                .duration_ms
                .unwrap_or(crate::state::LAUNCH_COUNTDOWN_DURATION_MS),
        ),
        LaunchClockKind::TMinus => (
            "−",
            match (clock.anchor_timestamp_ms, clock.duration_ms) {
                (Some(anchor), Some(duration)) => {
                    duration.saturating_sub(now.saturating_sub(anchor))
                }
                (Some(target), None) => target.saturating_sub(now),
                (None, Some(duration)) => duration,
                _ => return None,
            },
        ),
        LaunchClockKind::TPlus => ("+", now.saturating_sub(clock.anchor_timestamp_ms?)),
    };
    let centis = delta.max(0).saturating_add(5) / 10;
    Some(format!(
        "T{sign} {:02}:{:02}.{:02}",
        centis / 6000,
        centis / 100 % 60,
        centis % 100
    ))
}
const PROGRAM_HISTORY_MS: u64 = 90_000;
const PROGRAM_SAMPLE_MS: u64 = 250;
const MAX_PROGRAM_SNAPSHOTS: usize = 364;

fn retain_program_snapshot(
    history: &mut std::collections::VecDeque<(u64, serde_json::Value)>,
    now: u64,
    snapshot: serde_json::Value,
) {
    // A UTC correction cannot leave future frames or unbounded retained data.
    if history.back().is_some_and(|(time, _)| *time > now) {
        history.clear();
    }
    if history
        .back()
        .is_none_or(|(time, _)| now.saturating_sub(*time) >= PROGRAM_SAMPLE_MS)
    {
        history.push_back((now, snapshot));
    }
    while history.len() > MAX_PROGRAM_SNAPSHOTS
        || history
            .front()
            .is_some_and(|(time, _)| now.saturating_sub(*time) > PROGRAM_HISTORY_MS)
    {
        history.pop_front();
    }
}

fn delayed_program_snapshot(
    history: &std::collections::VecDeque<(u64, serde_json::Value)>,
    target: u64,
) -> Option<serde_json::Value> {
    history
        .iter()
        .rev()
        .find(|(time, _)| *time <= target)
        .filter(|(time, _)| target.saturating_sub(*time) < 2000)
        .map(|(_, value)| value.clone())
}

pub(super) async fn capture_history(state: Arc<MediaState>) {
    let mut shutdown = state.app.shutdown_subscribe();
    let mut timer = tokio::time::interval(Duration::from_millis(PROGRAM_SAMPLE_MS));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = timer.tick() => {},
        }
        let Ok(presentation) = read_presentation(&state).await else {
            continue;
        };
        let now = now_ms();
        let snapshot = telemetry_snapshot(&state.app, &presentation, now);
        retain_program_snapshot(&mut *state.program_history.lock().await, now, snapshot);
    }
}

async fn program_state(
    State(state): State<Arc<MediaState>>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let headers = ticket_headers(&state, &headers, &query, "program").await?;
    authorize(&state, &headers, Permission::ViewData).await?;
    let presentation = read_presentation(&state).await?;
    let mut streams = Vec::new();
    let available = fetch_streams(&state).await.unwrap_or_default().0;
    let chosen: Vec<String> = if let Some(replay) = &presentation.broadcast.replay {
        replay.cameras.iter().map(|c| c.id.clone()).collect()
    } else {
        available
            .iter()
            .filter(|s| s.live && !presentation.broadcast.hidden_stream_ids.contains(&s.id))
            .take(8)
            .map(|s| s.id.clone())
            .collect()
    };
    for id in chosen {
        let access = ticket(&state, &headers, &format!("stream:{id}")).await?;
        let url = if let Some(replay) = &presentation.broadcast.replay {
            format!(
                "/api/media-assets/replay/{}/{id}/index.m3u8?ticket={access}",
                replay.id
            )
        } else {
            format!("/api/media-assets/hls/{id}/index.m3u8?ticket={access}")
        };
        streams.push(serde_json::json!({"id":id,"label":presentation.stream_labels.get(&id).unwrap_or(&id),"url":url}));
    }
    let now = now_ms();
    let history = state.program_history.lock().await;
    let target = presentation
        .broadcast
        .replay
        .as_ref()
        .map(|p| p.target(now))
        .unwrap_or_else(|| {
            now.saturating_sub(presentation.broadcast.delay_seconds as u64 * 1000 + 2500)
        });
    let live_telemetry = delayed_program_snapshot(&history, target);
    drop(history);
    let telemetry = if let Some(plan) = &presentation.broadcast.replay {
        super::replay::telemetry(&state, &plan.id, target).await
    } else {
        live_telemetry
    };
    Ok(([(header::CACHE_CONTROL,"no-store")],Json(serde_json::json!({"broadcast":presentation.broadcast,"streams":streams,"server_now_ms":now,"telemetry":telemetry}))).into_response())
}

#[derive(Deserialize)]
struct AudioCursor {
    #[serde(default)]
    after: u64,
}

async fn program_audio(
    State(state): State<Arc<MediaState>>,
    Query(query): Query<MediaQuery>,
    Query(cursor): Query<AudioCursor>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authorize_media(&state, &headers, &query, "program").await?;
    let broadcast = read_presentation(&state).await?.broadcast;
    let batch = if broadcast.comms_audio_enabled && broadcast.replay.is_none() {
        state
            .voice
            .audience_batch(cursor.after, broadcast.delay_seconds)
            .await
    } else {
        serde_json::json!({"enabled":false,"generation":0,"cursor":0,"frames":[]})
    };
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(batch)).into_response())
}

async fn dashboard_status(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authorize(&state, &headers, Permission::ViewData).await?;
    let presentation = read_presentation(&state).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(telemetry_snapshot(&state.app, &presentation, now_ms())),
    )
        .into_response())
}
pub(super) fn telemetry_snapshot(
    app: &AppState,
    presentation: &Presentation,
    now: u64,
) -> serde_json::Value {
    let phase = *app.state.lock().unwrap();
    let fill_visible = cfg!(feature = "test_fire_mode")
        || matches!(
            phase,
            crate::types::FlightState::Startup
                | crate::types::FlightState::Idle
                | crate::types::FlightState::PreFill
                | crate::types::FlightState::FillTest
                | crate::types::FlightState::NitrogenFill
                | crate::types::FlightState::NitrousFill
                | crate::types::FlightState::Armed
                | crate::types::FlightState::Aborted
        );
    let stats: Vec<_> = {
        let rows = app.recent_telemetry_cache.lock().unwrap();
        presentation.stats.iter().filter(|stat|fill_visible || !matches!(stat.binding.data_type.as_str(),"KG50"|"KG1000"|"LOADCELL_WEIGHT_KG"|"LOADCELL_50_WEIGHT_KG"|"LOADCELL_FILL_PERCENT"|"FUEL_TANK_PRESSURE"|"PRESSURE_TRANSDUCER_CALIBRATED"|"VALVE_STATE")).map(|stat|{
            let b=&stat.binding;
            let value=rows.iter().rev().find(|r|r.data_type==b.data_type && b.sender_id.as_ref().is_none_or(|s|s==&r.sender_id))
                .filter(|r|r.timestamp_ms>=0 && now.saturating_sub(r.timestamp_ms as u64)<5000)
                .and_then(|r|r.values.get(b.index).copied().flatten()).map(|v|v*b.scale+b.offset).filter(|v|v.is_finite());
            serde_json::json!({"label":stat.label,"value":value,"unit":stat.unit,"precision":stat.precision.min(6),"binding":b})
        }).collect()
    };
    serde_json::json!({"phase":format!("{phase:?}"),"test_fire_mode":cfg!(feature="test_fire_mode"),"stats":stats,"t_clock":program_t_clock(&app.launch_clock_snapshot(),now.min(i64::MAX as u64) as i64)})
}

fn valid_hls_file(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 180
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && [".m3u8", ".mp4", ".m4s", ".ts"]
            .iter()
            .any(|s| name.ends_with(s))
}
pub(super) fn rewrite_playlist(text: &str, ticket: &str) -> Result<String, ()> {
    let uri = |v: &str| -> Result<String, ()> {
        parse_hls_uri(v)?;
        Ok(format!(
            "{v}{}ticket={ticket}",
            if v.contains('?') { "&" } else { "?" }
        ))
    };
    text.lines()
        .map(|line| {
            if line.starts_with('#') {
                if let Some((prefix, rest)) = line.split_once("URI=\"") {
                    let (name, suffix) = rest.split_once('"').ok_or(())?;
                    Ok(format!("{prefix}URI=\"{}\"{suffix}", uri(name)?))
                } else {
                    Ok(line.into())
                }
            } else if line.is_empty() {
                Ok(String::new())
            } else {
                uri(line)
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.join("\n") + "\n")
}
pub(super) fn parse_hls_uri(uri: &str) -> Result<&str, ()> {
    let (file, query) = uri
        .split_once('?')
        .map_or((uri, None), |(f, q)| (f, Some(q)));
    if !valid_hls_file(file) {
        return Err(());
    }
    if let Some(query) = query {
        let mut seen = std::collections::HashSet::new();
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').ok_or(())?;
            if !seen.insert(k)
                || !match k {
                    "session" => valid_id(v),
                    "cookieCheck" => v == "1",
                    _ => false,
                }
            {
                return Err(());
            }
        }
    }
    Ok(file)
}
/// Retain only complete segments older than the audience delay. This is enforced
/// again on segment fetch, not merely a seek instruction in a viewer's browser.
pub(super) fn delayed_playlist(
    text: &str,
    cutoff: u64,
) -> Result<(String, Vec<(String, u64)>), ()> {
    if !text.contains("#EXTINF:") {
        return Ok((text.into(), Vec::new()));
    }
    // MediaMTX dates only the last few segments. Back-propagate its first anchor
    // over preceding EXTINF durations, then emit that anchor in our shortened list.
    let mut preceding = 0.0_f64;
    let mut initial = None;
    for line in text.lines() {
        if let Some(date) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            let anchor =
                time::OffsetDateTime::parse(date, &time::format_description::well_known::Rfc3339)
                    .map_err(|_| ())?
                    .unix_timestamp_nanos()
                    / 1_000_000;
            initial =
                Some(u64::try_from(anchor - (preceding * 1000.0).round() as i128).map_err(|_| ())?);
            break;
        }
        if line == "#EXT-X-DISCONTINUITY" {
            return Err(());
        }
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            let d: f64 = value.split(',').next().ok_or(())?.parse().map_err(|_| ())?;
            if !d.is_finite() || d <= 0.0 || d > 30.0 {
                return Err(());
            }
            preceding += d;
        }
    }
    let mut result = Vec::new();
    let mut pending = Vec::new();
    let mut end_ms = Some(initial.ok_or(())?);
    let mut first = true;
    let mut duration = 0.0_f64;
    let mut segments = Vec::new();
    let mut count = 0;
    for line in text.lines() {
        if let Some(date) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            let date =
                time::OffsetDateTime::parse(date, &time::format_description::well_known::Rfc3339)
                    .map_err(|_| ())?;
            end_ms = Some(
                (date.unix_timestamp_nanos() / 1_000_000)
                    .try_into()
                    .map_err(|_| ())?,
            );
            pending.push(line.to_string());
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            if first {
                first = false;
                if !pending
                    .iter()
                    .any(|l: &String| l.starts_with("#EXT-X-PROGRAM-DATE-TIME:"))
                {
                    let date = time::OffsetDateTime::from_unix_timestamp_nanos(
                        end_ms.ok_or(())? as i128 * 1_000_000,
                    )
                    .map_err(|_| ())?
                    .format(&time::format_description::well_known::Rfc3339)
                    .map_err(|_| ())?;
                    pending.push(format!("#EXT-X-PROGRAM-DATE-TIME:{date}"));
                }
            }
            duration = value.split(',').next().ok_or(())?.parse().map_err(|_| ())?;
            if !duration.is_finite() || duration <= 0.0 || duration > 30.0 {
                return Err(());
            }
            pending.push(line.to_string());
        } else if line.starts_with("#EXT-X-MAP:") {
            let (_, rest) = line.split_once("URI=\"").ok_or(())?;
            let (name, _) = rest.split_once('"').ok_or(())?;
            segments.push((parse_hls_uri(name)?.into(), 0));
            result.push(line.to_string());
        } else if !line.is_empty() && !line.starts_with('#') {
            let end = end_ms.ok_or(())? + (duration * 1000.0).ceil() as u64;
            segments.push((parse_hls_uri(line)?.into(), end));
            if end <= cutoff {
                result.append(&mut pending);
                result.push(line.to_string());
                count += 1;
            } else {
                pending.clear();
            }
            end_ms = Some(end);
        } else if line.starts_with("#EXT-X-DISCONTINUITY") {
            pending.push(line.to_string());
        } else {
            result.push(line.to_string());
        }
    }
    if count == 0 {
        return Err(());
    }
    Ok((result.join("\n") + "\n", segments))
}
async fn hls(
    State(state): State<Arc<MediaState>>,
    Path((id, file)): Path<(String, String)>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    check_id(&id)?;
    if !valid_hls_file(&file) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid HLS path"));
    }
    let headers = ticket_headers(&state, &headers, &query, &format!("stream:{id}")).await?;
    authorize(&state, &headers, Permission::ViewData).await?;
    let broadcast = read_presentation(&state).await?.broadcast;
    if broadcast.hidden_stream_ids.contains(&id) {
        return Err(error(StatusCode::FORBIDDEN, "Camera removed from program"));
    }
    let cutoff = now_ms().saturating_sub(broadcast.delay_seconds as u64 * 1000);
    if !file.ends_with(".m3u8") {
        let segments = state.hls_segments.lock().await;
        if !segments
            .get(&(id.clone(), file.clone()))
            .is_some_and(|end| *end <= cutoff)
        {
            return Err(error(
                StatusCode::FORBIDDEN,
                "Segment is not yet released to the audience",
            ));
        }
    }
    if query.session.as_ref().is_some_and(|s| !valid_id(s)) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid HLS session"));
    }
    let suffix = query
        .session
        .as_ref()
        .map(|s| format!("&session={s}"))
        .unwrap_or_default();
    let request = state
        .client
        .get(format!(
            "{}/{id}/{file}?cookieCheck=1{suffix}",
            state.hls_url.trim_end_matches('/')
        ))
        .basic_auth("groundstation", Some(&state.relay_password));
    let response = request.send().await.map_err(upstream_error)?;
    if !response.status().is_success() {
        return Err(error(
            StatusCode::BAD_GATEWAY,
            "Delayed video not yet available",
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > 8 * 1024 * 1024)
    {
        return Err(error(StatusCode::BAD_GATEWAY, "HLS segment too large"));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(upstream_error)? {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err(error(StatusCode::BAD_GATEWAY, "HLS segment too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let content = if file.ends_with(".m3u8") {
        let access = ticket(&state, &headers, &format!("stream:{id}")).await?;
        let (delayed, entries) = delayed_playlist(
            std::str::from_utf8(&bytes)
                .map_err(|_| error(StatusCode::BAD_GATEWAY, "Invalid playlist"))?,
            cutoff,
        )
        .map_err(|_| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Building delayed video buffer",
            )
        })?;
        {
            let mut segments = state.hls_segments.lock().await;
            segments.retain(|_, end| *end == 0 || *end >= now_ms().saturating_sub(120000));
            if segments.len() > 20000 {
                segments.clear();
            }
            for (name, end) in entries {
                segments.insert((id.clone(), name), end);
            }
        }
        bytes = rewrite_playlist(&delayed, &access)
            .map_err(|_| error(StatusCode::BAD_GATEWAY, "Unsafe playlist URI"))?
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

async fn roles(State(state): State<Arc<MediaState>>, headers: HeaderMap) -> ApiResult<Response> {
    let p = crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await?;
    if p.anonymous
        || !p
            .roles
            .iter()
            .any(|role| matches!(role, StreamRole::StreamAdmin | StreamRole::Admin))
    {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Stream administrator required",
        ));
    }
    let config = state
        .app
        .auth
        .load_users_file()
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read accounts"))?;
    Ok(Json(config.users.into_iter().map(|u|serde_json::json!({"username":u.username,"roles":u.roles,"disabled":u.disabled})).collect::<Vec<_>>()).into_response())
}
#[derive(Deserialize)]
struct RoleChange {
    username: String,
    stream_master: bool,
}
async fn set_role(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    Json(change): Json<RoleChange>,
) -> ApiResult<Response> {
    let _guard = state.presentation_write.lock().await;
    let p = crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await?;
    if p.anonymous
        || !p
            .roles
            .iter()
            .any(|role| matches!(role, StreamRole::StreamAdmin | StreamRole::Admin))
    {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Stream administrator required",
        ));
    }
    let close_previews = state
        .app
        .auth
        .update_users(|config| {
            let user = config
                .users
                .iter_mut()
                .find(|u| u.username == change.username)
                .ok_or("Unknown user")?;
            user.roles.retain(|role| {
                *role != StreamRole::StreamMaster && *role != StreamRole::StreamViewer
            });
            if change.stream_master {
                user.roles.push(StreamRole::StreamMaster);
                user.permissions.view_data = true;
            } else {
                user.roles.push(StreamRole::StreamViewer);
            }
            Ok(!change.stream_master
                && !user.permissions.send_commands
                && !user
                    .roles
                    .iter()
                    .any(|role| matches!(role, StreamRole::StreamAdmin | StreamRole::Admin)))
        })
        .map_err(|_| error(StatusCode::CONFLICT, "Cannot update stream role"))?;
    if close_previews {
        let mut sessions = state.preview_sessions.lock().await;
        let closing: Vec<_> = sessions
            .iter()
            .filter(|(owner, _, _)| owner == &change.username)
            .cloned()
            .collect();
        sessions.retain(|(owner, _, _)| owner != &change.username);
        drop(sessions);
        let state = state.clone();
        tokio::spawn(async move {
            for (_, id, session) in closing {
                let result = state
                    .client
                    .delete(format!(
                        "{}/{id}/whep/{session}",
                        state.webrtc_url.trim_end_matches('/')
                    ))
                    .timeout(Duration::from_secs(3))
                    .basic_auth("groundstation", Some(&state.relay_password))
                    .send()
                    .await;
                if !result
                    .is_ok_and(|r| r.status().is_success() || r.status() == StatusCode::NOT_FOUND)
                {
                    log::warn!(
                        "Stream role revoked but a live preview could not be closed; check video relay"
                    );
                }
            }
        });
    }
    log::info!(
        "Stream master role changed by {:?} for {:?}: {}",
        p.username,
        change.username,
        change.stream_master
    );
    Ok(Json(serde_json::json!({"saved":true})).into_response())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn live_program_uses_phase_without_projecting_3d_attitude_or_motion() {
        let app = crate::state::tests::test_app_state().await;
        *app.state.lock().unwrap() = crate::types::FlightState::Ascent;
        let presentation = Presentation::default();
        let snapshot = telemetry_snapshot(&app, &presentation, 100_000);
        assert_eq!(snapshot["phase"], "Ascent");
        assert!(snapshot.get("model_state").is_none());
        assert!(snapshot.get("attitude").is_none());
    }
    #[tokio::test]
    async fn dashboard_resolves_channels_and_marks_stale_samples_unknown() {
        let app = crate::state::tests::test_app_state().await;
        let profile = Presentation {
            stats: default_stats(),
            ..Presentation::default()
        };
        app.recent_telemetry_cache
            .lock()
            .unwrap()
            .push_back(crate::types::TelemetryRow {
                timestamp_ms: 100_000,
                data_type: "GPS_DATA".into(),
                sender_id: "RF".into(),
                values: vec![Some(42.0), Some(-78.0), Some(125.0)],
            });
        let fresh = telemetry_snapshot(&app, &profile, 100_500);
        assert_eq!(fresh["stats"][0]["binding"]["data_type"], "GPS_DATA");
        assert_eq!(fresh["stats"][0]["binding"]["index"], 2);
        assert_eq!(fresh["stats"][0]["value"], 125.0);
        assert_eq!(fresh["stats"][1]["value"], 42.0);
        assert!(fresh["stats"][3]["value"].is_null());
        let stale = telemetry_snapshot(&app, &profile, 106_000);
        assert!(stale["stats"][0]["value"].is_null());
        for sample in 1..=5 {
            let now = 106_000 + sample * 200;
            app.recent_telemetry_cache
                .lock()
                .unwrap()
                .push_back(crate::types::TelemetryRow {
                    timestamp_ms: now as i64,
                    data_type: "GPS_DATA".into(),
                    sender_id: "RF".into(),
                    values: vec![Some(42.0), Some(-78.0), Some(sample as f32)],
                });
            assert_eq!(
                telemetry_snapshot(&app, &profile, now)["stats"][0]["value"].as_f64(),
                Some(sample as f64)
            );
        }
    }
    #[test]
    fn delayed_clock_is_available_to_the_first_viewer_without_live_time_fallback() {
        use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
        let clock = LaunchClockMsg {
            kind: LaunchClockKind::TMinus,
            anchor_timestamp_ms: Some(100_000),
            duration_ms: Some(20_000),
        };
        let mut history = std::collections::VecDeque::new();
        // The service sampler runs before any viewer joins, without a camera/API request.
        for now in (100_000..=115_000).step_by(PROGRAM_SAMPLE_MS as usize) {
            retain_program_snapshot(
                &mut history,
                now,
                serde_json::json!({"t_clock":program_t_clock(&clock, now as i64)}),
            );
        }
        let delayed = delayed_program_snapshot(&history, 102_500).unwrap();
        assert_eq!(delayed["t_clock"], "T− 00:17.50");
        assert_ne!(
            delayed["t_clock"],
            program_t_clock(&clock, 115_000).unwrap()
        );
        assert!(delayed_program_snapshot(&history, 99_999).is_none());
        assert!(delayed_program_snapshot(&history, 118_000).is_none());
    }

    #[test]
    fn program_history_is_bounded_and_rebuffers_after_a_backward_utc_step() {
        let mut history = std::collections::VecDeque::new();
        for now in 0..200_000 {
            retain_program_snapshot(&mut history, now, serde_json::json!({"captured":now}));
        }
        assert!(history.len() <= MAX_PROGRAM_SNAPSHOTS);
        assert!(history.back().unwrap().0 - history.front().unwrap().0 <= PROGRAM_HISTORY_MS);
        retain_program_snapshot(&mut history, 100, serde_json::json!({"captured":100}));
        assert_eq!(history.len(), 1);
        assert!(delayed_program_snapshot(&history, 99).is_none());
        assert_eq!(
            delayed_program_snapshot(&history, 100).unwrap()["captured"],
            100
        );
    }

    #[test]
    fn program_clock_uses_snapshot_time_and_backend_countdown_semantics() {
        use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
        let mut clock = LaunchClockMsg {
            kind: LaunchClockKind::TMinus,
            anchor_timestamp_ms: Some(100_000),
            duration_ms: Some(10_000),
        };
        assert_eq!(
            program_t_clock(&clock, 104_250).as_deref(),
            Some("T− 00:05.75")
        );
        assert_eq!(
            program_t_clock(&clock, 115_000).as_deref(),
            Some("T− 00:00.00")
        );
        clock.kind = LaunchClockKind::TPlus;
        clock.anchor_timestamp_ms = Some(110_000);
        assert_eq!(
            program_t_clock(&clock, 115_250).as_deref(),
            Some("T+ 00:05.25")
        );
        // A delayed snapshot must not use the current/live clock time.
        assert_eq!(
            program_t_clock(&clock, 125_250).as_deref(),
            Some("T+ 00:15.25")
        );
        clock.anchor_timestamp_ms = None;
        assert_eq!(program_t_clock(&clock, 125_250), None);
        clock.kind = LaunchClockKind::TMinus;
        assert_eq!(
            program_t_clock(&clock, 125_250).as_deref(),
            Some("T− 00:10.00")
        );
        assert_eq!(
            program_t_clock(&LaunchClockMsg::idle(), 125_250).as_deref(),
            Some("T− 00:10.00")
        );
    }
    #[test]
    fn playlist_uris_are_scoped() {
        let p =
            rewrite_playlist("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\nseg1.mp4\n", "abc").unwrap();
        assert!(p.contains("init.mp4?ticket=abc\""));
        assert!(p.contains("seg1.mp4?ticket=abc"));
    }
    #[test]
    fn playlist_cannot_escape_proxy() {
        for p in [
            "../secret",
            "https://evil/x.mp4",
            "#EXT-X-MAP:URI=\"/secret.mp4\"",
        ] {
            assert!(rewrite_playlist(p, "abc").is_err());
        }
    }
    #[test]
    fn relay_sessions_are_preserved_without_arbitrary_query_forwarding() {
        assert!(
            rewrite_playlist("video1_stream.m3u8?session=abc-123", "cap")
                .unwrap()
                .contains("?session=abc-123&ticket=cap")
        );
        assert!(rewrite_playlist("x.mp4?session=abc&redirect=evil", "cap").is_err());
    }
    #[test]
    fn future_segments_are_withheld() {
        let src = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXTINF:1,\nold.mp4\n#EXTINF:1,\nfuture.mp4\n";
        let cutoff = 1767225601000;
        let (p, entries) = delayed_playlist(src, cutoff).unwrap();
        assert!(p.contains("old.mp4"));
        assert!(!p.contains("future.mp4"));
        assert_eq!(entries[1].1, cutoff + 1000);
    }
    #[test]
    fn missing_clock_fails_closed() {
        assert!(delayed_playlist("#EXTM3U\n#EXTINF:1,\nseg.mp4\n", u64::MAX).is_err());
    }
    #[test]
    fn trailing_date_anchor_is_back_propagated() {
        let src = "#EXTM3U\n#EXTINF:1,\na.mp4?cookieCheck=1&session=abc\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:01Z\n#EXTINF:1,\nb.mp4?session=abc\n";
        let (p, _) = delayed_playlist(src, 1767225601000).unwrap();
        assert!(p.contains("2026-01-01T00:00:00Z"));
        assert!(p.contains("a.mp4"));
        assert!(!p.contains("b.mp4"));
    }
}
