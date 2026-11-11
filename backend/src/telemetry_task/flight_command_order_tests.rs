//! FLIGHT_COMMAND is an Ordered reliable stream: a command waits until the FC
//! has acknowledged the one before it, and when that earlier command runs out
//! of retries the SDK retires the stream and drops what is queued behind it.
//! These cases show what that does to an operator Deploy. Real SDK routers,
//! station delivery tuning; no hardware.
use super::*;
use super::radio_io::{process_router_queues_with_backoff, RadioRefusalBackoff};
use sedsnet::router::{EndpointHandler, RouterConfig};

struct StationTuning {
    previous: sedsnet::config::RuntimeTuningConfig,
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl StationTuning {
    fn install() -> Self {
        let guard = crate::RUNTIME_TUNING_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = sedsnet::config::runtime_tuning_config();
        sedsnet::config::set_runtime_tuning_config(crate::network_delivery_tuning()).unwrap();
        Self { previous, _serial: guard }
    }
}
impl Drop for StationTuning {
    fn drop(&mut self) {
        sedsnet::config::set_runtime_tuning_config(self.previous).unwrap();
    }
}

fn reliable_retry_span_ms() -> u64 {
    let tuning = crate::network_delivery_tuning();
    u64::from(tuning.reliable_retransmit_ms) * u64::from(tuning.reliable_max_retries + 1)
}

/// The station and the FC on real SDK routers with the station's delivery
/// tuning (250 ms x 18). The FC is unreachable for the first `outage_ms`
/// (frames to it are lost, as in a radio outage before the RRLS relay takes
/// over). FLIGHT_COMMAND is an Ordered reliable stream, so a command waits
/// until the one before it is acknowledged. Returns, per command written at
/// (ms after start, FC byte), the ms after its write at which the FC handler
/// first received it (None: never within `run_ms`).
fn flight_command_bay(outage_ms: u64, writes: &[(u64, u8)], run_ms: u64) -> Vec<Option<u64>> {
    let _tuning = StationTuning::install();
    crate::telemetry_schema::initialize().unwrap();
    let ty = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    crate::telemetry_task::refresh_host_network_time(&host, get_current_timestamp_ms());
    let got = Arc::new(Mutex::new(Vec::<(u8, std::time::Instant)>::new()));
    let seen = got.clone();
    let fc = Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            move |packet| {
                seen.lock().unwrap().push((packet.payload()[0], std::time::Instant::now()));
                Ok(())
            },
        )])
        .with_sender("FC"),
    ));
    let to_fc = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let out = to_fc.clone();
    host.add_side_packed("FC", move |bytes| {
        out.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let to_host = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let back = to_host.clone();
    fc.add_side_packed("GS", move |bytes| {
        back.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let mut backoff = RadioRefusalBackoff::default();
    let start = std::time::Instant::now();
    let mut pump = |link_up: bool| {
        process_router_queues_with_backoff(&host, None, &mut backoff);
        for bytes in std::mem::take(&mut *to_fc.lock().unwrap()) {
            if link_up {
                let _ = fc.rx_packed_from_side(&bytes, 0);
            }
        }
        let _ = fc.process_all_queues_with_timeout(0);
        for bytes in std::mem::take(&mut *to_host.lock().unwrap()) {
            if link_up {
                let _ = host.rx_packed_from_side(&bytes, 0);
            }
        }
    };
    fc.announce_discovery().unwrap();
    host.announce_discovery().unwrap();
    for _ in 0..50 {
        pump(true);
        std::thread::sleep(Duration::from_millis(1));
    }
    got.lock().unwrap().clear();
    let start_run = std::time::Instant::now();
    let _ = start;
    let mut sent_at = Vec::new();
    let mut next = 0;
    while start_run.elapsed() < Duration::from_millis(run_ms) {
        if let Some(&(at, byte)) = writes.get(next)
            && start_run.elapsed() >= Duration::from_millis(at)
        {
            host.tx_queue(crate::telemetry_task::controller_packet(&host, ty, &[byte]).unwrap()).unwrap();
            sent_at.push(std::time::Instant::now());
            next += 1;
        }
        pump(start_run.elapsed() >= Duration::from_millis(outage_ms));
        std::thread::sleep(Duration::from_millis(2));
    }
    let got = got.lock().unwrap().clone();
    writes
        .iter()
        .zip(&sent_at)
        .map(|(&(_, byte), sent)| got.iter().find(|(b, at)| *b == byte && at >= sent).map(|(_, at)| (*at - *sent).as_millis() as u64))
        .collect()
}

/// A Deploy pressed while an earlier FC command is still unacknowledged waits
/// behind it (FLIGHT_COMMAND is Ordered). On a link that returns within the
/// earlier command's retry span the Deploy is delivered as soon as the link
/// returns; it never waits longer than that span.
#[test]
fn deploy_behind_an_unacknowledged_fc_command_waits_at_most_one_retry_span() {
    let span = reliable_retry_span_ms();
    let delivered = flight_command_bay(span * 2 / 3, &[(0, 14), (500, 12)], span + 3_000);
    eprintln!("DEPLOY_BEHIND_SLOW_LINK span={span} delivered={delivered:?}");
    let deploy = delivered[1].unwrap_or_else(|| panic!("Deploy never reached the FC: {delivered:?}"));
    assert!(deploy <= span, "Deploy reached the FC {deploy} ms after it was pressed (span {span} ms)");
}

/// The strand: the earlier FC command runs out of retries during an outage
/// that ends inside the Deploy's own retry span. The SDK retires the Ordered
/// stream and drops the Deploy queued behind it unsent, so the FC never gets
/// the Deploy although the link came back while the operator's press was still
/// live. The Deploy must reach the FC.
#[test]
#[ignore = "the raw SDK strand without the station's re-send: kept to show the SDK behaviour"]
fn sdk_drops_a_command_queued_behind_an_exhausted_one() {
    let span = reliable_retry_span_ms();
    let delivered = flight_command_bay(span + 1_000, &[(0, 14), (2_000, 12)], span + 6_000);
    eprintln!("SDK_DROPS_BEHIND_EXHAUSTED span={span} delivered={delivered:?}");
    assert_eq!(delivered, vec![None, None], "the SDK alone drops the queued command");
}

/// What happens between the operator's presses in `operator_bay`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Between {
    Nothing,
    /// The FC receives every command throughout; only its acknowledgements
    /// are lost during the outage.
    AcksLost,
    /// An Abort latched and cleared again (R3).
    AbortLatched,
    /// Another operator command dispatched (R3).
    NewerCommand,
    /// The Deploy press is made to look older than its retry span (R2).
    PressAged,
    /// The Deploy was queued by the station itself, not pressed (R9).
    Automatic,
}

const DEPLOY: u8 = 12;
const FOCUS: u8 = 15;

/// The station (real AppState, HITL, flight state `flight`) and the FC on
/// real SDK routers. The FC is unreachable for span + 1 s. The operator
/// presses EvaluationRelax at 0 and `second` at 2 s, so `second` waits behind
/// it. A non-pyro `second` is queued in the SDK and dropped when
/// EvaluationRelax exhausts; a Deploy is held by the station instead. Returns
/// when the FC got `second` after its press (ms) and the notifications.
async fn operator_bay(flight: crate::types::FlightState, between: Between, second: u8) -> (Option<u64>, Vec<String>) {
    let (delivered, notes, _) = operator_bay_timed(flight, between, second).await;
    (delivered, notes)
}
/// `operator_bay`, plus the ms after the first press at which the station
/// first said a press got no confirmation from the FC.
async fn operator_bay_timed(flight: crate::types::FlightState, between: Between, second: u8) -> (Option<u64>, Vec<String>, Option<u64>) {
    let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    let _tuning = StationTuning::install();
    crate::telemetry_schema::initialize().unwrap();
    state.action_policy.lock().unwrap().software_buttons_enabled = true;
    *state.state.lock().unwrap() = flight;
    let span = reliable_retry_span_ms();
    let outage_ms = span + 1_000;
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    crate::telemetry_task::refresh_host_network_time(&host, get_current_timestamp_ms());
    let got = Arc::new(Mutex::new(Vec::<(u8, std::time::Instant)>::new()));
    let seen = got.clone();
    let fc = Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            move |packet| {
                seen.lock().unwrap().push((packet.payload()[0], std::time::Instant::now()));
                Ok(())
            },
        )])
        .with_sender("FC"),
    ));
    let to_fc = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let out = to_fc.clone();
    host.add_side_packed("FC", move |bytes| {
        out.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let to_host = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let back = to_host.clone();
    fc.add_side_packed("GS", move |bytes| {
        back.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let mut backoff = RadioRefusalBackoff::default();
    let acks_lost = between == Between::AcksLost;
    let mut pump = |link_up: bool| {
        process_router_queues_with_backoff(&host, None, &mut backoff);
        crate::operator_command_resend::service(&state, &host);
        for bytes in std::mem::take(&mut *to_fc.lock().unwrap()) {
            if link_up || acks_lost {
                let _ = fc.rx_packed_from_side(&bytes, 0);
            }
        }
        let _ = fc.process_all_queues_with_timeout(0);
        for bytes in std::mem::take(&mut *to_host.lock().unwrap()) {
            if link_up {
                let _ = host.rx_packed_from_side(&bytes, 0);
            }
        }
    };
    fc.announce_discovery().unwrap();
    host.announce_discovery().unwrap();
    for _ in 0..50 {
        pump(true);
        std::thread::sleep(Duration::from_millis(1));
    }
    got.lock().unwrap().clear();
    // What the telemetry task does for an operator's HITL flight command.
    let press = |byte: u8, operator: bool| {
        if operator {
            crate::operator_command_resend::note_operator_dispatch(&state);
            crate::operator_command_resend::submit_operator_flight_command(&state, &host, byte).unwrap();
        } else {
            crate::telemetry_task::queue_locally_routed_flight_command(&state, &host, "test", &[byte]).unwrap();
        }
    };
    let start = std::time::Instant::now();
    let ms = || start.elapsed().as_millis() as u64;
    let (mut relax, mut deploy_at, mut disturbed) = (false, None, false);
    let mut undelivered_at = None;
    while ms() < span + 2_500 {
        if !relax {
            press(14, true);
            relax = true;
        }
        if deploy_at.is_none() && ms() >= 2_000 {
            press(second, between != Between::Automatic);
            deploy_at = Some(std::time::Instant::now());
        }
        if !disturbed && ms() >= 3_000 {
            disturbed = true;
            match between {
                Between::AbortLatched => {
                    state.set_abort_indicator_latched(true);
                    state.set_abort_indicator_latched(false);
                }
                Between::NewerCommand => crate::operator_command_resend::note_operator_dispatch(&state),
                Between::PressAged => {
                    crate::operator_command_resend::backdate_press_for_test(&host, span);
                    crate::operator_command_resend::backdate_held_for_test(&host, crate::operator_command_resend::PYRO_HOLD_LIMIT);
                }
                _ => {}
            }
        }
        pump(ms() >= outage_ms);
        if undelivered_at.is_none()
            && state.notifications_snapshot().iter().any(|n| n.message.contains("no confirmation from FC"))
        {
            undelivered_at = Some(ms());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let sent = deploy_at.unwrap();
    let delivered = got.lock().unwrap().iter().find(|(b, at)| *b == second && *at >= sent).map(|(_, at)| (*at - sent).as_millis() as u64);
    let notes = state.notifications_snapshot().into_iter().map(|n| n.message).collect();
    (delivered, notes, undelivered_at)
}

/// pyroI (candidate 7ad9582): every copy of a relayed ExpandParachute was
/// lost before the RRLS; the station retired it after its retry span and
/// nothing told the operator. A press whose own packet runs out of retries
/// is reported, once and in one line naming that press, by the end of its
/// retry span, and is never re-sent (R1). Here the press is EvaluationRelax
/// (the harness's first press, its own packet the head that runs out).
#[tokio::test]
async fn a_press_whose_every_copy_was_lost_is_reported_unconfirmed() {
    let span = reliable_retry_span_ms();
    let (_, notes, at) = operator_bay_timed(crate::types::FlightState::Ascent, Between::Nothing, FOCUS).await;
    eprintln!("UNCONFIRMED span={span} at={at:?} notes={notes:?}");
    let lost: Vec<_> = notes.iter().filter(|n| n.contains("no confirmation from FC")).collect();
    assert_eq!(lost.len(), 1, "one line: {notes:?}");
    assert_eq!(lost[0].as_str(), "EvaluationRelax: no confirmation from FC. Check FC state before pressing again.");
    let at = at.expect("reported");
    assert!(at <= span + 500, "reported {at} ms after the press (span {span})");
}

/// The reverse case: the FC got and ran the command, and only every one of
/// its acknowledgements was lost. The station cannot tell the two apart, so
/// the notice never claims the command was not delivered and never tells the
/// operator to press again without checking the FC first (a duplicate pyro
/// command is the risk).
#[tokio::test]
async fn a_press_whose_acknowledgements_were_lost_is_never_called_undelivered() {
    let (_, notes, _) = operator_bay_timed(crate::types::FlightState::Ascent, Between::AcksLost, FOCUS).await;
    eprintln!("ACKS_LOST notes={notes:?}");
    let line = notes.iter().find(|n| n.starts_with("EvaluationRelax")).unwrap_or_else(|| panic!("reported: {notes:?}"));
    assert!(!line.contains("not delivered") && !line.contains("press again"), "{line}");
    assert!(line.contains("Check FC state before pressing again"), "{line}");
}

fn noted(notes: &[String], label: &str, words: &str) -> bool {
    notes.iter().any(|n| n.starts_with(label) && n.contains(words))
}

/// R1, R5, R8: a non-pyro command dropped behind the exhausted
/// EvaluationRelax is re-sent once and reaches the FC as soon as the link
/// returns, inside the press's own retry span, with its own notification.
#[tokio::test]
async fn command_dropped_behind_an_exhausted_fc_command_is_resent() {
    let span = reliable_retry_span_ms();
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::Nothing, FOCUS).await;
    eprintln!("FOCUS_RESENT span={span} delivered={delivered:?} notes={notes:?}");
    let ms = delivered.unwrap_or_else(|| panic!("EvaluationFocus never reached the FC: {notes:?}"));
    assert!(ms <= span + 750, "EvaluationFocus reached the FC {ms} ms after the press (span {span})");
    assert!(noted(&notes, "EvaluationFocus", "re-sent after the link recovered"), "{notes:?}");
}

/// R3: an Abort after the press means no re-send, and the operator is told.
#[tokio::test]
async fn command_is_not_resent_after_an_abort() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::AbortLatched, FOCUS).await;
    assert_eq!(delivered, None, "{notes:?}");
    assert!(noted(&notes, "EvaluationFocus", "not re-sent: an Abort"), "{notes:?}");
}

/// R3: a newer operator command means no re-send.
#[tokio::test]
async fn command_is_not_resent_after_a_newer_operator_command() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::NewerCommand, FOCUS).await;
    assert_eq!(delivered, None, "{notes:?}");
    assert!(noted(&notes, "EvaluationFocus", "a newer operator command"), "{notes:?}");
}

/// R2: a press older than its retry span is not re-sent; the operator is told.
#[tokio::test]
async fn command_older_than_its_retry_span_is_not_resent_and_is_reported() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::PressAged, FOCUS).await;
    assert_eq!(delivered, None, "{notes:?}");
    assert!(noted(&notes, "EvaluationFocus", "beyond its"), "{notes:?}");
}

/// R9: a command the station queued itself is never re-sent.
#[tokio::test]
async fn automatic_station_command_is_never_resent() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::Automatic, DEPLOY).await;
    assert_eq!(delivered, None, "{notes:?}");
    assert!(!noted(&notes, "DeployParachute", "sent"), "{notes:?}");
}

/// P1, P6: an operator Deploy pressed behind an unacknowledged FC command is
/// held by the station (never handed to the SDK, so never cancelled with
/// the exhausted head) and sent once the stream is idle; it reaches the FC as
/// soon as the link returns, exactly once.
#[tokio::test]
async fn deploy_behind_an_exhausted_fc_command_is_held_then_sent_once() {
    let span = reliable_retry_span_ms();
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::Nothing, DEPLOY).await;
    eprintln!("DEPLOY_HELD span={span} delivered={delivered:?} notes={notes:?}");
    let ms = delivered.unwrap_or_else(|| panic!("the Deploy never reached the FC: {notes:?}"));
    assert!(ms <= span + 750, "Deploy reached the FC {ms} ms after the press (span {span})");
    assert!(noted(&notes, "DeployParachute", "held, not sent yet"), "{notes:?}");
    assert!(noted(&notes, "DeployParachute", "ms after the press: it was held until the earlier"), "{notes:?}");
    assert!(!noted(&notes, "DeployParachute", "dropped unsent"), "{notes:?}");
}

/// P2: an Abort while a Deploy is held: never sent, and the operator is told.
#[tokio::test]
async fn held_deploy_is_never_sent_after_an_abort() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::AbortLatched, DEPLOY).await;
    assert_eq!(delivered, None, "{notes:?}");
    assert!(noted(&notes, "DeployParachute", "was never sent: an Abort or manual stop came while it was held"), "{notes:?}");
}

/// P5: an unrelated newer operator command does not cancel a held Deploy.
#[tokio::test]
async fn held_deploy_survives_a_newer_operator_command() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::NewerCommand, DEPLOY).await;
    assert!(delivered.is_some(), "{notes:?}");
}

/// P1: a Deploy held beyond the FC's 15 s bound is never sent.
#[tokio::test]
async fn deploy_held_beyond_the_fc_bound_is_never_sent() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Ascent, Between::PressAged, DEPLOY).await;
    assert_eq!(delivered, None, "{notes:?}");
    assert!(noted(&notes, "DeployParachute", "was never sent: it was held"), "{notes:?}");
}

/// P3 (R7): the FC would refuse a Deploy at or before Armed; never sent.
#[tokio::test]
async fn held_deploy_the_fc_would_refuse_is_never_sent() {
    let (delivered, notes) = operator_bay(crate::types::FlightState::Armed, Between::Nothing, DEPLOY).await;
    assert_eq!(delivered, None, "{notes:?}");
    assert!(noted(&notes, "DeployParachute", "would now refuse"), "{notes:?}");
}

/// What the FC handler got in `saturated_bay`: (byte, packet ID), in order.
type FcLog = Vec<(u8, u64)>;

/// Bench 2026-11-11 (GS 8f5a97d, FC 13de2ca, RFD900x, S5=0, nearly saturated
/// channel), on real SDK routers. Every frame reaches the FC; every frame
/// back from it is lost (its ACKs never arrive). The station's router gets a
/// TX turn only every `turn_ms`, so a retry span takes longer than its
/// nominal 250 ms x 18. The operator presses Deploy at 0 and, once the
/// station sees the drogue, Expand at `expand_at_ms`. Returns what the FC
/// handler got and the station's notifications.
async fn saturated_bay(turn_ms: u64, later: &[(u64, u8)], run_ms: u64) -> (FcLog, Vec<String>) {
    use crate::rocket_commands::FlightComputerCommands as Fc;
    let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    let _tuning = StationTuning::install();
    crate::telemetry_schema::initialize().unwrap();
    crate::fc_command_gate::forget(&state);
    state.action_policy.lock().unwrap().software_buttons_enabled = true;
    *state.state.lock().unwrap() = crate::types::FlightState::Ascent;
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    crate::telemetry_task::refresh_host_network_time(&host, get_current_timestamp_ms());
    let got = Arc::new(Mutex::new(FcLog::new()));
    let seen = got.clone();
    let fc = Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            move |packet| {
                seen.lock().unwrap().push((packet.payload()[0], packet.packet_id()));
                Ok(())
            },
        )])
        .with_sender("FC"),
    ));
    let to_fc = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let out = to_fc.clone();
    host.add_side_packed("FC", move |bytes| {
        out.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let to_host = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let back = to_host.clone();
    fc.add_side_packed("GS", move |bytes| {
        back.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let mut backoff = RadioRefusalBackoff::default();
    let deliver = |acks: bool| {
        for bytes in std::mem::take(&mut *to_fc.lock().unwrap()) {
            let _ = fc.rx_packed_from_side(&bytes, 0);
        }
        let _ = fc.process_all_queues_with_timeout(0);
        for bytes in std::mem::take(&mut *to_host.lock().unwrap()) {
            if acks {
                let _ = host.rx_packed_from_side(&bytes, 0);
            }
        }
    };
    fc.announce_discovery().unwrap();
    host.announce_discovery().unwrap();
    for _ in 0..50 {
        process_router_queues_with_backoff(&host, None, &mut backoff);
        deliver(true);
        std::thread::sleep(Duration::from_millis(1));
    }
    got.lock().unwrap().clear();
    // The telemetry task's operator HITL flight command.
    let press = |byte: u8| {
        crate::operator_command_resend::note_operator_dispatch(&state);
        crate::operator_command_resend::submit_operator_flight_command(&state, &host, byte).unwrap();
    };
    let start = std::time::Instant::now();
    let ms = || start.elapsed().as_millis() as u64;
    let (mut deploy, mut drogue, mut next) = (false, false, 0);
    let mut last_turn = 0;
    while ms() < run_ms {
        if !deploy {
            press(Fc::DeployParachute as u8);
            deploy = true;
        }
        if !drogue && ms() >= 100 {
            // The FC's deploy report: the station sees the drogue.
            crate::recovery_events::observe_fc_state(&state, crate::types::FlightState::Ascent, crate::types::FlightState::Descent, 1);
            drogue = true;
        }
        if let Some(&(at, byte)) = later.get(next)
            && ms() >= at
        {
            press(byte);
            next += 1;
        }
        if ms() >= last_turn + turn_ms {
            last_turn = ms();
            process_router_queues_with_backoff(&host, None, &mut backoff);
        }
        crate::operator_command_resend::service(&state, &host);
        deliver(false);
        std::thread::sleep(Duration::from_millis(2));
    }
    let got = got.lock().unwrap().clone();
    let notes = state.notifications_snapshot().into_iter().map(|n| n.message).collect();
    (got, notes)
}

fn copies(got: &FcLog, byte: u8) -> usize {
    got.iter().filter(|(b, _)| *b == byte).map(|(_, id)| *id).collect::<std::collections::BTreeSet<_>>().len()
}

/// The bench strand. Deploy's ACKs are lost; on the saturated channel its
/// retries take ~7 s to run out, longer than the nominal retry span. Expand,
/// pressed behind it, was cancelled from the Ordered queue unsent and then
/// refused a re-send as "older than its retry span": the operator's pyro
/// press was dropped. It must reach the FC exactly once, and the Deploy that
/// may already have fired must never be sent a second time.
#[tokio::test]
async fn expand_pressed_behind_an_unconfirmed_deploy_reaches_the_fc_once() {
    use crate::rocket_commands::FlightComputerCommands as Fc;
    let (got, notes) = saturated_bay(400, &[(500, Fc::ExpandParachute as u8)], 9_500).await;
    eprintln!("SATURATED_BAY got={got:?} notes={notes:?}");
    assert_eq!(copies(&got, Fc::DeployParachute as u8), 1, "Deploy is never sent twice: {got:?} {notes:?}");
    assert_eq!(copies(&got, Fc::ExpandParachute as u8), 1, "Expand reaches the FC exactly once: {got:?} {notes:?}");
    assert!(
        notes.iter().any(|n| n.starts_with("DeployParachute: no confirmation from FC")),
        "the unconfirmed Deploy is still reported: {notes:?}"
    );
    assert!(!notes.iter().any(|n| n.contains("was not re-sent")), "{notes:?}");
    assert!(notes.iter().any(|n| n.starts_with("ExpandParachute held, not sent yet")), "{notes:?}");
    assert!(
        notes.iter().any(|n| n.starts_with("ExpandParachute sent ") && n.contains("held until the earlier flight-computer command was confirmed or ran out of retries")),
        "{notes:?}"
    );
}

/// P4: a second Deploy pressed while the first is still awaiting FC
/// confirmation is never sent, at any time: the first may already have
/// fired, and the FC fires again for a copy with a new packet ID.
#[tokio::test]
async fn a_second_deploy_while_the_first_is_unconfirmed_is_never_sent() {
    use crate::rocket_commands::FlightComputerCommands as Fc;
    let (got, notes) = saturated_bay(400, &[(500, Fc::DeployParachute as u8)], 9_500).await;
    eprintln!("SECOND_DEPLOY got={got:?} notes={notes:?}");
    assert_eq!(copies(&got, Fc::DeployParachute as u8), 1, "{got:?} {notes:?}");
    let blocked: Vec<_> = notes.iter().filter(|n| n.starts_with("DeployParachute not sent:")).collect();
    assert_eq!(blocked.len(), 1, "{notes:?}");
    assert!(blocked[0].contains("is still awaiting FC confirmation, and a second copy could fire it twice"), "why: {}", blocked[0]);
    assert!(blocked[0].contains("Pressing again works as soon as the FC confirms it or its retries run out"), "when: {}", blocked[0]);
}

/// P4 ends with the earlier press: once the first Deploy runs out of retries
/// unconfirmed (~7 s on this channel), the operator's next press is sent,
/// exactly once. A press made before that is blocked with its reason.
#[tokio::test]
async fn a_deploy_pressed_after_the_first_ran_out_unconfirmed_is_sent_once() {
    use crate::rocket_commands::FlightComputerCommands as Fc;
    let deploy = Fc::DeployParachute as u8;
    let (got, notes) = saturated_bay(400, &[(500, deploy), (8_500, deploy)], 10_000).await;
    eprintln!("REPRESS got={got:?} notes={notes:?}");
    let first_unconfirmed = notes.iter().position(|n| n.starts_with("DeployParachute: no confirmation from FC")).unwrap_or_else(|| panic!("{notes:?}"));
    assert!(notes[..first_unconfirmed].iter().any(|n| n.starts_with("DeployParachute not sent:")), "the early press is blocked: {notes:?}");
    assert!(!notes[first_unconfirmed..].iter().any(|n| n.starts_with("DeployParachute not sent:")), "the later press is not: {notes:?}");
    assert_eq!(copies(&got, deploy), 2, "the first and the operator's later press, once each: {got:?} {notes:?}");
}

/// R2/R3/R4/R7 precedence: safety first, then age, then the FC's view.
#[test]
fn resend_verdict_follows_the_rules() {
    use crate::operator_command_resend::{verdict, Context, Verdict};
    let ok = Context { age: Duration::from_millis(1_000), span: Duration::from_millis(4_500), aborted: false,
        newer_command: false, effect_seen: false, admissible: true };
    assert_eq!(verdict(&ok), Verdict::Resend);
    assert_eq!(verdict(&Context { aborted: true, ..ok }), Verdict::Aborted);
    assert_eq!(verdict(&Context { newer_command: true, ..ok }), Verdict::Superseded);
    assert_eq!(verdict(&Context { age: Duration::from_millis(4_500), ..ok }), Verdict::TooOld);
    assert_eq!(verdict(&Context { effect_seen: true, ..ok }), Verdict::AlreadyDone);
    assert_eq!(verdict(&Context { admissible: false, ..ok }), Verdict::Inadmissible);
    assert_eq!(verdict(&Context { aborted: true, age: Duration::from_millis(9_000), ..ok }), Verdict::Aborted);
}
