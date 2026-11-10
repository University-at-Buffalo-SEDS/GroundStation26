//! Re-send a retained operator Abort while an AV-bay board has not read it back.
//!
//! ABORT is an unordered reliable SDK packet with a 4.5 s delivery window
//! (250 ms x 18). If the radio cannot carry it for that long, the SDK retires
//! it silently. RF's own modem check, for one, holds the radio for up to 16 s
//! with the modem in command mode, and nothing on the air reaches RF then. FC
//! and PB used to latch such an Abort only when a later recovery query reached
//! them. Now, while the operator Abort is retained and a present AV-bay board
//! this mode tracks does not yet read Aborted, the station sends the ABORT
//! again when the last one ran out of retries before that board acknowledged
//! it (one at a time; a fresh packet ID, the original wire and stamp). An
//! ABORT the AV bay acknowledged is never re-sent because its readback is
//! late: every copy re-latches the umbilical boards and costs radio air. It stops when every such board reads Aborted,
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
    /// Resumed after a bounced recovery: take the next observed recovery
    /// identity as the baseline instead of treating it as a new recovery.
    adopt_recovery: bool,
    /// The ABORT in flight as last seen: (AV-bay boards it still owed, its
    /// retries). Only an ABORT that ran out of retries with an owed AV-bay
    /// board still pending is sent again.
    in_flight: Option<(u8, u32)>,
}

#[derive(Default)]
pub(crate) struct AbortResend {
    armed: Mutex<Option<Armed>>,
    /// Held while a recovery is being prepared; restored if it resets nothing.
    suspended: Mutex<Option<Armed>>,
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
        *self.armed.lock().unwrap() = Some(Armed { epoch, wire, recovery, last_sent_ms: None, resends: 0, adopt_recovery: false, in_flight: None });
    }
    /// A recovery is being prepared: stop re-sending at once, before the
    /// coordinator drains in-flight ABORTs and takes its fresh queries, so no
    /// later copy can move a board's abort epoch under the prepared intent.
    pub(crate) fn disarm(&self, why: &str) {
        let taken = self.armed.lock().unwrap().take();
        if let Some(armed) = taken {
            if armed.resends > 0 {
                log::info!("operator Abort re-send paused after {} re-send(s): {why}", armed.resends);
            }
            *self.suspended.lock().unwrap() = Some(armed);
        }
    }
    /// The recovery that paused the re-send reset no board: resume it. A
    /// newer operator Abort armed meanwhile is kept instead. The next pass
    /// stops it again if the station has left Aborted or the boards read back.
    pub(crate) fn resume(&self) {
        let Some(mut armed) = self.suspended.lock().unwrap().take() else { return };
        let mut current = self.armed.lock().unwrap();
        if current.is_none() {
            // The recovery identity seen then is gone; adopt the current one.
            armed.recovery = None;
            armed.adopt_recovery = true;
            log::info!("operator Abort re-send resumed: the recovery reset no board");
            *current = Some(armed);
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
        if armed.adopt_recovery {
            armed.recovery = seen.recovery.clone();
            armed.adopt_recovery = false;
        }
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
        let rows = router.export_reliable_pending_stats();
        let pending = rows
            .iter()
            .filter(|row| row.data_type == abort)
            .map(|row| {
                let boards = row.pending_destinations.iter().fold(0u8, |mask, owner| {
                    crate::board_recovery::coordinator::pending_destination_board(router, *owner)
                        .and_then(|board| crate::board_abort::BOARDS.iter().position(|b| *b == board))
                        .map_or(mask, |i| mask | (1 << i))
                });
                (boards & AV_BAY_MASK, row.retries)
            })
            .max_by_key(|(_, retries)| *retries);
        if let Some(row) = pending {
            armed.in_flight = Some(row);
            return None;
        }
        // No ABORT in flight. Send again only if the last one ran out of
        // retries while an AV-bay board that still owes the Abort had not
        // acknowledged it. One the AV bay acknowledged is never re-sent:
        // a late readback is not a lost Abort.
        let max_retries = sedsnet::config::runtime_tuning_config().reliable_max_retries;
        let exhausted = armed
            .in_flight
            .take()
            .is_some_and(|(boards, retries)| boards & owed != 0 && retries.saturating_add(3) >= max_retries);
        if !exhausted {
            return None;
        }
        let retry_ms = u64::from(sedsnet::config::runtime_tuning_config().reliable_retransmit_ms.max(1));
        if armed.last_sent_ms.is_some_and(|last| seen.now_ms.saturating_sub(last) < retry_ms) {
            return None;
        }
        match crate::telemetry_task::queue_manual_abort_wire_receipted(router, armed.wire) {
            Ok(_) => {
                armed.resends = armed.resends.saturating_add(1);
                armed.last_sent_ms = Some(seen.now_ms);
                log::warn!(
                    "operator Abort re-sent ({}): the last ABORT ran out of retries before {} acknowledged it",
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
