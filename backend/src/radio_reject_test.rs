//! Radio test mode: the primary rejects its direct AV-bay radio link, so the
//! RRLS relay and the RRLS takeover can be tested with the rocket in range.
//!
//! The rejection sits in the radio driver (comms.rs), on both sides of the
//! raw-UART framing, and nowhere else: every other part of the station sees
//! exactly what a real loss of the link looks like. A rejected received frame
//! is never decoded (the relay decision, routes, telemetry and the air
//! scheduler never see it). A rejected transmitted frame is reported written
//! but never reaches the modem, so pacing, retries and delivery tracking see a
//! frame lost on the air. The umbilical (Pico-Fi) and every other link are
//! untouched.
//!
//! Two variants:
//! * Force relay: received AV-bay frames (anything not in an RR envelope,
//!   which only the AV bay sends, and the RF board's RR kind-14 profile
//!   replies) are dropped, and nothing addressed to the AV bay leaves (RR
//!   kinds 5/6/14, the only envelopes the RF board accepts, and any bare
//!   frame). The RRLS link stays up: HEALTH, READY, HOST_CONTROL, settings,
//!   STATION_STATE, budget control, and the relayed uplink (UP_*) and
//!   downlink (DOWN_*). The primary asks for the relay after its usual 3 s.
//! * Silent primary (failover): nothing is received or transmitted on the
//!   radio, so the RRLS hears the primary go silent and takes over after its
//!   usual 6 s. The silence begins SILENT_ARM_MS after the start, so the
//!   STATION_STATE copies announcing the test reach the RRLS first. Turning
//!   it off tests the hand-back.
//!
//! Safety:
//! * Administrator only (or an operator whose command list names
//!   RadioRejectTest explicitly), with a confirmation. Anyone who may send
//!   commands may stop it.
//! * Only on the ground in Startup, Idle or PreFill. It ends by itself the
//!   moment the station's flight state leaves those, after its timeout
//!   (default 10 min) and on a restart (it is never persisted; a start left
//!   open in the journal is closed with reason "restart").
//! * Abort (any operator or automatic Abort, from the station's one retained
//!   Abort intent):
//!   - Silent primary: lifted at once, before the Abort is published, so the
//!     Abort goes out on the direct link as normal. There is no relay to test
//!     in this variant; the RRLS's own Abort is the takeover's to test.
//!   - Force relay: the Abort goes through the RRLS relay, which is what is
//!     being tested. If the relay is not carrying the primary's uplink at
//!     that moment, the rejection is lifted at once. Otherwise, once RF, FC
//!     and PB (those the mode uses) all read Aborted the test ends (reason
//!     abort_relayed); if they do not within ABORT_ESCAPE_MS, the rejection
//!     is lifted and the primary returns to its direct path at once (reason
//!     abort_escape), so the SDK's remaining retries of the same ABORT and
//!     the station's Abort re-send go direct. ABORT_ESCAPE_MS is inside the
//!     ABORT's 4.8 s reliable delivery window.
//! * A banner on the primary and the RRLS; the state is in the STATION_STATE
//!   broadcast. Every start and stop is logged, notified and appended to
//!   radio_reject_test.jsonl beside the station database.
use crate::{auth::Permission, state::AppState, types::FlightState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The Actions-tab entry (an HTTP action, not an /api/command).
pub(crate) const COMMAND: &str = "RadioRejectTest";
pub(crate) const PATH: &str = "/api/radio/reject-test";
pub(crate) const DEFAULT_DURATION_S: u64 = 600;
pub(crate) const MIN_DURATION_S: u64 = 30;
pub(crate) const MAX_DURATION_S: u64 = 3_600;
/// Silent primary: how long the station still transmits after the start, so
/// its STATION_STATE copies (now, +250 ms, +750 ms) announce the test.
pub(crate) const SILENT_ARM_MS: u64 = 1_500;
/// Force relay: how long an Abort may take through the relay before the
/// station lifts the rejection and sends direct.
pub(crate) const ABORT_ESCAPE_MS: u64 = 4_000;
pub(crate) const BANNER: &str = "RADIO TEST: direct AV-bay link rejected";
const JOURNAL_FILE: &str = "radio_reject_test.jsonl";
const HISTORY: usize = 10;
/// RF, FC and PB in board_abort::BOARDS order.
const AV_BAY_MASK: u8 = 0b111;
/// RR envelope kinds the RF board accepts (AV-bay data/command, profile query).
const AV_BAY_KINDS: [u8; 3] = [5, 6, 14];
const RF_PROFILE_KIND: u8 = 14;

pub(crate) const FILTER_OFF: u8 = 0;
pub(crate) const FILTER_RELAY: u8 = 1;
pub(crate) const FILTER_SILENT: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Variant {
    ForceRelay,
    SilentPrimary,
}
impl Variant {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::ForceRelay => 1,
            Self::SilentPrimary => 2,
        }
    }
    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::ForceRelay),
            2 => Some(Self::SilentPrimary),
            _ => None,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ForceRelay => "Force relay",
            Self::SilentPrimary => "Force failover (silent primary)",
        }
    }
    fn filter(self) -> u8 {
        match self {
            Self::ForceRelay => FILTER_RELAY,
            Self::SilentPrimary => FILTER_SILENT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    Operator,
    Expired,
    FlightState,
    /// Silent primary: an Abort lifted the silence at once.
    AbortLift,
    /// Force relay: the AV bay read the Abort back through the relay.
    AbortRelayed,
    /// Force relay: the relay did not carry the Abort in time (or was not
    /// up); the station went direct.
    AbortEscape,
    /// The station restarted while the test was on.
    Restart,
}
impl StopReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Operator => "stopped by an operator",
            Self::Expired => "timed out",
            Self::FlightState => "the flight state left Startup/Idle/PreFill",
            Self::AbortLift => "an Abort lifted it (silent primary)",
            Self::AbortRelayed => "an Abort was read back through the relay",
            Self::AbortEscape => {
                "an Abort was not confirmed through the relay in time; the station went direct"
            }
            Self::Restart => "the station restarted",
        }
    }
}

/// Only on the ground, before fill.
pub(crate) fn allowed_state(state: FlightState) -> bool {
    matches!(
        state,
        FlightState::Startup | FlightState::Idle | FlightState::PreFill
    )
}

fn rr_kind(payload: &[u8]) -> Option<u8> {
    // RR v2: one marker byte (0xE0..=0xFF) carries the kind; anything else
    // (or an unknown marker) is a bare frame.
    crate::rrls::kind_of(payload)
}

/// Whether `filter` rejects a frame received on the radio. Anything outside
/// an RR envelope comes from the AV bay (SDK frames, RF window
/// advertisements), and so do RR kind-14 profile replies.
pub(crate) fn ingress_rejected(filter: u8, payload: &[u8]) -> bool {
    match filter {
        FILTER_RELAY => rr_kind(payload).is_none_or(|kind| kind == RF_PROFILE_KIND),
        FILTER_SILENT => true,
        _ => false,
    }
}

/// Whether `filter` rejects a frame this station would transmit: everything
/// the RF board would accept (RR kinds 5/6/14, any bare frame) under force
/// relay, everything when silent.
pub(crate) fn egress_rejected(filter: u8, payload: &[u8]) -> bool {
    match filter {
        FILTER_RELAY => rr_kind(payload).is_none_or(|kind| AV_BAY_KINDS.contains(&kind)),
        FILTER_SILENT => true,
        _ => false,
    }
}

#[cfg(not(test))]
static FILTER: AtomicU8 = AtomicU8::new(FILTER_OFF);
// Tests run in parallel threads of one process: each test thread has its own
// filter and controller, so a test's radio test never filters another's radio.
#[cfg(test)]
thread_local! {
    static FILTER: AtomicU8 = const { AtomicU8::new(FILTER_OFF) };
}
static RX_REJECTED: AtomicU64 = AtomicU64::new(0);
static TX_REJECTED: AtomicU64 = AtomicU64::new(0);

#[cfg(not(test))]
fn with_filter<R>(f: impl FnOnce(&AtomicU8) -> R) -> R {
    f(&FILTER)
}
#[cfg(test)]
fn with_filter<R>(f: impl FnOnce(&AtomicU8) -> R) -> R {
    FILTER.with(f)
}

/// The radio driver's current filter (FILTER_*).
pub(crate) fn active_filter() -> u8 {
    with_filter(|filter| filter.load(Ordering::Acquire))
}
/// Radio driver, receive side: false drops the frame.
pub(crate) fn admit_rx(payload: &[u8]) -> bool {
    if ingress_rejected(active_filter(), payload) {
        RX_REJECTED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}
/// Radio driver, transmit side: false loses the frame on the air.
pub(crate) fn admit_tx(payload: &[u8]) -> bool {
    if egress_rejected(active_filter(), payload) {
        TX_REJECTED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

/// Who asked, and whether they may.
pub(crate) struct Actor {
    pub name: String,
    pub may_start: bool,
    pub may_stop: bool,
}
impl Actor {
    pub(crate) fn of(principal: &crate::auth::AuthPrincipal) -> Self {
        let signed_in = !principal.anonymous && principal.username.is_some();
        let listed = principal.permissions.send_commands
            && principal
                .command_access
                .allowed_commands
                .iter()
                .any(|cmd| cmd == COMMAND);
        Self {
            name: principal.username.clone().unwrap_or_default(),
            may_start: principal.is_admin() || (signed_in && listed),
            may_stop: principal.is_admin() || (signed_in && principal.permissions.send_commands),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Record {
    pub event: String,
    pub utc_ms: u64,
    pub variant: Variant,
    #[serde(default)]
    pub actor: String,
    pub started_utc_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_rejected: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_rejected: Option<u64>,
}

#[derive(Clone, Debug)]
struct Active {
    variant: Variant,
    actor: String,
    started_ms: u64,
    started_utc_ms: u64,
    duration_s: u64,
    /// Force relay: an Abort since `ms`.
    abort_since: Option<u64>,
    rx_base: u64,
    tx_base: u64,
}
impl Active {
    fn expires_ms(&self) -> u64 {
        self.started_ms
            .saturating_add(self.duration_s.saturating_mul(1_000))
    }
}

/// Side effects the caller owes after a change (journal, log, notification,
/// return to the direct path).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Event {
    Started(Record),
    Stopped(Record),
    /// Force relay escape: leave relay mode now.
    GoDirect,
}

#[derive(Debug)]
pub(crate) struct Refusal(pub StatusCode, pub String);

/// What the station looks like to the controller on a tick.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inputs {
    pub flight_state: FlightState,
    /// RF, FC and PB (those the mode uses) all read Aborted.
    pub abort_confirmed: bool,
    /// The RRLS relay carries the primary's uplink (Mesh::active).
    pub relay_engaged: bool,
}

/// The test mode's state. Starts off; never persisted.
#[derive(Debug, Default)]
pub(crate) struct Controller {
    active: Option<Active>,
    relay_engaged: bool,
    pending: Vec<Event>,
    history: VecDeque<Record>,
}

impl Controller {
    pub(crate) fn variant(&self) -> Option<Variant> {
        self.active.as_ref().map(|a| a.variant)
    }
    /// The radio filter now.
    pub(crate) fn filter(&self, now_ms: u64) -> u8 {
        match self.active.as_ref() {
            None => FILTER_OFF,
            Some(a)
                if a.variant == Variant::SilentPrimary
                    && now_ms < a.started_ms.saturating_add(SILENT_ARM_MS) =>
            {
                FILTER_OFF
            }
            Some(a) => a.variant.filter(),
        }
    }
    /// Why it cannot start now, if anything (permission aside).
    pub(crate) fn block_reason(
        &self,
        flight_state: FlightState,
        radio: Result<(), String>,
    ) -> Option<String> {
        if let Err(reason) = radio {
            return Some(reason);
        }
        if !allowed_state(flight_state) {
            return Some(format!(
                "Only on the ground in Startup, Idle or PreFill; the station is in {flight_state:?}."
            ));
        }
        None
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "explicit inputs keep the state machine testable without globals"
    )]
    pub(crate) fn start(
        &mut self,
        actor: &Actor,
        variant: Variant,
        duration_s: Option<u64>,
        confirm: bool,
        flight_state: FlightState,
        radio: Result<(), String>,
        now_ms: u64,
        utc_ms: u64,
    ) -> Result<Record, Refusal> {
        if !actor.may_start {
            return Err(Refusal(
                StatusCode::FORBIDDEN,
                "Only an administrator (or an operator allowed RadioRejectTest) may start the radio test.".into(),
            ));
        }
        if !confirm {
            return Err(Refusal(
                StatusCode::BAD_REQUEST,
                "Confirm the radio test: the station stops hearing the AV bay directly.".into(),
            ));
        }
        if let Some(active) = &self.active {
            return Err(Refusal(
                StatusCode::CONFLICT,
                format!(
                    "The radio test is already on ({}); stop it first.",
                    active.variant.label()
                ),
            ));
        }
        if let Some(reason) = self.block_reason(flight_state, radio) {
            return Err(Refusal(StatusCode::CONFLICT, reason));
        }
        let duration_s = duration_s.unwrap_or_else(default_duration_s);
        if !(MIN_DURATION_S..=MAX_DURATION_S).contains(&duration_s) {
            return Err(Refusal(
                StatusCode::BAD_REQUEST,
                format!("The timeout must be {MIN_DURATION_S}-{MAX_DURATION_S} s."),
            ));
        }
        self.active = Some(Active {
            variant,
            actor: actor.name.clone(),
            started_ms: now_ms,
            started_utc_ms: utc_ms,
            duration_s,
            abort_since: None,
            rx_base: RX_REJECTED.load(Ordering::Relaxed),
            tx_base: TX_REJECTED.load(Ordering::Relaxed),
        });
        let record = Record {
            event: "start".into(),
            utc_ms,
            variant,
            actor: actor.name.clone(),
            started_utc_ms: utc_ms,
            duration_s: Some(duration_s),
            reason: None,
            stopped_by: None,
            rx_rejected: None,
            tx_rejected: None,
        };
        self.push(Event::Started(record.clone()));
        Ok(record)
    }
    pub(crate) fn stop(
        &mut self,
        reason: StopReason,
        by: Option<&str>,
        utc_ms: u64,
    ) -> Option<Record> {
        let active = self.active.take()?;
        let record = Record {
            event: "stop".into(),
            utc_ms,
            variant: active.variant,
            actor: active.actor,
            started_utc_ms: active.started_utc_ms,
            duration_s: Some(active.duration_s),
            reason: Some(reason),
            stopped_by: by.map(str::to_owned),
            rx_rejected: Some(
                RX_REJECTED
                    .load(Ordering::Relaxed)
                    .saturating_sub(active.rx_base),
            ),
            tx_rejected: Some(
                TX_REJECTED
                    .load(Ordering::Relaxed)
                    .saturating_sub(active.tx_base),
            ),
        };
        self.push(Event::Stopped(record.clone()));
        Some(record)
    }
    /// An Abort was requested (the retained Abort intent). Silent primary:
    /// lifted now. Force relay: through the relay when it is up, otherwise
    /// lifted now; tick decides the rest.
    pub(crate) fn note_abort(&mut self, now_ms: u64, utc_ms: u64) {
        let Some(variant) = self.variant() else {
            return;
        };
        match variant {
            Variant::SilentPrimary => {
                self.stop(StopReason::AbortLift, None, utc_ms);
            }
            Variant::ForceRelay if !self.relay_engaged => {
                self.stop(StopReason::AbortEscape, None, utc_ms);
                self.push(Event::GoDirect);
            }
            Variant::ForceRelay => {
                if let Some(active) = self.active.as_mut() {
                    active.abort_since.get_or_insert(now_ms);
                }
            }
        }
    }
    pub(crate) fn tick(&mut self, inputs: Inputs, now_ms: u64, utc_ms: u64) {
        self.relay_engaged = inputs.relay_engaged;
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if let Some(since) = active.abort_since {
            if inputs.abort_confirmed {
                self.stop(StopReason::AbortRelayed, None, utc_ms);
            } else if now_ms.saturating_sub(since) >= ABORT_ESCAPE_MS {
                self.stop(StopReason::AbortEscape, None, utc_ms);
                self.push(Event::GoDirect);
            }
            // An Abort in flight owns the outcome: its Aborted flight state
            // does not end the test before the relay had its chance.
            return;
        }
        if now_ms >= active.expires_ms() {
            self.stop(StopReason::Expired, None, utc_ms);
        } else if !allowed_state(inputs.flight_state) {
            self.stop(StopReason::FlightState, None, utc_ms);
        }
    }
    fn push(&mut self, event: Event) {
        if let Event::Started(record) | Event::Stopped(record) = &event {
            self.history.push_front(record.clone());
            self.history.truncate(HISTORY);
        }
        self.pending.push(event);
    }
    pub(crate) fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending)
    }
    /// STATION_STATE: variant code and whole minutes left (rounded up).
    pub(crate) fn wire(&self, now_ms: u64) -> (u8, u8) {
        self.active.as_ref().map_or((0, 0), |a| {
            let left_ms = a.expires_ms().saturating_sub(now_ms);
            (a.variant.code(), left_ms.div_ceil(60_000).min(255) as u8)
        })
    }
    pub(crate) fn view(&self, now_ms: u64, utc_ms: u64) -> Value {
        let active = self.active.as_ref().map(|a| {
            let left_ms = a.expires_ms().saturating_sub(now_ms);
            json!({
                "variant": a.variant,
                "variant_label": a.variant.label(),
                "actor": a.actor,
                "started_utc_ms": a.started_utc_ms,
                "duration_s": a.duration_s,
                "expires_utc_ms": utc_ms.saturating_add(left_ms),
                "remaining_s": left_ms.div_ceil(1_000),
                "filtering": self.filter(now_ms) != FILTER_OFF,
                "abort_pending_ms": a.abort_since.map(|since| now_ms.saturating_sub(since)),
                "relay_engaged": self.relay_engaged,
                "rx_rejected": RX_REJECTED.load(Ordering::Relaxed).saturating_sub(a.rx_base),
                "tx_rejected": TX_REJECTED.load(Ordering::Relaxed).saturating_sub(a.tx_base),
            })
        });
        json!({
            "active": active,
            "banner": self.active.as_ref().map(|a| format!("{BANNER} ({})", a.variant.label())),
            "history": self.history,
        })
    }
}

pub(crate) fn default_duration_s() -> u64 {
    std::env::var("GS_RADIO_REJECT_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| (MIN_DURATION_S..=MAX_DURATION_S).contains(v))
        .unwrap_or(DEFAULT_DURATION_S)
}

const OFF: Controller = Controller {
    active: None,
    relay_engaged: false,
    pending: Vec::new(),
    history: VecDeque::new(),
};
#[cfg(not(test))]
static CONTROLLER: Mutex<Controller> = Mutex::new(OFF);
#[cfg(test)]
thread_local! {
    static CONTROLLER: Mutex<Controller> = const { Mutex::new(OFF) };
}
static JOURNAL: OnceLock<PathBuf> = OnceLock::new();
static CLOCK: OnceLock<Instant> = OnceLock::new();

#[cfg(not(test))]
fn with_controller<R>(f: impl FnOnce(&mut Controller) -> R) -> R {
    f(&mut CONTROLLER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}
#[cfg(test)]
fn with_controller<R>(f: impl FnOnce(&mut Controller) -> R) -> R {
    CONTROLLER.with(|controller| {
        f(&mut controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    })
}
fn now_ms() -> u64 {
    crate::simulation_clock_ms()
        .unwrap_or_else(|| CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64)
}
fn utc_ms() -> u64 {
    crate::telemetry_task::get_current_timestamp_ms()
}
fn sync_filter(controller: &Controller, now: u64) {
    with_filter(|filter| filter.store(controller.filter(now), Ordering::Release));
}

/// The retained Abort intent was requested (manual_abort_intent). Runs
/// before the Abort is published: a silent primary is audible again for it.
pub(crate) fn note_operator_abort() {
    with_controller(|controller| {
        if controller.variant().is_none() {
            return;
        }
        let now = now_ms();
        controller.note_abort(now, utc_ms());
        sync_filter(controller, now);
    });
}

/// STATION_STATE bytes: (variant code, minutes left).
pub(crate) fn wire_state() -> (u8, u8) {
    if crate::rrls::is_rrls() {
        return (0, 0);
    }
    with_controller(|controller| controller.wire(now_ms()))
}

/// Why this station cannot run the test, if anything (other than the flight
/// state and permission).
fn radio_available() -> Result<(), String> {
    if crate::rrls::is_rrls() {
        return Err("The radio test runs on the primary ground station.".into());
    }
    let Some(mesh) = crate::rrls::settings_transport() else {
        return Err("Radio recovery (the RRLS mesh on the raw-UART radio) is not configured on this station.".into());
    };
    if mesh.lock().unwrap().role != crate::rrls::Role::Primary {
        return Err("The radio test runs on the primary ground station.".into());
    }
    Ok(())
}

fn abort_confirmed(state: &AppState) -> bool {
    let aborted = state.board_abort.masks().0;
    let scope = crate::board_recovery::scope::mode_scope(crate::station_mode::local_mode());
    scope & AV_BAY_MASK & !aborted == 0
}

/// Journal, log and notify `events`; return to the direct path on escape.
fn apply_events(state: &AppState, events: Vec<Event>) {
    for event in events {
        match &event {
            Event::Started(record) => {
                log::warn!(
                    "Radio test ON by {}: {} for {} s; the direct AV-bay link is rejected",
                    record.actor,
                    record.variant.label(),
                    record.duration_s.unwrap_or_default()
                );
                state.add_notification(format!(
                    "{BANNER}: {} started by {} for {} min.",
                    record.variant.label(),
                    record.actor,
                    record.duration_s.unwrap_or_default().div_ceil(60)
                ));
                append_journal(record);
            }
            Event::Stopped(record) => {
                let reason = record.reason.map_or("", StopReason::label);
                log::warn!(
                    "Radio test OFF ({}): {reason}; rejected {} received and {} transmitted frames",
                    record.variant.label(),
                    record.rx_rejected.unwrap_or_default(),
                    record.tx_rejected.unwrap_or_default()
                );
                state.add_notification(format!(
                    "Radio test ended ({}): {reason}. The direct AV-bay link is accepted again.",
                    record.variant.label()
                ));
                append_journal(record);
            }
            Event::GoDirect => {
                if let Some(mesh) = crate::rrls::settings_transport() {
                    let mut mesh = mesh.lock().unwrap();
                    let now = mesh.now();
                    mesh.force_direct(now);
                }
            }
        }
    }
}

fn append_journal(record: &Record) {
    let Some(path) = JOURNAL.get() else { return };
    if let Err(error) = append_record(path, record) {
        log::error!("Radio test journal {}: {error}", path.display());
    }
}
pub(crate) fn append_record(path: &Path, record: &Record) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()
}
/// The journal's last records, newest first.
pub(crate) fn read_history(path: &Path) -> VecDeque<Record> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Record>(line).ok())
        .take(HISTORY)
        .collect()
}
/// After a restart: a start the journal never closed is closed now (the
/// test is never persisted, so the restart ended it).
pub(crate) fn close_dangling_start(path: &Path, utc_ms: u64) -> Option<Record> {
    let last = read_history(path).into_iter().next()?;
    if last.event != "start" {
        return None;
    }
    let record = Record {
        event: "stop".into(),
        utc_ms,
        reason: Some(StopReason::Restart),
        stopped_by: None,
        rx_rejected: None,
        tx_rejected: None,
        ..last
    };
    append_record(path, &record).ok()?;
    Some(record)
}

/// Primary: expiry, flight-state gate, Abort outcome and the radio filter,
/// every 100 ms. The RRLS has nothing to run (its banner comes from
/// STATION_STATE).
pub fn start(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    if crate::rrls::is_rrls() {
        return None;
    }
    let dir = Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let path = JOURNAL.get_or_init(|| dir.join(JOURNAL_FILE)).clone();
    if let Some(record) = close_dangling_start(&path, utc_ms()) {
        log::warn!(
            "Radio test ({}) started by {} was still on when the station stopped; it is off after the restart",
            record.variant.label(),
            record.actor
        );
    }
    with_controller(|controller| controller.history = read_history(&path));
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_millis(100));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = ticks.tick() => {}
            }
            service_tick(&state);
        }
    }))
}

fn service_tick(state: &AppState) {
    let relay_engaged = crate::rrls::settings_transport().is_some_and(|mesh| {
        let mesh = mesh.lock().unwrap();
        let now = mesh.now();
        mesh.active(now)
    });
    let inputs = Inputs {
        flight_state: state.local_flight_state_snapshot(),
        abort_confirmed: abort_confirmed(state),
        relay_engaged,
    };
    let events = with_controller(|controller| {
        let now = now_ms();
        controller.tick(inputs, now, utc_ms());
        sync_filter(controller, now);
        controller.take_events()
    });
    apply_events(state, events);
}

/// RRLS: the primary's announced test (from STATION_STATE), shown until it
/// ends or its timeout has surely passed.
struct PrimaryView {
    variant: Variant,
    minutes: u8,
    seen: Instant,
}
#[cfg(not(test))]
static PRIMARY_VIEW: Mutex<Option<PrimaryView>> = Mutex::new(None);
#[cfg(test)]
thread_local! {
    static PRIMARY_VIEW: Mutex<Option<PrimaryView>> = const { Mutex::new(None) };
}
#[cfg(not(test))]
fn with_primary_view<R>(f: impl FnOnce(&mut Option<PrimaryView>) -> R) -> R {
    f(&mut PRIMARY_VIEW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}
#[cfg(test)]
fn with_primary_view<R>(f: impl FnOnce(&mut Option<PrimaryView>) -> R) -> R {
    PRIMARY_VIEW.with(|view| {
        f(&mut view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    })
}

/// RRLS Mesh: the primary's radio-test bytes from a current STATION_STATE.
pub(crate) fn observe_primary(code: u8, minutes: u8) {
    with_primary_view(|view| {
        *view = Variant::from_code(code).map(|variant| PrimaryView {
            variant,
            minutes,
            seen: Instant::now(),
        });
    });
}
pub(crate) fn primary_view() -> Option<Value> {
    with_primary_view(|view| primary_view_json(view.as_ref()?))
}
fn primary_view_json(v: &PrimaryView) -> Option<Value> {
    let window = Duration::from_secs(u64::from(v.minutes) * 60 + 60);
    let elapsed = v.seen.elapsed();
    if elapsed > window {
        return None;
    }
    let left_s = u64::from(v.minutes) * 60;
    Some(json!({
        "variant": v.variant,
        "variant_label": v.variant.label(),
        "remaining_s_at_most": left_s.saturating_sub(elapsed.as_secs()),
        "banner": format!("{BANNER} by the primary ({})", v.variant.label()),
    }))
}

fn status_json(actor: Option<&Actor>, state: &AppState) -> Value {
    let rrls = crate::rrls::is_rrls();
    let radio = radio_available();
    let flight_state = state.local_flight_state_snapshot();
    let now = now_ms();
    let utc = utc_ms();
    let (mut value, blocked) = with_controller(|controller| {
        (
            controller.view(now, utc),
            controller.block_reason(flight_state, radio.clone()),
        )
    });
    value["role"] = json!(if rrls { "rrls" } else { "primary" });
    value["available"] = json!(radio.is_ok());
    value["unavailable_reason"] = json!(radio.err());
    value["start_blocked_reason"] = json!(blocked);
    value["may_start"] = json!(actor.is_some_and(|a| a.may_start));
    value["may_stop"] = json!(actor.is_some_and(|a| a.may_stop));
    value["default_duration_s"] = json!(default_duration_s());
    value["min_duration_s"] = json!(MIN_DURATION_S);
    value["max_duration_s"] = json!(MAX_DURATION_S);
    value["abort_escape_ms"] = json!(ABORT_ESCAPE_MS);
    value["silent_arm_ms"] = json!(SILENT_ARM_MS);
    value["allowed_states"] = json!(["Startup", "Idle", "PreFill"]);
    value["primary"] = if rrls {
        primary_view().unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    value
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error": "radio_test_refused", "message": message})),
    )
        .into_response()
}

async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(principal) => principal,
            Err(response) => return response,
        };
    Json(status_json(Some(&Actor::of(&principal)), &state)).into_response()
}

#[derive(Deserialize)]
pub(crate) struct StartRequest {
    variant: Variant,
    #[serde(default)]
    duration_s: Option<u64>,
    #[serde(default)]
    confirm: bool,
}

async fn post_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<StartRequest>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(principal) => principal,
            Err(response) => return response,
        };
    let actor = Actor::of(&principal);
    let flight_state = state.local_flight_state_snapshot();
    let radio = radio_available();
    let started = with_controller(|controller| {
        let now = now_ms();
        let result = controller.start(
            &actor,
            request.variant,
            request.duration_s,
            request.confirm,
            flight_state,
            radio,
            now,
            utc_ms(),
        );
        sync_filter(controller, now);
        result.map(|_| controller.take_events())
    });
    let events = match started {
        Ok(events) => events,
        Err(Refusal(status, message)) => return refuse(status, &message),
    };
    apply_events(&state, events);
    Json(status_json(Some(&actor), &state)).into_response()
}

async fn post_stop(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(principal) => principal,
            Err(response) => return response,
        };
    let actor = Actor::of(&principal);
    if !actor.may_stop {
        return refuse(
            StatusCode::FORBIDDEN,
            "Sign in with an account that can send commands to stop the radio test.",
        );
    }
    let events = with_controller(|controller| {
        controller.stop(StopReason::Operator, Some(&actor.name), utc_ms());
        sync_filter(controller, now_ms());
        controller.take_events()
    });
    apply_events(&state, events);
    Json(status_json(Some(&actor), &state)).into_response()
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(PATH, get(get_status))
        .route("/api/radio/reject-test/start", post(post_start))
        .route("/api/radio/reject-test/stop", post(post_stop))
}

#[cfg(test)]
#[path = "radio_reject_test_tests.rs"]
mod tests;
