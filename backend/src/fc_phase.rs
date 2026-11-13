//! The Flight Computer's detailed phase (Ascent, Coast, Apogee, Descent,
//! Reefing, Landed, Recovery...).
//!
//! Under the state-split contract the network FLIGHT_STATE carries only
//! InFlight (16) for Ascent..Reefing and Landed (13) for Landed and Recovery.
//! The detailed phase reaches the GS only through the FC's own state report:
//! its BOARD_STATE_STATUS readback (byte 2), or a retired detailed FLIGHT_STATE
//! from old FC firmware. The GS shows "In flight" with this phase beside it,
//! and the deployment notifications follow its edges.
use crate::types::FlightState;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The FC's BOARD_STATE_STATUS readback.
    FcStateReport,
    /// The FC's own FLIGHT_STATE report.
    FcFlightState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub phase: FlightState,
    pub source: Source,
    /// Wall-clock receipt time (ms).
    pub received_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FcPhaseDto {
    pub value: u8,
    pub label: String,
    pub source: Source,
    pub age_ms: u64,
}

#[derive(Default)]
pub struct Tracker(Mutex<Option<Observation>>);

impl Tracker {
    pub fn snapshot(&self) -> Option<Observation> {
        *self.0.lock().unwrap()
    }
    pub fn dto(&self, now_ms: u64) -> Option<FcPhaseDto> {
        self.snapshot().map(|seen| FcPhaseDto {
            value: seen.phase as u8,
            label: crate::mission_phase::detailed_label(seen.phase).to_string(),
            source: seen.source,
            age_ms: now_ms.saturating_sub(seen.received_ms),
        })
    }
    /// Records one FC report. Returns the previous phase when it changed.
    fn record(&self, next: FlightState, source: Source, now_ms: u64) -> Option<FlightState> {
        let mut slot = self.0.lock().unwrap();
        let previous = slot.map(|seen| seen.phase);
        // A bare InFlight (the network value) never hides a detailed in-flight
        // phase the FC already reported: it says nothing newer than it.
        let phase = match (previous, next) {
            (Some(prev), FlightState::InFlight) if prev.is_in_flight() => prev,
            _ => next,
        };
        *slot = Some(Observation { phase, source, received_ms: now_ms });
        (previous != Some(phase)).then_some(previous).flatten()
    }
}

/// States the FC itself can be in (its global-state mapping), detailed or not.
fn fc_state(value: u8) -> Option<FlightState> {
    use FlightState as F;
    crate::types::u8_to_flight_state(value).filter(|state| {
        !matches!(state, F::Idle | F::PreFill | F::FillTest | F::NitrogenFill | F::NitrousFill)
    })
}

/// One FC state report. Deployment notifications follow the detailed edges
/// (Apogee->Descent drogue, Descent->Reefing main) the FC reports here.
pub fn observe(state: &crate::state::AppState, value: u8, source: Source, event_ms: i64) {
    let Some(next) = fc_state(value) else {
        return;
    };
    let now = crate::telemetry_task::get_current_timestamp_ms();
    let before = state.fc_phase.snapshot().map(|seen| seen.phase);
    let previous = state.fc_phase.record(next, source, now);
    if let (Some(previous), Some(current)) = (previous, state.fc_phase.snapshot()) {
        crate::recovery_events::observe_fc_state(state, previous, current.phase, event_ms);
    }
    if before != state.fc_phase.snapshot().map(|seen| seen.phase) {
        let _ = state.state_tx.send(crate::web::FlightStateMsg { state: state.local_flight_state_snapshot() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FlightState as F;

    #[test]
    fn a_bare_in_flight_keeps_the_detailed_phase_and_labels_follow_it() {
        let tracker = Tracker::default();
        assert_eq!(tracker.record(F::Launch, Source::FcFlightState, 1), None);
        assert_eq!(tracker.record(F::InFlight, Source::FcFlightState, 2), Some(F::Launch));
        assert_eq!(tracker.snapshot().unwrap().phase, F::InFlight);
        assert_eq!(tracker.record(F::Descent, Source::FcStateReport, 3), Some(F::InFlight));
        assert_eq!(tracker.record(F::InFlight, Source::FcFlightState, 4), None);
        let dto = tracker.dto(10).unwrap();
        assert_eq!((dto.value, dto.label.as_str(), dto.age_ms), (11, "Descent", 6));
        assert_eq!(tracker.record(F::Landed, Source::FcFlightState, 5), Some(F::Descent));
    }

    #[test]
    fn station_only_fill_states_are_never_an_fc_phase() {
        for value in [1u8, 2, 3, 4, 5, 17] {
            assert_eq!(fc_state(value), None, "{value}");
        }
        for value in [0u8, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16] {
            assert!(fc_state(value).is_some(), "{value}");
        }
    }

    #[tokio::test]
    async fn fc_reports_raise_the_deployment_notifications_from_detailed_edges() {
        let state = crate::state::tests::test_app_state().await;
        observe(&state, F::Launch as u8, Source::FcFlightState, 1_000);
        observe(&state, F::InFlight as u8, Source::FcFlightState, 2_000);
        observe(&state, F::Apogee as u8, Source::FcStateReport, 3_000);
        assert!(!crate::recovery_events::already_deployed(&state, crate::recovery_events::Deployment::Drogue));
        observe(&state, F::Descent as u8, Source::FcStateReport, 4_000);
        assert!(crate::recovery_events::already_deployed(&state, crate::recovery_events::Deployment::Drogue));
        observe(&state, F::InFlight as u8, Source::FcFlightState, 5_000);
        assert!(!crate::recovery_events::already_deployed(&state, crate::recovery_events::Deployment::Main));
        observe(&state, F::Reefing as u8, Source::FcStateReport, 6_000);
        assert!(crate::recovery_events::already_deployed(&state, crate::recovery_events::Deployment::Main));
    }
}
