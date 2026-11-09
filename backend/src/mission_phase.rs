//! Operator-facing mission phase: Startup, Idle, Fill, Launch, In flight,
//! Landed.
//!
//! The phase is a ground-station presentation derived from the detailed
//! `FlightState` byte shared by every board plus a little GS context. It never
//! changes the wire value, firmware enums or any safety rule that is keyed on
//! the detailed state. `mission_phase` is the single mapping function; it is
//! pure and tested exhaustively below.
//!
//! Mode rules:
//! - Flight builds: Startup lasts until every board required for the build has
//!   been seen; Fill also covers GSE fill work that runs while the detailed
//!   state is still Idle (nitrogen test, fill, turbocharge, cold flow holds).
//! - Test-fire builds: the required boards already exclude the AV bay
//!   (FC/RF/PB). There is no flight, so Launch covers the launch admission and
//!   the burn, and any later detailed state still reads as Launch until the
//!   operator reset returns the detailed state to Idle. In flight and Landed
//!   never appear in test-fire builds.
//! - HITL builds: the phase is manual-only. It follows the operator-assigned
//!   detailed state and never uses board presence or GSE activity.
//!
//! Phase and flight state are one concept for the operator. The phase is the
//! state; the detailed `FlightState` is only its secondary detail
//! (`phase_detail`, e.g. "Fill · Nitrous fill"). Every operator-visible state
//! comes from one `MissionPhaseMsg`, so the two can never disagree.
//!
//! Firmware migration path: see docs/system/mission-phases.md.
use crate::types::FlightState;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionPhase {
    Startup,
    Idle,
    Fill,
    Launch,
    InFlight,
    Landed,
}

impl MissionPhase {
    #[cfg(test)]
    pub const ALL: [MissionPhase; 6] = [
        MissionPhase::Startup,
        MissionPhase::Idle,
        MissionPhase::Fill,
        MissionPhase::Launch,
        MissionPhase::InFlight,
        MissionPhase::Landed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MissionPhase::Startup => "Startup",
            MissionPhase::Idle => "Idle",
            MissionPhase::Fill => "Fill",
            MissionPhase::Launch => "Launch",
            MissionPhase::InFlight => "In flight",
            MissionPhase::Landed => "Landed",
        }
    }

    /// Board-offline (disconnect) aborts are armed from Idle up to, but not
    /// including, Launch. The existing no-automatic-abort-after-Launch rule
    /// and the detailed-state Aborted exclusion still apply on top of this.
    pub fn disconnect_aborts_armed(self) -> bool {
        matches!(self, MissionPhase::Idle | MissionPhase::Fill)
    }

    /// No automatic abort of any kind (disconnect, fault, overpressure,
    /// tare/ADC failure) from Launch admission onward. Operator manual
    /// Abort/Stop is unaffected. The enforcing code stays keyed on the
    /// detailed state (`gse::automatic_abort_allowed`); this restates the
    /// boundary for the phase model and its tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn automatic_aborts_allowed(self) -> bool {
        matches!(
            self,
            MissionPhase::Startup | MissionPhase::Idle | MissionPhase::Fill
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseMode {
    Flight,
    TestFire,
    Hitl,
}

impl PhaseMode {
    pub fn current() -> Self {
        if crate::runtime_mode::is_hitl() {
            PhaseMode::Hitl
        } else if crate::runtime_mode::is_test_fire() {
            PhaseMode::TestFire
        } else {
            PhaseMode::Flight
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseInputs {
    pub mode: PhaseMode,
    pub detailed: FlightState,
    /// Every board required for this build has been seen at least once.
    pub required_boards_seen: bool,
    /// GSE fill work is running or propellant is loaded (qualification Fill
    /// or Armed stage) while the detailed state may still read Idle.
    pub fill_activity: bool,
    /// Last phase published, used only to keep an Aborted report from a
    /// vehicle in the air (or on the ground after flight) in that phase.
    pub previous: Option<MissionPhase>,
    /// Flight builds only: the T clock is past T+5 min and the FC has then
    /// been silent for 2.5 continuous minutes (`fc_contact_lost_after_flight`).
    pub fc_contact_lost: bool,
}

/// The comms-loss Landed fallback arms at T+5 min.
pub const COMMS_LOSS_ARM_T_PLUS_MS: u64 = 5 * 60 * 1000;
/// Continuous FC silence after arming that marks the vehicle Landed.
pub const COMMS_LOSS_LANDED_AFTER_MS: u64 = 150 * 1000;

/// The FC owns In flight -> Landed; this is the GS's only Landed inference.
/// Silence counts only from T+5 min: FC silence that began earlier counts
/// from T+5 min, and comms loss before T+5 min never changes the phase.
pub fn fc_contact_lost_after_flight(t_plus_ms: Option<u64>, fc_silence_ms: Option<u64>) -> bool {
    let (Some(t_plus), Some(silence)) = (t_plus_ms, fc_silence_ms) else {
        return false;
    };
    t_plus >= COMMS_LOSS_ARM_T_PLUS_MS
        && silence.min(t_plus - COMMS_LOSS_ARM_T_PLUS_MS) >= COMMS_LOSS_LANDED_AFTER_MS
}

/// Phase of a detailed state with no GS context: the HITL (manual-only) view
/// and the base of every other mode.
pub fn detailed_phase(detailed: FlightState, previous: Option<MissionPhase>) -> MissionPhase {
    use FlightState as S;
    match detailed {
        S::Startup => MissionPhase::Startup,
        S::Idle => MissionPhase::Idle,
        S::PreFill | S::FillTest | S::NitrogenFill | S::NitrousFill | S::Armed => {
            MissionPhase::Fill
        }
        S::Launch => MissionPhase::Launch,
        S::Ascent | S::Coast | S::Apogee | S::Descent | S::Reefing => MissionPhase::InFlight,
        S::Landed | S::Recovery => MissionPhase::Landed,
        S::Aborted => match previous {
            Some(MissionPhase::InFlight) => MissionPhase::InFlight,
            Some(MissionPhase::Landed) => MissionPhase::Landed,
            _ => MissionPhase::Idle,
        },
    }
}

/// Operator label of one detailed state, used only as secondary detail.
pub fn detailed_label(detailed: FlightState) -> &'static str {
    use FlightState as S;
    match detailed {
        S::Startup => "Startup",
        S::Idle => "Idle",
        S::PreFill => "Pre-fill",
        S::FillTest => "Fill test",
        S::NitrogenFill => "Nitrogen fill",
        S::NitrousFill => "Nitrous fill",
        S::Armed => "Armed",
        S::Launch => "Launch",
        S::Ascent => "Ascent",
        S::Coast => "Coast",
        S::Apogee => "Apogee",
        S::Descent => "Descent",
        S::Reefing => "Reefing",
        S::Landed => "Landed",
        S::Recovery => "Recovery",
        S::Aborted => "Aborted",
    }
}

/// Overlay text when the phase is not the detailed state's own phase.
pub const DETAIL_WAITING_FOR_BOARDS: &str = "Waiting for boards";
pub const DETAIL_GSE_FILL_ACTIVE: &str = "GSE fill active";
pub const DETAIL_FC_CONTACT_LOST: &str = "FC contact lost";

/// Secondary detail shown after the phase ("Fill · Nitrous fill"). Pure.
///
/// It first names the overlay that made the phase differ from the detailed
/// state's own phase (FC contact lost, waiting for boards, GSE fill active),
/// then the detailed state when its label adds information. Detailed Idle is
/// left out under the boards and fill overlays because those overlays only
/// apply to detailed Idle. `None` means the phase says everything.
pub fn phase_detail(inputs: &PhaseInputs, phase: MissionPhase) -> Option<String> {
    let mut parts: Vec<&'static str> = Vec::with_capacity(2);
    let mut idle_implied = false;
    let comms_loss = phase == MissionPhase::Landed
        && inputs.fc_contact_lost
        && mission_phase(&PhaseInputs {
            fc_contact_lost: false,
            ..*inputs
        }) != MissionPhase::Landed;
    if comms_loss {
        parts.push(DETAIL_FC_CONTACT_LOST);
    } else if phase == MissionPhase::Startup && inputs.detailed != FlightState::Startup {
        parts.push(DETAIL_WAITING_FOR_BOARDS);
        idle_implied = true;
    } else if phase == MissionPhase::Fill && inputs.detailed == FlightState::Idle {
        parts.push(DETAIL_GSE_FILL_ACTIVE);
        idle_implied = true;
    }
    let label = detailed_label(inputs.detailed);
    if label != phase.label() && !(idle_implied && inputs.detailed == FlightState::Idle) {
        parts.push(label);
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The single mapping from detailed state and GS context to the mission phase.
pub fn mission_phase(inputs: &PhaseInputs) -> MissionPhase {
    let base = detailed_phase(inputs.detailed, inputs.previous);
    match inputs.mode {
        PhaseMode::Hitl => base,
        mode => {
            if !inputs.required_boards_seen
                && matches!(base, MissionPhase::Startup | MissionPhase::Idle)
            {
                return MissionPhase::Startup;
            }
            match base {
                MissionPhase::Idle
                    if inputs.detailed == FlightState::Idle && inputs.fill_activity =>
                {
                    MissionPhase::Fill
                }
                MissionPhase::InFlight | MissionPhase::Landed if mode == PhaseMode::TestFire => {
                    MissionPhase::Launch
                }
                MissionPhase::Launch | MissionPhase::InFlight
                    if mode == PhaseMode::Flight && inputs.fc_contact_lost =>
                {
                    MissionPhase::Landed
                }
                other => other,
            }
        }
    }
}

/// Phase used by the flight-build safety task for the disconnect-abort
/// boundary. The safety task runs only in flight builds and only consults the
/// detailed state it already selected, so required-board and fill context do
/// not change the answer: Startup is the only pre-Launch phase without
/// disconnect aborts.
pub fn safety_phase(detailed: FlightState) -> MissionPhase {
    mission_phase(&PhaseInputs {
        mode: PhaseMode::Flight,
        detailed,
        required_boards_seen: true,
        fill_activity: false,
        previous: None,
        fc_contact_lost: false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionPhaseMsg {
    pub phase: MissionPhase,
    pub label: String,
    /// Secondary detail for the phase (`phase_detail`). The operator sees
    /// `label` alone or `label · detail`; never a second, separate state.
    pub detail: Option<String>,
    /// Detailed shared FlightState the phase was derived from. Same value as
    /// the `FlightState` message; clients use it only to build the detail.
    pub detailed: FlightState,
    pub mode: PhaseMode,
    pub manual_only: bool,
    pub aborted: bool,
    pub required_boards_seen: bool,
    pub fill_activity: bool,
    pub disconnect_aborts_armed: bool,
    /// Landed only because of the FC comms-loss fallback.
    pub comms_loss_landed: bool,
    /// Wall time of the last phase change.
    pub since_ms: i64,
}

impl MissionPhaseMsg {
    pub fn from_inputs(inputs: &PhaseInputs, since_ms: i64) -> Self {
        let phase = mission_phase(inputs);
        let aborted = inputs.detailed == FlightState::Aborted;
        Self {
            phase,
            label: phase.label().to_string(),
            detail: phase_detail(inputs, phase),
            detailed: inputs.detailed,
            mode: inputs.mode,
            manual_only: inputs.mode == PhaseMode::Hitl,
            aborted,
            required_boards_seen: inputs.required_boards_seen,
            fill_activity: inputs.fill_activity,
            disconnect_aborts_armed: inputs.mode == PhaseMode::Flight
                && !aborted
                && phase.disconnect_aborts_armed(),
            comms_loss_landed: phase == MissionPhase::Landed
                && mission_phase(&PhaseInputs {
                    fc_contact_lost: false,
                    ..*inputs
                }) != MissionPhase::Landed,
            since_ms,
        }
    }
}

static CHANNEL: LazyLock<watch::Sender<MissionPhaseMsg>> = LazyLock::new(|| {
    let inputs = PhaseInputs {
        mode: PhaseMode::current(),
        detailed: FlightState::Startup,
        required_boards_seen: false,
        fill_activity: false,
        previous: None,
        fc_contact_lost: false,
    };
    watch::channel(MissionPhaseMsg::from_inputs(&inputs, 0)).0
});

pub fn subscribe() -> watch::Receiver<MissionPhaseMsg> {
    CHANNEL.subscribe()
}

fn gather(state: &crate::state::AppState, previous: Option<MissionPhase>) -> PhaseInputs {
    let mode = PhaseMode::current();
    let detailed = state.local_flight_state_snapshot();
    let (required_boards_seen, fill_activity) = if mode == PhaseMode::Hitl {
        (true, false)
    } else {
        // A busy GSE owner keeps the last observed activity; the phase never
        // waits on the GSE lock.
        let last = CHANNEL.borrow().fill_activity;
        (
            state.all_required_boards_seen(),
            crate::gse::mission_fill_activity(state).unwrap_or(last),
        )
    };
    PhaseInputs {
        mode,
        detailed,
        required_boards_seen,
        fill_activity,
        previous,
        fc_contact_lost: mode == PhaseMode::Flight && fc_contact_lost(state),
    }
}

/// T+ elapsed and FC silence, both in the procedure clock domain (the
/// explicit model clock when configured, wall time otherwise).
fn fc_contact_lost(state: &crate::state::AppState) -> bool {
    let clock = state.launch_clock_snapshot();
    if clock.kind != crate::telemetry_db::LaunchClockKind::TPlus {
        return false;
    }
    let now = crate::telemetry_task::procedure_clock::now();
    let t_plus = clock
        .anchor_timestamp_ms
        .and_then(|anchor| u64::try_from(now.timestamp_ms().saturating_sub(anchor)).ok());
    let silence = {
        let boards = state.board_status.lock().unwrap();
        boards
            .get(&crate::types::Board::FlightComputer)
            .and_then(|status| match now.model_monotonic_ms() {
                Some(model_now) => status
                    .last_seen_model_ms
                    .and_then(|last| model_now.checked_sub(last)),
                None => status.last_seen_ms.map(|last| {
                    crate::telemetry_task::get_current_timestamp_ms().saturating_sub(last)
                }),
            })
    };
    fc_contact_lost_after_flight(t_plus, silence)
}

/// Recompute and publish the phase now. Returns the published message.
pub fn refresh(state: &crate::state::AppState) -> MissionPhaseMsg {
    let current = CHANNEL.borrow().clone();
    let previous = (current.since_ms != 0).then_some(current.phase);
    let inputs = gather(state, previous);
    let phase = mission_phase(&inputs);
    let since_ms = if previous == Some(phase) {
        current.since_ms
    } else {
        crate::telemetry_task::get_current_timestamp_ms() as i64
    };
    let next = MissionPhaseMsg::from_inputs(&inputs, since_ms);
    if next.comms_loss_landed && !current.comms_loss_landed {
        let text = "FC contact lost for 2.5 min after T+5 min; marking Landed.";
        log::warn!("{text}");
        state.add_notification_with_persistence(text, true);
        crate::web::persist_message_db_only(state, text.to_string());
    } else if current.comms_loss_landed && !next.comms_loss_landed && !inputs.fc_contact_lost {
        let text = format!(
            "FC contact restored; following FC-reported state ({:?}, phase {}).",
            inputs.detailed,
            phase.label()
        );
        log::info!("{text}");
        state.add_notification_with_persistence(text.clone(), true);
        crate::web::persist_message_db_only(state, text);
    }
    if next != current {
        if previous != Some(phase) {
            log::info!(
                "Mission phase {} (detailed {:?})",
                phase.label(),
                inputs.detailed
            );
        }
        CHANNEL.send_replace(next.clone());
    }
    next
}

/// Cheap periodic projection: one state lock, one board-status lock and a
/// nonwaiting GSE probe every 250 ms, plus an immediate refresh on every
/// flight-state publication so the phase never trails the `FlightState`
/// message it is derived from.
pub async fn service(
    state: Arc<crate::state::AppState>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    let mut flight_state_rx = state.state_tx.subscribe();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                refresh(&state);
                crate::flight_maxima::tick(&state);
            }
            changed = flight_state_rx.recv() => match changed {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    refresh(&state);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = shutdown_rx.recv() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [PhaseMode; 3] = [PhaseMode::Flight, PhaseMode::TestFire, PhaseMode::Hitl];
    const PREVIOUS: [Option<MissionPhase>; 7] = [
        None,
        Some(MissionPhase::Startup),
        Some(MissionPhase::Idle),
        Some(MissionPhase::Fill),
        Some(MissionPhase::Launch),
        Some(MissionPhase::InFlight),
        Some(MissionPhase::Landed),
    ];

    fn all_states() -> Vec<FlightState> {
        (0..16)
            .map(|wire| crate::types::u8_to_flight_state(wire).unwrap())
            .collect()
    }

    /// Independent restatement of the documented table, written per mode.
    fn expected(i: &PhaseInputs) -> MissionPhase {
        use FlightState as S;
        use MissionPhase as P;
        let aborted_phase = match i.previous {
            Some(P::InFlight) => P::InFlight,
            Some(P::Landed) => P::Landed,
            _ => P::Idle,
        };
        match i.mode {
            PhaseMode::Hitl => match i.detailed {
                S::Startup => P::Startup,
                S::Idle => P::Idle,
                S::PreFill | S::FillTest | S::NitrogenFill | S::NitrousFill | S::Armed => P::Fill,
                S::Launch => P::Launch,
                S::Ascent | S::Coast | S::Apogee | S::Descent | S::Reefing => P::InFlight,
                S::Landed | S::Recovery => P::Landed,
                S::Aborted => aborted_phase,
            },
            PhaseMode::Flight | PhaseMode::TestFire => {
                let tf = i.mode == PhaseMode::TestFire;
                let lost = !tf && i.fc_contact_lost;
                let flown = |p| match p {
                    _ if tf => P::Launch,
                    P::InFlight if lost => P::Landed,
                    other => other,
                };
                match i.detailed {
                    S::Startup => P::Startup,
                    S::Idle if !i.required_boards_seen => P::Startup,
                    S::Idle if i.fill_activity => P::Fill,
                    S::Idle => P::Idle,
                    S::PreFill | S::FillTest | S::NitrogenFill | S::NitrousFill | S::Armed => {
                        P::Fill
                    }
                    S::Launch if lost => P::Landed,
                    S::Launch => P::Launch,
                    S::Ascent | S::Coast | S::Apogee | S::Descent | S::Reefing => {
                        flown(P::InFlight)
                    }
                    S::Landed | S::Recovery => flown(P::Landed),
                    S::Aborted => match aborted_phase {
                        P::Idle if !i.required_boards_seen => P::Startup,
                        P::Idle => P::Idle,
                        other => flown(other),
                    },
                }
            }
        }
    }

    #[test]
    fn mapping_matches_the_documented_table_for_every_input() {
        let mut checked = 0;
        for mode in MODES {
            for detailed in all_states() {
                for required_boards_seen in [false, true] {
                    for fill_activity in [false, true] {
                        for previous in PREVIOUS {
                            for fc_contact_lost in [false, true] {
                                let inputs = PhaseInputs {
                                    mode,
                                    detailed,
                                    required_boards_seen,
                                    fill_activity,
                                    previous,
                                    fc_contact_lost,
                                };
                                assert_eq!(mission_phase(&inputs), expected(&inputs), "{inputs:?}");
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 3 * 16 * 2 * 2 * 7 * 2);
    }

    #[test]
    fn hitl_is_manual_only() {
        for detailed in all_states() {
            let base = PhaseInputs {
                mode: PhaseMode::Hitl,
                detailed,
                required_boards_seen: false,
                fill_activity: false,
                previous: None,
                fc_contact_lost: false,
            };
            let context = PhaseInputs {
                fc_contact_lost: true,
                required_boards_seen: true,
                fill_activity: true,
                ..base
            };
            assert_eq!(mission_phase(&base), mission_phase(&context));
            assert!(!MissionPhaseMsg::from_inputs(&context, 1).disconnect_aborts_armed);
            assert!(MissionPhaseMsg::from_inputs(&context, 1).manual_only);
        }
    }

    #[test]
    fn test_fire_never_reports_flight() {
        for detailed in all_states() {
            for previous in PREVIOUS {
                let phase = mission_phase(&PhaseInputs {
                    mode: PhaseMode::TestFire,
                    detailed,
                    required_boards_seen: true,
                    fill_activity: false,
                    previous,
                    fc_contact_lost: true,
                });
                assert!(!matches!(
                    phase,
                    MissionPhase::InFlight | MissionPhase::Landed
                ));
            }
        }
    }

    #[test]
    fn disconnect_aborts_arm_from_idle_until_launch() {
        for detailed in all_states() {
            let armed = safety_phase(detailed).disconnect_aborts_armed();
            let wire = detailed as u8;
            // Idle (1) through Armed (6) are armed; Aborted (15) maps to Idle
            // but the safety task's detailed-state suppression still applies.
            assert_eq!(armed, (1..=6).contains(&wire) || wire == 15, "{detailed:?}");
        }
        let msg = MissionPhaseMsg::from_inputs(
            &PhaseInputs {
                mode: PhaseMode::Flight,
                detailed: FlightState::Aborted,
                required_boards_seen: true,
                fill_activity: false,
                previous: None,
                fc_contact_lost: false,
            },
            1,
        );
        assert!(msg.aborted && !msg.disconnect_aborts_armed);
    }

    #[test]
    fn comms_loss_landed_needs_t_plus_5_then_150_s_of_fc_silence() {
        const MIN: u64 = 60_000;
        let lost = |t, s| fc_contact_lost_after_flight(Some(t), Some(s));
        // Before T+5 min comms loss never changes the phase.
        assert!(!lost(4 * MIN, 4 * MIN));
        assert!(!lost(5 * MIN - 1, 10 * MIN));
        // Silence that began before T+5 counts only from T+5.
        assert!(!lost(5 * MIN + 149_999, 10 * MIN));
        assert!(lost(5 * MIN + 150_000, 10 * MIN));
        // Silence that began after T+5 must last 2.5 continuous minutes.
        assert!(!lost(9 * MIN, 149_999));
        assert!(lost(9 * MIN, 150_000));
        // No T clock or an FC never seen: no inference.
        assert!(!fc_contact_lost_after_flight(None, Some(10 * MIN)));
        assert!(!fc_contact_lost_after_flight(Some(10 * MIN), None));
    }

    #[test]
    fn fc_reported_state_wins_when_contact_returns() {
        let flying = PhaseInputs {
            mode: PhaseMode::Flight,
            detailed: FlightState::Descent,
            required_boards_seen: true,
            fill_activity: false,
            previous: Some(MissionPhase::InFlight),
            fc_contact_lost: true,
        };
        let msg = MissionPhaseMsg::from_inputs(&flying, 1);
        assert_eq!(msg.phase, MissionPhase::Landed);
        assert!(msg.comms_loss_landed);
        // Contact returns, FC still reports descent: back to In flight.
        let back = PhaseInputs {
            fc_contact_lost: false,
            previous: Some(MissionPhase::Landed),
            ..flying
        };
        assert_eq!(mission_phase(&back), MissionPhase::InFlight);
        // FC reports Landed: stays Landed, and not because of the fallback.
        let landed = PhaseInputs {
            detailed: FlightState::Landed,
            ..back
        };
        assert_eq!(mission_phase(&landed), MissionPhase::Landed);
        let msg = MissionPhaseMsg::from_inputs(
            &PhaseInputs {
                fc_contact_lost: true,
                ..landed
            },
            1,
        );
        assert!(!msg.comms_loss_landed);
        // Prelaunch phases are never changed by the fallback.
        for detailed in [FlightState::Idle, FlightState::Armed, FlightState::Startup] {
            let i = PhaseInputs { detailed, ..flying };
            assert_ne!(mission_phase(&i), MissionPhase::Landed);
        }
    }

    #[tokio::test]
    async fn shared_automatic_abort_boundary_is_closed_after_launch() {
        let state = crate::state::tests::test_app_state().await;
        for wire in 7..=14u8 {
            let detailed = crate::types::u8_to_flight_state(wire).unwrap();
            state.set_preexisting_flight_state_for_test(detailed);
            assert!(!safety_phase(detailed).automatic_aborts_allowed());
            assert!(!crate::gse::automatic_abort_allowed(&state), "{detailed:?}");
            assert!(
                crate::gse::claim_automatic_abort_for_generation(
                    &state,
                    crate::gse::automatic_generation(&state)
                )
                .is_none(),
                "{detailed:?}"
            );
        }
    }

    fn every_input() -> Vec<PhaseInputs> {
        let mut out = Vec::new();
        for mode in MODES {
            for detailed in all_states() {
                for required_boards_seen in [false, true] {
                    for fill_activity in [false, true] {
                        for previous in PREVIOUS {
                            for fc_contact_lost in [false, true] {
                                out.push(PhaseInputs {
                                    mode,
                                    detailed,
                                    required_boards_seen,
                                    fill_activity,
                                    previous,
                                    fc_contact_lost,
                                });
                            }
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn detail_never_contradicts_the_phase() {
        for inputs in every_input() {
            let phase = mission_phase(&inputs);
            let detail = phase_detail(&inputs, phase);
            let own = detailed_phase(inputs.detailed, inputs.previous);
            let text = detail.clone().unwrap_or_default();
            let names_overlay = [
                DETAIL_WAITING_FOR_BOARDS,
                DETAIL_GSE_FILL_ACTIVE,
                DETAIL_FC_CONTACT_LOST,
            ]
            .iter()
            .any(|overlay| text.starts_with(overlay));
            // A phase that differs from the detailed state's own phase always
            // names its overlay, except the documented test-fire collapse of
            // flight states into Launch.
            let test_fire_collapse = inputs.mode == PhaseMode::TestFire
                && phase == MissionPhase::Launch
                && matches!(own, MissionPhase::InFlight | MissionPhase::Landed);
            assert!(
                phase == own || names_overlay || test_fire_collapse,
                "{inputs:?} -> {phase:?} {detail:?}"
            );
            // The detail never repeats the phase and never shows a second
            // phase label for a state that maps to the same phase.
            assert_ne!(detail.as_deref(), Some(phase.label()), "{inputs:?}");
            if phase == own && !names_overlay {
                let expected = (detailed_label(inputs.detailed) != phase.label())
                    .then(|| detailed_label(inputs.detailed).to_string());
                assert_eq!(detail, expected, "{inputs:?}");
            }
            // Aborted is always visible.
            if inputs.detailed == FlightState::Aborted {
                assert!(text.ends_with("Aborted"), "{inputs:?}");
            }
        }
    }

    #[test]
    fn detail_examples_match_the_documented_table() {
        let base = PhaseInputs {
            mode: PhaseMode::Flight,
            detailed: FlightState::Idle,
            required_boards_seen: true,
            fill_activity: false,
            previous: None,
            fc_contact_lost: false,
        };
        let shown = |i: PhaseInputs| {
            let msg = MissionPhaseMsg::from_inputs(&i, 1);
            match msg.detail {
                Some(detail) => format!("{} · {detail}", msg.label),
                None => msg.label,
            }
        };
        assert_eq!(shown(base), "Idle");
        assert_eq!(
            shown(PhaseInputs {
                required_boards_seen: false,
                ..base
            }),
            "Startup · Waiting for boards"
        );
        assert_eq!(
            shown(PhaseInputs {
                fill_activity: true,
                ..base
            }),
            "Fill · GSE fill active"
        );
        assert_eq!(
            shown(PhaseInputs {
                detailed: FlightState::NitrousFill,
                ..base
            }),
            "Fill · Nitrous fill"
        );
        assert_eq!(
            shown(PhaseInputs {
                detailed: FlightState::Coast,
                ..base
            }),
            "In flight · Coast"
        );
        assert_eq!(
            shown(PhaseInputs {
                detailed: FlightState::Launch,
                ..base
            }),
            "Launch"
        );
        assert_eq!(
            shown(PhaseInputs {
                detailed: FlightState::Aborted,
                ..base
            }),
            "Idle · Aborted"
        );
        assert_eq!(
            shown(PhaseInputs {
                detailed: FlightState::Aborted,
                previous: Some(MissionPhase::InFlight),
                ..base
            }),
            "In flight · Aborted"
        );
        assert_eq!(
            shown(PhaseInputs {
                detailed: FlightState::Descent,
                fc_contact_lost: true,
                ..base
            }),
            "Landed · FC contact lost · Descent"
        );
        assert_eq!(
            shown(PhaseInputs {
                mode: PhaseMode::Hitl,
                detailed: FlightState::Armed,
                ..base
            }),
            "Fill · Armed"
        );
        assert_eq!(
            shown(PhaseInputs {
                mode: PhaseMode::TestFire,
                detailed: FlightState::Coast,
                ..base
            }),
            "Launch · Coast"
        );
    }

    #[tokio::test]
    async fn phase_follows_a_flight_state_publication_without_waiting_for_the_tick() {
        let state = crate::state::tests::test_app_state().await;
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
        let mut rx = subscribe();
        let task = tokio::spawn(service(state.clone(), shutdown_rx));
        // Let the first periodic refresh run, then publish well inside the
        // 250 ms tick: only the flight-state subscription can meet 150 ms.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        state.set_preexisting_flight_state_for_test(FlightState::NitrousFill);
        let seen = tokio::time::timeout(std::time::Duration::from_millis(150), async {
            loop {
                if rx.borrow_and_update().detailed == FlightState::NitrousFill {
                    return;
                }
                rx.changed().await.unwrap();
            }
        })
        .await;
        let _ = shutdown_tx.send(());
        let _ = task.await;
        assert!(seen.is_ok(), "phase did not follow the flight state");
        let msg = rx.borrow().clone();
        assert_eq!(msg.phase, MissionPhase::Fill);
        assert_eq!(msg.detail.as_deref(), Some("Nitrous fill"));
    }

    #[test]
    fn phase_serializes_as_snake_case_labels() {
        let names: Vec<String> = MissionPhase::ALL
            .iter()
            .map(|p| {
                serde_json::to_value(p)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            ["startup", "idle", "fill", "launch", "in_flight", "landed"]
        );
        assert_eq!(MissionPhase::InFlight.label(), "In flight");
    }
}
