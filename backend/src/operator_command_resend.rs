//! Re-send an operator's flight-computer command that the SDK dropped unsent.
//!
//! FLIGHT_COMMAND is an Ordered end-to-end stream: a command waits until the
//! FC has acknowledged the one before it, and when that earlier command runs
//! out of retries the SDK retires the stream and drops every command queued
//! behind it, unsent (`flight_command_order_tests`). An operator Deploy
//! pressed during a radio outage could be lost although the link came back
//! while the press was still live.
//!
//! Rules (each pinned in `flight_command_order_tests`):
//! - R1 only the newest operator flight command, and only one that was queued
//!   behind another pending FLIGHT_COMMAND and dropped with it; never the
//!   retired head itself;
//! - R2 only while the press is younger than its own retry span; an older one
//!   is not re-sent and the operator is told;
//! - R3 never after a newer operator command, an Abort or a manual stop;
//! - R4 the station's admission checks are run again (`is_command_allowed`,
//!   and the controller fence in `gse::queue_controller`);
//! - R5 at most once per press, with a fresh packet ID and the original byte;
//! - R6 the FC refusing a repeat (Parachute_Deployed/Expanded) is a second
//!   guard only;
//! - R7 not when the station already saw the effect (the drogue or main
//!   deployment) or the FC state makes it inadmissible (Deploy needs a state
//!   beyond Armed, Expand a deployed drogue: FC fcapi.h);
//! - R8 every re-send, and every drop that is not re-sent, raises its own
//!   operator notification and log line;
//! - R9 only operator presses are recorded; automatic station commands never.
//!
//! An operator command that is itself the retired head (every copy went
//! unanswered for its whole retry span, for example lost on a degraded relay
//! uplink) is never re-sent (R1); the operator is told in one line that the
//! FC did not confirm it. The station cannot tell a lost command from lost
//! acknowledgements, so it never says "not delivered" and asks the operator
//! to check the FC state before pressing again.
use crate::state::AppState;
use crate::types::{FlightState, TelemetryCommand};
use sedsnet::router::Router;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per station (AppState address): operator dispatches and latched Aborts.
static COUNTERS: Mutex<BTreeMap<usize, (u64, u64)>> = Mutex::new(BTreeMap::new());

fn state_key(state: &AppState) -> usize {
    std::ptr::from_ref(state) as usize
}
fn counters(state: &AppState) -> (u64, u64) {
    COUNTERS.lock().unwrap().get(&state_key(state)).copied().unwrap_or_default()
}

#[derive(Clone, Debug)]
struct Press {
    packet_id: u64,
    byte: u8,
    pressed_at: Instant,
    dispatch: u64,
    abort_epoch: u64,
    behind: bool,
    dropped_at: Option<Instant>,
    missing_since: Option<Instant>,
    seen_pending: bool,
    done: bool,
}

/// The newest operator flight command per router (router address).
static NEWEST: Mutex<BTreeMap<usize, Press>> = Mutex::new(BTreeMap::new());
/// Recent operator presses per router (packet ID, command byte), newest last,
/// so a retired head can be named whichever press it was.
static RECENT: Mutex<BTreeMap<usize, std::collections::VecDeque<(u64, u8)>>> = Mutex::new(BTreeMap::new());
/// Presses per router whose own packet ran out of retries (no acknowledgement
/// came back): reported once.
static UNDELIVERED: Mutex<BTreeMap<usize, Vec<u8>>> = Mutex::new(BTreeMap::new());
const RECENT_PRESSES: usize = 16;

fn router_key(router: &Router) -> usize {
    router as *const Router as usize
}

fn retry_span() -> Duration {
    let tuning = sedsnet::config::runtime_tuning_config();
    Duration::from_millis(u64::from(tuning.reliable_retransmit_ms) * u64::from(tuning.reliable_max_retries + 1))
}

/// Every operator command the telemetry task dispatches (R3: a newer one).
pub(crate) fn note_operator_dispatch(state: &AppState) {
    COUNTERS.lock().unwrap().entry(state_key(state)).or_default().0 += 1;
}

/// Every Abort the station latches (R3).
pub(crate) fn note_abort(state: &AppState) {
    COUNTERS.lock().unwrap().entry(state_key(state)).or_default().1 += 1;
}

/// FLIGHT_COMMAND packets the router still owes the FC.
pub(crate) fn pending_flight_commands(router: &Router) -> BTreeSet<u64> {
    let ty = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
    router.export_reliable_pending_stats().into_iter().filter(|row| row.data_type == ty).map(|row| row.packet_id).collect()
}

/// The operator's last Deploy/Expand on each router: its packet and press time.
static LAST_DEPLOY: Mutex<BTreeMap<usize, (u64, Instant)>> = Mutex::new(BTreeMap::new());
/// A Deploy/Expand counts as in progress this long after its press, covering
/// the FC's pulse after it acknowledged the command.
pub(crate) const DEPLOY_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

fn is_deploy(byte: u8) -> bool {
    use crate::rocket_commands::FlightComputerCommands as C;
    byte == C::DeployParachute as u8 || byte == C::ExpandParachute as u8
}

/// An operator Deploy/Expand still queued, unacknowledged, or pressed less
/// than DEPLOY_SETTLE ago. The FC reset to Startup refuses meanwhile.
pub(crate) fn deploy_in_progress(router: &Router) -> bool {
    let Some((packet_id, pressed)) = LAST_DEPLOY.lock().unwrap().get(&router_key(router)).copied() else {
        return false;
    };
    pressed.elapsed() < DEPLOY_SETTLE || pending_flight_commands(router).contains(&packet_id)
}

#[cfg(test)]
pub(crate) fn note_deploy_for_test(router: &Router, packet_id: u64, age: std::time::Duration) {
    let at = Instant::now().checked_sub(age).unwrap();
    LAST_DEPLOY.lock().unwrap().insert(router_key(router), (packet_id, at));
}

/// The FLIGHT_COMMAND packet the station last queued on each router
/// (`gse::queue_controller`), so an operator press can name its packet.
static LAST_QUEUED: Mutex<BTreeMap<usize, u64>> = Mutex::new(BTreeMap::new());

pub(crate) fn note_queued(router: &Router, packet_id: u64) {
    LAST_QUEUED.lock().unwrap().insert(router_key(router), packet_id);
}

/// An operator's flight command was just queued (R9: called only from the
/// operator dispatch, right after the queue call on the same thread).
/// `before` is `pending_flight_commands` taken right before it was queued:
/// a non-empty set means the press waits behind an earlier command.
pub(crate) fn record_operator_press(state: &AppState, router: &Router, byte: u8, before: &BTreeSet<u64>) {
    let (dispatch, abort_epoch) = counters(state);
    let Some(packet_id) = LAST_QUEUED.lock().unwrap().remove(&router_key(router)) else {
        NEWEST.lock().unwrap().remove(&router_key(router));
        return;
    };
    if is_deploy(byte) {
        LAST_DEPLOY.lock().unwrap().insert(router_key(router), (packet_id, Instant::now()));
    }
    {
        let mut recent = RECENT.lock().unwrap();
        let presses = recent.entry(router_key(router)).or_default();
        presses.push_back((packet_id, byte));
        while presses.len() > RECENT_PRESSES {
            presses.pop_front();
        }
    }
    if let Some(notice) = crate::fc_command_gate::note_press_at(state, byte, &label_for(byte), Instant::now()) {
        log::warn!("{notice}");
        crate::web::emit_notification_warning(state, notice);
    }
    NEWEST.lock().unwrap().insert(router_key(router), Press {
        packet_id,
        byte,
        pressed_at: Instant::now(),
        dispatch,
        abort_epoch,
        behind: !before.is_empty(),
        dropped_at: None,
        missing_since: None,
        seen_pending: false,
        done: false,
    });
}

/// The router retired these exhausted FLIGHT_COMMAND heads. A newest press
/// queued behind them that is no longer pending was dropped with them (R1).
pub(crate) fn note_retired(router: &Router, heads: &[u64]) {
    if heads.is_empty() {
        return;
    }
    // An operator press that was itself a retired head was never delivered.
    {
        let mut recent = RECENT.lock().unwrap();
        if let Some(presses) = recent.get_mut(&router_key(router)) {
            let lost: Vec<u8> = presses.iter().filter(|(id, _)| heads.contains(id)).map(|(_, byte)| *byte).collect();
            presses.retain(|(id, _)| !heads.contains(id));
            if !lost.is_empty() {
                UNDELIVERED.lock().unwrap().entry(router_key(router)).or_default().extend(lost);
            }
        }
    }
    if let Some(press) = NEWEST.lock().unwrap().get_mut(&router_key(router))
        && heads.contains(&press.packet_id)
    {
        // Reported above; never re-sent (R1).
        press.done = true;
    }
    let pending: BTreeSet<u64> = router.export_reliable_pending_stats().iter().map(|row| row.packet_id).collect();
    if let Some(press) = NEWEST.lock().unwrap().get_mut(&router_key(router))
        && press.behind
        && !press.done
        && press.dropped_at.is_none()
        && !heads.contains(&press.packet_id)
        && !pending.contains(&press.packet_id)
    {
        press.dropped_at = Some(Instant::now());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Resend,
    Aborted,
    Superseded,
    TooOld,
    AlreadyDone,
    Inadmissible,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Context {
    pub age: Duration,
    pub span: Duration,
    pub aborted: bool,
    pub newer_command: bool,
    pub effect_seen: bool,
    pub admissible: bool,
}

/// The rules R2, R3, R4 and R7, in that order of precedence after safety.
pub(crate) fn verdict(c: &Context) -> Verdict {
    if c.aborted {
        Verdict::Aborted
    } else if c.newer_command {
        Verdict::Superseded
    } else if c.age >= c.span {
        Verdict::TooOld
    } else if c.effect_seen {
        Verdict::AlreadyDone
    } else if !c.admissible {
        Verdict::Inadmissible
    } else {
        Verdict::Resend
    }
}

fn command_for(byte: u8) -> Option<TelemetryCommand> {
    use TelemetryCommand::*;
    [
        DeployParachute, ExpandParachute, EvaluationRelax, EvaluationFocus, EvaluationAbort, ReinitSensors,
        ReinitBarometer, EnableIMU, DisableIMU, AbortAfter40, AbortAfter100, AbortAfter250, ReinitAfter15,
        ReinitAfter30, ReinitAfter50,
    ]
    .into_iter()
    .find(|cmd| crate::telemetry_task::hitl_flight_command_id(cmd) == Some(byte))
}

/// The one-line notice for a press whose own packet got no acknowledgement.
fn unconfirmed_message(state: &AppState, byte: u8) -> String {
    let label = label_for(byte);
    if let Some(reason) = crate::fc_command_gate::active_reason(state, Instant::now()) {
        format!("{label}: no confirmation from FC. The FC reports it refuses flight commands because {reason}.")
    } else if effect_and_admissible(state, byte).0 {
        format!("{label}: no confirmation from FC, but the station already sees its effect.")
    } else {
        format!("{label}: no confirmation from FC. Check FC state before pressing again.")
    }
}

fn label_for(byte: u8) -> String {
    command_for(byte).map_or_else(|| format!("Flight command {byte}"), |c| crate::sequences::command_name(&c).to_string())
}

/// R7: what the station already knows about the FC for this command.
fn effect_and_admissible(state: &AppState, byte: u8) -> (bool, bool) {
    use crate::recovery_events::{already_deployed, Deployment};
    use crate::rocket_commands::FlightComputerCommands as Fc;
    if byte == Fc::DeployParachute as u8 {
        let beyond_armed = (*state.state.lock().unwrap() as u8) > FlightState::Armed as u8;
        (already_deployed(state, Deployment::Drogue), beyond_armed)
    } else if byte == Fc::ExpandParachute as u8 {
        (already_deployed(state, Deployment::Main), already_deployed(state, Deployment::Drogue))
    } else {
        (false, true)
    }
}

/// Run from the telemetry task: decide on a dropped operator press, at most
/// once (R5), and re-send it or tell the operator why not (R8).
pub(crate) fn service(state: &AppState, router: &Router) {
    let undelivered = UNDELIVERED.lock().unwrap().remove(&router_key(router)).unwrap_or_default();
    for byte in undelivered {
        // No acknowledgement came back: the FC may still have run it and only
        // its acknowledgements were lost, so never claim it was not delivered
        // and never invite a blind second press of a pyro command.
        let message = unconfirmed_message(state, byte);
        log::warn!("{message} (no acknowledgement within its retry span)");
        crate::web::emit_notification_warning(state, message);
    }
    let press = {
        let mut newest = NEWEST.lock().unwrap();
        let Some(p) = newest.get_mut(&router_key(router)).filter(|p| !p.done) else { return };
        if p.dropped_at.is_none() {
            // A press that leaves the pending set without being dropped was
            // delivered (or refused for good); it is never re-sent. The grace
            // covers the moment between the SDK dropping it and the router
            // worker reporting the exhaustion.
            if pending_flight_commands(router).contains(&p.packet_id) {
                p.missing_since = None;
                p.seen_pending = true;
            } else if p.seen_pending
                && p.missing_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(250)
            {
                p.done = true;
            }
            return;
        }
        p.done = true;
        p.clone()
    };
    let cmd = command_for(press.byte);
    let label = label_for(press.byte);
    let (effect_seen, fc_admissible) = effect_and_admissible(state, press.byte);
    let context = Context {
        age: press.pressed_at.elapsed(),
        span: retry_span(),
        aborted: counters(state).1 != press.abort_epoch
            || state.abort_indicator_latched()
            || crate::gse::manual_stop_pending(state),
        newer_command: counters(state).0 != press.dispatch,
        effect_seen,
        admissible: fc_admissible && cmd.as_ref().is_some_and(|c| state.is_command_allowed(c)),
    };
    let age_ms = context.age.as_millis();
    let verdict = verdict(&context);
    if verdict == Verdict::Resend {
        let before = pending_flight_commands(router);
        match crate::telemetry_task::queue_locally_routed_flight_command(state, router, "re-sent operator command", &[press.byte]) {
            Ok(()) => {
                crate::telemetry_task::flush_command_tx(router, "re-sent operator command");
                let fresh = pending_flight_commands(router).difference(&before).count();
                let message = format!(
                    "{label} re-sent after the link recovered: it was dropped unsent behind an earlier flight-computer command that ran out of retries, {age_ms} ms after the press"
                );
                log::warn!("{message} (new packets {fresh})");
                crate::web::emit_notification_warning(state, message);
            }
            Err(err) => {
                let message = format!("{label} was dropped behind an earlier flight-computer command that ran out of retries and could not be re-sent ({err}); press it again if it is still needed");
                log::warn!("{message}");
                crate::web::emit_notification_warning(state, message);
            }
        }
        return;
    }
    let reason = match verdict {
        Verdict::Aborted => "an Abort or manual stop came after the press".to_string(),
        Verdict::Superseded => "a newer operator command came after it".to_string(),
        Verdict::TooOld => format!("it was pressed {:.1} s ago, beyond its {:.1} s retry span", age_ms as f64 / 1000.0, context.span.as_secs_f64()),
        Verdict::AlreadyDone => "the station already saw its effect".to_string(),
        _ => "the flight computer or the station would now refuse it".to_string(),
    };
    let message = format!(
        "{label} was dropped unsent behind an earlier flight-computer command that ran out of retries and was not re-sent: {reason}. Press it again if it is still needed."
    );
    log::warn!("{message}");
    crate::web::emit_notification_warning(state, message);
}

/// Makes the recorded press look `ms` older (R2 cases).
#[cfg(test)]
pub(crate) fn backdate_press_for_test(router: &Router, ms: u64) {
    if let Some(p) = NEWEST.lock().unwrap().get_mut(&router_key(router)) {
        p.pressed_at -= Duration::from_millis(ms);
    }
}

#[cfg(test)]
mod unconfirmed_tests {
    use super::*;
    use crate::rocket_commands::FlightComputerCommands as Fc;

    /// The softened notice guides a pyro decision too: it appears only when
    /// the station already sees that same command's deployment, never for a
    /// different command's (drogue seen, main pressed).
    #[tokio::test]
    async fn the_unconfirmed_notice_softens_only_for_the_pressed_commands_own_effect() {
        let state = crate::state::tests::test_app_state().await;
        crate::fc_command_gate::forget(&state);
        let deploy = Fc::DeployParachute as u8;
        let expand = Fc::ExpandParachute as u8;
        let check = |byte| unconfirmed_message(&state, byte);
        assert_eq!(check(deploy), "DeployParachute: no confirmation from FC. Check FC state before pressing again.");
        // The FC reaches Descent: the drogue deployed.
        crate::recovery_events::observe_fc_state(&state, FlightState::Ascent, FlightState::Descent, 1);
        assert_eq!(check(deploy), "DeployParachute: no confirmation from FC, but the station already sees its effect.");
        assert_eq!(check(expand), "ExpandParachute: no confirmation from FC. Check FC state before pressing again.",
            "the drogue says nothing about the main");
        // The FC reaches Reefing: the main deployed.
        crate::recovery_events::observe_fc_state(&state, FlightState::Descent, FlightState::Reefing, 2);
        assert_eq!(check(expand), "ExpandParachute: no confirmation from FC, but the station already sees its effect.");
    }

    /// When the FC reports it refuses flight commands, the unconfirmed notice
    /// says why instead of only asking the operator to check.
    #[tokio::test]
    async fn the_unconfirmed_notice_names_the_fcs_refusal_reason() {
        let state = crate::state::tests::test_app_state().await;
        let relax = Fc::EvaluationRelax as u8;
        crate::fc_command_gate::forget(&state);
        crate::fc_command_gate::observe_at(&state, 9 << 24, Instant::now());
        assert_eq!(
            unconfirmed_message(&state, relax),
            "EvaluationRelax: no confirmation from FC. The FC reports it refuses flight commands because its persistent store is not ready and its recovery floor failed."
        );
        crate::fc_command_gate::forget(&state);
    }
}
