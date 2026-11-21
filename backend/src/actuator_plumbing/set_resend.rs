//! A drive change is followed until the actuator's readback confirms it.
//!
//! AB_FILL_DRIVE_SETTINGS is one reliable Ordered stream that carries both
//! the read-only drive and motion queries and the drive SETs. When a frame
//! on it runs out of retries, the SDK retires the stream and drops
//! everything queued behind it, a SET included. In the Renode run
//! final3clock1, a query the actuator answered but never receipted held the
//! stream, and the boot-window SET posted at 33.98 s never reached CAN.
//!
//! So every drive change this station sends, whether from an operator or
//! from the board config resync, is tracked until a nonce-correlated
//! AB_FILL_DRIVE_STATUS shows the requested drive with the identity (bytes
//! 34..35) of one of its sends:
//! - the SET's own reply, or any query reply, confirms it ("applied");
//! - a retired stream (`note_drive_stream_retired`), no reply within
//!   `SET_CONFIRM_WINDOW`, or a stale/busy reply moves it to a check: one
//!   correlated query. A readback without the requested drive sends it
//!   again, with the same drive and saved revision and a new request time,
//!   after a backoff (1, 2, 4 s), up to `SET_MAX_SENDS` sends in all;
//! - every re-send runs the drive-change gates again (primary station,
//!   Startup or Idle, no retraction or manual Stop pending, fresh drive and
//!   board readback from the same actuator boot and Abort epoch, not moving,
//!   outputs off). When they no longer allow it, the change stops with
//!   "Drive change not applied: <reason>";
//! - a re-send is idempotent on the actuator: it saves the drive it may
//!   already hold again, under the newer request time, so the selected drive
//!   never changes. A late copy of an older send is refused there as stale,
//!   and its reply is ignored here.
//!
//! While a send awaits its reply, the background drive query pauses: that
//! reply is the readback, and queries queued behind the SET would only be
//! dropped with it. Superseded queries are never re-sent; a check asks one
//! new query.
//!
//! The board config resync (`board_config_sync`) starts its pushes through
//! the same path and waits while a change is tracked. It counts a whole
//! tracked change as its quick attempt, so only its once-a-minute retry
//! follows a change that gave up here.
use super::*;
use std::collections::BTreeMap;
use std::time::Duration;

/// Wait for a send's reply before checking. One reliable retry span at
/// station tuning is 400 ms x 12 = 4.8 s.
pub(super) const SET_CONFIRM_WINDOW: Duration = Duration::from_secs(6);
/// Wait for a check query's reply before asking again.
pub(super) const SET_CHECK_WINDOW: Duration = Duration::from_secs(6);
/// Sends of one drive change in all: the first one and three re-sends.
pub(super) const SET_MAX_SENDS: u32 = 4;
/// Unanswered check queries after one send before the change gives up.
const SET_MAX_CHECKS: u32 = 2;

/// The wait after a send before the next one: 1, 2, then 4 s.
pub(super) fn set_backoff(sends: u32) -> Duration {
    Duration::from_secs(1u64 << sends.saturating_sub(1).min(2))
}

/// How often each router retired its AB_FILL_DRIVE_SETTINGS stream (or an
/// Ordered stream whose type it could not tell).
static RETIRED: Mutex<BTreeMap<usize, u64>> = Mutex::new(BTreeMap::new());

/// The router worker saw an Ordered AB_FILL_DRIVE_SETTINGS head run out of
/// retries, or could not tell which type did: the SDK dropped what was
/// queued behind it.
pub(crate) fn note_drive_stream_retired(router: &sedsnet::router::Router) {
    let key = router as *const sedsnet::router::Router as usize;
    *RETIRED.lock().unwrap().entry(key).or_default() += 1;
}
pub(super) fn retired_count(state: &AppState) -> u64 {
    state.topology_router.get().map_or(0, |router| {
        let key = router.as_ref() as *const sedsnet::router::Router as usize;
        RETIRED.lock().unwrap().get(&key).copied().unwrap_or(0)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetOrigin {
    Operator,
    Resync,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SetPhase {
    /// The latest send awaits its reply.
    InFlight,
    /// The latest send was dropped, refused as stale/busy or not answered: a
    /// correlated query decides between applied and a re-send.
    Checking {
        query: Option<u64>,
        asked: Option<Instant>,
        /// Stream retirements when the query was queued.
        retired: u64,
        checks: u32,
        /// The drive the query's reply reported, once it came.
        answer: Option<Option<Mode>>,
    },
    Applied,
    /// `transport`: the actuator never confirmed it (rather than a gate or a
    /// refusal stopping it).
    Failed { reason: String, transport: bool },
}

pub(super) struct SetTrack {
    pub(super) origin: SetOrigin,
    pub(super) mode: Mode,
    pub(super) revision: u64,
    /// The operator approved releasing the idle stepper's holding torque.
    release: bool,
    /// Actuator boot and Abort epoch the change was asked against.
    expected: (u64, u32),
    recovery_generation: u64,
    nonces: Vec<u64>,
    /// The identity the actuator reports for each send's request time.
    identities: Vec<u16>,
    pub(super) sends: u32,
    last_sent: Instant,
    /// Stream retirements when the latest send was queued.
    retired: u64,
    pub(super) phase: SetPhase,
    /// A give-up the board config resync has not counted yet.
    give_up_unreported: bool,
}
impl SetTrack {
    pub(super) fn active(&self) -> bool {
        matches!(self.phase, SetPhase::InFlight | SetPhase::Checking { .. })
    }
    pub(super) fn in_flight(&self) -> bool {
        self.phase == SetPhase::InFlight
    }
    fn latest(&self) -> u64 {
        self.nonces.last().copied().unwrap_or(0)
    }
    fn confirms(&self, value: &Status) -> bool {
        value.ready()
            && value.mode == Some(self.mode)
            && self.identities.contains(&value.setting_id)
    }
    fn check() -> SetPhase {
        SetPhase::Checking { query: None, asked: None, retired: 0, checks: 0, answer: None }
    }
}

/// An operator notice to raise once the plumbing lock is released.
pub(super) enum Notice {
    Applied(String),
    Failed(String),
}
pub(super) fn raise(state: &AppState, notice: Option<Notice>) {
    match notice {
        Some(Notice::Applied(message)) => {
            state.add_notification(format!("Fill-line drive: {message}"));
        }
        Some(Notice::Failed(message)) => {
            crate::web::emit_notification_warning(state, format!("Fill-line drive: {message}"));
        }
        None => {}
    }
}

fn label(mode: Mode) -> &'static str {
    mode_label(mode as u8)
}
pub(super) fn applying_message(track: &SetTrack) -> String {
    let what = match track.origin {
        SetOrigin::Operator => "Applying",
        SetOrigin::Resync => "Re-applying the station's saved",
    };
    match track.sends {
        0 | 1 => format!("{what} {} drive… waiting for the actuator's readback.", label(track.mode)),
        sends => format!(
            "{what} {} drive… (retry {}): the actuator has not confirmed it yet.",
            label(track.mode),
            sends - 1
        ),
    }
}
fn applied(inner: &mut Inner) -> Option<Notice> {
    let track = inner.set_track.as_mut()?;
    track.phase = SetPhase::Applied;
    let message = match track.sends {
        0 | 1 => format!("applied: the actuator's readback confirms the {} drive.", label(track.mode)),
        sends => format!(
            "applied: the actuator's readback confirms the {} drive after {} re-send{}.",
            label(track.mode),
            sends - 1,
            if sends == 2 { "" } else { "s" }
        ),
    };
    let notify = track.sends > 1;
    inner.pending_setting = None;
    inner.message = format!("Drive change {message}");
    notify.then(|| Notice::Applied(format!("drive change {message}")))
}
fn failed(inner: &mut Inner, reason: String, transport: bool) -> Option<Notice> {
    let track = inner.set_track.as_mut()?;
    let message = format!("Drive change not applied: {reason}");
    track.phase = SetPhase::Failed { reason, transport };
    track.give_up_unreported = transport;
    inner.pending_setting = None;
    inner.message = message.clone();
    Some(Notice::Failed(message))
}

/// A drive change was admitted to the SDK: follow it.
pub(super) fn begin(
    state: &AppState,
    inner: &mut Inner,
    origin: SetOrigin,
    request: &Request,
    release: bool,
    stamp_ms: u64,
    retired: u64,
    now: Instant,
) -> Option<Notice> {
    let mode = request.mode?;
    let track = SetTrack {
        origin,
        mode,
        revision: inner.desired.revision,
        release,
        expected: (request.generation, request.epoch),
        recovery_generation: crate::gse::automatic_generation(state),
        nonces: vec![request.nonce],
        identities: vec![setting_identity(stamp_ms)],
        sends: 1,
        last_sent: now,
        retired,
        phase: SetPhase::InFlight,
        give_up_unreported: false,
    };
    inner.message = applying_message(&track);
    inner.set_track = Some(track);
    // A tracked change is pending until confirmed or given up, which holds
    // motion, Launch and the config resync back meanwhile.
    inner.pending_setting.get_or_insert(request.nonce);
    reply_already_in(inner, request.nonce)
}
/// The SET's reply can beat its tracking (the router worker runs alongside).
fn reply_already_in(inner: &mut Inner, nonce: u64) -> Option<Notice> {
    let value = inner.observed.as_ref().map(|row| row.value).filter(|value| value.nonce == nonce)?;
    on_reply(inner, true, &value, None)
}

/// A correlated AB_FILL_DRIVE_STATUS for a tracked change (`is_set`: the
/// reply to one of its SETs, otherwise to a query).
pub(super) fn on_reply(
    inner: &mut Inner,
    is_set: bool,
    value: &Status,
    refusal: Option<String>,
) -> Option<Notice> {
    let track = inner.set_track.as_mut().filter(|track| track.active())?;
    if is_set && !track.nonces.contains(&value.nonce) {
        return None;
    }
    if value.result == 0 && track.confirms(value) {
        return applied(inner);
    }
    if !is_set {
        // The check's own query: its reply decides between a re-send and
        // waiting (a later readback does not replace it).
        if let SetPhase::Checking { query: Some(query), answer, .. } = &mut track.phase
            && *query == value.nonce
        {
            *answer = Some(value.mode);
        }
        return None;
    }
    if value.nonce != track.latest() || value.result == 0 {
        // An older send's late reply (refused as stale once a newer one
        // applied): not this change's outcome.
        return None;
    }
    match value.result {
        // Stale (sent against an older actuator context, or too late) or
        // busy (an earlier send of this change is still being saved).
        4 | 5 => {
            track.phase = SetTrack::check();
            None
        }
        _ => {
            let reason = match refusal {
                Some(reason) => format!("the actuator refused it ({}: {reason}).", value.result_name()),
                None => format!("the actuator refused it ({}).", value.result_name()),
            };
            failed(inner, reason, false)
        }
    }
}

enum Step {
    Query,
    Resend,
}

/// Advance the tracked change: check, re-send or give up. Runs on the board
/// config resync's blocking worker (re-sends save the selection to disk).
pub(crate) fn service(state: &AppState, now: Instant) {
    let retired = retired_count(state);
    let step = {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        let revision = inner.desired.revision;
        let Some(track) = inner.set_track.as_mut().filter(|track| track.active()) else {
            return;
        };
        if track.revision != revision {
            // A newer drive change replaced this one.
            inner.set_track = None;
            return;
        }
        if track.recovery_generation != crate::gse::automatic_generation(state) {
            let notice = failed(&mut inner, "a station recovery reset cancelled it.".into(), false);
            drop(inner);
            return raise(state, notice);
        }
        if track.in_flight() {
            if retired <= track.retired && now < track.last_sent + SET_CONFIRM_WINDOW {
                return;
            }
            track.phase = SetTrack::check();
        }
        let SetPhase::Checking { query, asked, retired: asked_retired, checks, answer } = track.phase.clone() else {
            return;
        };
        if let Some(board_mode) = answer {
            // The check's own readback did not confirm the change.
            if track.sends >= SET_MAX_SENDS {
                let reason = format!(
                    "the actuator still reports the {} drive after {} sends of the {} drive.",
                    board_mode.map_or("unknown", label),
                    track.sends,
                    label(track.mode)
                );
                let notice = failed(&mut inner, reason, true);
                drop(inner);
                return raise(state, notice);
            }
            if now < track.last_sent + set_backoff(track.sends) {
                return;
            }
            Step::Resend
        } else if query.is_none()
            || asked.is_none_or(|asked| now >= asked + SET_CHECK_WINDOW)
            || retired > asked_retired
        {
            if checks >= SET_MAX_CHECKS {
                let reason = format!(
                    "no actuator drive readback after {} send{} of the {} drive.",
                    track.sends,
                    if track.sends == 1 { "" } else { "s" },
                    label(track.mode)
                );
                let notice = failed(&mut inner, reason, true);
                drop(inner);
                return raise(state, notice);
            }
            Step::Query
        } else {
            return;
        }
    };
    // The station's own gates are checked before asking the actuator
    // anything; the readback gates again before each re-send.
    if let Some(reason) = change_state_block(state) {
        let notice = {
            let mut inner = state.plumbing_drive.inner.lock().unwrap();
            if inner.set_track.as_ref().is_some_and(|track| track.active()) {
                failed(&mut inner, gate_reason(reason), false)
            } else {
                None
            }
        };
        return raise(state, notice);
    }
    match step {
        Step::Query => {
            // Recorded before it is queued: its reply can beat this thread.
            let nonce = rand::random::<u64>().max(1);
            let asked_retired = retired_count(state);
            let record = |query: Option<u64>, count: u32| {
                let mut inner = state.plumbing_drive.inner.lock().unwrap();
                if let Some(track) = inner.set_track.as_mut().filter(|track| track.active())
                    && let SetPhase::Checking { checks, .. } = track.phase
                {
                    track.phase = SetPhase::Checking {
                        query,
                        asked: Some(now),
                        retired: asked_retired,
                        checks: checks + count,
                        answer: None,
                    };
                }
            };
            record(Some(nonce), 1);
            if let Err(error) = queue_check_query(state, nonce) {
                log::warn!("drive change check query was not admitted: {error}");
                record(None, 0);
            }
        }
        Step::Resend => {
            if let Err(reason) = resend(state, now) {
                let notice = {
                    let mut inner = state.plumbing_drive.inner.lock().unwrap();
                    if inner.set_track.as_ref().is_some_and(|track| track.active()) {
                        failed(&mut inner, reason, false)
                    } else {
                        None
                    }
                };
                raise(state, notice);
            }
        }
    }
}

/// Send the tracked change again under the drive-change gates.
fn resend(state: &AppState, now: Instant) -> Result<(), String> {
    let (mode, release, expected) = {
        let inner = state.plumbing_drive.inner.lock().unwrap();
        let track = inner.set_track.as_ref().filter(|track| track.active()).ok_or("superseded.")?;
        (track.mode, track.release, track.expected)
    };
    send_set(state, SendKind::Resend { mode, release, expected }, now)
}

/// A query for the check, outside the background query's one-second budget.
fn queue_check_query(state: &AppState, nonce: u64) -> Result<(), String> {
    let router = state.topology_router.get().ok_or("Telemetry router unavailable")?;
    let prepared = prepare_request(
        state,
        Request { mode: None, nonce, generation: 0, epoch: 0, issuer: state.plumbing_drive.issuer },
        crate::system_clock::control_packet_stamp(router),
    )?;
    queue_prepared(state, prepared, false, true)
}

pub(super) enum SendKind {
    /// The board config resync re-applies the saved drive.
    Resync,
    /// The tracked change is sent again.
    Resend { mode: Mode, release: bool, expected: (u64, u32) },
}

/// Send the saved drive, keeping its revision, under a new request time
/// (the actuator saves it as the selection's identity). The resync starts a
/// new tracked change; a re-send continues the tracked one.
pub(super) fn send_set(state: &AppState, kind: SendKind, now: Instant) -> Result<(), String> {
    let generation = crate::gse::capture_operator_origin(state).ok_or("operator admission is closed.")?;
    let admission = crate::gse::plumbing_admission_ticket(state);
    let router = state.topology_router.get().ok_or("Telemetry router unavailable")?;
    let origin = crate::system_clock::required_control_network_stamp(router)
        .map_err(|_| "network UTC is required before sending the drive.")?;
    let _settings = state.plumbing_drive.settings_gate.lock().unwrap();
    let (gate_mode, release, own) = match kind {
        SendKind::Resync => (None, false, false),
        SendKind::Resend { mode, release, .. } => (Some(mode), release, true),
    };
    if let Some(reason) = change_block_reason(state, gate_mode, release, own) {
        return Err(match kind {
            SendKind::Resync => reason,
            SendKind::Resend { .. } => gate_reason(reason),
        });
    }
    let _owner = crate::gse::claim_plumbing_settings_dispatch(state, generation)
        .ok_or("a station recovery cancelled it.")?;
    let _busy = SettingsBusy(&state.plumbing_drive);
    crate::gse::with_plumbing_admission(state, admission, || ()).map_err(str::to_owned)?;
    let (desired, expected) = {
        let inner = state.plumbing_drive.inner.lock().unwrap();
        let row = inner
            .observed
            .as_ref()
            .filter(|row| fresh(state, row))
            .ok_or("the actuator's drive readback expired.")?;
        let expected = (row.value.generation, row.value.epoch);
        if let SendKind::Resend { expected: asked, .. } = kind
            && asked != expected
        {
            return Err("the actuator restarted or its Abort state changed since the change was requested.".into());
        }
        if origin <= inner.desired.source() || origin <= inner.last_motion() {
            return Err("its request time predates the saved selection.".into());
        }
        (Desired { source_ms: origin.ms as u64, source_epoch: origin.epoch, ..inner.desired }, expected)
    };
    let mode = Mode::decode(desired.mode).ok_or("the saved drive selection is unsupported.")?;
    if let SendKind::Resend { mode: tracked, .. } = kind
        && tracked != mode
    {
        return Err("the saved drive changed.".into());
    }
    let nonce = rand::random::<u64>().max(1);
    if let Err(message) = desired.save(&state.plumbing_drive.path, nonce) {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        inner.durable = false;
        inner.storage_error = Some(message.clone());
        return Err(message);
    }
    {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        inner.desired = desired;
        inner.durable = true;
    }
    let request = Request {
        mode: Some(mode),
        nonce,
        generation: expected.0,
        epoch: expected.1,
        issuer: state.plumbing_drive.issuer,
    };
    let prepared = prepare_request(state, request, origin)?;
    let retired = retired_count(state);
    let queued = crate::gse::with_plumbing_admission(state, admission, || {
        queue_prepared(state, prepared, true, false).map(|()| {
            state.last_command_ms.lock().unwrap().remove("dedup:RetractPlumbing");
        })
    })
    .map_err(str::to_owned)?;
    let mut inner = state.plumbing_drive.inner.lock().unwrap();
    let notice = match kind {
        SendKind::Resync => {
            queued?;
            begin(state, &mut inner, SetOrigin::Resync, &request, false, origin.ms as u64, retired, now)
        }
        SendKind::Resend { .. } => {
            let Some(track) = inner.set_track.as_mut().filter(|track| track.active()) else {
                return queued;
            };
            // A send the SDK refused counts too, so re-sends stay bounded;
            // the next check asks again.
            track.sends += 1;
            track.last_sent = now;
            track.retired = retired;
            if queued.is_ok() {
                track.nonces.push(nonce);
                track.identities.push(setting_identity(origin.ms as u64));
                track.phase = SetPhase::InFlight;
            } else {
                track.phase = SetTrack::check();
            }
            inner.message = applying_message(inner.set_track.as_ref().unwrap());
            match queued {
                Ok(()) => reply_already_in(&mut inner, nonce),
                Err(error) => {
                    log::warn!("drive change re-send was not admitted: {error}");
                    None
                }
            }
        }
    };
    drop(inner);
    raise(state, notice);
    Ok(())
}

/// A gate's reason as the end of "Drive change not applied: ...".
fn gate_reason(reason: String) -> String {
    let reason = reason.trim_start_matches("Deferred: ");
    let mut chars = reason.chars();
    chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect())
}

/// The reply to an earlier send of a tracked change that a newer send
/// replaced or that is already confirmed: the actuator refuses such a late
/// copy as stale (or busy). It is neither the change's outcome nor the
/// actuator's current drive readback.
pub(super) fn superseded_send(inner: &Inner, value: &Status) -> bool {
    value.result != 0
        && inner.set_track.as_ref().is_some_and(|track| {
            track.nonces.contains(&value.nonce) && (!track.active() || value.nonce != track.latest())
        })
}

/// The change's state for the fill-drive panel.
pub(super) fn view(inner: &Inner) -> Value {
    let Some(track) = inner.set_track.as_ref() else {
        return Value::Null;
    };
    let (state, reason) = match &track.phase {
        SetPhase::InFlight | SetPhase::Checking { .. } => ("applying", None),
        SetPhase::Applied => ("applied", None),
        SetPhase::Failed { reason, .. } => ("failed", Some(reason.as_str())),
    };
    json!({
        "state": state,
        "mode": track.mode.name(),
        "revision": track.revision,
        "origin": match track.origin { SetOrigin::Operator => "operator", SetOrigin::Resync => "resync" },
        "sends": track.sends,
        "retry": track.sends.saturating_sub(1),
        "reason": reason,
        "message": match &track.phase {
            SetPhase::InFlight | SetPhase::Checking { .. } => applying_message(track),
            _ => inner.message.clone(),
        },
    })
}

/// The tracked change gave up without the actuator confirming it, once.
pub(crate) fn take_give_up(state: &AppState) -> bool {
    let mut inner = state.plumbing_drive.inner.lock().unwrap();
    inner
        .set_track
        .as_mut()
        .is_some_and(|track| std::mem::take(&mut track.give_up_unreported))
}

/// The operator asked again for the change already being applied.
pub(super) fn already_applying(inner: &Inner, mode: Mode) -> Option<String> {
    let track = inner.set_track.as_ref().filter(|track| {
        track.active() && track.mode == mode && track.revision == inner.desired.revision
    })?;
    Some(format!("{} No new setting was saved.", applying_message(track)))
}
