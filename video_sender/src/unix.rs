//! Linux/Pi camera daemon. Encoded bytes travel over an OS pipe, never through Rust.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{Mutex, watch},
};

const DEFAULT_SOCKET: &str = "/run/gs-video-sender/control.sock";
const MAX_CONFIG: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StreamConfig {
    id: String,
    camera: u32,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Config {
    host: String,
    port: u16,
    username: String,
    streams: Vec<StreamConfig>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.host.is_empty()
                && self.host.len() <= 253
                && self
                    .host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
            "host must be an IPv4 address or DNS hostname"
        );
        ensure!(
            self.port > 0 && !self.username.is_empty(),
            "port and username are required"
        );
        ensure!(self.streams.len() <= 16, "at most 16 streams are supported");
        let mut ids = HashSet::new();
        let mut cameras = HashSet::new();
        for s in &self.streams {
            ensure!(
                !s.id.is_empty()
                    && s.id.len() <= 64
                    && s.id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "invalid stream ID"
            );
            ensure!(ids.insert(&s.id), "duplicate stream ID: {}", s.id);
            ensure!(
                !s.enabled || cameras.insert(s.camera),
                "camera {} is used by multiple enabled streams",
                s.camera
            );
            ensure!(s.camera <= 15, "camera index must be 0–15");
            ensure!(
                (160..=3840).contains(&s.width) && s.width % 2 == 0,
                "width must be even, 160–3840"
            );
            ensure!(
                (120..=2160).contains(&s.height) && s.height % 2 == 0,
                "height must be even, 120–2160"
            );
            ensure!((1..=60).contains(&s.fps), "fps must be 1–60");
            ensure!(
                (100_000..=25_000_000).contains(&s.bitrate),
                "bitrate must be 100000–25000000 bits/s"
            );
        }
        Ok(())
    }
}

async fn load_config(path: &Path) -> Result<Config> {
    let bytes = tokio::fs::read(path)
        .await
        .context("reading sender configuration")?;
    ensure!(bytes.len() <= MAX_CONFIG, "configuration exceeds 64 KiB");
    let config: Config = serde_json::from_slice(&bytes).context("invalid configuration JSON")?;
    config.validate()?;
    Ok(config)
}

async fn save_config(path: &Path, config: &Config) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(&temporary).await?;
    file.write_all(&serde_json::to_vec_pretty(config)?).await?;
    file.sync_all().await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

fn updated_config(config: &Config, id: &str, field: &str, value: Value) -> Result<Config> {
    ensure!(
        ["camera", "width", "height", "fps", "bitrate", "enabled"].contains(&field),
        "unknown setting; use camera, width, height, fps, bitrate or enabled"
    );
    let mut next = config.clone();
    let stream = next
        .streams
        .iter_mut()
        .find(|s| s.id == id)
        .context("unknown stream")?;
    let mut object = serde_json::to_value(&*stream)?;
    object[field] = value;
    *stream = serde_json::from_value(object).context("invalid setting value")?;
    next.validate()?;
    Ok(next)
}

fn escaped(value: &str) -> String {
    let mut result = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            result.push(b as char);
        } else {
            result.push_str(&format!("%{b:02X}"));
        }
    }
    result
}

fn camera_args(s: &StreamConfig) -> Vec<String> {
    [
        "--timeout",
        "0",
        "--nopreview",
        "--codec",
        "h264",
        "--inline",
        "--profile",
        "baseline",
        "--low-latency",
        "--camera",
        &s.camera.to_string(),
        "--width",
        &s.width.to_string(),
        "--height",
        &s.height.to_string(),
        "--framerate",
        &s.fps.to_string(),
        "--bitrate",
        &s.bitrate.to_string(),
        "--intra",
        &s.fps.to_string(),
        "--output",
        "-",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn ffmpeg_args(config: &Config, stream: &StreamConfig, password: &str) -> Vec<String> {
    let destination = format!(
        "rtsp://{}:{}@{}:{}/{}",
        escaped(&config.username),
        escaped(password),
        config.host,
        config.port,
        stream.id
    );
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-fflags",
        "+genpts",
        "-f",
        "h264",
        "-r",
        &stream.fps.to_string(),
        "-i",
        "pipe:0",
        "-map",
        "0:v:0",
        "-c:v",
        "copy",
        "-an",
        "-f",
        "rtsp",
        "-rtsp_transport",
        "tcp",
        "-rw_timeout",
        "5000000",
        "-progress",
        "pipe:1",
        "-stats_period",
        "1",
        &destination,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

struct Pipeline {
    config: Config,
    stream: StreamConfig,
    camera: Child,
    relay: Child,
    progress: Arc<Mutex<Instant>>,
    progress_task: tokio::task::JoinHandle<()>,
}

impl Pipeline {
    async fn start(config: &Config, stream: &StreamConfig, password: &str) -> Result<Self> {
        let mut camera = Command::new("rpicam-vid")
            .args(camera_args(stream))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting rpicam-vid")?;
        let output = camera.stdout.take().context("camera output pipe missing")?;
        let input: Stdio = output.try_into()?;
        let relay = Command::new("ffmpeg")
            .args(ffmpeg_args(config, stream, password))
            .stdin(input)
            .stdout(Stdio::piped())
            // FFmpeg errors can contain the authenticated destination URL.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let mut relay = match relay {
            Ok(relay) => relay,
            Err(err) => {
                let _ = camera.kill().await;
                return Err(err).context("starting ffmpeg");
            }
        };
        let progress = Arc::new(Mutex::new(Instant::now()));
        let progress_writer = progress.clone();
        let stdout = relay.stdout.take().context("relay progress pipe missing")?;
        let progress_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut last_frame = 0;
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(frame) = line
                    .strip_prefix("frame=")
                    .and_then(|n| n.trim().parse::<u64>().ok())
                    && frame > last_frame
                {
                    *progress_writer.lock().await = Instant::now();
                    last_frame = frame;
                }
            }
        });
        Ok(Self {
            config: config.clone(),
            stream: stream.clone(),
            camera,
            relay,
            progress,
            progress_task,
        })
    }

    fn matches(&self, config: &Config, stream: &StreamConfig) -> bool {
        self.stream == *stream
            && self.config.host == config.host
            && self.config.port == config.port
            && self.config.username == config.username
    }

    async fn healthy(&mut self) -> bool {
        matches!(self.camera.try_wait(), Ok(None))
            && matches!(self.relay.try_wait(), Ok(None))
            && self.progress.lock().await.elapsed() < Duration::from_secs(20)
    }

    async fn stop(mut self) {
        let _ = self.camera.kill().await;
        let _ = self.relay.kill().await;
        self.progress_task.abort();
    }
}

#[derive(Default, Clone, Serialize)]
struct StreamStatus {
    state: String,
    restarts: u64,
}

struct Daemon {
    path: PathBuf,
    changes: watch::Sender<Config>,
    update: Mutex<()>,
    status: Mutex<BTreeMap<String, StreamStatus>>,
}

async fn control(daemon: &Daemon, request: Value) -> Result<Value> {
    match request["op"].as_str().unwrap_or("") {
        "status" => Ok(
            json!({"config": daemon.changes.borrow().clone(), "streams": daemon.status.lock().await.clone()}),
        ),
        "set" => {
            let _guard = daemon.update.lock().await;
            let next = updated_config(
                &daemon.changes.borrow(),
                request["stream"].as_str().context("stream required")?,
                request["field"].as_str().context("field required")?,
                request["value"].clone(),
            )?;
            save_config(&daemon.path, &next).await?;
            daemon.changes.send_replace(next);
            Ok(json!({"message": "saved; affected stream will restart"}))
        }
        "reload" => {
            let _guard = daemon.update.lock().await;
            let next = load_config(&daemon.path).await?;
            daemon.changes.send_replace(next);
            Ok(json!({"message": "configuration reloaded"}))
        }
        _ => bail!("unknown control operation"),
    }
}

async fn handle_control(daemon: &Daemon, mut socket: UnixStream) -> Result<()> {
    let mut line = String::new();
    let mut reader = BufReader::new((&mut socket).take((MAX_CONFIG + 1) as u64));
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
    ensure!(
        line.len() <= MAX_CONFIG && line.ends_with('\n'),
        "invalid control request"
    );
    let result = match serde_json::from_str::<Value>(&line) {
        Ok(request) => control(daemon, request).await,
        Err(err) => Err(err.into()),
    };
    let response = match result {
        Ok(value) => json!({"ok": true, "result": value}),
        Err(err) => json!({"ok": false, "error": err.to_string()}),
    };
    socket.write_all(format!("{response}\n").as_bytes()).await?;
    Ok(())
}

async fn run(path: PathBuf, socket_path: PathBuf) -> Result<()> {
    let config = load_config(&path).await?;
    let password = std::env::var("GS_VIDEO_PASSWORD").context("GS_VIDEO_PASSWORD is required")?;
    ensure!(!password.is_empty(), "GS_VIDEO_PASSWORD must not be empty");
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::symlink_metadata(&socket_path).await.is_ok() {
        use std::os::unix::fs::FileTypeExt;
        ensure!(
            tokio::fs::symlink_metadata(&socket_path)
                .await?
                .file_type()
                .is_socket(),
            "control path exists and is not a socket"
        );
        match UnixStream::connect(&socket_path).await {
            Ok(_) => bail!("a sender already owns this control socket"),
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                tokio::fs::remove_file(&socket_path).await?
            }
            Err(err) => return Err(err).context("checking existing control socket"),
        }
    }
    let listener = UnixListener::bind(&socket_path)?;
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).await?;
    let (changes, mut changed) = watch::channel(config);
    let daemon = Arc::new(Daemon {
        path,
        changes,
        update: Mutex::new(()),
        status: Mutex::new(BTreeMap::new()),
    });
    let server_daemon = daemon.clone();
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await?;
            // Local control is serialized and bounded by a read timeout.
            if let Err(err) = handle_control(&server_daemon, socket).await {
                eprintln!("control request: {err}");
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut running = BTreeMap::<String, Pipeline>::new();
    let mut retry = BTreeMap::<String, Instant>::new();
    eprintln!(
        "video sender ready; control socket {}",
        socket_path.display()
    );
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
            _ = hup.recv() => { if let Err(err) = control(&daemon, json!({"op":"reload"})).await { eprintln!("reload rejected: {err}"); } },
            _ = changed.changed() => {},
            _ = tick.tick() => {},
        }
        let config = daemon.changes.borrow().clone();
        let mut stop = Vec::new();
        for (id, pipeline) in &mut running {
            let wanted = config.streams.iter().find(|s| &s.id == id && s.enabled);
            if !wanted.is_some_and(|s| pipeline.matches(&config, s)) {
                stop.push((id.clone(), false));
            } else if !pipeline.healthy().await {
                stop.push((id.clone(), true));
            }
        }
        for (id, failed) in stop {
            if let Some(pipeline) = running.remove(&id) {
                pipeline.stop().await;
            }
            if failed {
                retry.insert(id.clone(), Instant::now() + Duration::from_secs(3));
            }
            eprintln!(
                "stream {id}: {}",
                if failed {
                    "pipeline stopped or stalled; retrying"
                } else {
                    "applying configuration"
                }
            );
        }
        daemon
            .status
            .lock()
            .await
            .retain(|id, _| config.streams.iter().any(|s| &s.id == id));
        for stream in &config.streams {
            let mut status = daemon
                .status
                .lock()
                .await
                .get(&stream.id)
                .cloned()
                .unwrap_or_default();
            if !stream.enabled {
                status.state = "disabled".into();
            } else if running.contains_key(&stream.id) {
                status.state = "running".into();
            } else if retry
                .get(&stream.id)
                .is_some_and(|time| *time > Instant::now())
            {
                status.state = "retrying".into();
            } else {
                status.restarts += 1;
                match Pipeline::start(&config, stream, &password).await {
                    Ok(pipeline) => {
                        running.insert(stream.id.clone(), pipeline);
                        status.state = "running".into();
                        eprintln!("stream {}: started", stream.id);
                    }
                    Err(err) => {
                        eprintln!("stream {}: {err}; retrying", stream.id);
                        retry.insert(stream.id.clone(), Instant::now() + Duration::from_secs(3));
                        status.state = "retrying".into();
                    }
                }
            }
            daemon.status.lock().await.insert(stream.id.clone(), status);
        }
    }
    server.abort();
    for (_, pipeline) in running {
        pipeline.stop().await;
    }
    tokio::fs::remove_file(socket_path).await?;
    Ok(())
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let socket =
        PathBuf::from(std::env::var_os("GS_VIDEO_SOCKET").unwrap_or_else(|| DEFAULT_SOCKET.into()));
    match args.first().map(String::as_str) {
        Some("daemon") if args.len() == 2 => run(PathBuf::from(&args[1]), socket).await,
        Some("check") if args.len() == 2 => { load_config(Path::new(&args[1])).await?; println!("configuration valid"); Ok(()) },
        Some("status" | "reload") if args.len() == 1 => request(&socket, json!({"op":args[0]})).await,
        Some("set") if args.len() == 4 => request(&socket, json!({"op":"set", "stream":args[1], "field":args[2], "value":serde_json::from_str::<Value>(&args[3]).context("value must be a JSON number or boolean")?})).await,
        _ => { eprintln!("Usage: gs-video-sender daemon CONFIG.json | check CONFIG.json | status | reload | set STREAM FIELD VALUE\nGS_VIDEO_SOCKET overrides the local control socket."); bail!("invalid arguments") }
    }
}

async fn request(path: &Path, body: Value) -> Result<()> {
    let mut socket = UnixStream::connect(path)
        .await
        .context("connecting to sender daemon")?;
    socket.write_all(format!("{body}\n").as_bytes()).await?;
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        BufReader::new(socket).take(256 * 1024).read_line(&mut line),
    )
    .await??;
    let response: Value = serde_json::from_str(&line)?;
    ensure!(
        response["ok"] == true,
        "{}",
        response["error"]
            .as_str()
            .unwrap_or("daemon request failed")
    );
    println!("{}", serde_json::to_string_pretty(&response["result"])?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> Config {
        serde_json::from_str(include_str!("../config.example.json")).unwrap()
    }

    #[test]
    fn runtime_updates_validate_without_mutating_current_config() {
        let config = example();
        let next = updated_config(&config, "front", "bitrate", json!(1_000_000)).unwrap();
        assert_eq!(next.streams[0].bitrate, 1_000_000);
        assert_eq!(config.streams[0].bitrate, 2_000_000);
        assert!(updated_config(&config, "front", "fps", json!(0)).is_err());
        assert!(updated_config(&config, "front", "width", json!(641)).is_err());
        assert!(updated_config(&config, "front", "enabled", json!("true")).is_err());
        assert!(updated_config(&config, "front", "password", json!("secret")).is_err());
        assert!(updated_config(&config, "missing", "fps", json!(20)).is_err());
    }

    #[test]
    fn duplicate_cameras_and_paths_are_rejected() {
        let mut config = example();
        let mut second = config.streams[0].clone();
        second.id = "side".into();
        config.streams.push(second);
        assert!(config.validate().is_err());
        config.streams[1].camera = 1;
        assert!(config.validate().is_ok());
        config.streams[1].id = "../bad".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn pipeline_copies_h264_and_escapes_credentials() {
        let config = example();
        let args = ffmpeg_args(&config, &config.streams[0], "a@b:/");
        assert!(args.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(args.last().unwrap().contains("a%40b%3A%2F@"));
        let args = camera_args(&config.streams[0]);
        assert!(args.contains(&"--low-latency".into()));
        assert!(args.windows(2).any(|pair| pair == ["--intra", "30"]));
    }

    #[tokio::test]
    async fn control_changes_persist_and_invalid_reload_keeps_running_config() {
        let path = std::env::temp_dir().join(format!(
            "gs-video-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (changes, _) = watch::channel(example());
        let daemon = Daemon {
            path: path.clone(),
            changes,
            update: Mutex::new(()),
            status: Mutex::new(BTreeMap::new()),
        };
        control(
            &daemon,
            json!({"op":"set", "stream":"front", "field":"fps", "value":15}),
        )
        .await
        .unwrap();
        assert_eq!(load_config(&path).await.unwrap().streams[0].fps, 15);
        tokio::fs::write(&path, b"invalid json").await.unwrap();
        assert!(control(&daemon, json!({"op":"reload"})).await.is_err());
        assert_eq!(daemon.changes.borrow().streams[0].fps, 15);
        tokio::fs::remove_file(path).await.unwrap();
    }
}
