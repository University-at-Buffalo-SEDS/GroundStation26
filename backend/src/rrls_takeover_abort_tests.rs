// An RRLS in takeover owns the AV bay, so its operator's Abort must do what
// the primary's does: the ABORT and the Aborted FLIGHT_STATE write (RF and PB
// take an Abort only from FLIGHT_STATE). In fullnet runs C-dev and C-fix the
// takeover Abort reached RF in 37 ms but only the FC latched it: the RRLS
// never wrote FLIGHT_STATE. A listening RRLS still never writes it.
use super::*;
use crate::types::FlightState;
use sedsnet::router::{EndpointHandler, NetworkVariablePermissions, RouterConfig};
use std::sync::atomic::AtomicU8;

const CHILD: &str = "GS_RRLS_TAKEOVER_ABORT_TEST_CHILD";
const AV_BAY: [&str; 3] = ["RF", "FC", "PB"];

/// Runs `name` in a child process as an RRLS with a private variable cache.
fn rrls_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        assert!(crate::rrls::is_rrls());
        return false;
    }
    let root = std::env::temp_dir().join(format!("gs-rrls-takeover-abort-{}", rand::random::<u128>()));
    std::fs::create_dir_all(&root).unwrap();
    let case = format!("manual_abort_utc::rrls_takeover_abort_tests::{name}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &case, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("GS_RRLS_ROLE", "rrls")
        .env("GS_SIM_MODE", "0")
        .env_remove("GS_SIM_CLOCK_FILE")
        .env("GS_NETWORK_VARIABLE_CACHE", root.join("variables.json"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

struct Bay {
    state: Arc<AppState>,
    rrls: Arc<Router>,
    /// 0 deliver to the AV bay, 1 hold (no frame and so no ACK gets through).
    hold: Arc<AtomicU8>,
    held: Arc<Mutex<Vec<Vec<u8>>>>,
    peers: Vec<Arc<Router>>,
    names: Vec<&'static str>,
    /// While holding: the boards the radio still reaches (see `release_to`).
    open: Arc<Mutex<Vec<&'static str>>>,
}
impl Bay {
    fn pump(&self) {
        for _ in 0..10 {
            self.rrls.process_all_queues().unwrap();
            for peer in &self.peers {
                peer.process_all_queues().unwrap();
            }
        }
    }
    fn release(&self) {
        self.hold.store(0, Ordering::Relaxed);
        for bytes in self.held.lock().unwrap().drain(..) {
            for peer in &self.peers {
                peer.rx_packed_from_side(&bytes, 0).unwrap();
            }
        }
        self.pump();
    }
    /// Deliver what is on the air to the named boards only (the rest still owe).
    /// From now on the radio reaches the named boards again (the rest still
    /// owe); they also receive what was held.
    fn release_to(&self, boards: &[&'static str]) {
        self.open.lock().unwrap().extend_from_slice(boards);
        let held = self.held.lock().unwrap().clone();
        for (peer, name) in self.peers.iter().zip(&self.names) {
            if boards.contains(name) {
                for bytes in &held {
                    peer.rx_packed_from_side(bytes, 0).unwrap();
                }
            }
        }
        self.pump();
    }
    fn pending(&self, name: &str) -> Vec<u64> {
        let ty = crate::telemetry_schema::data_type(name);
        let mut owners: Vec<u64> = self.rrls.export_reliable_pending_stats().into_iter()
            .filter(|row| row.data_type == ty)
            .flat_map(|row| row.pending_destinations)
            .collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }
}

/// An RRLS whose radio reaches RF, FC and PB (discovered), as in a takeover.
async fn bay(takeover: bool) -> Bay {
    bay_with(takeover, false).await
}

/// `primary`: the primary was discovered too, then cut (its route outlives
/// the cut, so it is still listed), and it is not yet forgotten.
async fn bay_with(takeover: bool, primary: bool) -> Bay {
    crate::telemetry_schema::initialize().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let state = crate::state::tests::test_app_state().await;
    let clock = now.clone();
    let rrls = Arc::new(Router::new_with_clock(
        RouterConfig::new([]).with_sender("RRLS"),
        Box::new(move || clock.load(Ordering::Relaxed)),
    ));
    state.topology_router.set(rrls.clone()).unwrap();
    crate::network_variables::initialize(&rrls).unwrap();
    let mut peers = Vec::new();
    for name in AV_BAY {
        let clock = now.clone();
        let peer = Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("ABORT"),
                |_| Ok(()),
            )])
            .with_sender(name),
            Box::new(move || clock.load(Ordering::Relaxed)),
        ));
        peer.enable_network_variable(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            NetworkVariablePermissions::READ_ONLY,
        )
        .unwrap();
        let back = rrls.clone();
        peer.add_side_packed("RRLS", move |bytes| back.rx_packed_from_side(bytes, 0));
        peers.push(peer);
    }
    let hold = Arc::new(AtomicU8::new(0));
    let held = Arc::new(Mutex::new(Vec::new()));
    let open = Arc::new(Mutex::new(Vec::new()));
    let (phase, store, targets, reach) = (hold.clone(), held.clone(), peers.clone(), open.clone());
    rrls.add_side_packed("rocket_comms", move |bytes| {
        let holding = phase.load(Ordering::Relaxed) == 1;
        if holding {
            store.lock().unwrap().push(bytes.to_vec());
        }
        for (peer, name) in targets.iter().zip(AV_BAY) {
            if !holding || reach.lock().unwrap().contains(&name) {
                peer.rx_packed_from_side(bytes, 0)?;
            }
        }
        Ok(())
    });
    // The primary's link is side 1: the boards answer on side 0, the radio.
    let gs_up = Arc::new(std::sync::atomic::AtomicBool::new(primary));
    if primary {
        let clock = now.clone();
        let gs = Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("ABORT"), |_| Ok(()))])
                .with_sender("GS"),
            Box::new(move || clock.load(Ordering::Relaxed)),
        ));
        gs.enable_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"), NetworkVariablePermissions::READ_ONLY).unwrap();
        let (back, up) = (rrls.clone(), gs_up.clone());
        gs.add_side_packed("RRLS", move |bytes| if up.load(Ordering::Relaxed) { back.rx_packed_from_side(bytes, 0) } else { Ok(()) });
        let (to_gs, up) = (gs.clone(), gs_up.clone());
        rrls.add_side_packed("primary_link", move |bytes| if up.load(Ordering::Relaxed) { to_gs.rx_packed_from_side(bytes, 0) } else { Ok(()) });
        gs.announce_discovery().unwrap();
        for _ in 0..10 {
            gs.process_all_queues().unwrap();
            rrls.process_all_queues().unwrap();
        }
        std::mem::forget(gs);
    }
    let bay = Bay { state, rrls, hold, held, peers, names: AV_BAY.to_vec(), open };
    for peer in &bay.peers {
        peer.announce_discovery().unwrap();
    }
    bay.rrls.announce_discovery().unwrap();
    bay.pump();
    // The cut: from here the primary neither hears nor is heard.
    gs_up.store(false, Ordering::Relaxed);
    crate::rrls::set_takeover_for_test(takeover);
    // main.rs applies this on every takeover change.
    crate::network_variables::set_takeover_outputs_writable(&bay.rrls, takeover).unwrap();
    crate::telemetry_task::refresh_host_network_time(&bay.rrls, crate::telemetry_task::get_current_timestamp_ms());
    bay.state.set_preexisting_flight_state_for_test(FlightState::Idle);
    bay
}

fn press_abort(bay: &Bay) {
    bay.hold.store(1, Ordering::Relaxed);
    bay.state.manual_abort_intent.request();
    let mut claim = bay.state.manual_abort_intent.try_claim().unwrap();
    assert!(publish_local_claim(&bay.state, &bay.rrls, &mut claim).unwrap());
    claim.accepted();
    bay.pump();
}

#[tokio::test]
async fn a_takeover_rrls_abort_writes_aborted_flight_state_to_every_abort_owner() {
    if rrls_child("a_takeover_rrls_abort_writes_aborted_flight_state_to_every_abort_owner") {
        return;
    }
    let bay = bay(true).await;
    press_abort(&bay);
    let abort = bay.pending("ABORT");
    let flight_state = bay.pending("FLIGHT_STATE");
    assert_eq!(abort.len(), 3, "ABORT owed by RF, FC and PB until each ACKs: {abort:?}");
    assert_eq!(flight_state, abort, "the Aborted FLIGHT_STATE leaves with the ABORT, owed by the same boards");
    let latest = bay.rrls
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap()
        .expect("the RRLS wrote FLIGHT_STATE");
    assert_eq!(latest.payload(), [crate::types::NetworkFlightState::Aborted as u8]);
    assert_eq!(crate::types::canonical_sender_id(latest.sender()), "RRLS");
    assert_eq!(bay.state.local_flight_state_snapshot(), FlightState::Aborted);
    bay.release();
    assert!(bay.pending("ABORT").is_empty() && bay.pending("FLIGHT_STATE").is_empty(),
        "both are retired once every board has acknowledged");
    // The primary's re-send rules apply: the abort re-send path sends the
    // retained Aborted FLIGHT_STATE again (it re-sends only this router's own).
    bay.hold.store(1, Ordering::Relaxed);
    assert!(crate::network_variables::resend_aborted_flight_state(&bay.rrls).unwrap());
    bay.pump();
    assert_eq!(bay.pending("FLIGHT_STATE").len(), 3);
}

#[tokio::test]
async fn a_listening_rrls_abort_sends_only_the_abort() {
    if rrls_child("a_listening_rrls_abort_sends_only_the_abort") {
        return;
    }
    let bay = bay(false).await;
    press_abort(&bay);
    assert_eq!(bay.pending("ABORT").len(), 3);
    assert!(bay.pending("FLIGHT_STATE").is_empty(), "the primary owns the desired state");
    assert!(bay.rrls
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap()
        .is_none_or(|latest| crate::types::canonical_sender_id(latest.sender()) != "RRLS"));
}

fn hash(name: &str) -> u64 {
    sedsnet::packet::hash_bytes_u64(0x517C_C1B7_2722_0A95, name.as_bytes())
}

/// The AV-bay boards an Abort service sees owing `name`'s pending frames,
/// and whether any remaining owner is not a board (manual_abort_resend and
/// abort_retirement map owners the same way).
fn owed_boards(bay: &Bay, name: &str) -> (u8, bool) {
    let ty = crate::telemetry_schema::data_type(name);
    let (mut mask, mut unnamed) = (0u8, false);
    for row in bay.rrls.export_reliable_pending_stats().into_iter().filter(|row| row.data_type == ty) {
        for owner in row.pending_destinations {
            match crate::board_recovery::coordinator::pending_destination_board(&bay.rrls, owner)
                .and_then(|board| crate::board_abort::BOARDS.iter().position(|b| *b == board))
            {
                Some(i) => mask |= 1 << i,
                None => unnamed = true,
            }
        }
    }
    (mask, unnamed)
}

/// Forgetting the out-of-range primary (an RRLS taking over) only removes
/// the primary from the owners of a pending ABORT and Aborted FLIGHT_STATE:
/// neither completes while any AV-bay board still owes its acknowledgement,
/// and both Abort services then see exactly the boards that still owe it.
#[tokio::test]
async fn forgetting_the_primary_never_completes_an_abort_an_av_bay_board_still_owes() {
    if rrls_child("forgetting_the_primary_never_completes_an_abort_an_av_bay_board_still_owes") {
        return;
    }
    let bay = bay_with(true, true).await;
    press_abort(&bay);
    let gs = hash("GS");
    let mut boards: Vec<u64> = AV_BAY.iter().map(|name| hash(name)).collect();
    boards.sort_unstable();
    let mut with_gs = boards.clone();
    with_gs.push(gs);
    with_gs.sort_unstable();
    assert_eq!(bay.pending("ABORT"), with_gs, "owed to the still-listed primary as well");
    assert_eq!(bay.pending("FLIGHT_STATE"), with_gs);
    assert_eq!(owed_boards(&bay, "ABORT"), (0b111, true), "the primary is an owner no service can name");

    crate::network_variables::takeover_changed(&bay.rrls, true).unwrap();
    for name in ["ABORT", "FLIGHT_STATE"] {
        assert_eq!(bay.pending(name), boards, "{name}: only the primary is retired");
        assert_eq!(owed_boards(&bay, name), (0b111, false), "{name}: every board still owes it");
    }
    bay.release_to(&["RF", "FC"]);
    for name in ["ABORT", "FLIGHT_STATE"] {
        assert_eq!(bay.pending(name), vec![hash("PB")], "{name}: pending until PB acknowledges");
        assert_eq!(owed_boards(&bay, name), (1 << 2, false));
    }
    bay.release_to(&["PB"]);
    assert!(bay.pending("ABORT").is_empty() && bay.pending("FLIGHT_STATE").is_empty());
}
