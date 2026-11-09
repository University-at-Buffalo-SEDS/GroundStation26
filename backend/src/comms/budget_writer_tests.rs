use super::*;
use crate::rrls::budget_negotiation::{Nonce, Profile};
use std::io::Read;
use std::sync::{Arc, Mutex};
fn no_entropy() -> Option<Nonce> {
    None
}
fn repeated_entropy() -> Option<Nonce> {
    Nonce::new([1; 16])
}
fn clock(path: &std::path::Path, now: u64) {
    let next = path.with_extension("next");
    std::fs::write(&next, now.to_string()).unwrap();
    std::fs::rename(next, path).unwrap();
    assert_eq!(crate::simulation_clock_ms(), Some(now));
}
fn pair(
    entropy: Option<fn() -> Option<Nonce>>,
) -> (UartComms, serialport::TTYPort, crate::rrls::Shared) {
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(1)).unwrap();
    let mut mesh = crate::rrls::Mesh::new(crate::rrls::Role::Primary, 7);
    if let Some(entropy) = entropy {
        mesh.enable_budget_with_entropy(Profile::shipped(), entropy);
    } else {
        mesh.enable_budget(Profile::shipped());
    }
    let shared = Arc::new(Mutex::new(mesh));
    let mut uart = UartComms::test_mesh_pty(port, shared.clone());
    uart.enable_radio_capacity_control();
    (uart, peer, shared)
}
fn drain(peer: &mut serialport::TTYPort) -> Vec<u8> {
    let until = Instant::now() + Duration::from_millis(50);
    while peer.bytes_to_read().unwrap() == 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    let count = peer.bytes_to_read().unwrap() as usize;
    let mut bytes = vec![0; count];
    if count > 0 {
        peer.read_exact(&mut bytes).unwrap();
    }
    bytes
}
#[test]
fn actual_budget_uart_entropy_and_effective_override() {
    if !cfg!(feature = "firmware_sim") {
        crate::mode_suite!(Hitl);
    }
    const NAME: &str =
        "comms::budget_writer_tests::actual_budget_uart_entropy_and_effective_override";
    if std::env::var_os("GS_BUDGET_WRITER_CHILD").is_none() {
        let folder = std::env::temp_dir().join(format!(
            "gs-budget-writer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("clock"), "0").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env("GS_BUDGET_WRITER_CHILD", "1")
            .env("GS_SIM_CLOCK_FILE", folder.join("clock"))
            .env("GS_SIMULATED_SERIAL_PTY", "1")
            .env("GS_RRLS_ROLE", "primary")
            .env_remove("GS_RADIO_AIR_SHARE_PERCENT")
            .env_remove("GS_RADIO_AIR_BIT_RATE_BPS")
            .env_remove("GS_RADIO_AIR_FRAME_OVERHEAD_BYTES")
            .env_remove("GS_RADIO_PACKET_BURST_MS")
            .env_remove("GS_RADIO_PACKET_GAP_MS")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= until {
                child.kill().unwrap();
                panic!("boundedactualUARTfixturetimedout");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let result = child.wait_with_output().unwrap();
        std::fs::write(folder.join("stdout"), &result.stdout).unwrap();
        std::fs::write(folder.join("stderr"), &result.stderr).unwrap();
        println!("actual-budget-uart artifacts {}", folder.display());
        print!("{}", String::from_utf8_lossy(&result.stdout));
        eprint!("{}", String::from_utf8_lossy(&result.stderr));
        assert!(result.status.success());
        return;
    }
    let path = std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
    clock(&path, 0);
    rfd_profile::initialize(&crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "native-budget-writer".into(),
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115200,
            protocol: SerialProtocol::RawUart,
        },
    })
    .unwrap();
    for entropy in [no_entropy as fn() -> Option<Nonce>, repeated_entropy] {
        let (mut uart, mut peer, shared) = pair(Some(entropy));
        uart.service_transmit().unwrap();
        assert_eq!(
            uart.write_lock
                .lock()
                .unwrap()
                .radio_capacity
                .as_ref()
                .unwrap()
                .airtime_config(),
            (30000, 18)
        );
        let bytes = drain(&mut peer);
        assert!(
            !bytes.is_empty(),
            "actual HEALTH physical write uses the primary's full share despite constructor failure"
        );
        assert_eq!(shared.lock().unwrap().negotiated_air_share(0), Some(15));
        assert_eq!(shared.lock().unwrap().recovery_air_share(0), None);
        println!("entropyfailure actualHEALTH {}B capacity25", bytes.len());
    }
    let (mut uart, mut peer, shared) = pair(None);
    uart.write_lock.lock().unwrap().radio_capacity =
        Some(rfd_capacity::Capacity::new(64000, 25, 16));
    assert!(uart.service_transmit().is_err());
    assert!(uart.send_data_with_priority(&[0; 32], 0).is_err());
    assert!(
        drain(&mut peer).is_empty(),
        "contradictory64kdoesnotwriteANYbytes"
    );
    assert!(shared.lock().unwrap().budget_transport_fault_for_test());
    assert_eq!(shared.lock().unwrap().negotiated_air_share(0), Some(15));
    uart.write_lock.lock().unwrap().radio_capacity =
        Some(rfd_profile::capacity(115200));
    clock(&path, 10);
    uart.service_transmit().unwrap();
    assert!(!shared.lock().unwrap().budget_transport_fault_for_test());
    assert!(
        !drain(&mut peer).is_empty(),
        "validreconnectresumeswithfreshconservativecontext"
    );
    assert_eq!(
        uart.write_lock
            .lock()
            .unwrap()
            .radio_capacity
            .as_ref()
            .unwrap()
            .airtime_config(),
        (30000, 18)
    );
    writer_debt_case(&path, 204, 10_000);
    writer_debt_case(&path, 374, 20_000);
    reserved_revoke_keeps_wire_context_after_actual_refusal(&path, 0, 30_000);
    reserved_revoke_keeps_wire_context_after_actual_refusal(&path, 7, 40_000);
}
fn logical_send(from: &mut crate::rrls::Mesh, to: &mut crate::rrls::Mesh, now: u64) -> Vec<u8> {
    let tx = from.begin_transmission(now).unwrap();
    let bytes = tx.frame().bytes.clone();
    from.finish_transmission(tx, true);
    to.incoming(true, &bytes, now);
    bytes
}
fn healthy_writer(shared: &crate::rrls::Shared, now: u64) -> crate::rrls::Mesh {
    let mut relay = crate::rrls::Mesh::new(crate::rrls::Role::Rrls, 9);
    relay.enable_budget(Profile::shipped());
    let mut primary = shared.lock().unwrap();
    primary.tick(now);
    assert_eq!(logical_send(&mut primary, &mut relay, now)[3], 0);
    assert_eq!(logical_send(&mut primary, &mut relay, now)[3], 13);
    relay.tick(now);
    assert_eq!(logical_send(&mut relay, &mut primary, now)[3], 13);
    assert_eq!(primary.negotiated_air_share(now), Some(25));
    relay
}
fn rf_reply(query: &[u8]) -> Vec<u8> {
    let mut b = vec![b'R', b'R', 1, 14, 1, 1];
    b.extend_from_slice(&query[6..22]);
    // The RF reports the shipped profile's tuple.
    let p = Profile::shipped();
    b.extend_from_slice(&p.baud.to_le_bytes());
    b.extend_from_slice(&p.usable_bps.to_le_bytes());
    b.push(p.rf_share);
    for v in [p.overhead, p.burst_ms, p.gap_ms] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.push(0);
    let crc = crate::rrls::budget_negotiation::crc(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}
fn writer_debt_case(path: &std::path::Path, wire_bytes: usize, start: u64) {
    clock(path, start);
    let (mut uart, mut peer, shared) = pair(None);
    let mut relay = healthy_writer(&shared, start);
    clock(path, start + 2999);
    uart.send_data_with_priority(&vec![0; wire_bytes - 12], 0)
        .unwrap();
    let old = drain(&mut peer);
    assert_eq!(old.len(), wire_bytes);
    assert_eq!(
        uart.write_lock
            .lock()
            .unwrap()
            .radio_capacity
            .as_ref()
            .unwrap()
            .airtime_config(),
        (30000, 18)
    );
    clock(path, start + 3000);
    uart.service_transmit().unwrap();
    assert!(
        drain(&mut peer).is_empty(),
        "oldUARTowner remainsserialized"
    );
    let before_next = uart
        .write_lock
        .lock()
        .unwrap()
        .radio_capacity
        .as_ref()
        .unwrap()
        .next_air_virtual_ms()
        .unwrap();
    // Just before the old write's air debt is paid.
    assert!(before_next > start + 3001, "the old write leaves air debt");
    clock(path, before_next - 1);
    uart.service_transmit().unwrap();
    uart.service_background_transmit().unwrap();
    assert!(
        drain(&mut peer).is_empty(),
        "downshift/debt/quietcheckedbeforefalseHEALTHorGRANTbytes"
    );
    let after_next = {
        let guard = uart.write_lock.lock().unwrap();
        let cap = guard.radio_capacity.as_ref().unwrap();
        // A relay request no longer downshifts the primary: its share and
        // its unpaid air debt are both kept exactly.
        assert_eq!(cap.airtime_config(), (30000, 18));
        cap.next_air_virtual_ms().unwrap()
    };
    assert_eq!(after_next, before_next, "unpaid air debt is retained, never forgiven");
    let mut buffered = Vec::new();
    let mut grant_at = None;
    let mut false_health_at = None;
    let mut last_health_seq = 0;
    for now in (before_next.max(start + 3090)..=start + 5000).step_by(10) {
        clock(path, now);
        uart.service_transmit().unwrap();
        // Actual worker may reserve the ordinary turn for pending SDK traffic.
        // A negotiation owner must still use paid protected service, never a loan.
        uart.service_background_transmit_counted(false).unwrap();
        if uart
            .write_lock
            .lock()
            .unwrap()
            .radio_capacity
            .as_ref()
            .unwrap()
            .ready(Instant::now(), Some(now))
        {
            uart.send_data_with_priority(&[0; 192], 0).unwrap();
        }
        buffered.extend(drain(&mut peer));
        while buffered.len() >= 4 {
            assert_eq!(&buffered[..2], &[0xa6, 0x5b]);
            let n = usize::from(u16::from_le_bytes(buffered[2..4].try_into().unwrap()));
            if buffered.len() < 4 + n {
                break;
            }
            let body = buffered[4..4 + n].to_vec();
            buffered.drain(..4 + n);
            assert_eq!(
                uart.write_lock
                    .lock()
                    .unwrap()
                    .radio_capacity
                    .as_ref()
                    .unwrap()
                    .airtime_config(),
                (30000, 18)
            );
            if body[3] == 0 {
                assert_eq!(body[12], 0);
                last_health_seq = u32::from_le_bytes(body[8..12].try_into().unwrap());
                let first = false_health_at.is_none();
                false_health_at.get_or_insert(now);
                relay.incoming(true, &body, now);
                relay.tick(now);
                if first {
                    let mut query = None;
                    for _ in 0..4 {
                        let q = relay
                            .begin_transmission(now)
                            .expect("normaltick queues RFproof after READY");
                        let bytes = q.frame().bytes.clone();
                        relay.finish_transmission(q, true);
                        match bytes[3] {
                            7 => {
                                shared.lock().unwrap().incoming(true, &bytes, now);
                            }
                            14 => {
                                query = Some(bytes);
                                break;
                            }
                            other => panic!("unexpected relay prerequisite {other}"),
                        }
                    }
                    let query = query.expect("bounded READY drain reaches actual RFquery");
                    relay.incoming(true, &rf_reply(&query), now);
                    let offer = relay.begin_transmission(now).unwrap();
                    assert_eq!(offer.frame().bytes[3], 13);
                    let bytes = offer.frame().bytes.clone();
                    relay.finish_transmission(offer, true);
                    shared.lock().unwrap().incoming(true, &bytes, now);
                    shared.lock().unwrap().tick(now);
                }
            } else if body[3] == 13 && body[5] == 1 {
                grant_at = Some(now);
                relay.incoming(true, &body, now);
                assert_eq!(relay.recovery_air_share(now), Some(50));
                break;
            }
        }
        if grant_at.is_some() {
            break;
        }
    }
    assert!(false_health_at.is_some());
    let grant_at = grant_at.expect("realwriter makesprotocolprogressafterpaid204/374Bowner");
    // The primary keeps its pacing throughout (18% of 30 kbit/s until a
    // readback proves the modems' S5): the GRANT waits at least for the paid
    // owner's airtime, which is no shorter than at the full 3750 B/s.
    assert!(grant_at - (start + 2999) >= (wire_bytes as u64 + 16) * 1000 / 3750);
    assert!(grant_at < start + 5000);
    println!(
        "actualpaidowner{wire_bytes}B falseHEALTHat{:?} GRANTat{} capacity18",
        false_health_at.map(|t| t - start),
        grant_at - start
    );
    if wire_bytes == 374 {
        inactive_base_ack_writer(path, relay, grant_at + 1, last_health_seq + 1);
    }
}

fn inactive_base_ack_writer(
    path: &std::path::Path,
    mut relay: crate::rrls::Mesh,
    now: u64,
    seq: u32,
) {
    use crate::rrls::budget_negotiation::{Context, Message, encode};
    clock(path, now);
    assert_eq!(relay.recovery_air_share(now), Some(50));
    let mut health = vec![b'R', b'R', 1, 0];
    health.extend_from_slice(&7u32.to_le_bytes());
    health.extend_from_slice(&seq.to_le_bytes());
    health.push(1);
    let crc = crate::rrls::budget_negotiation::crc(&health);
    health.extend_from_slice(&crc.to_le_bytes());
    relay.incoming(true, &health, now);
    relay.tick(now);
    let revoke = encode(Message::Revoke(Context {
        session: 7,
        seq,
        nonce: Nonce::new([33; 16]).unwrap(),
    }));
    relay.incoming(true, revoke.wire(), now);
    relay.tick(now);
    assert_eq!(relay.recovery_air_share(now), Some(35));
    let shared = Arc::new(Mutex::new(relay));
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(1)).unwrap();
    let mut uart = UartComms::test_mesh_pty(port, shared.clone());
    uart.enable_radio_capacity_control();
    uart.write_lock.lock().unwrap().radio_capacity =
        Some(rfd_profile::capacity(115200));
    uart.service_transmit().unwrap();
    uart.service_background_transmit().unwrap();
    let bytes = drain(&mut peer);
    assert!(bytes.len() >= 4);
    assert_eq!(&bytes[..2], &[0xa6, 0x5b]);
    let n = usize::from(u16::from_le_bytes(bytes[2..4].try_into().unwrap()));
    assert!(bytes.len() >= n + 4);
    let body = &bytes[4..4 + n];
    assert!(
        matches!(
            crate::rrls::budget_negotiation::decode(body),
            Some(Message::BaseAck { .. })
        ),
        "inactive normalclear retains real BASE_ACK owner"
    );
    assert_eq!(
        uart.write_lock
            .lock()
            .unwrap()
            .radio_capacity
            .as_ref()
            .unwrap()
            .airtime_config(),
        (30000, 18)
    );
    assert_eq!(shared.lock().unwrap().recovery_air_share(now), Some(35));
    println!(
        "actualinactiveRRLS BASE_ACK {}B capacity35 beforebytes",
        n + 4
    );
}

fn reserved_revoke_keeps_wire_context_after_actual_refusal(
    path: &std::path::Path,
    limit: usize,
    start: u64,
) {
    clock(path, start);
    let (mut uart, mut peer, shared) = pair(None);
    uart.service_transmit().unwrap();
    let health0 = drain(&mut peer);
    assert_eq!(health0[7], 0);
    clock(path, start + 65);
    uart.test_scalar_limit(Some(limit));
    let result = uart.service_paid_budget_transmit_counted();
    let remaining = uart.test_scalar_limit(None).unwrap();
    assert_eq!(remaining, 0);
    assert!(result.is_err());
    let physical = drain(&mut peer);
    assert_eq!(physical.len(), limit);
    let (frozen, ever) = shared.lock().unwrap().queued_budget_for_test().unwrap();
    assert!(
        ever,
        "actual typed reservation freezes before zero/partial IO"
    );
    let Some(crate::rrls::budget_negotiation::Message::Revoke(context)) =
        crate::rrls::budget_negotiation::decode(&frozen)
    else {
        panic!()
    };
    assert_eq!(context.seq, 0);
    clock(path, start + 500);
    uart.service_transmit().unwrap();
    let health1 = drain(&mut peer);
    assert_eq!(u32::from_le_bytes(health1[12..16].try_into().unwrap()), 1);
    let (after, ever) = shared.lock().unwrap().queued_budget_for_test().unwrap();
    assert!(ever);
    assert_eq!(
        after, frozen,
        "later full HEALTH must not rewrite previously reserved bytes"
    );
    clock(path, start + 565);
    let progress = uart
        .service_paid_budget_transmit_counted()
        .unwrap()
        .unwrap();
    assert!(progress.physical_completed);
    let retry = drain(&mut peer);
    assert_eq!(&retry[4..], frozen.as_slice());
    assert_eq!(
        uart.write_lock
            .lock()
            .unwrap()
            .radio_capacity
            .as_ref()
            .unwrap()
            .airtime_config(),
        (30000, 18)
    );
    println!(
        "ACTUAL_RESERVED_REVOKE prefix{limit} frozen_seq0_after_full_HEALTH1 fullretry{}B",
        retry.len()
    );
}
