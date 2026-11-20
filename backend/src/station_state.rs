//! The primary's station-level state, broadcast to the RRLS (mesh kind
//! STATION_STATE) so an RRLS shows a change within about a second, listening
//! or relaying.
//!
//! Before this the RRLS learnt these from paths that are slow or never
//! refresh: the T clock through the signed SETTINGS clock sync (hint, query,
//! reply: several settings turns, seconds to tens of seconds), the station
//! mode through a signed query answered once per HEALTH session (an in-process
//! mode switch on the primary never reached it), the flight state only from
//! the primary's FLIGHT_STATE frames it happened to decode (and dropped until
//! it had seen every required board itself, which an RRLS that never hears the
//! fill boards never has), and the mission phase recomputed from its own board
//! presence and build mode instead of the primary's. The underglow and buzzer
//! rows came from the 10 s control observation (none while relaying) or the
//! canonical relayed frames.
//!
//! The primary sends one small frame on every change of what it mirrors, two
//! more copies within 750 ms, then one every PERIOD_MS. It carries the
//! primary's HEALTH session and a revision that grows by one per change, so
//! the RRLS drops copies from another session and older revisions (rrls.rs).
//!
//! Display only. Nothing here grants control, sends a command or an Abort, or
//! writes the RRLS's own flight state, network variables or clock journal.
//! During a takeover, or once the primary has not been heard for
//! VIEW_FRESH_MS, the RRLS shows its own state again.
use crate::{
    mission_phase::{MissionPhase, MissionPhaseMsg, PhaseInputs, PhaseMode},
    state::AppState,
    station_mode::Mode,
    telemetry_db::{LaunchClockKind, LaunchClockMsg},
    types::FlightState,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 2: the radio test bytes (30, 31).
pub(crate) const VERSION: u8 = 2;
/// Version, HEALTH session, revision.
const HEADER_LEN: usize = 9;
pub(crate) const BODY_LEN: usize = 32;
pub(crate) const PAYLOAD_LEN: usize = HEADER_LEN + BODY_LEN;
/// After a change: the frame now and two more copies, all within a second,
/// so one lost copy costs at most 250-500 ms.
const COPY_OFFSETS_MS: [u64; 3] = [0, 250, 750];
/// Unchanged state is re-sent this often (a missed change, a new RRLS).
pub(crate) const PERIOD_MS: u64 = 5_000;
/// A queued copy older than this is superseded by the next one.
pub(crate) const TTL_MS: u64 = 1_000;
/// The RRLS shows the primary's view for this long after the last copy (four
/// periods): longer than the relay's HEALTH gaps, shorter than an outage.
const VIEW_FRESH_MS: u64 = 4 * PERIOD_MS;
const NONE_I64: i64 = i64::MIN;
const FLAG_REQUIRED_BOARDS_SEEN: u8 = 1;
const FLAG_FILL_ACTIVITY: u8 = 2;
const FLAG_FC_CONTACT_LOST: u8 = 4;
const NETVAR_UNKNOWN: u8 = 0xff;

/// What the primary mirrors to the RRLS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StationState {
    pub(crate) flight_state: FlightState,
    pub(crate) phase: MissionPhase,
    pub(crate) required_boards_seen: bool,
    pub(crate) fill_activity: bool,
    pub(crate) fc_contact_lost: bool,
    pub(crate) mode: Mode,
    pub(crate) clock: LaunchClockMsg,
    /// Low 32 bits of the primary's ClockRecord revision.
    pub(crate) clock_revision: u32,
    /// First 32 bits of the primary's ClockRecord authority.
    pub(crate) clock_authority: u32,
    /// The primary's underglow/buzzer view (primary_mirror netvar byte).
    pub(crate) netvar: Option<u8>,
    /// The primary's radio test (radio_reject_test): variant code (0 off)
    /// and whole minutes left. A minute boundary is a change, so this costs
    /// three extra copies a minute while a test runs.
    pub(crate) radio_test: (u8, u8),
}

fn mode_code(mode: Mode) -> u8 {
    match mode {
        Mode::Normal => 0,
        Mode::Hitl => 1,
        Mode::TestFire => 2,
        Mode::Testing => 3,
        Mode::IntegratedGroundTest => 4,
    }
}
fn mode_from(code: u8) -> Option<Mode> {
    Some(match code {
        0 => Mode::Normal,
        1 => Mode::Hitl,
        2 => Mode::TestFire,
        3 => Mode::Testing,
        4 => Mode::IntegratedGroundTest,
        _ => return None,
    })
}
const PHASES: [MissionPhase; 6] = [
    MissionPhase::Startup,
    MissionPhase::Idle,
    MissionPhase::Fill,
    MissionPhase::Launch,
    MissionPhase::InFlight,
    MissionPhase::Landed,
];
fn phase_mode(mode: Mode) -> PhaseMode {
    match mode {
        Mode::Hitl => PhaseMode::Hitl,
        Mode::TestFire => PhaseMode::TestFire,
        Mode::Normal | Mode::Testing | Mode::IntegratedGroundTest => PhaseMode::Flight,
    }
}
fn clock_kind_code(kind: LaunchClockKind) -> u8 {
    match kind {
        LaunchClockKind::Idle => 0,
        LaunchClockKind::TMinus => 1,
        LaunchClockKind::TPlus => 2,
    }
}
/// The first 32 bits of a 32-hex-digit clock authority.
pub(crate) fn authority_tag(authority: &str) -> u32 {
    authority
        .get(..8)
        .and_then(|prefix| u32::from_str_radix(prefix, 16).ok())
        .unwrap_or(0)
}

impl StationState {
    pub(crate) fn encode(&self) -> [u8; BODY_LEN] {
        let mut out = [0; BODY_LEN];
        out[0] = self.flight_state as u8;
        out[1] = PHASES.iter().position(|p| *p == self.phase).unwrap_or(0) as u8;
        out[2] = u8::from(self.required_boards_seen) * FLAG_REQUIRED_BOARDS_SEEN
            | u8::from(self.fill_activity) * FLAG_FILL_ACTIVITY
            | u8::from(self.fc_contact_lost) * FLAG_FC_CONTACT_LOST;
        out[3] = mode_code(self.mode);
        out[4] = clock_kind_code(self.clock.kind);
        out[5..13].copy_from_slice(
            &self
                .clock
                .anchor_timestamp_ms
                .unwrap_or(NONE_I64)
                .to_le_bytes(),
        );
        out[13..21].copy_from_slice(&self.clock.duration_ms.unwrap_or(NONE_I64).to_le_bytes());
        out[21..25].copy_from_slice(&self.clock_revision.to_le_bytes());
        out[25..29].copy_from_slice(&self.clock_authority.to_le_bytes());
        out[29] = self.netvar.unwrap_or(NETVAR_UNKNOWN);
        out[30] = self.radio_test.0;
        out[31] = self.radio_test.1;
        out
    }
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; BODY_LEN] = bytes.try_into().ok()?;
        let i64_at = |at: usize| {
            Some(i64::from_le_bytes(bytes[at..at + 8].try_into().ok()?)).filter(|v| *v != NONE_I64)
        };
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        if bytes[2] & !(FLAG_REQUIRED_BOARDS_SEEN | FLAG_FILL_ACTIVITY | FLAG_FC_CONTACT_LOST) != 0
            || (bytes[30] != 0 && crate::radio_reject_test::Variant::from_code(bytes[30]).is_none())
        {
            return None;
        }
        Some(Self {
            flight_state: crate::types::u8_to_flight_state(bytes[0])?,
            phase: *PHASES.get(usize::from(bytes[1]))?,
            required_boards_seen: bytes[2] & FLAG_REQUIRED_BOARDS_SEEN != 0,
            fill_activity: bytes[2] & FLAG_FILL_ACTIVITY != 0,
            fc_contact_lost: bytes[2] & FLAG_FC_CONTACT_LOST != 0,
            mode: mode_from(bytes[3])?,
            clock: LaunchClockMsg {
                kind: match bytes[4] {
                    0 => LaunchClockKind::Idle,
                    1 => LaunchClockKind::TMinus,
                    2 => LaunchClockKind::TPlus,
                    _ => return None,
                },
                anchor_timestamp_ms: i64_at(5),
                duration_ms: i64_at(13),
            },
            clock_revision: u32_at(21),
            clock_authority: u32_at(25),
            netvar: Some(bytes[29]).filter(|b| *b != NETVAR_UNKNOWN),
            radio_test: (bytes[30], bytes[31]),
        })
    }
    /// Primary: its current station state.
    pub(crate) fn capture(state: &AppState) -> Self {
        // The published phase (the service refreshes it on every flight-state
        // publication, and the worker wakes on its change).
        let phase = crate::mission_phase::subscribe().borrow().clone();
        let record = state.launch_history.lock().unwrap().snapshot();
        Self::from_parts(
            state.local_flight_state_snapshot(),
            &phase,
            crate::station_mode::local_mode(),
            &record,
            Some(crate::primary_mirror::primary_netvar_byte()),
            crate::radio_reject_test::wire_state(),
        )
    }
    pub(crate) fn from_parts(
        flight_state: FlightState,
        phase: &MissionPhaseMsg,
        mode: Mode,
        record: &crate::launch_history::ClockRecord,
        netvar: Option<u8>,
        radio_test: (u8, u8),
    ) -> Self {
        Self {
            flight_state,
            phase: phase.phase,
            required_boards_seen: phase.required_boards_seen,
            fill_activity: phase.fill_activity,
            fc_contact_lost: phase.comms_loss_landed,
            mode,
            clock: record.clock.clone(),
            clock_revision: record.revision as u32,
            clock_authority: authority_tag(&record.authority),
            netvar,
            radio_test,
        }
    }
    /// The primary's mission phase message, rebuilt from what it sent.
    fn mission_phase(&self, since_ms: i64) -> MissionPhaseMsg {
        let inputs = PhaseInputs {
            mode: phase_mode(self.mode),
            detailed: self.flight_state,
            required_boards_seen: self.required_boards_seen,
            fill_activity: self.fill_activity,
            previous: Some(self.phase),
            fc_contact_lost: self.fc_contact_lost,
        };
        let mut msg = MissionPhaseMsg::from_inputs(&inputs, since_ms);
        if msg.phase != self.phase {
            msg.phase = self.phase;
            msg.label = self.phase.label().to_string();
        }
        msg
    }
}

/// Primary side: what to send and when (held by its Mesh).
#[derive(Default)]
pub(crate) struct Broadcast {
    body: Option<[u8; BODY_LEN]>,
    revision: u32,
    changed_at: u64,
    /// Copies of the current change already queued.
    copies: usize,
    last_queued: Option<u64>,
}
impl Broadcast {
    /// A new body starts a new revision and its copies; an unchanged one
    /// changes nothing.
    pub(crate) fn update(&mut self, body: [u8; BODY_LEN], now: u64) {
        if self.body == Some(body) {
            return;
        }
        self.body = Some(body);
        self.revision = self.revision.wrapping_add(1);
        self.changed_at = now;
        self.copies = 0;
    }
    /// The payload to queue now, if one is due.
    pub(crate) fn due(&mut self, session: u32, now: u64) -> Option<Vec<u8>> {
        let body = self.body?;
        let due = match COPY_OFFSETS_MS.get(self.copies) {
            Some(offset) => self.changed_at.saturating_add(*offset),
            None => self
                .last_queued
                .map_or(now, |last| last.saturating_add(PERIOD_MS)),
        };
        if now < due {
            return None;
        }
        self.copies = self.copies.saturating_add(1);
        self.last_queued = Some(now);
        let mut payload = Vec::with_capacity(PAYLOAD_LEN);
        payload.push(VERSION);
        payload.extend_from_slice(&session.to_le_bytes());
        payload.extend_from_slice(&self.revision.to_le_bytes());
        payload.extend_from_slice(&body);
        Some(payload)
    }
}

/// RRLS side: (session, revision, body) of a received payload; None when it is
/// malformed. Ordering against earlier copies is the Mesh's (rrls.rs).
pub(crate) fn parse(payload: &[u8]) -> Option<(u32, u32, &[u8])> {
    if payload.len() != PAYLOAD_LEN || payload[0] != VERSION {
        return None;
    }
    let session = u32::from_le_bytes(payload[1..5].try_into().ok()?);
    let revision = u32::from_le_bytes(payload[5..9].try_into().ok()?);
    let body = &payload[HEADER_LEN..];
    StationState::decode(body)?;
    Some((session, revision, body))
}

struct View {
    state: StationState,
    /// Last copy, for freshness.
    seen: Instant,
}
static VIEW: Mutex<Option<View>> = Mutex::new(None);
static CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn view() -> std::sync::MutexGuard<'static, Option<View>> {
    VIEW.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RRLS Mesh, takeover or not: the primary's radio test from a current copy.
pub(crate) fn observe_radio_test(body: &[u8]) {
    if let Some(state) = StationState::decode(body) {
        crate::radio_reject_test::observe_primary(state.radio_test.0, state.radio_test.1);
    }
}

/// RRLS Mesh, outside a takeover: an accepted copy (current session, not
/// older than the last). A copy of the same state only renews freshness.
pub(crate) fn receive(body: &[u8]) {
    let Some(state) = StationState::decode(body) else {
        return;
    };
    let mut view = view();
    if view.as_ref().is_none_or(|v| v.state != state) {
        CHANGED.store(true, std::sync::atomic::Ordering::Release);
    }
    if let Some(netvar) = state.netvar {
        crate::primary_mirror::relayed_outputs::observe_primary_view(netvar);
    }
    *view = Some(View {
        state,
        seen: Instant::now(),
    });
}

/// The primary's station state as this RRLS shows it: None on the primary,
/// during a takeover, and once no copy has arrived for VIEW_FRESH_MS.
pub(crate) fn presented() -> Option<StationState> {
    if !crate::rrls::is_rrls() || crate::rrls::takeover_active() {
        return None;
    }
    view()
        .as_ref()
        .filter(|v| v.seen.elapsed() <= Duration::from_millis(VIEW_FRESH_MS))
        .map(|v| v.state.clone())
}

/// The flight state to present for `local` (the RRLS's projection): the
/// primary's while its broadcast is current.
pub(crate) fn presented_flight_state(local: FlightState) -> FlightState {
    presented().map_or(local, |v| v.flight_state)
}

/// RRLS: the primary's mission phase, while its broadcast is current.
pub(crate) fn presented_mission_phase(since_ms: i64) -> Option<MissionPhaseMsg> {
    presented().map(|v| v.mission_phase(since_ms))
}

/// RRLS: the primary's station mode, while its broadcast is current.
pub(crate) fn presented_mode() -> Option<Mode> {
    presented().map(|v| v.mode)
}

/// Whether the broadcast clock is newer than `record` (same primary clock
/// authority, later revision), so a slower signed clock reply must not
/// replace it on screen.
pub(crate) fn clock_newer_than(record: &crate::launch_history::ClockRecord) -> bool {
    presented().is_some_and(|v| broadcast_clock_newer(&v, record))
}
fn broadcast_clock_newer(v: &StationState, record: &crate::launch_history::ClockRecord) -> bool {
    v.clock_authority != authority_tag(&record.authority)
        || (v.clock_revision.wrapping_sub(record.revision as u32) as i32) > 0
}

/// The clock this RRLS shows: the broadcast's when newer than its journal.
fn clock_for(v: &StationState, record: &crate::launch_history::ClockRecord) -> LaunchClockMsg {
    if broadcast_clock_newer(v, record) {
        v.clock.clone()
    } else {
        record.clock.clone()
    }
}

/// RRLS: present a changed broadcast (or its end) on the same paths the RRLS
/// already shows these on, at once. `was` is what was presented before; the
/// return value is what is presented now.
pub(crate) fn apply(state: &AppState, was: Option<StationState>) -> Option<StationState> {
    let now = presented();
    let changed = CHANGED.swap(false, std::sync::atomic::Ordering::AcqRel);
    if !changed && now == was {
        return now;
    }
    // Flight state: the websocket's FlightState (FlightStateWire maps the
    // RRLS's projection to the primary's), never the RRLS's own state.
    let flight = presented_flight_state(state.local_flight_state_snapshot());
    if was.as_ref().map(|v| v.flight_state) != now.as_ref().map(|v| v.flight_state) {
        let _ = state
            .state_tx
            .send(crate::web::FlightStateMsg { state: flight });
    }
    crate::mission_phase::refresh(state);
    // T clock: the RRLS's clock is a display of the primary's; its journal
    // (launch_history) stays on the signed sync.
    let record = state.launch_history.lock().unwrap().snapshot();
    let clock = match now.as_ref() {
        Some(v) => clock_for(v, &record),
        None => record.clock.clone(),
    };
    {
        let mut current = state.launch_clock.lock().unwrap();
        if *current != clock {
            *current = clock.clone();
            drop(current);
            let _ = state.launch_clock_tx.send(clock);
        }
    }
    if let Some(v) = now.as_ref() {
        crate::station_mode::observe_primary_mode(v.mode);
    }
    state.broadcast_action_policy_snapshot();
    now
}

/// Primary: capture and publish on every change. RRLS: present what arrives.
pub fn start(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    let mesh = crate::rrls::settings_transport()?;
    let rrls = crate::rrls::is_rrls();
    let mut shutdown = state.shutdown_subscribe();
    let mut flight = state.state_tx.subscribe();
    let mut clock = state.launch_clock_tx.subscribe();
    let mut phase = crate::mission_phase::subscribe();
    Some(tokio::spawn(async move {
        // The primary also wakes on each flight-state, clock and phase
        // publication; the tick catches the rest (mode, underglow/buzzer).
        let mut ticks = tokio::time::interval(Duration::from_millis(if rrls { 50 } else { 100 }));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut presented = None;
        let mut wake = !rrls;
        loop {
            use tokio::sync::broadcast::error::RecvError;
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = ticks.tick() => {}
                result = flight.recv(), if wake => wake = !matches!(result, Err(RecvError::Closed)),
                result = clock.recv(), if wake => wake = !matches!(result, Err(RecvError::Closed)),
                result = phase.changed(), if wake => wake = result.is_ok(),
            }
            if rrls {
                presented = apply(&state, presented);
            } else {
                let body = StationState::capture(&state).encode();
                let mut link = mesh.lock().unwrap();
                let now = link.now();
                link.publish_station_state(body, now);
            }
        }
    }))
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *view() = None;
    CHANGED.store(false, std::sync::atomic::Ordering::Release);
}

#[cfg(test)]
pub(crate) fn received_for_test() -> Option<StationState> {
    view().as_ref().map(|v| v.state.clone())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod igt_mode_code_tests {
    use super::*;
    #[test]
    fn every_mode_round_trips_and_igt_is_wire_code_4() {
        assert_eq!(mode_code(Mode::IntegratedGroundTest), 4);
        for mode in [Mode::Normal, Mode::Hitl, Mode::TestFire, Mode::Testing, Mode::IntegratedGroundTest] {
            assert_eq!(mode_from(mode_code(mode)), Some(mode));
        }
        assert_eq!(mode_from(5), None);
        assert_eq!(phase_mode(Mode::IntegratedGroundTest), PhaseMode::Flight);
    }
}
