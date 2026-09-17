//! Native video receiver lifecycle. External relays (including Compose) stay external.
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

const VERSION: &str = "1.21.0";
const CONFIG: &str = include_str!("../config/mediamtx.yml");

pub struct VideoRuntime {
    pub password: String,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for VideoRuntime {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort(); // Child uses kill_on_drop, including during early backend exit.
        }
    }
}

fn managed_mode(value: Option<&str>, external: bool) -> Result<bool> {
    match value {
        None | Some("auto") => Ok(!external && cfg!(any(target_os = "linux", target_os = "macos"))),
        Some("0" | "false") => Ok(false),
        Some("1" | "true") => {
            ensure!(
                !external,
                "managed video cannot be combined with GS_VIDEO_*_URL overrides; unset them or set GS_VIDEO_MANAGED=0"
            );
            Ok(true)
        }
        _ => bail!("GS_VIDEO_MANAGED must be auto, 1, or 0"),
    }
}

impl VideoRuntime {
    /// Video failures must not prevent telemetry and hardware control from starting.
    pub fn start() -> Self {
        let password = std::env::var("GS_VIDEO_PASSWORD").unwrap_or_default();
        match Self::try_start(password.clone()) {
            Ok(runtime) => runtime,
            Err(error) => {
                log::error!("video receiver not started: {error:#}");
                Self {
                    password,
                    task: None,
                }
            }
        }
    }

    fn try_start(mut password: String) -> Result<Self> {
        let external = [
            "GS_VIDEO_API_URL",
            "GS_VIDEO_WEBRTC_URL",
            "GS_VIDEO_HLS_URL",
        ]
        .iter()
        .any(|key| std::env::var_os(key).is_some());
        if !managed_mode(std::env::var("GS_VIDEO_MANAGED").ok().as_deref(), external)? {
            log::info!(
                "video receiver lifecycle is external (GS_VIDEO_MANAGED=0 or relay URL override)"
            );
            return Ok(Self {
                password,
                task: None,
            });
        }
        let directory = std::env::var_os("GS_VIDEO_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data/video"));
        std::fs::create_dir_all(&directory)?;
        let directory = std::fs::canonicalize(directory)?;
        if password.is_empty() {
            let path = directory.join("password");
            password = persistent_password(&path)?;
            log::info!(
                "video publishing password stored at {} (user camera)",
                path.display()
            );
        }
        let config = directory.join("mediamtx.yml");
        std::fs::write(&config, CONFIG)?;
        let binary = std::env::var_os("GS_MEDIAMTX_BINARY").map(PathBuf::from);
        let host = std::env::var("GS_VIDEO_HOST").unwrap_or_default();
        let child_password = password.clone();
        let task = tokio::spawn(async move {
            loop {
                let result: Result<()> = async {
                    let executable = match &binary {
                        Some(path) => path.clone(),
                        None => install(&directory).await?,
                    };
                    let mut child = receiver_command(&executable, &config, &child_password, &host)
                        .spawn().context("starting MediaMTX")?;
                    log::info!("managed MediaMTX started; RTSP :8554, WebRTC :8189; relay API/HLS/signaling on loopback");
                    let status = child.wait().await?;
                    bail!("MediaMTX exited with {status}")
                }.await;
                if let Err(error) = result {
                    log::error!("video receiver unavailable: {error:#}; retrying in 30 seconds");
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        Ok(Self {
            password,
            task: Some(task),
        })
    }

    pub async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

fn persistent_password(path: &Path) -> Result<String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            let mut bytes = [0; 32];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
                .map_err(|_| anyhow::anyhow!("generating video secret"))?;
            let password = hex(&bytes);
            writeln!(file, "{password}")?;
            file.sync_all()?;
            Ok(password)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let password = std::fs::read_to_string(path)?
                .trim_end_matches(['\r', '\n'])
                .to_owned();
            ensure!(
                !password.is_empty(),
                "video password file is empty: {}",
                path.display()
            );
            Ok(password)
        }
        Err(error) => Err(error).context("creating private video password file"),
    }
}

fn receiver_command(binary: &Path, config: &Path, password: &str, host: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .arg(config)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .env("MTX_APIADDRESS", "127.0.0.1:9997")
        .env("MTX_WEBRTCADDRESS", "127.0.0.1:8889")
        .env("MTX_HLSADDRESS", "127.0.0.1:8888")
        .env(
            "MTX_PATHDEFAULTS_RECORDPATH",
            crate::media::recordings_directory().join("%path/%Y-%m-%d_%H-%M-%S-%f"),
        )
        .env("MTX_WEBRTCIPSFROMINTERFACES", "yes")
        .env("MTX_WEBRTCADDITIONALHOSTS", host)
        .env("MTX_AUTHINTERNALUSERS_0_USER", "camera")
        .env("MTX_AUTHINTERNALUSERS_0_PASS", password)
        .env("MTX_AUTHINTERNALUSERS_0_PERMISSIONS_0_ACTION", "publish")
        .env("MTX_AUTHINTERNALUSERS_1_USER", "groundstation")
        .env("MTX_AUTHINTERNALUSERS_1_PASS", password)
        .env("MTX_AUTHINTERNALUSERS_1_PERMISSIONS_0_ACTION", "read")
        .env("MTX_AUTHINTERNALUSERS_1_PERMISSIONS_1_ACTION", "api");
    command
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn release(os: &str, arch: &str) -> Result<(&'static str, &'static str)> {
    // SHA-256 digests published with the upstream v1.21.0 release.
    match (os, arch) {
        ("linux", "aarch64") => Ok((
            "linux_arm64",
            "a8113b5928ba1a934b81557b61b8a07954b76921a4b567d54c7f086f8b39d9a2",
        )),
        ("linux", "x86_64") => Ok((
            "linux_amd64",
            "e02e34c3337a35f20ac9e5aa31524566108964e6e37dbc46cf8292169f6c792b",
        )),
        ("macos", "aarch64") => Ok((
            "darwin_arm64",
            "159b8e8164022189e654b61ee8bb7edf2d3ff9354ff06cf4c315bb59f5cc9eca",
        )),
        ("macos", "x86_64") => Ok((
            "darwin_amd64",
            "2ee772efb7e2e365307599e41f849c54539f6019a2ebd4563be9b1256edfeb65",
        )),
        _ => bail!(
            "no bundled MediaMTX release for {os}/{arch}; set GS_MEDIAMTX_BINARY to an installed executable"
        ),
    }
}

fn verify_archive(bytes: &[u8], expected: &str) -> Result<()> {
    ensure!(
        hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref()) == expected,
        "MediaMTX download failed SHA-256 verification"
    );
    Ok(())
}

async fn install(directory: &Path) -> Result<PathBuf> {
    let (platform, digest) = release(std::env::consts::OS, std::env::consts::ARCH)?;
    let executable = directory.join(format!("mediamtx-v{VERSION}-{platform}"));
    if executable.is_file() {
        return Ok(executable);
    }
    let archive_name = format!("mediamtx_v{VERSION}_{platform}.tar.gz");
    log::info!(
        "downloading verified MediaMTX v{VERSION} for {platform}; subsequent starts use the local cache"
    );
    let mut response = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?
        .get(format!(
            "https://github.com/bluenviron/mediamtx/releases/download/v{VERSION}/{archive_name}"
        ))
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 64 * 1024 * 1024,
            "MediaMTX archive exceeds 64 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    verify_archive(&bytes, digest)?;
    let staging = directory.join("install");
    tokio::fs::create_dir_all(&staging).await?;
    let archive = staging.join("release.tar.gz");
    tokio::fs::write(&archive, bytes).await?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .arg("mediamtx")
        .kill_on_drop(true)
        .status()
        .await
        .context("extracting MediaMTX (requires tar)")?;
    ensure!(status.success(), "extracting MediaMTX failed: {status}");
    tokio::fs::rename(staging.join("mediamtx"), &executable).await?;
    let _ = tokio::fs::remove_file(archive).await;
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_relays_are_never_started_implicitly() {
        assert!(!managed_mode(None, true).unwrap());
        assert!(!managed_mode(Some("0"), false).unwrap());
        assert!(managed_mode(Some("1"), true).is_err());
        assert!(managed_mode(Some("1"), false).unwrap());
        assert!(managed_mode(Some("typo"), false).is_err());
    }

    #[test]
    fn corrupted_download_is_rejected() {
        let expected = hex(ring::digest::digest(&ring::digest::SHA256, b"original").as_ref());
        assert!(verify_archive(b"original", &expected).is_ok());
        assert!(verify_archive(b"changed", &expected).is_err());
    }

    #[test]
    fn receiver_credentials_and_private_listeners_are_configured() {
        let command = receiver_command(
            Path::new("mediamtx"),
            Path::new("config.yml"),
            "test-secret",
            "192.168.7.3",
        );
        let env: std::collections::HashMap<_, _> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| (key.to_str().unwrap(), value.unwrap().to_str().unwrap()))
            .collect();
        assert_eq!(env["MTX_APIADDRESS"], "127.0.0.1:9997");
        assert_eq!(env["MTX_HLSADDRESS"], "127.0.0.1:8888");
        assert_eq!(env["MTX_WEBRTCADDRESS"], "127.0.0.1:8889");
        assert_eq!(env["MTX_AUTHINTERNALUSERS_0_PASS"], "test-secret");
        assert_eq!(env["MTX_AUTHINTERNALUSERS_1_PASS"], "test-secret");
        assert_eq!(
            env["MTX_AUTHINTERNALUSERS_0_PERMISSIONS_0_ACTION"],
            "publish"
        );
        assert_eq!(env["MTX_WEBRTCADDITIONALHOSTS"], "192.168.7.3");
        assert!(!command.as_std().get_args().any(|arg| arg == "test-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_the_owned_child() {
        // An inert substitute, never download or run MediaMTX during unit tests.
        let mut command =
            receiver_command(Path::new("/bin/sh"), Path::new("-c"), "test-secret", "");
        let child = command.arg("exec sleep 60").spawn().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut child = child;
            ready_tx.send(()).unwrap();
            let _ = child.wait().await;
        });
        ready_rx.await.unwrap();
        let mut runtime = VideoRuntime {
            password: String::new(),
            task: Some(task),
        };
        runtime.stop().await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned child should be killed and reaped");
    }

    #[test]
    fn generated_secret_survives_restart_and_is_private() {
        let directory = std::env::temp_dir().join(format!(
            "gs-video-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("password");
        let first = persistent_password(&path).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(first, persistent_password(&path).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&path, "").unwrap();
        assert!(persistent_password(&path).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
