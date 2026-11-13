//! Compares the GroundStation's durable 50 kg input selection with the input
//! the DAQ is actually reporting.
//!
//! The DAQ's report stream is the readback: it publishes `KG50` only while
//! scanning AMP2 and tagged `DAQ_KG50_SELECTED` only while scanning P6.
//!
//! On a mismatch the GroundStation wins. Its calibration file holds the
//! operator's selection and the calibration profile bound to that input, and
//! the DAQ only reads this variable. The DAQ's flash copy keeps the same input
//! through a reboot while the GroundStation is unreachable, and is overridden
//! as soon as this value arrives.
use crate::loadcell::Kg50Input;
use crate::state::AppState;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The live stream runs at tens of hertz, so silence this long is an outage.
const REPORT_FRESH: Duration = Duration::from_secs(3);
const REPUBLISH_INTERVAL: Duration = Duration::from_secs(2);
/// A disagreement outlasting several re-sends is worth telling the operator.
const MISMATCH_AFTER: Duration = Duration::from_secs(10);
/// Re-sends at `REPUBLISH_INTERVAL` before backing off. The DAQ acknowledges a
/// selection it does not apply, so more of the same cannot help.
const QUICK_ATTEMPTS: u32 = 3;
/// Backed-off re-sends double from `REPUBLISH_INTERVAL` up to this.
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A send counts as refused only on a report taken this long after it, once
/// the board has had time to switch its scan.
const SETTLE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    Synced,
    /// The board reports another input and the selection is being re-sent.
    Syncing,
    /// As `Syncing`, but for longer than a healthy link would need.
    Mismatch,
    /// No current report, so the board's input is unknown.
    NoReadback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Status {
    pub selected: Kg50Input,
    /// Last input the board reported; check `state` before calling it current.
    pub board_input: Option<Kg50Input>,
    pub board_age_ms: Option<u64>,
    pub state: SyncState,
}

/// One re-send the service should make now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resend {
    /// The one operator warning for this refusal.
    pub warning: Option<String>,
}

#[derive(Default)]
struct Inner {
    board: Option<(Kg50Input, Instant)>,
    disagreeing_since: Option<Instant>,
    last_publish: Option<Instant>,
    /// Re-sends since the board last agreed, reported nothing, or the
    /// operator chose an input.
    attempts: u32,
    /// The operator was warned about this refusal; cleared once synced.
    warned: bool,
}

fn label(input: Kg50Input) -> &'static str {
    match input {
        Kg50Input::Amp2 => "AMP2",
        Kg50Input::P6Differential => "P6 differential",
    }
}

fn refusal_warning(selected: Kg50Input, board: Kg50Input, attempts: u32) -> String {
    format!(
        "DAQ still reports 50 kg input {} after {attempts} re-sends; the station selected {}. \
         The DAQ acknowledges the selection without applying it, most likely because it holds a \
         newer revision of this setting from the station (another station mode's calibration \
         file, a station run that lost the calibration file, or a station clock that stepped \
         back) and drops older ones. The station re-stamps the selection and now retries at \
         most once a minute. If the DAQ still disagrees, reboot the DAQ or update its firmware.",
        label(board),
        label(selected),
    )
}

#[derive(Default)]
pub struct Tracker(Mutex<Inner>);

impl Tracker {
    pub fn observe(&self, input: Kg50Input) {
        self.observe_at(input, Instant::now());
    }
    fn observe_at(&self, input: Kg50Input, now: Instant) {
        self.0.lock().unwrap().board = Some((input, now));
    }
    /// The operator chose an input and it was sent: a fresh start.
    pub fn note_selected(&self) {
        self.note_selected_at(Instant::now());
    }
    fn note_selected_at(&self, now: Instant) {
        let mut inner = self.0.lock().unwrap();
        inner.last_publish = Some(now);
        inner.attempts = 0;
        inner.warned = false;
    }
    /// The service re-sent the selection the board disagrees with.
    pub fn note_resent(&self) {
        self.note_resent_at(Instant::now());
    }
    fn note_resent_at(&self, now: Instant) {
        let mut inner = self.0.lock().unwrap();
        inner.last_publish = Some(now);
        inner.attempts = inner.attempts.saturating_add(1);
    }
    pub fn status(&self, selected: Kg50Input) -> Status {
        self.status_at(selected, Instant::now())
    }
    fn status_at(&self, selected: Kg50Input, now: Instant) -> Status {
        let mut inner = self.0.lock().unwrap();
        let board = inner.board;
        let age = board.map(|(_, at)| now.saturating_duration_since(at));
        let fresh = age.is_some_and(|age| age <= REPORT_FRESH);
        let disagreeing = fresh && board.is_some_and(|(input, _)| input != selected);
        let since = disagreeing.then(|| *inner.disagreeing_since.get_or_insert(now));
        if !disagreeing {
            inner.disagreeing_since = None;
            // Agreement or a silent board ends this refusal: a board that
            // comes back gets quick re-sends again.
            inner.attempts = 0;
            if fresh {
                inner.warned = false;
            }
        }
        let state = if !fresh {
            SyncState::NoReadback
        } else if let Some(since) = since {
            if now.saturating_duration_since(since) >= MISMATCH_AFTER {
                SyncState::Mismatch
            } else {
                SyncState::Syncing
            }
        } else {
            SyncState::Synced
        };
        Status {
            selected,
            board_input: board.map(|(input, _)| input),
            board_age_ms: age.map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX)),
            state,
        }
    }
    /// A sustained mismatch means the board holds a newer write than this
    /// station's stamp; re-stamp at most once per MISMATCH_AFTER.
    fn restamp_due(&self, selected: Kg50Input, stamp_ms: u64, wall_ms: u64, now: Instant) -> bool {
        self.status_at(selected, now).state == SyncState::Mismatch
            && wall_ms.saturating_sub(stamp_ms) >= MISMATCH_AFTER.as_millis() as u64
    }
    /// The re-send due now, if the board still disagrees on a report taken
    /// after the last send settled. A few quick re-sends, then doubling
    /// intervals up to `BACKOFF_MAX`, with one warning when backing off.
    fn resend_plan(&self, selected: Kg50Input, now: Instant) -> Option<Resend> {
        let status = self.status_at(selected, now);
        if !matches!(status.state, SyncState::Syncing | SyncState::Mismatch) {
            return None;
        }
        let mut inner = self.0.lock().unwrap();
        if let Some(last) = inner.last_publish {
            let backoff = inner.attempts.saturating_sub(QUICK_ATTEMPTS - 1).min(16);
            let interval = REPUBLISH_INTERVAL
                .saturating_mul(1 << backoff)
                .min(BACKOFF_MAX);
            let reported_after = inner
                .board
                .is_some_and(|(_, at)| at >= last + SETTLE);
            if now.saturating_duration_since(last) < interval || !reported_after {
                return None;
            }
        }
        let backing_off = inner.attempts >= QUICK_ATTEMPTS;
        let warning = match status.board_input {
            Some(board) if backing_off && !inner.warned => {
                inner.warned = true;
                Some(refusal_warning(selected, board, inner.attempts))
            }
            _ => None,
        };
        Some(Resend { warning })
    }
}

/// Re-sends the selection while the board reports another input. This covers a
/// bootstrap publication that left before the DAQ was discovered, a DAQ that
/// restored a value this station has since replaced, and a DAQ that holds a
/// newer-stamped value and drops this station's older stamp.
pub async fn service(
    state: Arc<AppState>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = shutdown.recv() => return,
        }
        let Some(router) = state.topology_router.get().filter(|_| !crate::rrls::is_rrls()) else {
            continue;
        };
        let selected = state.loadcell_calibration.lock().unwrap().kg50_input;
        let now = Instant::now();
        let Some(plan) = state.kg50_input_sync.resend_plan(selected, now) else {
            continue;
        };
        let Some(generation) = crate::gse::capture_operator_origin(&state) else {
            continue;
        };
        let Some(_claim) = crate::gse::claim_operator_dispatch(&state, generation) else {
            continue;
        };
        let Some(calibration) = restamp_if_due(&state, selected, now) else {
            continue;
        };
        state.kg50_input_sync.note_resent();
        if let Some(warning) = plan.warning {
            log::warn!("{warning}");
            state.add_notification_with_persistence(warning, true);
        }
        match crate::network_variables::set_daq_kg50_input(router, &calibration) {
            Ok(()) => log::info!(
                "DAQ reports a different 50 kg input; re-sent selection {:?} (revision {})",
                calibration.kg50_input,
                calibration.kg50_input_stamp_ms
            ),
            Err(error) => log::warn!("re-sending DAQ 50 kg input failed: {error}"),
        }
    }
}

/// The selection to re-send, re-stamped first when due. The input is a network
/// variable: the newest stamp wins. A board that keeps reporting another input
/// holds a newer write (another station, a station run that lost this file, or
/// another station mode's calibration file), so an older stamp is ignored
/// forever. This station owns the selection: re-stamp it, at most once per
/// MISMATCH_AFTER, so the next send outranks the stray value. Done under the
/// calibration lock so a concurrent operator change is never overwritten; None
/// when the operator changed the selection since the plan was made.
fn restamp_if_due(
    state: &AppState,
    selected: Kg50Input,
    now: Instant,
) -> Option<crate::loadcell::LoadcellCalibrationFile> {
    let mut current = state.loadcell_calibration.lock().unwrap();
    if current.kg50_input != selected {
        return None;
    }
    let wall = crate::telemetry_task::get_current_timestamp_ms();
    if !state
        .kg50_input_sync
        .restamp_due(selected, current.kg50_input_stamp_ms, wall, now)
    {
        return Some(current.clone());
    }
    let mut next = current.clone();
    next.kg50_input_stamp_ms = wall.max(next.kg50_input_stamp_ms.saturating_add(1));
    match crate::loadcell::save(&next) {
        Ok(()) => {
            *current = next;
            log::warn!(
                "DAQ kept reporting another 50 kg input; re-stamped this station's selection {:?} so it outranks the stray value",
                selected
            );
        }
        Err(error) => log::warn!("re-stamping the 50 kg input selection failed: {error}"),
    }
    Some(current.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_board_holding_a_newer_stray_input_gets_a_fresh_stamp_once_per_window() {
        // A station run without this file wrote another input with a newer
        // stamp; re-sends with the old stamp were ignored forever.
        let sync = Tracker::default();
        let t = Instant::now();
        let old_stamp = 1_000;
        sync.observe_at(Kg50Input::Amp2, t);
        assert!(!sync.restamp_due(Kg50Input::P6Differential, old_stamp, 100_000, t), "only once it persists");
        let later = t + MISMATCH_AFTER + Duration::from_secs(1);
        sync.observe_at(Kg50Input::Amp2, later);
        assert!(sync.restamp_due(Kg50Input::P6Differential, old_stamp, 100_000, later));
        // Just re-stamped: wait a full window before stamping again.
        assert!(!sync.restamp_due(Kg50Input::P6Differential, 99_000, 100_000, later));
        // An agreeing board never triggers it.
        sync.observe_at(Kg50Input::P6Differential, later);
        assert!(!sync.restamp_due(Kg50Input::P6Differential, old_stamp, 100_000, later));
    }

    #[test]
    fn unknown_until_the_board_reports() {
        let tracker = Tracker::default();
        let status = tracker.status(Kg50Input::P6Differential);
        assert_eq!(status.state, SyncState::NoReadback);
        assert_eq!(status.board_input, None);
        assert_eq!(status.board_age_ms, None);
        assert_eq!(tracker.resend_plan(Kg50Input::P6Differential, Instant::now()), None);
    }

    #[test]
    fn matching_report_is_synced_and_never_resent() {
        let tracker = Tracker::default();
        let now = Instant::now();
        tracker.observe_at(Kg50Input::Amp2, now);
        assert_eq!(tracker.status_at(Kg50Input::Amp2, now).state, SyncState::Synced);
        assert_eq!(tracker.resend_plan(Kg50Input::Amp2, now), None);
    }

    #[test]
    fn silence_reports_the_last_input_as_stale_without_resending() {
        let tracker = Tracker::default();
        let now = Instant::now();
        tracker.observe_at(Kg50Input::Amp2, now);
        let later = now + REPORT_FRESH + Duration::from_millis(1);
        let status = tracker.status_at(Kg50Input::P6Differential, later);
        assert_eq!(status.state, SyncState::NoReadback);
        assert_eq!(status.board_input, Some(Kg50Input::Amp2));
        assert!(status.board_age_ms.unwrap() > 3_000);
        assert_eq!(tracker.resend_plan(Kg50Input::P6Differential, later), None);
    }

    #[test]
    fn disagreement_is_resent_at_a_bounded_rate_until_it_resolves() {
        let tracker = Tracker::default();
        let start = Instant::now();
        tracker.observe_at(Kg50Input::Amp2, start);
        let selected = Kg50Input::P6Differential;
        assert_eq!(tracker.status_at(selected, start).state, SyncState::Syncing);
        assert_eq!(tracker.resend_plan(selected, start), Some(Resend::default()));
        tracker.note_resent_at(start);
        assert_eq!(tracker.resend_plan(selected, start + Duration::from_secs(1)), None);
        tracker.observe_at(Kg50Input::Amp2, start + REPUBLISH_INTERVAL);
        assert!(tracker.resend_plan(selected, start + REPUBLISH_INTERVAL).is_some());
        tracker.observe_at(selected, start + REPUBLISH_INTERVAL + Duration::from_millis(1));
        let resolved = start + REPUBLISH_INTERVAL + Duration::from_secs(1);
        assert_eq!(tracker.status_at(selected, resolved).state, SyncState::Synced);
        assert_eq!(tracker.resend_plan(selected, resolved), None);
    }

    #[test]
    fn long_disagreement_becomes_a_reported_mismatch_and_a_new_one_starts_over() {
        let tracker = Tracker::default();
        let start = Instant::now();
        let selected = Kg50Input::P6Differential;
        tracker.observe_at(Kg50Input::Amp2, start);
        assert_eq!(tracker.status_at(selected, start).state, SyncState::Syncing);
        let late = start + MISMATCH_AFTER;
        tracker.observe_at(Kg50Input::Amp2, late);
        assert_eq!(tracker.status_at(selected, late).state, SyncState::Mismatch);
        tracker.observe_at(selected, late);
        assert_eq!(tracker.status_at(selected, late).state, SyncState::Synced);
        let again = late + Duration::from_secs(1);
        tracker.observe_at(Kg50Input::Amp2, again);
        assert_eq!(tracker.status_at(selected, again).state, SyncState::Syncing);
    }

    /// Drive the service decisions against a board that acknowledges every
    /// send but keeps reporting AMP2, with a report every 100 ms.
    fn refused_sends(tracker: &Tracker, start: Instant, until: Duration) -> Vec<(Duration, Resend)> {
        let selected = Kg50Input::P6Differential;
        let mut sends = Vec::new();
        let mut at = Duration::ZERO;
        while at <= until {
            tracker.observe_at(Kg50Input::Amp2, start + at);
            if let Some(plan) = tracker.resend_plan(selected, start + at) {
                tracker.note_resent_at(start + at);
                sends.push((at, plan));
            }
            at += Duration::from_millis(100);
        }
        sends
    }

    #[test]
    fn a_refused_selection_backs_off_and_warns_once() {
        let tracker = Tracker::default();
        let start = Instant::now();
        let sends = refused_sends(&tracker, start, Duration::from_secs(600));
        let times: Vec<u64> = sends.iter().map(|(at, _)| at.as_secs()).collect();
        // A few quick re-sends, then doubling intervals capped at one minute.
        assert_eq!(&times[..8], &[0, 2, 4, 8, 16, 32, 64, 124]);
        assert!(times.windows(2).all(|w| w[1] - w[0] <= BACKOFF_MAX.as_secs()));
        assert!(sends.len() < 20, "{times:?}");
        let warnings: Vec<&String> = sends.iter().filter_map(|(_, plan)| plan.warning.as_ref()).collect();
        assert_eq!(warnings.len(), 1);
        let warning = warnings[0];
        assert!(warning.contains("reports 50 kg input AMP2"), "{warning}");
        assert!(warning.contains("selected P6 differential"), "{warning}");
        assert!(warning.contains("newer revision"), "{warning}");
        assert_eq!(sends[QUICK_ATTEMPTS as usize].1.warning.as_ref(), Some(warning));
    }

    #[test]
    fn a_report_from_before_the_last_send_does_not_count_as_a_refusal() {
        let tracker = Tracker::default();
        let start = Instant::now();
        let selected = Kg50Input::P6Differential;
        tracker.observe_at(Kg50Input::Amp2, start);
        assert!(tracker.resend_plan(selected, start).is_some());
        tracker.note_resent_at(start);
        // Still fresh, but it was reported before the board could switch.
        let later = start + REPUBLISH_INTERVAL + Duration::from_millis(500);
        assert_eq!(tracker.status_at(selected, later).state, SyncState::Syncing);
        assert_eq!(tracker.resend_plan(selected, later), None);
        tracker.observe_at(Kg50Input::Amp2, later);
        assert!(tracker.resend_plan(selected, later).is_some());
    }

    #[test]
    fn the_selection_converges_once_the_cause_is_fixed_and_a_new_refusal_warns_again() {
        let tracker = Tracker::default();
        let start = Instant::now();
        let selected = Kg50Input::P6Differential;
        let sends = refused_sends(&tracker, start, Duration::from_secs(300));
        assert!(sends.iter().any(|(_, plan)| plan.warning.is_some()));
        let fixed = start + Duration::from_secs(301);
        tracker.observe_at(selected, fixed);
        assert_eq!(tracker.status_at(selected, fixed).state, SyncState::Synced);
        assert_eq!(tracker.resend_plan(selected, fixed), None);
        // A later, separate refusal starts with quick re-sends and warns again.
        let again = refused_sends(&tracker, fixed + Duration::from_secs(1), Duration::from_secs(60));
        assert_eq!(again[0].0, Duration::ZERO);
        assert!(again[0].1.warning.is_none());
        assert_eq!(again.iter().filter(|(_, plan)| plan.warning.is_some()).count(), 1);
    }

    #[test]
    fn an_operator_selection_restarts_the_quick_attempts() {
        let tracker = Tracker::default();
        let start = Instant::now();
        let selected = Kg50Input::P6Differential;
        let sends = refused_sends(&tracker, start, Duration::from_secs(100));
        let last = sends.last().unwrap().0;
        tracker.note_selected_at(start + last + Duration::from_secs(1));
        let after = start + last + Duration::from_secs(1) + REPUBLISH_INTERVAL;
        tracker.observe_at(Kg50Input::Amp2, after);
        let plan = tracker.resend_plan(selected, after).unwrap();
        assert!(plan.warning.is_none());
    }

    #[test]
    fn a_silent_board_restarts_the_quick_attempts_without_a_second_warning() {
        let tracker = Tracker::default();
        let start = Instant::now();
        let selected = Kg50Input::P6Differential;
        let sends = refused_sends(&tracker, start, Duration::from_secs(100));
        assert!(sends.iter().any(|(_, plan)| plan.warning.is_some()));
        // The DAQ rebooted: no report for a while, then AMP2 again.
        let back = start + Duration::from_secs(110);
        assert_eq!(tracker.status_at(selected, back).state, SyncState::NoReadback);
        assert_eq!(tracker.resend_plan(selected, back), None);
        let again = refused_sends(&tracker, back, Duration::from_secs(30));
        assert_eq!(again[0].0, Duration::ZERO);
        assert!(again[0].1.warning.is_none());
        assert!(again.iter().all(|(_, plan)| plan.warning.is_none()));
    }
}
