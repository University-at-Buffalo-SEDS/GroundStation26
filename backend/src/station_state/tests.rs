//! A change on the primary reaches the RRLS's display from one frame,
//! listening or relaying; stale, reordered, foreign and takeover copies do not.
use super::*;
use crate::rrls::{Mesh, Role};
use crate::types::FlightState as S;

const SESSION: u32 = 0x51a7_e5e5;

fn idle_clock() -> LaunchClockMsg {
    LaunchClockMsg::idle()
}
fn body(
    flight_state: S,
    phase: MissionPhase,
    clock: LaunchClockMsg,
    clock_revision: u32,
) -> StationState {
    StationState {
        flight_state,
        phase,
        required_boards_seen: true,
        fill_activity: false,
        fc_contact_lost: false,
        mode: Mode::Normal,
        clock,
        clock_revision,
        clock_authority: 0x0123_4567,
        netvar: Some(0),
        radio_test: (0, 0),
    }
}

/// A primary and an RRLS that has its HEALTH: listening (the primary reports
/// its direct AV-bay link) or relaying for it.
fn link(relaying: bool, now: u64) -> (Mesh, Mesh) {
    let mut primary = Mesh::new(Role::Primary, SESSION);
    let mut rrls = Mesh::new(Role::Rrls, 7);
    if relaying {
        rrls.assume_av_bay_heard_for_test();
    }
    rrls.incoming(true, &primary.health_frame_for_test(!relaying), now);
    assert_eq!(rrls.active(now), relaying);
    (primary, rrls)
}

/// The primary publishes `state`; its first copy crosses to the RRLS.
fn carry(primary: &mut Mesh, rrls: &mut Mesh, state: &StationState, now: u64) {
    primary.publish_station_state(state.encode(), now);
    let frame = primary
        .take_station_state_for_test()
        .expect("a change is queued at once");
    rrls.incoming(true, &frame, now);
}

#[test]
fn the_body_round_trips_and_malformed_bodies_are_refused() {
    let mut state = body(S::NitrousFill, MissionPhase::Fill, idle_clock(), 3);
    state.clock = LaunchClockMsg {
        kind: LaunchClockKind::TMinus,
        anchor_timestamp_ms: Some(1_700_000_000_000),
        duration_ms: Some(60_000),
    };
    state.mode = Mode::TestFire;
    state.fill_activity = true;
    state.netvar = None;
    let bytes = state.encode();
    assert_eq!(bytes.len(), BODY_LEN);
    assert_eq!(StationState::decode(&bytes), Some(state));
    let mut bad = bytes;
    bad[0] = 99;
    assert_eq!(StationState::decode(&bad), None);
    let mut bad = bytes;
    bad[3] = 9;
    assert_eq!(StationState::decode(&bad), None);
    assert_eq!(StationState::decode(&bytes[1..]), None);
}

#[test]
fn a_change_goes_at_once_then_two_copies_within_a_second_then_every_five_seconds() {
    let mut primary = Mesh::new(Role::Primary, SESSION);
    let state = body(S::Idle, MissionPhase::Idle, idle_clock(), 0);
    let mut sent = Vec::new();
    let mut revisions = Vec::new();
    for now in (1_000..=12_000).step_by(50) {
        if now == 1_000 {
            primary.publish_station_state(state.encode(), now);
        } else {
            // Unchanged state: the worker publishes it every 100 ms.
            primary.publish_station_state(state.encode(), now);
            primary.tick(now);
        }
        while let Some(frame) = primary.take_station_state_for_test() {
            sent.push(now);
            revisions.push(u32::from_le_bytes(frame[6..10].try_into().unwrap()));
        }
    }
    assert_eq!(sent, [1_000, 1_250, 1_750, 6_750, 11_750]);
    assert!(
        revisions.iter().all(|r| *r == revisions[0]),
        "unchanged state keeps its revision"
    );
    // A change: a new revision, queued in the same call.
    let reset = body(S::Startup, MissionPhase::Startup, idle_clock(), 1);
    primary.publish_station_state(reset.encode(), 12_010);
    let frame = primary.take_station_state_for_test().unwrap();
    assert_eq!(
        u32::from_le_bytes(frame[6..10].try_into().unwrap()),
        revisions[0] + 1
    );
    assert_eq!(frame[1], VERSION, "version");
    assert_eq!(
        &frame[2..6],
        &SESSION.to_le_bytes(),
        "the primary's HEALTH session"
    );
}

#[test]
fn the_broadcast_waits_behind_abort_and_control_and_takes_an_owed_settings_turn() {
    let mut primary = Mesh::new(Role::Primary, SESSION);
    let state = body(S::Armed, MissionPhase::Fill, idle_clock(), 0);
    primary.queue_control_for_test(1_000);
    primary.publish_station_state(state.encode(), 1_000);
    let queued = primary.queued_station_state_for_test();
    assert_eq!(queued.len(), 1);
    assert!(queued[0].1 < 254, "below Abort and control (254)");
    // Protected selection (Abort, control, HEALTH) never takes it.
    let first = primary.begin_critical_for_test(1_000).unwrap();
    assert_eq!(crate::rrls::kind_of(&first), Some(11), "the control goes first");
    assert_eq!(primary.begin_critical_for_test(1_000), None);
    // The owed settings turn carries it when nothing else is owed.
    let owed = primary.begin_owed_for_test(1_000).unwrap();
    assert_eq!(crate::rrls::kind_of(&owed), Some(16));
    // An unsent copy older than TTL_MS is dropped; the next copy replaces it.
    primary.publish_station_state(
        body(S::Aborted, MissionPhase::Idle, idle_clock(), 0).encode(),
        2_000,
    );
    assert_eq!(primary.begin_any_for_test(2_000 + TTL_MS + 1), None);
}

/// rrls_relay just after a cut (native SITL): a launch's control stream kept
/// background admission shut, and the one owed settings turn in the second
/// after the change went to a HOST_CONTROL reply. Every copy (0, 250, 750 ms)
/// then expired unsent and the RRLS showed the launch 5-10 s late, with the
/// next periodic copy. The newest copy now waits for the next owed turn.
#[test]
fn a_change_whose_owed_turn_went_to_a_reply_goes_on_the_next_turn() {
    let _serial = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset_for_test();
    let (mut primary, mut rrls) = link(false, 0);
    let armed = body(S::Armed, MissionPhase::Fill, idle_clock(), 3);
    carry(&mut primary, &mut rrls, &armed, 0);
    // The launch: the change and its copies, queued while nothing is sent.
    let mut launch = body(S::Launch, MissionPhase::Launch, idle_clock(), 4);
    launch.clock = LaunchClockMsg {
        kind: LaunchClockKind::TPlus,
        anchor_timestamp_ms: Some(1_767_225_760_000),
        duration_ms: None,
    };
    for now in (1_000..=1_800).step_by(100) {
        primary.publish_station_state(launch.encode(), now);
    }
    // The owed turn at 1.8 s carries an RRLS's HOST_CONTROL reply.
    primary.queue_control_for_test(1_800);
    let reply = primary.begin_owed_for_test(1_800).unwrap();
    assert_eq!(crate::rrls::kind_of(&reply), Some(11));
    // The next owed turn, one SETTINGS_TURN_MS later: the change goes now,
    // not 5 s later with the next periodic copy.
    for now in (1_900..=2_800).step_by(100) {
        primary.publish_station_state(launch.encode(), now);
    }
    let frame = primary
        .begin_owed_for_test(2_800)
        .expect("the newest copy is still queued for the next owed turn");
    assert_eq!(crate::rrls::kind_of(&frame), Some(16));
    rrls.incoming(true, &frame, 2_800);
    assert_eq!(received_for_test(), Some(launch));
    reset_for_test();
}

#[test]
fn an_rrls_takes_the_primarys_state_from_one_frame_listening_and_relaying() {
    let _serial = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for relaying in [false, true] {
        reset_for_test();
        let (mut primary, mut rrls) = link(relaying, 1_000);
        let state = body(S::Aborted, MissionPhase::Idle, idle_clock(), 4);
        carry(&mut primary, &mut rrls, &state, 1_010);
        assert_eq!(received_for_test(), Some(state), "relaying: {relaying}");
    }
    reset_for_test();
}

#[test]
fn stale_reordered_and_foreign_copies_are_ignored() {
    let _serial = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset_for_test();
    let (_, mut rrls) = link(false, 1_000);
    let older = body(S::Armed, MissionPhase::Fill, idle_clock(), 1);
    let newer = body(S::Aborted, MissionPhase::Idle, idle_clock(), 1);
    // Before any HEALTH, or from another session: not this primary's.
    let mut unheard = Mesh::new(Role::Rrls, 8);
    unheard.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION, 1, &older.encode()),
        1_000,
    );
    assert_eq!(received_for_test(), None);
    rrls.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION ^ 1, 9, &older.encode()),
        1_001,
    );
    assert_eq!(received_for_test(), None);
    // Revision 5, then a delayed revision 4: the older one is dropped.
    rrls.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION, 5, &newer.encode()),
        1_002,
    );
    assert_eq!(received_for_test(), Some(newer.clone()));
    rrls.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION, 4, &older.encode()),
        1_003,
    );
    assert_eq!(received_for_test(), Some(newer.clone()));
    // A corrupt frame is dropped by the RR checksum.
    let mut corrupt = crate::rrls::station_state_frame_for_test(SESSION, 6, &older.encode());
    corrupt[10] ^= 0x40;
    rrls.incoming(true, &corrupt, 1_004);
    assert_eq!(received_for_test(), Some(newer));
    reset_for_test();
}

#[test]
fn a_takeover_rrls_ignores_the_broadcast() {
    let _serial = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset_for_test();
    let (_, mut rrls) = link(false, 1_000);
    rrls.assume_av_bay_heard_for_test();
    // The primary unheard past the takeover wait while the AV bay is heard.
    rrls.tick(1_000 + 7_000);
    assert!(rrls.takeover());
    let state = body(S::Aborted, MissionPhase::Idle, idle_clock(), 1);
    rrls.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION, 1, &state.encode()),
        8_001,
    );
    assert_eq!(received_for_test(), None);
    crate::rrls::set_takeover_for_test(false);
    reset_for_test();
}

#[test]
fn the_primary_capture_follows_its_flight_state_clock_and_phase() {
    let mut record = crate::launch_history::Store::ephemeral().snapshot();
    let phase = MissionPhaseMsg::from_inputs(
        &PhaseInputs {
            mode: PhaseMode::Flight,
            detailed: S::Idle,
            required_boards_seen: true,
            fill_activity: true,
            previous: None,
            fc_contact_lost: false,
        },
        1,
    );
    let before = StationState::from_parts(S::Idle, &phase, Mode::Normal, &record, Some(0b000_111), (0, 0));
    assert_eq!(before.phase, MissionPhase::Fill);
    assert!(before.fill_activity);
    assert_eq!(before.clock_authority, authority_tag(&record.authority));
    // A T-clock reset: the next revision of the same authority.
    record.revision += 1;
    record.clock = LaunchClockMsg::idle();
    let after = StationState::from_parts(S::Idle, &phase, Mode::Normal, &record, Some(0b000_111), (0, 0));
    assert_ne!(before.encode(), after.encode());
    assert_eq!(after.clock_revision, before.clock_revision + 1);
}

/// Runs the calling test again in a child process that is an RRLS (the role
/// is process-wide). Returns true in the child.
fn rrls_child(name: &str) -> bool {
    const CHILD: &str = "GS_STATION_STATE_RRLS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(crate::rrls::is_rrls());
        return true;
    }
    let root = crate::recording_export::tests::TestDirectory::new();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("GS_RRLS_ROLE", "rrls")
        .env("GS_SIM_MODE", "0")
        .env_remove("GS_SIM_CLOCK_FILE")
        .env("GS_NETWORK_VARIABLE_CACHE", root.0.join("variables.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

/// One frame from the primary (listening, then relaying), then what the RRLS
/// presents.
async fn present_each_mode(
    states: &[StationState],
    mut check: impl FnMut(&Arc<AppState>, &StationState, bool),
) {
    for relaying in [false, true] {
        reset_for_test();
        let state = crate::state::tests::test_app_state().await;
        let (mut primary, mut rrls) = link(relaying, 1_000);
        let mut presented = None;
        for (i, station) in states.iter().enumerate() {
            carry(&mut primary, &mut rrls, station, 1_010 + i as u64);
            presented = apply(&state, presented);
            assert_eq!(presented.as_ref(), Some(station), "relaying: {relaying}");
            check(&state, station, relaying);
        }
    }
}

const PREFIX: &str = "station_state::tests::";

#[tokio::test]
async fn an_rrls_shows_a_t_clock_start_hold_and_reset_from_one_frame() {
    if !rrls_child(&format!(
        "{PREFIX}an_rrls_shows_a_t_clock_start_hold_and_reset_from_one_frame"
    )) {
        return;
    }
    let journal = crate::state::tests::test_app_state()
        .await
        .launch_history
        .lock()
        .unwrap()
        .snapshot();
    let tag = authority_tag(&journal.authority);
    let mut states = Vec::new();
    // Countdown started, held (T- with no running anchor), launched, reset.
    for (revision, clock) in [
        (
            1,
            LaunchClockMsg {
                kind: LaunchClockKind::TMinus,
                anchor_timestamp_ms: Some(1_700_000_000_000),
                duration_ms: Some(60_000),
            },
        ),
        (
            2,
            LaunchClockMsg {
                kind: LaunchClockKind::TMinus,
                anchor_timestamp_ms: None,
                duration_ms: Some(42_000),
            },
        ),
        (
            3,
            LaunchClockMsg {
                kind: LaunchClockKind::TPlus,
                anchor_timestamp_ms: Some(1_700_000_060_000),
                duration_ms: None,
            },
        ),
        (4, LaunchClockMsg::idle()),
    ] {
        let mut station = body(S::Idle, MissionPhase::Idle, clock, revision);
        station.clock_authority = 0x0bad_cafe ^ tag;
        states.push(station);
    }
    present_each_mode(&states, |state, station, _| {
        assert_eq!(state.launch_clock_snapshot(), station.clock);
        // The RRLS's clock journal stays on the signed sync.
        assert_eq!(state.launch_history.lock().unwrap().snapshot().revision, 0);
    })
    .await;
    // A slower signed reply with an older revision of the same authority
    // does not replace the broadcast clock on screen.
    let mut older = journal.clone();
    older.authority = format!("{:08x}{}", 0x0bad_cafe ^ tag, &journal.authority[8..]);
    older.revision = 3;
    assert!(clock_newer_than(&older));
    older.revision = 4;
    assert!(!clock_newer_than(&older));
}

#[tokio::test]
async fn an_rrls_shows_the_primarys_station_mode_from_one_frame() {
    if !rrls_child(&format!(
        "{PREFIX}an_rrls_shows_the_primarys_station_mode_from_one_frame"
    )) {
        return;
    }
    let mut states = Vec::new();
    for mode in [Mode::TestFire, Mode::Hitl, Mode::Normal] {
        let mut station = body(S::Idle, MissionPhase::Idle, idle_clock(), 0);
        station.mode = mode;
        states.push(station);
    }
    present_each_mode(&states, |_, station, _| {
        let snapshot = crate::station_mode::snapshot();
        assert_eq!(snapshot.effective_mode, Some(station.mode));
        assert_eq!(
            snapshot.local_capabilities_match,
            station.mode == crate::station_mode::local_mode()
        );
        let verified = crate::station_mode::verified_mode_for_test();
        if snapshot.effective_mode != verified {
            // Shown at once but not synchronized until the mode sync verifies
            // it; command capability waits for that.
            assert!(!snapshot.synchronized);
            if verified.is_some() {
                assert!(crate::station_mode::reverify_requested_for_test());
            }
        }
    })
    .await;
}

#[tokio::test]
async fn an_rrls_shows_each_flight_state_change_from_one_frame() {
    if !rrls_child(&format!(
        "{PREFIX}an_rrls_shows_each_flight_state_change_from_one_frame"
    )) {
        return;
    }
    // Advance, Rewind, Launch, Abort, Reset flight state to Idle, Reset all
    // boards (Startup).
    let states = [
        (S::PreFill, MissionPhase::Fill),
        (S::Idle, MissionPhase::Idle),
        (S::Launch, MissionPhase::Launch),
        (S::Aborted, MissionPhase::Idle),
        (S::Idle, MissionPhase::Idle),
        (S::Startup, MissionPhase::Startup),
    ]
    .map(|(flight, phase)| body(flight, phase, idle_clock(), 0));
    let mut ui = None;
    present_each_mode(&states, |state, station, _| {
        let ui = ui.get_or_insert_with(|| state.state_tx.subscribe());
        // The websocket's FlightState shows the primary's state...
        let wire = crate::web::FlightStateWire::new(state.local_flight_state_snapshot(), state);
        assert_eq!(wire.state, station.flight_state.network());
        while let Ok(msg) = ui.try_recv() {
            assert_eq!(
                crate::web::FlightStateWire::new(msg.state, state).state,
                station.flight_state.network()
            );
        }
        // ...and the RRLS's own flight state is not written.
        assert_eq!(state.local_flight_state_snapshot(), S::Startup);
    })
    .await;
}

#[tokio::test]
async fn an_rrls_shows_the_primarys_mission_phase_from_one_frame() {
    if !rrls_child(&format!(
        "{PREFIX}an_rrls_shows_the_primarys_mission_phase_from_one_frame"
    )) {
        return;
    }
    // GSE fill work while the detailed state is Idle is Fill on the primary;
    // the RRLS has seen none of the fill boards and would show Startup.
    let mut fill = body(S::Idle, MissionPhase::Fill, idle_clock(), 0);
    fill.fill_activity = true;
    let flight = body(S::InFlight, MissionPhase::InFlight, idle_clock(), 0);
    let mut landed = body(S::InFlight, MissionPhase::Landed, idle_clock(), 0);
    landed.fc_contact_lost = true;
    present_each_mode(&[fill, flight, landed], |_, station, _| {
        let msg = crate::mission_phase::subscribe().borrow().clone();
        assert_eq!(msg.phase, station.phase);
        assert_eq!(msg.detailed, station.flight_state.network());
        if station.phase == MissionPhase::Fill {
            assert_eq!(
                msg.detail.as_deref(),
                Some(crate::mission_phase::DETAIL_GSE_FILL_ACTIVE)
            );
        }
    })
    .await;
}

#[tokio::test]
async fn an_rrls_in_takeover_or_without_the_primary_shows_its_own_state() {
    if !rrls_child(&format!(
        "{PREFIX}an_rrls_in_takeover_or_without_the_primary_shows_its_own_state"
    )) {
        return;
    }
    let state = crate::state::tests::test_app_state().await;
    let (mut primary, mut rrls) = link(false, 1_000);
    let mut station = body(S::Armed, MissionPhase::Fill, idle_clock(), 0);
    station.mode = Mode::TestFire;
    carry(&mut primary, &mut rrls, &station, 1_010);
    let presented = apply(&state, None);
    assert_eq!(presented, Some(station));
    crate::rrls::set_takeover_for_test(true);
    assert_eq!(apply(&state, presented), None);
    assert_eq!(
        crate::web::FlightStateWire::new(state.local_flight_state_snapshot(), &state).state,
        S::Startup.network()
    );
    assert_ne!(
        crate::station_mode::snapshot().effective_mode,
        Some(Mode::TestFire)
    );
    assert_ne!(
        crate::mission_phase::subscribe().borrow().phase,
        MissionPhase::Fill
    );
    crate::rrls::set_takeover_for_test(false);
}
