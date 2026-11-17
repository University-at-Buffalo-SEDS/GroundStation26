//! Why the actuator board refused an operator command.
//!
//! The AB acknowledges and drops an ACTUATOR_COMMAND or fill-drive change it
//! refuses (a stale stamp, the wrong fill drive, a pending recovery...), so
//! during the 2026-11-16 test fire refused AUX QD retracts and drive changes
//! showed the operator nothing. The AB now reports its last refusal in
//! BOARD_HEALTH (ActuatorBoard26 `board_refusal.h`): word 16 the "ABR1" tag,
//! word 17 `sequence << 16 | command << 8 | reason`, word 18 the ms since the
//! AB first reported it. A new refusal is reported at once, not in up to 5 s.
//! Older AB firmware leaves word 16 zero.
//!
//! Each refusal raises one operator notice ("QD retract: refused by AB
//! because the selected fill drive does not support it."). A refused drive
//! change also shows on the fill-drive panel. A refusal the AB reported long
//! before this station heard it (station restart) raises nothing.
use crate::state::AppState;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// "ABR1" little-endian in AB BOARD_HEALTH word 16.
const TAG: u32 = 0x3152_4241;
/// A refusal older than this when first heard is history, not news.
const NEWS_MS: u32 = 15_000;
/// How long a drive-change refusal explains the drive panel's result.
const DRIVE_NOTE: Duration = Duration::from_secs(15);
/// `command` for a fill-drive change: 0x40 | requested mode.
const DRIVE_SET: u8 = 0x40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) sequence: u16,
    pub(crate) command: u8,
    pub(crate) reason: u8,
    pub(crate) age_ms: u32,
    /// AB uptime (ms, low 32 bits) when it first reported this refusal: with
    /// the sequence it names one refusal across reports and AB restarts.
    first_reported_ms: u32,
}

/// The AB's last refusal from its BOARD_HEALTH words; None for older
/// firmware or when it has refused nothing since boot.
pub(crate) fn decode(words: &[u32]) -> Option<Refusal> {
    if words.len() != 24 || words[16] != TAG || words[17] >> 16 == 0 {
        return None;
    }
    Some(Refusal {
        sequence: (words[17] >> 16) as u16,
        command: (words[17] >> 8) as u8,
        reason: words[17] as u8,
        age_ms: words[18],
        first_reported_ms: words[2].wrapping_sub(words[18]),
    })
}

/// Operator text for an AB refusal reason code.
pub(crate) fn describe(reason: u8) -> String {
    match reason {
        1 => "its timestamp was outside the board's freshness window (late delivery or a station clock mismatch)",
        2 => "it was stamped at or before the board's recovery floor (sent before the last board recovery)",
        3 => "the selected fill drive does not support it (check the AUX or stepper drive selection)",
        4 => "a flight-state interlock blocks it (drive changes need Startup or Idle)",
        5 => "a fill-drive motion or drive change is still in progress",
        6 => "the board is not commissioned (critical commissioning is pending)",
        7 => "a board state recovery is pending",
        8 => "Abort is latched on the board",
        9 => "its persistent store is not ready",
        10 => "its fill-drive setting is not loaded",
        11 => "its fill drive is faulted",
        12 => "it names an older board boot or Stop epoch (refresh the drive readback and try again)",
        13 => "it was sent before the last fill-drive change",
        14 => "its command owner latched a fault",
        other => return format!("of reason code {other}"),
    }
    .to_string()
}

fn drive_name(mode: u8) -> &'static str {
    match mode {
        0 => "Stepper",
        1 => "H-bridge",
        2 => "Auxiliary",
        _ => "unknown drive",
    }
}

/// Operator name for the refused AB command byte.
pub(crate) fn label(command: u8) -> String {
    use crate::rocket_commands::ActuatorBoardCommands as Ab;
    let name = match command {
        c if c == Ab::IgniterOn as u8 => "Igniter on",
        c if c == Ab::RetractPlumbing as u8 => "QD retract",
        c if c == Ab::NitrogenOpen as u8 => "Nitrogen open",
        c if c == Ab::NitrousOpen as u8 => "Nitrous open",
        c if c == Ab::IgniterOff as u8 => "Igniter off",
        c if c == Ab::NitrogenClose as u8 => "Nitrogen close",
        c if c == Ab::NitrousClose as u8 => "Nitrous close",
        c if c == Ab::IgniterSequence as u8 => "Igniter sequence",
        15 => "QD extend",
        c if c & 0xFC == DRIVE_SET => return format!("Fill drive change to {}", drive_name(c & 3)),
        other => return format!("AB command {other}"),
    };
    name.to_string()
}

#[derive(Default)]
struct Gate {
    last: Option<(u16, u32)>,
    /// The last refused drive change: (reason, heard at).
    drive: Option<(String, Instant)>,
}

static GATES: Mutex<BTreeMap<usize, Gate>> = Mutex::new(BTreeMap::new());

fn key(state: &AppState) -> usize {
    std::ptr::from_ref(state) as usize
}

/// An AB BOARD_HEALTH report arrived. Returns the notice for a refusal this
/// station has not reported yet.
pub(crate) fn observe_at(state: &AppState, words: &[u32], now: Instant) -> Option<String> {
    let refusal = decode(words)?;
    let mut gates = GATES.lock().unwrap();
    let gate = gates.entry(key(state)).or_default();
    let identity = (refusal.sequence, refusal.first_reported_ms);
    if gate.last == Some(identity) {
        return None;
    }
    gate.last = Some(identity);
    if refusal.age_ms > NEWS_MS {
        return None;
    }
    let reason = describe(refusal.reason);
    if refusal.command & 0xFC == DRIVE_SET {
        gate.drive = Some((reason.clone(), now));
    }
    Some(format!("{}: refused by AB because {reason}.", label(refusal.command)))
}

/// Why the AB refused the latest drive change, if it did so recently.
pub(crate) fn recent_drive_refusal(state: &AppState, now: Instant) -> Option<String> {
    let gates = GATES.lock().unwrap();
    let (reason, at) = gates.get(&key(state))?.drive.as_ref()?;
    (now.duration_since(*at) <= DRIVE_NOTE).then(|| reason.clone())
}

/// Raise the notice for an AB BOARD_HEALTH report, and show a refused drive
/// change on the fill-drive panel.
pub(crate) fn observe(state: &AppState, words: &[u32]) {
    let Some(notice) = observe_at(state, words, Instant::now()) else {
        return;
    };
    log::warn!("{notice}");
    if notice.starts_with("Fill drive change") {
        crate::actuator_plumbing::note_board_refusal(state, notice.clone());
    }
    crate::web::emit_notification_warning(state, notice);
}

/// Test states can reuse a dropped state's address: start each test clean.
#[cfg(test)]
pub(crate) fn forget(state: &AppState) {
    GATES.lock().unwrap().remove(&key(state));
}

#[cfg(test)]
pub(crate) fn health_words(uptime_ms: u32, sequence: u16, command: u8, reason: u8, age_ms: u32) -> [u32; 24] {
    let mut words = [0u32; 24];
    words[0] = 1;
    words[2] = uptime_ms;
    words[4] = 1000;
    words[16] = TAG;
    words[17] = (u32::from(sequence) << 16) | (u32::from(command) << 8) | u32::from(reason);
    words[18] = age_ms;
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_only_tagged_reports_with_a_refusal() {
        let words = health_words(90_000, 3, 8, 3, 250);
        assert_eq!(
            decode(&words),
            Some(Refusal { sequence: 3, command: 8, reason: 3, age_ms: 250, first_reported_ms: 89_750 })
        );
        let mut older = words;
        older[16] = 0;
        assert_eq!(decode(&older), None, "older AB firmware");
        assert_eq!(decode(&health_words(1_000, 0, 0, 0, 0)), None, "no refusal since boot");
        assert_eq!(decode(&words[..23]), None);
    }

    #[test]
    fn every_required_reason_reads_as_operator_text() {
        assert!(describe(1).contains("freshness window"));
        assert!(describe(2).contains("recovery floor"));
        assert!(describe(3).contains("AUX or stepper"));
        assert!(describe(4).contains("interlock"));
        assert!(describe(5).contains("in progress"));
        assert!(describe(6).contains("not commissioned"));
        assert!(describe(7).contains("recovery is pending"));
        assert_eq!(describe(99), "of reason code 99");
        assert_eq!(label(8), "QD retract");
        assert_eq!(label(0x42), "Fill drive change to Auxiliary");
        assert_eq!(label(0x40), "Fill drive change to Stepper");
        assert_eq!(label(99), "AB command 99");
    }

    #[tokio::test]
    async fn each_refusal_is_announced_once() {
        let state = crate::state::tests::test_app_state().await;
        forget(&state);
        let t = Instant::now();
        assert_eq!(
            observe_at(&state, &health_words(10_000, 1, 8, 3, 0), t).as_deref(),
            Some("QD retract: refused by AB because the selected fill drive does not support it (check the AUX or stepper drive selection).")
        );
        // The periodic report repeats the same refusal: no second notice.
        assert_eq!(observe_at(&state, &health_words(15_000, 1, 8, 3, 5_000), t), None);
        assert_eq!(
            observe_at(&state, &health_words(15_300, 2, 9, 1, 0), t).as_deref(),
            Some("Nitrogen open: refused by AB because its timestamp was outside the board's freshness window (late delivery or a station clock mismatch).")
        );
        // After an AB restart the sequence starts again at 1: still news.
        assert!(observe_at(&state, &health_words(400, 1, 8, 7, 0), t).unwrap().ends_with("a board state recovery is pending."));
        assert_eq!(recent_drive_refusal(&state, t), None, "no drive change was refused");
        forget(&state);
    }

    #[tokio::test]
    async fn an_old_refusal_heard_late_is_not_news() {
        let state = crate::state::tests::test_app_state().await;
        forget(&state);
        let t = Instant::now();
        assert_eq!(observe_at(&state, &health_words(600_000, 4, 8, 5, 300_000), t), None);
        assert_eq!(observe_at(&state, &health_words(605_000, 4, 8, 5, 305_000), t), None);
        assert!(observe_at(&state, &health_words(605_200, 5, 8, 5, 0), t).is_some());
        forget(&state);
    }

    #[tokio::test]
    async fn a_refused_drive_change_explains_the_drive_result() {
        let state = crate::state::tests::test_app_state().await;
        forget(&state);
        let t = Instant::now();
        assert_eq!(
            observe_at(&state, &health_words(10_000, 1, 0x42, 4, 0), t).as_deref(),
            Some("Fill drive change to Auxiliary: refused by AB because a flight-state interlock blocks it (drive changes need Startup or Idle).")
        );
        assert_eq!(
            recent_drive_refusal(&state, t + Duration::from_secs(2)).as_deref(),
            Some("a flight-state interlock blocks it (drive changes need Startup or Idle)")
        );
        assert_eq!(recent_drive_refusal(&state, t + Duration::from_secs(30)), None);
        forget(&state);
    }
}
