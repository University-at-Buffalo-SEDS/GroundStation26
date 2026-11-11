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
//! An operator Deploy/Expand never enters that queue: the station holds it
//! while an earlier FLIGHT_COMMAND is unresolved and submits it once the
//! stream is idle (rules P1..P6 below), so it cannot be dropped behind an
//! earlier command and every transmission is its first. R1..R9 cover the
//! other operator flight commands.
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
    /// The FLIGHT_COMMANDs unresolved when it was queued (registered before it).
    before: BTreeSet<u64>,
    /// An earlier command in `before` retired while this press was never
    /// seen as a pending row: it may still sit in the transmit queue.
    unproven_since: Option<Instant>,
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
    if pyro_held(router) {
        return true;
    }
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
    UNSEEN.lock().unwrap().entry(router_key(router)).or_default().insert(packet_id, Instant::now());
}

/// An operator's flight command was just queued (R9: called only from the
/// operator dispatch, right after the queue call on the same thread).
/// `before` is `pending_flight_commands` taken right before it was queued:
/// a non-empty set means the press waits behind an earlier command.
pub(crate) fn record_operator_press(state: &AppState, router: &Router, byte: u8, before: &BTreeSet<u64>) -> Option<u64> {
    let (dispatch, abort_epoch) = counters(state);
    let Some(packet_id) = LAST_QUEUED.lock().unwrap().remove(&router_key(router)) else {
        NEWEST.lock().unwrap().remove(&router_key(router));
        return None;
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
        before: before.clone(),
        unproven_since: None,
        dropped_at: None,
        missing_since: None,
        seen_pending: false,
        done: false,
    });
    Some(packet_id)
}

// ---- Operator Deploy/Expand: never queued behind an earlier FC command ----
//
// The SDK drops every FLIGHT_COMMAND queued behind a head that runs out of
// retries, and the FC fires a Deploy/Expand again for any copy with a new
// packet ID (FC recovery.c manual_deployment, in any flight state). So an
// operator pyro press is never handed to the SDK while an earlier
// FLIGHT_COMMAND is unresolved: the station holds it and submits it as the
// only FLIGHT_COMMAND once the stream is idle. A held press has provably never
// been transmitted (the SDK has never seen it), so sending it is its first and
// only transmission. Rules:
// - P1 held at most PYRO_HOLD_LIMIT after the press (the FC's own bound for
//   an operator pyro request, board_recovery_operator_pyro_allowed);
// - P2 an Abort or manual stop while held drops every held command;
// - P3 on release the station's admission (R4) and the FC's view (R7) are
//   checked again;
// - P4 a press of the same pyro command as an earlier one that is still
//   unresolved (held, queued or awaiting FC confirmation) is never sent: a
//   second copy could fire twice;
// - P5 while a pyro press is held, later operator flight commands queue
//   behind it in the station, so the operator's order is kept;
// - P6 every hold, release and drop raises its own notice.

/// The FC refuses an operator Deploy/Expand stamped more than 15 s ago.
pub(crate) const PYRO_HOLD_LIMIT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
struct Held {
    byte: u8,
    pressed_at: Instant,
    abort_epoch: u64,
}

/// Operator flight commands held by the station per router, oldest first.
static HELD: Mutex<BTreeMap<usize, std::collections::VecDeque<Held>>> = Mutex::new(BTreeMap::new());
/// FLIGHT_COMMANDs the station queued that have not yet been seen as SDK
/// pending rows: still in the router's transmit queue, or already gone.
/// The SDK dispatches an accepted FLIGHT_COMMAND or cancels it within its
/// retry lifetime (`expire_deferred_original`), so an entry older than
/// `unseen_horizon()` is no longer in the transmit queue.
static UNSEEN: Mutex<BTreeMap<usize, BTreeMap<u64, Instant>>> = Mutex::new(BTreeMap::new());
/// Operator pyro presses handed to the SDK per router: (packet ID, byte,
/// press time).
static SUBMITTED_PYRO: Mutex<BTreeMap<usize, Vec<(u64, u8, Instant)>>> = Mutex::new(BTreeMap::new());

fn unseen_horizon() -> Duration {
    retry_span() + Duration::from_secs(1)
}

fn flight_command_type() -> sedsnet::config::DataType {
    crate::telemetry_schema::data_type("FLIGHT_COMMAND")
}

/// The FLIGHT_COMMANDs that may still be ahead of a new one: SDK pending rows
/// plus those queued but never seen as rows while the router still holds
/// FLIGHT_COMMAND work.
fn unresolved_flight_commands(router: &Router) -> BTreeSet<u64> {
    let mut ahead = pending_flight_commands(router);
    let mut unseen = UNSEEN.lock().unwrap();
    if let Some(ids) = unseen.get_mut(&router_key(router)) {
        ids.retain(|id, at| !ahead.contains(id) && at.elapsed() < unseen_horizon());
        if !ids.is_empty() && !router.has_pending_reliable_tx_type(flight_command_type()) {
            ids.clear();
        }
        ahead.extend(ids.keys().copied());
    }
    ahead
}

/// P4: the notice for a press of this pyro command that must not be sent,
/// if it must not. The block lasts only while the earlier press is held or
/// unresolved in the SDK: it ends when that press is confirmed, runs out of
/// retries (reported "no confirmation from FC") or is dropped unsent, and an
/// unregistered one is forgotten after `unseen_horizon()`.
fn duplicate_pyro(router: &Router, byte: u8, ahead: &BTreeSet<u64>) -> Option<String> {
    let label = label_for(byte);
    if let Some(held) = HELD.lock().unwrap().get(&router_key(router)).and_then(|q| q.iter().find(|h| h.byte == byte).cloned()) {
        return Some(format!(
            "{label} not sent: the {label} pressed {:.1} s ago is held and not sent yet, and a second copy could fire it twice. Pressing again works once that press is sent and confirmed, reported unconfirmed, or reported never sent (at most {} s after it).",
            held.pressed_at.elapsed().as_secs_f64(), PYRO_HOLD_LIMIT.as_secs()
        ));
    }
    let mut submitted = SUBMITTED_PYRO.lock().unwrap();
    let list = submitted.entry(router_key(router)).or_default();
    list.retain(|(id, _, _)| ahead.contains(id));
    let (_, _, at) = list.iter().find(|(_, b, _)| *b == byte)?;
    Some(format!(
        "{label} not sent: the {label} sent {:.1} s ago is still awaiting FC confirmation, and a second copy could fire it twice. Pressing again works as soon as the FC confirms it or its retries run out (the station then reports \"{label}: no confirmation from FC\"). Check FC state first.",
        at.elapsed().as_secs_f64()
    ))
}

fn notify(state: &AppState, message: String) {
    log::warn!("{message}");
    crate::web::emit_notification_warning(state, message);
}

/// Queue one operator flight command now. `ahead` is what was unresolved.
fn send_now(state: &AppState, router: &Router, byte: u8, ahead: &BTreeSet<u64>) -> sedsnet::TelemetryResult<()> {
    crate::telemetry_task::queue_locally_routed_flight_command(state, router, "HITL flight command", &[byte])?;
    if let Some(id) = record_operator_press(state, router, byte, ahead)
        && is_deploy(byte)
    {
        SUBMITTED_PYRO.lock().unwrap().entry(router_key(router)).or_default().push((id, byte, Instant::now()));
    }
    crate::telemetry_task::flush_command_tx(router, "HITL flight command tx");
    Ok(())
}

fn hold(router: &Router, held: Held, front: bool) {
    let mut all = HELD.lock().unwrap();
    let queue = all.entry(router_key(router)).or_default();
    if front {
        queue.push_front(held);
    } else {
        queue.push_back(held);
    }
}

/// The telemetry task's operator HITL flight command (R9: operator presses only).
pub(crate) fn submit_operator_flight_command(state: &AppState, router: &Router, byte: u8) -> sedsnet::TelemetryResult<()> {
    let label = label_for(byte);
    let ahead = unresolved_flight_commands(router);
    if is_deploy(byte)
        && let Some(notice) = duplicate_pyro(router, byte, &ahead)
    {
        notify(state, notice);
        return Ok(());
    }
    let holding = HELD.lock().unwrap().get(&router_key(router)).is_some_and(|q| !q.is_empty());
    if holding || (is_deploy(byte) && !ahead.is_empty()) {
        hold(router, Held { byte, pressed_at: Instant::now(), abort_epoch: counters(state).1 }, false);
        let behind = if holding { "an earlier held operator command" } else { "an earlier flight-computer command that is still awaiting FC confirmation" };
        notify(state, format!(
            "{label} held, not sent yet: it waits behind {behind}. It is sent once no earlier flight-computer command is pending, or dropped with a notice if that takes more than {} s.",
            PYRO_HOLD_LIMIT.as_secs()
        ));
        return Ok(());
    }
    send_now(state, router, byte, &ahead)
}

/// A pyro press is held: the FC reset path treats it as a Deploy in progress.
fn pyro_held(router: &Router) -> bool {
    HELD.lock().unwrap().get(&router_key(router)).is_some_and(|q| q.iter().any(|h| is_deploy(h.byte)))
}

/// Run from the telemetry task: release, or drop with a notice, the oldest
/// held operator command.
fn service_held(state: &AppState, router: &Router) {
    let Some(front) = HELD.lock().unwrap().get(&router_key(router)).and_then(|q| q.front().cloned()) else {
        return;
    };
    let label = label_for(front.byte);
    let age = front.pressed_at.elapsed();
    let pop = || HELD.lock().unwrap().get_mut(&router_key(router)).and_then(|q| q.pop_front());
    // P2
    if counters(state).1 != front.abort_epoch || state.abort_indicator_latched() || crate::gse::manual_stop_pending(state) {
        let dropped: Vec<Held> = HELD.lock().unwrap().remove(&router_key(router)).map(Vec::from).unwrap_or_default();
        for held in dropped {
            notify(state, format!("{} was never sent: an Abort or manual stop came while it was held behind an earlier flight-computer command.", label_for(held.byte)));
        }
        return;
    }
    let ahead = unresolved_flight_commands(router);
    // P1
    if age >= PYRO_HOLD_LIMIT {
        pop();
        notify(state, format!(
            "{label} was never sent: it was held {:.1} s behind an earlier flight-computer command that was neither confirmed nor ran out of retries within {} s. Press it again if it is still needed.",
            age.as_secs_f64(), PYRO_HOLD_LIMIT.as_secs()
        ));
        return;
    }
    if !ahead.is_empty() {
        return;
    }
    pop();
    // P3
    let (effect_seen, fc_admissible) = effect_and_admissible(state, front.byte);
    if effect_seen {
        notify(state, format!("{label} was never sent: the station already sees its effect."));
        return;
    }
    if !(fc_admissible && command_for(front.byte).is_some_and(|c| state.is_command_allowed(&c))) {
        notify(state, format!("{label} was never sent: the flight computer or the station would now refuse it."));
        return;
    }
    match send_now(state, router, front.byte, &ahead) {
        Ok(()) => notify(state, format!(
            "{label} sent {} ms after the press: it was held until the earlier flight-computer command was confirmed or ran out of retries.",
            age.as_millis()
        )),
        Err(err) => notify(state, format!("{label} was never sent: {err}. Press it again if it is still needed.")),
    }
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
        // Proof it never reached the air: a retired head was registered
        // before it and existed until it retired, so the SDK never released
        // this press (one released entry per Ordered type, in order), and the
        // retirement removed it with the head.
        && heads.iter().any(|head| press.before.contains(head))
    {
        // Seen registered, or no FLIGHT_COMMAND left anywhere in the router:
        // it is gone. Otherwise it may still be queued and become the new
        // head, so nothing is decided yet.
        if press.seen_pending || !router.has_pending_reliable_tx_type(flight_command_type()) {
            press.dropped_at = Some(Instant::now());
        } else {
            press.unproven_since = Some(Instant::now());
        }
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
    service_held(state, router);
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
        if p.dropped_at.is_none() && p.unproven_since.is_some() {
            if pending_flight_commands(router).contains(&p.packet_id) {
                // It survived in the transmit queue and is the new head.
                p.unproven_since = None;
                p.seen_pending = true;
            } else if !router.has_pending_reliable_tx_type(flight_command_type()) {
                p.done = true;
                let label = label_for(p.byte);
                drop(newest);
                notify(state, format!("{label} may have been dropped behind an earlier flight-computer command that ran out of retries. The station cannot tell whether it was sent, so it was not re-sent. Check FC state before pressing again."));
            }
            return;
        }
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
    if is_deploy(press.byte) {
        // Provably never transmitted: hold it again (P1..P3 decide), with
        // its original press time and Abort epoch.
        hold(router, Held { byte: press.byte, pressed_at: press.pressed_at, abort_epoch: press.abort_epoch }, true);
        notify(state, format!("{label} was dropped unsent behind an earlier flight-computer command that ran out of retries. It is held and sent once no earlier flight-computer command is pending."));
        return;
    }
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

/// Makes every held command look `by` older (P1 cases).
#[cfg(test)]
pub(crate) fn backdate_held_for_test(router: &Router, by: Duration) {
    if let Some(queue) = HELD.lock().unwrap().get_mut(&router_key(router)) {
        for held in queue {
            held.pressed_at -= by;
        }
    }
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
