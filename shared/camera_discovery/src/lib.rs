//! Authenticated LAN discovery for RTSP publishers; never sent through SEDSnet.
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;

pub const PORT: u16 = 49362;
const SIZE: usize = 64;
const MAGIC: &[u8; 8] = b"GSCAM002";

fn packet(kind: u8, port: u16, nonce: [u8; 16], secret: &str, address: Ipv4Addr) -> [u8; SIZE] {
    let mut bytes = [0; SIZE];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8] = kind;
    bytes[10..14].copy_from_slice(&address.octets());
    bytes[14..16].copy_from_slice(&port.to_le_bytes());
    bytes[16..32].copy_from_slice(&nonce);
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, &bytes[..32]);
    bytes[32..].copy_from_slice(tag.as_ref());
    bytes
}

fn decode(bytes: &[u8], kind: u8, secret: &str) -> Option<(u16, [u8; 16], Ipv4Addr)> {
    if secret.is_empty()
        || bytes.len() != SIZE
        || &bytes[..8] != MAGIC
        || bytes[8] != kind
        || bytes[9] != 0
    {
        return None;
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    hmac::verify(&key, &bytes[..32], &bytes[32..]).ok()?;
    let port = u16::from_le_bytes(bytes[14..16].try_into().ok()?);
    let nonce = bytes[16..32].try_into().ok()?;
    let address = Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[10..14]).ok()?);
    if nonce == [0; 16]
        || (kind == 1 && (port != 0 || !address.is_unspecified()))
        || (kind == 2
            && (port == 0
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_broadcast()))
    {
        return None;
    }
    Some((port, nonce, address))
}

/// Enumerate directed LAN broadcasts afresh on every reconnect (DHCP/IPv4LL).
pub fn targets(port: u16, interface: Option<&str>) -> io::Result<Vec<SocketAddr>> {
    let mut result = Vec::new();
    for entry in if_addrs::get_if_addrs()? {
        if entry.is_loopback() || interface.is_some_and(|name| name != entry.name) {
            continue;
        }
        if let if_addrs::IfAddr::V4(addr) = entry.addr
            && let Some(broadcast) = addr.broadcast
        {
            let target = SocketAddr::from((broadcast, port));
            if result.len() < 16 && !result.contains(&target) {
                result.push(target);
            }
        }
    }
    // Explicit interfaces must not silently leak discovery onto other networks.
    if result.is_empty() && interface.is_none() {
        result.push(SocketAddr::from((Ipv4Addr::BROADCAST, port)));
    }
    Ok(result)
}

/// One bounded discovery attempt. Call only while publishing is disconnected.
pub async fn discover(targets: &[SocketAddr], secret: &str) -> io::Result<Option<SocketAddr>> {
    if secret.is_empty() {
        return Err(io::Error::other("video discovery requires a password"));
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let mut nonce = [0; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| io::Error::other("discovery randomness unavailable"))?;
    let query = packet(1, 0, nonce, secret, Ipv4Addr::UNSPECIFIED);
    let mut sent = false;
    let mut last_error = None;
    for target in targets.iter().take(16) {
        match socket.send_to(&query, target).await {
            Ok(_) => sent = true,
            Err(error) => last_error = Some(error),
        }
    }
    // A disconnected/stale interface must not prevent a working LAN finding GS.
    if !sent && let Some(error) = last_error {
        return Err(error);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    let mut best = None;
    let mut bytes = [0; SIZE + 1];
    for _ in 0..256 {
        let Ok(result) = tokio::time::timeout_at(deadline, socket.recv_from(&mut bytes)).await
        else {
            break;
        };
        let (length, peer) = result?;
        if let Some((port, received, address)) = decode(&bytes[..length], 2, secret)
            && received == nonce
            && peer.ip() == std::net::IpAddr::V4(address)
            && !peer.ip().is_unspecified()
        {
            let endpoint = SocketAddr::new(peer.ip(), port);
            if best.is_none_or(|previous| endpoint < previous) {
                best = Some(endpoint);
            }
        }
    }
    Ok(best)
}

async fn reply_address(socket: &UdpSocket, peer: SocketAddr) -> io::Result<Ipv4Addr> {
    if let std::net::IpAddr::V4(ip) = socket.local_addr()?.ip()
        && !ip.is_unspecified()
    {
        return Ok(ip);
    }
    // Query the kernel's current source route, including after DHCP changes.
    let route = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    route.connect(peer).await?;
    match route.local_addr()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Ok(ip),
        _ => Err(io::Error::other("no camera discovery IPv4 route")),
    }
}

/// Serve signed replies at a bounded rate. Credentials never appear on the wire.
pub async fn serve(socket: UdpSocket, rtsp_port: u16, secret: &str) -> io::Result<()> {
    if secret.is_empty() || rtsp_port == 0 {
        return Err(io::Error::other("invalid camera discovery settings"));
    }
    let mut bytes = [0; SIZE + 1];
    let mut window = Instant::now();
    let mut received = 0;
    loop {
        let (length, peer) = socket.recv_from(&mut bytes).await?;
        if window.elapsed() >= Duration::from_secs(1) {
            window = Instant::now();
            received = 0;
        }
        // Bound authentication work and amplification, including invalid packets.
        if received >= 8 {
            continue;
        }
        received += 1;
        if let Some((_, nonce, _)) = decode(&bytes[..length], 1, secret)
            && peer.is_ipv4()
            && !peer.ip().is_unspecified()
            && !peer.ip().is_multicast()
        {
            let Ok(address) = reply_address(&socket, peer).await else {
                continue;
            };
            // A peer may disappear or the LAN may be renumbered between receive
            // and reply. Keep the listener alive for the next discovery attempt.
            let _ = socket
                .send_to(&packet(2, rtsp_port, nonce, secret, address), peer)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_auth_shape_service_and_replayed_challenge() {
        let nonce = [42; 16];
        let valid = packet(2, 8554, nonce, "secret", Ipv4Addr::LOCALHOST);
        assert_eq!(
            decode(&valid, 2, "secret"),
            Some((8554, nonce, Ipv4Addr::LOCALHOST))
        );
        assert!(decode(&valid, 2, "other").is_none());
        assert!(decode(&valid, 1, "secret").is_none());
        assert!(decode(&valid[..63], 2, "secret").is_none());
        assert!(
            decode(
                &packet(2, 0, nonce, "secret", Ipv4Addr::LOCALHOST),
                2,
                "secret"
            )
            .is_none()
        );
        assert!(
            decode(
                &packet(1, 8554, nonce, "secret", Ipv4Addr::UNSPECIFIED),
                1,
                "secret"
            )
            .is_none()
        );
        assert!(
            decode(
                &packet(2, 8554, [0; 16], "secret", Ipv4Addr::LOCALHOST),
                2,
                "secret"
            )
            .is_none()
        );
        for index in 0..SIZE {
            let mut corrupt = valid;
            corrupt[index] ^= 1;
            assert!(decode(&corrupt, 2, "secret").is_none(), "byte {index}");
        }
        for address in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 1),
        ] {
            assert!(decode(&packet(2, 8554, nonce, "secret", address), 2, "secret").is_none());
        }
        assert!(
            decode(
                &packet(1, 0, nonce, "secret", Ipv4Addr::LOCALHOST),
                1,
                "secret"
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn wildcard_listener_signs_the_current_reply_address() {
        let receiver = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, receiver.local_addr().unwrap().port()));
        let task = tokio::spawn(async move { serve(receiver, 8554, "secret").await });
        assert_eq!(
            discover(&[target], "secret").await.unwrap(),
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 8554)))
        );
        task.abort();
    }

    #[tokio::test]
    async fn rediscovers_changed_ip_and_port_using_real_udp_and_tcp() {
        use tokio::net::TcpListener;
        // macOS requires root to add loopback aliases; exercise the port
        // migration there and both address + port migration on Linux/Pi.
        let second = if cfg!(target_os = "macos") {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::new(127, 0, 0, 2)
        };
        for ip in [Ipv4Addr::LOCALHOST, second] {
            let listener = TcpListener::bind((ip, 0)).await.unwrap();
            let receiver = UdpSocket::bind((ip, 0)).await.unwrap();
            let target = receiver.local_addr().unwrap();
            let port = listener.local_addr().unwrap().port();
            let task = tokio::spawn(async move { serve(receiver, port, "secret").await });
            assert!(discover(&[target], "wrong").await.unwrap().is_none());
            let found = discover(&[target], "secret").await.unwrap().unwrap();
            assert_eq!(found, listener.local_addr().unwrap());
            let _connection = tokio::net::TcpStream::connect(found).await.unwrap();
            listener.accept().await.unwrap();
            task.abort();
        }
    }

    #[tokio::test]
    async fn bad_interface_target_does_not_block_working_lan() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target = receiver.local_addr().unwrap();
        let task = tokio::spawn(async move { serve(receiver, 8554, "secret").await });
        let incompatible: SocketAddr = "[::1]:49362".parse().unwrap();
        assert_eq!(
            discover(&[incompatible, target], "secret").await.unwrap(),
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 8554)))
        );
        task.abort();
    }

    #[test]
    fn missing_explicit_interface_cannot_broadcast_on_other_networks() {
        assert!(
            targets(PORT, Some("not-a-real-camera-lan-interface"))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn stale_or_unrelated_offer_cannot_redirect_publisher() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut bytes = [0; SIZE + 1];
            let (_, peer) = socket.recv_from(&mut bytes).await.unwrap();
            socket
                .send_to(
                    &packet(2, 8554, [9; 16], "secret", Ipv4Addr::LOCALHOST),
                    peer,
                )
                .await
                .unwrap();
            socket
                .send_to(
                    &packet(
                        2,
                        8554,
                        bytes[16..32].try_into().unwrap(),
                        "other",
                        Ipv4Addr::LOCALHOST,
                    ),
                    peer,
                )
                .await
                .unwrap();
        });
        assert!(discover(&[target], "secret").await.unwrap().is_none());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn relayed_signed_offer_cannot_redirect_to_another_address() {
        let receiver_ip = if cfg!(target_os = "macos") {
            // macOS needs root for additional loopback aliases. A second
            // address already assigned to this host stays entirely local.
            let Some(ip) = if_addrs::get_if_addrs()
                .unwrap()
                .into_iter()
                .find_map(|entry| match entry.addr {
                    if_addrs::IfAddr::V4(addr) if !addr.ip.is_loopback() => Some(addr.ip),
                    _ => None,
                })
            else {
                return;
            };
            ip
        } else {
            Ipv4Addr::new(127, 0, 0, 2)
        };
        let receiver = UdpSocket::bind((receiver_ip, 0)).await.unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        let service = tokio::spawn(async move { serve(receiver, 8554, "secret").await });
        let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target = relay.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut bytes = [0; SIZE + 1];
            let (n, client) = relay.recv_from(&mut bytes).await.unwrap();
            relay.send_to(&bytes[..n], receiver_addr).await.unwrap();
            let (n, source) = relay.recv_from(&mut bytes).await.unwrap();
            assert_eq!(source, receiver_addr);
            // Relay a genuine response unchanged, without knowing its key.
            relay.send_to(&bytes[..n], client).await.unwrap();
        });
        assert!(discover(&[target], "secret").await.unwrap().is_none());
        task.await.unwrap();
        service.abort();
    }
}
