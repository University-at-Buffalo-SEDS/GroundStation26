use anyhow::{Context, Result, bail, ensure};
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
            runtime.block_on(control(rx, shared));
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
    id: String,
    input: String,
    output: String,
}
#[derive(Deserialize)]
struct Heartbeat {
    selected: bool,
    jobs: Vec<Job>,
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
    json(
        session
            .client
            .post(endpoint(
                &session.base,
                &format!("/api/video/transcode/host/{id}"),
            )?)
            .bearer_auth(&session.token)
            .json(&serde_json::json!({"offered":offered}))
            .send()
            .await?,
    )
    .await
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
        _ = proxy.cancelled.cancelled() => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
        result = tokio::time::timeout(Duration::from_secs(15), proxy.response(uri.path(), &headers)) => {
            match result {
                Ok(Ok(response)) => response,
                _ => axum::http::StatusCode::BAD_GATEWAY.into_response(),
            }
        }
    }
}
struct ProxyGuard(CancellationToken);
impl Drop for ProxyGuard {
    fn drop(&mut self) {
        self.0.cancel();
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
    let guard = ProxyGuard(cancelled.clone());
    let app = axum::Router::new()
        .fallback(axum::routing::get(proxy_request))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(cancelled.cancelled_owned())
            .await;
    });
    Ok((guard, url))
}
async fn transcode(session: Session, job: Job) -> Result<()> {
    let (_proxy, input) = proxy(session.clone(), &job.input).await?;
    let mut child = Command::new("ffmpeg")
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
        .stderr(if cfg!(test) {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .context("Cannot start FFmpeg")?;
    let output = child.stdout.take().unwrap();
    let upload = session
        .upload_client
        .post(endpoint(&session.base, &job.output)?)
        .bearer_auth(&session.token)
        .header("Content-Type", "video/x-matroska")
        .body(reqwest::Body::wrap_stream(
            tokio_util::io::ReaderStream::new(output),
        ))
        .send();
    let result = tokio::select! {response=upload=>{let response=response.context("Encoded upload interrupted")?;ensure!(response.status().is_success(),"Station rejected encoded video (HTTP {})", response.status().as_u16());Ok(())},status=child.wait()=>{status.context("Encoder stopped")?;bail!("Encoder stopped")}};
    let _ = child.kill().await;
    result
}
async fn control(mut rx: mpsc::UnboundedReceiver<Action>, view: Arc<Mutex<View>>) {
    let mut session: Option<Session> = None;
    let mut jobs = HashMap::<String, JoinHandle<Result<()>>>::new();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let mut offered = false;
    loop {
        tokio::select! {
        action=rx.recv()=>{let Some(action)=action else{break;};
         match action{
          Action::Connect{station,username,password,local_certificate}=>{for(_,job)in jobs.drain(){job.abort();}offered=false;update(&view,|v|{*v=View::default();v.busy=true;v.message="Connecting…".into();});match connect(station,username,password,local_certificate).await{Ok(s)=>{session=Some(s);update(&view,|v|{v.connected=true;v.message="Connected. Test encoding and network capacity before offering this host.".into();});},Err(_)=>{session=None;update(&view,|v|v.message="Connection failed. Check the station address, certificate, account and stream-manager permission.".into());}}update(&view,|v|v.busy=false);},
          Action::Test{name}=>{if let Some(s)=session.as_mut(){update(&view,|v|{v.busy=true;v.message="Testing network bandwidth and H.264/AAC encoding…".into();});match tokio::time::timeout(Duration::from_secs(60),test(s,name)).await{Ok(Ok(report))=>update(&view,|v|{v.passed=report["passed"].as_bool().unwrap_or(false);v.capacity=report["capacity"].as_u64().unwrap_or(0)as usize;v.encoding_ms=report["encoding_ms"].as_u64().unwrap_or(0);v.upload_bps=report["upload_bps"].as_u64().unwrap_or(0);v.download_bps=report["download_bps"].as_u64().unwrap_or(0);v.message=if v.passed{"Tests passed. Make this computer available, then select it in Stream Manager."}else{"Insufficient compute or bandwidth. This host cannot be selected."}.into();}),_=>update(&view,|v|{v.passed=false;v.message="Test failed. Check FFmpeg availability and the network connection.".into();})}update(&view,|v|v.busy=false);}},
          Action::Offer(value)=>{offered=value;update(&view,|v|v.offered=value);if !value{for(_,job)in jobs.drain(){job.abort();}if let Some(s)=&session{let _=heartbeat(s,false).await;}}},
          Action::Disconnect=>{offered=false;for(_,job)in jobs.drain(){job.abort();}if let Some(s)=&session{let _=heartbeat(s,false).await;}session=None;update(&view,|v|*v=View::default());}
         }
        },
        _=tick.tick()=>{if offered && let Some(s)=&session{match heartbeat(s,true).await{Ok(reply)=>{
           let wanted:HashSet<_>=reply.jobs.iter().map(|j|j.id.clone()).collect();let remove:Vec<_>=jobs.iter().filter(|(id,h)|!wanted.contains(*id)||h.is_finished()).map(|(id,_)|id.clone()).collect();for id in remove{if let Some(task)=jobs.remove(&id){task.abort();}}
           for job in reply.jobs{if !jobs.contains_key(&job.id){jobs.insert(job.id.clone(),tokio::spawn(transcode(s.clone(),job)));}}
           update(&view,|v|{v.jobs=jobs.len();v.message=if reply.selected{format!("Selected by GroundStation · encoding {} camera(s)",jobs.len())}else{"Available · waiting for Stream Manager selection".into()};});
          },Err(_)=>{for(_,job)in jobs.drain(){job.abort();}update(&view,|v|{v.jobs=0;v.message="Station unavailable or permission changed. Encoding stopped; reconnect if the station restarted.".into();});}}}}
        }
    }
    for (_, job) in jobs {
        job.abort();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
        drop(guard);
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
