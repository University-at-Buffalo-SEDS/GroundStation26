//! Shared native/Docker HTTP + HTTPS listeners for GroundStation and RRLS.
use anyhow::{Context, Result, bail};
use axum::Router;
use axum_server::{Handle, tls_rustls::RustlsConfig};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone)]
pub struct Config {
    http: SocketAddr,
    https: Option<SocketAddr>,
    directory: PathBuf,
    custom_pair: Option<(PathBuf, PathBuf)>,
    names: Vec<String>,
}
impl Config {
    pub fn from_env() -> Result<Self> {
        let http = std::env::var("GS_BIND_ADDRESS")
            .unwrap_or_else(|_| "0.0.0.0:3000".into())
            .parse()
            .context("invalid GS_BIND_ADDRESS")?;
        let address =
            std::env::var("GS_HTTPS_BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:443".into());
        let https = if address == "off" {
            None
        } else {
            Some(
                address
                    .parse()
                    .context("invalid GS_HTTPS_BIND_ADDRESS (use off to disable)")?,
            )
        };
        let custom_pair = match (
            std::env::var_os("GS_TLS_CERT"),
            std::env::var_os("GS_TLS_KEY"),
        ) {
            (None, None) => None,
            (Some(cert), Some(key)) => Some((cert.into(), key.into())),
            _ => bail!("set both GS_TLS_CERT and GS_TLS_KEY, or neither"),
        };
        let mut names = vec![
            "localhost".into(),
            "groundstation.local".into(),
            "rrls.local".into(),
            "127.0.0.1".into(),
            "::1".into(),
        ];
        let extra_names = std::env::var("GS_TLS_NAMES").unwrap_or_default();
        let extra_names: Vec<_> = extra_names
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();
        // Explicit names avoid rotating a Docker identity when its private IP changes.
        if extra_names.is_empty() {
            if let Ok(interfaces) = if_addrs::get_if_addrs() {
                names.extend(
                    interfaces
                        .into_iter()
                        .map(|interface| interface.ip().to_string()),
                );
            }
        } else {
            names.extend(extra_names);
        }
        names.sort();
        names.dedup();
        Ok(Self {
            http,
            https,
            custom_pair,
            names,
            directory: std::env::var_os("GS_TLS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "backend/data/tls".into()),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Identity {
    names: Vec<String>,
    renew_at: i64,
    certificate_hash: String,
    key_hash: String,
}
fn hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}
fn identity(config: &Config) -> Result<(PathBuf, PathBuf)> {
    if let Some(pair) = &config.custom_pair {
        return Ok(pair.clone());
    }
    let dir = &config.directory;
    std::fs::create_dir_all(dir).context("create persistent HTTPS certificate directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let manifest = dir.join("identity.json");
    let now = time::OffsetDateTime::now_utc();
    let saved = std::fs::read(&manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Identity>(&bytes).ok());
    if let (Some(saved), Ok(bytes)) = (saved, std::fs::read(&cert)) {
        if key.is_file()
            && saved.names == config.names
            && saved.renew_at > now.unix_timestamp()
            && saved.certificate_hash == hash(&bytes)
            && std::fs::read(&key).is_ok_and(|bytes| saved.key_hash == hash(&bytes))
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
            }
            return Ok((cert, key));
        }
    }
    let mut params = rcgen::CertificateParams::new(config.names.clone())?;
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(397);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "GroundStation / RRLS");
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let signing_key = rcgen::KeyPair::generate()?;
    let certificate = params.self_signed(&signing_key)?.pem();
    let saved = Identity {
        names: config.names.clone(),
        renew_at: (now + time::Duration::days(367)).unix_timestamp(),
        certificate_hash: hash(certificate.as_bytes()),
        key_hash: hash(signing_key.serialize_pem().as_bytes()),
    };
    // Restrict permissions before writing keys; publish complete files by rename.
    // An incomplete rotation is detected through the manifest on the next start.
    let staging = dir.join(format!(
        ".new-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        write_private(
            &staging.join("key.pem"),
            signing_key.serialize_pem().as_bytes(),
        )?;
        write_private(&staging.join("cert.pem"), certificate.as_bytes())?;
        write_private(&staging.join("identity.json"), &serde_json::to_vec(&saved)?)?;
        std::fs::rename(staging.join("key.pem"), &key)?;
        std::fs::rename(staging.join("cert.pem"), &cert)?;
        std::fs::rename(staging.join("identity.json"), &manifest)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(staging);
    result?;
    log::info!(
        "generated persistent self-signed HTTPS certificate at {} (client trust required)",
        cert.display()
    );
    Ok((cert, key))
}

pub async fn serve(app: Router, config: Config, shutdown: impl Future<Output = ()>) -> Result<()> {
    // Both crypto providers can exist through dependencies. Choose explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls = if config.https.is_some() {
        let config = config.clone();
        let (cert, key) = tokio::task::spawn_blocking(move || identity(&config)).await??;
        Some(
            RustlsConfig::from_pem_file(cert, key)
                .await
                .context("load HTTPS certificate/key")?,
        )
    } else {
        None
    };
    // Bind both before serving: a failed HTTPS bind must not leave HTTP running alone.
    let http =
        std::net::TcpListener::bind(config.http).context("bind HTTP listener (GS_BIND_ADDRESS)")?;
    http.set_nonblocking(true)?;
    let https = if let Some(address) = config.https {
        let listener = std::net::TcpListener::bind(address)
            .context("bind HTTPS listener (GS_HTTPS_BIND_ADDRESS)")?;
        listener.set_nonblocking(true)?;
        log::info!("HTTPS listening on {}", listener.local_addr()?);
        Some(listener)
    } else {
        None
    };
    log::info!("HTTP listening on {}", http.local_addr()?);
    struct DiscoveryGuard(tokio::task::JoinHandle<()>);
    impl Drop for DiscoveryGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let pairing = crate::station_pairing::Pairing::load()?;
    let (scheme, address) = if let Some(listener) = &https {
        ("https", listener.local_addr()?)
    } else {
        ("http", http.local_addr()?)
    };
    let _discovery = DiscoveryGuard(tokio::spawn(async move {
        if let Err(error) = crate::station_pairing::serve_discovery(pairing, scheme, address).await
        {
            log::warn!("station LAN discovery unavailable: {error}");
        }
    }));
    let http_handle = Handle::new();
    let https_handle = Handle::new();
    let http_server = axum_server::from_tcp(http)?
        .handle(http_handle.clone())
        .serve(app.clone().into_make_service());
    let https_server = async {
        if let (Some(listener), Some(tls)) = (https, tls) {
            axum_server::from_tcp_rustls(listener, tls)?
                .handle(https_handle.clone())
                .serve(app.into_make_service())
                .await
        } else {
            std::future::pending::<std::io::Result<()>>().await
        }
    };
    tokio::pin!(http_server, https_server, shutdown);
    tokio::select! {
        result = &mut http_server => { https_handle.shutdown(); result.context("HTTP server")?; }
        result = &mut https_server => { http_handle.shutdown(); result.context("HTTPS server")?; }
        _ = &mut shutdown => {
            http_handle.graceful_shutdown(Some(Duration::from_secs(5)));
            https_handle.graceful_shutdown(Some(Duration::from_secs(5)));
            if config.https.is_some() { tokio::try_join!(&mut http_server, &mut https_server)?; }
            else { http_server.await?; }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config {
            http: "127.0.0.1:0".parse().unwrap(),
            https: Some("127.0.0.1:0".parse().unwrap()),
            directory: std::env::temp_dir().join(format!("gs-https-{}", rand::random::<u64>())),
            names: vec!["localhost".into(), "127.0.0.1".into()],
            custom_pair: None,
        }
    }
    #[test]
    fn identity_persists_and_rotates_for_new_names_and_expiry() {
        let mut c = config();
        let (cert, key) = identity(&c).unwrap();
        let original = std::fs::read(&cert).unwrap();
        identity(&c).unwrap();
        assert_eq!(original, std::fs::read(&cert).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        c.names.push("rrls.local".into());
        identity(&c).unwrap();
        let rotated = std::fs::read(&cert).unwrap();
        assert_ne!(original, rotated);
        let path = c.directory.join("identity.json");
        let mut saved: Identity = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        saved.renew_at = 0;
        std::fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
        identity(&c).unwrap();
        assert_ne!(rotated, std::fs::read(&cert).unwrap());
        // Interrupted rotation / damaged key: regenerate a coherent pair.
        std::fs::write(&key, b"damaged").unwrap();
        identity(&c).unwrap();
        assert_ne!(std::fs::read(&key).unwrap(), b"damaged");
        std::fs::remove_file(key).unwrap();
        identity(&c).unwrap();
        std::fs::remove_dir_all(c.directory).unwrap();
    }
    #[test]
    fn supplied_certificates_are_not_overwritten() {
        let mut c = config();
        c.custom_pair = Some(("operator.crt".into(), "operator.key".into()));
        assert_eq!(identity(&c).unwrap(), c.custom_pair.unwrap());
        assert!(!c.directory.exists());
    }
    #[tokio::test]
    async fn unavailable_https_port_fails_without_serving_http() {
        let mut c = config();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        c.https = Some(occupied.local_addr().unwrap());
        let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        c.http = reserve.local_addr().unwrap();
        drop(reserve);
        assert!(
            serve(Router::new(), c.clone(), std::future::pending())
                .await
                .is_err()
        );
        assert!(tokio::net::TcpStream::connect(c.http).await.is_err());
        std::fs::remove_dir_all(c.directory).unwrap();
    }
    #[tokio::test]
    async fn http_only_does_not_create_a_certificate() {
        let mut c = config();
        c.https = None;
        serve(Router::new(), c.clone(), async {}).await.unwrap();
        assert!(!c.directory.exists());
    }
    #[tokio::test]
    async fn both_transports_serve_api_and_websocket_and_stop_together() {
        use axum::{
            extract::ws::{Message, WebSocketUpgrade},
            routing::get,
        };
        let mut c = config();
        // Reserve ephemeral ports before handing them to the server.
        let reserve_http = std::net::TcpListener::bind(c.http).unwrap();
        c.http = reserve_http.local_addr().unwrap();
        let reserve_https = std::net::TcpListener::bind(c.https.unwrap()).unwrap();
        c.https = Some(reserve_https.local_addr().unwrap());
        let (cert, _) = identity(&c).unwrap();
        let client = reqwest::Client::builder()
            .http1_only()
            .add_root_certificate(
                reqwest::Certificate::from_pem(&std::fs::read(cert).unwrap()).unwrap(),
            )
            .build()
            .unwrap();
        let app = Router::new()
            .route("/api/rrls/status", get(|| async { "healthy" }))
            .route(
                "/ws",
                get(|ws: WebSocketUpgrade| async move {
                    ws.on_upgrade(|mut socket| async move {
                        let _ = socket.send(Message::Text("ready".into())).await;
                    })
                }),
            );
        let (stop, rx) = tokio::sync::oneshot::channel();
        drop(reserve_http);
        drop(reserve_https);
        let task = tokio::spawn(serve(app, c.clone(), async {
            let _ = rx.await;
        }));
        for _ in 0..100 {
            if client
                .get(format!("https://{}/api/rrls/status", c.https.unwrap()))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A peer which never starts TLS must not hold up other clients.
        let _stalled = tokio::net::TcpStream::connect(c.https.unwrap())
            .await
            .unwrap();
        for origin in [
            format!("http://{}", c.http),
            format!("https://{}", c.https.unwrap()),
        ] {
            assert_eq!(
                client
                    .get(format!("{origin}/api/rrls/status"))
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
                "healthy"
            );
            let response = client
                .get(format!("{origin}/ws"))
                .header("Connection", "Upgrade")
                .header("Upgrade", "websocket")
                .header("Sec-WebSocket-Version", "13")
                .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 101);
            // Verify traffic after the upgrade, not just handshake acceptance.
            use tokio::io::AsyncReadExt;
            let mut socket = response.upgrade().await.unwrap();
            let mut frame = [0; 7];
            tokio::time::timeout(Duration::from_secs(1), socket.read_exact(&mut frame))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&frame, b"\x81\x05ready");
        }
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(7), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(c.directory).unwrap();
    }
}
