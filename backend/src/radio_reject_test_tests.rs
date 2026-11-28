//! The radio test mode: the radio driver's filters per variant, the RRLS link
//! kept or dropped, the relay and the takeover it causes, its gates, expiry,
//! restart, Abort handling and the banner/broadcast state.
use super::*;
use crate::rrls::{Mesh, Role, rr_frame_for_test};
use crate::types::FlightState as S;

const SESSION: u32 = 0x5eed_0001;
const WINDOW_AD: &[u8] = b"RS\x01\0\0\0\0\0\0";
const SDK: &[u8] = b"\x01\x02sdk-frame";
// RR kinds (rrls.rs).
const HEALTH: u8 = 0;
const DOWN_DATA: u8 = 1;
const UP_DATA: u8 = 2;
const UP_COMMAND: u8 = 4;
const AV_BAY_DATA: u8 = 5;
const AV_BAY_COMMAND: u8 = 6;
const READY: u8 = 7;
const SETTINGS: u8 = 10;
const HOST_CONTROL: u8 = 11;
const BUDGET: u8 = 13;
const RF_PROFILE: u8 = 14;
const STATION_STATE: u8 = 16;

fn admin() -> Actor {
    Actor {
        name: "admin".into(),
        may_start: true,
        may_stop: true,
    }
}
fn operator() -> Actor {
    Actor {
        name: "op".into(),
        may_start: false,
        may_stop: true,
    }
}
fn idle(relay_engaged: bool) -> Inputs {
    Inputs {
        flight_state: S::Idle,
        abort_confirmed: false,
        relay_engaged,
    }
}
fn started(variant: Variant, now: u64) -> Controller {
    let mut c = Controller::default();
    c.start(
        &admin(),
        variant,
        None,
        true,
        S::Idle,
        Ok(()),
        now,
        1_000_000 + now,
    )
    .unwrap();
    c
}
fn stops(c: &mut Controller) -> Vec<(StopReason, bool)> {
    let events = c.take_events();
    let direct = events.contains(&Event::GoDirect);
    events
        .into_iter()
        .filter_map(|e| match e {
            Event::Stopped(r) => r.reason.map(|reason| (reason, direct)),
            _ => None,
        })
        .collect()
}
fn kind_of(frame: &[u8]) -> Option<u8> {
    crate::rrls::kind_of(frame)
}

#[test]
fn force_relay_drops_the_av_bay_and_keeps_every_rrls_frame() {
    let f = FILTER_RELAY;
    // Received: bare SDK frames and RF window advertisements (the AV bay),
    // and the RF board's kind-14 profile replies.
    assert!(ingress_rejected(f, SDK));
    assert!(ingress_rejected(f, WINDOW_AD));
    assert!(ingress_rejected(f, &rr_frame_for_test(RF_PROFILE, &[1, 1])));
    for kind in [
        HEALTH,
        DOWN_DATA,
        READY,
        SETTINGS,
        HOST_CONTROL,
        BUDGET,
        STATION_STATE,
    ] {
        assert!(
            !ingress_rejected(f, &rr_frame_for_test(kind, b"x")),
            "RRLS kind {kind} kept"
        );
    }
    // Transmitted: whatever the RF board accepts (kinds 5, 6, 14, bare).
    for frame in [
        rr_frame_for_test(AV_BAY_DATA, SDK),
        rr_frame_for_test(AV_BAY_COMMAND, b"RS"),
        rr_frame_for_test(RF_PROFILE, &[1, 0]),
        SDK.to_vec(),
    ] {
        assert!(egress_rejected(f, &frame), "{frame:?}");
    }
    for kind in [
        HEALTH,
        UP_DATA,
        UP_COMMAND,
        SETTINGS,
        HOST_CONTROL,
        BUDGET,
        STATION_STATE,
    ] {
        assert!(
            !egress_rejected(f, &rr_frame_for_test(kind, b"x")),
            "RRLS kind {kind} kept"
        );
    }
}

#[test]
fn silent_primary_drops_everything_and_off_drops_nothing() {
    let frames = [
        SDK.to_vec(),
        WINDOW_AD.to_vec(),
        rr_frame_for_test(HEALTH, b"x"),
        rr_frame_for_test(READY, b"x"),
        rr_frame_for_test(HOST_CONTROL, b"x"),
        rr_frame_for_test(UP_DATA, b"x"),
        rr_frame_for_test(AV_BAY_DATA, b"x"),
    ];
    for frame in &frames {
        assert!(ingress_rejected(FILTER_SILENT, frame) && egress_rejected(FILTER_SILENT, frame));
        assert!(!ingress_rejected(FILTER_OFF, frame) && !egress_rejected(FILTER_OFF, frame));
    }
}

/// Moves every frame `from` would transmit now to `to`, through the
/// primary's filter (egress when the primary sends, ingress when it hears).
fn pump(from: &mut Mesh, to: &mut Mesh, filter: u8, primary_sends: bool, now: u64) -> Vec<Vec<u8>> {
    let mut sent = Vec::new();
    while let Some(frame) = from.begin_any_for_test(now) {
        let lost = if primary_sends {
            egress_rejected(filter, &frame)
        } else {
            ingress_rejected(filter, &frame)
        };
        if !lost {
            to.incoming(true, &frame, now);
        }
        sent.push(frame);
    }
    sent
}

/// The primary in force relay (from 0) next to an RRLS that hears the AV bay
/// throughout. Returns when the primary asked for the relay, and when the
/// relay carried its uplink.
fn run_force_relay(filter_on: bool) -> (Option<u64>, Option<u64>, Mesh, Mesh, u64) {
    let mut c = Controller::default();
    if filter_on {
        c.start(
            &admin(),
            Variant::ForceRelay,
            None,
            true,
            S::Idle,
            Ok(()),
            0,
            0,
        )
        .unwrap();
    }
    let mut p = Mesh::new(Role::Primary, SESSION);
    p.assume_listening_since_boot_for_test();
    let mut r = Mesh::new(Role::Rrls, 9);
    r.assume_av_bay_heard_for_test();
    let (mut requested, mut engaged) = (None, None);
    let mut now = 0;
    while now <= 12_000 {
        c.tick(idle(p.active(now)), now, now);
        let f = c.filter(now);
        // The AV bay's window advertisement reaches both radios.
        if !ingress_rejected(f, WINDOW_AD) {
            p.incoming(true, WINDOW_AD, now);
        }
        r.incoming(true, WINDOW_AD, now);
        p.tick(now);
        r.tick(now);
        pump(&mut p, &mut r, f, true, now);
        pump(&mut r, &mut p, f, false, now);
        if requested.is_none() && p.relay_decision(now).relay_requested {
            requested = Some(now);
        }
        if engaged.is_none() && p.active(now) {
            engaged = Some(now);
        }
        now += 250;
    }
    (requested, engaged, p, r, now)
}

#[test]
fn force_relay_makes_the_primary_ask_for_the_relay_and_the_relay_engages() {
    let (requested, engaged, mut p, mut r, now) = run_force_relay(true);
    assert_eq!(
        requested,
        Some(3_000),
        "the usual LOSS_MS after the last direct frame"
    );
    let engaged = engaged.expect("the RRLS link is kept, so READY arrives and the relay engages");
    assert!(engaged <= 4_000, "{engaged}");
    assert!(r.active(now), "the RRLS relays");
    // Uplink: wrapped for the relay, kept by the filter, sent on by the RRLS.
    let up = p.local_tx(false, SDK, now).unwrap();
    assert_eq!(kind_of(&up.bytes), Some(UP_DATA));
    assert!(!egress_rejected(FILTER_RELAY, &up.bytes));
    r.incoming(true, &up.bytes, now);
    let relayed = std::iter::from_fn(|| r.begin_any_for_test(now)).collect::<Vec<_>>();
    assert!(
        relayed
            .iter()
            .any(|f| kind_of(f) == Some(AV_BAY_DATA) && f.windows(SDK.len()).any(|w| w == SDK)),
        "{relayed:?}"
    );
    // Downlink: the RRLS forwards what it hears; the primary takes it.
    r.incoming(false, SDK, now);
    let down = std::iter::from_fn(|| r.begin_any_for_test(now))
        .find(|f| kind_of(f) == Some(DOWN_DATA))
        .unwrap();
    assert!(!ingress_rejected(FILTER_RELAY, &down));
    assert_eq!(
        p.incoming(true, &down, now).map(|f| f.bytes),
        Some(SDK.to_vec())
    );

    // Control: the same run without the test never asks for the relay.
    let (requested, engaged, ..) = run_force_relay(false);
    assert_eq!((requested, engaged), (None, None));
}

#[test]
fn force_relay_before_the_relay_engages_sends_the_av_bay_nothing() {
    let mut p = Mesh::new(Role::Primary, SESSION);
    p.assume_listening_since_boot_for_test();
    p.tick(3_000);
    assert!(p.relay_decision(3_000).relay_requested && !p.active(3_000));
    let direct = p.local_tx(false, SDK, 3_000).unwrap();
    assert_eq!(kind_of(&direct.bytes), Some(AV_BAY_DATA));
    assert!(
        egress_rejected(FILTER_RELAY, &direct.bytes),
        "lost: no READY yet, and the AV bay is not to hear it"
    );
}

#[test]
fn the_abort_escape_returns_the_primary_to_its_direct_path_at_once() {
    let (_, _, mut p, _, now) = run_force_relay(true);
    assert!(p.active(now));
    p.force_direct(now);
    let decision = p.relay_decision(now);
    assert!(!decision.relay_requested, "{decision:?}");
    assert!(decision.reason.unwrap().contains("radio test"));
    let direct = p.local_tx(false, SDK, now).unwrap();
    assert_eq!(
        kind_of(&direct.bytes),
        Some(AV_BAY_DATA),
        "the Abort's retries go direct"
    );
    // It stays direct for the Abort's retries even with no direct frame yet.
    p.tick(now + 9_000);
    assert!(!p.relay_decision(now + 9_000).relay_requested);
}

fn station_state(code: u8, minutes: u8) -> Vec<u8> {
    crate::station_state::StationState {
        flight_state: S::Idle,
        phase: crate::mission_phase::MissionPhase::Idle,
        required_boards_seen: true,
        fill_activity: false,
        fc_contact_lost: false,
        mode: crate::station_mode::Mode::Normal,
        clock: crate::telemetry_db::LaunchClockMsg::idle(),
        clock_revision: 0,
        clock_authority: 0,
        netvar: None,
        radio_test: (code, minutes),
    }
    .encode()
    .to_vec()
}

#[test]
fn silent_primary_makes_the_rrls_take_over_and_turning_it_off_hands_back() {
    observe_primary(0, 0);
    let mut c = Controller::default();
    let mut p = Mesh::new(Role::Primary, SESSION);
    let mut r = Mesh::new(Role::Rrls, 9);
    let (mut took, mut handed, mut banner_in_takeover) = (None, None, false);
    let mut now = 0;
    while now <= 45_000 {
        if now == 10_000 {
            c.start(
                &admin(),
                Variant::SilentPrimary,
                None,
                true,
                S::Idle,
                Ok(()),
                now,
                now,
            )
            .unwrap();
        }
        if now == 30_000 {
            c.stop(StopReason::Operator, Some("admin"), now);
        }
        c.tick(idle(false), now, now);
        let f = c.filter(now);
        // The test announces itself before the silence (STATION_STATE).
        p.publish_station_state(
            station_state(c.wire(now).0, c.wire(now).1)
                .try_into()
                .unwrap(),
            now,
        );
        while let Some(frame) = p.take_station_state_for_test() {
            if !egress_rejected(f, &frame) {
                r.incoming(true, &frame, now);
            }
        }
        if now % 500 == 0 {
            let health = p.health_frame_for_test(true);
            if !egress_rejected(f, &health) {
                r.incoming(true, &health, now);
            }
        }
        r.incoming(true, WINDOW_AD, now);
        r.tick(now);
        if took.is_none() && r.takeover() {
            took = Some(now);
        }
        if took.is_some() && r.takeover() && primary_view().is_some() {
            banner_in_takeover = true;
        }
        if took.is_some() && handed.is_none() && !r.takeover() {
            handed = Some(now);
        }
        now += 250;
    }
    // Silent from 11.5 s (SILENT_ARM_MS); last HEALTH at 11.0 s; TAKEOVER_MS 6 s.
    assert_eq!(took, Some(17_000));
    assert!(
        banner_in_takeover,
        "the RRLS shows the primary's announced test through the takeover"
    );
    // HEALTH again from 30 s; RESTORE_MS 5 s with an AV-bay route.
    assert_eq!(handed, Some(35_000));
    assert!(
        primary_view().is_none(),
        "the primary's next broadcast ends the banner"
    );
}

#[test]
fn starting_is_refused_without_permission_confirmation_ground_state_or_radio() {
    let mut c = Controller::default();
    let go = |c: &mut Controller,
              actor: &Actor,
              confirm: bool,
              fs: S,
              radio: Result<(), String>,
              duration: Option<u64>| {
        c.start(
            actor,
            Variant::ForceRelay,
            duration,
            confirm,
            fs,
            radio,
            0,
            0,
        )
        .map_err(|r| r.0)
    };
    assert_eq!(
        go(&mut c, &operator(), true, S::Idle, Ok(()), None).unwrap_err(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        go(&mut c, &admin(), false, S::Idle, Ok(()), None).unwrap_err(),
        StatusCode::BAD_REQUEST
    );
    for fs in [
        S::FillTest,
        S::NitrousFill,
        S::Armed,
        S::Launch,
        S::InFlight,
        S::Ascent,
        S::Landed,
        S::Aborted,
    ] {
        assert_eq!(
            go(&mut c, &admin(), true, fs, Ok(()), None).unwrap_err(),
            StatusCode::CONFLICT,
            "{fs:?}"
        );
    }
    assert_eq!(
        go(&mut c, &admin(), true, S::Idle, Err("no mesh".into()), None).unwrap_err(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        go(&mut c, &admin(), true, S::Idle, Ok(()), Some(10)).unwrap_err(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        go(&mut c, &admin(), true, S::Idle, Ok(()), Some(7_200)).unwrap_err(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(c.filter(0), FILTER_OFF);
    assert!(c.take_events().is_empty());
    for fs in [S::Startup, S::Idle, S::PreFill] {
        let mut c = Controller::default();
        assert!(
            go(&mut c, &admin(), true, fs, Ok(()), None).is_ok(),
            "{fs:?}"
        );
        assert_eq!(c.filter(0), FILTER_RELAY);
        assert_eq!(
            go(&mut c, &admin(), true, fs, Ok(()), None).unwrap_err(),
            StatusCode::CONFLICT,
            "already on"
        );
    }
}

#[test]
fn it_ends_by_itself_when_the_flight_state_advances_and_when_it_times_out() {
    let mut c = started(Variant::ForceRelay, 0);
    c.tick(
        Inputs {
            flight_state: S::PreFill,
            ..idle(false)
        },
        1_000,
        0,
    );
    assert_eq!(c.filter(1_000), FILTER_RELAY);
    c.tick(
        Inputs {
            flight_state: S::Armed,
            ..idle(false)
        },
        2_000,
        0,
    );
    assert_eq!(c.filter(2_000), FILTER_OFF);
    assert_eq!(stops(&mut c), [(StopReason::FlightState, false)]);

    let mut c = started(Variant::SilentPrimary, 0);
    let end = DEFAULT_DURATION_S * 1_000;
    c.tick(idle(false), end - 1, 0);
    assert_eq!(c.filter(end - 1), FILTER_SILENT);
    c.tick(idle(false), end, 0);
    assert_eq!(c.filter(end), FILTER_OFF);
    assert_eq!(stops(&mut c), [(StopReason::Expired, false)]);
}

#[test]
fn silent_primary_transmits_until_its_announcement_is_out() {
    let c = started(Variant::SilentPrimary, 5_000);
    assert_eq!(c.filter(5_000), FILTER_OFF);
    assert_eq!(c.filter(5_000 + SILENT_ARM_MS - 1), FILTER_OFF);
    assert_eq!(c.filter(5_000 + SILENT_ARM_MS), FILTER_SILENT);
    assert_eq!(c.wire(5_000), (2, 10), "announced from the start");
}

#[test]
fn an_abort_lifts_a_silent_primary_at_once() {
    let mut c = started(Variant::SilentPrimary, 0);
    c.tick(idle(false), 2_000, 0);
    assert_eq!(c.filter(2_000), FILTER_SILENT);
    c.note_abort(2_100, 0);
    assert_eq!(c.filter(2_100), FILTER_OFF);
    assert_eq!(stops(&mut c), [(StopReason::AbortLift, false)]);
}

#[test]
fn a_force_relay_abort_goes_through_the_relay_and_escapes_direct_if_unconfirmed() {
    // Confirmed through the relay in time: the test ends, no escape.
    let mut c = started(Variant::ForceRelay, 0);
    c.tick(idle(true), 1_000, 0);
    c.note_abort(1_000, 0);
    assert_eq!(
        c.filter(1_000),
        FILTER_RELAY,
        "the Abort goes through the relay"
    );
    // The station's own Aborted state does not end the test early.
    c.tick(
        Inputs {
            flight_state: S::Aborted,
            abort_confirmed: false,
            relay_engaged: true,
        },
        3_000,
        0,
    );
    assert_eq!(c.filter(3_000), FILTER_RELAY);
    c.tick(
        Inputs {
            flight_state: S::Aborted,
            abort_confirmed: true,
            relay_engaged: true,
        },
        3_500,
        0,
    );
    assert_eq!(c.filter(3_500), FILTER_OFF);
    assert_eq!(stops(&mut c), [(StopReason::AbortRelayed, false)]);

    // Not confirmed within ABORT_ESCAPE_MS: lifted, and back to direct.
    let mut c = started(Variant::ForceRelay, 0);
    c.tick(idle(true), 1_000, 0);
    c.note_abort(1_000, 0);
    c.tick(
        Inputs {
            flight_state: S::Aborted,
            abort_confirmed: false,
            relay_engaged: true,
        },
        1_000 + ABORT_ESCAPE_MS - 100,
        0,
    );
    assert_eq!(c.filter(1_000 + ABORT_ESCAPE_MS - 100), FILTER_RELAY);
    c.tick(
        Inputs {
            flight_state: S::Aborted,
            abort_confirmed: false,
            relay_engaged: true,
        },
        1_000 + ABORT_ESCAPE_MS,
        0,
    );
    assert_eq!(c.filter(1_000 + ABORT_ESCAPE_MS), FILTER_OFF);
    assert_eq!(stops(&mut c), [(StopReason::AbortEscape, true)]);

    // No relay carrying the uplink: nothing to test, lifted at once.
    let mut c = started(Variant::ForceRelay, 0);
    c.tick(idle(false), 1_000, 0);
    c.note_abort(1_000, 0);
    assert_eq!(c.filter(1_000), FILTER_OFF);
    assert_eq!(stops(&mut c), [(StopReason::AbortEscape, true)]);
    assert!(
        ABORT_ESCAPE_MS < 4_800,
        "inside the ABORT's reliable delivery window"
    );
}

#[test]
fn every_abort_request_reaches_the_test_before_it_is_published() {
    with_controller(|c| {
        *c = Controller::default();
        c.start(
            &admin(),
            Variant::ForceRelay,
            None,
            true,
            S::Idle,
            Ok(()),
            now_ms(),
            0,
        )
        .unwrap();
        sync_filter(c, now_ms());
        c.take_events();
    });
    assert_eq!(active_filter(), FILTER_RELAY);
    let intent = crate::manual_abort_intent::ManualAbortIntent::default();
    intent.request();
    assert_eq!(
        active_filter(),
        FILTER_OFF,
        "no relay up: lifted before the Abort is sent"
    );
    let events = with_controller(|c| c.take_events());
    assert!(events.contains(&Event::GoDirect), "{events:?}");
    // Off: an Abort changes nothing.
    intent.request();
    assert!(with_controller(|c| c.take_events()).is_empty());
}

#[test]
fn the_admit_functions_follow_this_threads_filter() {
    with_filter(|f| f.store(FILTER_RELAY, Ordering::Release));
    assert!(!admit_rx(SDK) && admit_rx(&rr_frame_for_test(HEALTH, b"x")));
    assert!(
        !admit_tx(&rr_frame_for_test(AV_BAY_DATA, SDK))
            && admit_tx(&rr_frame_for_test(HEALTH, b"x"))
    );
    with_filter(|f| f.store(FILTER_OFF, Ordering::Release));
    assert!(admit_rx(SDK) && admit_tx(SDK));
}

#[test]
fn a_restart_turns_it_off_and_closes_the_journal_entry() {
    let dir = std::env::temp_dir().join(format!(
        "gs-radio-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(JOURNAL_FILE);
    let _ = std::fs::remove_file(&path);
    let mut c = started(Variant::ForceRelay, 0);
    for event in c.take_events() {
        if let Event::Started(record) = event {
            append_record(&path, &record).unwrap();
        }
    }
    // The process ends here. Nothing is persisted: a new station starts off.
    let fresh = Controller::default();
    assert_eq!((fresh.variant(), fresh.filter(0)), (None, FILTER_OFF));
    let closed = close_dangling_start(&path, 2_000_000).expect("the open start is closed");
    assert_eq!(
        (closed.reason, closed.utc_ms, closed.started_utc_ms),
        (Some(StopReason::Restart), 2_000_000, 1_000_000)
    );
    assert!(
        close_dangling_start(&path, 3_000_000).is_none(),
        "already closed"
    );
    let history = read_history(&path);
    assert_eq!(
        history.iter().map(|r| r.event.as_str()).collect::<Vec<_>>(),
        ["stop", "start"]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_status_carries_the_banner_variant_remaining_time_and_history() {
    let mut c = started(Variant::ForceRelay, 0);
    let view = c.view(61_000, 5_000_000);
    assert_eq!(view["banner"], format!("{BANNER} (Force relay)"));
    assert_eq!(view["active"]["variant"], "force_relay");
    assert_eq!(view["active"]["remaining_s"], 539);
    assert_eq!(view["active"]["expires_utc_ms"], 5_539_000);
    assert_eq!(c.wire(61_000), (1, 9));
    c.stop(StopReason::Operator, Some("admin"), 1_100_000);
    let view = c.view(70_000, 0);
    assert!(view["active"].is_null() && view["banner"].is_null());
    assert_eq!(view["history"][0]["reason"], "operator");
    assert_eq!(view["history"][0]["started_utc_ms"], 1_000_000);
    assert_eq!(view["history"][1]["event"], "start");
    assert_eq!(c.wire(70_000), (0, 0));
}

#[test]
fn the_station_state_broadcast_carries_it_to_the_rrls() {
    use crate::station_state::StationState;
    let body = station_state(Variant::SilentPrimary.code(), 7);
    assert_eq!(StationState::decode(&body).unwrap().radio_test, (2, 7));
    let mut bad = body.clone();
    bad[30] = 9;
    assert!(StationState::decode(&bad).is_none(), "unknown variant");
    observe_primary(0, 0);
    let mut p = Mesh::new(Role::Primary, SESSION);
    let mut r = Mesh::new(Role::Rrls, 9);
    r.incoming(true, &p.health_frame_for_test(true), 0);
    r.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION, 1, &station_state(1, 10)),
        10,
    );
    let view = primary_view().expect("shown on the RRLS");
    assert_eq!(view["variant"], "force_relay");
    assert!(view["banner"].as_str().unwrap().starts_with(BANNER));
    // A foreign session never sets it; the primary's next copy clears it.
    r.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION ^ 1, 2, &station_state(0, 0)),
        20,
    );
    assert!(primary_view().is_some());
    r.incoming(
        true,
        &crate::rrls::station_state_frame_for_test(SESSION, 2, &station_state(0, 0)),
        30,
    );
    assert!(primary_view().is_none());
}
