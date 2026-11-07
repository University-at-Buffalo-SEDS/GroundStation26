//! Preserved hardware bytes exercised through the unchanged ac61 GS parser.
//! These tests do not model modem loss or establish hardware parser timing.
use super::*;

const CHUNK: &[u8] =
    include_bytes!("captured-prefix-fixtures/chunk135-prefix-then-valid-message.bin");
const MESSAGE: &[u8] =
    include_bytes!("captured-prefix-fixtures/chunk135-prefix-then-valid-message-valid-only.bin");
const COMPACT: &[u8] =
    include_bytes!("captured-prefix-fixtures/compact4-prefix-then-valid-baro.bin");
const BARO: &[u8] =
    include_bytes!("captured-prefix-fixtures/compact4-prefix-then-valid-baro-valid-only.bin");

fn poll(
    bytes: &mut Vec<u8>,
    wait: &mut RawUartFrameWait,
    wall: Instant,
    at_us: u64,
) -> TelemetryResult<Option<(RawUartFrameKind, Vec<u8>)>> {
    take_raw_uart_framed_payload_with_clock(
        bytes,
        wait,
        wall + Duration::from_micros(at_us),
        Some(at_us / 1000),
    )
}
fn only_body(frame: &[u8]) -> &[u8] {
    assert_eq!(&frame[..2], &[0xa5, 0x5a]);
    assert_eq!(
        usize::from(u16::from_le_bytes([frame[2], frame[3]])),
        frame.len() - 4
    );
    &frame[4..]
}
fn assert_valid_wrapper(body: &[u8]) {
    assert_eq!(&body[..3], b"SDT");
    assert_eq!(
        crc32fast::hash(&body[..body.len() - 4]),
        u32::from_le_bytes(body[body.len() - 4..].try_into().unwrap())
    );
}
fn one_recovered(
    bytes: &mut Vec<u8>,
    wait: &mut RawUartFrameWait,
    wall: Instant,
    at_us: u64,
    expected: &[u8],
) {
    let (kind, body) = poll(bytes, wait, wall, at_us)
        .unwrap()
        .expect("following frame must recover");
    assert!(matches!(kind, RawUartFrameKind::Data));
    assert_eq!(body, only_body(expected));
    assert!(
        poll(bytes, wait, wall, at_us).unwrap().is_none(),
        "no phantom frame"
    );
    assert!(bytes.is_empty());
}

#[test]
fn captured_valid_only_controls_deliver_once_even_with_seven_byte_reads() {
    for frame in [MESSAGE, BARO] {
        assert_valid_wrapper(only_body(frame));
        let wall = Instant::now();
        let mut wait = RawUartFrameWait::default();
        let mut bytes = Vec::new();
        let mut delivered = Vec::new();
        for piece in frame.chunks(7) {
            bytes.extend_from_slice(piece);
            while let Some((kind, body)) = poll(&mut bytes, &mut wait, wall, 0).unwrap() {
                assert!(matches!(kind, RawUartFrameKind::Data));
                delivered.push(body);
            }
        }
        assert_eq!(delivered, vec![only_body(frame).to_vec()]);
        assert!(bytes.is_empty());
    }
}

#[test]
fn captured_overlaps_reject_crc_then_recover_one_frame_without_timeout() {
    for (captured, valid) in [(CHUNK, MESSAGE), (COMPACT, BARO)] {
        let wall = Instant::now();
        let mut wait = RawUartFrameWait::default();
        let mut bytes = captured.to_vec();
        let error = poll(&mut bytes, &mut wait, wall, 0).unwrap_err();
        assert!(format!("{error:?}").contains("raw UART SDT crc32 mismatch"));
        assert!(
            wait.started.is_none(),
            "complete rejected span is not a partial timeout"
        );
        one_recovered(&mut bytes, &mut wait, wall, 0, valid);
    }
}

#[test]
fn captured_chunk_read_split_recovers_after_1282_microseconds_not_two_seconds() {
    assert_eq!(CHUNK.len(), 212);
    assert_eq!(MESSAGE.len(), 60);
    let wall = Instant::now();
    let mut wait = RawUartFrameWait::default();
    let mut bytes = CHUNK[..152].to_vec();
    assert!(poll(&mut bytes, &mut wait, wall, 0).unwrap().is_none());
    assert_eq!(bytes.len(), 152);
    bytes.extend_from_slice(&CHUNK[152..]);
    assert!(
        format!("{:?}", poll(&mut bytes, &mut wait, wall, 1282).unwrap_err())
            .contains("raw UART SDT crc32 mismatch")
    );
    one_recovered(&mut bytes, &mut wait, wall, 1282, MESSAGE);
}

#[test]
fn captured_compact_gap_without_intervening_poll_is_crc_recovery_not_measured_timeout() {
    assert_eq!(COMPACT.len(), 46);
    let wall = Instant::now();
    let mut wait = RawUartFrameWait::default();
    let mut bytes = COMPACT[..4].to_vec();
    assert!(poll(&mut bytes, &mut wait, wall, 0).unwrap().is_none());
    bytes.extend_from_slice(&COMPACT[4..8]);
    assert!(poll(&mut bytes, &mut wait, wall, 1366).unwrap().is_none());
    // Capture contains no parser poll observations during this UART input gap.
    bytes.extend_from_slice(&COMPACT[8..]);
    assert!(
        format!(
            "{:?}",
            poll(&mut bytes, &mut wait, wall, 4_317_056).unwrap_err()
        )
        .contains("raw UART SDT crc32 mismatch")
    );
    one_recovered(&mut bytes, &mut wait, wall, 4_317_056, BARO);
}

#[test]
fn captured_compact_gap_with_explicit_poll_expires_only_at_existing_deadline() {
    let wall = Instant::now();
    let mut wait = RawUartFrameWait::default();
    let mut bytes = COMPACT[..4].to_vec();
    assert!(poll(&mut bytes, &mut wait, wall, 0).unwrap().is_none());
    bytes.extend_from_slice(&COMPACT[4..8]);
    assert!(poll(&mut bytes, &mut wait, wall, 1366).unwrap().is_none());
    assert!(
        poll(&mut bytes, &mut wait, wall, 1_999_000)
            .unwrap()
            .is_none()
    );
    assert_eq!(bytes, COMPACT[..8]);
    assert!(
        poll(&mut bytes, &mut wait, wall, 2_000_000)
            .unwrap()
            .is_none()
    );
    assert!(
        bytes.is_empty(),
        "existing timeout clears incomplete prefix, not a packet"
    );
    bytes.extend_from_slice(&COMPACT[8..]);
    one_recovered(&mut bytes, &mut wait, wall, 4_317_056, BARO);
}

#[test]
fn valid_unexpired_crc_wrapper_keeps_nested_frame_bytes_as_its_payload() {
    // A wrapper-valid synthetic chunk may carry frame-looking bytes. This is
    // only a framing invariant, not a claim that its SDK catalog is meaningful.
    let mut outer_body = b"SDT\x03".to_vec();
    outer_body.extend_from_slice(&0x12345678u32.to_le_bytes());
    outer_body.extend_from_slice(&0u16.to_le_bytes());
    outer_body.extend_from_slice(&1u16.to_le_bytes());
    outer_body.extend_from_slice(MESSAGE);
    outer_body.extend_from_slice(&crc32fast::hash(&outer_body).to_le_bytes());
    assert_valid_wrapper(&outer_body);
    let frame = build_raw_uart_frame(&outer_body).unwrap();
    let wall = Instant::now();
    let mut wait = RawUartFrameWait::default();
    let mut bytes = frame[..frame.len() - 1].to_vec();
    assert!(poll(&mut bytes, &mut wait, wall, 0).unwrap().is_none());
    assert!(
        poll(&mut bytes, &mut wait, wall, 1_999_000)
            .unwrap()
            .is_none()
    );
    assert_eq!(bytes, frame[..frame.len() - 1]);
    bytes.push(*frame.last().unwrap());
    let (kind, body) = poll(&mut bytes, &mut wait, wall, 1_999_999)
        .unwrap()
        .unwrap();
    assert!(matches!(kind, RawUartFrameKind::Data));
    assert_eq!(
        body, outer_body,
        "never preempt valid unexpired payload at nested sync"
    );
    assert!(
        poll(&mut bytes, &mut wait, wall, 1_999_999)
            .unwrap()
            .is_none()
    );
    assert!(bytes.is_empty());
}
