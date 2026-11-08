//! Valve-state report freshness for each output board, kept apart from board
//! presence. A board can keep sending other telemetry while its valve-state
//! reports stop arriving or are refused. This only reports that condition. It
//! never changes valve evidence, command admission, fill policy or aborts.
use crate::types::Board;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Valve evidence expires after two seconds; allow one missed 1 Hz report.
pub const STALE_AFTER_MS: u64 = 3_000;
/// The board counts as present if any packet arrived this recently.
pub const PRESENT_WITHIN_MS: u64 = 3_000;
const OUTPUT_BOARDS: [Board; 2] = [Board::ValveBoard, Board::ActuatorBoard];
const UTC_2020_MS: i64 = 1_577_836_800_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Refusal {
    /// The report's source time was outside the station's acceptance window
    /// (more than 100 ms ahead of station network time, or over 2 s behind).
    ClockSkew { source_minus_station_ms: i64 },
    /// The station could not attribute the report's sender to the board that
    /// owns the valve (unresolved or mismatched network address).
    Unowned,
    /// Replay, ordering or freshness refusal with an in-window source clock.
    Refused,
}

#[derive(Clone, Debug, Default)]
struct Record {
    last_received_ms: Option<u64>,
    /// Last admitted report per valve key seen from this board.
    admitted: Vec<(u8, u64)>,
    last_refusal: Option<(u64, Refusal)>,
    refused: u64,
    stale_since_ms: Option<u64>,
    warned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BoardValveReports {
    pub sender_id: &'static str,
    pub board_present: bool,
    /// True when the board is present but at least one of its valves has had
    /// no admitted report for longer than [`STALE_AFTER_MS`] (or none ever
    /// has). Distinct from the board being absent.
    pub stale: bool,
    pub stale_for_ms: Option<u64>,
    /// Age of the stalest valve key's last admitted report.
    pub last_admitted_age_ms: Option<u64>,
    /// Valve keys (command ids) whose last admitted report is too old.
    pub stale_keys: Vec<u8>,
    pub last_received_age_ms: Option<u64>,
    pub refused_reports: u64,
    pub last_refusal: Option<Refusal>,
    pub last_refusal_age_ms: Option<u64>,
}

/// One edge to show the operator, produced at most once per stale episode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edge {
    Stale(String),
    Resumed(String),
}

#[derive(Default)]
pub struct Tracker {
    boards: HashMap<Board, Record>,
}

impl Tracker {
    pub fn observe(
        &mut self,
        board: Board,
        key: u8,
        now_ms: u64,
        admitted: bool,
        refusal: Refusal,
    ) {
        let record = self.boards.entry(board).or_default();
        record.last_received_ms = Some(now_ms);
        if admitted {
            if let Some(entry) = record.admitted.iter_mut().find(|(k, _)| *k == key) {
                entry.1 = now_ms;
            } else if record.admitted.len() < 16 {
                record.admitted.push((key, now_ms));
            }
        } else {
            record.refused = record.refused.saturating_add(1);
            record.last_refusal = Some((now_ms, refusal));
        }
    }

    pub fn snapshot(
        &self,
        board: Board,
        now_ms: u64,
        board_age_ms: Option<u64>,
    ) -> BoardValveReports {
        let record = self.boards.get(&board).cloned().unwrap_or_default();
        let age = |at: Option<u64>| at.map(|at| now_ms.saturating_sub(at));
        let board_present = board_age_ms.is_some_and(|age| age <= PRESENT_WITHIN_MS);
        let admitted_age = record
            .admitted
            .iter()
            .map(|(_, at)| now_ms.saturating_sub(*at))
            .max();
        let stale_keys: Vec<u8> = record
            .admitted
            .iter()
            .filter(|(_, at)| now_ms.saturating_sub(*at) > STALE_AFTER_MS)
            .map(|(key, _)| *key)
            .collect();
        let stale = board_present && admitted_age.is_none_or(|age| age > STALE_AFTER_MS);
        BoardValveReports {
            sender_id: board.sender_id(),
            board_present,
            stale,
            stale_for_ms: if stale {
                age(record.stale_since_ms)
            } else {
                None
            },
            last_admitted_age_ms: admitted_age,
            stale_keys,
            last_received_age_ms: age(record.last_received_ms),
            refused_reports: record.refused,
            last_refusal: record.last_refusal.map(|(_, refusal)| refusal),
            last_refusal_age_ms: age(record.last_refusal.map(|(at, _)| at)),
        }
    }

    /// Advance stale episodes. `board_age_ms` is the age of the board's last
    /// packet of any kind. A board that has never had an admitted valve report
    /// only becomes stale after it has been present for [`STALE_AFTER_MS`].
    pub fn tick(&mut self, board: Board, now_ms: u64, board_age_ms: Option<u64>) -> Option<Edge> {
        let snapshot = self.snapshot(board, now_ms, board_age_ms);
        let record = self.boards.entry(board).or_default();
        if !snapshot.stale {
            let was = record.stale_since_ms.take();
            let warned = core::mem::take(&mut record.warned);
            return match (
                was,
                warned,
                snapshot.board_present,
                snapshot.last_admitted_age_ms,
            ) {
                (Some(since), true, true, Some(_)) => Some(Edge::Resumed(format!(
                    "{} valve-state reports resumed after {} s stale",
                    board.sender_id(),
                    now_ms.saturating_sub(since) / 1000
                ))),
                _ => None,
            };
        }
        let since = *record.stale_since_ms.get_or_insert(now_ms);
        // First presence after start-up: wait one window before calling it stale.
        if record.warned
            || (record.admitted.is_empty() && now_ms.saturating_sub(since) < STALE_AFTER_MS)
        {
            return None;
        }
        record.warned = true;
        Some(Edge::Stale(format!(
            "{} valve state STALE: board is still reporting but {} for over {} s ({}). Those valve indications are unknown until reports resume.",
            board.sender_id(),
            if snapshot.stale_keys.is_empty() {
                "no valve-state report has been accepted".to_string()
            } else {
                format!(
                    "valve keys {:?} have had no accepted report",
                    snapshot.stale_keys
                )
            },
            STALE_AFTER_MS / 1000,
            reason_text(&snapshot)
        )))
    }
}

pub fn reason_text(snapshot: &BoardValveReports) -> String {
    let recent_refusal = snapshot
        .last_refusal_age_ms
        .is_some_and(|age| age <= STALE_AFTER_MS);
    match snapshot.last_refusal {
        Some(Refusal::ClockSkew {
            source_minus_station_ms,
        }) if recent_refusal => format!(
            "reports are arriving but refused: board clock {} station network time by {} ms",
            if source_minus_station_ms > 0 {
                "ahead of"
            } else {
                "behind"
            },
            source_minus_station_ms.unsigned_abs()
        ),
        Some(Refusal::Unowned) if recent_refusal => {
            "reports are arriving but the sender address is not attributed to this board".into()
        }
        Some(Refusal::Refused) if recent_refusal => {
            "reports are arriving but refused as repeats, out of order or late".into()
        }
        _ if snapshot
            .last_received_age_ms
            .is_some_and(|age| age <= STALE_AFTER_MS) =>
        {
            if snapshot.stale_keys.is_empty() {
                "reports are arriving but none were admitted".into()
            } else {
                "other valve reports from this board are arriving; the missing ones are lost in transit or held back by the board's ordered delivery, for example when its end-to-end acknowledgements are not reaching it".into()
            }
        }
        _ => "no valve-state reports are arriving from this board".into(),
    }
}

/// Classify a refused report from its source and station network clocks.
pub fn classify_refusal(source_ms: i64, station_network_ms: i64) -> Refusal {
    if source_ms >= UTC_2020_MS && station_network_ms >= UTC_2020_MS {
        let skew = source_ms.saturating_sub(station_network_ms);
        if !(-2_000..=100).contains(&skew) {
            return Refusal::ClockSkew {
                source_minus_station_ms: skew,
            };
        }
    }
    Refusal::Refused
}

fn tracker() -> &'static Mutex<Tracker> {
    static TRACKER: OnceLock<Mutex<Tracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(Tracker::default()))
}

fn clock_ms() -> u64 {
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    crate::simulation_clock_ms().unwrap_or_else(|| {
        u64::try_from(
            ORIGIN
                .get_or_init(std::time::Instant::now)
                .elapsed()
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    })
}

pub fn observe(board: Board, key: u8, admitted: bool, refusal: Refusal) {
    tracker()
        .lock()
        .unwrap()
        .observe(board, key, clock_ms(), admitted, refusal);
}

fn board_age_ms(state: &crate::state::AppState, board: Board, now_ms: u64) -> Option<u64> {
    let map = state.board_status.lock().unwrap();
    let status = map.get(&board)?;
    if crate::simulation_clock_ms().is_some() {
        return status
            .last_seen_model_ms
            .map(|at| now_ms.saturating_sub(at));
    }
    status
        .last_seen_instant
        .map(|at| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX))
}

pub fn status(state: &crate::state::AppState) -> Vec<BoardValveReports> {
    let now = clock_ms();
    let tracker = tracker().lock().unwrap();
    OUTPUT_BOARDS
        .iter()
        .map(|board| tracker.snapshot(*board, now, board_age_ms(state, *board, now)))
        .collect()
}

pub async fn service(
    state: Arc<crate::state::AppState>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        let now = clock_ms();
        for board in OUTPUT_BOARDS {
            let age = board_age_ms(&state, board, now);
            let edge = tracker().lock().unwrap().tick(board, now, age);
            match edge {
                Some(Edge::Stale(message)) => {
                    log::warn!("{message}");
                    let _ = state.add_backend_message(message);
                }
                Some(Edge::Resumed(message)) => {
                    log::info!("{message}");
                    let _ = state.add_backend_message(message);
                }
                None => {}
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            _ = shutdown_rx.recv() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AB: Board = Board::ActuatorBoard;

    fn stale_edges(
        tracker: &mut Tracker,
        from: u64,
        to: u64,
        board_age: impl Fn(u64) -> Option<u64>,
    ) -> Vec<(u64, Edge)> {
        (from..to)
            .step_by(500)
            .filter_map(|t| tracker.tick(AB, t, board_age(t)).map(|edge| (t, edge)))
            .collect()
    }

    #[test]
    fn healthy_reports_never_stale() {
        let mut t = Tracker::default();
        for now in (0..120_000).step_by(500) {
            if now % 1000 == 0 {
                t.observe(AB, 8, now, true, Refusal::Refused);
            }
            assert_eq!(t.tick(AB, now, Some(0)), None);
            assert!(!t.snapshot(AB, now, Some(0)).stale);
        }
    }

    #[test]
    fn absent_board_is_not_stale_valve_state() {
        let mut t = Tracker::default();
        t.observe(AB, 8, 0, true, Refusal::Refused);
        // Board silent entirely: that is board absence, reported elsewhere.
        let edges = stale_edges(&mut t, 500, 60_000, |now| Some(now));
        assert!(edges.is_empty(), "{edges:?}");
        assert!(!t.snapshot(AB, 60_000, Some(60_000)).stale);
    }

    #[test]
    fn present_board_without_valve_reports_warns_once_then_resumes() {
        let mut t = Tracker::default();
        t.observe(AB, 8, 0, true, Refusal::Refused);
        let edges = stale_edges(&mut t, 500, 30_000, |_| Some(100));
        assert_eq!(edges.len(), 1, "{edges:?}");
        let (at, Edge::Stale(message)) = &edges[0] else {
            panic!("{edges:?}")
        };
        assert_eq!(*at, 3_500);
        assert!(message.starts_with("AB valve state STALE"), "{message}");
        assert!(
            message.contains("no valve-state reports are arriving"),
            "{message}"
        );
        let snapshot = t.snapshot(AB, 30_000, Some(100));
        assert!(snapshot.stale && snapshot.board_present);
        assert_eq!(snapshot.stale_for_ms, Some(26_500));
        t.observe(AB, 8, 30_000, true, Refusal::Refused);
        let Some(Edge::Resumed(message)) = t.tick(AB, 30_000, Some(0)) else {
            panic!()
        };
        assert_eq!(message, "AB valve-state reports resumed after 26 s stale");
        assert_eq!(t.tick(AB, 30_500, Some(0)), None);
    }

    #[test]
    fn refused_reports_name_the_clock_skew() {
        let mut t = Tracker::default();
        t.observe(AB, 8, 0, true, Refusal::Refused);
        let base = 1_800_000_000_000_i64;
        for now in (1_000..10_000).step_by(1000) {
            t.observe(AB, 8, now, false, classify_refusal(base - 4_200, base));
        }
        let edges = stale_edges(&mut t, 500, 10_000, |_| Some(0));
        let [(_, Edge::Stale(message))] = edges.as_slice() else {
            panic!("{edges:?}")
        };
        assert!(
            message.contains("board clock behind station network time by 4200 ms"),
            "{message}"
        );
        assert_eq!(
            classify_refusal(base + 150, base),
            Refusal::ClockSkew {
                source_minus_station_ms: 150
            }
        );
        assert_eq!(classify_refusal(base - 1_500, base), Refusal::Refused);
        assert_eq!(classify_refusal(12_345, base), Refusal::Refused);
    }

    #[test]
    fn first_presence_waits_one_window() {
        let mut t = Tracker::default();
        let edges = stale_edges(&mut t, 0, 20_000, |_| Some(0));
        let [(at, Edge::Stale(message))] = edges.as_slice() else {
            panic!("{edges:?}")
        };
        assert_eq!(*at, 3_000);
        assert!(message.contains("no valve-state reports"), "{message}");
    }

    #[test]
    fn one_valve_going_quiet_is_stale_even_if_another_still_reports() {
        // Ordered delivery with lost acknowledgements lets only some keys through.
        let mut t = Tracker::default();
        for now in (0..20_000).step_by(1000) {
            t.observe(AB, 8, now, true, Refusal::Refused);
            if now < 5_000 {
                t.observe(AB, 10, now, true, Refusal::Refused);
            }
        }
        let snapshot = t.snapshot(AB, 19_500, Some(0));
        assert!(snapshot.stale);
        assert_eq!(snapshot.stale_keys, vec![10]);
        let edges = stale_edges(&mut t, 19_500, 20_000, |_| Some(0));
        let [(_, Edge::Stale(message))] = edges.as_slice() else {
            panic!("{edges:?}")
        };
        assert!(message.contains("valve keys [10]"), "{message}");
        assert!(
            message.contains("held back by the board's ordered delivery"),
            "{message}"
        );
    }
}
