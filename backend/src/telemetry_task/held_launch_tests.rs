//! Zero before Launch: the held press keeps every send-time refusal.
//! Production hold/send path, production Launch handlers and an actual SDK
//! router with warm ignition-pair peers. No hardware outputs.
use super::*;
use crate::auto_zero::{Runtime, Settings, before_launch};

type Hook = (usize, Box<dyn FnOnce(&AppState) + Send>);
static BEFORE_SEND: std::sync::Mutex<Option<Hook>> = std::sync::Mutex::new(None);
/// Every held Launch that entered the send path in this (isolated) process.
static SENDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Runs once, after the worker's send-time checks and immediately before the
/// ordinary Launch handler, so the handler's own refusals are exercised.
pub(super) fn entered() {
    SENDS.fetch_add(1, Ordering::SeqCst);
}

pub(super) fn before_send(state: &AppState) {
    let hook = {
        let mut slot = BEFORE_SEND.lock().unwrap();
        if slot.as_ref().is_some_and(|(owner, _)| *owner == state as *const AppState as usize) {
            slot.take().map(|(_, hook)| hook)
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        hook(state);
    }
}

fn install(state: &Arc<AppState>, hook: impl FnOnce(&AppState) + Send + 'static) {
    *BEFORE_SEND.lock().unwrap() = Some((Arc::as_ptr(state) as usize, Box::new(hook)));
}

struct Rig {
    state: Arc<AppState>,
    router: Arc<Router>,
    _peers: crate::ignition_pair::fixture::Peers,
}

/// The same armed fixtures the existing Launch suites use in each build.
async fn rig(before_launch: bool) -> Rig {
    let state = crate::state::tests::test_app_state().await;
    #[cfg(all(feature = "test_fire_mode", not(feature = "hitl_mode")))]
    let (router, peers) = {
        let router = Arc::new(Router::new(
            sedsnet::router::RouterConfig::new([crate::ignition_pair::fixture::handler(&state, |_| Ok(()))])
                .with_sender("GS"),
        ));
        let _qualification = crate::gse::armed_fixture_with_real_pair_for_test(&state, &router);
        let peers = crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
        (router, peers)
    };
    #[cfg(not(all(feature = "test_fire_mode", not(feature = "hitl_mode"))))]
    let (router, peers) = {
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            crate::ignition_pair::fixture::handler(&state, |_| Ok(())),
        ])));
        let valve_side = router.add_side_packed_with_options(
            "umbilical_comms",
            |_| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let discovery = sedsnet::discovery::build_discovery_announce(
            Board::ValveBoard.sender_id(),
            123,
            &[crate::telemetry_schema::endpoint("VALVE_BOARD")],
        )
        .unwrap();
        router.rx_packed_from_side(&serialize::pack_packet(&discovery), valve_side).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        #[cfg(not(feature = "hitl_mode"))]
        let _qualification = crate::gse::armed_fixture_for_test(&state, &router);
        if state.topology_router.get().is_none() {
            state.topology_router.set(router.clone()).unwrap();
        }
        let peers = crate::ignition_pair::fixture::Peers::warm(&state, router.clone()).await;
        (router, peers)
    };
    let mut cfg = crate::loadcell::LoadcellCalibrationFile::default();
    cfg.ch1.m = Some(1.0);
    cfg.ch1.b = Some(0.0);
    cfg.ch1_zero_raw = None;
    let ch1 = cfg.extra_channels.entry("ch1".into()).or_default();
    ch1.linear.m = Some(1.0);
    ch1.linear.b = Some(0.0);
    *state.loadcell_calibration.lock().unwrap() = cfg;
    *state.auto_zero.lock().unwrap() =
        Runtime::testing_settings(Settings { enabled: true, before_launch });
    Rig { state, router, _peers: peers }
}

/// Exactly what the worker does with a queued Launch press.
fn press(rig: &Rig) -> bool {
    hold_launch_for_loadcell_zero(
        &rig.state,
        &rig.router,
        HeldLaunch {
            command: TelemetryCommand::Launch,
            launch_revision: crate::board_abort::capture_launch_revision_named(&rig.state, "Launch"),
            recovery_origin: None,
            queued_generation: Some(crate::gse::automatic_generation(&rig.state)),
            pressed: crate::gse::LaunchAdmission::capture(&rig.state),
        },
    )
}

/// The worker's immediate (setting off) Launch path in this build.
async fn immediate(rig: &Rig) {
    let revision = crate::board_abort::capture_launch_revision_named(&rig.state, "Launch");
    #[cfg(all(feature = "test_fire_mode", not(feature = "hitl_mode")))]
    handle_local_ground_station_launch_command(rig.state.clone(), rig.router.clone(), revision).await;
    #[cfg(not(all(feature = "test_fire_mode", not(feature = "hitl_mode"))))]
    handle_flight_computer_launch_command(rig.state.clone(), rig.router.clone(), revision).await;
}

/// DAQ KG1000 receipts at `hz`, as the ingress path records them.
fn feed(state: &Arc<AppState>, hz: u64, raw: impl Fn(u64) -> f32 + Send + 'static) -> tokio::task::JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        let mut id = 0;
        loop {
            id += 1;
            state.auto_zero.lock().unwrap().observe_packet("KG1000", raw(id), id);
            tokio::time::sleep(Duration::from_millis(1_000 / hz)).await;
        }
    })
}

async fn released(state: &AppState) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while before_launch::held(state) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("held Launch did not finish");
}

fn notified(state: &AppState, text: &str) -> bool {
    state.notifications_snapshot().iter().any(|n| n.message.contains(text))
}

fn tare_kg(state: &AppState, raw: f32) -> f32 {
    crate::loadcell::calibrated_weight_kg(&state.loadcell_calibration.lock().unwrap(), "KG1000", raw).unwrap()
}

fn sent(state: &AppState) -> bool {
    state.launch_indicator_latched()
}

macro_rules! isolated {
    ($name:ident) => {
        if crate::operator_queue::isolated_native_case(module_path!(), stringify!($name)) {
            return;
        }
    };
}

#[tokio::test]
async fn setting_off_leaves_launch_and_at_launch_zero_unchanged() {
    isolated!(setting_off_leaves_launch_and_at_launch_zero_unchanged);
    let rig = rig(false).await;
    let before = rig.state.auto_zero.lock().unwrap().capture_identity_for_test();
    assert!(!press(&rig), "an opted-out press must reach the ordinary path");
    assert!(!before_launch::held(&rig.state));
    assert_eq!(rig.state.auto_zero.lock().unwrap().capture_identity_for_test(), before);
    assert!(rig.state.action_policy_snapshot().launch_zero.is_none());
    immediate(&rig).await;
    assert!(sent(&rig.state), "{:?}", rig.state.notifications_snapshot());
    // Today's concurrent at-launch zero still starts when the fill setting enables it.
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    assert!(rig.state.auto_zero.lock().unwrap().pending());
    rig.state.request_shutdown();
}

#[tokio::test]
async fn held_launch_applies_tare_before_t_clock_and_launch_file() {
    isolated!(held_launch_applies_tare_before_t_clock_and_launch_file);
    let rig = rig(true).await;
    let feeder = feed(&rig.state, 1_000, |_| 0.25);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(tare_kg(&rig.state, 0.25) > 0.2);
    let observed = Arc::new(Mutex::new(None));
    let record = observed.clone();
    install(&rig.state, move |state| {
        *record.lock().unwrap() = Some((
            tare_kg(state, 0.25),
            state.launch_indicator_latched(),
            state.launch_history.lock().unwrap().automatic_actions_inhibited(),
            *state.daq_log_session.lock().unwrap(),
            state.launch_clock_snapshot().kind,
        ));
    });
    assert!(press(&rig));
    assert!(before_launch::held(&rig.state));
    let progress = rig.state.action_policy_snapshot().launch_zero.expect("progress is published");
    assert!(!progress.sending && progress.remaining_s <= 10 && progress.samples == 200);
    released(&rig.state).await;
    feeder.abort();
    let (kg, latched, inhibited, session, clock) = observed.lock().unwrap().take().expect("Launch reached send");
    assert!(kg.abs() < 1e-4, "tare must be applied before Launch is sent: {kg}");
    assert!(!latched && !inhibited, "nothing was sent before the tare");
    assert_eq!(session, 0, "the DAQ launch file starts only after the tare");
    assert_eq!(clock, LaunchClockKind::Idle, "T-10 starts only after the tare");
    assert!(notified(&rig.state, "KG1000 zero saved and sent to the DAQ"));
    assert!(sent(&rig.state), "{:?}", rig.state.notifications_snapshot());
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    assert_ne!(*rig.state.daq_log_session.lock().unwrap(), 0);
    // The held path replaces, never repeats, the at-launch capture.
    assert!(!rig.state.auto_zero.lock().unwrap().pending());
    assert!(tare_kg(&rig.state, 0.25).abs() < 1e-4);
    rig.state.request_shutdown();
}

#[tokio::test]
async fn missing_stream_still_launches_with_previous_calibration_warning() {
    isolated!(missing_stream_still_launches_with_previous_calibration_warning);
    let rig = rig(true).await;
    assert!(press(&rig));
    released(&rig.state).await;
    assert!(notified(&rig.state, "Loadcell zero before Launch failed: no live KG1000 samples"));
    assert!(notified(&rig.state, "PREVIOUS loadcell calibration"));
    assert!(sent(&rig.state), "{:?}", rig.state.notifications_snapshot());
    assert!((tare_kg(&rig.state, 0.25) - 0.25).abs() < 1e-6, "calibration unchanged");
    rig.state.request_shutdown();
}

#[tokio::test]
async fn unstable_load_still_launches_with_previous_calibration_warning() {
    isolated!(unstable_load_still_launches_with_previous_calibration_warning);
    let rig = rig(true).await;
    let feeder = feed(&rig.state, 1_000, |id| if id % 2 == 0 { -0.7 } else { 0.7 });
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(press(&rig));
    released(&rig.state).await;
    feeder.abort();
    assert!(notified(&rig.state, "moving or unstable"), "{:?}", rig.state.notifications_snapshot());
    assert!(notified(&rig.state, "PREVIOUS loadcell calibration"));
    assert!(sent(&rig.state));
    assert!((tare_kg(&rig.state, 0.25) - 0.25).abs() < 1e-6);
    rig.state.request_shutdown();
}

#[tokio::test]
async fn stalled_stream_times_out_within_ten_seconds_and_launches_with_warning() {
    isolated!(stalled_stream_times_out_within_ten_seconds_and_launches_with_warning);
    let rig = rig(true).await;
    let feeder = feed(&rig.state, 1_000, |_| 0.25);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(press(&rig));
    feeder.abort();
    let started = std::time::Instant::now();
    released(&rig.state).await;
    let took = started.elapsed();
    assert!(took >= Duration::from_secs(9) && took < Duration::from_secs(12), "{took:?}");
    assert!(notified(&rig.state, "within 10 seconds"), "{:?}", rig.state.notifications_snapshot());
    assert!(notified(&rig.state, "PREVIOUS loadcell calibration"));
    assert_eq!(SENDS.load(Ordering::SeqCst), 1, "the timed-out zero still sends Launch");
    // Operational builds seed their armed readiness once; ten seconds later
    // that modeled evidence is stale and the send-time interlock refuses it.
    #[cfg(feature = "hitl_mode")]
    assert!(sent(&rig.state));
    #[cfg(not(feature = "hitl_mode"))]
    assert!(sent(&rig.state) || notified(&rig.state, "Held Launch not sent: Command Launch blocked by sequence/key interlock"));
    rig.state.request_shutdown();
}

#[tokio::test]
async fn stop_during_zero_cancels_the_held_launch() {
    isolated!(stop_during_zero_cancels_the_held_launch);
    let rig = rig(true).await;
    let feeder = feed(&rig.state, 20, |_| 0.25);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(press(&rig));
    tokio::time::sleep(Duration::from_millis(200)).await;
    rig.state.manual_abort_intent.request();
    released(&rig.state).await;
    feeder.abort();
    assert!(notified(&rig.state, "Launch cancelled by Stop"));
    assert!(!sent(&rig.state));
    assert!(!rig.state.launch_history.lock().unwrap().automatic_actions_inhibited());
    assert!((tare_kg(&rig.state, 0.25) - 0.25).abs() < 1e-6, "a cancelled Launch keeps its tare");
    rig.state.request_shutdown();
}

#[tokio::test]
async fn completed_stop_after_press_refuses_the_held_launch_by_its_ticket() {
    isolated!(completed_stop_after_press_refuses_the_held_launch_by_its_ticket);
    let rig = rig(true).await;
    install(&rig.state, |state| {
        // A Stop pressed after Launch whose SDK ownership already retired:
        // nothing is pending any more, only the epoch moved on.
        state.manual_abort_intent.request();
        state.manual_abort_intent.try_claim().unwrap().accepted();
        assert!(state.manual_abort_intent.pending_epoch().is_none());
    });
    assert!(press(&rig));
    released(&rig.state).await;
    assert!(!sent(&rig.state), "{:?}", rig.state.notifications_snapshot());
    // The Stop epoch also advances the launch revision; either refusal applies.
    assert!(
        notified(&rig.state, "Manual Abort invalidated launch admission")
            || notified(&rig.state, "A board Abort superseded this queued launch intent"),
        "{:?}",
        rig.state.notifications_snapshot()
    );
    assert!(!rig.state.launch_history.lock().unwrap().automatic_actions_inhibited());
    rig.state.request_shutdown();
}

#[tokio::test]
async fn pressed_ticket_refuses_admission_after_a_completed_stop_even_with_a_fresh_revision() {
    isolated!(pressed_ticket_refuses_admission_after_a_completed_stop_even_with_a_fresh_revision);
    let rig = rig(true).await;
    let state = &rig.state;
    let pressed = crate::gse::LaunchAdmission::capture(state);
    state.manual_abort_intent.request();
    state.manual_abort_intent.try_claim().unwrap().accepted();
    let revision = crate::board_abort::capture_launch_revision_named(state, "Launch");
    assert!(crate::board_abort::validate_launch_revision(state, revision).is_ok());
    #[cfg(feature = "hitl_mode")]
    let refused = crate::gse::inhibit_automatic_actions_for_pressed_launch(state, revision, Some(pressed));
    #[cfg(not(feature = "hitl_mode"))]
    let refused = crate::gse::commit_fire_intent_for_pressed_launch(state, revision, Some(pressed));
    assert!(refused.unwrap_err().contains("Manual Abort invalidated launch admission"));
    assert!(!state.launch_history.lock().unwrap().automatic_actions_inhibited());
    state.request_shutdown();
}

#[tokio::test]
async fn abort_cancel_or_cancel_fill_during_zero_cancels_the_held_launch() {
    isolated!(abort_cancel_or_cancel_fill_during_zero_cancels_the_held_launch);
    for (index, how) in ["Abort", "Cancel", "CancelFill"].into_iter().enumerate() {
        let rig = rig(true).await;
        let feeder = feed(&rig.state, 20, |_| 0.25);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(press(&rig));
        tokio::time::sleep(Duration::from_millis(200)).await;
        match index {
            0 => rig.state.auto_zero.lock().unwrap().cancel_for_command(&TelemetryCommand::Abort),
            1 => before_launch::cancel(&rig.state).unwrap(),
            _ => rig.state.auto_zero.lock().unwrap().cancel_for_command(&TelemetryCommand::CancelFill),
        }
        released(&rig.state).await;
        feeder.abort();
        assert!(notified(&rig.state, "Launch cancelled by Cancel or Abort"), "{how}");
        assert!(!sent(&rig.state), "{how}");
        assert!(before_launch::cancel(&rig.state).is_err());
        rig.state.request_shutdown();
    }
}

#[tokio::test]
async fn board_abort_after_press_refuses_the_held_launch() {
    isolated!(board_abort_after_press_refuses_the_held_launch);
    let rig = rig(true).await;
    install(&rig.state, |state| {
        state.board_abort.observe_abort(Board::ValveBoard, std::time::Instant::now(), get_current_timestamp_ms());
    });
    assert!(press(&rig));
    released(&rig.state).await;
    assert!(!sent(&rig.state));
    assert!(
        notified(&rig.state, "A board reports Abort") || notified(&rig.state, "A board Abort superseded"),
        "{:?}",
        rig.state.notifications_snapshot()
    );
    rig.state.request_shutdown();
}

#[tokio::test]
async fn launch_lock_set_during_zero_refuses_the_held_launch() {
    isolated!(launch_lock_set_during_zero_refuses_the_held_launch);
    let rig = rig(true).await;
    let feeder = feed(&rig.state, 1_000, |_| 0.25);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(press(&rig));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(before_launch::held(&rig.state), "still zeroing");
    rig.state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
    released(&rig.state).await;
    feeder.abort();
    assert!(!sent(&rig.state));
    assert!(notified(&rig.state, crate::gse::LAUNCH_LOCKED_MESSAGE), "{:?}", rig.state.notifications_snapshot());
    rig.state.request_shutdown();
}

#[tokio::test]
async fn turbocharge_started_during_zero_refuses_the_held_launch() {
    isolated!(turbocharge_started_during_zero_refuses_the_held_launch);
    let rig = rig(true).await;
    let feeder = feed(&rig.state, 1_000, |_| 0.25);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(press(&rig));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(before_launch::held(&rig.state), "still zeroing");
    rig.state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Turbocharge;
    released(&rig.state).await;
    feeder.abort();
    assert!(!sent(&rig.state));
    assert!(
        notified(&rig.state, crate::gse::TURBOCHARGE_UNFINISHED_MESSAGE)
            || notified(&rig.state, "Held Launch not sent"),
        "{:?}",
        rig.state.notifications_snapshot()
    );
    assert!(!rig.state.launch_history.lock().unwrap().automatic_actions_inhibited());
    rig.state.request_shutdown();
}

#[tokio::test]
async fn refused_press_never_starts_a_zero() {
    isolated!(refused_press_never_starts_a_zero);
    let rig = rig(true).await;
    rig.state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
    let before = rig.state.auto_zero.lock().unwrap().capture_identity_for_test();
    assert!(press(&rig));
    assert!(!before_launch::held(&rig.state));
    assert_eq!(rig.state.auto_zero.lock().unwrap().capture_identity_for_test(), before);
    assert!(notified(&rig.state, crate::gse::LAUNCH_LOCKED_MESSAGE));
    assert!(!sent(&rig.state));
    rig.state.request_shutdown();
}

#[tokio::test]
async fn second_press_during_zero_does_not_queue_a_second_launch() {
    isolated!(second_press_during_zero_does_not_queue_a_second_launch);
    let rig = rig(true).await;
    let feeder = feed(&rig.state, 1_000, |_| 0.25);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(press(&rig));
    let identity = rig.state.auto_zero.lock().unwrap().capture_identity_for_test();
    assert!(press(&rig), "a second press is taken over, not dispatched");
    assert!(notified(&rig.state, before_launch::ALREADY_PENDING));
    assert_eq!(
        rig.state.auto_zero.lock().unwrap().capture_identity_for_test(),
        identity,
        "the first capture continues untouched"
    );
    released(&rig.state).await;
    feeder.abort();
    assert!(sent(&rig.state));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(SENDS.load(Ordering::SeqCst), 1, "only the first press reached the send path");
    rig.state.request_shutdown();
}
