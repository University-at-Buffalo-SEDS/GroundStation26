//! Automatic aborts on a disk that has stopped answering, and the order of
//! their records. A stall ends only when the test ends it (or after 30 s), so
//! an ABORT that arrives before that waited for no disk. `WAIT` only bounds a
//! failing run.
use super::*;
use crate::disk_stall::{Site, Stall};
use sedsnet::router::{EndpointHandler, Router, RouterConfig, RouterSideOptions};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(15);

fn elsewhere(name: &str) -> bool {
    crate::operator_queue::isolated_native_case(module_path!(), name)
}

/// The station with every board seen, armed, and one transport.
struct Rig {
    state: Arc<AppState>,
    router: Arc<Router>,
    path: std::path::PathBuf,
    /// Every frame as the transport gets it.
    wire: Receiver<(String, Vec<u8>)>,
    /// Every controller command and ABORT, in the order the station sent them
    /// (its own audit copies; the transport gets a command only once the one
    /// before it is acknowledged).
    sent: Arc<Mutex<Vec<(String, u8)>>>,
}
impl Rig {
    async fn new() -> Self {
        let state = crate::state::tests::test_app_state().await;
        let observed = state.clone();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let audit = |observe: Option<Arc<AppState>>| {
            let (sent, waits) = (sent.clone(), state.clone());
            move |packet: &sedsnet::packet::Packet| {
                let kind = packet.data_type().as_str().to_string();
                if kind == "BATTERY_VOLTAGE" {
                    // What many handlers of received packets do.
                    drop(waits.gse.lock());
                }
                // An ABORT comes to both endpoints.
                if (kind == "ABORT") == observe.is_some() {
                    sent.lock().unwrap().push((kind, packet.payload()[0] & 0x7f));
                }
                if let Some(state) = &observe {
                    crate::deferred_abort::observe(state, packet, automatic_generation(state));
                }
                Ok(())
            }
        };
        let router = Arc::new(Router::new(
            RouterConfig::new([
                EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("ABORT"), audit(Some(observed))),
                EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("GROUND_STATION"), audit(None)),
            ])
            .with_sender("GS"),
        ));
        let (frames, wire) = channel();
        let side = router.add_side_packed_with_options(
            "wire",
            move |bytes| {
                if let Ok(packet) = sedsnet::wire_format::unpack_packet(bytes) {
                    let _ = frames.send((packet.data_type().as_str().to_string(), packet.payload().to_vec()));
                }
                Ok(())
            },
            RouterSideOptions { reliable_enabled: false, ..Default::default() },
        );
        let announce = sedsnet::discovery::build_discovery_announce(
            "WIRE",
            123,
            &["ABORT", "VALVE_BOARD", "ACTUATOR_BOARD"].map(crate::telemetry_schema::endpoint),
        )
        .unwrap();
        router.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), side).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        let path = armed_fixture_for_test(&state, &router);
        operational::persist_diagnostic::take_saved(&path);
        Self { state, router, path, wire, sent }
    }
    /// Wait for the next ABORT at the transport.
    fn abort(&self) -> Result<(), String> {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.wire.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((kind, payload)) if kind == "ABORT" => {
                    assert!(payload.starts_with(b"SEDS_AUTO_ABORT_V1:"), "an automatic ABORT");
                    return Ok(());
                }
                Ok(_) => {}
                Err(_) => return Err(format!("no ABORT left while the disk was stalled; sent: {:?}", self.sent.lock().unwrap())),
            }
        }
    }
    /// Every automatic ABORT the station sent.
    fn aborts_so_far(&self) -> usize {
        self.router.process_all_queues_with_timeout(0).unwrap();
        self.sent.lock().unwrap().iter().filter(|(kind, _)| kind == "ABORT").count()
    }
    fn saved(&self) -> Vec<(bool, bool)> {
        operational::persist_diagnostic::take_saved(&self.path)
    }
    fn on_disk(&self) -> bool {
        operational::Saved::load(&self.path).unwrap().automatic_abort_claimed
    }
    fn offline(&self, board: crate::types::Board) {
        let now = crate::telemetry_task::get_current_timestamp_ms();
        let mut boards = self.state.board_status.lock().unwrap();
        for (other, status) in boards.iter_mut() {
            let age = if *other == board { 13_000 } else { 0 };
            status.last_seen_ms = Some(now - age);
            status.last_seen_model_ms = None;
            status.last_seen_instant = Instant::now().checked_sub(Duration::from_millis(age));
        }
    }
    /// Another thread saves the qualification, holding the dispatch gate and
    /// the GSE lock, and is stopped by the disk. Its snapshot is older than
    /// any later claim.
    fn saving_elsewhere(&self, stall: &Stall) -> std::thread::JoinHandle<()> {
        let state = self.state.clone();
        let saving = std::thread::spawn(move || {
            with_command_gate(|| state.gse.lock().unwrap().operational.persist().unwrap());
        });
        assert!(stall.entered(), "the other save is stopped by the disk");
        saving
    }
    fn fire(&self) -> std::thread::JoinHandle<Result<(), String>> {
        let state = self.state.clone();
        crate::runtime_mode::spawn_thread("fire", move || commit_fire_intent(&state).map(drop)).unwrap()
    }
    /// What the shared path of every producer answers now.
    fn claim(&self) -> Decision {
        decide_automatic_abort(&self.state, automatic_generation(&self.state))
    }
    /// One producer's whole dispatch: claim, send, drop.
    fn send(&self) {
        let Decision::Claimed(claim) = self.claim() else { panic!("eligible") };
        send_claimed(
            &self.state,
            &self.router,
            &claim,
            "test",
            crate::abort_retirement::AbortCause::Unknown("test".into()),
        )
        .unwrap();
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---- Every producer, with the dispatch gate and the GSE lock held by a save ----

/// Sending runs no received packet's handler: those wait for the GSE lock.
#[tokio::test]
async fn sending_an_abort_runs_no_received_packet() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("sending_an_abort_runs_no_received_packet") {
        return;
    }
    let rig = Rig::new().await;
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    rig.router
        .rx_queue_from_side(
            sedsnet::packet::Packet::new(
                crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "PB",
                crate::telemetry_task::get_current_timestamp_ms(),
                Arc::from(&12.0_f32.to_le_bytes()[..]),
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let (done, sent) = channel();
    let (state, router) = (rig.state.clone(), rig.router.clone());
    let sending = crate::runtime_mode::spawn_thread("send", move || {
        let Decision::Claimed(claim) = decide_automatic_abort(&state, automatic_generation(&state)) else {
            panic!("eligible")
        };
        let cause = crate::abort_retirement::AbortCause::Unknown("test".into());
        send_claimed(&state, &router, &claim, "test", cause).unwrap();
        drop(claim);
        done.send(()).unwrap();
    })
    .unwrap();
    let left = rig.abort();
    let returned = sent.recv_timeout(WAIT).is_ok();
    let stalled = !saving.is_finished();
    drop(stall);
    saving.join().unwrap();
    sending.join().unwrap();
    left.unwrap();
    assert!(returned && stalled);
    rig.state.gse.abort.settle();
}

/// The safety task's disconnect abort.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_abort_leaves_while_another_save_is_stalled() {
    crate::mode_suite!(Normal, Testing);
    if elsewhere("a_disconnect_abort_leaves_while_another_save_is_stalled") {
        return;
    }
    let rig = Rig::new().await;
    rig.state.set_local_flight_state(FlightState::PreFill);
    rig.offline(crate::types::Board::ActuatorBoard);
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    let (_stop, shutdown) = tokio::sync::broadcast::channel(1);
    let task = tokio::spawn(crate::safety_task::safety_task(rig.state.clone(), rig.router.clone(), shutdown));
    let sent = tokio::task::block_in_place(|| rig.abort());
    let stalled = !saving.is_finished();
    let claimed_again = matches!(rig.claim(), Decision::Claimed(_));
    let in_flight = rig.state.gse.abort.in_flight();
    let latched = rig.state.abort_indicator_latched();
    let launch_refused = rig.state.gse.abort.admit_launch(true).is_none();
    let recorded_early = rig.saved();
    drop(stall);
    saving.join().unwrap();
    sent.unwrap();
    assert!(stalled, "the ABORT left before the disk answered");
    assert!(latched && in_flight && launch_refused && !claimed_again, "latched in RAM, sent once");
    assert!(recorded_early.is_empty(), "nothing is on disk yet: {recorded_early:?}");
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    rig.state.gse.abort.settle();
    // The other save was older than the claim, and the claim's is last.
    assert_eq!(rig.saved(), [(false, false), (true, false)]);
    assert!(rig.on_disk());
    assert_eq!(rig.aborts_so_far(), 1, "one ABORT");
    assert_eq!(rig.state.gse.lock().unwrap().engine.status.phase, Phase::Fault);
}

/// The same with the disk stopping the abort's own record, and the reporting
/// loop held up too (the log, the layout file, a policy refresh).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_abort_leaves_while_its_own_record_is_stalled() {
    crate::mode_suite!(Normal, Testing);
    if elsewhere("a_disconnect_abort_leaves_while_its_own_record_is_stalled") {
        return;
    }
    let rig = Rig::new().await;
    rig.offline(crate::types::Board::ActuatorBoard);
    let stall = Stall::begin(Site::Qualification);
    let (_stop, shutdown) = tokio::sync::broadcast::channel(1);
    let task = tokio::spawn(crate::safety_task::safety_task(rig.state.clone(), rig.router.clone(), shutdown));
    let sent = tokio::task::block_in_place(|| rig.abort());
    let recording = tokio::task::block_in_place(|| stall.entered());
    let in_flight = rig.state.gse.abort.in_flight();
    // Fire is refused while the claim is only in RAM: it waits for the record
    // that holds the GSE lock, and then for nothing.
    let fire = rig.fire();
    drop(stall);
    sent.unwrap();
    assert!(recording && in_flight, "the record was still being written");
    assert!(fire.join().unwrap().is_err());
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    rig.state.gse.abort.settle();
    assert_eq!(rig.saved(), [(true, false)], "recorded, and no Fire was");
}

/// A safety loop whose reporting is stopped before the abort is selected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_abort_does_not_wait_for_the_safety_loops_reports() {
    crate::mode_suite!(Normal, Testing);
    if elsewhere("a_disconnect_abort_does_not_wait_for_the_safety_loops_reports") {
        return;
    }
    let rig = Rig::new().await;
    rig.offline(crate::types::Board::GroundStation);
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    let mut reports = rig.state.board_status_tx.subscribe();
    let (_stop, shutdown) = tokio::sync::broadcast::channel(1);
    let task = tokio::spawn(crate::safety_task::safety_task(rig.state.clone(), rig.router.clone(), shutdown));
    // The first report waits for the dispatch gate. The watch goes on: a
    // board that drops off afterwards is found and aborted on.
    tokio::task::block_in_place(|| {
        let deadline = Instant::now() + WAIT;
        while rig.state.gse.abort_watch_cycles_for_test() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    rig.offline(crate::types::Board::ActuatorBoard);
    let sent = tokio::task::block_in_place(|| rig.abort());
    let reported = reports.try_recv().is_ok();
    drop(stall);
    saving.join().unwrap();
    sent.unwrap();
    assert!(!reported, "the reporting was still waiting");
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    rig.state.gse.abort.settle();
    assert!(rig.on_disk());
}

fn tank_pressure(psi: f32) -> sedsnet::packet::Packet {
    sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("OX_TANK_PRESSURE"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "VB",
        crate::telemetry_task::get_current_timestamp_ms(),
        Arc::from(psi.to_le_bytes().to_vec().into_boxed_slice()),
    )
    .unwrap()
}

/// Overpressure: detection, relief, the main-valve window and the Abort, from
/// readings the telemetry worker never gets to.
async fn overpressure_abort(site: Site) {
    let rig = Rig::new().await;
    rig.state.gse.lock().unwrap().engine.config.overpressure_pilot_ms = 500;
    overpressure::watch::spawn(rig.state.clone()).unwrap();
    let stall = Stall::begin(site);
    let saving = (site == Site::Qualification).then(|| rig.saving_elsewhere(&stall));
    let feeding = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let feeder = {
        let (state, feeding) = (rig.state.clone(), feeding.clone());
        std::thread::spawn(move || {
            while feeding.load(std::sync::atomic::Ordering::Acquire) {
                overpressure::watch::receive(&state, &tank_pressure(960.0));
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let sent = tokio::task::block_in_place(|| rig.abort());
    feeding.store(false, std::sync::atomic::Ordering::Release);
    feeder.join().unwrap();
    let stalled = saving.as_ref().is_none_or(|saving| !saving.is_finished());
    let latched = rig.state.abort_indicator_latched();
    let launch_refused = rig.state.gse.abort.admit_launch(true).is_none();
    drop(stall);
    if let Some(saving) = saving {
        saving.join().unwrap();
    }
    sent.unwrap();
    assert!(stalled && latched && launch_refused);
    let before = rig.sent.lock().unwrap().clone();
    let at = |kind: &str, byte: u8| before.iter().position(|sent| sent.0 == kind && sent.1 == byte);
    let order = [
        at("ACTUATOR_COMMAND", 13), // Nitrous close
        at("ACTUATOR_COMMAND", 12), // Nitrogen close
        at("VALVE_COMMAND", 2),     // Dump open
        at("VALVE_COMMAND", 1),     // Vent open
        at("VALVE_COMMAND", 0),     // Pilot open
        at("VALVE_COMMAND", 3),     // Pilot close
        before.iter().position(|sent| sent.0 == "ABORT"),
    ];
    assert!(order.iter().all(Option::is_some) && order.is_sorted(), "{order:?} in {before:?}");
    assert!(at("ACTUATOR_COMMAND", 16).is_some(), "Actuator Board abort: {before:?}");
    rig.state.gse.abort.settle();
    assert!(rig.on_disk());
    assert_eq!(rig.aborts_so_far(), 1, "one ABORT");
    // The runtime is told afterwards.
    let deadline = Instant::now() + WAIT;
    while rig.state.gse.lock().unwrap().engine.status.phase != Phase::Fault && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(rig.state.gse.lock().unwrap().engine.status.phase, Phase::Fault);
    rig.state.request_shutdown();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_overpressure_abort_leaves_while_another_save_is_stalled() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("an_overpressure_abort_leaves_while_another_save_is_stalled") {
        return;
    }
    overpressure_abort(Site::Qualification).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_leaves_while_its_own_record_is_stalled() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("an_abort_leaves_while_its_own_record_is_stalled") {
        return;
    }
    let rig = Rig::new().await;
    let stall = Stall::begin(Site::Qualification);
    rig.send();
    let sent = rig.abort();
    let recording = stall.entered();
    drop(stall);
    sent.unwrap();
    assert!(recording);
    rig.state.gse.abort.settle();
    assert!(rig.on_disk());
}
/// HITL raises it too (and records none).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hitl_overpressure_abort_leaves_while_the_runtime_is_held() {
    crate::mode_suite!(Hitl);
    if elsewhere("a_hitl_overpressure_abort_leaves_while_the_runtime_is_held") {
        return;
    }
    let rig = Rig::new().await;
    rig.state.gse.lock().unwrap().engine.config.overpressure_pilot_ms = 500;
    overpressure::watch::spawn(rig.state.clone()).unwrap();
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    let sent = tokio::task::block_in_place(|| {
        let deadline = Instant::now() + WAIT;
        loop {
            overpressure::watch::receive(&rig.state, &tank_pressure(960.0));
            match rig.wire.recv_timeout(Duration::from_millis(20)) {
                Ok((kind, _)) if kind == "ABORT" => break true,
                _ if Instant::now() > deadline => break false,
                _ => {}
            }
        }
    });
    let stalled = !saving.is_finished();
    drop(stall);
    saving.join().unwrap();
    assert!(sent && stalled);
    assert!(!rig.state.gse.lock().unwrap().operational.saved.automatic_abort_claimed);
    rig.state.request_shutdown();
}

/// An AutomaticAbort intent in the command queue (nothing sends one today).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_automatic_abort_leaves_while_another_save_is_stalled() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_queued_automatic_abort_leaves_while_another_save_is_stalled") {
        return;
    }
    let mut rig = Rig::new().await;
    let (commands, receiver) = tokio::sync::mpsc::channel(4);
    let (_db_tx, db_rx) = tokio::sync::mpsc::channel(8);
    commands
        .try_send(TelemetryCommand::AutomaticAbort { generation: automatic_generation(&rig.state) })
        .unwrap();
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    let worker = tokio::spawn(crate::telemetry_task::telemetry_task(
        rig.state.clone(),
        rig.router.clone(),
        vec![],
        receiver,
        db_rx,
        rig.state.shutdown_subscribe(),
    ));
    let sent = tokio::task::block_in_place(|| rig.abort());
    let stalled = !saving.is_finished();
    drop(stall);
    saving.join().unwrap();
    rig.state.request_shutdown();
    tokio::time::timeout(WAIT, worker).await.unwrap().unwrap();
    sent.unwrap();
    assert!(stalled);
    rig.state.gse.abort.settle();
    assert_eq!(rig.saved().last(), Some(&(true, false)));
    rig.wire = channel().1;
}

/// A board's own automatic ABORT latches the station.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_boards_automatic_abort_latches_while_another_save_is_stalled() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_boards_automatic_abort_latches_while_another_save_is_stalled") {
        return;
    }
    let rig = Rig::new().await;
    let (_observer, _drain) = crate::deferred_abort::spawn(rig.state.clone());
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    crate::deferred_abort::observe(&rig.state, &board_abort("VB"), automatic_generation(&rig.state));
    let latched = until(|| rig.state.abort_indicator_latched()).await;
    let stalled = !saving.is_finished();
    drop(stall);
    saving.join().unwrap();
    assert!(latched && stalled);
}
fn board_abort(sender: &str) -> sedsnet::packet::Packet {
    sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("ABORT"),
        &[crate::telemetry_schema::endpoint("ABORT")],
        sender,
        crate::telemetry_task::get_current_timestamp_ms(),
        Arc::from(&b"SEDS_AUTO_ABORT_V1:board"[..]),
    )
    .unwrap()
}
async fn until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    true
}

// ---- The record ----

#[tokio::test]
async fn a_failed_record_latches_after_the_abort_is_sent() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_failed_record_latches_after_the_abort_is_sent") {
        return;
    }
    let rig = Rig::new().await;
    // A directory: the save fails at its rename, after the stalled sync.
    let blocked = std::env::temp_dir().join(format!("gse-abort-record-{}", rand::random::<u128>()));
    std::fs::create_dir(&blocked).unwrap();
    rig.state.gse.lock().unwrap().operational.path = Some(blocked.clone());
    let stall = Stall::begin(Site::Qualification);
    rig.send();
    let sent = rig.abort();
    let recording = stall.entered();
    drop(stall);
    sent.unwrap();
    assert!(recording);
    rig.state.gse.abort.settle();
    {
        let rt = rig.state.gse.lock().unwrap();
        assert!(rt.operational.saved.automatic_abort_claimed);
        assert!(rt.operational.persist_error.is_some());
        assert_eq!(rt.operational.saved.qualification.fault(), Some(Fault::MissingTelemetry));
    }
    assert!(commit_fire_intent(&rig.state).is_err());
    assert!(clear_ground_abort(&rig.state).is_err());
    assert!(matches!(rig.claim(), Decision::Unavailable), "still latched, sent once");
    std::fs::remove_dir(&blocked).unwrap();
}

/// A save whose snapshot is older than the claim, and one begun after it by
/// someone else: the last record has the claim either way.
#[tokio::test]
async fn no_older_record_overwrites_a_claim() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("no_older_record_overwrites_a_claim") {
        return;
    }
    let rig = Rig::new().await;
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    rig.send();
    // Queued behind the stalled save, like the claim's record: whichever of
    // the two gets the lock first, both write the claim.
    let state = rig.state.clone();
    let later = std::thread::spawn(move || state.gse.lock().unwrap().operational.persist().unwrap());
    drop(stall);
    saving.join().unwrap();
    later.join().unwrap();
    rig.state.gse.abort.settle();
    assert_eq!(rig.saved(), [(false, false), (true, false), (true, false)]);
    assert!(rig.on_disk());
}

#[tokio::test]
async fn a_clear_is_written_after_the_claim_it_clears() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_clear_is_written_after_the_claim_it_clears") {
        return;
    }
    let rig = Rig::new().await;
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    rig.send();
    let refused = automatic_abort_clear_refused(&rig.state);
    let state = rig.state.clone();
    let clear = crate::runtime_mode::spawn_thread("clear", move || clear_ground_abort(&state)).unwrap();
    drop(stall);
    saving.join().unwrap();
    assert!(refused, "no Clear is reserved before the record is written");
    let first = clear.join().unwrap();
    rig.state.gse.abort.settle();
    undeliverable(&rig.router);
    if first.is_err() {
        assert!(rig.on_disk(), "a refused Clear cleared nothing");
        clear_ground_abort(&rig.state).unwrap();
    }
    let saved = rig.saved();
    let claim = saved.iter().position(|record| record.0).expect("the claim was recorded");
    assert!(!saved[0].0 && !saved.last().unwrap().0, "{saved:?}");
    let clear = saved.iter().rposition(|record| record.0).unwrap();
    assert!(claim <= clear && saved[clear + 1..].iter().all(|record| !record.0), "{saved:?}");
    assert!(!rig.on_disk());
    // The next abort is a new claim, recorded after the Clear.
    rig.send();
    rig.state.gse.abort.settle();
    assert_eq!(rig.saved(), [(true, false)]);
}
fn automatic_abort_clear_refused(state: &AppState) -> bool {
    reserve_abort_clear(state).is_none()
}
/// The ABORT's delivery no longer holds a Clear (its window is over).
fn undeliverable(router: &Router) {
    commands_still_in_delivery(router, &[crate::telemetry_schema::data_type("ABORT")]);
    age_delivery_tracking_for_test(crate::sim_freshness::wall(undeliverable_after()) * 2);
}

/// A Clear being saved: no abort, and the claim stays until it is on disk.
#[tokio::test]
async fn an_abort_during_a_stalled_clear_is_refused_and_the_next_is_not() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("an_abort_during_a_stalled_clear_is_refused_and_the_next_is_not") {
        return;
    }
    let rig = Rig::new().await;
    rig.send();
    rig.state.gse.abort.settle();
    undeliverable(&rig.router);
    rig.saved();
    let stall = Stall::begin(Site::Qualification);
    let state = rig.state.clone();
    let clear = crate::runtime_mode::spawn_thread("clear", move || clear_ground_abort(&state)).unwrap();
    let clearing = stall.entered();
    let refused = matches!(rig.claim(), Decision::Unavailable);
    let launch_refused = rig.state.gse.abort.admit_launch(true).is_none();
    drop(stall);
    clear.join().unwrap().unwrap();
    assert!(clearing && refused && launch_refused);
    let saved = rig.saved();
    assert!(!saved.last().unwrap().0, "{saved:?}");
    rig.state.set_abort_indicator_latched(false);
    rig.send();
    rig.state.gse.abort.settle();
    assert!(rig.on_disk());
}

/// A power cut between the ABORT and its record: the station restores as it
/// was last saved, without the claim.
#[tokio::test]
async fn a_record_that_was_never_written_restores_the_earlier_one() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_record_that_was_never_written_restores_the_earlier_one") {
        return;
    }
    let rig = Rig::new().await;
    let stall = Stall::begin(Site::Qualification);
    rig.send();
    let recording = stall.entered();
    let restored = operational::Saved::load(&rig.path);
    drop(stall);
    assert!(recording);
    let restored = restored.unwrap();
    assert!(!restored.automatic_abort_claimed);
    assert_eq!(restored.qualification.stage(), gse_sequence::qualification::MissionStage::Armed);
    rig.state.gse.abort.settle();
}

// ---- Against a Launch ----

#[tokio::test]
async fn a_claim_before_fire_refuses_it_with_the_record_stalled() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_claim_before_fire_refuses_it_with_the_record_stalled") {
        return;
    }
    let rig = Rig::new().await;
    let stall = Stall::begin(Site::Qualification);
    let saving = rig.saving_elsewhere(&stall);
    rig.send();
    let sent = rig.abort();
    let fire = rig.fire();
    drop(stall);
    saving.join().unwrap();
    sent.unwrap();
    assert!(fire.join().unwrap().is_err());
    rig.state.gse.abort.settle();
    assert!(rig.saved().iter().all(|record| !record.1), "Fire was never written");
    assert!(!rig.state.launch_history.lock().unwrap().automatic_actions_inhibited());
}

/// Fire admitted and being saved. `outcome`: how its save ends.
async fn fire_first(site: Site, fails: bool) {
    let rig = Rig::new().await;
    let root = std::env::temp_dir().join(format!("gse-fire-first-{}", rand::random::<u128>()));
    std::fs::create_dir(&root).unwrap();
    if fails {
        rig.state.gse.lock().unwrap().operational.path = Some(root.clone());
    }
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    *rig.state.launch_history.lock().unwrap() =
        crate::launch_history::Store::open(root.join("launch-clock.json"), false, None, now, 0).unwrap();
    rig.offline(crate::types::Board::ActuatorBoard);
    // Proofs are fresh for a moment only.
    qualified_launch_proofs_for_test(&rig.state);
    let stall = Stall::begin(site);
    let fire = rig.fire();
    let saving = stall.entered();
    let undecided = matches!(rig.claim(), Decision::LaunchSaving)
        && matches!(decide_liveness_abort(&rig.state, crate::runtime_mode::token()), Decision::LaunchSaving)
        && !automatic_abort_allowed(&rig.state)
        && accept_observed_abort(&rig.state, true, automatic_generation(&rig.state)).is_none();
    // Overpressure judges nothing meanwhile.
    let pending = overpressure_undecided(&rig.state);
    let latched = rig.state.abort_indicator_latched();
    let stalled = !fire.is_finished();
    drop(stall);
    let fire = fire.join().unwrap();
    assert!(saving && stalled, "Fire is being saved: {fire:?}");
    assert!(undecided && pending && !latched, "an admitted Fire is not overtaken");
    assert_eq!(rig.aborts_so_far(), 0);
    if fails {
        fire.unwrap_err();
        // It was not a Launch: the abort is decided again, and goes.
        rig.send();
        assert!(rig.abort().is_ok());
        rig.state.gse.abort.settle();
    } else {
        fire.unwrap();
        assert!(matches!(rig.claim(), Decision::Unavailable));
        assert!(matches!(
            decide_liveness_abort(&rig.state, crate::runtime_mode::token()),
            Decision::Unavailable
        ));
        assert_eq!(accept_observed_abort(&rig.state, true, automatic_generation(&rig.state)), Some(false));
        assert!(!rig.state.abort_indicator_latched());
        assert_eq!(rig.aborts_so_far(), 0, "no automatic abort after a committed Fire");
        assert!(rig.saved().iter().all(|record| !record.0));
    }
    std::fs::remove_dir_all(&root).unwrap();
}
fn overpressure_undecided(state: &Arc<AppState>) -> bool {
    // Evaluating settles nothing (and takes no lock Fire holds).
    overpressure::evaluate(state);
    state.gse.overpressure.lock().latch.is_none()
}
#[tokio::test]
async fn fire_being_saved_wins_over_an_abort() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("fire_being_saved_wins_over_an_abort") {
        return;
    }
    fire_first(Site::Qualification, false).await;
}
#[tokio::test]
async fn fire_saving_the_launch_journal_wins_over_an_abort() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("fire_saving_the_launch_journal_wins_over_an_abort") {
        return;
    }
    fire_first(Site::Journal, false).await;
}
#[tokio::test]
async fn an_abort_goes_once_the_fire_it_waited_for_was_not_saved() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("an_abort_goes_once_the_fire_it_waited_for_was_not_saved") {
        return;
    }
    fire_first(Site::Qualification, true).await;
}
/// The safety task itself, with a board offline since before Fire: it asks
/// during the save and after it, and sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_safety_task_sends_nothing_around_a_committed_fire() {
    crate::mode_suite!(Normal, Testing);
    if elsewhere("the_safety_task_sends_nothing_around_a_committed_fire") {
        return;
    }
    let rig = Rig::new().await;
    qualified_launch_proofs_for_test(&rig.state);
    let stall = Stall::begin(Site::Qualification);
    let fire = rig.fire();
    let saving = tokio::task::block_in_place(|| stall.entered());
    rig.offline(crate::types::Board::ActuatorBoard);
    let (stop, shutdown) = tokio::sync::broadcast::channel(1);
    let task = tokio::spawn(crate::safety_task::safety_task(rig.state.clone(), rig.router.clone(), shutdown));
    let asked = |more: usize| {
        let target = rig.state.gse.abort_watch_cycles_for_test() + more;
        tokio::task::block_in_place(|| {
            let deadline = Instant::now() + WAIT;
            while rig.state.gse.abort_watch_cycles_for_test() < target && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
    };
    asked(2);
    drop(stall);
    fire.join().unwrap().unwrap();
    asked(2);
    let _ = stop.send(());
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(saving);
    assert_eq!(rig.aborts_so_far(), 0);
    assert!(!rig.state.abort_indicator_latched());
}
/// A HITL Launch saves the journal only.
#[tokio::test]
async fn a_hitl_launch_being_saved_wins_over_an_abort() {
    crate::mode_suite!(Hitl);
    if elsewhere("a_hitl_launch_being_saved_wins_over_an_abort") {
        return;
    }
    let rig = Rig::new().await;
    let root = std::env::temp_dir().join(format!("gse-hitl-first-{}", rand::random::<u128>()));
    std::fs::create_dir(&root).unwrap();
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    *rig.state.launch_history.lock().unwrap() =
        crate::launch_history::Store::open(root.join("launch-clock.json"), false, None, now, 0).unwrap();
    qualified_launch_proofs_for_test(&rig.state);
    let stall = Stall::begin(Site::Journal);
    let state = rig.state.clone();
    let launch = crate::runtime_mode::spawn_thread("launch", move || {
        inhibit_automatic_actions_for_launch(&state).map(drop)
    })
    .unwrap();
    let saving = stall.entered();
    let undecided = matches!(rig.claim(), Decision::LaunchSaving);
    drop(stall);
    let launch = launch.join().unwrap();
    assert!(saving, "{launch:?}");
    launch.unwrap();
    assert!(undecided && matches!(rig.claim(), Decision::Unavailable));
    std::fs::remove_dir_all(&root).unwrap();
}

/// How long a Fire takes with nothing in its way.
async fn fire_alone() -> Duration {
    let rig = Rig::new().await;
    let started = Instant::now();
    commit_fire_intent(&rig.state).unwrap();
    started.elapsed()
}
/// The other side of round `round` of 40 starts anywhere within a Fire.
fn spread(round: u32, alone: Duration) {
    let until = Instant::now() + alone * round / 40;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}
/// Both at once, many times: never both, and one of them always.
#[tokio::test]
async fn a_claim_and_a_fire_at_once_are_one_or_the_other() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_claim_and_a_fire_at_once_are_one_or_the_other") {
        return;
    }
    let alone = fire_alone().await;
    let (mut fires, mut aborts) = (0, 0);
    for round in 0..40 {
        let rig = Rig::new().await;
        let go = Arc::new(std::sync::Barrier::new(2));
        let (state, start) = (rig.state.clone(), go.clone());
        let fire = crate::runtime_mode::spawn_thread("fire", move || {
            start.wait();
            commit_fire_intent(&state).is_ok()
        })
        .unwrap();
        go.wait();
        spread(round, alone);
        let claimed = loop {
            match rig.claim() {
                Decision::Claimed(claim) => break Some(claim),
                Decision::LaunchSaving => std::thread::sleep(Duration::from_micros(100)),
                Decision::Unavailable | Decision::Startup(_) => break None,
            }
        };
        let fired = fire.join().unwrap();
        assert_ne!(claimed.is_some(), fired, "round {round}");
        drop(claimed);
        rig.state.gse.abort.settle();
        let saved = operational::Saved::load(&rig.path).unwrap();
        let on_disk = saved.qualification.stage() == gse_sequence::qualification::MissionStage::Fire;
        assert_eq!((saved.automatic_abort_claimed, on_disk), (!fired, fired), "round {round}");
        if fired { fires += 1 } else { aborts += 1 }
    }
    eprintln!("claims {aborts} fires {fires}");
}

/// Tank overpressure latches without the GSE lock: it refuses a Fire, or the
/// Fire is admitted first and nothing is commanded. Never both.
#[tokio::test]
async fn an_overpressure_and_a_fire_at_once_never_both_command() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("an_overpressure_and_a_fire_at_once_never_both_command") {
        return;
    }
    let alone = fire_alone().await;
    let (mut fires, mut reliefs) = (0, 0);
    for round in 0..40 {
        let rig = Rig::new().await;
        let reading = |step: u64| {
            let mut rt = rig.state.gse.lock().unwrap();
            let now = rt.test_now_ms.take().unwrap_or_else(|| rt.now()) + step;
            rt.test_now_ms = Some(now);
            rt.valve_pressure = Some(Sample { value: 860.0, at_ms: now });
        };
        for _ in 0..13 {
            reading(20);
            overpressure::evaluate(&rig.state);
        }
        // This one trips.
        reading(20);
        assert!(!rig.state.gse.overpressure.latched());
        let go = Arc::new(std::sync::Barrier::new(2));
        let (state, start) = (rig.state.clone(), go.clone());
        let fire = crate::runtime_mode::spawn_thread("fire", move || {
            start.wait();
            commit_fire_intent(&state).is_ok()
        })
        .unwrap();
        go.wait();
        spread(round, alone);
        overpressure::evaluate(&rig.state);
        let fired = fire.join().unwrap();
        overpressure::evaluate(&rig.state);
        let relief = rig.state.gse.overpressure.owns_valves();
        assert_ne!(fired, relief, "round {round}");
        rig.router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(rig.sent.lock().unwrap().is_empty(), fired, "round {round}: {:?}", rig.sent.lock().unwrap());
        if fired { fires += 1 } else { reliefs += 1 }
    }
    eprintln!("reliefs {reliefs} fires {fires}");
}

/// The three places a Launch and an automatic action can meet without a lock
/// in common, each with the other one arriving exactly there.
fn trip_reading(rig: &Rig) {
    let reading = |step: u64| {
        let mut rt = rig.state.gse.lock().unwrap();
        let now = rt.test_now_ms.take().unwrap_or_else(|| rt.now()) + step;
        rt.test_now_ms = Some(now);
        rt.valve_pressure = Some(Sample { value: 860.0, at_ms: now });
    };
    for _ in 0..13 {
        reading(20);
        overpressure::evaluate(&rig.state);
    }
    // The next evaluation trips.
    reading(20);
}
#[tokio::test]
async fn an_overpressure_latched_after_fires_checks_refuses_it() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("an_overpressure_latched_after_fires_checks_refuses_it") {
        return;
    }
    let rig = Rig::new().await;
    trip_reading(&rig);
    let state = rig.state.clone();
    between::at(between::Point::FireAdmitted, move || assert_eq!(overpressure::act_for_test(&state), 1));
    assert!(commit_fire_intent(&rig.state).unwrap_err().contains("overpressure"));
    assert!(rig.saved().iter().all(|record| !record.1), "Fire was never written");
    overpressure::evaluate(&rig.state);
    assert!(rig.state.gse.overpressure.owns_valves());
}
#[tokio::test]
async fn a_fire_admitted_while_overpressure_decides_is_not_overridden() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("a_fire_admitted_while_overpressure_decides_is_not_overridden") {
        return;
    }
    let rig = Rig::new().await;
    trip_reading(&rig);
    let state = rig.state.clone();
    let fired = Arc::new(Mutex::new(None));
    let result = fired.clone();
    between::at(between::Point::OverpressureDecided, move || {
        *result.lock().unwrap() = Some(commit_fire_intent(&state).map(drop));
    });
    overpressure::evaluate(&rig.state);
    fired.lock().unwrap().take().unwrap().unwrap();
    rig.router.process_all_queues_with_timeout(0).unwrap();
    assert!(rig.sent.lock().unwrap().is_empty(), "nothing commanded after Fire: {:?}", rig.sent.lock().unwrap());
    let latch = rig.state.gse.overpressure.lock().latch.clone().expect("alarmed");
    assert!(!latch.relief_commanded);
}
#[tokio::test]
async fn a_launch_admitted_while_an_abort_is_claimed_is_noticed() {
    crate::mode_suite!(Hitl);
    if elsewhere("a_launch_admitted_while_an_abort_is_claimed_is_noticed") {
        return;
    }
    let rig = Rig::new().await;
    qualified_launch_proofs_for_test(&rig.state);
    let state = rig.state.clone();
    between::at(between::Point::ClaimTaken, move || {
        inhibit_automatic_actions_for_launch(&state).unwrap();
    });
    assert!(matches!(rig.claim(), Decision::Unavailable), "the claim is decided again, after the Launch");
    assert_eq!(rig.aborts_so_far(), 0);
}

/// An observed automatic abort is accepted before a Launch is admitted, and
/// then refuses it, or not until the Launch is decided.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observed_abort_refuses_a_launch_it_precedes() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if elsewhere("an_observed_abort_refuses_a_launch_it_precedes") {
        return;
    }
    let rig = Rig::new().await;
    assert_eq!(accept_observed_abort(&rig.state, true, automatic_generation(&rig.state)), Some(true));
    assert!(commit_fire_intent(&rig.state).is_err());
    assert!(rig.saved().is_empty(), "refused before anything was written");
    assert!(launch_abort_latched(&rig.state).is_err(), "checked again once admitted");
    let hold = rig.state.gse.abort.admit_launch(true).unwrap();
    rig.state.set_abort_indicator_latched(false);
    let (observer, drain) = crate::deferred_abort::spawn(rig.state.clone());
    crate::deferred_abort::observe(&rig.state, &board_abort("AB"), automatic_generation(&rig.state));
    assert_eq!(accept_observed_abort(&rig.state, true, automatic_generation(&rig.state)), None);
    // The worker has it, and is told to wait for the Launch.
    let taken = |n| rig.state.deferred_abort.claimed.load(std::sync::atomic::Ordering::Acquire) >= n;
    assert!(until(|| taken(1)).await);
    // A manual one is not held behind it.
    let manual = sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("ABORT"),
        &[crate::telemetry_schema::endpoint("ABORT")],
        "GS",
        crate::telemetry_task::get_current_timestamp_ms(),
        Arc::from(&b"Manual Abort"[..]),
    )
    .unwrap();
    crate::deferred_abort::observe(&rig.state, &manual, automatic_generation(&rig.state));
    assert!(until(|| !rig.state.deferred_abort.manual_pending()).await, "manual observed during the save");
    assert!(rig.state.abort_indicator_latched());
    drop(hold);
    let _ = drain.send(());
    assert!(tokio::time::timeout(WAIT, observer).await.is_ok());
}

// ---- Admission and mode changes ----

/// Recovery closes admission under the dispatch gate; an automatic abort is
/// counted without it. One of them always sees the other.
#[tokio::test]
async fn a_closed_admission_and_an_uncounted_abort_never_meet() {
    let state = crate::state::tests::test_app_state().await;
    use std::sync::atomic::Ordering;
    for round in 0..3_000_u64 {
        let go = Arc::new(std::sync::Barrier::new(2));
        let (closing, start) = (state.clone(), go.clone());
        let close = std::thread::spawn(move || {
            start.wait();
            reset_generation::close_operator_admission(&closing, automatic_generation(&closing), 77).unwrap();
            closing.operator_dispatch_inflight.load(Ordering::Acquire)
        });
        go.wait();
        let claim = claim_abort_dispatch(&state, automatic_generation(&state));
        let counted = close.join().unwrap();
        assert!(claim.is_none() || counted == 1, "round {round}: admitted and not counted");
        drop(claim);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        state.operator_admission_barrier.store(0, Ordering::Release);
    }
    state.operator_admission_barrier.store(77, Ordering::Release);
    assert!(matches!(decide_automatic_abort(&state, automatic_generation(&state)), Decision::Unavailable));
}

#[tokio::test]
async fn a_mode_change_and_an_abort_exclude_each_other() {
    crate::mode_suite!(Normal, Testing);
    if elsewhere("a_mode_change_and_an_abort_exclude_each_other") {
        return;
    }
    let rig = Rig::new().await;
    let token = crate::runtime_mode::token();
    let hold = hold_for_mode_switch(&rig.state).unwrap();
    assert!(matches!(rig.claim(), Decision::LaunchSaving), "decided again after the change");
    drop(hold);
    {
        let _other = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::TestFire);
        assert!(matches!(decide_liveness_abort(&rig.state, token), Decision::Unavailable), "its mode is over");
    }
    let stall = Stall::begin(Site::Qualification);
    rig.send();
    let recording = stall.entered();
    let refused = hold_for_mode_switch(&rig.state).is_err();
    drop(stall);
    assert!(recording && refused, "not while the record is being written");
    rig.state.gse.abort.settle();
    assert_eq!(mode_switch_block_reason(&rig.state), None);
}
