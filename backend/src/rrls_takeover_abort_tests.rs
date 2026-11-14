// An RRLS in takeover owns the AV bay, so its operator's Abort must do what
// the primary's does: the ABORT and the Aborted FLIGHT_STATE write (RF and PB
// take an Abort only from FLIGHT_STATE). In fullnet runs C-dev and C-fix the
// takeover Abort reached RF in 37 ms but only the FC latched it: the RRLS
// never wrote FLIGHT_STATE. A listening RRLS still never writes it.
use super::*;
use crate::types::{Board, FlightState};
use sedsnet::router::{EndpointHandler, NetworkVariablePermissions, RouterConfig};
use std::sync::atomic::AtomicU8;

const CHILD: &str = "GS_RRLS_TAKEOVER_ABORT_TEST_CHILD";
const AV_BAY: [&str; 3] = ["RF", "FC", "PB"];

/// Runs `name` in a child process as an RRLS with a private variable cache.
fn rrls_child(name: &str) -> bool {
    station_child(name, true)
}

/// Runs `name` in a child process as the primary with a private variable cache.
fn primary_child(name: &str) -> bool {
    station_child(name, false)
}

fn station_child(name: &str, rrls: bool) -> bool {
    if std::env::var_os(CHILD).is_some() {
        assert_eq!(crate::rrls::is_rrls(), rrls);
        return false;
    }
    let root = std::env::temp_dir().join(format!("gs-rrls-takeover-abort-{}", rand::random::<u128>()));
    std::fs::create_dir_all(&root).unwrap();
    let case = format!("manual_abort_utc::rrls_takeover_abort_tests::{name}");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    if rrls {
        command.env("GS_RRLS_ROLE", "rrls");
    } else {
        command.env_remove("GS_RRLS_ROLE");
    }
    let output = command
        .args(["--exact", &case, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
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
    /// Every router's clock (ms).
    now: Arc<AtomicU64>,
    state: Arc<AppState>,
    rrls: Arc<Router>,
    /// 0 deliver to the AV bay, 1 hold (no frame and so no ACK gets through).
    hold: Arc<AtomicU8>,
    held: Arc<Mutex<Vec<Vec<u8>>>>,
    peers: Vec<Arc<Router>>,
    names: Vec<&'static str>,
    /// While holding: the boards the radio still reaches (see `release_to`).
    open: Arc<Mutex<Vec<&'static str>>>,
    /// Boards that received an ABORT, in order.
    abort_rx: Arc<Mutex<Vec<&'static str>>>,
    /// While holding, a board the radio does not reach is not heard either.
    both_ways: Arc<std::sync::atomic::AtomicBool>,
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
    // The station under test: the RRLS, or the primary in a primary child.
    let station = if crate::rrls::is_rrls() { "RRLS" } else { "GS" };
    let rrls = Arc::new(Router::new_with_clock(
        RouterConfig::new([]).with_sender(station),
        Box::new(move || clock.load(Ordering::Relaxed)),
    ));
    state.topology_router.set(rrls.clone()).unwrap();
    crate::network_variables::initialize(&rrls).unwrap();
    let mut peers = Vec::new();
    let abort_rx = Arc::new(Mutex::new(Vec::new()));
    let hold = Arc::new(AtomicU8::new(0));
    let open = Arc::new(Mutex::new(Vec::new()));
    let both_ways = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for name in AV_BAY {
        let clock = now.clone();
        let peer = Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("ABORT"),
                {
                    let abort_rx = abort_rx.clone();
                    move |_| {
                        abort_rx.lock().unwrap().push(name);
                        Ok(())
                    }
                },
            )])
            .with_sender(name),
            Box::new(move || clock.load(Ordering::Relaxed)),
        ));
        peer.enable_network_variable(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            NetworkVariablePermissions::READ_ONLY,
        )
        .unwrap();
        let (back, phase, reach, gated) = (rrls.clone(), hold.clone(), open.clone(), both_ways.clone());
        peer.add_side_packed("RRLS", move |bytes| {
            if gated.load(Ordering::Relaxed) && phase.load(Ordering::Relaxed) == 1 && !reach.lock().unwrap().contains(&name) {
                return Ok(());
            }
            back.rx_packed_from_side(bytes, 0)
        });
        peers.push(peer);
    }
    let held = Arc::new(Mutex::new(Vec::new()));
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
    let bay = Bay { now, state, rrls, hold, held, peers, names: AV_BAY.to_vec(), open, abort_rx, both_ways };
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
    // An RRLS runs no time sync (telemetry_task::timesync_enabled), so its router
    // has no network UTC: fullnet C-fix2 failed the takeover FLIGHT_STATE write
    // on exactly that ("network UTC is not available").
    if crate::rrls::is_rrls() {
        assert!(bay.rrls.network_time_ms().is_none());
    } else {
        // The primary runs time sync: its controls carry network UTC.
        bay.rrls
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_945_600_000))
            .unwrap();
    }
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

/// FLIGHT_STATE frames the RRLS put on the air since the last clear:
/// (sender, value).
fn rrls_flight_states(bay: &Bay) -> Vec<(String, u8)> {
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    bay.held.lock().unwrap().iter()
        .filter_map(|bytes| sedsnet::wire_format::unpack_packet(bytes).ok())
        .filter(|packet| packet.data_type() == ty)
        .map(|packet| (crate::types::canonical_sender_id(packet.sender()).to_string(), packet.payload().first().copied().unwrap_or(0)))
        .collect()
}

/// A board's request for FLIGHT_STATE from the RRLS in takeover.
fn board_requests_flight_state(bay: &Bay) {
    bay.held.lock().unwrap().clear();
    bay.hold.store(1, Ordering::Relaxed);
    bay.peers[0].request_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap();
    bay.pump();
}

/// fullnet Cshort-fix11: an RRLS in takeover answered the boards' periodic
/// FLIGHT_STATE requests from its cache (it held the variable writable for the
/// whole takeover), replaying the flight computer's own InFlight report 11
/// times. A station serving a cached FLIGHT_STATE to the AV bay is how a stale
/// value could come back after an Abort or a Reset. The RRLS never answers
/// FLIGHT_STATE requests.
#[tokio::test]
async fn a_takeover_rrls_never_answers_a_flight_state_request() {
    if rrls_child("a_takeover_rrls_never_answers_a_flight_state_request") {
        return;
    }
    let bay = bay(true).await;
    press_abort(&bay);
    bay.release();
    board_requests_flight_state(&bay);
    assert_eq!(rrls_flight_states(&bay), vec![], "no answer from the RRLS's cache");
}

/// No FLIGHT_STATE frame but the RRLS's own Aborted write leaves it in a
/// takeover, even while it holds the flight computer's report as the latest.
#[tokio::test]
async fn only_the_rrls_own_aborted_flight_state_leaves_it_in_takeover() {
    if rrls_child("only_the_rrls_own_aborted_flight_state_leaves_it_in_takeover") {
        return;
    }
    let bay = bay(true).await;
    // The flight computer's report (InFlight) reaches the RRLS.
    let report = sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("FLIGHT_STATE"),
        &[crate::telemetry_schema::endpoint("FLIGHT_STATE")],
        "FC",
        crate::telemetry_task::get_current_timestamp_ms(),
        std::sync::Arc::from([crate::types::NetworkFlightState::InFlight as u8]),
    )
    .unwrap();
    let _ = bay.rrls.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&report), 0);
    bay.pump();
    board_requests_flight_state(&bay);
    assert_eq!(rrls_flight_states(&bay), vec![], "the report is never served back");
    bay.held.lock().unwrap().clear();
    press_abort(&bay);
    let sent = rrls_flight_states(&bay);
    assert!(!sent.is_empty() && sent.iter().all(|f| *f == ("RRLS".to_string(), crate::types::NetworkFlightState::Aborted as u8)),
        "only its own Aborted write: {sent:?}");
}

/// The RRLS's own Aborted write never opens FLIGHT_STATE to writes, not even
/// while it is being sent: a board request handled during the write would be
/// answered from the cache. Probed from RF as the Aborted frame arrives (inside
/// the RRLS's send): a write attempt there must be refused as read-only.
#[tokio::test]
async fn flight_state_stays_read_only_while_the_rrls_abort_is_sent() {
    if rrls_child("flight_state_stays_read_only_while_the_rrls_abort_is_sent") {
        return;
    }
    let bay = bay(true).await;
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let probes = Arc::new(Mutex::new(Vec::<bool>::new()));
    let (seen, weak) = (probes.clone(), Arc::downgrade(&bay.rrls));
    bay.peers[0].on_network_variable_update(ty, move |packet| {
        if crate::types::canonical_sender_id(packet.sender()) != "RRLS" {
            return Ok(());
        }
        if let Some(rrls) = weak.upgrade() {
            let refused = matches!(rrls.set_network_variable(packet.clone()), Err(sedsnet::TelemetryError::PermissionDenied));
            seen.lock().unwrap().push(refused);
        }
        Ok(())
    }).unwrap();
    // Delivered as sent, so RF handles the frame inside the RRLS's write.
    bay.state.manual_abort_intent.request();
    let mut claim = bay.state.manual_abort_intent.try_claim().unwrap();
    assert!(publish_local_claim(&bay.state, &bay.rrls, &mut claim).unwrap());
    claim.accepted();
    bay.pump();
    let probes = probes.lock().unwrap().clone();
    assert!(!probes.is_empty() && probes.iter().all(|refused| *refused), "writable while sending: {probes:?}");
}

impl Bay {
    /// Run the clock until no `name` frame is pending: the owners still
    /// unreachable exhaust their retries.
    fn exhaust(&self, name: &str) {
        for _ in 0..20_000 {
            if self.pending(name).is_empty() {
                return;
            }
            self.now.fetch_add(100, Ordering::Relaxed);
            // Exhaustion surfaces as a queue error, as on the router worker.
            for _ in 0..10 {
                let _ = self.rrls.process_all_queues();
                for peer in &self.peers {
                    let _ = peer.process_all_queues();
                }
            }
        }
        panic!("{name} still pending: {:?}", self.pending(name));
    }
}

/// The flight computer's own state report (InFlight) reaching the station.
fn fc_reports_in_flight(bay: &Bay) {
    let report = sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("FLIGHT_STATE"),
        &[crate::telemetry_schema::endpoint("FLIGHT_STATE")],
        "FC",
        bay.rrls.network_time_ms().unwrap_or_else(crate::telemetry_task::get_current_timestamp_ms) + 1_000,
        std::sync::Arc::from([crate::types::NetworkFlightState::InFlight as u8]),
    )
    .unwrap();
    let _ = bay.rrls.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&report), 0);
    bay.pump();
}

/// The station's Aborted FLIGHT_STATE reached RF and FC, but PB's ACK never
/// came back and the write ran out of retries; the flight computer then
/// reported InFlight, so its report is the newest FLIGHT_STATE the station
/// holds. The Aborted re-send must still go to PB (PB takes an Abort only
/// from FLIGHT_STATE): the station re-sends its own write, never a report.
fn aborted_resend_after_a_newer_report_reaches_pb(bay: &Bay, station: &str) {
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    assert_eq!(bay.pending("FLIGHT_STATE").len(), 3, "the Aborted write is owed by RF, FC and PB");
    bay.release_to(&["RF", "FC"]);
    bay.exhaust("FLIGHT_STATE");
    fc_reports_in_flight(bay);
    assert_eq!(bay.rrls.get_cached_network_variable(ty).unwrap().map(|p| p.payload()[0]),
        Some(crate::types::NetworkFlightState::InFlight as u8), "the report is the newest observation");
    bay.held.lock().unwrap().clear();
    assert!(crate::network_variables::resend_aborted_flight_state(&bay.rrls).unwrap(), "the Aborted write is re-sent");
    bay.pump();
    assert!(bay.pending("FLIGHT_STATE").contains(&hash("PB")), "PB owes the re-send");
    let sent = rrls_flight_states(bay);
    assert!(!sent.is_empty() && sent.iter().all(|f| *f == (station.to_string(), crate::types::NetworkFlightState::Aborted as u8)),
        "only the station's own Aborted write: {sent:?}");
}

#[tokio::test]
async fn a_primary_resends_its_aborted_flight_state_to_pb_after_a_newer_fc_report() {
    if primary_child("a_primary_resends_its_aborted_flight_state_to_pb_after_a_newer_fc_report") {
        return;
    }
    let bay = bay(false).await;
    bay.hold.store(1, Ordering::Relaxed);
    crate::network_variables::set_flight_state(&bay.rrls, FlightState::Aborted as u8).unwrap();
    bay.pump();
    aborted_resend_after_a_newer_report_reaches_pb(&bay, "GS");
}

#[tokio::test]
async fn a_takeover_rrls_resends_its_aborted_flight_state_to_pb_after_a_newer_fc_report() {
    if rrls_child("a_takeover_rrls_resends_its_aborted_flight_state_to_pb_after_a_newer_fc_report") {
        return;
    }
    let bay = bay(true).await;
    press_abort(&bay);
    aborted_resend_after_a_newer_report_reaches_pb(&bay, "RRLS");
}

/// A board's FLIGHT_STATE request to the primary is answered with the
/// primary's own last write, even after the flight computer's newer report
/// (the report is a state report, which every board ignores as a write).
#[tokio::test]
async fn a_primary_answers_a_flight_state_request_with_its_own_write_after_an_fc_report() {
    if primary_child("a_primary_answers_a_flight_state_request_with_its_own_write_after_an_fc_report") {
        return;
    }
    let bay = bay(false).await;
    crate::network_variables::set_flight_state(&bay.rrls, FlightState::Aborted as u8).unwrap();
    bay.pump();
    assert!(bay.pending("FLIGHT_STATE").is_empty());
    fc_reports_in_flight(&bay);
    board_requests_flight_state(&bay);
    assert_eq!(rrls_flight_states(&bay), vec![("GS".to_string(), crate::types::NetworkFlightState::Aborted as u8)]);
}

/// The operator Abort as main.rs publishes it: sent, then armed for re-send.
fn press_abort_armed(bay: &Bay) {
    bay.hold.store(1, Ordering::Relaxed);
    bay.state.manual_abort_intent.request();
    let mut claim = bay.state.manual_abort_intent.try_claim().unwrap();
    let mark = crate::manual_abort_resend::send_mark(&bay.rrls);
    assert!(mark.av_bay_routed(), "the AV bay is routed when the Abort is sent");
    assert!(publish_local_claim(&bay.state, &bay.rrls, &mut claim).unwrap());
    let wire = claim.wire().expect("an operator Abort carries its wire");
    let recovery = crate::board_recovery::coordinator::recovery_activity(&bay.state).flatten();
    bay.state.abort_resend.arm(claim.epoch(), wire, recovery, mark);
    claim.accepted();
    bay.pump();
}

fn mark_present(bay: &Bay, boards: &[Board]) {
    let mut status = bay.state.board_status.lock().unwrap();
    for board in boards {
        status.get_mut(board).unwrap().last_seen_instant = Some(std::time::Instant::now());
    }
}

/// The station's production services on the shared clock: its router worker
/// (crate::telemetry_task::radio_io), the boards, and the Abort re-send
/// service's pass (manual_abort_resend::service), until `done` or `ms` of
/// station time. The worker samples retrying writes on wall time, so each
/// 200 ms of station time takes 100 ms.
fn run_services(bay: &Bay, worker: &mut crate::telemetry_task::RouterServiceForTest, ms: u64, mut done: impl FnMut(&Bay) -> bool) -> bool {
    for _ in 0..ms / 200 {
        bay.now.fetch_add(200, Ordering::Relaxed);
        worker.pump(&bay.rrls);
        for peer in &bay.peers {
            let _ = peer.process_all_queues();
        }
        worker.pump(&bay.rrls);
        if done(bay) {
            return true;
        }
        if crate::manual_abort_resend::service_runs() && bay.state.abort_resend.active_resends().is_some()
            && let Some(seen) = crate::manual_abort_resend::observe(&bay.state, bay.now.load(Ordering::Relaxed))
        {
            bay.state.abort_resend.step(&bay.rrls, seen);
        }
        if done(bay) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// PB's FLIGHT_STATE, as it holds it: (sender, value).
fn pb_flight_state(bay: &Bay) -> Option<(String, u8)> {
    bay.peers[2]
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap()
        .map(|p| (crate::types::canonical_sender_id(p.sender()).to_string(), p.payload()[0]))
}

const PB_BIT: u8 = 1 << 2;

/// The station's Aborted FLIGHT_STATE reached RF and FC; PB was out of reach
/// until it ran out of retries. Returns the router worker for the rest.
fn aborted_flight_state_exhausts_on_pb(bay: &Bay) -> crate::telemetry_task::RouterServiceForTest {
    mark_present(bay, &[Board::RFBoard, Board::FlightComputer, Board::PowerBoard]);
    press_abort_armed(bay);
    assert_eq!(bay.pending("FLIGHT_STATE").len(), 3, "the Aborted write is owed by RF, FC and PB");
    bay.release_to(&["RF", "FC"]);
    let mut worker = crate::telemetry_task::RouterServiceForTest::default();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let written = bay.rrls.own_network_variable(ty).unwrap().packet_id();
    // Done as PB's copy runs out of retries: once the worker has recorded it
    // (before the re-send service's next pass), or once it is no longer pending.
    assert!(run_services(bay, &mut worker, 20_000, |bay| !bay.pending("FLIGHT_STATE").contains(&hash("PB"))
            || crate::manual_abort_resend::flight_state_unacked_for_test() & PB_BIT != 0),
        "PB's copy runs out of retries");
    assert_eq!(bay.rrls.own_network_variable(ty).unwrap().packet_id(), written, "not re-sent yet");
    assert_eq!(pb_flight_state(bay), None, "PB never got it");
    worker
}

/// PB comes back in reach (nothing held is replayed to it).
fn pb_comes_back(bay: &Bay) {
    bay.open.lock().unwrap().push("PB");
}

/// 1. Production path, no direct call: the Aborted FLIGHT_STATE exhausts on
/// PB; when PB is back it receives the station's own Aborted value without
/// any operator action. Exhausting once never ends its delivery.
#[tokio::test]
async fn a_primary_keeps_delivering_its_aborted_flight_state_to_pb_after_it_exhausts() {
    if primary_child("a_primary_keeps_delivering_its_aborted_flight_state_to_pb_after_it_exhausts") {
        return;
    }
    let bay = bay(false).await;
    let mut worker = aborted_flight_state_exhausts_on_pb(&bay);
    pb_comes_back(&bay);
    assert!(run_services(&bay, &mut worker, 20_000, |bay| pb_flight_state(bay).is_some()), "PB is sent it again");
    assert_eq!(pb_flight_state(&bay), Some(("GS".to_string(), crate::types::NetworkFlightState::Aborted as u8)));
}

/// 4. The same for an RRLS in takeover.
#[tokio::test]
async fn a_takeover_rrls_keeps_delivering_its_aborted_flight_state_to_pb_after_it_exhausts() {
    if rrls_child("a_takeover_rrls_keeps_delivering_its_aborted_flight_state_to_pb_after_it_exhausts") {
        return;
    }
    let bay = bay(true).await;
    let mut worker = aborted_flight_state_exhausts_on_pb(&bay);
    pb_comes_back(&bay);
    assert!(run_services(&bay, &mut worker, 20_000, |bay| pb_flight_state(bay).is_some()), "PB is sent it again");
    assert_eq!(pb_flight_state(&bay), Some(("RRLS".to_string(), crate::types::NetworkFlightState::Aborted as u8)));
}

/// 2. Not after a Reset: the station has left Aborted.
#[tokio::test]
async fn no_aborted_flight_state_resend_after_a_reset() {
    if primary_child("no_aborted_flight_state_resend_after_a_reset") {
        return;
    }
    let bay = bay(false).await;
    let mut worker = aborted_flight_state_exhausts_on_pb(&bay);
    // Reset: the station leaves Aborted and writes the new state.
    bay.state.set_preexisting_flight_state_for_test(FlightState::Idle);
    crate::network_variables::set_flight_state(&bay.rrls, FlightState::Idle as u8).unwrap();
    pb_comes_back(&bay);
    run_services(&bay, &mut worker, 15_000, |_| false);
    let aborted = crate::types::NetworkFlightState::Aborted as u8;
    assert!(pb_flight_state(&bay).is_some_and(|(sender, value)| sender == "GS" && value != aborted),
        "PB gets the Reset's state, never the Aborted one again: {:?}", pb_flight_state(&bay));
}

/// 3. Not once PB's readback shows Aborted.
#[tokio::test]
async fn no_aborted_flight_state_resend_once_pb_reads_aborted() {
    if primary_child("no_aborted_flight_state_resend_once_pb_reads_aborted") {
        return;
    }
    let bay = bay(false).await;
    let mut worker = aborted_flight_state_exhausts_on_pb(&bay);
    bay.state.board_abort.observe_abort(Board::PowerBoard, std::time::Instant::now(), bay.now.load(Ordering::Relaxed));
    pb_comes_back(&bay);
    run_services(&bay, &mut worker, 15_000, |_| false);
    assert_eq!(pb_flight_state(&bay), None, "PB reads Aborted: not owed it again");
}

/// No FLIGHT_STATE but Aborted ever leaves an RRLS, even in a takeover: a
/// later non-Aborted write (a post-Abort Idle) would undo the Abort on the
/// boards. Its only write is its operator's Abort.
#[tokio::test]
async fn a_takeover_rrls_never_sends_a_non_aborted_flight_state() {
    if rrls_child("a_takeover_rrls_never_sends_a_non_aborted_flight_state") {
        return;
    }
    let bay = bay(true).await;
    press_abort(&bay);
    bay.held.lock().unwrap().clear();
    assert!(crate::network_variables::set_takeover_flight_state(&bay.rrls, FlightState::Idle as u8).is_err());
    bay.pump();
    assert_eq!(rrls_flight_states(&bay), vec![], "no Idle on the air");
    assert_eq!(bay.rrls.own_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).map(|p| p.payload()[0]),
        Some(crate::types::NetworkFlightState::Aborted as u8), "its own latest stays Aborted");
}

/// fullnet C-fix3 (+1 h RRLS clock): the RRLS stamped its HEARTBEAT and the
/// cosmetic outputs with its own host clock. The FC advances its station
/// clock from a station's heartbeat, so it then took station time as an hour
/// ahead and refused the RRLS's correctly stamped Deploy as stale. Every
/// station frame an RRLS originates is stamped like a control: the station
/// time it heard, never its host clock.
#[tokio::test]
async fn a_takeover_rrls_stamps_its_heartbeat_and_outputs_with_the_heard_station_clock() {
    if rrls_child("a_takeover_rrls_stamps_its_heartbeat_and_outputs_with_the_heard_station_clock") {
        return;
    }
    let bay = bay(true).await;
    // The station time it heard from the AV bay: an hour behind its host clock.
    let heard = crate::telemetry_task::get_current_timestamp_ms() - 3_600_000;
    crate::system_clock::observe_station_timestamp(heard);
    let stamps: Vec<(String, u64)> = vec![
        ("HEARTBEAT".into(), crate::telemetry_task::station_heartbeat(&bay.rrls).unwrap().timestamp()),
        ("FLIGHT_BUZZER".into(), crate::network_variables::network_output_packet(&bay.rrls, crate::network_variables::NetworkOutput::FlightBuzzer, true).unwrap().timestamp()),
    ];
    assert!(stamps.len() >= 2, "both sent: {stamps:?}");
    assert!(stamps.iter().all(|(_, t)| t.abs_diff(heard) < 60_000), "stamped with the heard station clock {heard}: {stamps:?}");
}

fn flight_state_of(bay: &Bay, board: usize) -> Option<(String, u8)> {
    bay.peers[board]
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap()
        .map(|p| (crate::types::canonical_sender_id(p.sender()).to_string(), p.payload()[0]))
}

/// fullnet C-fix3, the GS Abort at hand-back: the routes to RF and PB had
/// expired during the cut while FC was still reachable (over the umbilical),
/// so the Abort counted as routed; its Aborted FLIGHT_STATE went to no AV-bay
/// owner and was never sent again. Routing is judged per board: RF and PB,
/// unrouted when the Abort was sent, get the ABORT and the station's Aborted
/// FLIGHT_STATE once they are routed again, without operator action.
#[tokio::test]
async fn a_primary_abort_reaches_rf_and_pb_once_they_are_routed_again() {
    if primary_child("a_primary_abort_reaches_rf_and_pb_once_they_are_routed_again") {
        return;
    }
    let bay = bay(false).await;
    // RF and PB fall out of reach both ways; FC stays reachable and keeps announcing.
    bay.both_ways.store(true, Ordering::Relaxed);
    bay.hold.store(1, Ordering::Relaxed);
    bay.open.lock().unwrap().push("FC");
    for _ in 0..40 {
        bay.now.fetch_add(1_000, Ordering::Relaxed);
        bay.peers[1].announce_discovery().unwrap();
        let _ = bay.rrls.process_all_queues();
        for peer in &bay.peers {
            let _ = peer.process_all_queues();
        }
    }
    assert_eq!(crate::manual_abort_resend::routed_av_bay_boards(&bay.rrls), 1 << 1, "only FC is routed");
    mark_present(&bay, &[Board::RFBoard, Board::FlightComputer, Board::PowerBoard]);
    press_abort_armed(&bay);
    let mut worker = crate::telemetry_task::RouterServiceForTest::default();
    run_services(&bay, &mut worker, 3_000, |_| false);
    assert!(!bay.abort_rx.lock().unwrap().iter().any(|b| *b == "RF" || *b == "PB"), "RF and PB are out of reach");
    assert_eq!(flight_state_of(&bay, 0), None);
    assert_eq!(flight_state_of(&bay, 2), None);
    bay.abort_rx.lock().unwrap().clear();
    // RF and PB come back and are rediscovered.
    bay.open.lock().unwrap().extend(["RF", "PB"]);
    bay.peers[0].announce_discovery().unwrap();
    bay.peers[2].announce_discovery().unwrap();
    let aborted = ("GS".to_string(), crate::types::NetworkFlightState::Aborted as u8);
    assert!(run_services(&bay, &mut worker, 20_000, |bay| {
        let got = bay.abort_rx.lock().unwrap().clone();
        got.contains(&"RF") && got.contains(&"PB") && flight_state_of(bay, 0) == Some(aborted.clone()) && flight_state_of(bay, 2) == Some(aborted.clone())
    }), "RF and PB get the ABORT and (GS, 15): abort_rx={:?} rf={:?} pb={:?}", bay.abort_rx.lock().unwrap(), flight_state_of(&bay, 0), flight_state_of(&bay, 2));
}

/// With segment proxies acknowledging on acceptance (SEDSnet proxy-ACK), the
/// station's Aborted FLIGHT_STATE completes once RF has accepted it; RF owns
/// the delivery to PB behind it. If that runs out, the station sees no
/// exhaustion: PB still not reading Aborted is the trigger. Station - radio -
/// RF (segment proxy) - CAN - PB, with the CAN link to PB down until RF's own
/// delivery has given up; PB then receives the station's Aborted value from a
/// station re-send, without operator action.
async fn a_proxied_pb_gets_the_aborted_flight_state_from_a_readback_resend(station: &'static str) {
    crate::telemetry_schema::initialize().unwrap();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let now = Arc::new(AtomicU64::new(1_000));
    let clk = |n: &Arc<AtomicU64>| { let n = n.clone(); Box::new(move || n.load(Ordering::Relaxed)) as Box<dyn sedsnet::router::Clock + Send + Sync> };
    let state = crate::state::tests::test_app_state().await;
    let host = Arc::new(Router::new_with_clock(RouterConfig::new([]).with_sender(station), clk(&now)));
    state.topology_router.set(host.clone()).unwrap();
    crate::network_variables::initialize(&host).unwrap();
    // The ABORT's owner on the radio segment (FC's role on the real bay).
    let rf = Arc::new(Router::new_with_clock(RouterConfig::new([EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("ABORT"), |_| Ok(()))]).with_sender("RF"), clk(&now)));
    let pb = Arc::new(Router::new_with_clock(RouterConfig::new([]).with_sender("PB"), clk(&now)));
    for board in [&rf, &pb] {
        board.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY).unwrap();
        board.on_network_variable_update(ty, |_| Ok(())).unwrap();
    }
    let can_up = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let to_rf = rf.clone();
    host.add_side_packed("rocket_comms", move |b| { let _ = to_rf.rx_packed_from_side(b, 0); Ok(()) });
    let to_host = host.clone();
    rf.add_side_packed("radio", move |b| { let _ = to_host.rx_packed_from_side(b, 0); Ok(()) });
    let (to_pb, up) = (pb.clone(), can_up.clone());
    rf.add_side_packed("can", move |b| { if up.load(Ordering::Relaxed) { let _ = to_pb.rx_packed_from_side(b, 0); } Ok(()) });
    let (to_rf, up) = (rf.clone(), can_up.clone());
    pb.add_side_packed("can", move |b| { if up.load(Ordering::Relaxed) { let _ = to_rf.rx_packed_from_side(b, 1); } Ok(()) });
    let pump = || for _ in 0..10 { let _ = host.process_all_queues(); let _ = rf.process_all_queues(); let _ = pb.process_all_queues(); };
    for r in [&host, &rf, &pb] { r.announce_discovery().unwrap(); }
    pump();
    if crate::rrls::is_rrls() {
        crate::rrls::set_takeover_for_test(true);
        crate::network_variables::set_takeover_outputs_writable(&host, true).unwrap();
    } else {
        host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_945_600_000)).unwrap();
    }
    state.set_preexisting_flight_state_for_test(FlightState::Idle);
    {
        let mut status = state.board_status.lock().unwrap();
        for board in [Board::RFBoard, Board::PowerBoard, Board::FlightComputer] {
            status.get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
    }
    // RF and FC read back Aborted; PB, behind RF, is the one still owed.
    let pb_value = || pb.get_cached_network_variable(ty).unwrap().map(|p| (crate::types::canonical_sender_id(p.sender()).to_string(), p.payload()[0]));
    can_up.store(false, Ordering::Relaxed);
    state.manual_abort_intent.request();
    let mut claim = state.manual_abort_intent.try_claim().unwrap();
    let mark = crate::manual_abort_resend::send_mark(&host);
    assert!(publish_local_claim(&state, &host, &mut claim).unwrap());
    state.abort_resend.arm(claim.epoch(), claim.wire().unwrap(), crate::board_recovery::coordinator::recovery_activity(&state).flatten(), mark);
    claim.accepted();
    for board in [Board::RFBoard, Board::FlightComputer] {
        state.board_abort.observe_abort(board, std::time::Instant::now(), now.load(Ordering::Relaxed));
    }
    let mut worker = crate::telemetry_task::RouterServiceForTest::default();
    let mut run = |ms: u64, done: &dyn Fn() -> bool| -> bool {
        for _ in 0..ms / 200 {
            now.fetch_add(200, Ordering::Relaxed);
            worker.pump(&host);
            let _ = rf.process_all_queues();
            let _ = pb.process_all_queues();
            worker.pump(&host);
            if let Some(seen) = crate::manual_abort_resend::observe(&state, now.load(Ordering::Relaxed)) {
                state.abort_resend.step(&host, seen);
            }
            if done() { return true; }
        }
        false
    };
    // With proxy-ACK, RF accepted it: nothing is pending at the station and RF
    // owns PB's copy (without it, the station's write waits for PB through RF
    // and the exhaustion path re-sends it instead; both must reach PB).
    run(1_000, &|| false);
    // RF's own delivery to PB runs out while the CAN link is down.
    run(8_000, &|| false);
    assert_eq!(pb_value(), None, "PB never got it");
    can_up.store(true, Ordering::Relaxed);
    let aborted = (station.to_string(), crate::types::NetworkFlightState::Aborted as u8);
    assert!(run(20_000, &|| pb_value() == Some(aborted.clone())), "PB gets ({station}, 15) from a station re-send: {:?}", pb_value());
}

#[tokio::test]
async fn a_primary_resends_its_aborted_flight_state_to_a_proxied_pb_that_does_not_read_aborted() {
    if primary_child("a_primary_resends_its_aborted_flight_state_to_a_proxied_pb_that_does_not_read_aborted") {
        return;
    }
    a_proxied_pb_gets_the_aborted_flight_state_from_a_readback_resend("GS").await;
}

#[tokio::test]
async fn a_takeover_rrls_resends_its_aborted_flight_state_to_a_proxied_pb_that_does_not_read_aborted() {
    if rrls_child("a_takeover_rrls_resends_its_aborted_flight_state_to_a_proxied_pb_that_does_not_read_aborted") {
        return;
    }
    a_proxied_pb_gets_the_aborted_flight_state_from_a_readback_resend("RRLS").await;
}
