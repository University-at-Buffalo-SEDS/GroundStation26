//! Optional per-socket VPN bypass. Never changes routes or interface configuration.
use reqwest::{Client, Response};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(crate) const MAX_TILE_BYTES: usize = 512 * 1024;

const DIRECT_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone)]
pub(crate) struct TileClient {
    normal: Client,
    direct: Option<Client>,
    failed: Arc<AtomicBool>,
}

impl From<Client> for TileClient {
    fn from(normal: Client) -> Self {
        Self {
            normal,
            direct: None,
            failed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl TileClient {
    pub(crate) async fn configured(probe_url: &str) -> anyhow::Result<Self> {
        let normal = builder().build()?;
        let mut selected = Self::from(normal);
        let Some(requested) = std::env::var_os("MAP_DIRECT_INTERFACE") else {
            return Ok(selected);
        };
        let Some(requested) = requested.to_str().filter(|s| valid_interface(s)) else {
            eprintln!(
                "Direct map download unavailable: invalid interface name; using normal network."
            );
            return Ok(selected);
        };
        let interface = if requested == "auto" {
            detect_interface()
        } else {
            Some(requested.to_owned())
        };
        let Some(interface) = interface else {
            eprintln!(
                "Direct map download unavailable: no physical default interface found; using normal network."
            );
            return Ok(selected);
        };
        let Some(direct) = direct_client(&interface) else {
            eprintln!(
                "Direct map download unavailable on this platform or interface; using normal network."
            );
            return Ok(selected);
        };
        // Probe once, including the response body, before launching concurrent tiles.
        // Do not print reqwest errors: they may contain URL/proxy credentials.
        let ready = tokio::time::timeout(DIRECT_TIMEOUT, async {
            let response = direct.get(probe_url).send().await?.error_for_status()?;
            tile_body(response).await
        })
        .await
        .is_ok_and(|result| result.is_ok());
        if ready {
            println!(
                "Map downloads bound to interface {interface}; normal network fallback enabled."
            );
            selected.direct = Some(direct);
        } else {
            eprintln!("Direct map download probe failed; using normal network for this run.");
        }
        Ok(selected)
    }

    pub(crate) async fn send(&self, url: &str) -> reqwest::Result<Response> {
        if let Some(direct) = self
            .direct
            .as_ref()
            .filter(|_| !self.failed.load(Ordering::Relaxed))
        {
            match direct.get(url).send().await {
                Ok(response) => return Ok(response),
                Err(_) => self.direct_failed(),
            }
        }
        self.normal.get(url).send().await
    }

    pub(crate) fn direct_failed(&self) {
        if self.direct.is_some() && !self.failed.swap(true, Ordering::Relaxed) {
            eprintln!("Direct map connection failed; remaining downloads use the normal network.");
        }
    }
}

#[derive(Debug)]
pub(crate) struct OversizedTile;
impl std::fmt::Display for OversizedTile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tile response exceeds512KiB")
    }
}
impl std::error::Error for OversizedTile {}

pub(crate) async fn tile_body(mut response: Response) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TILE_BYTES as u64)
    {
        return Err(OversizedTile.into());
    }
    let mut bytes = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_TILE_BYTES.saturating_sub(bytes.len()) {
            return Err(OversizedTile.into());
        }
        bytes.reserve_exact(chunk.len());
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn builder() -> reqwest::ClientBuilder {
    Client::builder()
        .user_agent("GroundStationOfflineTileFetcher/0.1")
        .dns_resolver(crate::dns::CachedResolver::system())
}

fn direct_client(interface: &str) -> Option<Client> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        builder()
            .interface(interface)
            .no_proxy()
            .timeout(DIRECT_TIMEOUT)
            .build()
            .ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = interface;
        None
    }
}

fn valid_interface(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':'))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn physical_candidate(name: &str) -> bool {
    valid_interface(name)
        && ![
            "lo",
            "utun",
            "tun",
            "tap",
            "wg",
            "ppp",
            "ipsec",
            "tailscale",
            "docker",
            "veth",
            "br-",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(target_os = "linux")]
fn detect_interface() -> Option<String> {
    linux_default(&std::fs::read_to_string("/proc/net/route").ok()?)
}
#[cfg(target_os = "macos")]
fn detect_interface() -> Option<String> {
    let result = std::process::Command::new("/usr/sbin/netstat")
        .args(["-rn", "-f", "inet"])
        .output()
        .ok()?;
    result
        .status
        .success()
        .then(|| macos_default(&String::from_utf8_lossy(&result.stdout)))
        .flatten()
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_interface() -> Option<String> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn linux_default(routes: &str) -> Option<String> {
    routes
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 8
                || fields[1] != "00000000"
                || fields[7] != "00000000"
                || !physical_candidate(fields[0])
            {
                return None;
            }
            let flags = u32::from_str_radix(fields[3], 16).ok()?;
            if flags & 1 == 0 || flags & 0x200 != 0 {
                return None;
            }
            Some((fields[6].parse::<u32>().ok()?, fields[0]))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, name)| name.to_owned())
}

#[cfg(any(target_os = "macos", test))]
fn macos_default(routes: &str) -> Option<String> {
    let mut netif = None;
    for line in routes.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.first() == Some(&"Destination") {
            netif = fields.iter().position(|field| *field == "Netif");
            continue;
        }
        if fields.first() != Some(&"default") {
            continue;
        }
        let name = *fields.get(netif?)?;
        if physical_candidate(name)
            && fields
                .get(2)
                .is_some_and(|flags| flags.contains('U') && !flags.contains('R'))
        {
            return Some(name.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn failed_direct_transport_falls_back_once_across_clones() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let direct = TcpListener::bind("127.0.0.1:0").unwrap();
        let direct_addr = direct.local_addr().unwrap();
        let normal = TcpListener::bind("127.0.0.1:0").unwrap();
        let normal_addr = normal.local_addr().unwrap();
        let direct_thread = std::thread::spawn(move || {
            let (mut stream, _) = direct.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            // Connection closes before headers; normal routing must recover.
        });
        let normal_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = normal.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .unwrap();
            }
        });
        let name = "map-direct-test.invalid";
        let client = TileClient {
            normal: Client::builder()
                .no_proxy()
                .resolve(name, normal_addr)
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            direct: Some(
                Client::builder()
                    .no_proxy()
                    .resolve(name, direct_addr)
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap(),
            ),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let peer = client.clone();
        let url = format!("http://{name}/tile");
        assert_eq!(client.send(&url).await.unwrap().text().await.unwrap(), "ok");
        assert!(peer.failed.load(Ordering::Relaxed));
        // The direct listener is gone. A clone must retain the fallback latch.
        assert_eq!(peer.send(&url).await.unwrap().text().await.unwrap(), "ok");
        direct_thread.join().unwrap();
        normal_thread.join().unwrap();
    }

    #[test]
    fn linux_selects_lowest_up_physical_default() {
        let routes = "Iface Destination Gateway Flags RefCnt Use Metric Mask\ntun0 00000000 0 0001 0 0 0 00000000\neth0 00000000 0 0001 0 0 100 00000000\nwlan0 00000000 0 0001 0 0 600 00000000\neth1 00000000 0 0000 0 0 1 00000000\n";
        assert_eq!(linux_default(routes).as_deref(), Some("eth0"));
        assert!(linux_default("eth0 00000000 0 0001 0 0 1 FFFFFFFF").is_none());
    }
    #[test]
    fn macos_handles_expire_column_and_skips_tunnels() {
        let routes = "Destination Gateway Flags Netif Expire\ndefault link#1 UCS utun4\ndefault 192.0.2.1 UGScg en0\n";
        assert_eq!(macos_default(routes).as_deref(), Some("en0"));
        assert!(macos_default("Destination Gateway Flags Netif\ndefault 1 UGS utun0").is_none());
    }
    #[test]
    fn interface_names_cannot_be_commands() {
        for name in ["", "en0;id", "-n get default", "en0\n", "$(id)"] {
            assert!(!valid_interface(name));
        }
        for name in ["auto", "en0", "eth0.12", "enp2s0", "wlan0"] {
            assert!(valid_interface(name));
        }
    }
    #[tokio::test]
    async fn tile_bodies_are_bounded_with_or_without_content_length() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        for oversized_header in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                if oversized_header {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        MAX_TILE_BYTES + 1
                    );
                } else {
                    let _=stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    let _ = write!(stream, "{:x}\r\n", MAX_TILE_BYTES);
                    let _ = stream.write_all(&vec![0; MAX_TILE_BYTES]);
                    let _ = stream.write_all(b"\r\n1\r\nx\r\n0\r\n\r\n");
                }
            });
            let response = Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap()
                .get(format!("http://{addr}/tile"))
                .send()
                .await
                .unwrap();
            assert!(tile_body(response).await.unwrap_err().is::<OversizedTile>());
            server.join().unwrap();
        }
    }
}
