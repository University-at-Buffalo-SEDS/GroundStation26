//! Re-send a retained operator Abort while an AV-bay board has not read it back.
//!
//! ABORT is an unordered reliable SDK packet with a 4.5 s delivery window
//! (250 ms x 18). If the radio cannot carry it for that long, the SDK retires
//! it silently. RF's own modem check, for one, holds the radio for up to 16 s
//! with the modem in command mode, and nothing on the air reaches RF then. FC
//! and PB used to latch such an Abort only when a later recovery query reached
//! them. Now, while the operator Abort is retained and a present AV-bay board
//! this mode tracks does not yet read Aborted, the station sends the ABORT
//! again whenever none is in flight (one at a time; a fresh packet ID, the
//! original wire and stamp). It stops when every such board reads Aborted,
//! when the station leaves Aborted (Clear or Reset all), when a recovery is
//! prepared or running, or on a newer operator Abort (which re-arms it).
//! Re-sending an Abort only ever moves a board toward the safe state.
use crate::manual_abort_intent::WirePolicy;
use sedsnet::router::Router;
use std::sync::Mutex;

/// RF, FC and PB in `board_abort::BOARDS` order.
const AV_BAY_MASK: u8 = 0b111;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Armed {
    epoch: u64,
    wire: WirePolicy,
    /// Recovery activity seen when armed; any other activity stops re-sending.
    recovery: Option<String>,
    last_sent_ms: Option<u64>,
    resends: u32,
}

#[derive(Default)]
pub(crate) struct AbortResend {
    armed: Mutex<Option<Armed>>,
}

/// What the caller observed this pass.
pub(crate) struct Observation {
    pub now_ms: u64,
    pub epoch: u64,
    pub station_aborted: bool,
    pub recovery: Option<String>,
    /// AV-bay boards (BOARDS mask) present, in scope and not reading Aborted.
    pub owed: u8,
}

impl AbortResend {
    /// The SDK accepted the operator Abort `epoch` with `wire`.
    pub(crate) fn arm(&self, epoch: u64, wire: WirePolicy, recovery: Option<String>) {
        *self.armed.lock().unwrap() = Some(Armed { epoch, wire, recovery, last_sent_ms: None, resends: 0 });
    }
    /// A recovery is being prepared: stop re-sending at once, before the
    /// coordinator drains in-flight ABORTs and takes its fresh queries, so no
    /// later copy can move a board's abort epoch under the prepared intent.
    pub(crate) fn disarm(&self, why: &str) {
        if let Some(armed) = self.armed.lock().unwrap().take()
            && armed.resends > 0
        {
            log::info!("operator Abort re-send stopped after {} re-send(s): {why}", armed.resends);
        }
    }
    /// Re-sends so far for the current operator Abort, while still active.
    pub(crate) fn active_resends(&self) -> Option<u32> {
        self.armed.lock().unwrap().as_ref().map(|armed| armed.resends)
    }
    /// One pass. Sends at most one ABORT, only when none is in flight and at
    /// least one retry interval after the last. Returns the boards still owed
    /// when it sent.
    pub(crate) fn step(&self, router: &Router, seen: Observation) -> Option<u8> {
        let mut guard = self.armed.lock().unwrap();
        let armed = guard.as_mut()?;
        let owed = seen.owed & AV_BAY_MASK;
        if seen.epoch != armed.epoch || !seen.station_aborted || seen.recovery != armed.recovery || owed == 0 {
            if armed.resends > 0 {
                log::info!(
                    "operator Abort re-send stopped after {} re-send(s): {}",
                    armed.resends,
                    if owed == 0 { "every AV-bay board reads Aborted" } else { "recovery started or the station left Aborted" }
                );
            }
            *guard = None;
            return None;
        }
        let abort = crate::telemetry_schema::data_type("ABORT");
        if router.export_reliable_pending_stats().iter().any(|row| row.data_type == abort) {
            return None;
        }
        let retry_ms = u64::from(sedsnet::config::runtime_tuning_config().reliable_retransmit_ms.max(1));
        if armed.last_sent_ms.is_some_and(|last| seen.now_ms.saturating_sub(last) < retry_ms) {
            return None;
        }
        // The first pass after arming only starts the clock: the original
        // ABORT may not have reached the SDK's pending table yet.
        if armed.last_sent_ms.is_none() {
            armed.last_sent_ms = Some(seen.now_ms);
            return None;
        }
        match crate::telemetry_task::queue_manual_abort_wire_receipted(router, armed.wire) {
            Ok(_) => {
                armed.resends = armed.resends.saturating_add(1);
                armed.last_sent_ms = Some(seen.now_ms);
                log::warn!(
                    "operator Abort re-sent ({}): no Aborted readback yet from {}",
                    armed.resends,
                    crate::board_abort::BOARDS
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| owed & (1 << i) != 0)
                        .map(|(_, board)| board.sender_id())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                Some(owed)
            }
            Err(error) => {
                log::warn!("operator Abort re-send refused: {error}");
                None
            }
        }
    }
}

/// Station-side observation for `step`.
pub(crate) fn observe(state: &crate::state::AppState, now_ms: u64) -> Option<Observation> {
    let recovery = crate::board_recovery::coordinator::recovery_activity(state)?;
    let aborted = state.board_abort.masks().0;
    let scope = crate::board_recovery::scope::mode_scope(crate::station_mode::local_mode());
    Some(Observation {
        now_ms,
        epoch: state.manual_abort_intent.epoch(),
        station_aborted: state.local_flight_state_snapshot() == crate::types::FlightState::Aborted,
        recovery,
        owed: scope & crate::board_abort::present_mask(state) & !aborted,
    })
}

pub(crate) async fn service(
    state: std::sync::Arc<crate::state::AppState>,
    router: std::sync::Arc<Router>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let start = std::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
        if crate::rrls::is_rrls() || state.abort_resend.active_resends().is_none() {
            continue;
        }
        let now_ms = crate::simulation_clock_ms().unwrap_or_else(|| start.elapsed().as_millis() as u64);
        if let Some(seen) = observe(&state, now_ms) {
            state.abort_resend.step(&router, seen);
        }
    }
}

#[cfg(test)]
#[path = "manual_abort_resend_tests.rs"]
mod tests;
