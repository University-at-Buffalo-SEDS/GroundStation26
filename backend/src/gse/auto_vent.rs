//! Valve Board post-Launch auto-vent (ValveBoard26 `vent_countdown.h`).
//!
//! After an accepted Launch the Valve Board holds Dump and Vent closed. If its
//! GS heartbeat is lost for 5 s or the launch continuity wire breaks it counts
//! down 150 s and then vents on its own (never before Launch + 60 s). It
//! reports as a WARNING string:
//!
//! - `Valve Board auto-vent in 145 s: link lost [L145]` (K continuity broken,
//!   B both; the value is the remaining seconds),
//! - `Valve Board auto-vented: Vent and Dump open, Pilot closed [V1]` (the
//!   value is the trigger mask at the vent: 1 link, 2 continuity),
//! - `Valve Board auto-vent cancelled [X0]`.
//!
//! The board re-announces every 5 s while counting or vented. The station
//! keeps the last report (the link may well be gone: that is the point),
//! counts the remaining time down locally and shows it in the GSE status.
//! Only changes reach the warning log.

use super::Runtime;
use crate::state::AppState;
use std::sync::Arc;

const PREFIX: &str = "Valve Board auto-vent";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    Counting { link: bool, continuity: bool, remaining_s: u32 },
    Vented { link: bool, continuity: bool },
    Cancelled,
}

impl Report {
    fn triggers(self) -> (bool, bool) {
        match self {
            Report::Counting { link, continuity, .. } | Report::Vented { link, continuity } => (link, continuity),
            Report::Cancelled => (false, false),
        }
    }
    /// Same state and triggers; the remaining time alone is not a change.
    fn same_kind(self, other: Report) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other) && self.triggers() == other.triggers()
    }
}

pub(crate) fn parse(text: &str) -> Option<Report> {
    let body = text.strip_prefix(PREFIX)?.strip_suffix(']')?;
    let open = body.rfind(" [")?;
    let mut tag = body[open + 2..].chars();
    let code = tag.next()?;
    let value: u32 = tag.as_str().parse().ok()?;
    Some(match code {
        'L' => Report::Counting { link: true, continuity: false, remaining_s: value },
        'K' => Report::Counting { link: false, continuity: true, remaining_s: value },
        'B' => Report::Counting { link: true, continuity: true, remaining_s: value },
        'V' if value <= 3 => Report::Vented { link: value & 1 != 0, continuity: value & 2 != 0 },
        'X' => Report::Cancelled,
        _ => return None,
    })
}

#[derive(Default)]
pub(crate) struct Monitor {
    last: Option<(Report, u64)>,
}

impl Monitor {
    /// Records a report; returns whether it changed the state or triggers.
    fn observe(&mut self, report: Report, now_ms: u64) -> bool {
        let changed = self.last.is_none_or(|(last, _)| !last.same_kind(report));
        // A cancellation with nothing shown is not news (board start-up).
        let news = changed && !(report == Report::Cancelled && self.last.is_none());
        self.last = Some((report, now_ms));
        news
    }
}

/// `None` when `text` is not an auto-vent report (another handler owns it);
/// otherwise the warning to show, if any (changes only).
pub(crate) fn board_warning(state: &Arc<AppState>, text: &str) -> Option<Option<String>> {
    let report = parse(text)?;
    let display = text[..text.rfind(" [").unwrap_or(text.len())].to_string();
    let mut rt = state.gse.lock().unwrap();
    let now = rt.now();
    Some(rt.auto_vent.observe(report, now).then(|| match report {
        Report::Vented { .. } => format!("{display}. The tank vented on the Valve Board's own countdown."),
        _ => display,
    }))
}

/// `auto_vent` in the GSE status: `state` idle, counting or vented; the
/// active triggers; while counting the remaining time counted down locally
/// from the last report, and the age of that report.
pub(super) fn decorate_status(rt: &Runtime, response: &mut serde_json::Value) {
    response["auto_vent"] = status(&rt.auto_vent, rt.now());
}

fn status(monitor: &Monitor, now: u64) -> serde_json::Value {
    let Some((report, at)) = monitor.last else {
        return serde_json::json!({"state": "idle"});
    };
    let age_ms = now.saturating_sub(at);
    let (link, continuity) = report.triggers();
    match report {
        Report::Cancelled => serde_json::json!({"state": "idle"}),
        Report::Counting { remaining_s, .. } => serde_json::json!({
            "state": "counting",
            "link_lost": link,
            "continuity_broken": continuity,
            "remaining_ms": (u64::from(remaining_s) * 1000).saturating_sub(age_ms),
            "report_age_ms": age_ms,
        }),
        Report::Vented { .. } => serde_json::json!({
            "state": "vented",
            "link_lost": link,
            "continuity_broken": continuity,
            "report_age_ms": age_ms,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_board_report() {
        assert_eq!(
            parse("Valve Board auto-vent in 145 s: link lost [L145]"),
            Some(Report::Counting { link: true, continuity: false, remaining_s: 145 })
        );
        assert_eq!(
            parse("Valve Board auto-vent in 144 s: continuity broken [K144]"),
            Some(Report::Counting { link: false, continuity: true, remaining_s: 144 })
        );
        assert_eq!(
            parse("Valve Board auto-vent in 145 s: link lost, continuity broken [B145]"),
            Some(Report::Counting { link: true, continuity: true, remaining_s: 145 })
        );
        assert_eq!(
            parse("Valve Board auto-vented: Vent and Dump open, Pilot closed [V3]"),
            Some(Report::Vented { link: true, continuity: true })
        );
        assert_eq!(
            parse("Valve Board auto-vented: Vent and Dump open, Pilot closed [V0]"),
            Some(Report::Vented { link: false, continuity: false })
        );
        assert_eq!(parse("Valve Board auto-vent cancelled [X0]"), Some(Report::Cancelled));
        for bad in [
            "Valve Board overpressure relief: 912 psi [R3]",
            "Valve Board auto-vent in 145 s: link lost [Q145]",
            "Valve Board auto-vent in 145 s: link lost [L]",
            "Valve Board auto-vent in 145 s: link lost",
            "Valve Board auto-vented [V9]",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn shows_changes_only_and_counts_down_locally() {
        let mut m = Monitor::default();
        assert_eq!(status(&m, 0)["state"], "idle");
        // A quiet board start-up "cancelled" is not news.
        assert!(!m.observe(Report::Cancelled, 0));
        let l = |s| Report::Counting { link: true, continuity: false, remaining_s: s };
        assert!(m.observe(l(145), 1000));
        assert!(!m.observe(l(140), 6000), "a re-announce is not a change");
        let s = status(&m, 9000);
        assert_eq!(s["state"], "counting");
        assert_eq!(s["link_lost"], true);
        assert_eq!(s["continuity_broken"], false);
        assert_eq!(s["remaining_ms"], 137_000);
        assert_eq!(s["report_age_ms"], 3000);
        // Long after the last report the estimate stops at zero.
        assert_eq!(status(&m, 1_000_000)["remaining_ms"], 0);
        assert!(m.observe(Report::Counting { link: true, continuity: true, remaining_s: 130 }, 16000), "a new trigger is shown");
        assert!(m.observe(Report::Vented { link: true, continuity: true }, 150_000));
        assert!(!m.observe(Report::Vented { link: true, continuity: true }, 155_000));
        let s = status(&m, 156_000);
        assert_eq!(s["state"], "vented");
        assert!(s.get("remaining_ms").is_none());
        assert!(m.observe(Report::Cancelled, 200_000), "cancel after a vent is shown");
        assert_eq!(status(&m, 200_000)["state"], "idle");
    }

    #[tokio::test]
    async fn station_adopts_reports_and_decorates_status() {
        let state = crate::state::tests::test_app_state().await;
        assert_eq!(board_warning(&state, "Valve Board overpressure relief: 912 psi [R3]"), None);
        assert_eq!(
            board_warning(&state, "Valve Board auto-vent in 150 s: link lost [L150]"),
            Some(Some("Valve Board auto-vent in 150 s: link lost".into()))
        );
        assert_eq!(board_warning(&state, "Valve Board auto-vent in 145 s: link lost [L145]"), Some(None));
        let mut response = serde_json::json!({});
        decorate_status(&state.gse.lock().unwrap(), &mut response);
        assert_eq!(response["auto_vent"]["state"], "counting");
        let shown = board_warning(&state, "Valve Board auto-vented: Vent and Dump open, Pilot closed [V1]").unwrap().unwrap();
        assert!(shown.starts_with("Valve Board auto-vented: Vent and Dump open, Pilot closed."), "{shown}");
        let mut response = serde_json::json!({});
        decorate_status(&state.gse.lock().unwrap(), &mut response);
        assert_eq!(response["auto_vent"]["state"], "vented");
        assert_eq!(response["auto_vent"]["link_lost"], true);
    }
}
