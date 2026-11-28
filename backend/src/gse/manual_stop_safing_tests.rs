//! An operator Abort while the GSE saves its qualification record with the
//! dispatch gate and the GSE lock held. The save is held at its fsync
//! (`persist_diagnostic`) for as long as the test wants: the ABORT and the
//! GSE's own safe outputs must leave while it is still held, and the GSE
//! bookkeeping follows once it ends, whether the write succeeds or fails.
use super::*;
use sedsnet::router::{EndpointHandler, Router, RouterConfig, RouterSideOptions};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

/// Longest the ABORT and the GSE safe outputs may take while the save is
/// held. The save is held for up to 10 s.
const PROMPT: Duration = Duration::from_millis(750);

/// The safe directions `Engine::fault` asks for (RELIEVED), as (type, byte).
fn safing() -> Vec<(String, u8)> {
    vec![
        ("VALVE_COMMAND".into(), crate::rocket_commands::ValveBoardCommands::PilotClose as u8),
        ("VALVE_COMMAND".into(), crate::rocket_commands::ValveBoardCommands::NormallyOpenOpen as u8),
        ("VALVE_COMMAND".into(), crate::rocket_commands::ValveBoardCommands::DumpOpen as u8),
        ("ACTUATOR_COMMAND".into(), crate::rocket_commands::ActuatorBoardCommands::NitrogenClose as u8),
        ("ACTUATOR_COMMAND".into(), crate::rocket_commands::ActuatorBoardCommands::NitrousClose as u8),
    ]
}

struct Rig {
    state: Arc<AppState>,
    router: Arc<Router>,
    path: std::path::PathBuf,
    /// Every ABORT and controller command the station sent, in order (its own
    /// audit copies).
    sent: Receiver<(String, u8)>,
}
impl Rig {
    async fn new() -> Self {
        let state = crate::state::tests::test_app_state().await;
        let (record, sent) = channel::<(String, u8)>();
        let audit = |observe: Option<Arc<AppState>>, record: Sender<(String, u8)>| {
            move |packet: &sedsnet::packet::Packet| {
                let kind = packet.data_type().as_str().to_string();
                if (kind == "ABORT") == observe.is_some() {
                    let _ = record.send((kind, packet.payload().first().copied().unwrap_or(0) & 0x7f));
                }
                if let Some(state) = &observe {
                    crate::deferred_abort::observe(state, packet, automatic_generation(state));
                }
                Ok(())
            }
        };
        let router = Arc::new(Router::new(
            RouterConfig::new([
                EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("ABORT"),
                    audit(Some(state.clone()), record.clone()),
                ),
                EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("GROUND_STATION"),
                    audit(None, record),
                ),
            ])
            .with_sender("GS"),
        ));
        let side = router.add_side_packed_with_options(
            "wire",
            |_| Ok(()),
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
        // A fill sequence owns the valves.
        state.gse.lock().unwrap().engine.status.phase = Phase::Filling;
        assert!(state.gse.sequence_owned_valves());
        Self { state, router, path, sent }
    }
    /// Another thread saves `next` as the qualification record, holding the
    /// dispatch gate and the GSE lock; it is held at its fsync until `release`
    /// is answered (or dropped, which fails the write).
    fn saving_elsewhere(
        &self,
        next: operational::Saved,
    ) -> (std::thread::JoinHandle<Result<(), String>>, Sender<()>) {
        let (entered, release) = operational::persist_diagnostic::arm(self.path.clone());
        let state = self.state.clone();
        let saving = std::thread::spawn(move || {
            with_command_gate(|| {
                let mut rt = state.gse.lock().unwrap();
                rt.operational.saved = next;
                rt.operational.persist()
            })
        });
        entered.recv_timeout(Duration::from_secs(5)).expect("the save reached its fsync");
        (saving, release)
    }
    /// The operator presses Abort: the production publisher's path, on a
    /// thread of its own as in the station.
    fn press_abort(&self) -> std::thread::JoinHandle<()> {
        self.state.manual_abort_intent.request();
        let mut claim = self.state.manual_abort_intent.try_claim().unwrap();
        let (state, router) = (self.state.clone(), self.router.clone());
        std::thread::spawn(move || {
            assert!(crate::manual_abort_utc::publish_local_claim(&state, &router, &mut claim).unwrap());
            claim.accepted();
        })
    }
    /// What was sent within `within`, until the ABORT and every safe output
    /// have been seen.
    fn abort_and_safing(&self, within: Duration) -> Vec<(String, u8)> {
        let deadline = Instant::now() + within;
        let mut seen = Vec::new();
        let mut wanted = safing();
        wanted.push(("ABORT".into(), 0));
        while !wanted.is_empty() {
            let Ok(sent) = self.sent.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
                break;
            };
            wanted.retain(|(kind, byte)| !(*kind == sent.0 && (kind == "ABORT" || *byte == sent.1)));
            seen.push(sent);
        }
        seen
    }
    fn wait_for_fault(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.state.gse.lock().unwrap().engine.status.phase != Phase::Fault {
            assert!(Instant::now() < deadline, "the GSE took the Abort once the save ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn has_all(seen: &[(String, u8)]) -> bool {
    seen.iter().any(|(kind, _)| kind == "ABORT")
        && safing().iter().all(|wanted| seen.contains(wanted))
}

/// The ABORT and the GSE's supply close and dump/vent open leave while the
/// save is still held at its fsync; the GSE faults once the save ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manual_abort_waits_for_no_qualification_save() {
    crate::mode_suite!(Normal, TestFire, Hitl, IntegratedGroundTest);
    // Own process: the fsync hold is process-wide.
    if crate::operator_queue::isolated_native_case(module_path!(), "a_manual_abort_waits_for_no_qualification_save") {
        return;
    }
    let rig = Rig::new().await;
    let (observer, _stop) = crate::deferred_abort::spawn(rig.state.clone());
    let next = rig.state.gse.lock().unwrap().operational.saved.clone();
    let (saving, release) = rig.saving_elsewhere(next);
    let pressed = Instant::now();
    let publisher = rig.press_abort();
    let seen = rig.abort_and_safing(PROMPT);
    let took = pressed.elapsed();
    let still_saving = !saving.is_finished();
    release.send(()).unwrap();
    assert_eq!(saving.join().unwrap(), Ok(()));
    publisher.join().unwrap();
    assert!(has_all(&seen), "ABORT and GSE safe outputs within {PROMPT:?}; sent: {seen:?}");
    assert!(still_saving, "the save was still held at its fsync when they left");
    assert!(took < PROMPT, "{took:?}");
    rig.wait_for_fault();
    assert!(rig.state.gse.lock().unwrap().operational.persist_error.is_none());
    observer.abort();
}

/// A qualification write that fails while an Abort is pressed: the ABORT and
/// the safe outputs still leave at once, the record on disk is the one from
/// before, and the failure is reported (latched fault and persist error).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_qualification_save_leaves_the_record_and_reports() {
    crate::mode_suite!(Normal, TestFire, IntegratedGroundTest);
    // Own process: the fsync hold is process-wide.
    if crate::operator_queue::isolated_native_case(module_path!(), "a_failed_qualification_save_leaves_the_record_and_reports") {
        return;
    }
    let rig = Rig::new().await;
    let (observer, _stop) = crate::deferred_abort::spawn(rig.state.clone());
    let before = std::fs::read(&rig.path).unwrap();
    let mut next = rig.state.gse.lock().unwrap().operational.saved.clone();
    next.fill_target_kg = next.fill_target_kg.map(|kg| kg + 1.0);
    let (saving, release) = rig.saving_elsewhere(next);
    let publisher = rig.press_abort();
    let seen = rig.abort_and_safing(PROMPT);
    let still_saving = !saving.is_finished();
    // The disk fails the held write.
    drop(release);
    let error = saving.join().unwrap().expect_err("the write failed");
    publisher.join().unwrap();
    assert!(has_all(&seen), "ABORT and GSE safe outputs within {PROMPT:?}; sent: {seen:?}");
    assert!(still_saving);
    assert_eq!(std::fs::read(&rig.path).unwrap(), before, "a failed write leaves the record on disk");
    assert!(
        std::fs::read_dir(rig.path.parent().unwrap()).unwrap().flatten().all(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !name.starts_with(&*rig.path.file_stem().unwrap().to_string_lossy()) || entry.path() == rig.path
        }),
        "no temporary file is left"
    );
    rig.wait_for_fault();
    let rt = rig.state.gse.lock().unwrap();
    assert_eq!(rt.operational.persist_error.as_deref(), Some(error.as_str()), "the failure is reported");
    assert!(rt.operational.saved.qualification.fault().is_some());
    drop(rt);
    observer.abort();
}

/// Nothing is sent early when the GSE is free (the ordinary path reacts at
/// once) or when no sequence owns the valves; a failure to send is reported
/// and changes no valve state.
#[tokio::test]
async fn safing_is_only_sent_for_a_busy_gse_and_reports_a_failure() {
    // Own process: other tests take the process-wide dispatch gate.
    if crate::operator_queue::isolated_native_case(module_path!(), "safing_is_only_sent_for_a_busy_gse_and_reports_a_failure") {
        return;
    }
    let state = crate::state::tests::test_app_state().await;
    state.gse.lock().unwrap().engine.status.phase = Phase::Idle;
    assert_eq!(manual_stop_safing(&state), Ok(0), "no sequence owns the valves");
    state.gse.lock().unwrap().engine.status.phase = Phase::Filling;
    assert_eq!(manual_stop_safing(&state), Ok(0), "the GSE is free");
    // Held, and no command router: nothing can be sent.
    let held = state.gse.lock().unwrap();
    let result = manual_stop_safing(&state);
    drop(held);
    assert!(result.as_ref().is_err_and(|error| error.contains("router")), "{result:?}");
    for key in KEYS {
        assert_eq!(state.get_pending_umbilical_valve_state(key), None);
    }
}
