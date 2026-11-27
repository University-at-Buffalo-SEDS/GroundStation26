//! Undo of a Launch attempt none of whose packets the SDK took.
//!
//! A Launch press records its attempt on disk (Fire stage, launch-history
//! inhibition and launch lock, `engine.claimed`, the checklist update) before
//! it offers any packet to the SDK. When the SDK then takes none of them (the
//! core stayed busy past `LAUNCH_CORE_BUSY_WAIT`, or a check refused before
//! the first admission), no board received anything, and the records alone
//! would keep Launch locked until a manual recovery. This undoes exactly what
//! that attempt recorded, so the operator can press Launch again, and keeps
//! the attempt in the launch journal as refused and undone.
//!
//! It never runs once any packet was taken (those keep their own handling),
//! and never after a newer Stop, Abort or reset, or while an Abort is latched:
//! the Abort wins and the attempt stays recorded as today. It changes no
//! Abort latch and no flight state, and it holds neither the dispatch gate nor
//! the GSE lock across its disk writes. A manual Abort never waits for it.
use super::operational::{FireRecord, Operational, Saved};
use super::{LaunchAdmission, LaunchQueueError, Phase, Runtime, with_stop_admission};
use crate::state::AppState;
use crate::types::FlightState;
use std::sync::Arc;

/// What one Launch press recorded before offering any packet.
pub(crate) struct LaunchAttempt {
    pub(crate) ticket: LaunchAdmission,
    /// The launch journal's inhibition as this attempt set it.
    journal_mark: u64,
    /// The Fire stage it committed (not in HITL, which records none).
    fire: Option<FireUndo>,
}
struct FireUndo {
    record: FireRecord,
    prior_claimed: bool,
}
impl LaunchAttempt {
    pub(super) fn inhibited(ticket: LaunchAdmission, journal_mark: u64) -> Self {
        Self { ticket, journal_mark, fire: None }
    }
    pub(super) fn fired(ticket: LaunchAdmission, journal_mark: u64, record: FireRecord, prior_claimed: bool) -> Self {
        Self { ticket, journal_mark, fire: Some(FireUndo { record, prior_claimed }) }
    }
}

/// What the operator sees when a refused Launch was undone.
pub(crate) const UNDONE_MESSAGE: &str = "Launch refused, nothing was sent; Launch is available again";

/// Undo `attempt` after its Launch ended with no packet taken. `refusal` is
/// that outcome; `packets` how many the Launch had. Ok: Launch is available
/// again and the journal records the attempt as refused and undone. Err: why
/// it was not (fully) undone; Launch then stays locked as before.
pub(crate) fn undo_unsent_launch(
    state: &Arc<AppState>,
    attempt: LaunchAttempt,
    refusal: &LaunchQueueError,
    packets: usize,
) -> Result<(), String> {
    if refusal.admitted != 0 {
        // A board may have it: never undone (partial or late-failure handling).
        return Err(format!("{}/{packets} of its packets were taken", refusal.admitted));
    }
    still_current(state, attempt.ticket)?;
    if let Some(fire) = &attempt.fire {
        undo_fire(state, fire)?;
    }
    // Checked again just before Launch becomes available: an Abort or Stop
    // that came while the records above were written keeps it locked.
    still_current(state, attempt.ticket)?;
    let record = crate::launch_history::UndoneLaunch {
        utc_ms: crate::telemetry_task::get_current_timestamp_ms(),
        mode: crate::station_mode::local_mode(),
        packets,
        reason: refusal.message.clone(),
    };
    let pending = state
        .launch_history
        .lock()
        .unwrap()
        .undo_unsent_launch(attempt.journal_mark, record)
        .map_err(|error| format!("the launch journal was not undone: {error:#}"))?;
    // The journal write is waited for with no lock held.
    if let Some((writer, ticket)) = pending {
        writer
            .wait(ticket)
            .map_err(|error| format!("the launch journal undo could not be saved: {error:#}"))?;
    }
    Ok(())
}

/// No newer Stop, Abort or reset than the Launch, no Abort latched, and the
/// station neither Aborted nor in Launch.
fn still_current(state: &AppState, ticket: LaunchAdmission) -> Result<(), String> {
    with_stop_admission(state, ticket, || ()).map_err(str::to_string)?;
    if state.abort_indicator_latched() {
        return Err("an Abort is latched".into());
    }
    match state.local_flight_state_snapshot() {
        FlightState::Aborted => Err("the station is Aborted".into()),
        FlightState::Launch => Err("the station is in Launch".into()),
        _ => Ok(()),
    }
}

/// Give back the qualification from before Fire, the nitrogen test's
/// checklist progress and `engine.claimed`.
fn undo_fire(state: &Arc<AppState>, fire: &FireUndo) -> Result<(), String> {
    let restore = Operational::fire_restore_event(&fire.record);
    write_outside_lock(
        state,
        |operational| operational.fire_undone(&fire.record, &restore),
        |rt| {
            // A fault the engine latched meanwhile keeps its claim.
            if !fire.prior_claimed && rt.engine.status.phase != Phase::Fault {
                rt.engine.claimed = false;
            }
        },
    )
    .map_err(|error| format!("the Fire stage was not undone: {error}"))?;
    let queued = state
        .gse
        .lock()
        .unwrap()
        .operational
        .saved
        .pending_checklists
        .iter()
        .any(|event| event.id == restore.id);
    if !queued {
        // The Clear was never delivered and was dropped: nothing to give back.
        return Ok(());
    }
    // Delivered with no lock held; the checklist store ignores a repeat.
    let delivered = if crate::checklists::paired::requested() {
        crate::checklists::paired::deliver_event(&restore)
    } else {
        crate::checklists::sequence_completed(&restore).map(|()| true)
    }
    .map_err(|error| format!("the checklist progress was not given back: {error}"))?;
    if !delivered {
        return Err("the checklist progress was not given back yet".into());
    }
    write_outside_lock(
        state,
        |operational| {
            let saved = &operational.saved;
            Ok(saved.pending_checklists.iter().any(|event| event.id == restore.id).then(|| {
                let mut next = saved.clone();
                next.pending_checklists.retain(|event| event.id != restore.id);
                next
            }))
        },
        |_| {},
    )
    .map_err(|error| format!("the delivered checklist progress was not saved: {error}"))
}

/// Install a qualification record that `build` derives from the current one,
/// writing its file with no GSE lock held. The record is installed only if
/// no other write of it landed meanwhile and `build` still gives the same
/// record; otherwise it is built and written again. `build` returning None
/// means there is nothing to do.
fn write_outside_lock(
    state: &AppState,
    build: impl Fn(&Operational) -> Result<Option<Saved>, String>,
    installed: impl FnOnce(&mut Runtime),
) -> Result<(), String> {
    const ATTEMPTS: usize = 8;
    let mut written = false;
    let result = (|| {
        for _ in 0..ATTEMPTS {
            let (next, path, writes) = {
                let rt = state.gse.lock().unwrap();
                let Some(next) = build(&rt.operational)? else {
                    return Ok(());
                };
                let path = rt.operational.path.clone().ok_or("Qualification storage is not initialized")?;
                (next, path, rt.operational.writes)
            };
            written = true;
            next.save_aside(&path)?;
            let bytes = serde_json::to_vec(&next).map_err(|error| error.to_string())?;
            let mut rt = state.gse.lock().unwrap();
            let unchanged = rt.operational.writes == writes
                && build(&rt.operational)?
                    .is_some_and(|again| serde_json::to_vec(&again).is_ok_and(|again| again == bytes));
            if unchanged {
                rt.operational.saved = next;
                rt.operational.writes += 1;
                installed(&mut rt);
                written = false;
                return Ok(());
            }
        }
        Err("the qualification record kept changing while it was written".to_string())
    })();
    if result.is_err() && written {
        // The file may now hold a record that was not installed: put back
        // the one in force (only on this failure path, under the GSE lock as
        // every ordinary qualification write is).
        let mut rt = state.gse.lock().unwrap();
        if let Err(error) = rt.operational.persist() {
            log::error!("Qualification record could not be restored after a failed Launch undo: {error}");
        }
    }
    result
}

#[cfg(test)]
#[path = "launch_undo_tests.rs"]
mod tests;
