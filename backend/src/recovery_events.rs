//! Flight Computer drogue and main deployment notifications.
//!
//! The FC reports a deployment in two ways today, both on the network:
//! - an `ORDERED_MESSAGE` text sent from `release_parachute`/`expand_parachute`
//!   right before the pulse: "PD approx altitude: <m>" (drogue) and
//!   "PR approx altitude: <m>" (main/reef);
//! - the next `FLIGHT_STATE`: Apogee->Descent (drogue) and Descent->Reefing
//!   (main).
//!
//! Whichever arrives first raises one notification per deployment and launch
//! cycle; the other is folded into it. The message carries the FC's own
//! altitude; a state-only report uses the latest received altitude telemetry.
use crate::{state::AppState, types::FlightState};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deployment {
    Drogue,
    Main,
}

impl Deployment {
    fn index(self) -> usize {
        match self {
            Deployment::Drogue => 0,
            Deployment::Main => 1,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Deployment::Drogue => "Drogue",
            Deployment::Main => "Main",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Evidence {
    /// FC deployment message with its reported altitude.
    Message { altitude_m: f64 },
    /// FC flight-state change only.
    State { altitude_m: Option<f64> },
}

/// "PD approx altitude: 812" -> (Drogue, 812). The FC prefixes no sender; the
/// GS adds "FC: " before storing, so both forms are accepted.
pub fn parse_fc_message(text: &str) -> Option<(Deployment, f64)> {
    let text = text.trim();
    let text = text.strip_prefix("FC:").map(str::trim).unwrap_or(text);
    let (kind, rest) = if let Some(rest) = text.strip_prefix("PD approx altitude") {
        (Deployment::Drogue, rest)
    } else {
        (Deployment::Main, text.strip_prefix("PR approx altitude")?)
    };
    let value: f64 = rest.trim_start_matches([':', ' ']).trim().parse().ok()?;
    value.is_finite().then_some((kind, value))
}

/// Deployments implied by an accepted FC flight-state change. The FC fires
/// the drogue right before entering Descent and the main right before
/// Reefing, so any in-flight change that crosses one of those states implies
/// that deployment. Crossing (not only the adjacent edge) matters because the
/// GS can miss intermediate reports in flight: Apogee -> Reefing still means
/// both parachutes deployed.
pub fn deployments_for_transition(previous: FlightState, next: FlightState) -> Vec<Deployment> {
    let (prev, next) = (previous as u8, next as u8);
    let in_flight = |s: u8| (FlightState::Launch as u8..=FlightState::Recovery as u8).contains(&s);
    if !in_flight(prev) || !in_flight(next) || next <= prev {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (kind, edge) in [
        (Deployment::Drogue, FlightState::Descent as u8),
        (Deployment::Main, FlightState::Reefing as u8),
    ] {
        if prev < edge && next >= edge {
            out.push(kind);
        }
    }
    out
}

/// One notification per deployment per launch cycle, per station state (a
/// process has one; unit fixtures have many). A prelaunch state re-arms both.
static LEDGER: Mutex<Vec<(usize, [bool; 2])>> = Mutex::new(Vec::new());

fn station_key(state: &AppState) -> usize {
    std::ptr::from_ref(state) as usize
}

pub fn rearm_for_new_cycle(state: &AppState) {
    let key = station_key(state);
    LEDGER.lock().unwrap().retain(|(k, _)| *k != key);
}

fn claim(state: &AppState, kind: Deployment) -> bool {
    let key = station_key(state);
    let mut ledger = LEDGER.lock().unwrap();
    let index = match ledger.iter().position(|(k, _)| *k == key) {
        Some(index) => index,
        None => {
            ledger.push((key, [false; 2]));
            ledger.len() - 1
        }
    };
    !std::mem::replace(&mut ledger[index].1[kind.index()], true)
}

fn format_utc(ms: i64) -> String {
    let Ok(at) = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000) else {
        return "unknown time".into();
    };
    format!("{:02}:{:02}:{:02} UTC", at.hour(), at.minute(), at.second())
}

pub fn notification_text(
    kind: Deployment,
    evidence: Evidence,
    event_ms: i64,
    t_plus_ms: Option<i64>,
) -> String {
    let altitude = match evidence {
        Evidence::Message { altitude_m } => format!("{altitude_m:.0} m (FC)"),
        Evidence::State {
            altitude_m: Some(altitude_m),
        } => format!("{altitude_m:.0} m (latest telemetry)"),
        Evidence::State { altitude_m: None } => "unknown altitude".into(),
    };
    let t_plus = t_plus_ms
        .filter(|ms| *ms >= 0)
        .map(|ms| format!(", T+{:.1} s", ms as f64 / 1000.0))
        .unwrap_or_default();
    format!(
        "{} parachute deployed at {altitude}, {}{t_plus}",
        kind.label(),
        format_utc(event_ms)
    )
}

fn t_plus_ms(state: &AppState, event_ms: i64) -> Option<i64> {
    let clock = state.launch_clock_snapshot();
    (clock.kind == crate::telemetry_db::LaunchClockKind::TPlus)
        .then_some(clock.anchor_timestamp_ms)
        .flatten()
        .map(|anchor| event_ms - anchor)
}

/// Raise the operator notification (live popup and notification history) and
/// persist it in the message log. Returns the text when a notification was
/// raised.
pub fn notify(
    state: &AppState,
    kind: Deployment,
    evidence: Evidence,
    event_ms: i64,
) -> Option<String> {
    if !claim(state, kind) {
        return None;
    }
    let evidence = match evidence {
        Evidence::State { altitude_m: None } => Evidence::State {
            altitude_m: crate::flight_maxima::latest_altitude_m(),
        },
        other => other,
    };
    let text = notification_text(kind, evidence, event_ms, t_plus_ms(state, event_ms));
    log::info!("{text}");
    state.add_notification_with_persistence(text.clone(), true);
    crate::web::persist_message_db_only(state, text.clone());
    Some(text)
}

/// FC text message ingress.
pub fn observe_fc_message(state: &AppState, text: &str, event_ms: i64) -> Option<String> {
    let (kind, altitude_m) = parse_fc_message(text)?;
    notify(state, kind, Evidence::Message { altitude_m }, event_ms)
}

/// Accepted FC flight-state change.
pub fn observe_fc_state(
    state: &AppState,
    previous: FlightState,
    next: FlightState,
    event_ms: i64,
) -> Option<String> {
    if (next as u8) <= FlightState::Armed as u8 {
        rearm_for_new_cycle(state);
        return None;
    }
    deployments_for_transition(previous, next)
        .into_iter()
        .filter_map(|kind| notify(state, kind, Evidence::State { altitude_m: None }, event_ms))
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_fc_deployment_messages() {
        assert_eq!(
            parse_fc_message("PD approx altitude: 812"),
            Some((Deployment::Drogue, 812.0))
        );
        assert_eq!(
            parse_fc_message("FC: PR approx altitude: 304"),
            Some((Deployment::Main, 304.0))
        );
        assert_eq!(parse_fc_message("PD drogue blocked, state: 3"), None);
        assert_eq!(parse_fc_message("PR reef blocked: not drogue"), None);
        assert_eq!(parse_fc_message("EV new flight state: 7"), None);
    }

    #[test]
    fn maps_the_fc_state_edges() {
        use FlightState as S;
        let mut hits = Vec::new();
        for a in 0..16 {
            for b in 0..16 {
                let (a, b) = (
                    crate::types::u8_to_flight_state(a).unwrap(),
                    crate::types::u8_to_flight_state(b).unwrap(),
                );
                for kind in deployments_for_transition(a, b) {
                    hits.push((a, b, kind));
                }
            }
        }
        // Every adjacent edge the FC takes after firing is still mapped.
        for edge in [
            (S::Ascent, S::Descent, Deployment::Drogue),
            (S::Coast, S::Descent, Deployment::Drogue),
            (S::Apogee, S::Descent, Deployment::Drogue),
            (S::Descent, S::Reefing, Deployment::Main),
        ] {
            assert!(hits.contains(&edge), "{edge:?}");
        }
        // Missed intermediate reports still imply what was crossed.
        assert_eq!(deployments_for_transition(S::Apogee, S::Reefing), vec![Deployment::Drogue, Deployment::Main]);
        assert_eq!(deployments_for_transition(S::Launch, S::Landed), vec![Deployment::Drogue, Deployment::Main]);
        assert_eq!(deployments_for_transition(S::Descent, S::Landed), vec![Deployment::Main]);
        // Never from prelaunch, Aborted, backwards or within one stage.
        assert!(deployments_for_transition(S::Armed, S::Reefing).is_empty());
        assert!(deployments_for_transition(S::Aborted, S::Landed).is_empty());
        assert!(deployments_for_transition(S::Reefing, S::Descent).is_empty());
        assert!(deployments_for_transition(S::Ascent, S::Apogee).is_empty());
        assert!(deployments_for_transition(S::Landed, S::Recovery).is_empty());
    }

    #[tokio::test]
    async fn a_missed_descent_report_still_raises_both_notifications_once() {
        let state = crate::state::tests::test_app_state().await;
        let text = observe_fc_state(&state, FlightState::Apogee, FlightState::Reefing, 5_000).unwrap();
        assert!(text.starts_with("Main parachute deployed at"));
        let history = state.notifications_snapshot();
        assert!(history.iter().any(|n| n.message.starts_with("Drogue parachute deployed at") && n.persistent));
        assert!(observe_fc_state(&state, FlightState::Reefing, FlightState::Landed, 6_000).is_none());
        rearm_for_new_cycle(&state);
    }

    #[test]
    fn text_has_altitude_and_time() {
        let text = notification_text(
            Deployment::Drogue,
            Evidence::Message { altitude_m: 812.4 },
            86_399_000,
            Some(31_250),
        );
        assert_eq!(
            text,
            "Drogue parachute deployed at 812 m (FC), 23:59:59 UTC, T+31.2 s"
        );
        let text = notification_text(
            Deployment::Main,
            Evidence::State { altitude_m: None },
            0,
            None,
        );
        assert_eq!(
            text,
            "Main parachute deployed at unknown altitude, 00:00:00 UTC"
        );
    }

    #[tokio::test]
    async fn one_notification_per_deployment_per_cycle() {
        let state = crate::state::tests::test_app_state().await;
        let first = observe_fc_message(&state, "FC: PD approx altitude: 900", 1_000).unwrap();
        assert!(first.starts_with("Drogue parachute deployed at 900 m (FC)"));
        assert!(
            observe_fc_state(&state, FlightState::Apogee, FlightState::Descent, 1_100).is_none()
        );
        let main =
            observe_fc_state(&state, FlightState::Descent, FlightState::Reefing, 2_000).unwrap();
        assert!(main.starts_with("Main parachute deployed at"));
        assert!(observe_fc_message(&state, "PR approx altitude: 300", 2_050).is_none());
        let history = state.notifications_snapshot();
        assert!(history.iter().any(|n| n.message == first && n.persistent));
        assert!(history.iter().any(|n| n.message == main && n.persistent));
        // A new prelaunch cycle re-arms both notifications.
        assert!(observe_fc_state(&state, FlightState::Landed, FlightState::Idle, 3_000).is_none());
        assert!(observe_fc_message(&state, "PD approx altitude: 10", 4_000).is_some());
        rearm_for_new_cycle(&state);
    }
}
