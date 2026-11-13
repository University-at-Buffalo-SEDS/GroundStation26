//! Why the flight computer refuses ordinary flight commands.
//!
//! The FC acknowledges and drops a FLIGHT_COMMAND that fails its store and
//! recovery-floor check (FlightComputer26 distribution.c), so the operator saw
//! only that nothing happened. The FC reports the reason in BOARD_HEALTH word
//! 6 bits 24..28 (`board_recovery_controller_gate`), sent every 5 s:
//! 1 store not ready, 2 commissioning hold, 4 store ownership fault,
//! 8 recovery floor failed, 16 watchdog inhibit (a watchdog reset the FC could
//! not resume from: it refuses flight commands until a power cycle). Bits 0..15 are the watchdog mask and RF/PB
//! service faults use 16..23; older FC firmware leaves 24..31 zero.
//!
//! The operator is told once when the FC starts refusing (and why), once
//! when it accepts again, and for each press made while it refuses. Deploy
//! and Expand are admitted past this check (FC 3f64653), so a press of either
//! raises no refusal notice. A test fire has no AV bay and raises none.
use crate::state::AppState;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REASONS: [(u32, &str); 5] = [
    (1, "its persistent store is not ready"),
    (2, "a recovery commissioning hold"),
    (4, "a persistent store ownership fault"),
    (8, "its recovery floor failed"),
    (16, "a watchdog reset it could not resume from; flight commands and actuation stay disabled until a power cycle"),
];
/// Word 6 bit 28: the FC's watchdog inhibit (FlightComputer26 fc_recovery_watchdog_inhibited).
const WATCHDOG_INHIBIT: u32 = 16;
/// Flight commands the FC refuses while it holds its sequence. Deploy, Expand and
/// EvaluationAbort pass that hold (FC recovery.c fc_recovery_command_admissible).
const HELD_COMMANDS: [&str; 25] = [
    "Postinit", "Launch", "GroundStationLaunch", "MonitorAltitude", "RevokeMonitorAltitude", "ConsecutiveSamples",
    "RevokeConsecutiveSamples", "ResetFailures", "RevokeResetFailures", "ValidateMeasms", "RevokeValidateMeasms",
    "EvaluationRelax", "EvaluationFocus", "ReinitSensors", "ReinitBarometer", "EnableIMU", "DisableIMU",
    "AdvanceFlightState", "RewindFlightState", "AbortAfter40", "AbortAfter100", "AbortAfter250", "ReinitAfter15",
    "ReinitAfter30", "ReinitAfter50",
];
/// A report older than this no longer describes the FC (reports every 5 s).
const REPORT_FRESH: Duration = Duration::from_secs(12);
/// A press waits this long for the next report when the last one is stale.
const PRESS_WINDOW: Duration = Duration::from_secs(12);

#[derive(Default)]
struct Gate {
    bits: u32,
    reported_at: Option<Instant>,
    /// A press made without a fresh report, waiting for the next one.
    waiting: Option<(String, Instant)>,
}

static GATES: Mutex<BTreeMap<usize, Gate>> = Mutex::new(BTreeMap::new());

fn key(state: &AppState) -> usize {
    std::ptr::from_ref(state) as usize
}

fn quiet() -> bool {
    crate::station_mode::local_mode() == crate::station_mode::Mode::TestFire
}

/// The refusal bits of an FC BOARD_HEALTH word 6.
pub(crate) fn bits_from_word6(word6: u32) -> u32 {
    (word6 >> 24) & 0x1F
}

/// "its persistent store is not ready and its recovery floor failed".
pub(crate) fn describe(bits: u32) -> Option<String> {
    let reasons: Vec<&str> = REASONS
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, text)| *text)
        .collect();
    match reasons.as_slice() {
        [] => None,
        [one] => Some((*one).to_string()),
        [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
    }
}

fn refused(label: &str, reason: &str) -> String {
    format!("{label}: refused by FC because {reason}. Pressing again will not help until this clears.")
}

/// An FC BOARD_HEALTH report arrived. Returns the notices to raise.
pub(crate) fn observe_at(state: &AppState, word6: u32, now: Instant) -> Vec<String> {
    let bits = bits_from_word6(word6);
    let mut gates = GATES.lock().unwrap();
    let gate = gates.entry(key(state)).or_default();
    let previous = gate.bits;
    gate.bits = bits;
    gate.reported_at = Some(now);
    let waiting = gate.waiting.take();
    drop(gates);
    if quiet() {
        return Vec::new();
    }
    let mut notices = Vec::new();
    if bits != previous {
        notices.push(match describe(bits) {
            Some(reason) => format!("FC refuses flight commands: {reason}. A refused command does nothing."),
            None => "FC accepts flight commands again.".to_string(),
        });
    }
    if let (Some((label, pressed_at)), Some(reason)) = (waiting, describe(bits))
        && now.duration_since(pressed_at) <= PRESS_WINDOW
    {
        notices.push(refused(&label, &reason));
    }
    notices
}

/// An operator pressed a flight command. Returns a notice when the FC is
/// known to refuse it now; with no fresh report, the next report decides.
pub(crate) fn note_press_at(state: &AppState, byte: u8, label: &str, now: Instant) -> Option<String> {
    use crate::rocket_commands::FlightComputerCommands as Fc;
    if byte == Fc::DeployParachute as u8 || byte == Fc::ExpandParachute as u8 || quiet() {
        return None;
    }
    let mut gates = GATES.lock().unwrap();
    let gate = gates.entry(key(state)).or_default();
    let fresh = gate
        .reported_at
        .is_some_and(|at| now.duration_since(at) <= REPORT_FRESH);
    if fresh {
        gate.waiting = None;
        return describe(gate.bits).map(|reason| refused(label, &reason));
    }
    gate.waiting = Some((label.to_string(), now));
    None
}

/// The FC's current refusal reason, from a fresh report.
pub(crate) fn active_reason(state: &AppState, now: Instant) -> Option<String> {
    let gates = GATES.lock().unwrap();
    let gate = gates.get(&key(state))?;
    gate.reported_at.filter(|at| now.duration_since(*at) <= REPORT_FRESH)?;
    describe(gate.bits)
}

/// A fresh report of the watchdog inhibit refuses the FC's held flight commands inline, with the reason,
/// instead of sending presses the FC acknowledges and drops (fullnet S-Hbench: an FC hang, its IWDG reset,
/// then every flight command answered "ok" and never applied).
pub(crate) fn block_policy(state: &AppState, policy: &mut crate::sequences::ActionPolicyMsg) {
    if quiet() { return; }
    let inhibited = {
        let gates = GATES.lock().unwrap();
        gates.get(&key(state)).is_some_and(|gate| {
            gate.bits & WATCHDOG_INHIBIT != 0 && gate.reported_at.is_some_and(|at| at.elapsed() <= REPORT_FRESH)
        })
    };
    if !inhibited { return; }
    for control in &mut policy.controls {
        if HELD_COMMANDS.contains(&control.cmd.as_str()) {
            control.enabled = false;
            control.disabled_reason = Some(format!("FC refuses {} after {}.", control.cmd, REASONS[4].1));
            control.blink = crate::sequences::BlinkMode::None;
        }
    }
}

/// Raise every notice from an FC BOARD_HEALTH report.
pub(crate) fn observe(state: &AppState, word6: u32) {
    for notice in observe_at(state, word6, Instant::now()) {
        log::warn!("{notice}");
        crate::web::emit_notification_warning(state, notice);
    }
}

/// Test states can reuse a dropped state's address: start each test clean.
#[cfg(test)]
pub(crate) fn forget(state: &AppState) {
    GATES.lock().unwrap().remove(&key(state));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rocket_commands::FlightComputerCommands as Fc;

    const BENCH: u32 = (1 | 8) << 24;

    #[test]
    fn reasons_read_only_bits_24_to_27() {
        assert_eq!(
            bits_from_word6(0x0000_ffff | (0xff << 16)),
            0,
            "watchdog mask and RF/PB service faults"
        );
        assert_eq!(bits_from_word6(BENCH | 5), 9);
        assert_eq!(describe(0), None);
        assert_eq!(
            describe(9).unwrap(),
            "its persistent store is not ready and its recovery floor failed"
        );
        assert_eq!(
            describe(15).unwrap(),
            "its persistent store is not ready, a recovery commissioning hold, a persistent store ownership fault and its recovery floor failed"
        );
    }

    /// FC watchdog reset with no resumable record (word 6 bit 28): every ordinary flight command is refused
    /// inline with the reason; Deploy, Expand and EvaluationAbort, which the FC still admits, stay available.
    #[tokio::test]
    async fn a_watchdog_inhibited_fc_refuses_flight_commands_inline() {
        let state = crate::state::tests::test_app_state().await;
        forget(&state);
        if quiet() { forget(&state); return; }
        assert_eq!(bits_from_word6(16 << 24), 16);
        assert!(describe(16).unwrap().contains("power cycle"));
        observe_at(&state, 16 << 24, Instant::now());
        let mut policy = state.action_policy.lock().unwrap().clone();
        block_policy(&state, &mut policy);
        let control = |cmd: &str| policy.controls.iter().find(|c| c.cmd == cmd).cloned();
        assert!(control("MonitorAltitude").is_some(), "MonitorAltitude control missing");
        for cmd in ["MonitorAltitude", "ReinitSensors", "EnableIMU", "Postinit"] {
            let Some(c) = control(cmd) else { continue };
            assert!(!c.enabled, "{cmd}");
            assert!(c.disabled_reason.unwrap_or_default().contains("power cycle"), "{cmd}");
        }
        for cmd in ["DeployParachute", "ExpandParachute"] {
            if let Some(c) = control(cmd) {
                assert!(!c.disabled_reason.unwrap_or_default().contains("power cycle"), "{cmd} stays available");
            }
        }
        // The store/floor reasons keep their notice-only behaviour.
        observe_at(&state, 1 << 24, Instant::now());
        let mut policy = state.action_policy.lock().unwrap().clone();
        block_policy(&state, &mut policy);
        assert!(!policy.controls.iter().any(|c| c.disabled_reason.as_deref().is_some_and(|r| r.contains("power cycle"))));
        forget(&state);
    }

    #[tokio::test]
    async fn the_bench_fault_is_announced_once_and_its_clearing_too() {
        let state = crate::state::tests::test_app_state().await;
        forget(&state);
        if quiet() {
            assert!(observe_at(&state, BENCH, Instant::now()).is_empty(), "a test fire has no AV bay");
            forget(&state);
            return;
        }
        let t = Instant::now();
        assert!(
            observe_at(&state, 5, t).is_empty(),
            "an accepting FC with only watchdog bits says nothing"
        );
        assert_eq!(
            observe_at(&state, BENCH | 5, t + Duration::from_secs(5)),
            vec![
                "FC refuses flight commands: its persistent store is not ready and its recovery floor failed. A refused command does nothing."
            ]
        );
        assert!(
            observe_at(&state, BENCH, t + Duration::from_secs(10)).is_empty(),
            "no repeat every 5 s"
        );
        assert_eq!(
            observe_at(&state, 0, t + Duration::from_secs(15)),
            vec!["FC accepts flight commands again."]
        );
        forget(&state);
    }

    #[tokio::test]
    async fn a_press_while_the_fc_refuses_names_the_reason() {
        let state = crate::state::tests::test_app_state().await;
        forget(&state);
        if quiet() {
            assert!(observe_at(&state, BENCH, Instant::now()).is_empty(), "a test fire has no AV bay");
            forget(&state);
            return;
        }
        let t = Instant::now();
        observe_at(&state, BENCH, t);
        let relax = Fc::EvaluationRelax as u8;
        assert_eq!(
            note_press_at(&state, relax, "EvaluationRelax", t + Duration::from_secs(2)).as_deref(),
            Some(
                "EvaluationRelax: refused by FC because its persistent store is not ready and its recovery floor failed. Pressing again will not help until this clears."
            )
        );
        assert_eq!(
            active_reason(&state, t + Duration::from_secs(2)).as_deref(),
            Some("its persistent store is not ready and its recovery floor failed")
        );
        // Deploy and Expand are admitted past the store/floor check.
        assert_eq!(
            note_press_at(
                &state,
                Fc::DeployParachute as u8,
                "DeployParachute",
                t + Duration::from_secs(3)
            ),
            None
        );
        assert_eq!(
            note_press_at(
                &state,
                Fc::ExpandParachute as u8,
                "ExpandParachute",
                t + Duration::from_secs(3)
            ),
            None
        );
        // An accepting FC raises nothing for a press.
        observe_at(&state, 0, t + Duration::from_secs(5));
        assert_eq!(
            note_press_at(&state, relax, "EvaluationRelax", t + Duration::from_secs(6)),
            None
        );
        forget(&state);
    }

    #[tokio::test]
    async fn a_press_without_a_fresh_report_waits_for_the_next_one() {
        let state = crate::state::tests::test_app_state().await;
        forget(&state);
        if quiet() {
            assert!(observe_at(&state, BENCH, Instant::now()).is_empty(), "a test fire has no AV bay");
            forget(&state);
            return;
        }
        let t = Instant::now();
        let relax = Fc::EvaluationRelax as u8;
        assert_eq!(
            note_press_at(&state, relax, "EvaluationRelax", t),
            None,
            "nothing known yet"
        );
        let notices = observe_at(&state, BENCH, t + Duration::from_secs(4));
        assert_eq!(notices.len(), 2);
        assert!(notices[1].starts_with("EvaluationRelax: refused by FC because its persistent store is not ready"));
        // A stale press is not attributed to a later refusal.
        observe_at(&state, 0, t + Duration::from_secs(5));
        note_press_at(&state, relax, "EvaluationRelax", t + Duration::from_secs(30));
        let notices = observe_at(&state, BENCH, t + Duration::from_secs(60));
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(
            active_reason(&state, t + Duration::from_secs(90)),
            None,
            "a stale report is not current"
        );
        forget(&state);
    }
}
