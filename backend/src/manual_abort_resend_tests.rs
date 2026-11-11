use super::*;
use sedsnet::router::{EndpointHandler, RouterConfig};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// GS and RF on one radio side, on a shared stepped clock and the station's
/// delivery tuning. RF's radio is deaf (modem in command mode) from the
/// Abort until `deaf_ms`. Returns the ms after the Abort at which RF first
/// received an ABORT (None: never within `run_ms`), and the ABORT packets the
/// station re-sent.
fn abort_across_a_deaf_rf(resend: bool, deaf_ms: u64, run_ms: u64) -> (Option<u64>, u64) {
    abort_across_rf(resend, deaf_ms, run_ms, false)
}

/// `readback_never`: the AV bay never reads back Aborted (its readback is
/// stuck elsewhere) even though RF acknowledged the ABORT.
fn abort_across_rf(resend: bool, deaf_ms: u64, run_ms: u64, readback_never: bool) -> (Option<u64>, u64) {
    let _serial = crate::RUNTIME_TUNING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let previous = sedsnet::config::runtime_tuning_config();
    sedsnet::config::set_runtime_tuning_config(crate::network_delivery_tuning()).unwrap();
    crate::telemetry_schema::initialize().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = |n: &Arc<AtomicU64>| {
        let n = n.clone();
        Box::new(move || n.load(Ordering::Relaxed)) as Box<dyn sedsnet::router::Clock + Send + Sync>
    };
    let gs = Arc::new(Router::new_with_clock(RouterConfig::new([]).with_sender("GS"), clock(&now)));
    let got = Arc::new(Mutex::new(None::<u64>));
    let (seen, at) = (got.clone(), now.clone());
    let rf = Arc::new(Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"), move |p| {
            if p.data_type() == crate::telemetry_schema::data_type("ABORT") {
                seen.lock().unwrap().get_or_insert(at.load(Ordering::Relaxed));
            }
            Ok(())
        })])
        .with_sender("RF"),
        clock(&now),
    ));
    let (to_rf, to_gs): (Arc<Mutex<Vec<Vec<u8>>>>, Arc<Mutex<Vec<Vec<u8>>>>) = Default::default();
    let link: crate::comms_config::CommsLinkConfig =
        serde_json::from_value(serde_json::json!({"interface": "serial", "port": "/dev/test-radio", "protocol": "raw_uart"})).unwrap();
    let w = to_rf.clone();
    let gs_side = gs.add_side_packed_with_options("rocket_comms", move |b| { w.lock().unwrap().push(b.to_vec()); Ok(()) }, crate::rocket_router_side_options(&link, false));
    let w = to_gs.clone();
    let rf_side = rf.add_side_packed_with_options("radio", move |b| { w.lock().unwrap().push(b.to_vec()); Ok(()) }, crate::rocket_router_side_options(&link, false));
    let deaf_until = Arc::new(AtomicU64::new(0));
    let pump = |steps: usize| {
        for _ in 0..steps {
            now.fetch_add(10, Ordering::Relaxed);
            let _ = gs.process_all_queues();
            let _ = rf.process_all_queues();
            let deaf = now.load(Ordering::Relaxed) < deaf_until.load(Ordering::Relaxed);
            for b in std::mem::take(&mut *to_rf.lock().unwrap()) {
                if !deaf { let _ = rf.rx_packed_from_side(&b, rf_side); }
            }
            for b in std::mem::take(&mut *to_gs.lock().unwrap()) {
                if !deaf { let _ = gs.rx_packed_from_side(&b, gs_side); }
            }
        }
    };
    gs.announce_discovery().unwrap();
    rf.announce_discovery().unwrap();
    pump(100);
    let abort_at = now.load(Ordering::Relaxed);
    deaf_until.store(abort_at + deaf_ms, Ordering::Relaxed);
    let resender = AbortResend::default();
    crate::telemetry_task::queue_manual_abort_wire_receipted(&gs, WirePolicy::Untimed).unwrap();
    resender.arm(1, WirePolicy::Untimed, None, SendMark { av_bay_routed: av_bay_abort_route(&gs), ..SendMark::default() });
    let mut sent = 0u64;
    while now.load(Ordering::Relaxed) < abort_at + run_ms {
        pump(5);
        let owed = if got.lock().unwrap().is_some() && !readback_never { 0 } else { 0b111 };
        if resend
            && resender
                .step(&gs, Observation { now_ms: now.load(Ordering::Relaxed), epoch: 1, station_aborted: true, recovery: None, owed, unheard: 0 })
                .is_some()
        {
            sent += 1;
        }
    }
    sedsnet::config::set_runtime_tuning_config(previous).unwrap();
    let latched = got.lock().unwrap().map(|t| t - abort_at);
    (latched, sent)
}

/// RF's modem check holds the radio for 10 s across an operator Abort. The
/// ABORT's 4.8 s delivery window runs out while RF is deaf. FC and PB (behind
/// RF) must still get the Abort within the hold plus about one retry
/// interval, not only when a later recovery query reaches them.
#[test]
fn operator_abort_reaches_the_av_bay_after_a_ten_second_rf_hold() {
    let retry = u64::from(crate::network_delivery_tuning().reliable_retransmit_ms);
    let (latched, sent) = abort_across_a_deaf_rf(true, 10_000, 20_000);
    eprintln!("ABORT_ACROSS_RF_HOLD latched_ms={latched:?} resends={sent}");
    let ms = latched.unwrap_or_else(|| panic!("the Abort never reached RF ({sent} sent)"));
    assert!(ms <= 10_000 + 2 * retry, "Abort reached RF {ms} ms after it was sent");
    // One in flight at a time: a re-send only when the previous one's 4.8 s
    // window has run out (at about 4.8 s and 9.6 s here).
    assert!((1..=2).contains(&sent), "no stacking: {sent} re-sends in 20 s");
}

/// Without the re-send the same Abort is lost for good.
#[test]
fn without_a_resend_the_abort_is_lost_across_the_hold() {
    let (latched, _) = abort_across_a_deaf_rf(false, 10_000, 20_000);
    assert_eq!(latched, None);
}

/// Once every AV-bay board reads Aborted, or the station leaves Aborted, or a
/// recovery starts, nothing more is sent.
#[test]
fn resend_stops_on_readback_reset_or_recovery() {
    crate::telemetry_schema::initialize().unwrap();
    let gs = Router::new(RouterConfig::new([]).with_sender("GS"));
    let seen = |owed, station_aborted, recovery: Option<&str>| Observation {
        now_ms: 100_000, epoch: 1, station_aborted, recovery: recovery.map(str::to_string), owed, unheard: 0,
    };
    for (owed, aborted, recovery) in [(0u8, true, None), (0b010, false, None), (0b010, true, Some("operation 7"))] {
        let resender = AbortResend::default();
        resender.arm(1, WirePolicy::Untimed, None, routed_mark());
        assert_eq!(resender.step(&gs, seen(owed, aborted, recovery)), None);
        assert_eq!(resender.active_resends(), None, "disarmed for ({owed:#b}, {aborted}, {recovery:?})");
    }
    // A newer operator Abort epoch also disarms the old one.
    let resender = AbortResend::default();
    resender.arm(1, WirePolicy::Untimed, None, routed_mark());
    assert_eq!(resender.step(&gs, Observation { epoch: 2, ..seen(0b010, true, None) }), None);
    assert_eq!(resender.active_resends(), None);
    // Umbilical-only owed boards never trigger a re-send.
    let resender = AbortResend::default();
    resender.arm(1, WirePolicy::Untimed, None, routed_mark());
    assert_eq!(resender.step(&gs, seen(0b1111000, true, None)), None);
    assert_eq!(resender.active_resends(), None);
}

/// deploy-hitl-a1: the ABORT reached and was acknowledged by the AV bay at
/// once, but RF and PB read back late (their readback queries waited on the
/// radio). The re-send fired every second for 20 s; each copy re-latched the
/// umbilical boards and held the radio's capacity. An acknowledged ABORT is
/// never re-sent, however late the readback.
#[test]
fn an_acknowledged_abort_is_not_resent_for_a_late_readback() {
    let (latched, sent) = abort_across_rf(true, 0, 20_000, true);
    assert!(latched.is_some_and(|ms| ms < 500), "RF got it at once: {latched:?}");
    assert_eq!(sent, 0, "no re-send of an acknowledged ABORT");
}

/// The station on two sides: the radio to the AV bay ("RF", which owns the
/// ABORT endpoint here) and the umbilical ("GB"). The radio is dead from the
/// start of the run for `outage_ms` (longer than the 30 s discovery TTL, as in
/// a takeover or any long radio outage). The operator Aborts `abort_after_ms`
/// into the outage. `unheard_until_ms` (after the Abort): the AV bay is not
/// heard, so it neither owes nor confirms the Abort. Returns when, after the
/// Abort, RF and GB first received an ABORT.
fn abort_across_a_radio_outage(outage_ms: u64, abort_after_ms: u64, unheard_until_ms: u64, run_ms: u64) -> (Option<u64>, Option<u64>) {
    let _serial = crate::RUNTIME_TUNING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let previous = sedsnet::config::runtime_tuning_config();
    sedsnet::config::set_runtime_tuning_config(crate::network_delivery_tuning()).unwrap();
    crate::telemetry_schema::initialize().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = |n: &Arc<AtomicU64>| {
        let n = n.clone();
        Box::new(move || n.load(Ordering::Relaxed)) as Box<dyn sedsnet::router::Clock + Send + Sync>
    };
    let abort = crate::telemetry_schema::data_type("ABORT");
    let board = |name: &str, got: &Arc<Mutex<Option<u64>>>| {
        let (seen, at) = (got.clone(), now.clone());
        Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("ABORT"), move |p| {
                if p.data_type() == abort {
                    seen.lock().unwrap().get_or_insert(at.load(Ordering::Relaxed));
                }
                Ok(())
            })])
            .with_sender(name),
            clock(&now),
        ))
    };
    let (rf_got, gb_got) = (Arc::new(Mutex::new(None::<u64>)), Arc::new(Mutex::new(None::<u64>)));
    let gs = Arc::new(Router::new_with_clock(RouterConfig::new([]).with_sender("GS"), clock(&now)));
    let (rf, gb) = (board("RF", &rf_got), board("GB", &gb_got));
    type Wire = Arc<Mutex<Vec<Vec<u8>>>>;
    let (to_rf, rf_to_gs, to_gb, gb_to_gs): (Wire, Wire, Wire, Wire) = Default::default();
    let link: crate::comms_config::CommsLinkConfig =
        serde_json::from_value(serde_json::json!({"interface": "serial", "port": "/dev/test-radio", "protocol": "raw_uart"})).unwrap();
    let tx = |w: &Wire| { let w = w.clone(); move |b: &[u8]| { w.lock().unwrap().push(b.to_vec()); Ok(()) } };
    let gs_radio = gs.add_side_packed_with_options("rocket_comms", tx(&to_rf), crate::rocket_router_side_options(&link, false));
    let rf_side = rf.add_side_packed_with_options("radio", tx(&rf_to_gs), crate::rocket_router_side_options(&link, false));
    let gs_umbilical = gs.add_side_packed("umbilical_comms", tx(&to_gb));
    let gb_side = gb.add_side_packed("umbilical", tx(&gb_to_gs));
    let start = now.load(Ordering::Relaxed);
    let abort_at = start + 1_000 + abort_after_ms;
    let radio_back = start + 1_000 + outage_ms;
    let pump = |steps: usize| {
        for _ in 0..steps {
            now.fetch_add(10, Ordering::Relaxed);
            let t = now.load(Ordering::Relaxed);
            let radio = t < start + 1_000 || t >= radio_back;
            for r in [&gs, &rf, &gb] {
                let _ = r.poll_discovery();
                let _ = r.process_all_queues();
            }
            for b in std::mem::take(&mut *to_rf.lock().unwrap()) {
                if radio { let _ = rf.rx_packed_from_side(&b, rf_side); }
            }
            for b in std::mem::take(&mut *rf_to_gs.lock().unwrap()) {
                if radio { let _ = gs.rx_packed_from_side(&b, gs_radio); }
            }
            for b in std::mem::take(&mut *to_gb.lock().unwrap()) {
                let _ = gb.rx_packed_from_side(&b, gb_side);
            }
            for b in std::mem::take(&mut *gb_to_gs.lock().unwrap()) {
                let _ = gs.rx_packed_from_side(&b, gs_umbilical);
            }
        }
    };
    for r in [&gs, &rf, &gb] {
        r.announce_discovery().unwrap();
    }
    pump(100);
    while now.load(Ordering::Relaxed) < abort_at {
        pump(10);
    }
    let resender = AbortResend::default();
    let routed = av_bay_routed_now(&gs);
    crate::telemetry_task::queue_manual_abort_wire_receipted(&gs, WirePolicy::Untimed).unwrap();
    arm_now(&resender, routed);
    while now.load(Ordering::Relaxed) < abort_at + run_ms {
        pump(5);
        let t = now.load(Ordering::Relaxed);
        let heard = t >= abort_at + unheard_until_ms;
        let aborted = rf_got.lock().unwrap().is_some();
        let (owed, unheard) = match (aborted, heard) {
            (true, _) => (0, 0),
            (false, true) => (0b111, 0),
            (false, false) => (0, 0b111),
        };
        resender.step(&gs, observation(t, owed, unheard));
    }
    sedsnet::config::set_runtime_tuning_config(previous).unwrap();
    let since = |got: &Arc<Mutex<Option<u64>>>| got.lock().unwrap().map(|t| t - abort_at);
    (since(&rf_got), since(&gb_got))
}

/// rrls_takeover on GS fc0de4d: the radio was out 40 s, the Abort came 0.4 s
/// after it returned, and the station's radio route to the AV bay had aged
/// out (no AV-bay discovery frame heard yet). The SDK routed the ABORT to the
/// umbilical only, the re-send never fired (no AV-bay board ever owed it in
/// flight), and the AV bay never got the Abort. It must get it once the AV
/// bay is reachable again.
#[test]
fn an_abort_sent_while_the_radio_route_is_expired_reaches_the_av_bay() {
    let (rf, gb) = abort_across_a_radio_outage(40_000, 39_500, 0, 20_000);
    eprintln!("ABORT_AFTER_ROUTE_EXPIRY rf={rf:?} gb={gb:?}");
    assert!(gb.is_some_and(|ms| ms < 500), "the umbilical got it at once: {gb:?}");
    let ms = rf.unwrap_or_else(|| panic!("the AV bay never got the Abort"));
    // Radio back 0.5 s after the Abort; rediscovery takes a topology request.
    assert!(ms <= 500 + 2 * TOPOLOGY_REQUEST_MS_FOR_TEST + 1_000, "AV bay got it {ms} ms after the Abort");
}

/// The same outage, with the Abort pressed while the AV bay is unheard: it
/// neither owes nor confirms the Abort, which must not end the re-send.
#[test]
fn an_abort_pressed_while_the_av_bay_is_unheard_reaches_it_when_heard_again() {
    let (rf, gb) = abort_across_a_radio_outage(40_000, 35_000, 6_000, 20_000);
    eprintln!("ABORT_WHILE_UNHEARD rf={rf:?} gb={gb:?}");
    assert!(gb.is_some_and(|ms| ms < 500), "the umbilical got it at once: {gb:?}");
    let ms = rf.unwrap_or_else(|| panic!("the AV bay never got the Abort"));
    assert!(ms <= 6_000 + 2 * TOPOLOGY_REQUEST_MS_FOR_TEST + 1_000, "AV bay got it {ms} ms after the Abort");
}

const TOPOLOGY_REQUEST_MS_FOR_TEST: u64 = TOPOLOGY_REQUEST_MS;
fn av_bay_routed_now(router: &Router) -> SendMark {
    send_mark(router)
}
fn arm_now(resender: &AbortResend, mark: SendMark) {
    resender.arm(1, WirePolicy::Untimed, None, mark);
}
fn routed_mark() -> SendMark {
    SendMark { av_bay_routed: true, ..SendMark::default() }
}
fn observation(now_ms: u64, owed: u8, unheard: u8) -> Observation {
    Observation { now_ms, epoch: 1, station_aborted: true, recovery: None, owed, unheard }
}
