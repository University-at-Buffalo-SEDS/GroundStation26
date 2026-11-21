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
const USB_BY_ID: &str = "/dev/v4l/by-id";
const USB_BY_PATH: &str = "/dev/v4l/by-path";

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
    #[serde(default)]
    audio_device: Option<String>,
    /// Set only on streams synthesized for detected USB cameras.
    #[serde(skip)]
    usb: Option<UsbSource>,
}

#[derive(Clone, Debug, PartialEq)]
struct UsbSource {
    device: String,
    input_format: String,
    encoder: String,
}

/// Publishes every connected USB (UVC) camera with these settings.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct UsbCameras {
    enabled: bool,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    #[serde(default = "default_input_format")]
    input_format: String,
    #[serde(default = "default_encoder")]
    encoder: String,
    /// Also publish each camera's built-in microphone, when it has one.
    #[serde(default = "default_audio")]
    audio: bool,
}

fn default_audio() -> bool {
    true
}

fn default_input_format() -> String {
    "mjpeg".into()
}

fn default_encoder() -> String {
    "libx264".into()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Config {
    /// `auto` discovers the station using the shared video password.
    host: String,
    #[serde(default)]
    discovery_interface: Option<String>,
    #[serde(default = "default_discovery_port")]
    discovery_port: u16,
    port: u16,
    username: String,
    streams: Vec<StreamConfig>,
    #[serde(default)]
    usb_cameras: Option<UsbCameras>,
}

fn default_discovery_port() -> u16 {
    gs_camera_discovery::PORT
}

fn validate_mode(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<()> {
    ensure!(
        (160..=3840).contains(&width) && width.is_multiple_of(2),
        "width must be even, 160–3840"
    );
    ensure!(
        (120..=2160).contains(&height) && height.is_multiple_of(2),
        "height must be even, 120–2160"
    );
    ensure!((1..=60).contains(&fps), "fps must be 1–60");
    ensure!(
        (100_000..=25_000_000).contains(&bitrate),
        "bitrate must be 100000–25000000 bits/s"
    );
    Ok(())
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
        ensure!(self.discovery_port != 0, "discovery_port must be nonzero");
        ensure!(
            self.discovery_interface
                .as_ref()
                .is_none_or(|name| !name.is_empty() && name.len() <= 64),
            "invalid discovery interface"
        );
        ensure!(self.streams.len() <= 16, "at most 16 streams are supported");
        let mut ids = HashSet::new();
        let mut cameras = HashSet::new();
        for s in &self.streams {
            ensure!(
                s.audio_device.as_ref().is_none_or(|d| !d.is_empty()
                    && d.len() <= 256
                    && !d.starts_with('-')
                    && d.chars().all(|c| c.is_ascii_graphic())),
                "invalid ALSA audio device"
            );
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
            validate_mode(s.width, s.height, s.fps, s.bitrate)?;
        }
        if let Some(usb) = &self.usb_cameras {
            validate_mode(usb.width, usb.height, usb.fps, usb.bitrate)?;
            ensure!(
                ["mjpeg", "yuyv422"].contains(&usb.input_format.as_str()),
                "usb_cameras.input_format must be mjpeg or yuyv422"
            );
            ensure!(
                ["libx264", "h264_v4l2m2m"].contains(&usb.encoder.as_str()),
                "usb_cameras.encoder must be libx264 or h264_v4l2m2m"
            );
        }
        Ok(())
    }
}

/// Stream ID for a `/dev/v4l/by-id` or `/dev/v4l/by-path` capture link name.
fn usb_stream_id(name: &str) -> Option<String> {
    let stem = name.strip_suffix("-video-index0")?;
    let (prefix, label) = if let Some(label) = stem.strip_prefix("usb-") {
        ("usb-", label)
    } else {
        ("usb-port-", stem.split_once("-usb-")?.1)
    };
    let label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!label.is_empty()).then(|| format!("{prefix}{label}").chars().take(64).collect())
}

/// Connected USB capture devices as (stream ID, device path), sorted by ID.
/// Serial-qualified `by-id` names are preferred; cameras without a usable
/// serial fall back to their physical port so identical models stay distinct.
fn detect_usb_cameras(by_id: &Path, by_path: &Path, sys: &Path) -> Vec<UsbCamera> {
    let mut found = BTreeMap::new();
    let mut targets = HashSet::new();
    for dir in [by_id, by_path] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
        names.sort();
        for name in names {
            let Some(name) = name.to_str() else { continue };
            let link = dir.join(name);
            let (Some(id), Ok(target)) = (usb_stream_id(name), std::fs::canonicalize(&link)) else {
                continue;
            };
            if !found.contains_key(&id) && targets.insert(target.clone()) {
                let camera = UsbCamera {
                    id: id.clone(),
                    device: link.to_string_lossy().into_owned(),
                    microphone: usb_microphone(&target, sys),
                };
                found.insert(id, camera);
            }
        }
    }
    found.into_values().collect()
}

#[derive(Clone, Debug, PartialEq)]
struct UsbCamera {
    id: String,
    device: String,
    /// ALSA capture device on the same physical USB device, if any.
    microphone: Option<String>,
}

/// Finds the ALSA card exposed by the same USB device as a `/dev/videoN` node,
/// i.e. a webcam's built-in microphone, addressed by its stable card ID.
fn usb_microphone(video: &Path, sys: &Path) -> Option<String> {
    let usb_device = |interface: PathBuf| interface.parent().map(Path::to_path_buf);
    let node = video.file_name()?;
    let camera = usb_device(
        std::fs::canonicalize(sys.join("class/video4linux").join(node).join("device")).ok()?,
    )?;
    let mut cards: Vec<_> = std::fs::read_dir(sys.join("class/sound"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("card"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    cards.sort();
    cards.into_iter().find_map(|card| {
        let device = usb_device(std::fs::canonicalize(card.join("device")).ok()?)?;
        let id = std::fs::read_to_string(card.join("id")).ok()?;
        let id = id.trim();
        (device == camera
            && !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .then(|| format!("plughw:CARD={id},DEV=0"))
    })
}

/// Adds a stream for each detected USB camera not already configured by ID.
fn add_usb_streams(config: &mut Config, cameras: Vec<UsbCamera>) {
    let Some(usb) = config.usb_cameras.clone().filter(|usb| usb.enabled) else {
        return;
    };
    for UsbCamera {
        id,
        device,
        microphone,
    } in cameras
    {
        if config.streams.len() >= 16 {
            break;
        }
        if config.streams.iter().any(|s| s.id == id) {
            continue;
        }
        config.streams.push(StreamConfig {
            id,
            camera: 0,
            width: usb.width,
            height: usb.height,
            fps: usb.fps,
            bitrate: usb.bitrate,
            enabled: true,
            audio_device: microphone.filter(|_| usb.audio),
            usb: Some(UsbSource {
                device,
                input_format: usb.input_format.clone(),
                encoder: usb.encoder.clone(),
            }),
        });
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
        [
            "camera",
            "width",
            "height",
            "fps",
            "bitrate",
            "enabled",
            "audio_device"
        ]
        .contains(&field),
        "unknown setting; use camera, width, height, fps, bitrate, enabled or audio_device"
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
    let fps = stream.fps.to_string();
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    // rpicam-vid already encodes H.264; USB cameras deliver raw/MJPEG frames.
    let video: Vec<String> = if let Some(usb) = &stream.usb {
        let size = format!("{}x{}", stream.width, stream.height);
        args.extend(
            [
                "-thread_queue_size",
                "512",
                "-f",
                "v4l2",
                "-input_format",
                &usb.input_format,
                "-framerate",
                &fps,
                "-video_size",
                &size,
                "-i",
                &usb.device,
            ]
            .into_iter()
            .map(str::to_owned),
        );
        let bitrate = stream.bitrate.to_string();
        let mut video: Vec<String> = ["-c:v", &usb.encoder, "-pix_fmt", "yuv420p"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        if usb.encoder == "libx264" {
            video.extend(
                [
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-profile:v",
                    "baseline",
                    "-maxrate",
                    &bitrate,
                    "-bufsize",
                    &bitrate,
                    "-keyint_min",
                    &fps,
                    "-sc_threshold",
                    "0",
                    "-bf",
                    "0",
                ]
                .into_iter()
                .map(str::to_owned),
            );
        }
        video.extend(
            ["-b:v", &bitrate, "-g", &fps]
                .into_iter()
                .map(str::to_owned),
        );
        video
    } else {
        args.extend(
            [
                "-fflags", "+genpts", "-f", "h264", "-r", &fps, "-i", "pipe:0",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        vec!["-c:v".into(), "copy".into()]
    };
    if let Some(device) = &stream.audio_device {
        args.extend(
            [
                "-thread_queue_size",
                "512",
                "-f",
                "alsa",
                "-i",
                device,
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        args.extend(video);
        args.extend(
            ["-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2"]
                .into_iter()
                .map(str::to_owned),
        );
    } else {
        args.extend(["-map".into(), "0:v:0".into()]);
        args.extend(video);
        args.push("-an".into());
    }
    args.extend(
        [
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
        .map(str::to_owned),
    );
    args
}

struct Pipeline {
    automatic: bool,
    config: Config,
    stream: StreamConfig,
    /// rpicam-vid; absent for USB cameras, which FFmpeg captures directly.
    camera: Option<Child>,
    relay: Child,
    progress: Arc<Mutex<Instant>>,
    progress_task: tokio::task::JoinHandle<()>,
}

impl Pipeline {
    async fn start(
        config: &Config,
        stream: &StreamConfig,
        password: &str,
        automatic: bool,
    ) -> Result<Self> {
        ensure!(
            config.host != "auto",
            "receiver discovery has not completed"
        );
        let mut camera = if stream.usb.is_some() {
            None
        } else {
            Some(
                Command::new("rpicam-vid")
                    .args(camera_args(stream))
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .context("starting rpicam-vid")?,
            )
        };
        let input: Stdio = match camera.as_mut() {
            Some(camera) => camera
                .stdout
                .take()
                .context("camera output pipe missing")?
                .try_into()?,
            None => Stdio::null(),
        };
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
                if let Some(camera) = camera.as_mut() {
                    let _ = camera.kill().await;
                }
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
            automatic,
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
            && self.config.discovery_interface == config.discovery_interface
            && self.config.discovery_port == config.discovery_port
    }

    async fn healthy(&mut self) -> bool {
        self.camera
            .as_mut()
            .is_none_or(|camera| matches!(camera.try_wait(), Ok(None)))
            && matches!(self.relay.try_wait(), Ok(None))
            && self.progress.lock().await.elapsed() < Duration::from_secs(20)
    }

    async fn stop(mut self) {
        if let Some(camera) = self.camera.as_mut() {
            let _ = camera.kill().await;
        }
        let _ = self.relay.kill().await;
        self.progress_task.abort();
    }
}

#[derive(Default, Clone, Serialize)]
struct StreamStatus {
    destination: Option<String>,
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
    let mut discovered = None;
    let mut previous_discovery_interface = None;
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
        let mut config = daemon.changes.borrow().clone();
        // Rescanned every tick so cameras plugged in after boot start too.
        add_usb_streams(
            &mut config,
            detect_usb_cameras(
                Path::new(USB_BY_ID),
                Path::new(USB_BY_PATH),
                Path::new("/sys"),
            ),
        );
        let automatic = config.host == "auto";
        if config.discovery_interface != previous_discovery_interface {
            discovered = None;
            previous_discovery_interface = config.discovery_interface.clone();
        }
        let mut stop = Vec::new();
        for (id, pipeline) in &mut running {
            let wanted = config.streams.iter().find(|s| &s.id == id && s.enabled);
            let mut destination = config.clone();
            if automatic {
                destination.host = pipeline.config.host.clone();
                destination.port = pipeline.config.port;
            }
            if pipeline.automatic != automatic
                || !wanted.is_some_and(|s| pipeline.matches(&destination, s))
            {
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
                discovered = None;
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
        if automatic
            && discovered.is_none()
            && let Some(pipeline) = running.values().next()
        {
            discovered = pipeline
                .config
                .host
                .parse()
                .ok()
                .map(|ip| std::net::SocketAddr::new(ip, pipeline.config.port));
        }
        if automatic && running.is_empty() && config.streams.iter().any(|stream| stream.enabled) {
            // A failed publish discards the old address. Resolve again rather than
            // persisting a DHCP address or endlessly retrying the obsolete peer.
            discovered = match gs_camera_discovery::targets(
                config.discovery_port,
                config.discovery_interface.as_deref(),
            ) {
                Ok(targets) => match gs_camera_discovery::discover(&targets, &password).await {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        eprintln!("camera discovery: {error}");
                        None
                    }
                },
                Err(error) => {
                    eprintln!("camera discovery: {error}");
                    None
                }
            };
        }
        if automatic && let Some(endpoint) = discovered {
            config.host = endpoint.ip().to_string();
            config.port = endpoint.port();
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
            status.destination = if automatic && discovered.is_none() {
                None
            } else {
                Some(format!("{}:{}", config.host, config.port))
            };
            if !stream.enabled {
                status.state = "disabled".into();
            } else if automatic && discovered.is_none() && !running.contains_key(&stream.id) {
                status.state = "discovering groundstation".into();
            } else if running.contains_key(&stream.id) {
                status.state = "running".into();
            } else if retry
                .get(&stream.id)
                .is_some_and(|time| *time > Instant::now())
            {
                status.state = "retrying".into();
            } else {
                status.restarts += 1;
                match Pipeline::start(&config, stream, &password, automatic).await {
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
        Some("advertise") if args.len() == 2 => {
            let port = args[1].parse::<u16>().context("RTSP port must be 1–65535")?;
            ensure!(port != 0, "RTSP port must be 1–65535");
            let secret = std::env::var("GS_VIDEO_PASSWORD").context("GS_VIDEO_PASSWORD is required")?;
            ensure!(!secret.is_empty(), "GS_VIDEO_PASSWORD must not be empty");
            let address = std::env::var("GS_VIDEO_DISCOVERY_BIND")
                .unwrap_or_else(|_| format!("0.0.0.0:{}", gs_camera_discovery::PORT));
            let socket = tokio::net::UdpSocket::bind(&address).await?;
            eprintln!("camera receiver discovery ready on {}; RTSP port {port}", socket.local_addr()?);
            tokio::select! {
                result = gs_camera_discovery::serve(socket, port, &secret) => result.context("camera discovery listener"),
                result = tokio::signal::ctrl_c() => result.context("shutdown signal"),
            }
        }
        Some("daemon") if args.len() == 2 => run(PathBuf::from(&args[1]), socket).await,
        Some("check") if args.len() == 2 => { load_config(Path::new(&args[1])).await?; println!("configuration valid"); Ok(()) },
        Some("status" | "reload") if args.len() == 1 => request(&socket, json!({"op":args[0]})).await,
        Some("set") if args.len() == 4 => request(&socket, json!({"op":"set", "stream":args[1], "field":args[2], "value":serde_json::from_str::<Value>(&args[3]).context("value must be a JSON number or boolean")?})).await,
        _ => { eprintln!("Usage: gs-video-sender daemon CONFIG.json | check CONFIG.json | status | reload | set STREAM FIELD VALUE | advertise RTSP_PORT\nGS_VIDEO_SOCKET overrides the local control socket."); bail!("invalid arguments") }
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
    fn automatic_and_existing_fixed_destinations_validate_without_rewriting_host() {
        let automatic = example();
        assert_eq!(automatic.host, "auto");
        assert_eq!(automatic.discovery_port, 49362);
        automatic.validate().unwrap();
        let mut fixed = automatic.clone();
        fixed.host = "192.168.7.3".into();
        fixed.validate().unwrap();
        let updated = updated_config(&automatic, "front", "fps", json!(25)).unwrap();
        assert_eq!(updated.host, "auto");
        fixed.discovery_port = 0;
        assert!(fixed.validate().is_err());
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

    #[test]
    fn microphone_audio_is_optional_and_keeps_hardware_video_copy() {
        let mut config = example();
        config.streams[0].audio_device = Some("plughw:1,0".into());
        config.validate().unwrap();
        let args = ffmpeg_args(&config, &config.streams[0], "fixture-only");
        for expected in [
            ["-f", "alsa"],
            ["-i", "plughw:1,0"],
            ["-map", "1:a:0"],
            ["-c:v", "copy"],
            ["-c:a", "aac"],
        ] {
            assert!(args.windows(2).any(|pair| pair == expected));
        }
        assert!(!args.contains(&"-an".to_string()));
        config.streams[0].audio_device = Some("-unsafe".into());
        assert!(config.validate().is_err());
        config.streams[0].audio_device = None;
        assert!(
            ffmpeg_args(&config, &config.streams[0], "fixture-only").contains(&"-an".to_string())
        );
    }

    #[test]
    fn usb_link_names_map_to_stable_stream_ids() {
        assert_eq!(
            usb_stream_id("usb-Logitech_HD_Webcam_C270_A1B2-video-index0").as_deref(),
            Some("usb-Logitech_HD_Webcam_C270_A1B2")
        );
        assert_eq!(
            usb_stream_id("platform-xhci-hcd.0-usb-0:1.2:1.0-video-index0").as_deref(),
            Some("usb-port-0_1_2_1_0")
        );
        assert_eq!(usb_stream_id("usb-Cam_A1B2-video-index1"), None);
        assert_eq!(
            usb_stream_id("platform-xhci-hcd.0-usbv2-0:1:1.0-video-index0"),
            None
        );
        assert_eq!(usb_stream_id("platform-1f00128000.csi-video-index0"), None);
        let long = format!("usb-{}-video-index0", "x".repeat(100));
        assert_eq!(usb_stream_id(&long).unwrap().len(), 64);
    }

    #[test]
    fn usb_detection_prefers_serial_names_and_falls_back_to_ports() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("gs-video-usb-{}", std::process::id()));
        let (by_id, by_path) = (root.join("by-id"), root.join("by-path"));
        std::fs::create_dir_all(&by_id).unwrap();
        std::fs::create_dir_all(&by_path).unwrap();
        for node in ["video0", "video1", "video2"] {
            std::fs::write(root.join(node), b"").unwrap();
        }
        symlink(root.join("video0"), by_id.join("usb-Cam_A1-video-index0")).unwrap();
        symlink(root.join("video0"), by_id.join("usb-Cam_A1-video-index1")).unwrap();
        symlink(
            root.join("video0"),
            by_path.join("p-usb-0:1:1.0-video-index0"),
        )
        .unwrap();
        symlink(
            root.join("video1"),
            by_path.join("p-usb-0:2:1.0-video-index0"),
        )
        .unwrap();
        symlink(root.join("video2"), by_path.join("p-csi-video-index0")).unwrap();
        // Fake sysfs: video0 and card1 share USB device 1-1; video1 (1-2) has
        // no microphone; card0 is the Pi's onboard audio.
        let sys = root.join("sys");
        let devices = sys.join("devices");
        for (link, interface) in [
            ("class/video4linux/video0", "usb1/1-1/1-1:1.0"),
            ("class/video4linux/video1", "usb1/1-2/1-2:1.0"),
            ("class/sound/card0", "platform/bcm2835_audio"),
            ("class/sound/card1", "usb1/1-1/1-1:1.2"),
        ] {
            std::fs::create_dir_all(devices.join(interface)).unwrap();
            std::fs::create_dir_all(sys.join(link)).unwrap();
            symlink(devices.join(interface), sys.join(link).join("device")).unwrap();
        }
        std::fs::write(sys.join("class/sound/card0/id"), "Headphones\n").unwrap();
        std::fs::write(sys.join("class/sound/card1/id"), "C920\n").unwrap();
        let found = detect_usb_cameras(&by_id, &by_path, &sys);
        std::fs::remove_dir_all(&root).unwrap();
        let ids: Vec<_> = found.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["usb-Cam_A1", "usb-port-0_2_1_0"]);
        assert!(found[0].device.ends_with("usb-Cam_A1-video-index0"));
        assert_eq!(
            found[0].microphone.as_deref(),
            Some("plughw:CARD=C920,DEV=0")
        );
        assert_eq!(found[1].microphone, None);
    }

    #[test]
    fn usb_cameras_become_encoded_streams_without_overriding_configured_ids() {
        let mut config = example();
        config.validate().unwrap();
        let cameras = vec![
            UsbCamera {
                id: "front".into(),
                device: "/dev/video9".into(),
                microphone: None,
            },
            UsbCamera {
                id: "usb-Cam".into(),
                device: "/dev/v4l/by-id/usb-Cam-video-index0".into(),
                microphone: Some("plughw:CARD=Cam,DEV=0".into()),
            },
        ];
        let mut disabled = config.clone();
        disabled.usb_cameras.as_mut().unwrap().enabled = false;
        add_usb_streams(&mut disabled, cameras.clone());
        assert_eq!(disabled.streams.len(), 1);
        add_usb_streams(&mut config, cameras);
        assert_eq!(config.streams.len(), 2);
        let usb = &config.streams[1];
        assert_eq!(usb.id, "usb-Cam");
        let args = ffmpeg_args(&config, usb, "fixture-only");
        for expected in [
            ["-f", "v4l2"],
            ["-input_format", "mjpeg"],
            ["-video_size", "1280x720"],
            ["-i", "/dev/v4l/by-id/usb-Cam-video-index0"],
            ["-c:v", "libx264"],
            ["-g", "30"],
        ] {
            assert!(args.windows(2).any(|pair| pair == expected));
        }
        assert!(!args.contains(&"pipe:0".to_string()));
        assert!(args.last().unwrap().ends_with(":8554/usb-Cam"));
        for expected in [
            ["-f", "alsa"],
            ["-i", "plughw:CARD=Cam,DEV=0"],
            ["-map", "1:a:0"],
            ["-c:a", "aac"],
        ] {
            assert!(args.windows(2).any(|pair| pair == expected));
        }
        let mut muted = example();
        muted.usb_cameras.as_mut().unwrap().audio = false;
        add_usb_streams(
            &mut muted,
            vec![UsbCamera {
                id: "usb-Cam".into(),
                device: "/dev/video0".into(),
                microphone: Some("plughw:CARD=Cam,DEV=0".into()),
            }],
        );
        assert_eq!(muted.streams[1].audio_device, None);
        assert!(ffmpeg_args(&muted, &muted.streams[1], "fixture-only").contains(&"-an".into()));
        let mut bad = example();
        bad.usb_cameras.as_mut().unwrap().encoder = "h265".into();
        assert!(bad.validate().is_err());
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
