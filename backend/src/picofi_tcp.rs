//! Pico-Fi's existing PICOFI1 / B5 4E u16LE Ethernet protocol.
//! Socket work never blocks the router. TCP backpressure bounds both queues.
use super::*;
use crate::comms_config::{PicoFiTcpConfig, TcpMode};
use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
};
#[path = "picofi_discovery_wire.rs"]
mod discovery_wire;
#[path = "picofi_lan.rs"]
mod lan;
use lan::LanDiscovery;
const HELLO: &[u8; 7] = b"PICOFI1";
const MAX: usize = 4096;
const DEPTH: usize = 128;
const STALL: Duration = Duration::from_millis(500);
struct Shared {
    connected: AtomicBool,
    generation: AtomicU64,
    tx: SyncSender<(u64, Vec<u8>)>,
}
pub struct PicoFiTcp {
    shared: Arc<Shared>,
    rx: Arc<Mutex<Receiver<Vec<u8>>>>,
    side: Option<RouterSideId>,
    exhausted: bool,
}
impl PicoFiTcp {
    pub fn open(config: &PicoFiTcpConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !config.discovery || config.discovery_port != 0,
            "Discovery UDP port must be nonzero (default {})",
            discovery_wire::PORT
        );
        let address: SocketAddr = config
            .address
            .parse()
            .context("Pico-Fi address must be a numeric IP:port")?;
        anyhow::ensure!(
            address.port() != 0 || config.discovery,
            "Manual Pico-Fi TCP port must be nonzero"
        );
        anyhow::ensure!(
            !config.discovery || address.is_ipv4(),
            "Pico-Fi discovery requires an IPv4 Ethernet address"
        );
        let listener = if config.mode == TcpMode::Server {
            let l = TcpListener::bind(address)?;
            l.set_nonblocking(true)?;
            Some(l)
        } else {
            None
        };
        if config.mode == TcpMode::Server {
            anyhow::ensure!(
                config.discovery || config.allowed_peer.is_some() || address.ip().is_loopback(),
                "A non-loopback Pico-Fi server requires discovery or allowed_peer (Pico IP)"
            );
        }
        let discovery = if config.discovery {
            let port = listener
                .as_ref()
                .map(|l| l.local_addr())
                .transpose()?
                .map_or(address.port(), |a| a.port());
            Some(LanDiscovery::open(
                config.mode,
                port,
                config.allowed_peer,
                config.discovery_port,
                config.discovery_interface.as_deref(),
            )?)
        } else {
            None
        };
        let (tx, outgoing) = mpsc::sync_channel(DEPTH);
        let (incoming, rx) = mpsc::sync_channel(DEPTH);
        let shared = Arc::new(Shared {
            connected: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            tx,
        });
        let weak = Arc::downgrade(&shared);
        let cfg = config.clone();
        std::thread::Builder::new()
            .name("picofi-tcp".into())
            .spawn(move || worker(weak, outgoing, incoming, listener, address, cfg, discovery))?;
        Ok(Self {
            shared,
            rx: Arc::new(Mutex::new(rx)),
            side: None,
            exhausted: false,
        })
    }
}
impl CommsDevice for PicoFiTcp {
    fn try_clone_tx(&self) -> std::io::Result<Option<Box<dyn CommsDevice>>> {
        Ok(Some(Box::new(Self {
            shared: self.shared.clone(),
            rx: self.rx.clone(),
            side: self.side,
            exhausted: false,
        })))
    }
    fn set_side_id(&mut self, side: RouterSideId) {
        self.side = Some(side);
    }
    fn receive_budget_exhausted(&self) -> bool {
        self.exhausted
    }
    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        if payload.is_empty() || payload.len() > MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Pico-Fi payload must be 1..4096 bytes",
            )
            .into());
        }
        let generation = self.shared.generation.load(Ordering::Acquire);
        if !self.shared.connected.load(Ordering::Acquire) {
            return Err(std::io::Error::from(std::io::ErrorKind::NotConnected).into());
        }
        let mut frame = Vec::with_capacity(payload.len() + 4);
        frame.extend_from_slice(&[0xb5, 0x4e]);
        frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        frame.extend_from_slice(payload);
        self.shared
            .tx
            .try_send((generation, frame))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::WouldBlock))?;
        Ok(())
    }
    fn recv_packet(
        &mut self,
        router: &Router,
        tap: &mut dyn FnMut(&Packet),
    ) -> TelemetryResult<()> {
        let side = self
            .side
            .ok_or(TelemetryError::HandlerError("comms side id not set"))?;
        let mut result = Ok(());
        self.recv_serialized_packets_with_budget(
            &mut |payload| {
                if result.is_ok() {
                    tap_non_groundstation_gps_payload(&payload, tap);
                    result = router.rx_packed_queue_from_side(&payload, side);
                }
            },
            Duration::ZERO,
            64,
        )?;
        result
    }
    fn recv_serialized_packets(&mut self, sink: &mut dyn FnMut(Vec<u8>)) -> TelemetryResult<()> {
        self.recv_serialized_packets_with_budget(sink, Duration::ZERO, 64)
    }
    fn recv_serialized_packets_with_budget(
        &mut self,
        sink: &mut dyn FnMut(Vec<u8>),
        _: Duration,
        max: usize,
    ) -> TelemetryResult<()> {
        self.exhausted = false;
        let rx = self.rx.lock().unwrap();
        for _ in 0..max {
            match rx.try_recv() {
                Ok(frame) => sink(frame),
                Err(_) => return Ok(()),
            }
        }
        self.exhausted = max > 0;
        Ok(())
    }
}
fn handshake(socket: &mut TcpStream, client: bool) -> std::io::Result<()> {
    // Accepted sockets inherit nonblocking mode on macOS; the bounded
    // handshake must explicitly enter blocking mode on both platforms.
    socket.set_nonblocking(false)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(STALL))?;
    socket.set_write_timeout(Some(STALL))?;
    let mut hello = [0; 7];
    if client {
        socket.write_all(HELLO)?;
    }
    socket.read_exact(&mut hello)?;
    if &hello != HELLO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid Pico-Fi handshake",
        ));
    }
    if !client {
        socket.write_all(HELLO)?;
    }
    socket.set_nonblocking(true)
}
/// A frame is at most 4100 bytes. Never grow based on an unvalidated length.
#[derive(Default)]
struct Decoder {
    bytes: Vec<u8>,
}
impl Decoder {
    fn needed(&self) -> std::io::Result<usize> {
        if self.bytes.len() < 4 {
            return Ok(4 - self.bytes.len());
        }
        if self.bytes[..2] != [0xb5, 0x4e] {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid Pico-Fi framing",
            ));
        }
        let n = u16::from_le_bytes([self.bytes[2], self.bytes[3]]) as usize;
        if n == 0 || n > MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid Pico-Fi frame size",
            ));
        }
        Ok(4 + n - self.bytes.len())
    }
    fn read(&mut self, socket: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
        let need = self.needed()?;
        if need == 0 {
            let frame = self.bytes.split_off(4);
            self.bytes.clear();
            return Ok(Some(frame));
        }
        let mut chunk = [0; 1024];
        let count = need.min(chunk.len());
        match socket.read(&mut chunk[..count]) {
            Ok(0) => Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
            Ok(n) => {
                self.bytes.extend_from_slice(&chunk[..n]);
                if self.needed()? == 0 {
                    let frame = self.bytes.split_off(4);
                    self.bytes.clear();
                    Ok(Some(frame))
                } else {
                    Ok(None)
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}
fn worker(
    weak: Weak<Shared>,
    tx: Receiver<(u64, Vec<u8>)>,
    rx: SyncSender<Vec<u8>>,
    listener: Option<TcpListener>,
    address: SocketAddr,
    config: PicoFiTcpConfig,
    mut discovery: Option<LanDiscovery>,
) {
    loop {
        if let Some(discovery) = discovery.as_mut() {
            discovery.poll(false);
        }
        let Some(shared) = weak.upgrade() else { return };
        shared.connected.store(false, Ordering::Release);
        let generation = shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        while tx.try_recv().is_ok() {}
        drop(shared);
        let candidate = match &listener {
            Some(l) => l.accept().and_then(|(s, peer)| {
                if config.allowed_peer.is_none_or(|ip| ip == peer.ip())
                    && discovery.as_ref().is_none_or(|d| d.authorized(peer.ip()))
                {
                    Ok(s)
                } else {
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                }
            }),
            None => {
                let target = discovery
                    .as_ref()
                    .map_or(Some(address), LanDiscovery::candidate);
                target
                    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::WouldBlock))
                    .and_then(|target| {
                        TcpStream::connect_timeout(&target, Duration::from_millis(250))
                    })
            }
        };
        let socket = candidate.and_then(|mut s| {
            handshake(&mut s, listener.is_none())?;
            Ok(s)
        });
        let Ok(mut socket) = socket else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let Some(shared) = weak.upgrade() else { return };
        shared.connected.store(true, Ordering::Release);
        drop(shared);
        let mut decoder = Decoder::default();
        let mut inbound = None;
        let mut pending: Option<(Vec<u8>, usize, Instant)> = None;
        'session: loop {
            if let Some(discovery) = discovery.as_mut() {
                discovery.poll(true);
            }
            if weak.strong_count() == 0 {
                return;
            }
            // Bounded work per direction prevents a busy reader starving writes.
            for _ in 0..8 {
                if let Some(frame) = inbound.take() {
                    match rx.try_send(frame) {
                        Ok(()) => {}
                        Err(TrySendError::Full(frame)) => {
                            inbound = Some(frame);
                            break;
                        }
                        Err(_) => return,
                    }
                }
                match decoder.read(&mut socket) {
                    Ok(Some(frame)) => inbound = Some(frame),
                    Ok(None) => break,
                    Err(_) => break 'session,
                }
            }
            for _ in 0..8 {
                if pending.is_none() {
                    match tx.try_recv() {
                        Ok((g, bytes)) if g == generation => {
                            pending = Some((bytes, 0, Instant::now()))
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                let Some((bytes, offset, started)) = pending.as_mut() else {
                    break;
                };
                if started.elapsed() > STALL {
                    break 'session;
                }
                match socket.write(&bytes[*offset..]) {
                    Ok(0) => break 'session,
                    Ok(n) => {
                        *offset += n;
                        if *offset == bytes.len() {
                            pending = None
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        break;
                    }
                    Err(_) => break 'session,
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(discovery) = discovery.as_mut() {
            discovery.reconnect();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (b, _) = listener.accept().unwrap();
        (a, b)
    }
    #[test]
    fn discovered_ephemeral_server_transfers_frames_and_rediscovers_after_disconnect() {
        use discovery_wire::{Kind, Message, Party, SIZE};
        use std::net::{Ipv4Addr, UdpSocket};
        let reservation = UdpSocket::bind("127.0.0.1:0").unwrap();
        let discovery_port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let config = PicoFiTcpConfig {
            discovery: true,
            discovery_port,
            discovery_interface: None,
            mode: TcpMode::Server,
            address: "0.0.0.0:0".into(),
            allowed_peer: None,
        };
        let mut device = PicoFiTcp::open(&config).unwrap();
        let discovery = UdpSocket::bind("127.0.0.1:0").unwrap();
        discovery
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        for nonce in [10, 11] {
            let query = Message {
                kind: Kind::Query,
                party: Party::Pico,
                port: 0,
                nonce,
                identity: [2, 3, 4, 5, 6, 7],
            };
            discovery
                .send_to(&query.encode(), (Ipv4Addr::LOCALHOST, discovery_port))
                .unwrap();
            let mut bytes = [0; SIZE];
            let (n, _) = discovery.recv_from(&mut bytes).unwrap();
            let offer = Message::decode(&bytes[..n]).unwrap();
            assert!(offer.compatible_offer(query));
            assert_ne!(offer.port, 0);
            let mut peer = TcpStream::connect((Ipv4Addr::LOCALHOST, offer.port)).unwrap();
            handshake(&mut peer, true).unwrap();
            peer.set_nonblocking(false).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while device.send_data(&[nonce as u8]).is_err() {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let mut frame = [0; 5];
            peer.read_exact(&mut frame).unwrap();
            assert_eq!(frame, [0xb5, 0x4e, 1, 0, nonce as u8]);
            peer.write_all(&[0xb5, 0x4e, 1, 0, 42]).unwrap();
            let mut received = vec![];
            while received.is_empty() {
                assert!(Instant::now() < deadline);
                device
                    .recv_serialized_packets(&mut |frame| received.push(frame))
                    .unwrap();
                std::thread::yield_now();
            }
            assert_eq!(received, vec![vec![42]]);
            peer.shutdown(std::net::Shutdown::Both).unwrap();
            drop(peer);
            while device.shared.connected.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
    }
    #[test]
    fn client_worker_discovers_pico_server_and_reconnects_to_a_new_port() {
        use discovery_wire::{Kind, Message, Party, SIZE};
        use std::net::{Ipv4Addr, UdpSocket};
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let discovery = LanDiscovery::open_at(
            TcpMode::Client,
            0,
            None,
            "127.0.0.1:0".parse().unwrap(),
            vec![udp.local_addr().unwrap()],
        )
        .unwrap();
        let config = PicoFiTcpConfig {
            discovery: true,
            discovery_port: udp.local_addr().unwrap().port(),
            discovery_interface: None,
            mode: TcpMode::Client,
            address: "0.0.0.0:0".into(),
            allowed_peer: None,
        };
        let (tx, outgoing) = mpsc::sync_channel(DEPTH);
        let (incoming, rx) = mpsc::sync_channel(DEPTH);
        let shared = Arc::new(Shared {
            connected: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            tx,
        });
        let weak = Arc::downgrade(&shared);
        let address = config.address.parse().unwrap();
        let mut device = PicoFiTcp {
            shared,
            rx: Arc::new(Mutex::new(rx)),
            side: None,
            exhausted: false,
        };
        let handle = std::thread::spawn(move || {
            worker(
                weak,
                outgoing,
                incoming,
                None,
                address,
                config,
                Some(discovery),
            )
        });
        for value in [41, 42] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut bytes = [0; SIZE];
            let (n, source) = udp.recv_from(&mut bytes).unwrap();
            let query = Message::decode(&bytes[..n]).unwrap();
            assert_eq!(query.party, Party::GroundStation);
            let offer = Message {
                kind: Kind::Offer,
                party: Party::Pico,
                port: listener.local_addr().unwrap().port(),
                nonce: query.nonce,
                identity: [2, 9, 8, 7, 6, 5],
            };
            udp.send_to(&offer.encode(), source).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut peer = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            handshake(&mut peer, false).unwrap();
            peer.set_nonblocking(false).unwrap();
            while device.send_data(&[value]).is_err() {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let mut frame = [0; 5];
            peer.read_exact(&mut frame).unwrap();
            assert_eq!(frame, [0xb5, 0x4e, 1, 0, value]);
            peer.shutdown(std::net::Shutdown::Both).unwrap();
            while device.shared.connected.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
        drop(device);
        handle.join().unwrap();
    }
    #[test]
    fn fragmented_and_coalesced_frames_preserve_every_payload() {
        let (mut a, mut b) = pair();
        b.set_nonblocking(true).unwrap();
        let mut d = Decoder::default();
        a.write_all(&[0xb5]).unwrap();
        assert!(d.read(&mut b).unwrap().is_none());
        a.write_all(&[0x4e, 3, 0, 1, 2, 3, 0xb5, 0x4e, 1, 0, 9])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut frames = vec![];
        while frames.len() < 2 && Instant::now() < deadline {
            if let Some(p) = d.read(&mut b).unwrap() {
                frames.push(p)
            }
        }
        assert_eq!(frames, vec![vec![1, 2, 3], vec![9]]);
    }
    #[test]
    fn malformed_and_oversize_frames_fail_without_unbounded_storage() {
        for header in [[0, 0, 1, 0], [0xb5, 0x4e, 1, 16], [0xb5, 0x4e, 0, 0]] {
            let d = Decoder {
                bytes: header.to_vec(),
            };
            assert!(d.needed().is_err());
            assert_eq!(d.bytes.len(), 4)
        }
    }
    #[test]
    fn bounded_admission_rejects_disconnected_and_full_queues() {
        let (tx, _outgoing) = mpsc::sync_channel(1);
        let (_incoming, rx) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            connected: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            tx,
        });
        let mut device = PicoFiTcp {
            shared: shared.clone(),
            rx: Arc::new(Mutex::new(rx)),
            side: None,
            exhausted: false,
        };
        assert!(device.send_data(&[1]).is_err());
        shared.connected.store(true, Ordering::Release);
        device.send_data(&[1]).unwrap();
        let error = device.send_data(&[2]).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(device.send_data(&vec![0; MAX + 1]).is_err());
    }
    #[test]
    fn server_accepts_existing_pico_handshake_and_independent_writer() {
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let config = PicoFiTcpConfig {
            discovery: false,
            discovery_port: discovery_wire::PORT,
            discovery_interface: None,
            mode: TcpMode::Server,
            address: address.to_string(),
            allowed_peer: None,
        };
        let device = PicoFiTcp::open(&config).unwrap();
        let mut peer = TcpStream::connect(address).unwrap();
        handshake(&mut peer, true).unwrap();
        peer.set_nonblocking(false).unwrap();
        let mut writer = device.try_clone_tx().unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while writer.send_data(&[9]).is_err() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut bytes = [0; 5];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [0xb5, 0x4e, 1, 0, 9]);
    }
    #[test]
    fn invalid_handshake_is_rejected_and_public_server_requires_peer() {
        let (mut a, mut b) = pair();
        a.write_all(b"NOTPICO").unwrap();
        assert!(handshake(&mut b, false).is_err());
        let config = PicoFiTcpConfig {
            discovery: false,
            discovery_port: discovery_wire::PORT,
            discovery_interface: None,
            mode: TcpMode::Server,
            address: "0.0.0.0:0".into(),
            allowed_peer: None,
        };
        assert!(PicoFiTcp::open(&config).is_err());
    }
    #[test]
    fn client_server_roundtrip_reconnect_and_shutdown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cfg = PicoFiTcpConfig {
            discovery: false,
            discovery_port: discovery_wire::PORT,
            discovery_interface: None,
            mode: TcpMode::Client,
            address: address.to_string(),
            allowed_peer: None,
        };
        let mut device = PicoFiTcp::open(&cfg).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        handshake(&mut peer, false).unwrap();
        peer.set_nonblocking(false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while device.send_data(&[7, 8]).is_err() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut got = [0; 6];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(got, [0xb5, 0x4e, 2, 0, 7, 8]);
        drop(peer);
        let (mut peer, _) = listener.accept().unwrap();
        handshake(&mut peer, false).unwrap();
        peer.set_nonblocking(false).unwrap();
        peer.write_all(&[0xb5, 0x4e, 2, 0, 4, 5]).unwrap();
        let mut frames = vec![];
        while frames.is_empty() {
            assert!(Instant::now() < deadline);
            device
                .recv_serialized_packets(&mut |p| frames.push(p))
                .unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(frames, vec![vec![4, 5]]);
        let weak = Arc::downgrade(&device.shared);
        drop(device);
        while weak.strong_count() != 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
