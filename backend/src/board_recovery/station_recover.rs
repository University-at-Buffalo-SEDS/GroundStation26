//! Recover station: clear the station's own operator-Abort latch and return
//! the station to Startup (Idle in HITL, as Reset all boards finishes there)
//! once every board has been recovered on its own, without resetting any
//! board. A convenience: Reset all boards also clears the latch, but resets
//! every board to do it.
//!
//! Nothing here sends a board command, a recovery request or an Abort. The
//! station publishes its own FLIGHT_STATE (the managed network variable every
//! station state change writes), which is also what keeps the recovered state
//! across a restart, and journals the action in the launch journal.
use super::*;

pub(crate) const PATH: &str = "/api/board-recovery/recover-station";

/// The station's own Abort latch is set: the operator Abort indicator, or the
/// station's state is Aborted (a restart restores only the state).
pub(crate) fn latched(state: &AppState) -> bool {
    state.abort_indicator_latched()
        || state.local_flight_state_snapshot() == crate::types::FlightState::Aborted
}

/// Where Recover station takes the station from Aborted: Startup, as Reset all
/// boards does, and in HITL straight on to Idle (no automatic Startup to Idle
/// runs there).
pub(crate) fn target_state() -> crate::types::FlightState {
    if crate::runtime_mode::is_hitl() {
        crate::types::FlightState::Idle
    } else {
        crate::types::FlightState::Startup
    }
}

fn names(boards: &[&str]) -> (String, bool) {
    (boards.join(", "), boards.len() == 1)
}

/// Why a present board of this mode keeps the station latched, from the board
/// Abort register and each board's latest recovery readback. A board absent
/// from the network, or one this mode does not use, does not hold it.
fn board_block_reason(state: &AppState) -> Option<String> {
    let scope = expected_boards();
    let aborted = state.board_abort.launch_abort_mask(state) & scope;
    if aborted != 0 {
        let (list, one) = names(&board_names(aborted));
        return Some(format!(
            "{list} still {} Abort. Recover {} first (Board recovery), or use Reset all boards.",
            if one { "reports" } else { "report" },
            if one { "it" } else { "them" }
        ));
    }
    let latched = latched_launch_boards(state);
    if !latched.is_empty() {
        let (list, one) = names(&latched);
        return Some(format!(
            "{list} still {} Launch and keep{} the launch latch; use Reset all boards.",
            if one { "reports" } else { "report" },
            if one { "s" } else { "" }
        ));
    }
    let present = crate::board_abort::present_mask(state) & scope;
    let now = Stamp::now();
    let inner = state.board_recovery.inner.lock().unwrap();
    for (at, board) in BOARDS.iter().enumerate() {
        if present & (1 << at) == 0 {
            continue;
        }
        // A board with no recovery readback is held only by its Abort row above.
        let Some(observation) = inner.values[at].as_ref() else { continue };
        let row = &observation.value;
        if row.flags & 1 != 0 || row.state == Some(crate::types::FlightState::Aborted as u8) {
            return Some(format!(
                "{} last read back Abort. Recover it first (Board recovery), or use Reset all boards.",
                board.sender_id()
            ));
        }
        if !observation.fresh(now) {
            return Some(format!(
                "{}'s last readback is too old; query it again (Board recovery).",
                board.sender_id()
            ));
        }
        if !readback_healthy_idle(*board, observation, now) {
            return Some(format!(
                "{} reads back {}, not healthy in Startup or Idle with safe outputs. Recover it first (Board recovery).",
                board.sender_id(),
                board_state_label(*board, row.state)
            ));
        }
    }
    None
}

/// Why Recover station is refused now, or None when it may run. Checked in
/// the order an operator can act on. With `router` the network checks run
/// too (time sync, frames still owed to boards); the action policy leaves
/// them to the request. Never waits on an owner another path may hold.
// Safety catalog (About safety): recover-station. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn block_reason(state: &AppState, router: bool) -> Option<String> {
    if !latched(state) {
        return Some("The station Abort is not latched; there is nothing to recover.".into());
    }
    if crate::rrls::is_rrls() {
        return Some("Only the primary ground station can recover the station.".into());
    }
    if state.shutdown_requested.load(Ordering::Acquire) {
        return Some("The ground station is shutting down.".into());
    }
    match state.board_recovery.coordinator.0.try_lock() {
        Ok(control) => {
            if control.preparing != 0
                || control.ticket.as_ref().is_some_and(|ticket| ticket.created.elapsed() < CONFIRM_TTL)
                || control.operation.as_ref().is_some_and(|value| value["completed"] == false)
            {
                return Some("A board recovery is being prepared or is running; wait for it to finish.".into());
            }
        }
        Err(_) => return Some("Board recovery is busy; try again in a moment.".into()),
    }
    {
        let Ok(history) = state.launch_history.try_lock() else {
            return Some("The launch journal is busy; try again in a moment.".into());
        };
        if !history.recovery_available() {
            return Some("Launch history storage is unavailable, so the recovery cannot be recorded.".into());
        }
        if history.recovery_pending().is_some_and(|pending| !pending.published) {
            drop(history);
            let blockers = pending_blockers(state);
            return Some(if blockers.unsatisfied & 0x7f != 0 {
                format!(
                    "Board recovery is unfinished for {}. Finish it first (Board recovery).",
                    board_names(blockers.unsatisfied & 0x7f).join(", ")
                )
            } else {
                "The unfinished board recovery is finishing; try again in a moment.".into()
            });
        }
    }
    if state.operator_admission_barrier.load(Ordering::Acquire) != 0 {
        return Some("Station commands are closed by an unfinished board recovery; finish it first.".into());
    }
    let current = state.local_flight_state_snapshot();
    if !matches!(
        current,
        crate::types::FlightState::Aborted | crate::types::FlightState::Startup | crate::types::FlightState::Idle
    ) {
        return Some(format!(
            "The station is in {}; Recover station needs it Aborted, Startup or Idle. Use Reset all boards.",
            crate::mission_phase::detailed_label(current)
        ));
    }
    if let Some(reason) = crate::gse::station_recovery_work_block_reason(state) {
        return Some(reason);
    }
    if router {
        let Some(router) = state.topology_router.get() else {
            return Some("The hardware network router is not running.".into());
        };
        if crate::system_clock::required_control_network_utc_ms(router).is_err() {
            return Some("Network time is not available yet; wait for the boards' time sync.".into());
        }
        // An Abort, valve, actuator or flight command still owed to a board.
        if let Some(reason) = crate::gse::operator_reset_block_reason(state) {
            return Some(reason);
        }
    }
    board_block_reason(state)
}

/// Action-policy view: present only while the latch is set, with the reason
/// the control is disabled. Presentation only; the request checks again.
pub(crate) fn policy_view(state: &AppState) -> Option<crate::sequences::StationRecoveryView> {
    if crate::rrls::is_rrls() || !latched(state) {
        return None;
    }
    Some(crate::sequences::StationRecoveryView {
        disabled_reason: block_reason(state, false),
        target_state_label: crate::mission_phase::detailed_label(target_state()).to_string(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    #[serde(default)]
    confirm: bool,
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":"recover_station_refused","message":message}))).into_response()
}

/// POST /api/board-recovery/recover-station {"confirm":true}. Same authority as
/// board recovery: the primary, a signed-in account that can send commands.
pub(crate) async fn recover_station(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Request>,
) -> Response {
    // Captured before authentication can wait; closed admission has none.
    let generation = crate::gse::capture_operator_origin(&state);
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if let Err(response) = primary_only() {
        return response;
    }
    if principal.anonymous || !principal.permissions.send_commands {
        return refuse(StatusCode::FORBIDDEN, "Sign in with an account that can send commands to recover the station.");
    }
    if !request.confirm {
        return refuse(StatusCode::BAD_REQUEST, "Confirm recovering the station; no board is reset.");
    }
    let claim = generation.and_then(|generation| crate::gse::claim_operator_dispatch(&state, generation));
    let Some(claim) = claim else {
        return refuse(StatusCode::CONFLICT, &admission_closed_message(&state, "Recover station"));
    };
    let actor = principal.username.clone().unwrap_or_default();
    // A held SDK core decided nothing: check again, briefly.
    match crate::core_busy::retry(
        || run(&state, &claim, &actor),
        |error: &(StatusCode, String)| error.1 == crate::gse::ROUTER_CORE_BUSY,
    )
    .await
    {
        Ok(value) => Json(value).into_response(),
        Err((status, message)) => refuse(status, &message),
    }
}

fn run(
    state: &Arc<AppState>,
    claim: &crate::gse::OperatorDispatchClaim,
    actor: &str,
) -> Result<Value, (StatusCode, String)> {
    // What an Abort or reset would change, sampled before the checks.
    let basis = crate::gse::StationRecoveryBasis::capture(state);
    if let Some(reason) = block_reason(state, true) {
        return Err((StatusCode::CONFLICT, reason));
    }
    let router = state
        .topology_router
        .get()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "The hardware network router is not running.".to_string()))?
        .clone();
    let next = target_state();
    // A PREPARE that slips in after the check above closes host admission only
    // at CONFIRM, which the commit refuses; one that confirms after it finds
    // the station in Startup or Idle and rechecks its own host admission.
    let moved = crate::gse::commit_station_recovery(state, basis, next)
        .map_err(|message| (StatusCode::CONFLICT, message.to_string()))?;
    // No operator Abort re-send is owed once the station is no longer Aborted.
    state.abort_resend.disarm("the station was recovered");
    let record = crate::launch_history::StationRecovery {
        utc_ms: crate::telemetry_task::get_current_timestamp_ms(),
        actor: actor.to_string(),
        from_state: moved.map(|from| format!("{from:?}")),
        to_state: moved.map(|_| format!("{next:?}")),
    };
    if let Err(error) = state.launch_history.lock().unwrap().record_station_recovery(record) {
        log::error!("Recording Recover station by {actor} failed: {error:#}");
        state.add_notification(format!("Station recovered, but the launch journal could not record it: {error}"));
    }
    let mut published = true;
    if let Some(from) = moved {
        // The station's own FLIGHT_STATE: its durable value is what a restart
        // restores. A failure leaves the cached Aborted, which a restart then
        // restores again (fail closed); say so.
        if let Err(error) = crate::network_variables::set_flight_state(&router, next as u8) {
            published = false;
            log::error!("Recover station: publishing {next:?} failed: {error:#}");
            state.add_notification(format!(
                "Station recovered locally, but its {next:?} state could not be published or saved: {error}. A restart returns it to Aborted; use Reset all boards."
            ));
        }
        state.observe_local_flight_state_change(from, next, Some(claim), false);
        // An Abort that landed after the commit has already set Aborted again;
        // its FLIGHT_STATE must stay the latest the boards see.
        let current = state.local_flight_state_snapshot();
        if current != next {
            crate::network_variables::set_flight_state(&router, current as u8).ok();
        }
    }
    crate::sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
    let station = state.local_flight_state_snapshot();
    log::warn!("Station recovered by {actor} without resetting boards ({moved:?} -> {station:?})");
    state.add_notification(format!(
        "Station recovered by {actor}: its Abort latch is cleared and it is in {}. No board was reset.",
        crate::mission_phase::detailed_label(station)
    ));
    Ok(json!({
        "recovered": true,
        "flight_state": station as u8,
        "flight_state_label": crate::mission_phase::detailed_label(station),
        "from_state": moved.map(|from| from as u8),
        "published": published,
    }))
}

#[cfg(test)]
#[path = "station_recover_tests.rs"]
mod tests;
