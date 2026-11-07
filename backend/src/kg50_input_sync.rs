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

#[derive(Default)]
struct Inner {
    board: Option<(Kg50Input, Instant)>,
    disagreeing_since: Option<Instant>,
    last_publish: Option<Instant>,
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
    pub fn note_published(&self) {
        self.note_published_at(Instant::now());
    }
    fn note_published_at(&self, now: Instant) {
        self.0.lock().unwrap().last_publish = Some(now);
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
    /// True when the board currently disagrees and no re-send is outstanding.
    fn republish_due(&self, selected: Kg50Input, now: Instant) -> bool {
        let status = self.status_at(selected, now);
        matches!(status.state, SyncState::Syncing | SyncState::Mismatch)
            && self.0.lock().unwrap().last_publish.is_none_or(|last| {
                now.saturating_duration_since(last) >= REPUBLISH_INTERVAL
            })
    }
}

/// Re-sends the selection while the board reports another input. This covers a
/// bootstrap publication that left before the DAQ was discovered, and a DAQ that
/// restored a value this station has since replaced.
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
        let calibration = state.loadcell_calibration.lock().unwrap().clone();
        if !state
            .kg50_input_sync
            .republish_due(calibration.kg50_input, Instant::now())
        {
            continue;
        }
        let Some(generation) = crate::gse::capture_operator_origin(&state) else {
            continue;
        };
        let Some(_claim) = crate::gse::claim_operator_dispatch(&state, generation) else {
            continue;
        };
        state.kg50_input_sync.note_published();
        match crate::network_variables::set_daq_kg50_input(router, &calibration) {
            Ok(()) => log::info!(
                "DAQ reports a different 50 kg input; re-sent selection {:?}",
                calibration.kg50_input
            ),
            Err(error) => log::warn!("re-sending DAQ 50 kg input failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_until_the_board_reports() {
        let tracker = Tracker::default();
        let status = tracker.status(Kg50Input::P6Differential);
        assert_eq!(status.state, SyncState::NoReadback);
        assert_eq!(status.board_input, None);
        assert_eq!(status.board_age_ms, None);
        assert!(!tracker.republish_due(Kg50Input::P6Differential, Instant::now()));
    }

    #[test]
    fn matching_report_is_synced_and_never_resent() {
        let tracker = Tracker::default();
        let now = Instant::now();
        tracker.observe_at(Kg50Input::Amp2, now);
        assert_eq!(tracker.status_at(Kg50Input::Amp2, now).state, SyncState::Synced);
        assert!(!tracker.republish_due(Kg50Input::Amp2, now));
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
        assert!(!tracker.republish_due(Kg50Input::P6Differential, later));
    }

    #[test]
    fn disagreement_is_resent_at_a_bounded_rate_until_it_resolves() {
        let tracker = Tracker::default();
        let start = Instant::now();
        tracker.observe_at(Kg50Input::Amp2, start);
        let selected = Kg50Input::P6Differential;
        assert_eq!(tracker.status_at(selected, start).state, SyncState::Syncing);
        assert!(tracker.republish_due(selected, start));
        tracker.note_published_at(start);
        assert!(!tracker.republish_due(selected, start + Duration::from_secs(1)));
        tracker.observe_at(Kg50Input::Amp2, start + REPUBLISH_INTERVAL);
        assert!(tracker.republish_due(selected, start + REPUBLISH_INTERVAL));
        tracker.observe_at(selected, start + REPUBLISH_INTERVAL + Duration::from_millis(1));
        let resolved = start + REPUBLISH_INTERVAL + Duration::from_secs(1);
        assert_eq!(tracker.status_at(selected, resolved).state, SyncState::Synced);
        assert!(!tracker.republish_due(selected, resolved));
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
}
