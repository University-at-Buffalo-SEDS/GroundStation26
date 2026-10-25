//! Optional per-socket VPN bypass. Never changes routes or interface configuration.
use reqwest::{Client, Response, StatusCode};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const MAX_TILE_BYTES: usize = 512 * 1024;
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
const DIRECT_TIMEOUT: Duration = Duration::from_secs(30);
const DIRECT_COOLDOWN_MS: u64 = 30_000;
const DIRECT_FAILURE_THRESHOLD: u8 = 3;

#[derive(Clone, Copy)]
struct Ticket {
    direct: bool,
    generation: u64,
}
#[derive(Default)]
struct Policy {
    generation: u64,
    failures: u8,
    retry_at: Option<u64>,
    probe: bool,
}
impl Policy {
    fn acquire(&mut self, now: u64) -> Ticket {
        let direct = match self.retry_at {
            None => true,
            Some(at) if now >= at && !self.probe => {
                self.probe = true;
                true
            }
            Some(_) => false,
        };
        Ticket {
            direct,
            generation: self.generation,
        }
    }
    fn success(&mut self, t: Ticket) -> bool {
        if !t.direct || t.generation != self.generation {
            return false;
        }
        self.failures = 0;
        let recovered = self.retry_at.take().is_some();
        if recovered {
            self.probe = false;
            self.generation = self.generation.wrapping_add(1);
        }
        recovered
    }
    fn failure(&mut self, t: Ticket, now: u64) -> bool {
        if !t.direct || t.generation != self.generation {
            return false;
        }
        self.failures = self.failures.saturating_add(1);
        if self.retry_at.is_some() || self.failures >= DIRECT_FAILURE_THRESHOLD {
            self.retry_at = Some(now.saturating_add(DIRECT_COOLDOWN_MS));
            self.probe = false;
            self.generation = self.generation.wrapping_add(1);
            return true;
        }
        false
    }
}
struct Health {
    state: Mutex<Policy>,
    started: Instant,
}
impl Health {
    fn new() -> Self {
        Self {
            state: Mutex::new(Policy::default()),
            started: Instant::now(),
        }
    }
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Policy> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}
// Own a direct attempt through body completion. Cancellation releases a half-open
// probe too, so a dropped request cannot disable future direct retries forever.
struct Attempt {
    health: Arc<Health>,
    ticket: Ticket,
    done: bool,
}
impl Attempt {
    fn success(mut self) {
        self.done = true;
        if self.health.lock().success(self.ticket) {
            println!("Direct map downloads restored.");
        }
    }
    fn failure(mut self) {
        self.done = true;
        self.failed();
    }
    fn failed(&self) {
        if self.health.lock().failure(self.ticket, self.health.now()) {
            eprintln!(
                "Direct map downloads temporarily unavailable; using normal network and retrying direct after 30 seconds."
            );
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.done {
            self.failed();
        }
    }
}

#[derive(Clone)]
pub(crate) struct TileClient {
    normal: Client,
    direct: Option<Client>,
    health: Arc<Health>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Direct,
    Normal,
    NormalFallback,
}
impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Normal => "normal",
            Self::NormalFallback => "normal-fallback",
        }
    }
}
pub(crate) struct TileResponse {
    pub status: StatusCode,
    pub bytes: Vec<u8>,
    pub route: Route,
    pub direct_failed: bool,
}
impl From<Client> for TileClient {
    fn from(normal: Client) -> Self {
        Self {
            normal,
            direct: None,
            health: Arc::new(Health::new()),
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
        // Retain the existing one-time startup check. Recovery never downloads probes:
        // one ordinary requested tile becomes the shared half-open attempt.
        let ready = tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, async {
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
            eprintln!(
                "Direct map download probe failed; using normal network and retrying direct after 30 seconds."
            );
            selected.health.lock().retry_at =
                Some(selected.health.now().saturating_add(DIRECT_COOLDOWN_MS));
            selected.direct = Some(direct);
        }
        Ok(selected)
    }
    pub(crate) async fn fetch(&self, url: &str) -> anyhow::Result<TileResponse> {
        let mut direct_failed = false;
        if let Some(direct) = &self.direct {
            let ticket = self.health.lock().acquire(self.health.now());
            if ticket.direct {
                let attempt = Attempt {
                    health: self.health.clone(),
                    ticket,
                    done: false,
                };
                match fetch_response(direct, url, Route::Direct).await {
                    Ok(response) => {
                        attempt.success();
                        return Ok(response);
                    }
                    Err(error) if error.is::<OversizedTile>() => {
                        attempt.success();
                        return Err(error);
                    }
                    Err(_) => {
                        direct_failed = true;
                        attempt.failure();
                    }
                }
            }
        }
        let route = if self.direct.is_some() {
            Route::NormalFallback
        } else {
            Route::Normal
        };
        let mut response = fetch_response(&self.normal, url, route).await?;
        response.direct_failed = direct_failed;
        Ok(response)
    }
}
async fn fetch_response(client: &Client, url: &str, route: Route) -> anyhow::Result<TileResponse> {
    let response = client.get(url).send().await?;
    let status = response.status();
    // HTTP responses describe tile availability/server load, not routing failure.
    // Avoid downloading error bodies and preserve existing retry policy at caller.
    let bytes = if status.is_success() {
        tile_body(response).await?
    } else {
        Vec::new()
    };
    Ok(TileResponse {
        status,
        bytes,
        route,
        direct_failed: false,
    })
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
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent("GroundStationOfflineTileFetcher/0.1")
        .dns_resolver(crate::dns::CachedResolver::system())
}

fn direct_client(interface: &str) -> Option<Client> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        builder()
            .interface(interface)
            .no_proxy()
            .connect_timeout(DIRECT_CONNECT_TIMEOUT)
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

#[cfg(test)]
mod policy_tests {
    use super::*;
    fn open(p: &mut Policy) {
        for _ in 0..3 {
            let t = p.acquire(0);
            p.failure(t, 0);
        }
        assert!(p.retry_at.is_some());
    }
    #[test]
    fn one_transient_keeps_direct() {
        let mut p = Policy::default();
        let t = p.acquire(0);
        p.failure(t, 1);
        assert!(p.acquire(2).direct);
    }
    #[test]
    fn body_success_resets_failure_streak() {
        let mut p = Policy::default();
        for _ in 0..2 {
            let t = p.acquire(0);
            p.failure(t, 0);
        }
        let t = p.acquire(1);
        p.success(t);
        for _ in 0..2 {
            let t = p.acquire(2);
            p.failure(t, 2);
        }
        assert!(p.acquire(3).direct);
    }
    #[test]
    fn normal_body_errors_never_poison_direct() {
        let mut p = Policy::default();
        p.failure(
            Ticket {
                direct: false,
                generation: 0,
            },
            0,
        );
        assert_eq!(p.failures, 0);
        assert!(p.acquire(0).direct);
    }
    #[test]
    fn threshold_cools_then_one_probe_recovers() {
        let mut p = Policy::default();
        open(&mut p);
        assert!(!p.acquire(29999).direct);
        let probe = p.acquire(30000);
        assert!(probe.direct);
        for _ in 0..1024 {
            assert!(!p.acquire(30000).direct)
        }
        p.success(probe);
        assert!(p.acquire(30001).direct);
    }
    #[test]
    fn failed_probe_reopens_cooldown() {
        let mut p = Policy::default();
        open(&mut p);
        let t = p.acquire(30000);
        p.failure(t, 30000);
        assert!(!p.acquire(59999).direct);
        assert!(p.acquire(60000).direct);
    }
    #[test]
    fn stale_success_cannot_close_new_circuit() {
        let mut p = Policy::default();
        let stale = p.acquire(0);
        open(&mut p);
        p.success(stale);
        assert!(!p.acquire(1).direct);
    }
    #[test]
    fn stale_failures_cannot_reopen_recovered_circuit() {
        let mut p = Policy::default();
        let stale = p.acquire(0);
        open(&mut p);
        let probe = p.acquire(30000);
        p.success(probe);
        for _ in 0..1024 {
            p.failure(stale, 30001);
        }
        assert!(p.acquire(30001).direct);
    }
    #[test]
    fn in_flight_failures_cannot_multiply_probe_or_extend_cooldown() {
        let mut p = Policy::default();
        let stale = p.acquire(0);
        open(&mut p);
        let at = p.retry_at;
        for _ in 0..1024 {
            p.failure(stale, 10000);
        }
        assert_eq!(p.retry_at, at);
    }
}
#[cfg(test)]
mod recovery_transport_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
    };
    fn server(
        failures: usize,
        requests: usize,
        partial: bool,
    ) -> (
        std::net::SocketAddr,
        Arc<AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while seen.load(Ordering::SeqCst) < requests && Instant::now() < deadline {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                // Accepted sockets inherit nonblocking mode on macOS.
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while request.len() < 8192 && !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let read = socket.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let index = seen.fetch_add(1, Ordering::SeqCst);
                if index < failures {
                    if partial {
                        let _=socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nbad");
                    }
                } else {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
        });
        (address, count, thread)
    }
    fn client(direct: std::net::SocketAddr, normal: std::net::SocketAddr) -> TileClient {
        let make = |address| {
            Client::builder()
                .no_proxy()
                .resolve("tile.invalid", address)
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap()
        };
        TileClient {
            normal: make(normal),
            direct: Some(make(direct)),
            health: Arc::new(Health::new()),
        }
    }
    #[tokio::test]
    async fn transient_body_failure_recovers_next_requested_tile_across_clones() {
        let (direct, ds, dt) = server(1, 2, true);
        let (normal, ns, nt) = server(0, 1, false);
        let client = client(direct, normal);
        let peer = client.clone();
        let fallback = client.fetch("http://tile.invalid/tile").await.unwrap();
        assert_eq!(fallback.bytes, b"ok");
        assert_eq!(fallback.route, Route::NormalFallback);
        assert!(fallback.direct_failed);
        let recovered = peer.fetch("http://tile.invalid/tile2").await.unwrap();
        assert_eq!(recovered.bytes, b"ok");
        assert_eq!(recovered.route, Route::Direct);
        assert!(!recovered.direct_failed);
        dt.join().unwrap();
        nt.join().unwrap();
        assert_eq!(ds.load(Ordering::SeqCst), 2);
        assert_eq!(ns.load(Ordering::SeqCst), 1);
        assert_eq!(client.health.lock().failures, 0);
    }
    #[tokio::test]
    async fn cooldown_and_recovery_use_only_requested_tiles_no_probe_downloads() {
        let (direct, ds, dt) = server(3, 4, false);
        let (normal, ns, nt) = server(0, 4, false);
        let client = client(direct, normal);
        for _ in 0..4 {
            assert_eq!(
                client
                    .fetch("http://tile.invalid/tile")
                    .await
                    .unwrap()
                    .bytes,
                b"ok"
            )
        }
        assert_eq!(ds.load(Ordering::SeqCst), 3);
        assert_eq!(ns.load(Ordering::SeqCst), 4);
        client.health.lock().retry_at = Some(client.health.now());
        assert_eq!(
            client
                .fetch("http://tile.invalid/tile")
                .await
                .unwrap()
                .bytes,
            b"ok"
        );
        dt.join().unwrap();
        nt.join().unwrap();
        assert_eq!(ds.load(Ordering::SeqCst), 4);
        assert_eq!(ns.load(Ordering::SeqCst), 4);
        assert!(client.health.lock().retry_at.is_none());
    }
    #[test]
    fn canceled_half_open_attempt_releases_probe_and_preserves_cooldown() {
        let health = Arc::new(Health::new());
        {
            let mut state = health.lock();
            state.retry_at = Some(0);
        }
        let ticket = health.lock().acquire(0);
        assert!(ticket.direct);
        assert!(health.lock().probe);
        drop(Attempt {
            health: health.clone(),
            ticket,
            done: false,
        });
        assert!(!health.lock().probe);
        assert!(
            health
                .lock()
                .retry_at
                .is_some_and(|at| at >= DIRECT_COOLDOWN_MS)
        );
    }
    #[tokio::test]
    async fn normal_fallback_body_failure_does_not_poison_or_extend_direct_cooldown() {
        let (direct, _ds, dt) = server(0, 0, false);
        let (normal, ns, nt) = server(1, 2, true);
        let client = client(direct, normal);
        {
            let mut state = client.health.lock();
            state.retry_at = Some(client.health.now() + DIRECT_COOLDOWN_MS);
            state.failures = 3;
        }
        let before = {
            let state = client.health.lock();
            (state.generation, state.retry_at, state.failures)
        };
        assert!(client.fetch("http://tile.invalid/tile").await.is_err());
        let after = {
            let state = client.health.lock();
            (state.generation, state.retry_at, state.failures)
        };
        assert_eq!(before, after);
        let recovered = client.fetch("http://tile.invalid/tile2").await.unwrap();
        assert_eq!(recovered.route, Route::NormalFallback);
        assert!(!recovered.direct_failed);
        nt.join().unwrap();
        dt.join().unwrap();
        assert_eq!(ns.load(Ordering::SeqCst), 2);
    }
}
