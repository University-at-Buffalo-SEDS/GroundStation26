use anyhow::{Context, Result, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    hmac, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Default)]
pub struct View {
    pub connected: bool,
    pub busy: bool,
    pub passed: bool,
    pub offered: bool,
    pub message: String,
    pub capacity: usize,
    pub encoding_ms: u64,
    pub upload_bps: u64,
    pub download_bps: u64,
    pub jobs: usize,
}
pub enum Action {
    Connect {
        station: String,
        username: String,
        password: String,
        local_certificate: bool,
    },
    Test {
        name: String,
    },
    Offer(bool),
    Disconnect,
}
#[derive(Clone)]
pub struct Controller {
    pub tx: mpsc::UnboundedSender<Action>,
    pub view: Arc<Mutex<View>>,
}
impl Controller {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let view = Arc::new(Mutex::new(View::default()));
        let shared = view.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("worker runtime");
            let final_view = shared.clone();
            if runtime.block_on(control(rx, shared)).is_err() {
                update(&final_view, |v| v.message = "video_processing_shutdown_failed: cleanup was not verified; new work remains blocked.".into());
            }
        });
        Self { tx, view }
    }
}
#[derive(Clone)]
struct Session {
    client: reqwest::Client,
    upload_client: reqwest::Client,
    base: Url,
    token: String,
    host: Option<String>,
    generation: Option<String>,
}
#[derive(Deserialize)]
struct Challenge {
    algorithm: String,
    iterations: u32,
    salt_b64: String,
    challenge_id: String,
    server_nonce_b64: String,
}
#[derive(Serialize)]
struct Proof<'a> {
    username: &'a str,
    challenge_id: &'a str,
    server_nonce_b64: &'a str,
    client_nonce_b64: &'a str,
    remember_me: bool,
}
#[derive(Clone, Deserialize)]
struct Job {
    #[serde(default)]
    generation: Option<String>,
    id: String,
    input: String,
    output: String,
}
#[derive(Deserialize)]
struct Heartbeat {
    selected: bool,
    jobs: Vec<Job>,
    #[serde(default)]
    generation: Option<String>,
    #[serde(default)]
    stopped_generation: Option<String>,
    #[serde(default)]
    cleanup_complete: bool,
}
fn update(view: &Arc<Mutex<View>>, f: impl FnOnce(&mut View)) {
    if let Ok(mut v) = view.lock() {
        f(&mut v);
    }
}
fn local_host(url: &Url) -> bool {
    url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.ends_with(".local")
            || h.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| match ip {
                    std::net::IpAddr::V4(v) => {
                        v.is_private() || v.is_loopback() || v.is_link_local()
                    }
                    std::net::IpAddr::V6(v) => {
                        v.is_loopback() || v.is_unique_local() || v.is_unicast_link_local()
                    }
                })
    })
}
fn endpoint(base: &Url, path: &str) -> Result<Url> {
    let url = base.join(path)?;
    ensure!(
        url.origin() == base.origin() && url.username().is_empty() && url.password().is_none(),
        "Station returned an off-station media address"
    );
    Ok(url)
}
async fn bytes(response: reqwest::Response, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        response.status().is_success(),
        "Station request failed (HTTP {})",
        response.status().as_u16()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= maximum as u64),
        "Station response too large"
    );
    let mut response = response;
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Station transfer interrupted")?
    {
        ensure!(
            data.len() + chunk.len() <= maximum,
            "Station response too large"
        );
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}
async fn json<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    Ok(serde_json::from_slice(&bytes(response, 65536).await?)?)
}
async fn connect(
    station: String,
    username: String,
    password: String,
    local_certificate: bool,
) -> Result<Session> {
    let base = Url::parse(station.trim())?;
    ensure!(
        matches!(base.scheme(), "http" | "https")
            && base.username().is_empty()
            && base.password().is_none()
            && base.query().is_none()
            && base.fragment().is_none(),
        "Enter a station HTTP(S) address without credentials or query parameters"
    );
    ensure!(
        base.scheme() == "https" || local_host(&base),
        "Use HTTPS for a nonlocal station"
    );
    ensure!(
        !local_certificate || local_host(&base),
        "Local-certificate override is restricted to a local station"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .read_timeout(Duration::from_secs(15))
        .danger_accept_invalid_certs(local_certificate)
        .build()?;
    // Live uploads intentionally do not receive response headers until the
    // stream ends. A control-request read timeout would terminate healthy media.
    let upload_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .danger_accept_invalid_certs(local_certificate)
        .build()?;
    let username = username.trim().to_lowercase();
    let challenge: Challenge = json(
        client
            .post(endpoint(&base, "/api/auth/challenge")?)
            .json(&serde_json::json!({"username":username}))
            .send()
            .await
            .context("Cannot contact station")?,
    )
    .await?;
    ensure!(
        challenge.algorithm == "pbkdf2_sha256"
            && (10_000..=2_000_000).contains(&challenge.iterations),
        "Unsupported station login challenge"
    );
    let salt = STANDARD.decode(&challenge.salt_b64)?;
    ensure!(salt.len() <= 1024, "Invalid login salt");
    let mut key = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(challenge.iterations).unwrap(),
        &salt,
        password.as_bytes(),
        &mut key,
    );
    let mut nonce = [0u8; 32];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("Secure random unavailable"))?;
    let nonce = URL_SAFE_NO_PAD.encode(nonce);
    let payload = Proof {
        username: &username,
        challenge_id: &challenge.challenge_id,
        server_nonce_b64: &challenge.server_nonce_b64,
        client_nonce_b64: &nonce,
        remember_me: false,
    };
    let proof = URL_SAFE_NO_PAD.encode(
        hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, &key),
            &serde_json::to_vec(&payload)?,
        )
        .as_ref(),
    );
    key.fill(0);
    let result:serde_json::Value=json(client.post(endpoint(&base,"/api/auth/login")?).json(&serde_json::json!({"username":username,"challenge_id":challenge.challenge_id,"client_nonce_b64":nonce,"remember_me":false,"proof_b64":proof})).send().await.context("Station login failed")?).await?;
    let token = result["token"]
        .as_str()
        .context("Station did not return a session")?
        .to_owned();
    let session = Session {
        client,
        upload_client,
        base,
        token,
        host: None,
        generation: None,
    };
    let response = session
        .client
        .get(endpoint(&session.base, "/api/video/transcode/hosts")?)
        .bearer_auth(&session.token)
        .send()
        .await
        .context("Cannot check stream-manager access")?;
    let _: serde_json::Value = json(response).await?;
    Ok(session)
}
async fn test(session: &mut Session, name: String) -> Result<serde_json::Value> {
    let begin: serde_json::Value = json(
        session
            .client
            .post(endpoint(&session.base, "/api/video/transcode/test")?)
            .bearer_auth(&session.token)
            .json(&serde_json::json!({"name":name}))
            .send()
            .await?,
    )
    .await?;
    let id = begin["id"].as_str().context("Invalid test ID")?;
    let count = begin["test_bytes"].as_u64().context("Invalid test size")? as usize;
    ensure!(count <= 8 * 1024 * 1024, "Network test too large");
    let download = endpoint(
        &session.base,
        &format!("/api/video/transcode/test/{id}/download"),
    )?;
    let start = Instant::now();
    let data = bytes(
        session
            .client
            .get(download)
            .bearer_auth(&session.token)
            .header("Accept-Encoding", "identity")
            .send()
            .await?,
        count,
    )
    .await?;
    ensure!(data.len() == count, "Incomplete download test");
    let down = (count as f64 * 8.0 / start.elapsed().as_secs_f64().max(0.001)) as u64;
    let response = session
        .client
        .post(endpoint(
            &session.base,
            &format!("/api/video/transcode/test/{id}/upload"),
        )?)
        .bearer_auth(&session.token)
        .body(data)
        .send()
        .await?;
    ensure!(response.status().is_success(), "Upload test failed");
    let encoding_ms = benchmark().await?;
    let report: serde_json::Value = json(
        session
            .client
            .post(endpoint(
                &session.base,
                &format!("/api/video/transcode/test/{id}/complete"),
            )?)
            .bearer_auth(&session.token)
            .json(&serde_json::json!({"encoding_ms":encoding_ms,"download_bps":down}))
            .send()
            .await?,
    )
    .await?;
    session.host = Some(id.to_owned());
    session.generation = match report.get("generation") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) if valid_generation(value) => Some(value.clone()),
        _ => anyhow::bail!("Invalid station processing generation"),
    };
    Ok(report)
}
pub async fn benchmark() -> Result<u64> {
    let start = Instant::now();
    let status = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "quiet",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1280x720:rate=30",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "3",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-tune",
                "zerolatency",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "30",
                "-b:v",
                "2500k",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("Encoding test timed out")?
    .context("Install FFmpeg with libx264 and AAC support")?;
    ensure!(
        status.success(),
        "FFmpeg could not complete the encoding test"
    );
    Ok(start.elapsed().as_millis().max(1) as u64)
}
async fn heartbeat(session: &Session, offered: bool) -> Result<Heartbeat> {
    let id = session.host.as_deref().context("Test this host first")?;
    let reply: Heartbeat = json(session.client.post(endpoint(&session.base, &format!("/api/video/transcode/host/{id}"))?)
        .bearer_auth(&session.token)
        .json(&serde_json::json!({"offered":offered,"generation":session.generation}))
        .send().await?).await?;
    if offered && session.generation.is_some() {
        ensure!(reply.generation == session.generation, "Station returned a different processing generation");
    }
    Ok(reply)
}
// FFmpeg must see the complete HLS master, including alternate audio renditions.
// Keep station credentials inside this bounded loopback proxy, never in argv.
#[derive(Clone)]
struct HlsProxy {
    session: Session,
    prefix: String,
    origin: String,
    capability: String,
    cancelled: CancellationToken,
    requests: Arc<tokio::sync::Semaphore>,
}
impl HlsProxy {
    fn validate(&self, base: &Url, path: &str) -> Result<Url> {
        let url = endpoint(base, path)?;
        ensure!(
            url.path().starts_with(&self.prefix),
            "Media address outside camera input"
        );
        ensure!(url.fragment().is_none(), "Invalid media fragment");
        Ok(url)
    }
    fn address(&self, base: &Url, path: &str) -> Result<String> {
        let url = self.validate(base, path)?;
        let filename = url.path().rsplit('/').next().unwrap_or("media");
        ensure!(
            filename
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "Invalid media filename"
        );
        Ok(format!(
            "{}/{}/{}/{}",
            self.origin,
            self.capability,
            URL_SAFE_NO_PAD.encode(url.as_str()),
            filename
        ))
    }
    fn rewrite(&self, base: &Url, text: &str) -> Result<String> {
        ensure!(text.starts_with("#EXTM3U"), "Invalid HLS playlist");
        let mut output = String::new();
        for line in text.lines().map(str::trim) {
            if !line.is_empty() && !line.starts_with('#') {
                output.push_str(&self.address(base, line)?);
            } else {
                let mut rest = line;
                // URI attributes occur on rendition, map, key and low-latency tags.
                while let Some(index) = rest.find("URI=") {
                    output.push_str(&rest[..index + 4]);
                    rest = &rest[index + 4..];
                    ensure!(rest.starts_with('"'), "Unquoted HLS media address");
                    rest = &rest[1..];
                    let end = rest.find('"').context("Invalid HLS media address")?;
                    output.push('"');
                    output.push_str(&self.address(base, &rest[..end])?);
                    output.push('"');
                    rest = &rest[end + 1..];
                }
                output.push_str(rest);
            }
            output.push('\n');
        }
        Ok(output)
    }
    async fn response(
        &self,
        path: &str,
        headers: &axum::http::HeaderMap,
    ) -> Result<axum::response::Response> {
        let _permit = self.requests.acquire().await?;
        let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
        ensure!(
            parts.len() == 3 && parts[0] == self.capability,
            "Invalid media capability"
        );
        let decoded = URL_SAFE_NO_PAD.decode(parts[1])?;
        let url = self.validate(&self.session.base, std::str::from_utf8(&decoded)?)?;
        let playlist = url.path().ends_with(".m3u8");
        let mut request = self
            .session
            .client
            .get(url.clone())
            .bearer_auth(&self.session.token);
        if let Some(range) = headers.get(axum::http::header::RANGE) {
            request = request.header("Range", range.to_str()?);
        }
        let response = request.send().await?;
        let status = response.status();
        let range = response.headers().get("content-range").cloned();
        let data = bytes(
            response,
            if playlist {
                1024 * 1024
            } else {
                8 * 1024 * 1024
            },
        )
        .await?;
        let data = if playlist {
            self.rewrite(&url, std::str::from_utf8(&data)?)?
                .into_bytes()
        } else {
            data
        };
        let mut result = axum::response::Response::builder()
            .status(status.as_u16())
            .header(
                "Content-Type",
                if playlist {
                    "application/vnd.apple.mpegurl"
                } else {
                    "application/octet-stream"
                },
            );
        if let Some(range) = range {
            result = result.header("Content-Range", range);
        }
        Ok(result.body(axum::body::Body::from(data))?)
    }
}
async fn proxy_request(
    axum::extract::State(proxy): axum::extract::State<HlsProxy>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    tokio::select! {
        biased;
        _ = proxy.cancelled.cancelled() => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
        result = tokio::time::timeout(Duration::from_secs(15), proxy.response(uri.path(), &headers)) => {
            match result {
                Ok(Ok(response)) => response,
                _ => axum::http::StatusCode::BAD_GATEWAY.into_response(),
            }
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum CleanupUncertain {
    #[cfg(unix)]
    EncoderGroup,
    EncoderKillAndWait,
    EncoderWait,
    EncoderTimeout,
    ProxyJoin,
    ProxyAbort,
    ProxyTimeout,
    RemoteCompletion,
    JobJoin,
}
impl CleanupUncertain {
    fn code(self) -> &'static str {
        match self {
            #[cfg(unix)]
            Self::EncoderGroup => "video_processing_encoder_group_cleanup_unverified",
            Self::EncoderKillAndWait => "video_processing_encoder_kill_and_wait_unverified",
            Self::EncoderWait => "video_processing_encoder_wait_unverified",
            Self::EncoderTimeout => "video_processing_encoder_cleanup_timeout",
            Self::ProxyJoin => "video_processing_proxy_join_unverified",
            Self::ProxyAbort => "video_processing_proxy_abort_unverified",
            Self::ProxyTimeout => "video_processing_proxy_cleanup_timeout",
            Self::RemoteCompletion => "video_processing_remote_completion_unverified",
            Self::JobJoin => "video_processing_job_join_unverified",
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum JobOutcome {
    Completed,
    Cancelled,
    Failed(&'static str),
    CleanupUncertain(CleanupUncertain),
}
#[derive(Default)]
struct StopReport {
    failed: Option<&'static str>,
    uncertain: Option<CleanupUncertain>,
}
impl StopReport {
    fn add_uncertainty(&mut self, reason: CleanupUncertain) {
        // Local cleanup uncertainty cannot be overwritten by an unrelated
        // remote-only uncertainty whose acknowledgement could later clear it.
        if self.uncertain.is_none() || matches!(self.uncertain, Some(CleanupUncertain::RemoteCompletion)) {
            self.uncertain = Some(reason);
        }
    }
    fn observe(&mut self, outcome: Result<JobOutcome, tokio::task::JoinError>) {
        match outcome {
            Ok(JobOutcome::Completed | JobOutcome::Cancelled) => {},
            Ok(JobOutcome::Failed(code)) => self.failed = Some(code),
            Ok(JobOutcome::CleanupUncertain(reason)) => self.add_uncertainty(reason),
            Err(_) => self.uncertain = Some(CleanupUncertain::JobJoin),
        }
    }
}
fn publish_cleanup_blocked(view: &Arc<Mutex<View>>, reason: CleanupUncertain) {
    update(view, |v| {
        v.offered = false;
        v.passed = false;
        v.busy = false;
        v.message = format!("{}: cleanup was not verified; new processing work is blocked.", reason.code());
    });
}

struct ProxyGuard {
    cancelled: CancellationToken,
    task: Option<JoinHandle<std::io::Result<()>>>,
}
impl ProxyGuard {
    async fn close(mut self) -> Result<(), CleanupUncertain> {
        self.cancelled.cancel();
        let Some(mut task) = self.task.take() else { return Ok(()); };
        // Existing proxy requests already observe cancellation. Give graceful
        // shutdown its existing request budget, then abort AND join the server.
        match tokio::time::timeout(Duration::from_secs(15), &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(_) => Err(CleanupUncertain::ProxyJoin),
            Err(_) => {
                task.abort();
                let joined = task.await;
                if !matches!(joined, Err(ref error) if error.is_cancelled()) {
                    return Err(CleanupUncertain::ProxyAbort);
                }
                Err(CleanupUncertain::ProxyTimeout)
            }
        }
    }
}
impl Drop for ProxyGuard {
    fn drop(&mut self) {
        self.cancelled.cancel();
        // Panic/foreign-abort fallback only, never a quiescence proof. Normal
        // production paths consume close() and await the actual JoinHandle.
        if let Some(task) = &self.task { task.abort(); }
    }
}

async fn proxy(session: Session, input: &str) -> Result<(ProxyGuard, String)> {
    let input = endpoint(&session.base, input)?;
    ensure!(
        input.path().starts_with("/api/video/transcode/input/") && input.path().ends_with(".m3u8"),
        "Invalid camera input"
    );
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let mut random = [0u8; 24];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow::anyhow!("Cannot secure media proxy"))?;
    let state = HlsProxy {
        prefix: format!(
            "{}/",
            input
                .path()
                .rsplit_once('/')
                .context("Invalid camera input")?
                .0
        ),
        origin: format!("http://{}", listener.local_addr()?),
        capability: URL_SAFE_NO_PAD.encode(random),
        session,
        cancelled: CancellationToken::new(),
        requests: Arc::new(tokio::sync::Semaphore::new(8)),
    };
    let url = state.address(&input, input.as_str())?;
    let cancelled = state.cancelled.clone();
    let app = axum::Router::new()
        .fallback(axum::routing::get(proxy_request))
        .with_state(state);
    let server_cancelled = cancelled.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_cancelled.cancelled_owned())
            .await
    });
    Ok((ProxyGuard { cancelled, task: Some(task) }, url))
}
#[cfg(test)]
async fn transcode(session: Session, job: Job) -> Result<()> {
    match transcode_until_cancelled(session, job, CancellationToken::new()).await {
        JobOutcome::Completed | JobOutcome::Cancelled => Ok(()),
        JobOutcome::Failed(code) => Err(anyhow::anyhow!(code)),
        JobOutcome::CleanupUncertain(reason) => Err(anyhow::anyhow!(reason.code())),
    }
}
// No crate dependency is added for POSIX group ownership. These calls use
// only an owned/unreaped Child id, and are never compiled on non-Unix hosts.
#[cfg(unix)]
mod encoder_group {
    unsafe extern "C" {
        fn getpgid(pid: std::ffi::c_int) -> std::ffi::c_int;
        fn getpgrp() -> std::ffi::c_int;
        fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
    }
    pub fn verified(pid: u32) -> bool {
        // SAFETY: caller holds the unreaped Child that supplied this id.
        unsafe { getpgid(pid as std::ffi::c_int) == pid as std::ffi::c_int
            && pid as std::ffi::c_int != getpgrp() }
    }
    pub fn terminate(pid: u32) -> bool {
        if !verified(pid) { return false; }
        // SIGKILL is 9 on the supported Unix platforms. Revalidate first so a
        // failed setpgid or parent group can never receive a group signal.
        unsafe { kill(-(pid as std::ffi::c_int), 9) == 0 }
    }
}
async fn stop_encoder(child: &mut tokio::process::Child) -> Result<(), CleanupUncertain> {
    #[cfg(unix)]
    let group_failed = child.id().is_some_and(|pid| !encoder_group::terminate(pid));
    #[cfg(not(unix))]
    let group_failed = false;
    // A direct-kill error never bypasses wait. A completed child can make the
    // kill redundant; actual wait is authoritative for reaping its pid.
    let killed = child.start_kill();
    let waited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    match waited {
        Ok(Ok(_)) if !group_failed => Ok(()),
        #[cfg(unix)]
        Ok(Ok(_)) => Err(CleanupUncertain::EncoderGroup),
        #[cfg(not(unix))]
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) if killed.is_err() => Err(CleanupUncertain::EncoderKillAndWait),
        Ok(Err(_)) => Err(CleanupUncertain::EncoderWait),
        Err(_) => Err(CleanupUncertain::EncoderTimeout),
    }
}
async fn rejected_upload(response: reqwest::Response, cancelled: &CancellationToken) -> JobOutcome {
    let status = response.status();
    // Auth rejection is pre-spawn or follows known cleanup in this backend.
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return JobOutcome::Failed("video_processing_authorization_rejected");
    }
    // Read only the bounded stable error code, never log an error body. Unknown
    // remote completion is not evidence that another remux process is gone.
    let read = async {
        let mut response = response;
        if response.content_length().is_some_and(|length| length > 256) { return None; }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > 256 { return None; }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).ok()
    };
    let code = tokio::select! {
        biased;
        _ = cancelled.cancelled() => None,
        code = tokio::time::timeout(Duration::from_secs(15), read) => code.ok().flatten(),
    };
    match (status.as_u16(), code.as_deref()) {
        (400, Some("Invalid source camera" | "IDs must be 1–64 ASCII letters, digits, hyphens or underscores" | "video_upload_interrupted"))
        | (409, Some("Host is no longer selected" | "video_processing_assignment_revoked" | "video_processing_generation_revoked" | "video_processing_generation_stale" | "video_processing_previous_upload_not_reaped"))
        | (408, Some("video_upload_cancelled"))
        | (502, Some("video_remux_failed" | "video_remux_ended_before_input" | "video_remux_input_failed" | "video_remux_pipe_missing"))
        | (503, Some("FFmpeg remuxer is unavailable" | "Invalid local video receiver" | "Invalid receiver"))
        | (504, Some("video_remux_timeout")) => JobOutcome::Failed("video_processing_upload_rejected_after_known_cleanup"),
        _ => JobOutcome::CleanupUncertain(CleanupUncertain::RemoteCompletion),
    }
}

async fn transcode_until_cancelled(
    session: Session,
    job: Job,
    cancelled: CancellationToken,
) -> JobOutcome {
    if job.generation != session.generation {
        return JobOutcome::Failed("video_processing_job_generation_mismatch");
    }
    let destination = match endpoint(&session.base, &job.output) {
        Ok(destination) => destination,
        Err(_) => return JobOutcome::Failed("video_processing_output_address_rejected"),
    };
    if cancelled.is_cancelled() { return JobOutcome::Cancelled; }
    let (proxy, input) = tokio::select! {
        biased;
        _ = cancelled.cancelled() => return JobOutcome::Cancelled,
        input = proxy(session.clone(), &job.input) => match input {
            Ok(input) => input,
            Err(_) => return JobOutcome::Failed("video_processing_proxy_start_failed"),
        },
    };
    let work = async {
        if cancelled.is_cancelled() { return JobOutcome::Cancelled; }
        let mut command = Command::new("ffmpeg");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let spawned = command
        .args([
            "-hide_banner",
            "-loglevel",
            if cfg!(test) { "warning" } else { "quiet" },
            "-nostdin",
            "-re",
            "-i",
            &input,
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-vf",
            "scale=w=1280:h=720:force_original_aspect_ratio=decrease:force_divisible_by=2",
            "-r",
            "30",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-profile:v",
            "main",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-keyint_min",
            "30",
            "-sc_threshold",
            "0",
            "-b:v",
            "2500k",
            "-maxrate",
            "3000k",
            "-bufsize",
            "3000k",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-ar",
            "48000",
            "-ac",
            "2",
            // Matroska preserves AAC AudioSpecificConfig for station RTSP copy.
            "-f",
            "matroska",
            "-live",
            "1",
            "-cluster_time_limit",
            "500",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if cfg!(test) && std::env::var_os("GS_VIDEO_COMPLETION_TEST").is_some() {
            Stdio::null()
        } else if cfg!(test) {
            Stdio::inherit()
        } else {
            Stdio::null()
        })

            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(_) => return JobOutcome::Failed("video_processing_encoder_spawn_failed"),
        };
        #[cfg(unix)]
        if !child.id().is_some_and(encoder_group::verified) {
            return match stop_encoder(&mut child).await {
                Ok(()) => JobOutcome::Failed("video_processing_encoder_group_unverified"),
                Err(reason) => JobOutcome::CleanupUncertain(reason),
            };
        }
        let Some(output) = child.stdout.take() else {
            return match stop_encoder(&mut child).await {
                Ok(()) => JobOutcome::Failed("video_processing_encoder_pipe_missing"),
                Err(reason) => JobOutcome::CleanupUncertain(reason),
            };
        };

        let mut request = session.upload_client.post(destination)
            .bearer_auth(&session.token)
            .header("Content-Type", "video/x-matroska")
            .body(reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(output)));
        if let Some(generation) = &job.generation {
            request = request.header("X-Station-Transcode-Generation", generation);
        }
        let mut upload = Box::pin(request.send());
        let mut encoder_status = None;
        enum Transfer {
            Cancelled,
            Reply(Result<reqwest::Response, reqwest::Error>),
            EncoderFailed,
            UploadTimedOut,
        }
        let transfer = tokio::select! {
            biased;
            _ = cancelled.cancelled() => Transfer::Cancelled,
            response = &mut upload => Transfer::Reply(response),
            status = child.wait() => {
                match status {
                    Ok(status) if status.success() => {
                        encoder_status = Some(status);
                        tokio::select! {
                            biased;
                            _ = cancelled.cancelled() => Transfer::Cancelled,
                            response = tokio::time::timeout(Duration::from_secs(15), &mut upload) => {
                                match response {
                                    Ok(response) => Transfer::Reply(response),
                                    Err(_) => Transfer::UploadTimedOut,
                                }
                            }
                        }
                    }
                    _ => Transfer::EncoderFailed,
                }
            }
        };
        drop(upload);
        match transfer {
            Transfer::Cancelled => {
                proxy.cancelled.cancel();
                match stop_encoder(&mut child).await {
                    // The upload may already have reached the station. A
                    // local reap does not acknowledge its asynchronous remux
                    // cleanup after the request future is dropped.
                    Ok(()) => JobOutcome::CleanupUncertain(CleanupUncertain::RemoteCompletion),
                    Err(reason) => JobOutcome::CleanupUncertain(reason),
                }
            }
            Transfer::EncoderFailed => {
                proxy.cancelled.cancel();
                match stop_encoder(&mut child).await {
                    // No backend completion response was received before
                    // encoder failure, so remote cleanup remains unverified.
                    Ok(()) => JobOutcome::CleanupUncertain(CleanupUncertain::RemoteCompletion),
                    Err(reason) => JobOutcome::CleanupUncertain(reason),
                }
            }
            Transfer::Reply(Err(_)) | Transfer::UploadTimedOut => {
                proxy.cancelled.cancel();
                match stop_encoder(&mut child).await {
                    Ok(()) => JobOutcome::CleanupUncertain(CleanupUncertain::RemoteCompletion),
                    Err(reason) => JobOutcome::CleanupUncertain(reason),
                }
            }
            Transfer::Reply(Ok(response)) if !response.status().is_success() => {
                // Stop/join local work BEFORE reading any rejection body. The
                // existing control budget bounds that small completion read;
                // it adds no delay or timeout to a healthy live upload.
                proxy.cancelled.cancel();
                match stop_encoder(&mut child).await {
                    Ok(()) => rejected_upload(response, &cancelled).await,
                    Err(reason) => JobOutcome::CleanupUncertain(reason),
                }
            }
            Transfer::Reply(Ok(_)) => {
                let completion = if let Some(status) = encoder_status {
                    Some(Ok(Ok(status)))
                } else {
                    tokio::select! {
                        biased;
                        _ = cancelled.cancelled() => None,
                        completion = tokio::time::timeout(Duration::from_secs(5), child.wait()) => Some(completion),
                    }
                };
                match completion {
                    None => {
                        proxy.cancelled.cancel();
                        match stop_encoder(&mut child).await {
                            Ok(()) => JobOutcome::Cancelled,
                            Err(reason) => JobOutcome::CleanupUncertain(reason),
                        }
                    }
                    Some(Ok(Ok(status))) if status.success() => JobOutcome::Completed,
                    Some(Ok(Ok(_))) => JobOutcome::Failed("video_processing_encoder_failed"),
                    Some(_) => {
                        proxy.cancelled.cancel();
                        match stop_encoder(&mut child).await {
                            Ok(()) => JobOutcome::Failed("video_processing_encoder_completion_failed"),
                            Err(reason) => JobOutcome::CleanupUncertain(reason),
                        }
                    }
                }
            }
        }
    }.await;
    let proxy_closed = proxy.close().await;
    with_proxy_completion(work, proxy_closed)
}
fn with_proxy_completion(work: JobOutcome, proxy_closed: Result<(), CleanupUncertain>) -> JobOutcome {
    match (work, proxy_closed) {
        (JobOutcome::CleanupUncertain(reason), _) if !matches!(reason, CleanupUncertain::RemoteCompletion) => JobOutcome::CleanupUncertain(reason),
        (_, Err(reason)) => JobOutcome::CleanupUncertain(reason),
        (outcome, Ok(())) => outcome,
    }
}

struct EncodingJob {
    cancelled: CancellationToken,
    task: JoinHandle<JobOutcome>,
}
async fn stop_jobs(jobs: &mut HashMap<String, EncodingJob>) -> StopReport {
    for job in jobs.values() { job.cancelled.cancel(); }
    let mut report = StopReport::default();
    for (_, job) in jobs.drain() { report.observe(job.task.await); }
    report
}
fn valid_generation(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn stop_acknowledges(original: &str, reply: &Heartbeat) -> bool {
    reply.cleanup_complete && !reply.selected && reply.jobs.is_empty()
        && reply.stopped_generation.as_deref() == Some(original)
        && reply.generation.as_deref().is_some_and(|next| valid_generation(next) && next != original)
}
async fn stop_with_fence(
    session: Option<&mut Session>, jobs: &mut HashMap<String, EncodingJob>,
    prior: Option<CleanupUncertain>,
) -> StopReport {
    let mut report = stop_jobs(jobs).await;
    if let Some(reason) = prior { report.add_uncertainty(reason); }
    if let Some(session) = session {
        let original = session.generation.clone();
        let reply = heartbeat(session, false).await;
        let acknowledged = original.as_deref().zip(reply.as_ref().ok())
            .is_some_and(|(original, reply)| stop_acknowledges(original, reply));
        if acknowledged {
            // This mutable session remains owned by the single controller
            // throughout the await. No replacement session adopts this ack.
            if report.uncertain.is_none() || matches!(report.uncertain, Some(CleanupUncertain::RemoteCompletion)) {
                session.generation = reply.unwrap().generation;
                report.uncertain = None;
            }
        } else if original.is_some() {
            report.add_uncertainty(CleanupUncertain::RemoteCompletion);
        }
    }
    report
}
async fn control(mut rx: mpsc::UnboundedReceiver<Action>, view: Arc<Mutex<View>>) -> Result<()> {
    let mut session: Option<Session> = None;
    let mut jobs = HashMap::<String, EncodingJob>::new();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let mut offered = false;
    // Only an authenticated acknowledgement of the original offer generation
    // can clear remote-only uncertainty; local cleanup failures remain latched.
    let mut uncertain: Option<CleanupUncertain> = None;
    loop {
        tokio::select! {
            action = rx.recv() => {
                let Some(action) = action else { break; };
                match action {
                    Action::Connect { station, username, password, local_certificate } => {
                        if let Some(reason) = uncertain { publish_cleanup_blocked(&view,reason); continue; }
                        let stopped = stop_with_fence(session.as_mut(), &mut jobs, uncertain).await;
                        if let Some(reason) = stopped.uncertain {
                            uncertain = Some(reason); offered = false;
                            publish_cleanup_blocked(&view,reason); continue;
                        }
                        offered = false;
                        update(&view,|v|{*v=View::default();v.busy=true;v.message="Connecting…".into();});
match connect(station,username,password,local_certificate).await{Ok(s)=>{session=Some(s);update(&view,|v|{v.connected=true;v.message="Connected. Test encoding and network capacity before offering this host.".into();});},Err(_)=>{session=None;update(&view,|v|v.message="Connection failed. Check the station address, certificate, account and stream-manager permission.".into());}}update(&view,|v|v.busy=false);
                    }
                    Action::Test { name } => {
                        if let Some(reason) = uncertain { publish_cleanup_blocked(&view,reason); continue; }
                        // The real UI already disables Test while offered. Keep
                        // that guard in the worker too, before any benchmark.
                        if offered || !jobs.is_empty() {
                            update(&view,|v|v.message="Stop offering and finish current processing before testing this host.".into());
                            continue;
                        }
if let Some(s)=session.as_mut(){update(&view,|v|{v.busy=true;v.message="Testing network bandwidth and H.264/AAC encoding…".into();});match tokio::time::timeout(Duration::from_secs(60),test(s,name)).await{Ok(Ok(report))=>update(&view,|v|{v.passed=report["passed"].as_bool().unwrap_or(false);v.capacity=report["capacity"].as_u64().unwrap_or(0)as usize;v.encoding_ms=report["encoding_ms"].as_u64().unwrap_or(0);v.upload_bps=report["upload_bps"].as_u64().unwrap_or(0);v.download_bps=report["download_bps"].as_u64().unwrap_or(0);v.message=if v.passed{"Tests passed. Make this computer available, then select it in Stream Manager."}else{"Insufficient compute or bandwidth. This host cannot be selected."}.into();}),_=>update(&view,|v|{v.passed=false;v.message="Test failed. Check FFmpeg availability and the network connection.".into();})}update(&view,|v|v.busy=false);}
                    }
                    Action::Offer(value) => {
                        if value && let Some(reason)=uncertain { publish_cleanup_blocked(&view,reason);continue; }
                        offered=value; update(&view,|v|v.offered=value);
                        if !value {
                            update(&view,|v|v.busy=true);
                            let stopped=stop_with_fence(session.as_mut(), &mut jobs, uncertain).await;
                            uncertain=stopped.uncertain;
                            if let Some(reason)=uncertain { publish_cleanup_blocked(&view,reason); }
                            else { update(&view,|v|{v.busy=false;v.jobs=0;v.message=stopped.failed.unwrap_or("Processing stopped after acknowledged remote cleanup; this host is no longer offered.").into();}); }
                        }
                    }
                    Action::Disconnect => {
                        update(&view,|v|v.busy=true);
                        offered=false;
                        let stopped=stop_with_fence(session.as_mut(), &mut jobs, uncertain).await;
                        uncertain=stopped.uncertain;
                        session=None;
                        if let Some(reason)=uncertain {
                            update(&view,|v|v.connected=false);
                            publish_cleanup_blocked(&view,reason);
                        } else { update(&view,|v|*v=View::default()); }
                    }
                }
            }
            _ = tick.tick() => {
                if uncertain.is_some() || !offered { continue; }
                let Some(s)=session.clone() else { continue; };
                match heartbeat(&s,true).await {
                    Ok(reply) => {
                        let wanted:HashSet<_>=reply.jobs.iter().map(|job|job.id.clone()).collect();
                        let remove:Vec<_>=jobs.iter().filter(|(id,job)|!wanted.contains(*id)||job.task.is_finished()).map(|(id,_)|id.clone()).collect();
                        let mut report=StopReport::default();
                        let mut completed=false;
                        for id in remove {
                            if let Some(job)=jobs.remove(&id) {
                                job.cancelled.cancel();
                                let outcome=job.task.await;
                                completed |= matches!(&outcome,Ok(JobOutcome::Completed));
                                report.observe(outcome);
                            }
                        }
                        if let Some(reason)=report.uncertain {
                            uncertain=Some(reason);offered=false;
                            let remaining=stop_with_fence(session.as_mut(), &mut jobs, uncertain).await;
                            uncertain=remaining.uncertain;
                            if let Some(reason)=uncertain { publish_cleanup_blocked(&view,reason); }
                            else { update(&view,|v|{v.offered=false;v.jobs=0;v.message="Processing stopped after acknowledged remote cleanup; offer again explicitly.".into();}); }
                            continue;
                        }
                        // Only proven-cleanup failures can retry the same
                        // assigned camera on the unchanged 3-second heartbeat.
                        for job in reply.jobs {
                            if let std::collections::hash_map::Entry::Vacant(entry) = jobs.entry(job.id.clone()) {
                                let cancelled=CancellationToken::new();
                                let task=tokio::spawn(transcode_until_cancelled(s.clone(),job.clone(),cancelled.clone()));
                                entry.insert(EncodingJob{cancelled,task});
                            }
                        }
                        update(&view,|v|{
                            v.jobs=jobs.len();
                            v.message=if !reply.selected {"Available · waiting for Stream Manager selection".into()}
                            else if let Some(code)=report.failed {format!("{code}: processing failed after verified cleanup; retrying the assigned camera job.")}
                            else if completed {"video_processing_finished: encoder and upload completed; restarting the assigned camera job.".into()}
                            else {format!("Selected by GroundStation · encoding {} camera(s)",jobs.len())};
                        });
                    }
                    Err(_) => {
                        offered=false;
                        update(&view,|v|v.offered=false);
                        let stopped=stop_with_fence(session.as_mut(), &mut jobs, uncertain).await;
                        if let Some(reason)=stopped.uncertain {
                            uncertain=Some(reason);offered=false;publish_cleanup_blocked(&view,reason);
                        } else {
                            update(&view,|v|{v.jobs=0;v.message="Station unavailable or permission changed. Encoding stopped; reconnect if the station restarted.".into();});
                        }
                    }
                }
            }
        }
    }
    let stopped=stop_with_fence(session.as_mut(), &mut jobs, uncertain).await;
    if let Some(reason)=stopped.uncertain { anyhow::bail!(reason.code()); }
    if let Some(code)=stopped.failed { update(&view,|v|v.message=code.into()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_ack_requires_original_generation_and_never_clears_local_uncertainty() {
        let original = "11111111111111111111111111111111";
        let mut reply = Heartbeat { selected: false, jobs: Vec::new(), generation: Some("22222222222222222222222222222222".into()),
            stopped_generation: Some(original.into()), cleanup_complete: true };
        assert!(stop_acknowledges(original, &reply));
        assert!(!stop_acknowledges("33333333333333333333333333333333", &reply));
        reply.cleanup_complete = false;
        assert!(!stop_acknowledges(original, &reply));
        reply.cleanup_complete = true;
        reply.generation = Some(original.into());
        assert!(!stop_acknowledges(original, &reply));
        let combined = with_proxy_completion(JobOutcome::CleanupUncertain(CleanupUncertain::RemoteCompletion), Err(CleanupUncertain::ProxyTimeout));
        assert!(matches!(combined, JobOutcome::CleanupUncertain(CleanupUncertain::ProxyTimeout)));
        let preserved = with_proxy_completion(JobOutcome::CleanupUncertain(CleanupUncertain::EncoderTimeout), Err(CleanupUncertain::ProxyTimeout));
        assert!(matches!(preserved, JobOutcome::CleanupUncertain(CleanupUncertain::EncoderTimeout)));
        let mut report = StopReport::default();
        report.add_uncertainty(CleanupUncertain::ProxyTimeout);
        report.add_uncertainty(CleanupUncertain::RemoteCompletion);
        assert!(matches!(report.uncertain, Some(CleanupUncertain::ProxyTimeout)));
        let mut report = StopReport::default();
        report.add_uncertainty(CleanupUncertain::RemoteCompletion);
        report.add_uncertainty(CleanupUncertain::EncoderTimeout);
        assert!(matches!(report.uncertain, Some(CleanupUncertain::EncoderTimeout)));
    }
    #[tokio::test]
    #[ignore = "manual synthetic MediaMTX/backend roundtrip; loopback fixture only"]
    async fn native_worker_roundtrip() {
        let origin = std::env::var("GS_MEDIA_TEST_ORIGIN")
            .unwrap_or_else(|_| "http://127.0.0.1:19090".into());
        let url = Url::parse(&origin).unwrap();
        assert!(
            url.host_str()
                .is_some_and(|h| h == "127.0.0.1" || h == "localhost")
        );
        let mut session = connect(
            origin,
            "producer".into(),
            "fixture-only-password".into(),
            false,
        )
        .await
        .unwrap();
        let report = test(&mut session, "Synthetic encoding worker".into())
            .await
            .unwrap();
        assert_eq!(
            report["passed"], true,
            "Compute/network qualification failed: {report}"
        );
        let _ = heartbeat(&session, true).await.unwrap();
        let response = session
            .client
            .post(endpoint(&session.base, "/api/video/transcode/hosts").unwrap())
            .bearer_auth(&session.token)
            .json(&serde_json::json!({"host_id":session.host}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        // Exercise the old helper's video-only MPEG-TS upload against the same
        // station route before starting current Matroska jobs.
        let mut legacy_job = heartbeat(&session, true)
            .await
            .unwrap()
            .jobs
            .into_iter()
            .find(|j| j.id == "front")
            .unwrap();
        // Isolate this qualification stream so retained HLS segments cannot
        // mix its synthetic resolution with current camera outputs.
        legacy_job.output = format!(
            "{}/compatibility",
            legacy_job.output.rsplit_once('/').unwrap().0
        );
        let receiver =
            Url::parse(&std::env::var("GS_VIDEO_API_URL").expect("loopback fixture receiver URL"))
                .unwrap();
        assert_eq!(receiver.host_str(), Some("127.0.0.1"));
        let mut legacy = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-re",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=30",
                "-t",
                "6",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-g",
                "30",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "mpegts",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let legacy_output = legacy.stdout.take().unwrap();
        let legacy_session = session.clone();
        let upload = tokio::spawn(async move {
            legacy_session
                .upload_client
                .post(endpoint(&legacy_session.base, &legacy_job.output).unwrap())
                .bearer_auth(&legacy_session.token)
                .header("Content-Type", "video/mp2t")
                .body(reqwest::Body::wrap_stream(
                    tokio_util::io::ReaderStream::new(legacy_output),
                ))
                .send()
                .await
                .unwrap()
        });
        let legacy_start = Instant::now();
        let mut legacy_ready = None;
        while !upload.is_finished() {
            let _ = heartbeat(&session, true).await.unwrap();
            let response = session
                .client
                .get(
                    receiver
                        .join("/v3/paths/get/transcoded-compatibility")
                        .unwrap(),
                )
                .basic_auth(
                    "groundstation",
                    Some(std::env::var("GS_VIDEO_PASSWORD").unwrap()),
                )
                .send()
                .await
                .unwrap();
            if legacy_ready.is_none()
                && response.status().is_success()
                && json::<serde_json::Value>(response).await.unwrap()["ready"] == true
            {
                legacy_ready = Some(legacy_start.elapsed().as_millis());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(upload.await.unwrap().status().is_success());
        assert!(legacy.wait().await.unwrap().success());
        assert!(
            legacy_ready.is_some(),
            "Legacy MPEG-TS never reached the station receiver"
        );
        println!("LEGACY_VIDEO_TS_AVAILABLE_MS={}", legacy_ready.unwrap());
        let mut jobs = Vec::new();
        let start = Instant::now();
        let mut last_heartbeat = Instant::now() - Duration::from_secs(5);
        let mut encoded = false;
        while start.elapsed() < Duration::from_secs(45) {
            if let Some(index) = jobs
                .iter()
                .position(|job: &JoinHandle<Result<()>>| job.is_finished())
            {
                let result = jobs.swap_remove(index).await;
                panic!("Native encoding job stopped: {result:?}");
            }
            if last_heartbeat.elapsed() > Duration::from_secs(3) {
                let reply = heartbeat(&session, true).await.unwrap();
                if jobs.is_empty() {
                    for job in reply.jobs {
                        jobs.push(tokio::spawn(transcode(session.clone(), job)));
                    }
                }
                last_heartbeat = Instant::now();
            }
            let state: serde_json::Value = json(
                session
                    .client
                    .get(endpoint(&session.base, "/api/live_streams").unwrap())
                    .bearer_auth(&session.token)
                    .send()
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
            if state["streams"].as_array().is_some_and(|s| {
                s.iter()
                    .any(|v| v["url"].as_str().is_some_and(|u| u.contains("transcoded-")))
            }) {
                encoded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(encoded, "Encoded feed never became available");
        println!(
            "MATROSKA_AUDIO_VIDEO_AVAILABLE_MS={}",
            start.elapsed().as_millis()
        );
        // Keep the worker alive so a concurrent browser regression can use its feed.
        let hold = std::env::var("GS_TRANSCODE_TEST_HOLD_SECONDS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        for _ in 0..hold {
            if let Some(index) = jobs.iter().position(|job| job.is_finished()) {
                panic!(
                    "Native encoding job stopped during playback: {:?}",
                    jobs.swap_remove(index).await
                );
            }
            let _ = heartbeat(&session, true).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let _ = heartbeat(&session, false).await.unwrap();
        for job in jobs {
            job.abort();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let state: serde_json::Value = json(
            session
                .client
                .get(endpoint(&session.base, "/api/live_streams").unwrap())
                .bearer_auth(&session.token)
                .send()
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert!(
            state["streams"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| !v["url"].as_str().unwrap().contains("transcoded-")),
            "Disconnect did not restore original feeds"
        );
        println!(
            "PASS native login, compute/network qualification, remote H264/AAC encode, copy remux and disconnect fallback: {report}"
        );
    }
    #[test]
    fn input_urls_cannot_export_station_authorization() {
        let base = Url::parse("https://station.local/api/video/input/front/index.m3u8").unwrap();
        assert!(endpoint(&base, "segment.mp4?session=sample").is_ok());
        assert!(endpoint(&base, "https://elsewhere.invalid/segment.mp4").is_err());
        assert!(endpoint(&base, "//elsewhere.invalid/segment.mp4").is_err());
    }
    fn fixture_proxy() -> HlsProxy {
        HlsProxy {
            session: Session {
                client: reqwest::Client::new(),
                upload_client: reqwest::Client::new(),
                base: Url::parse("http://127.0.0.1:19090").unwrap(),
                token: "secret-token".into(),
                host: None,
        generation: None,
            },
            prefix: "/api/video/transcode/input/front/".into(),
            origin: "http://127.0.0.1:12345".into(),
            capability: "private-capability".into(),
            cancelled: CancellationToken::new(),
            requests: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }
    #[test]
    fn hls_proxy_preserves_alternate_audio_and_initialization() {
        let proxy = fixture_proxy();
        let base = proxy
            .session
            .base
            .join("/api/video/transcode/input/front/index.m3u8")
            .unwrap();
        let master = proxy.rewrite(&base, "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",URI=\"audio/index.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"audio\"\nvideo.m3u8\n").unwrap();
        assert!(master.contains(
            "TYPE=AUDIO,GROUP-ID=\"audio\",URI=\"http://127.0.0.1:12345/private-capability/"
        ));
        assert!(master.contains("/index.m3u8\""));
        assert!(master.contains("/video.m3u8\n"));
        assert!(!master.contains("secret-token"));
        let media = proxy
            .rewrite(
                &base,
                "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4?ticket=sample\"\n#EXTINF:1,\na.m4s\n",
            )
            .unwrap();
        assert!(media.contains("/init.mp4\""));
        assert!(media.contains("/a.m4s\n"));
    }
    #[test]
    fn hls_proxy_rejects_off_station_and_other_camera_addresses() {
        let proxy = fixture_proxy();
        let base = proxy
            .session
            .base
            .join("/api/video/transcode/input/front/index.m3u8")
            .unwrap();
        for path in [
            "https://elsewhere.invalid/a.m3u8",
            "//elsewhere.invalid/a.m3u8",
            "../side/a.m3u8",
            "/api/video/password",
            "file:///etc/passwd",
        ] {
            assert!(
                proxy.rewrite(&base, &format!("#EXTM3U\n{path}\n")).is_err(),
                "{path}"
            );
            assert!(
                proxy
                    .rewrite(
                        &base,
                        &format!("#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,URI=\"{path}\"\n")
                    )
                    .is_err()
            );
        }
        assert!(
            proxy
                .rewrite(&base, "#EXTM3U\n#EXT-X-MAP:URI=init.mp4\n")
                .is_err()
        );
    }
    #[tokio::test]
    async fn hls_proxy_authenticates_upstream_and_closes_with_job() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let app = axum::Router::new().fallback(axum::routing::get(
            move |headers: axum::http::HeaderMap| {
                let calls = observed.clone();
                async move {
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer secret-token");
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio.m3u8\"\nvideo.m3u8\n"
                }
            },
        ));
        let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut session = fixture_proxy().session;
        session.base = Url::parse(&format!("http://{address}")).unwrap();
        let (guard, input) = proxy(session, "/api/video/transcode/input/front/index.m3u8")
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let response = client.get(&input).send().await.unwrap();
        assert!(response.status().is_success());
        let body = response.text().await.unwrap();
        assert!(body.contains("/audio.m3u8\""));
        assert!(body.contains("/video.m3u8\n"));
        assert!(!body.contains("secret-token"));
        let mut invalid = Url::parse(&input).unwrap();
        invalid.set_path("/wrong-capability/target/index.m3u8");
        assert_eq!(
            client.get(invalid).send().await.unwrap().status(),
            reqwest::StatusCode::BAD_GATEWAY
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        guard.close().await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            client
                .get(input)
                .send()
                .await
                .map_or(true, |r| !r.status().is_success())
        );
        upstream.abort();
    }
    #[test]
    fn certificate_override_is_local_only() {
        assert!(local_host(&Url::parse("https://192.168.1.2").unwrap()));
        assert!(local_host(&Url::parse("https://station.local").unwrap()));
        assert!(!local_host(&Url::parse("https://example.com").unwrap()));
    }
}

#[cfg(test)]
#[path = "qualification_tests.rs"]
mod qualification_tests;
