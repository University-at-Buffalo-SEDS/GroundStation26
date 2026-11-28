//! The operator Kalman filter commands (FC bytes 34..36; FlightComputer26
//! `docs/kalman-commands.md`).
//!
//! - Kalman: stop (`KalmanStop`) stops an FC flight evaluation that no
//!   station Launch started (a false launch detection on the pad) without
//!   changing the global state or reverting anything to Startup.
//! - Kalman: reset (`KalmanReset`) clears the stopped estimator's memory so
//!   the next start begins from scratch.
//! - Kalman: start (`KalmanStart`, HITL only) starts flight evaluation by hand.
//!
//! A real flight must never be stoppable by accident: Stop and Reset are
//! refused once the station's own Launch was sent (a Launch attempt on the
//! launch journal, the Launch latch, or a Launch sequence command in flight),
//! until Reset all boards clears that. The FC refuses them too for a flight a
//! station Launch admitted. Stop and Reset ask for confirmation.
use crate::state::AppState;
use crate::types::TelemetryCommand;

pub(crate) const STOP: &str = "KalmanStop";
pub(crate) const RESET: &str = "KalmanReset";
pub(crate) const START: &str = "KalmanStart";

/// The station's own Launch was sent this flight.
pub(crate) fn station_launch_sent(state: &AppState) -> bool {
    use std::sync::atomic::Ordering;
    state.launch_indicator_latched()
        || state.launch_sequence_command_pending.load(Ordering::Acquire)
        || state.launch_history.lock().unwrap().launch_locked()
}

fn is_stop_or_reset(name: &str) -> bool {
    name == STOP || name == RESET
}

/// Why `name` is refused now, for the Kalman commands.
pub(crate) fn refusal_by_name(state: &AppState, name: &str) -> Option<String> {
    if name == START && !crate::runtime_mode::is_hitl() {
        return Some(crate::runtime_mode::unavailable(START));
    }
    if is_stop_or_reset(name) && station_launch_sent(state) {
        return Some(
            "refused: the station's Launch was sent, so the FC may be flying. Only an FC flight \
             evaluation no station Launch started (a false launch detection) can be stopped or reset; \
             Reset all boards clears this."
                .into(),
        );
    }
    None
}

pub(crate) fn refusal(state: &AppState, cmd: &TelemetryCommand) -> Option<String> {
    match cmd {
        TelemetryCommand::KalmanStop | TelemetryCommand::KalmanReset | TelemetryCommand::KalmanStart => {
            refusal_by_name(state, crate::sequences::command_name(cmd))
        }
        _ => None,
    }
}

/// Disables the Kalman controls the station would refuse, with the reason.
pub(crate) fn block_policy(state: &AppState, policy: &mut crate::sequences::ActionPolicyMsg) {
    for control in &mut policy.controls {
        if !matches!(control.cmd.as_str(), STOP | RESET | START) {
            continue;
        }
        if let Some(reason) = refusal_by_name(state, &control.cmd) {
            control.enabled = false;
            control.disabled_reason = Some(reason);
            control.blink = crate::sequences::BlinkMode::None;
        }
    }
}

/// After a Kalman stop the FC returns its own flight state to the pad state
/// without publishing FLIGHT_STATE (the station's state is not its to
/// change). Ask the FC for a fresh readback (the board-recovery QUERY that
/// feeds the station's view of the FC phase, `fc_phase`) once it has had
/// time to stop, so the station stops showing a phase the FC has left. Its
/// estimator status follows in the FC's next BOARD_HEALTH (sent at once on a
/// change). This changes no station state.
pub(crate) fn refresh_fc_after_stop(state: &std::sync::Arc<AppState>) {
    let Some(index) = crate::board_abort::index(crate::types::Board::FlightComputer) else { return };
    let state = state.clone();
    tokio::spawn(async move {
        for delay_ms in [1_500u64, 4_000] {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            state.board_abort.request_queries(1 << index);
        }
    });
}

/// The confirmation the client shows before sending (layout `confirm`).
pub(crate) fn confirmation(cmd: &str) -> Option<crate::layout::ActionConfirm> {
    let (title, message, button) = match cmd {
        STOP => (
            "Stop the FC's Kalman filter?",
            "Use this only for a false launch detection: the FC is evaluating a flight while the vehicle is on the pad. \
             The FC stops its estimator, cancels that flight evaluation and disarms its backup drogue timer; its own flight \
             state returns to the pad state. Automatic deployment cannot come from the stopped evaluation. The station's \
             state, fill and valves do not change, and manual Deploy and Abort stay available. The FC and the station \
             refuse this after a station Launch.",
            "Stop Kalman filter",
        ),
        RESET => (
            "Reset the FC's Kalman filter?",
            "The FC clears its stopped estimator's memory so the next start begins from scratch. The FC refuses this \
             while the estimator runs (stop it first) and after a station Launch. Nothing else changes: no reset to \
             Startup, no state, fill or valve change.",
            "Reset Kalman filter",
        ),
        _ => return None,
    };
    Some(crate::layout::ActionConfirm { title: title.into(), message: message.into(), button: button.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_and_reset_are_refused_once_the_station_launch_was_sent() {
        let state = crate::state::tests::test_app_state().await;
        assert_eq!(refusal(&state, &TelemetryCommand::KalmanStop), None);
        assert_eq!(refusal(&state, &TelemetryCommand::KalmanReset), None);
        state.set_launch_indicator_latched(true);
        for cmd in [TelemetryCommand::KalmanStop, TelemetryCommand::KalmanReset] {
            let reason = refusal(&state, &cmd).expect("refused after a station Launch");
            assert!(reason.contains("Launch was sent"), "{reason}");
            assert!(!state.is_command_allowed(&cmd));
        }
        state.set_launch_indicator_latched(false);
        state
            .launch_sequence_command_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(refusal(&state, &TelemetryCommand::KalmanStop).is_some());
        state
            .launch_sequence_command_pending
            .store(false, std::sync::atomic::Ordering::SeqCst);
        state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
        assert!(refusal(&state, &TelemetryCommand::KalmanReset).is_some());
        let mut policy = state.action_policy.lock().unwrap().clone();
        policy.controls.push(crate::sequences::ActionControl {
            disabled_reason: None, requested: None, network_note: None, progress: None,
            cmd: STOP.into(), enabled: true, blink: crate::sequences::BlinkMode::None, actuated: None,
        });
        block_policy(&state, &mut policy);
        let stop = policy.controls.iter().find(|c| c.cmd == STOP).unwrap();
        assert!(!stop.enabled && stop.disabled_reason.as_deref().unwrap().contains("Launch was sent"));
    }

    #[tokio::test]
    async fn start_is_hitl_only() {
        let state = crate::state::tests::test_app_state().await;
        let start = refusal_by_name(&state, START);
        assert_eq!(start.is_none(), crate::runtime_mode::is_hitl(), "{start:?}");
        // Deserialized only where it exists.
        let parsed = serde_json::from_str::<TelemetryCommand>("\"KalmanStart\"");
        assert_eq!(parsed.is_ok(), crate::runtime_mode::is_hitl());
        let stop = serde_json::from_str::<TelemetryCommand>("\"KalmanStop\"");
        assert_eq!(stop.is_ok(), !crate::runtime_mode::is_test_fire());
    }

    #[test]
    fn stop_and_reset_ask_first_and_name_their_scope() {
        let stop = confirmation(STOP).unwrap();
        assert!(stop.message.contains("false launch detection") && stop.message.contains("do not change"));
        assert!(confirmation(RESET).unwrap().message.contains("stop it first"));
        assert!(confirmation(START).is_none());
    }
}
