//! Retained actual-board Abort observations. This is a new-launch interlock,
//! never an emergency command producer or authority to clear the manual latch.
use crate::{
    state::AppState,
    types::{Board, FlightState},
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Mutex,
    atomic::{AtomicU8, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
mod discovery_refresh;

pub(crate) const BOARDS: [Board; 7] = [
    Board::RFBoard,
    Board::FlightComputer,
    Board::PowerBoard,
    Board::ActuatorBoard,
    Board::ValveBoard,
    Board::GatewayBoard,
    Board::DaqBoard,
];
const PROOF_TTL: Duration = Duration::from_secs(5);
pub(crate) fn board_names(mask: u8) -> String {
    BOARDS
        .into_iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, b)| b.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
pub(crate) fn capture_launch_revision(
    state: &AppState,
    command: &crate::types::TelemetryCommand,
) -> Option<u64> {
    if crate::operator_phase::is_local(command) {return None;}
    capture_launch_revision_named(state, crate::sequences::command_name(command))
}
pub(crate) fn capture_launch_revision_named(state: &AppState, name: &str) -> Option<u64> {
    if state.shutdown_requested.load(Ordering::Acquire) || blocked_message(state).is_some() { return None; }
    if !launch_command(name) {
        return None;
    }
    synchronize_stop_revision(state);
    let register = state.board_abort.register.lock().unwrap();
    let (active, _) = Runtime::masks_at(&register, Instant::now());
    (active == 0 && register.abort_revision != u64::MAX).then_some(register.abort_revision)
}
pub(crate) fn validate_launch_revision(
    state: &AppState,
    revision: Option<u64>,
) -> Result<u64, String> {
    synchronize_stop_revision(state);
    if let Some(message) = blocked_message(state) {
        return Err(message);
    }
    let Some(revision) = revision.filter(|r| *r != u64::MAX) else {
        return Err("Launch origin is missing or was captured while a board reported Abort. Check readiness and make a new explicit request.".into());
    };
    if revision != state.board_abort.abort_revision() {
        return Err("A board Abort superseded this queued launch intent. Check readiness and make a new explicit request.".into());
    }
    Ok(revision)
}
/// Stop retention remains independent: synchronization happens only on
/// launch/readiness consumers. Sample intent locks before taking the board
/// gate, and release both before any board lock or SDK call.
pub(crate) fn synchronize_stop_revision(state: &AppState) {
    let epochs = (
        state.manual_abort_intent.epoch(),
        state.deferred_abort.manual_epoch(),
    );
    let mut register = state.board_abort.register.lock().unwrap();
    if register.history_frozen || register.stop_epochs == epochs {
        return;
    }
    register.stop_epochs = epochs;
    register.abort_revision = if epochs.0 == u64::MAX || epochs.1 == u64::MAX {
        u64::MAX
    } else {
        register.abort_revision.saturating_add(1)
    };
    state
        .board_abort
        .abort_counter
        .store(register.abort_revision, Ordering::Release);
    register.version = register.version.saturating_add(1);
    drop(register);
    state.board_abort.notify_changed();
}
pub(crate) fn controller_can_launch(kind: sedsnet::config::DataType, payload: &[u8]) -> bool {
    if kind != crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
        return false;
    }
    if payload == [crate::rocket_commands::FlightComputerCommands::Launch as u8] {
        return true;
    }
    #[cfg(feature = "hitl_mode")]
    if payload == [crate::rocket_commands::FlightComputerCommands::AdvanceFlightState as u8]
        || payload == [crate::rocket_commands::FlightComputerCommands::RewindFlightState as u8]
    {
        return true;
    }
    false
}
pub(crate) fn blocked_message(state: &AppState) -> Option<String> {
    let register = state.board_abort.register.lock().unwrap();
    if state.shutdown_requested.load(Ordering::Acquire) || register.history_frozen {
        return Some("Ground station is shutting down".into());
    }
    if let Some(error) = state.board_abort.history_failure() { return Some(format!("Abort history unavailable: {error}")); }
    let mask = Runtime::masks_at(&register, Instant::now()).0;
    if mask != 0 { return Some(format!("Launch disabled: {} report Abort.", board_names(mask))); }
    let unknown = required_unknown_mask(state, register.history_unknown);
    (unknown != 0).then(|| format!("Fresh actual state required because saved history is missing or was not clean for {}.", board_names(unknown)))
}

fn index(board: Board) -> Option<usize> {
    BOARDS.iter().position(|b| *b == board)
}
#[derive(Clone, Copy, Default)]
struct Record {
    aborted: bool,
    abort_revision: u64,
    last_abort: Option<Instant>,
    last_local_abort: Option<Instant>,
    first_abort_ms: u64,
    alert_episode: Option<AlertEpisode>,
    accepted_non_abort: bool,
    proof_after: Option<Instant>,
    proof_boot: u64,
    proof_epoch: u32,
    proof_nonce: u64,
}
/// Process-local diagnostic chronology only. These tokens never cross the wire
/// or restore current proof from disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AlertEpisode {
    pub(crate) ordinal: u64,
    pub(crate) first_abort_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AlertEdgeKind { Raised, AcceptedExit }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AlertEdge {
    pub(crate) ordinal: u64,
    pub(crate) board: crate::alert_conditions::BoardKey,
    pub(crate) episode: Option<AlertEpisode>,
    pub(crate) kind: AlertEdgeKind,
    pub(crate) at_ms: u64,
}
const ALERT_EDGE_CAPACITY: usize = 64;
struct AlertEdges {
    next: u64,
    entries: [Option<AlertEdge>; ALERT_EDGE_CAPACITY],
    len: usize,
    incomplete: bool,
}
impl Default for AlertEdges {
    fn default() -> Self { Self { next: 0, entries: [None; ALERT_EDGE_CAPACITY], len: 0, incomplete: false } }
}
impl AlertEdges {
    fn push(&mut self, board: Board, kind: AlertEdgeKind, episode: Option<AlertEpisode>, at_ms: u64, original_first_ms: Option<u64>) -> Option<AlertEpisode> {
        let Some(next) = self.next.checked_add(1).filter(|value| *value != u64::MAX) else { self.incomplete = true; return None; };
        self.next = next;
        let episode = if kind == AlertEdgeKind::Raised { Some(AlertEpisode { ordinal: next, first_abort_ms: original_first_ms.unwrap_or(at_ms) }) } else { episode };
        if let Some(key) = crate::alert_conditions::BoardKey::from_sender(board.sender_id()) {
            if self.len < ALERT_EDGE_CAPACITY {
                self.entries[self.len] = Some(AlertEdge { ordinal: next, board: key, episode, kind, at_ms });
                self.len += 1;
            } else { self.incomplete = true; }
        } else { self.incomplete = true; }
        episode
    }
}
#[derive(Default)]
struct Register {
    version: u64,
    abort_revision: u64,
    stop_epochs: (u64, u64),
    reset_policy_revision: u64,
    rows: [Record; 7],
    history_unknown: u8,
    history_enrolled: u8,
    history_frozen: bool,
    alert_edges: AlertEdges,
}
/// Process-local scoped-reset selection context, never restored board proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResetContext {
    policy_revision: u64,
    episodes: [Option<AlertEpisode>; 7],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutomaticAdmissionError { Busy, Changed, ActiveAbort, Unavailable }

#[derive(Default)]
pub(crate) struct Runtime {
    register: Mutex<Register>,
    abort_counter: AtomicU64,
    demand: AtomicU8,
    pub(crate) changed: std::sync::Arc<Notify>,
    pub(crate) query_needed: Notify,
    history: Option<crate::board_abort_history_writer::Publisher>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ActiveError {
    pub source: String,
    pub board: String,
    pub board_label: String,
    pub timestamp_ms: u64,
    pub message: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub version: u64,
    pub active_errors: Vec<ActiveError>,
    pub required_boards: Vec<String>,
    pub missing_required_state: Vec<String>,
    #[serde(default)]
    pub retraction: Option<crate::actuator_plumbing::RetractionGate>,
    #[serde(default)]
    pub history_unknown_boards: Vec<String>,
    #[serde(default)]
    pub history_error: Option<String>,
}
impl Runtime {
    pub(crate) fn restored(
        value: crate::board_abort_history::Restoration,
        publisher: crate::board_abort_history_writer::Publisher,
        changed: std::sync::Arc<Notify>,
    ) -> Self {
        let at = Instant::now();
        let rows = std::array::from_fn(|i| Record {
            aborted: value.snapshot.actual_abort & (1 << i) != 0,
            first_abort_ms: value.snapshot.first_abort_ms[i],
            last_abort: Some(at), last_local_abort: Some(at), abort_revision: 1,
            ..Default::default()
        });
        let mut register = Register {
            version: value.snapshot.revision.saturating_add(1), abort_revision: 1,
            history_unknown: value.snapshot.unknown, history_enrolled: value.snapshot.enrolled,
            rows, ..Default::default()
        };
        for (i, board) in BOARDS.into_iter().enumerate() {
            if register.rows[i].aborted {
                let first = register.rows[i].first_abort_ms;
                register.rows[i].alert_episode = register.alert_edges.push(board, AlertEdgeKind::Raised, None, crate::telemetry_task::get_current_timestamp_ms(), Some(first));
            }
        }
        Self {
            register: Mutex::new(register),
            abort_counter: AtomicU64::new(1), changed,
            history: Some(publisher), ..Default::default()
        }
    }
    fn history_snapshot_locked(register: &Register) -> crate::board_abort_history::Snapshot {
        crate::board_abort_history::Snapshot {
            revision: register.version,
            actual_abort: register.rows.iter().enumerate().fold(0, |mask,(i,row)| mask | if row.aborted {1 << i} else {0}),
            unknown: register.history_unknown, enrolled: register.history_enrolled,
            first_abort_ms: std::array::from_fn(|i| register.rows[i].first_abort_ms),
        }
    }
    pub(crate) fn history_snapshot(&self) -> crate::board_abort_history::Snapshot {
        Self::history_snapshot_locked(&self.register.lock().unwrap())
    }
    pub(crate) fn freeze_history_after_quiescence(&self) -> crate::board_abort_history::Snapshot {
        let mut register = self.register.lock().unwrap();
        register.history_frozen = true;
        Self::history_snapshot_locked(&register)
    }
    pub(crate) fn publish_initial_history(&self) { self.notify_changed(); }
    fn notify_changed(&self) {
        if let Some(history) = &self.history { history.publish(self.history_snapshot()); }
        self.changed.notify_one();
    }
    fn history_failure(&self) -> Option<String> { self.history.as_ref().and_then(|h| h.failure()) }
    /// Shared launch-only context boundary. The callback is RAM-only and may
    /// not call SDK, filesystem, GSE or asynchronous work.
    pub(crate) fn change_launch_context_if(&self, change: impl FnOnce() -> bool) -> bool {
        let mut register = self.register.lock().unwrap();
        if register.history_frozen { return false; }
        if !change() {
            return false;
        }
        register.reset_policy_revision = register.reset_policy_revision.saturating_add(1);
        register.abort_revision = register.abort_revision.saturating_add(1);
        self.abort_counter
            .store(register.abort_revision, Ordering::Release);
        register.version = register.version.saturating_add(1);
        drop(register);
        self.notify_changed();
        true
    }
    pub(crate) fn capture_reset_context(
        &self, policy: impl FnOnce() -> u8,
    ) -> Result<(ResetContext, u8, u8), &'static str> {
        let register = self.register.lock().unwrap();
        if register.history_frozen || register.reset_policy_revision == u64::MAX {
            return Err("Scoped reset policy is closed or exhausted");
        }
        if register.rows.iter().any(|row| row.aborted && row.alert_episode.is_none()) {
            return Err("Scoped reset cannot identify an active Abort episode");
        }
        let context = ResetContext {
            policy_revision: register.reset_policy_revision,
            episodes: std::array::from_fn(|i| register.rows[i].alert_episode),
        };
        let actual = register.rows.iter().enumerate().fold(0, |mask,(i,row)|
            mask | if row.aborted {1 << i} else {0});
        Ok((context, policy(), actual))
    }
    /// Final RAM-only publication fence. No lock acquisition, SDK, filesystem, await or GSE work
    /// may be introduced by this callback. Final lock order: DGate→preacquired RAM owners→Stop→board.try.
    pub(crate) fn with_reset_context<T>(
        &self, expected: ResetContext, apply: impl FnOnce() -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let register = self.register.try_lock().map_err(|_| "Reset policy owner busy")?;
        if register.history_frozen || register.reset_policy_revision == u64::MAX
            || expected.policy_revision != register.reset_policy_revision
            || expected.episodes != std::array::from_fn(|i| register.rows[i].alert_episode)
            || register.rows.iter().any(|row| row.aborted && row.alert_episode.is_none()) {
            return Err("Scoped reset policy or Abort episode changed; prepare a new intent");
        }
        apply()
    }
    pub(crate) fn abort_revision(&self) -> u64 {
        self.abort_counter.load(Ordering::Acquire)
    }
    /// Same authoritative register as Launch admission and actual ingress.
    /// This is NOT the Launch readiness/retraction policy: only known Abort,
    /// original revision and shutdown/frozen lifetime gate automatic metadata.
    /// Unknown history/freshness still belongs to the separate Launch interlock.
    pub(crate) fn automatic_revision_current(&self, revision: u64) -> bool {
        self.try_admit_automatic_prelaunch(revision, || ()).is_ok()
    }
    /// Final lock order: Dispatch -> preacquired RAM owners -> original Stop
    /// scopes -> this register.try -> SDK.try. Never wait on the register.
    /// The caller prebuilds all work; callback is SDK admission + infallible
    /// scalar assignments only, no provider/FS/user callback/retirement.
    pub(crate) fn try_admit_automatic_prelaunch<T>(
        &self, revision: u64, admit: impl FnOnce() -> T,
    ) -> Result<T, AutomaticAdmissionError> {
        let register = self.register.try_lock().map_err(|_| AutomaticAdmissionError::Busy)?;
        if register.history_frozen || register.abort_revision == u64::MAX {
            return Err(AutomaticAdmissionError::Unavailable);
        }
        if register.abort_revision != revision {
            return Err(AutomaticAdmissionError::Changed);
        }
        if register.rows.iter().any(|row| row.aborted) {
            return Err(AutomaticAdmissionError::ActiveAbort);
        }
        let result = admit();
        // Explicitly retain the exact ingress/revision owner through BOTH SDK
        // transfer and host/model scalar commit. No unlocked-counter shortcut.
        drop(register);
        Ok(result)
    }

    pub(crate) fn query_revision(&self, board: Board) -> u64 {
        index(board).map_or(u64::MAX, |i| {
            self.register.lock().unwrap().rows[i].abort_revision
        })
    }
    pub(crate) fn observe_abort(&self, board: Board, received: Instant, timestamp_ms: u64) {
        let Some(i) = index(board) else { return };
        let mut register = self.register.lock().unwrap();
        if register.history_frozen { return; }
        register.history_enrolled |= 1 << i;
        register.abort_revision = register.abort_revision.saturating_add(1);
        self.abort_counter
            .store(register.abort_revision, Ordering::Release);
        register.version = register.version.saturating_add(1);
        let row = &mut register.rows[i];
        row.abort_revision = row.abort_revision.saturating_add(1);
        row.last_abort = Some(received);
        row.last_local_abort = Some(received);
        row.proof_after = None;
        let raised = !row.aborted;
        if raised { row.first_abort_ms = timestamp_ms; }
        row.aborted = true;
        row.accepted_non_abort = false;
        if raised {
            let episode = register.alert_edges.push(board, AlertEdgeKind::Raised, None, timestamp_ms, None);
            register.rows[i].alert_episode = episode;
        }
        drop(register);
        self.notify_changed();
    }
    /// The caller has already validated the actual request nonce, issuer,
    /// sender, clock-domain deadline and current reply. Duplicates never call it.
    #[expect(clippy::too_many_arguments, reason = "Independent boot, epoch, nonce, state, receipt and query revision evidence remains explicit")]
    pub(crate) fn observe_status(
        &self,
        board: Board,
        boot: u64,
        epoch: u32,
        nonce: u64,
        state: Option<u8>,
        abort_latched: bool,
        strong_non_abort: bool,
        query_after: Instant,
        query_abort_revision: u64,
        received: Instant,
        timestamp_ms: u64,
    ) {
        if abort_latched || state == Some(FlightState::Aborted as u8) {
            self.observe_abort(board, received, timestamp_ms);
            return;
        }
        let Some(i) = index(board) else { return };
        let mut register = self.register.lock().unwrap();
        if register.history_frozen { return; }
        register.history_enrolled |= 1 << i;
        let row = &mut register.rows[i];
        row.proof_after = None;
        if strong_non_abort
            && !matches!(boot, 0 | u64::MAX)
            && !matches!(epoch, 0 | u32::MAX)
            && query_abort_revision == row.abort_revision
            && row.abort_revision != u64::MAX
            && row.last_abort.is_none_or(|at| query_after > at)
        {
            let publish_exit = !row.accepted_non_abort;
            let episode = row.alert_episode;
            row.accepted_non_abort = true;
            row.aborted = false;
            row.proof_after = Some(query_after);
            row.proof_boot = boot;
            row.proof_epoch = epoch;
            row.proof_nonce = nonce;
            if publish_exit { register.alert_edges.push(board, AlertEdgeKind::AcceptedExit, episode, timestamp_ms, None); }
            register.history_unknown &= !(1 << i);
        }
        register.version = register.version.saturating_add(1);
        drop(register);
        self.notify_changed();
    }
    pub(crate) fn mirror_proof(&self) -> (u8, u8, [u16; 7], u64) {
        let register = self.register.lock().unwrap();
        let (active, proven) = Self::masks_at(&register, Instant::now());
        let ages = std::array::from_fn(|i| {
            register.rows[i]
                .proof_after
                .and_then(|at| u16::try_from(at.elapsed().as_millis()).ok())
                .filter(|age| *age != u16::MAX)
                .unwrap_or(u16::MAX)
        });
        (active, proven, ages, register.abort_revision)
    }
    /// Only the authenticated primary-mirror consumer calls this. Empty/stale
    /// masks never clear; local observations require a conservatively later
    /// actual query, not simply a newly received mirror frame.
    pub(crate) fn observe_authenticated_mirror(
        &self,
        active: u8,
        proven: u8,
        ages: [u16; 7],
        transit_ms: u64,
    ) {
        let now = Instant::now();
        let mut register = self.register.lock().unwrap();
        if register.history_frozen { return; }
        let mut changed = false;
        for (i, age) in ages.iter().enumerate() {
            let mut clear_unknown = false;
            let mut raised = None;
            let mut exited = None;
            if active & (1 << i) != 0 { register.history_enrolled |= 1 << i; }
            let row = &mut register.rows[i];
            if active & (1 << i) != 0 {
                if !row.aborted {
                    row.aborted = true;
                    row.last_abort = Some(now);
                    row.abort_revision = row.abort_revision.saturating_add(1);
                    row.first_abort_ms = crate::telemetry_task::get_current_timestamp_ms();
                    raised = Some(row.first_abort_ms);
                    row.proof_after = None;
                    row.accepted_non_abort = false;
                    changed = true;
                }
            } else if proven & (1 << i) != 0 && *age != u16::MAX {
                let age = u64::from(ages[i]).saturating_add(transit_ms);
                if age <= PROOF_TTL.as_millis() as u64
                    && let Some(after) = now.checked_sub(Duration::from_millis(age))
                        && row.last_local_abort.is_none_or(|abort| after > abort) {
                            changed |= row.aborted;
                            if !row.accepted_non_abort { exited = Some(row.alert_episode); }
                            row.accepted_non_abort = true;
                            row.aborted = false;
                            row.proof_after = Some(after);
                            clear_unknown = true;
                        }
            }
            if let Some(first) = raised {
                let episode = register.alert_edges.push(BOARDS[i], AlertEdgeKind::Raised, None, first, None);
                register.rows[i].alert_episode = episode;
            }
            if let Some(episode) = exited { register.alert_edges.push(BOARDS[i], AlertEdgeKind::AcceptedExit, episode, crate::telemetry_task::get_current_timestamp_ms(), None); }
            if clear_unknown { changed |= register.history_unknown & (1 << i) != 0; register.history_unknown &= !(1 << i); }
        }
        if changed {
            register.abort_revision = register.abort_revision.saturating_add(1);
            self.abort_counter
                .store(register.abort_revision, Ordering::Release);
            register.version = register.version.saturating_add(1);
        }
        drop(register);
        self.notify_changed();
    }
    pub(crate) fn request_queries(&self, mask: u8) {
        if mask != 0 {
            self.demand.fetch_or(mask & 0x7f, Ordering::AcqRel);
            self.query_needed.notify_one();
        }
    }
    pub(crate) fn take_queries(&self) -> u8 {
        self.demand.swap(0, Ordering::AcqRel)
    }
    fn masks_at(register: &Register, now: Instant) -> (u8, u8) {
        let mut aborted = 0;
        let mut proven = 0;
        for (i, row) in register.rows.iter().enumerate() {
            if row.aborted {
                aborted |= 1 << i;
            } else if row.proof_after.is_some_and(|after| {
                now.checked_duration_since(after)
                    .is_some_and(|age| age <= PROOF_TTL)
            }) {
                proven |= 1 << i;
            }
        }
        (aborted, proven)
    }
    pub(crate) fn masks(&self) -> (u8, u8) {
        Self::masks_at(&self.register.lock().unwrap(), Instant::now())
    }
    /// Shared linearization with actual ingress. Only prebuilt SDK queue
    /// admission or the pure model effect is permitted here, never a flush,
    /// callback, journal, GSE/state lookup or await. Public437 RX callbacks run
    /// after its core guard is dropped; held-core/reentrant tests protect that.
    /// Mandatory pair proof is independent of optional HITL participation.
    pub(crate) fn ignition_pair_current(&self) -> bool {
        let Ok(register) = self.register.try_lock() else { return false; };
        let (aborted, proven) = Self::masks_at(&register, Instant::now());
        !register.history_frozen && register.abort_revision != u64::MAX
            && aborted == 0 && proven & crate::ignition_pair::REQUIRED_MASK == crate::ignition_pair::REQUIRED_MASK
    }
    /// Direct sequence admission adds no Launch/retraction/history mutation.
    /// Caller owns the original Stop/reset context; callback is SDK.try only.
    pub(crate) fn admit_ignition_pair<T>(
        &self, revision: u64, admit: impl FnOnce() -> T,
    ) -> Result<T, &'static str> {
        let register = self.register.try_lock().map_err(|_| "Ignition board proof owner busy")?;
        if register.history_frozen || register.abort_revision == u64::MAX
            || register.abort_revision != revision {
            return Err("Ignition context changed; no new sequence was admitted");
        }
        let (aborted, proven) = Self::masks_at(&register, Instant::now());
        if aborted != 0 || proven & crate::ignition_pair::REQUIRED_MASK != crate::ignition_pair::REQUIRED_MASK {
            return Err("Ignition requires fresh non-Abort Actuator and Valve Board readbacks");
        }
        let result = admit();
        drop(register);
        Ok(result)
    }
    pub(crate) fn admit_launch<T>(
        &self,
        state: &AppState,
        required: u8,
        revision: u64,
        admit: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let register = self.register.lock().unwrap();
        if state.shutdown_requested.load(Ordering::Acquire) || register.history_frozen { return Err("Ground station is shutting down".into()); }
        if let Some(error) = self.history_failure() { return Err(format!("Abort history unavailable: {error}")); }
        let unknown = required_unknown_mask(state, register.history_unknown);
        if unknown != 0 { self.request_queries(unknown); return Err(format!("Fresh actual state required because saved history is missing or was not clean for {}.", board_names(unknown))); }
        let (aborted, proven) = Self::masks_at(&register, Instant::now());
        if aborted != 0 {
            drop(register);
            return Err(format!(
                "Launch disabled: {} report Abort.",
                board_names(aborted)
            ));
        }
        if register.abort_revision == u64::MAX || register.abort_revision != revision {
            return Err("A board Abort superseded this launch intent; prepare a new launch".into());
        }
        if required & !proven != 0 {
            self.request_queries(required & !proven);
            return Err(format!(
                "Fresh actual state required for {}. Query then prepare a new launch.",
                board_names(required & !proven)
            ));
        }
        let motion = crate::actuator_plumbing::retraction_gate(state);
        if !motion.allowed {
            return Err(motion.message);
        }
        Ok(admit())
    }
    pub(crate) fn snapshot(&self, required: u8) -> Snapshot {
        let register = self.register.lock().unwrap();
        let version = register.version;
        let rows = register.rows;
        let history_unknown = register.history_unknown;
        let (_, proven) = Self::masks_at(&register, Instant::now());
        drop(register);
        let mut out = Snapshot {
            version,
            history_error: self.history_failure(),
            ..Default::default()
        };
        for (i, board) in BOARDS.into_iter().enumerate() {
            if history_unknown & (1 << i) != 0 { out.history_unknown_boards.push(board.sender_id().into()); }
            if required & (1 << i) != 0 {
                out.required_boards.push(board.sender_id().into());
                if proven & (1 << i) == 0 {
                    out.missing_required_state.push(board.sender_id().into());
                }
            }
            if rows[i].aborted {
                out.active_errors.push(ActiveError{source:format!("board_abort:{}",board.sender_id()),
                board:board.sender_id().into(),board_label:board.as_str().into(),timestamp_ms:rows[i].first_abort_ms,
                message:format!("{} reports Abort. New launch is disabled until a fresh actual non-Abort state is confirmed.",board.as_str())});
            }
        }
        out
    }
    pub(crate) fn take_alert_edges(&self) -> ([Option<AlertEdge>; ALERT_EDGE_CAPACITY], bool) {
        let mut register = self.register.lock().unwrap();
        let entries = std::mem::replace(&mut register.alert_edges.entries, [None; ALERT_EDGE_CAPACITY]);
        register.alert_edges.len = 0;
        (entries, register.alert_edges.incomplete)
    }

}
pub(crate) fn required_unknown_mask(state: &AppState, unknown: u8) -> u8 {
    if crate::rrls::is_rrls() {
        return unknown & crate::primary_mirror::board_history_required_scope(state).unwrap_or(0x7f);
    }
    crate::history_policy::uncertain_launch_blockers(unknown,
        if cfg!(feature="test_fire_mode") {crate::history_policy::LaunchScope::TestFire}
        else {crate::history_policy::LaunchScope::Flight},
        cfg!(feature="hitl_mode"), state.board_state_proof_enforced())
}
/// Inspect only after actual control/receiver/observer owners have joined and
/// outside application locks. Unknown ownership never earns a Clean receipt.
pub(crate) fn shutdown_stop_quiesced(state: &AppState) -> bool {
    if state.manual_abort_intent.epoch()==u64::MAX || state.deferred_abort.manual_epoch()==u64::MAX
        || state.manual_abort_intent.pending_epoch().is_some() || state.deferred_abort.manual_pending() {
        return false;
    }
    state.topology_router.get().is_some_and(|router| !router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT")))
}
pub(crate) fn required_mask(state: &AppState) -> u8 {
    if crate::rrls::is_rrls() {
        return crate::primary_mirror::board_required_state_mask(state).unwrap_or(0x7f);
    }
    if !state.board_state_proof_enforced() { return 0; }
    BOARDS.into_iter().enumerate().fold(0, |mask, (i, b)| {
        mask | if state.board_required_for_progression(b) {
            1 << i
        } else {
            0
        }
    })
}
pub(crate) fn observe_raw(state: &AppState, packet: &sedsnet::packet::Packet) {
    if packet.data_type() != crate::telemetry_schema::data_type("FLIGHT_STATE")
        || packet.payload().len() != 1
    {
        return;
    }
    let Some(board) = state
        .board_from_network_sender(packet.sender().as_ref())
        .filter(|b| index(*b).is_some())
    else {
        return;
    };
    let Some(value) = crate::types::u8_to_flight_state(packet.payload()[0]) else {
        return;
    };
    #[cfg(all(feature="testing",not(any(feature="hitl_mode",feature="test_fire_mode"))))]
    if crate::testing_board_lifecycle::retired_model_abort_state(board,packet) {return;}
    if value == FlightState::Aborted {
        state.board_abort.observe_abort(
            board,
            Instant::now(),
            crate::telemetry_task::get_current_timestamp_ms(),
        );
    } else {
        let i = index(board).unwrap();
        if state.board_abort.masks().0 & (1 << i) != 0 {
            state.board_abort.request_queries(1 << i);
        }
    }
}
pub(crate) fn launch_command(name: &str) -> bool {
    matches!(
        name,
        "Launch" | "GroundStationLaunch" | "AdvanceFlightState" | "RewindFlightState"
    )
}
pub(crate) fn block_policy(state: &AppState, policy: &mut crate::sequences::ActionPolicyMsg) {
    if blocked_message(state).is_some()
        || !crate::rrls::is_rrls() && !crate::actuator_plumbing::retraction_gate(state).allowed
    {
        for control in &mut policy.controls {
            if launch_command(&control.cmd) {
                control.enabled = false;
                control.blink = crate::sequences::BlinkMode::None;
            }
        }
    }
}

pub(crate) fn snapshot_for_state(state: &AppState) -> Snapshot {
    let required = required_mask(state) | required_unknown_mask(state, state.board_abort.history_snapshot().unknown);
    let mut value = state.board_abort.snapshot(required);
    if !crate::rrls::is_rrls() {
        value.retraction = Some(crate::actuator_plumbing::retraction_gate(state));
    } else {
        value.retraction = crate::primary_mirror::retraction_presentation(state);
    }
    value
}
pub(crate) async fn publish_service(
    state: std::sync::Arc<AppState>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {biased;_=shutdown.recv()=>return,_=state.board_abort.changed.notified()=>{}}
        let snapshot = snapshot_for_state(&state);
        let _ = state.board_abort_tx.send(snapshot);
        state.broadcast_action_policy_snapshot();
        let authority = state.alert_conditions.authority();
        let (edges, incomplete) = state.board_abort.take_alert_edges();
        crate::alert_condition_runtime::observe_board_abort_edges(&state, &authority, &edges, incomplete);

    }
}
/// Dedicated bounded drive/state observations keep control readiness independent
/// of an open Settings page. Every frame still traverses the existing nonce
/// correlation and original-source freshness checks. This emits QUERY only.
#[derive(Default)]
pub(crate) struct ActuatorReadbackRefresh {
    last: Option<Instant>,
}
impl ActuatorReadbackRefresh {
    pub(crate) fn poll(&mut self, state: &std::sync::Arc<AppState>) -> bool {
        let now = Instant::now();
        if self.last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
            || crate::rrls::is_rrls()
            || !crate::board_recovery::coordinator::readonly_query_idle(state)
            || state.topology_router.get().is_none_or(|router| router.sender().as_ref() != "GS")
        {
            return false;
        }
        let healthy = state.network_health.lock().unwrap()
            .snapshot(now, cfg!(feature = "test_fire_mode"))
            .iter().any(|row| row.sender_id == "AB" && row.healthy);
        if !healthy { return false; }
        self.last = Some(now);
        // The board query verifies the dedicated discovered endpoint and keeps
        // any outstanding nonce alive. Do not probe legacy command endpoints.
        if crate::board_recovery::request_board_query(state, Board::ActuatorBoard).status()
            != axum::http::StatusCode::OK
        {
            return false;
        }
        let _ = crate::actuator_plumbing::request_drive_query(state);
        let _ = crate::actuator_plumbing::request_motion_query(state);
        true
    }
}

/// Read-only health refresh plus finite explicit-demand batches. All requests
/// share a two-second background cooldown; actuator control readbacks have a
/// separate one-second budget. Recovery still needs human intent.
pub(crate) async fn query_service(
    state: std::sync::Arc<AppState>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut refresh = crate::board_recovery::ReadonlyQueryRefresh::default();
    let mut actuator = ActuatorReadbackRefresh::default();
    let mut discovery = discovery_refresh::DiscoveryRefresh::default();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {biased;_=shutdown.recv()=>return,_=state.board_abort.query_needed.notified()=>{},_=tick.tick()=>{}}
        let demand = state.board_abort.take_queries();
        if crate::rrls::is_rrls() || !crate::board_recovery::coordinator::readonly_query_idle(&state) {
            continue;
        }
        let _ = discovery.poll(&state);
        let _ = actuator.poll(&state);
        for (i, board) in BOARDS.into_iter().enumerate() {
            if demand & (1 << i) == 0 {
                continue;
            }
            let _ = refresh.request(&state, board);
            tokio::select! {biased;_=shutdown.recv()=>return,_=tokio::time::sleep(Duration::from_millis(100))=>{}}
        }
        let _ = refresh.poll(&state);
    }
}
pub(crate) async fn get_status(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData).await
    {
        return response;
    }
    synchronize_stop_revision(&state);
    axum::Json(snapshot_for_state(&state)).into_response()
}

/// Explicit authenticated RRLS readiness demand, never periodic Observe.
/// Primary proof scope is independent of whether its own history was clean.
pub(crate) fn request_paired_readiness_queries(state: &AppState) -> Result<(), String> {
    if crate::rrls::is_rrls() || !crate::board_recovery::coordinator::manual_utc_idle(state) {
        return Err("Readiness scan requires idle primary recovery admission".into());
    }
    let scope=if !state.board_state_proof_enforced() {0}
        else if cfg!(feature="test_fire_mode") {0x78} else {0x7f};
    state.board_abort.request_queries(scope | state.board_abort.masks().0);
    crate::actuator_plumbing::request_motion_query(state)
}
pub(crate) fn request_readiness_queries(state: &AppState) -> Result<(), String> {
    let mut mask = required_mask(state) | crate::ignition_pair::REQUIRED_MASK
        | state.board_abort.masks().0 | required_unknown_mask(state, state.board_abort.history_snapshot().unknown);
    if crate::actuator_plumbing::retraction_gate(state).configured {
        mask |= 1 << index(Board::ActuatorBoard).unwrap();
    }
    state.board_abort.request_queries(mask);
    crate::actuator_plumbing::request_motion_query(state)
}
pub(crate) async fn refresh_status(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData).await
    {
        return response;
    }
    synchronize_stop_revision(&state);
    if crate::rrls::is_rrls() {
        if let Err(error) = crate::command_proxy::request_readiness_check().await {
            return (axum::http::StatusCode::SERVICE_UNAVAILABLE, error).into_response();
        }
    } else {
        if let Err(error) = request_readiness_queries(&state) {
            return (axum::http::StatusCode::SERVICE_UNAVAILABLE, error).into_response();
        }
    }
    (axum::http::StatusCode::ACCEPTED,axum::Json(serde_json::json!({"message":"Readiness check requested. Inspect fresh board states, then make a separate explicit Launch request. No Launch was sent.","snapshot":snapshot_for_state(&state)}))).into_response()
}

#[cfg(test)]
mod reset_scope_context_tests {
    use super::*;
    #[test]
    fn ignition_pair_lease_survives_healthy_two_second_refresh_but_expires_at_five() {
        let runtime = Runtime::default();
        let start = Instant::now();
        for board in [Board::ActuatorBoard, Board::ValveBoard] {
            runtime.observe_status(board, 7, 1, 22, Some(1), false, true,
                start, runtime.query_revision(board), start, 100);
        }
        {
            let register = runtime.register.lock().unwrap();
            assert_eq!(Runtime::masks_at(&register, start + Duration::from_secs(3)).1
                & crate::ignition_pair::REQUIRED_MASK, crate::ignition_pair::REQUIRED_MASK);
            assert_eq!(Runtime::masks_at(&register, start + Duration::from_secs(5)).1
                & crate::ignition_pair::REQUIRED_MASK, crate::ignition_pair::REQUIRED_MASK);
            assert_eq!(Runtime::masks_at(&register, start + Duration::from_millis(5_001)).1
                & crate::ignition_pair::REQUIRED_MASK, 0);
        }
        runtime.observe_abort(Board::ActuatorBoard, start + Duration::from_millis(1), 101);
        assert!(!runtime.ignition_pair_current());
    }

    #[test]
    fn same_abort_episode_and_accepted_exit_preserve_original_selection_but_new_episode_does_not() {
        let runtime = Runtime::default();
        let (before, _, _) = runtime.capture_reset_context(|| 0x78).unwrap();
        let at = Instant::now();
        runtime.observe_abort(Board::RFBoard, at, 100);
        assert!(runtime.with_reset_context(before, || Ok(())).is_err());
        let (episode, required, actual) = runtime.capture_reset_context(|| 0x78).unwrap();
        assert_eq!((required, actual), (0x78, 1));
        runtime.observe_abort(Board::RFBoard, at + Duration::from_millis(1), 100);
        assert!(runtime.with_reset_context(episode, || Ok(())).is_ok());
        let query_at = at + Duration::from_millis(2);
        runtime.observe_status(Board::RFBoard, 12, 2, 34, Some(0), false, true,
            query_at, runtime.query_revision(Board::RFBoard), query_at, 101);
        assert!(runtime.with_reset_context(episode, || Ok(())).is_ok());
        // Same UTC is a distinct episode after actual accepted exit (ABA).
        runtime.observe_abort(Board::RFBoard, at + Duration::from_millis(3), 100);
        assert!(runtime.with_reset_context(episode, || Ok(())).is_err());
    }
    #[test]
    fn policy_aba_exhaustion_and_unidentified_abort_cannot_authorize_scoped_reset() {
        let runtime = Runtime::default();
        let (context, _, _) = runtime.capture_reset_context(|| 0).unwrap();
        assert!(runtime.change_launch_context_if(|| true));
        assert!(runtime.change_launch_context_if(|| true));
        assert!(runtime.with_reset_context(context, || Ok(())).is_err());
        runtime.register.lock().unwrap().reset_policy_revision = u64::MAX;
        assert!(runtime.capture_reset_context(|| panic!("exhausted policy callback ran")).is_err());
        let runtime = Runtime::default();
        runtime.register.lock().unwrap().rows[0].aborted = true;
        assert!(runtime.capture_reset_context(|| 0).is_err());
    }
    #[test]
    fn unknown_history_is_preserved_and_never_becomes_actual_abort_selection() {
        let runtime = Runtime::default();
        runtime.register.lock().unwrap().history_unknown = 0x7f;
        let (_, required, actual) = runtime.capture_reset_context(|| 0x78).unwrap();
        assert_eq!((required, actual), (0x78, 0));
        assert_eq!(runtime.history_snapshot().unknown, 0x7f);
    }
}

#[cfg(test)]
mod automatic_admission_tests;
