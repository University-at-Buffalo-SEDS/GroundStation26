//! Bounded Ethernet discovery for PICOFI1. This never enters the SEDSnet router.
use super::discovery_wire::{Kind, Message, Party, SIZE, hardware_mac};
use crate::comms_config::TcpMode;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};
const FRESH: Duration = Duration::from_secs(5);

pub(super) struct LanDiscovery {
    socket: UdpSocket,
    targets: Vec<SocketAddr>,
    mode: TcpMode,
    tcp_port: u16,
    allowed_peer: Option<IpAddr>,
    identity: [u8; 6],
    query: Option<Message>,
    next_query: Instant,
    settle: Option<Instant>,
    best: Option<([u8; 6], SocketAddr)>,
    candidate: Option<(SocketAddr, Instant)>,
    peers: [Option<(IpAddr, Instant)>; 8],
    response_window: Instant,
    responses: u8,
    refresh: Option<(u16, Option<String>, Instant)>,
}
impl LanDiscovery {
    pub(super) fn open(
        mode: TcpMode,
        tcp_port: u16,
        allowed_peer: Option<IpAddr>,
        discovery_port: u16,
        interface: Option<&str>,
    ) -> io::Result<Self> {
        let bind = SocketAddr::from((
            Ipv4Addr::UNSPECIFIED,
            if mode == TcpMode::Server {
                discovery_port
            } else {
                0
            },
        ));
        let mut discovery = Self::open_at(
            mode,
            tcp_port,
            allowed_peer,
            bind,
            broadcast_targets(discovery_port, interface)?,
        )?;
        discovery.refresh = Some((discovery_port, interface.map(str::to_owned), Instant::now()));
        Ok(discovery)
    }
    pub(super) fn open_at(
        mode: TcpMode,
        tcp_port: u16,
        allowed_peer: Option<IpAddr>,
        bind: SocketAddr,
        targets: Vec<SocketAddr>,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;
        socket.set_broadcast(true)?;
        let now = Instant::now();
        Ok(Self {
            socket,
            targets,
            mode,
            tcp_port,
            allowed_peer,
            identity: hardware_mac(&rand::random::<[u8; 16]>()),
            query: None,
            next_query: now,
            settle: None,
            best: None,
            candidate: None,
            peers: [None; 8],
            response_window: now,
            responses: 0,
            refresh: None,
        })
    }
    pub(super) fn poll(&mut self, connected: bool) {
        let now = Instant::now();
        if !connected
            && let Some((port, interface, refreshed)) = &mut self.refresh
            && now.duration_since(*refreshed) >= Duration::from_secs(2)
        {
            if let Ok(targets) = broadcast_targets(*port, interface.as_deref()) {
                self.targets = targets;
            }
            *refreshed = now;
        }
        if self.mode == TcpMode::Client && !connected && now >= self.next_query {
            let query = Message {
                kind: Kind::Query,
                party: Party::GroundStation,
                port: 0,
                nonce: rand::random::<u64>().max(1),
                identity: self.identity,
            };
            for target in &self.targets {
                let _ = self.socket.send_to(&query.encode(), target);
            }
            self.query = Some(query);
            self.best = None;
            self.settle = Some(now + Duration::from_millis(400));
            self.next_query = now + Duration::from_secs(1);
        }
        if self.settle.is_some_and(|until| now >= until) {
            if let Some((_, candidate)) = self.best.take() {
                self.candidate = Some((candidate, now));
            }
            self.settle = None;
        }
        if now.duration_since(self.response_window) >= Duration::from_secs(1) {
            self.response_window = now;
            self.responses = 0;
        }
        let mut bytes = [0; SIZE + 1];
        for _ in 0..8 {
            let (size, source) = match self.socket.recv_from(&mut bytes) {
                Ok(value) => value,
                Err(_) => break,
            };
            let Some(message) = Message::decode(&bytes[..size]) else {
                continue;
            };
            if !source.is_ipv4()
                || source.ip().is_unspecified()
                || source.ip().is_multicast()
                || self.allowed_peer.is_some_and(|ip| ip != source.ip())
            {
                continue;
            }
            match self.mode {
                TcpMode::Server
                    if message.kind == Kind::Query
                        && message.party == Party::Pico
                        && message.identity != self.identity
                        && self.responses < 4 =>
                {
                    let offer = Message {
                        kind: Kind::Offer,
                        party: Party::GroundStation,
                        port: self.tcp_port,
                        nonce: message.nonce,
                        identity: self.identity,
                    };
                    if self.socket.send_to(&offer.encode(), source).is_ok() {
                        self.responses += 1;
                        let slot = self
                            .peers
                            .iter()
                            .position(|peer| peer.is_some_and(|(ip, _)| ip == source.ip()))
                            .or_else(|| {
                                self.peers.iter().position(|peer| {
                                    peer.is_none_or(|(_, seen)| now.duration_since(seen) > FRESH)
                                })
                            })
                            .unwrap_or_else(|| {
                                self.peers
                                    .iter()
                                    .enumerate()
                                    .min_by_key(|(_, peer)| peer.map(|(_, seen)| seen))
                                    .unwrap()
                                    .0
                            });
                        self.peers[slot] = Some((source.ip(), now));
                    }
                }
                TcpMode::Client if !connected && self.settle.is_some_and(|until| now <= until) => {
                    if self
                        .query
                        .is_some_and(|query| message.compatible_offer(query))
                        && self
                            .best
                            .as_ref()
                            .is_none_or(|(id, _)| message.identity < *id)
                    {
                        self.best =
                            Some((message.identity, SocketAddr::new(source.ip(), message.port)));
                    }
                }
                _ => {}
            }
        }
    }
    pub(super) fn authorized(&self, peer: IpAddr) -> bool {
        self.peers
            .iter()
            .any(|entry| entry.is_some_and(|(ip, seen)| ip == peer && seen.elapsed() <= FRESH))
    }
    pub(super) fn candidate(&self) -> Option<SocketAddr> {
        self.candidate
            .filter(|(_, seen)| seen.elapsed() <= FRESH)
            .map(|(peer, _)| peer)
    }
    pub(super) fn reconnect(&mut self) {
        self.query = None;
        self.settle = None;
        self.best = None;
        self.candidate = None;
        self.next_query = Instant::now();
    }
}
fn broadcast_targets(port: u16, selected: Option<&str>) -> io::Result<Vec<SocketAddr>> {
    let mut targets = Vec::new();
    let mut selected_present = false;
    for interface in if_addrs::get_if_addrs()? {
        selected_present |=
            selected.is_some_and(|name| name == interface.name) && !interface.is_loopback();
        if selected.is_some_and(|name| name != interface.name) || interface.is_loopback() {
            continue;
        }
        #[cfg(target_os = "linux")]
        if selected.is_none()
            && (std::path::Path::new("/sys/class/net")
                .join(&interface.name)
                .join("wireless")
                .exists()
                || !std::path::Path::new("/sys/class/net")
                    .join(&interface.name)
                    .join("device")
                    .exists())
        {
            continue;
        }
        if let if_addrs::IfAddr::V4(addr) = interface.addr
            && let Some(broadcast) = addr.broadcast
        {
            let endpoint = SocketAddr::from((broadcast, port));
            if !targets.contains(&endpoint) && targets.len() < 16 {
                targets.push(endpoint);
            }
        }
    }
    if targets.is_empty() {
        if selected.is_some() && !selected_present {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Selected Ethernet interface does not exist or is loopback",
            ));
        }
        targets.push(SocketAddr::from((Ipv4Addr::BROADCAST, port)));
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn endpoint() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }
    fn query(party: Party, nonce: u64) -> Message {
        Message {
            kind: Kind::Query,
            party,
            port: 0,
            nonce,
            identity: [2, 1, 2, 3, 4, 5],
        }
    }
    fn ready(socket: &UdpSocket) {
        let until = Instant::now() + Duration::from_secs(1);
        let mut byte = [0];
        while socket.peek_from(&mut byte).is_err() {
            assert!(Instant::now() < until, "UDP fixture did not arrive");
            std::thread::yield_now();
        }
    }
    fn receive(socket: &UdpSocket) -> Message {
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut bytes = [0; SIZE + 1];
        let (n, _) = socket.recv_from(&mut bytes).unwrap();
        Message::decode(&bytes[..n]).unwrap()
    }
    #[test]
    fn server_advertises_selected_port_and_bounds_authorization_and_replies() {
        let mut server =
            LanDiscovery::open_at(TcpMode::Server, 32123, None, endpoint(), vec![]).unwrap();
        let peer = UdpSocket::bind(endpoint()).unwrap();
        let target = server.socket.local_addr().unwrap();
        for nonce in 1..=6 {
            peer.send_to(&query(Party::Pico, nonce).encode(), target)
                .unwrap();
        }
        ready(&server.socket);
        server.poll(false);
        for nonce in 1..=4 {
            let offer = receive(&peer);
            assert_eq!(offer.port, 32123);
            assert!(offer.compatible_offer(query(Party::Pico, nonce)));
        }
        assert_eq!(server.responses, 4);
        assert!(server.authorized(Ipv4Addr::LOCALHOST.into()));
        server
            .peers
            .iter_mut()
            .flatten()
            .for_each(|(_, seen)| *seen = Instant::now() - FRESH - Duration::from_secs(1));
        assert!(!server.authorized(Ipv4Addr::LOCALHOST.into()));
        assert_eq!(server.peers.len(), 8);
    }
    #[test]
    fn client_selects_compatible_host_rejects_replays_and_is_quiet_when_connected() {
        let host = UdpSocket::bind(endpoint()).unwrap();
        let mut client = LanDiscovery::open_at(
            TcpMode::Client,
            0,
            None,
            endpoint(),
            vec![host.local_addr().unwrap()],
        )
        .unwrap();
        client.poll(false);
        let q = receive(&host);
        let target = client.socket.local_addr().unwrap();
        let offer = |id, nonce, party| Message {
            kind: Kind::Offer,
            party,
            port: 23456,
            nonce,
            identity: [id; 6],
        };
        for message in [
            offer(1, q.nonce + 1, Party::Pico),
            offer(1, q.nonce, Party::GroundStation),
            offer(9, q.nonce, Party::Pico),
            offer(3, q.nonce, Party::Pico),
        ] {
            host.send_to(&message.encode(), target).unwrap();
        }
        host.send_to(&[0; SIZE + 1], target).unwrap();
        ready(&client.socket);
        // Loopback datagrams may arrive across multiple nonblocking polls.
        // Exercise that normal service pattern within the selection window.
        let until = Instant::now() + Duration::from_millis(300);
        while client.best.map(|(identity, _)| identity) != Some([3; 6]) {
            assert!(Instant::now() < until, "compatible offers did not arrive");
            client.poll(false);
            std::thread::yield_now();
        }
        assert_eq!(client.best.unwrap().0, [3; 6]);
        client.settle = Some(Instant::now());
        client.poll(false);
        assert_eq!(client.candidate().unwrap().port(), 23456);
        client.next_query = Instant::now();
        let old = client.query;
        client.poll(true);
        assert_eq!(client.query.unwrap().nonce, old.unwrap().nonce);
        client.reconnect();
        assert!(client.candidate().is_none());
        client.poll(false);
        let fresh = receive(&host);
        assert_ne!(fresh.nonce, q.nonce);
        host.send_to(&offer(1, q.nonce, Party::Pico).encode(), target)
            .unwrap();
        client.poll(false);
        assert!(client.best.is_none());
    }
    #[test]
    fn server_rejects_wrong_role_and_pinned_out_peers() {
        let mut server = LanDiscovery::open_at(
            TcpMode::Server,
            9000,
            Some("192.0.2.1".parse().unwrap()),
            endpoint(),
            vec![],
        )
        .unwrap();
        let peer = UdpSocket::bind(endpoint()).unwrap();
        peer.send_to(
            &query(Party::Pico, 1).encode(),
            server.socket.local_addr().unwrap(),
        )
        .unwrap();
        ready(&server.socket);
        server.poll(false);
        assert_eq!(server.responses, 0);
        assert!(!server.authorized(Ipv4Addr::LOCALHOST.into()));
        server.allowed_peer = None;
        peer.send_to(
            &query(Party::GroundStation, 2).encode(),
            server.socket.local_addr().unwrap(),
        )
        .unwrap();
        ready(&server.socket);
        server.poll(false);
        assert_eq!(server.responses, 0);
    }
}
