#![cfg(unix)]
use ring::hmac;
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UdpSocket,
    process::Command,
    time::timeout,
};

#[tokio::test]
async fn actual_responder_authenticates_and_advertises_without_camera_programs() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gs-video-sender"))
        .args(["advertise", "8554"])
        .env("GS_VIDEO_PASSWORD", "isolated-test-secret")
        .env("GS_VIDEO_DISCOVERY_BIND", "127.0.0.1:0")
        .env("PATH", "/nonexistent-camera-programs")
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let ready = timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let address: std::net::SocketAddr = ready
        .split("ready on ")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let key = hmac::Key::new(hmac::HMAC_SHA256, b"isolated-test-secret");
    let mut query = [0u8; 64];
    query[..8].copy_from_slice(b"GSCAM002");
    query[8] = 1;
    query[16..32].fill(23);
    let tag = hmac::sign(&key, &query[..32]);
    query[32..].copy_from_slice(tag.as_ref());
    let mut reply = [0; 65];
    let mut invalid = query;
    invalid[63] ^= 1;
    socket.send_to(&invalid, address).await.unwrap();
    assert!(
        timeout(Duration::from_millis(100), socket.recv_from(&mut reply))
            .await
            .is_err()
    );
    socket.send_to(&query, address).await.unwrap();
    let (length, peer) = timeout(Duration::from_secs(2), socket.recv_from(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer, address);
    assert_eq!(length, 64);
    assert_eq!(&reply[..8], b"GSCAM002");
    assert_eq!(reply[8], 2);
    assert_eq!(reply[10..14], [127, 0, 0, 1]);
    assert_eq!(u16::from_le_bytes(reply[14..16].try_into().unwrap()), 8554);
    assert_eq!(reply[16..32], query[16..32]);
    hmac::verify(&key, &reply[..32], &reply[32..64]).unwrap();
    child.kill().await.unwrap();
}

#[tokio::test]
async fn invalid_rtsp_port_fails_before_starting_discovery() {
    let result = Command::new(env!("CARGO_BIN_EXE_gs-video-sender"))
        .args(["advertise", "0"])
        .env_remove("GS_VIDEO_PASSWORD")
        .output()
        .await
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("RTSP port"));
}
