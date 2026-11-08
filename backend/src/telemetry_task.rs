mod prelude;
#[cfg(test)]
mod umbilical_owner_tests;
#[cfg(test)]
mod held_launch_tests;
use prelude::*;

mod commands;
use commands::log_command_dispatch;
#[cfg(feature = "hitl_mode")]
use commands::queue_locally_routed_flight_command_with_launch_revision;
pub(crate) use commands::{
    controller_packet, flush_command_tx, queue_abort_packet, queue_automatic_abort_packet,
    queue_automatic_controller_command, queue_locally_routed_flight_command,
    queue_manual_abort_wire_receipted,
};
// The Valve Board overpressure acknowledgement must still reach the board while
// an Abort is latched, so it uses the raw controller queue in every build.
pub(crate) use commands::queue_controller_command;
#[cfg(test)]
pub(crate) use commands::{queue_priority_manual_abort_packet, queue_manual_abort_wire};
mod heartbeat;
mod periodic;
pub(crate) mod procedure_clock;
mod radio_io;
pub(crate) mod recording_owner;
pub(crate) mod recording_diagnostics;
pub use radio_io::{CommsWorkerHandle, TransportTxActivity, TxQueueBudget,RecoveryFrameClassifier};
pub(crate) use radio_io::{is_recovery_control_frame,RECOVERY_CONTROL_TYPES};
#[cfg(test)]
use radio_io::{radio_command_log_line, spawn_dedicated_radio_io_threads};
use radio_io::{spawn_comms_worker_threads, spawn_router_worker_thread};

const PACKET_WORK_QUEUE_SIZE: usize = 8_192;
const PACKET_ENQUEUE_BURST: usize = 256;
const BACKPRESSURE_LOG_INTERVAL_MS: u64 = 10_000;
const DB_BATCH_MAX_DEFAULT: usize = 256;
const DB_BATCH_WAIT_MS_DEFAULT: u64 = 8;
const ROUTER_QUEUE_BUDGET_MS: u32 = 6;
const ROUTER_TX_BUDGET_MS: u32 = 3;
const ROUTER_RX_BUDGET_MS: u32 = ROUTER_QUEUE_BUDGET_MS - ROUTER_TX_BUDGET_MS;
const COMMS_ERROR_LOG_INTERVAL_MS: u64 = 30_000;
const ROUTER_DECODE_ERROR_LOG_INTERVAL_MS: u64 = 30_000;
static ROUTER_DECODE_ERROR_LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
static ROUTER_DECODE_ERROR_SUPPRESSED: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn command_at_dispatch(
    state: &AppState,
    command: TelemetryCommand,
) -> Option<TelemetryCommand> {
    command_at_dispatch_with_manual_origin(state, command, None)
}
fn command_at_dispatch_with_manual_origin(
    state: &AppState,
    command: TelemetryCommand,
    origin: Option<crate::operator_queue::manual_origin::Origin>,
) -> Option<TelemetryCommand> {
    // Recovery closes ordinary dispatch independently of the manual Stop path.
    if state.operator_admission_barrier.load(Ordering::Acquire) != 0
        && !matches!(command, TelemetryCommand::Abort)
        && !origin.is_some_and(|origin| origin.dispatched(state))
    {
        return None;
    }
    // Only the worker's generation-checked admission may unwrap operator intent.
    if matches!(command, TelemetryCommand::QueuedOperator { .. }) {
        return None;
    }
    if command
        .automatic_generation()
        .is_some_and(|generation| generation != crate::gse::automatic_generation(state))
    {
        return None;
    }
    if matches!(
        command,
        TelemetryCommand::AutomaticNitrogenClose { .. }
            | TelemetryCommand::AutomaticNitrousClose { .. }
    ) {
        return crate::gse::automatic_abort_allowed(state).then_some(match command {
            TelemetryCommand::AutomaticNitrogenClose { .. } => TelemetryCommand::NitrogenClose,
            _ => TelemetryCommand::NitrousClose,
        });
    }
    if matches!(command, TelemetryCommand::AutomaticAbort { .. }) {
        // A failed tare can enqueue before Fire commits and be consumed after
        // it. Preserve its automatic origin until this final policy check.
        crate::gse::automatic_abort_allowed(state).then_some(TelemetryCommand::Abort)
    } else {
        Some(command)
    }
}

#[cfg(feature = "hitl_mode")]
fn hitl_flight_command_id(cmd: &TelemetryCommand) -> Option<u8> {
    Some(match cmd {
        TelemetryCommand::DeployParachute => FlightComputerCommands::DeployParachute as u8,
        TelemetryCommand::ExpandParachute => FlightComputerCommands::ExpandParachute as u8,
        TelemetryCommand::EvaluationRelax => FlightComputerCommands::EvaluationRelax as u8,
        TelemetryCommand::EvaluationFocus => FlightComputerCommands::EvaluationFocus as u8,
        TelemetryCommand::EvaluationAbort => FlightComputerCommands::EvaluationAbort as u8,
        TelemetryCommand::ReinitSensors => FlightComputerCommands::ReinitSensors as u8,
        TelemetryCommand::ReinitBarometer => FlightComputerCommands::ReinitBarometer as u8,
        TelemetryCommand::EnableIMU => FlightComputerCommands::EnableIMU as u8,
        TelemetryCommand::DisableIMU => FlightComputerCommands::DisableIMU as u8,
        TelemetryCommand::Postinit => FlightComputerCommands::Postinit as u8,
        TelemetryCommand::AdvanceFlightState => FlightComputerCommands::AdvanceFlightState as u8,
        TelemetryCommand::RewindFlightState => FlightComputerCommands::RewindFlightState as u8,
        TelemetryCommand::AbortAfter40 => FlightComputerCommands::AbortAfter40 as u8,
        TelemetryCommand::AbortAfter100 => FlightComputerCommands::AbortAfter100 as u8,
        TelemetryCommand::AbortAfter250 => FlightComputerCommands::AbortAfter250 as u8,
        TelemetryCommand::ReinitAfter15 => FlightComputerCommands::ReinitAfter15 as u8,
        TelemetryCommand::ReinitAfter30 => FlightComputerCommands::ReinitAfter30 as u8,
        TelemetryCommand::ReinitAfter50 => FlightComputerCommands::ReinitAfter50 as u8,
        _ => return None,
    })
}

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
const OPERATOR_MODE_FLIGHT_STATE_ORDER: [FlightState; 16] = [
    FlightState::Startup,
    FlightState::Idle,
    FlightState::PreFill,
    FlightState::FillTest,
    FlightState::NitrogenFill,
    FlightState::NitrousFill,
    FlightState::Armed,
    FlightState::Launch,
    FlightState::Ascent,
    FlightState::Coast,
    FlightState::Apogee,
    FlightState::Descent,
    FlightState::Reefing,
    FlightState::Landed,
    FlightState::Recovery,
    FlightState::Aborted,
];

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
fn operator_mode_adjacent_flight_state(current: FlightState, delta: i32) -> FlightState {
    let idx = OPERATOR_MODE_FLIGHT_STATE_ORDER
        .iter()
        .position(|s| *s == current)
        .unwrap_or(0) as i32;
    let next_idx =
        (idx + delta).clamp(0, (OPERATOR_MODE_FLIGHT_STATE_ORDER.len() - 1) as i32) as usize;
    OPERATOR_MODE_FLIGHT_STATE_ORDER[next_idx]
}

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
async fn set_local_flight_state_for_operator_mode(
    state: &Arc<AppState>,
    next_state: FlightState,
    launch_revision: Option<u64>,
) {
    #[cfg(all(test, not(feature = "hitl_mode"), feature = "test_fire_mode"))]
    if let Some(router) = state.topology_router.get() {
        flight_state_timeline::event(router, "processed", next_state as u8, None, None, None);
    }
    if next_state == FlightState::Launch {
        state.request_local_launch_state(launch_revision);
    } else {
        state.set_local_flight_state(next_state);
    }
}

#[cfg(all(test, not(feature = "hitl_mode"), feature = "test_fire_mode"))]
pub(crate) mod flight_state_timeline;

mod derived;
use derived::*;

#[cfg(feature = "hitl_mode")]
fn reset_operator_flight_state(state: &Arc<AppState>) {
    // Use the managed network variable and existing persistence path, not a
    // frontend-only display override. Do not operate physical outputs here.
    state.set_local_flight_state(FlightState::Idle);
    sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
}

#[cfg(all(test, feature = "hitl_mode"))]
mod recovery_tests {
    use super::*;

    #[tokio::test]
    async fn reset_state_preserves_clock_and_confirmed_valve_state() {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Recovery);
        let clock = state.launch_clock_snapshot();
        state.set_umbilical_valve_state(ActuatorBoardCommands::NitrousOpen as u8, true);
        let mut updates = state.state_tx.subscribe();
        reset_operator_flight_state(&state);
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        assert_eq!(updates.try_recv().unwrap().state, FlightState::Idle);
        assert_eq!(state.launch_clock_snapshot(), clock);
        assert_eq!(
            state.get_umbilical_valve_state(ActuatorBoardCommands::NitrousOpen as u8),
            Some(true)
        );
    }

    #[tokio::test]
    async fn reset_clock_preserves_flight_state_and_broadcasts_idle_clock() {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Ascent);
        let mut updates = state.launch_clock_tx.subscribe();
        state.reset_launch_clock();
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Ascent);
        assert_eq!(
            updates.try_recv().unwrap(),
            crate::telemetry_db::LaunchClockMsg::idle()
        );
    }
}

pub(crate) fn refresh_host_network_time(router: &Router, utc_ms: u64) {
    let utc_ms = procedure_clock::for_wall(utc_ms).utc_ms();
    // Calendar range shared with DAQ's timestamped logging validation.
    if (315_532_800_000..4_354_819_200_000).contains(&utc_ms) {
        router.set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(utc_ms));
    }
}

pub fn set_network_time_router(router: Arc<Router>) {
    let _ = NETWORK_TIME_ROUTER.set(router);
}

pub(crate) fn recording_network_utc_ms() -> Option<i64> {
    NETWORK_TIME_ROUTER
        .get()?
        .network_time()?
        .unix_time_ms?
        .try_into()
        .ok()
}

pub(crate) fn displayed_network_utc_ms() -> i64 {
    recording_network_utc_ms().unwrap_or_else(|| get_current_timestamp_ms() as i64)
}

fn build_launch_packet(
    router: &Router,
    kind: &str,
    owner: &str,
    payload: &[u8],
) -> sedsnet::TelemetryResult<Packet> {
    Packet::new(
        crate::telemetry_schema::data_type(kind),
        &[
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            crate::telemetry_schema::endpoint(owner),
        ],
        &router.sender(),
        crate::system_clock::control_packet_timestamp_ms(router),
        Arc::from(payload),
    )
}

fn vent_valve_known_open_for_launch(state: &Arc<AppState>) -> bool {
    effective_umbilical_valve_state(state, ValveBoardCommands::NormallyOpenOpen as u8) == Some(true)
}

fn warn_launch_blocked_by_vent_valve(state: &Arc<AppState>) {
    emit_notification_warning(
        state,
        "Vent valve is still reported open. Close it before initiating the launch sequence.",
    );
}

fn start_launch_clock_from_valve_sequence_status(state: &Arc<AppState>) {
    if crate::gse::cold_flow::active(state){return;}
    if matches!(
        state.launch_clock_snapshot().kind,
        LaunchClockKind::TMinus | LaunchClockKind::TPlus
    ) {
        return;
    }

    state.clear_launch_sequence_command_pending();
    let now_ms = procedure_clock::timestamp_ms();
    state.set_launch_clock(launch_countdown_clock(now_ms));

    gs_debug_println!(
        "Valve-board sequence started launch clock; T-0 in {} ms",
        crate::state::LAUNCH_COUNTDOWN_DURATION_MS
    );
}

fn transition_launch_clock_to_t_plus_from_pilot_open(state: &Arc<AppState>) {
    if crate::gse::cold_flow::active(state){return;}
    if state.launch_clock_snapshot().kind != LaunchClockKind::TMinus {
        return;
    }

    state.set_launch_clock(LaunchClockMsg {
        kind: LaunchClockKind::TPlus,
        anchor_timestamp_ms: Some(procedure_clock::timestamp_ms()),
        duration_ms: None,
    });
    gs_debug_println!("Pilot valve open status received; launch clock switched to T+");
}

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
async fn handle_local_ground_station_launch_command(
    state: Arc<AppState>,
    router: Arc<Router>,
    launch_revision: Option<u64>,
) {
    send_local_ground_station_launch(state, router, launch_revision, None).await;
}

/// `pressed` is the press-time ticket of a Launch held behind the loadcell
/// zero; `None` is an immediate press.
#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
async fn send_local_ground_station_launch(
    state: Arc<AppState>,
    router: Arc<Router>,
    launch_revision: Option<u64>,
    pressed: Option<crate::gse::LaunchAdmission>,
) {
    if let Some(reason) =
        crate::checklists::launch_block().filter(|_| state.checklist_enforcement_enabled())
    {
        emit_notification_warning(&state, reason);
        return;
    }
    if vent_valve_known_open_for_launch(&state) {
        // HITL warns only: the last report may come from a Valve Board that is
        // gone, leaving no way to close it and no way to send Launch.
        if cfg!(feature = "hitl_mode") {
            emit_notification_warning(&state, "Vent valve is still reported open; sending Launch anyway (HITL).");
        } else {
            warn_launch_blocked_by_vent_valve(&state);
            sequences::refresh_action_policy_now(&state);
            state.broadcast_action_policy_snapshot();
            gs_debug_println!("Launch command ignored because vent valve is reported open");
            return;
        }
    }
    if matches!(
        state.launch_clock_snapshot().kind,
        LaunchClockKind::TMinus | LaunchClockKind::TPlus
    ) {
        emit_notification_warning(&state, "Launch not sent: the T-clock is already running. Use Reset all boards to send Launch again.");
        return;
    }
    if !state.try_begin_launch_sequence_command() {
        emit_notification_warning(&state, "Launch not sent: the last Launch is still waiting for the Valve Board to start its sequence. Use Reset all boards to send Launch again.");
        return;
    }
    if state.recording_status_snapshot().mode != RecordingModeWire::Recording {
        let _ = state
            .db_queue_tx
            .send(DbQueueItem::Control(RecordingCommand::StartNow))
            .await;
        gs_debug_println!("Ground-station launch auto-started DB recording");
    }
    // Refuse known missing ignition evidence before recording a sticky Fire
    // intent. Final nonce/epoch/Stop/route checks still run at SDK admission.
    if let Err(reason) = crate::ignition_pair::readiness(&state) {
        state.clear_launch_sequence_command_pending();
        emit_notification_warning(&state, reason);
        return;
    }
    #[cfg(feature = "hitl_mode")]
    let ticket = match crate::gse::inhibit_automatic_actions_for_pressed_launch(
        &state,
        launch_revision,
        pressed,
    ) {
        Ok(ticket) => ticket,
        Err(error) => {
            state.clear_launch_sequence_command_pending();
            emit_notification_warning(&state, error);
            return;
        }
    };
    #[cfg(not(feature = "hitl_mode"))]
    let ticket = match crate::gse::commit_fire_intent_for_pressed_launch(&state, launch_revision, pressed) {
        Ok(ticket) => ticket,
        Err(error) => {
            state.clear_launch_sequence_command_pending();
            emit_notification_warning(&state, error);
            return;
        }
    };
    let packet = match build_launch_packet(
        &router,
        "VALVE_COMMAND",
        "VALVE_BOARD",
        &[ValveBoardCommands::Sequence as u8],
    ) {
        Ok(packet) => packet,
        Err(error) => {
            state.clear_launch_sequence_command_pending();
            emit_notification_warning(
                &state,
                format!("Launch packet preparation failed; inhibition retained: {error}"),
            );
            return;
        }
    };
    if let Err(error) =
        crate::gse::queue_admitted_launch_packets(&state, &router, ticket, vec![packet])
    {
        state.clear_launch_sequence_command_pending();
        emit_notification_warning(&state, error);
        return;
    }
    state.begin_daq_launch_log();
    log_command_dispatch(
        "Valve launch sequence command",
        "umbilical_comms",
        crate::telemetry_schema::data_type("VALVE_COMMAND"),
        &[ValveBoardCommands::Sequence as u8],
    );
    flush_command_tx(&router, "Valve launch sequence command tx");
    // Zero before Launch already replaced the at-launch capture.
    if pressed.is_none() {
        crate::auto_zero::launch(&state);
    }
    state.set_launch_indicator_latched(true);
    sequences::refresh_action_policy_now(&state);
    state.broadcast_action_policy_snapshot();
    gs_debug_println!(
        "Ground-station launch sequence command sent; waiting for valve-board clock start"
    );
}

#[cfg(any(test, not(all(feature = "test_fire_mode", not(feature = "hitl_mode")))))]
async fn handle_flight_computer_launch_command(
    state: Arc<AppState>,
    router: Arc<Router>,
    launch_revision: Option<u64>,
) {
    send_flight_computer_launch(state, router, launch_revision, None).await;
}

/// `pressed` is the press-time ticket of a Launch held behind the loadcell
/// zero; `None` is an immediate press.
#[cfg(any(test, not(all(feature = "test_fire_mode", not(feature = "hitl_mode")))))]
async fn send_flight_computer_launch(
    state: Arc<AppState>,
    router: Arc<Router>,
    launch_revision: Option<u64>,
    pressed: Option<crate::gse::LaunchAdmission>,
) {
    if let Some(reason) =
        crate::checklists::launch_block().filter(|_| state.checklist_enforcement_enabled())
    {
        emit_notification_warning(&state, reason);
        return;
    }
    if vent_valve_known_open_for_launch(&state) {
        // HITL warns only: the last report may come from a Valve Board that is
        // gone, leaving no way to close it and no way to send Launch.
        if cfg!(feature = "hitl_mode") {
            emit_notification_warning(&state, "Vent valve is still reported open; sending Launch anyway (HITL).");
        } else {
            warn_launch_blocked_by_vent_valve(&state);
            sequences::refresh_action_policy_now(&state);
            state.broadcast_action_policy_snapshot();
            gs_debug_println!("Launch command ignored because vent valve is reported open");
            return;
        }
    }
    if matches!(
        state.launch_clock_snapshot().kind,
        LaunchClockKind::TMinus | LaunchClockKind::TPlus
    ) {
        emit_notification_warning(&state, "Launch not sent: the T-clock is already running. Use Reset all boards to send Launch again.");
        return;
    }
    if state.recording_status_snapshot().mode != RecordingModeWire::Recording {
        let _ = state
            .db_queue_tx
            .send(DbQueueItem::Control(RecordingCommand::StartNow))
            .await;
        gs_debug_println!("Launch auto-started DB recording");
    }
    // Refuse known missing ignition evidence before recording a sticky Fire
    // intent. Final nonce/epoch/Stop/route checks still run at SDK admission.
    if let Err(reason) = crate::ignition_pair::readiness(&state) {
        state.clear_launch_sequence_command_pending();
        emit_notification_warning(&state, reason);
        return;
    }
    #[cfg(feature = "hitl_mode")]
    let ticket = match crate::gse::inhibit_automatic_actions_for_pressed_launch(
        &state,
        launch_revision,
        pressed,
    ) {
        Ok(ticket) => ticket,
        Err(error) => {
            state.clear_launch_sequence_command_pending();
            emit_notification_warning(&state, error);
            return;
        }
    };
    #[cfg(not(feature = "hitl_mode"))]
    let ticket = match crate::gse::commit_fire_intent_for_pressed_launch(&state, launch_revision, pressed) {
        Ok(ticket) => ticket,
        Err(error) => {
            state.clear_launch_sequence_command_pending();
            emit_notification_warning(&state, error);
            return;
        }
    };
    let packets = (|| -> sedsnet::TelemetryResult<Vec<Packet>> {
        Ok(vec![
            build_launch_packet(
                &router,
                "FLIGHT_COMMAND",
                "FLIGHT_CONTROLLER",
                &[FlightComputerCommands::Launch as u8],
            )?,
            build_launch_packet(
                &router,
                "VALVE_COMMAND",
                "VALVE_BOARD",
                &[ValveBoardCommands::Sequence as u8],
            )?,
        ])
    })();
    let packets = match packets {
        Ok(packets) => packets,
        Err(error) => {
            emit_notification_warning(
                &state,
                format!("Launch packet preparation failed; inhibition retained: {error}"),
            );
            return;
        }
    };
    if let Err(error) = crate::gse::queue_admitted_launch_packets(&state, &router, ticket, packets)
    {
        // An accepted first packet remains an honest SDK owner. Do not retry
        // the second packet or undo durable inhibition after partial admission.
        emit_notification_warning(&state, error);
        flush_command_tx(
            &router,
            "Launch admission failure: progress already accepted owners",
        );
        return;
    }
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    state.begin_daq_launch_log();
    log_command_dispatch(
        "Launch command",
        "broadcast",
        crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
        &[FlightComputerCommands::Launch as u8],
    );
    log_command_dispatch(
        "Valve launch sequence command",
        "umbilical_comms",
        crate::telemetry_schema::data_type("VALVE_COMMAND"),
        &[ValveBoardCommands::Sequence as u8],
    );
    flush_command_tx(&router, "FC and valve Launch command tx");
    let flight_command_sent = true;
    let valve_command_sent = true;
    if flight_command_sent || valve_command_sent {
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        if pressed.is_none() {
            crate::auto_zero::launch(&state);
        }
        state.set_launch_indicator_latched(true);
        sequences::refresh_action_policy_now(&state);
        state.broadcast_action_policy_snapshot();
    }
    gs_debug_println!(
        "Launch command sent to flight computer: {}; valve board sequence command sent: {}",
        flight_command_sent,
        valve_command_sent
    );
}

/// A Launch press held behind the opt-in loadcell zero, with everything the
/// worker needs to re-run its send-time checks.
struct HeldLaunch {
    command: TelemetryCommand,
    launch_revision: Option<u64>,
    recovery_origin: Option<crate::operator_queue::manual_origin::Origin>,
    queued_generation: Option<u64>,
    /// Stop/reset ownership at the press; any later Stop or reset refuses it.
    pressed: crate::gse::LaunchAdmission,
}

/// Refusals a Launch press would meet anyway, checked before the zero so a
/// refused Launch never changes the tare. All are checked again at send time.
fn held_launch_precheck(state: &Arc<AppState>) -> Result<(), String> {
    if let Some(reason) =
        crate::checklists::launch_block().filter(|_| state.checklist_enforcement_enabled())
    {
        return Err(reason.to_string());
    }
    if !cfg!(feature = "hitl_mode") && vent_valve_known_open_for_launch(state) {
        return Err(
            "Vent valve is still reported open. Close it before initiating the launch sequence."
                .into(),
        );
    }
    if matches!(
        state.launch_clock_snapshot().kind,
        LaunchClockKind::TMinus | LaunchClockKind::TPlus
    ) {
        return Err("Launch not sent: the T-clock is already running. Use Reset all boards to send Launch again.".into());
    }
    if state.launch_sequence_command_pending.load(Ordering::SeqCst) {
        return Err("Launch not sent: the last Launch is still waiting for the Valve Board to start its sequence. Use Reset all boards to send Launch again.".into());
    }
    if state
        .launch_history
        .lock()
        .unwrap()
        .automatic_actions_inhibited()
    {
        return Err(crate::gse::LAUNCH_LOCKED_MESSAGE.into());
    }
    if crate::gse::turbocharge_active_nowait(state) {
        return Err(crate::gse::TURBOCHARGE_UNFINISHED_MESSAGE.into());
    }
    crate::ignition_pair::readiness(state).map_err(|reason| reason.to_string())
}

/// Zero before Launch (opt-in). Returns true when this press was taken over:
/// it is either refused now or held and sent later by `send_held_launch`.
fn hold_launch_for_loadcell_zero(
    state: &Arc<AppState>,
    router: &Arc<Router>,
    held: HeldLaunch,
) -> bool {
    if !crate::auto_zero::before_launch::enabled(state) {
        return false;
    }
    if crate::auto_zero::before_launch::held(state) {
        emit_notification_warning(state, crate::auto_zero::before_launch::ALREADY_PENDING);
        return true;
    }
    if let Err(reason) = held_launch_precheck(state) {
        emit_notification_warning(state, reason);
        sequences::refresh_action_policy_now(state);
        state.broadcast_action_policy_snapshot();
        return true;
    }
    let owned_state = state.clone();
    let owned_router = router.clone();
    let started = crate::auto_zero::before_launch::start(state, move |outcome| async move {
        if let crate::auto_zero::before_launch::Outcome::Failed(reason) = &outcome {
            emit_notification_warning(
                &owned_state,
                format!(
                    "Loadcell zero before Launch failed: {reason}. Sending Launch with the PREVIOUS loadcell calibration."
                ),
            );
        }
        send_held_launch(owned_state, owned_router, held).await;
    });
    if let Err(reason) = started {
        emit_notification_warning(state, reason);
    }
    true
}

/// The worker's own send-time checks, re-run for a held press, then the
/// ordinary Launch path with the press-time Stop/reset ticket.
async fn send_held_launch(state: Arc<AppState>, router: Arc<Router>, held: HeldLaunch) {
    #[cfg(test)]
    held_launch_tests::entered();
    let refuse = |reason: String| {
        emit_notification_warning(&state, format!("Held Launch not sent: {reason}"));
        sequences::refresh_action_policy_now(&state);
        state.broadcast_action_policy_snapshot();
    };
    let _operator_claim = match held.queued_generation {
        Some(generation) => {
            match crate::gse::claim_manual_dispatch(&state, generation, held.recovery_origin) {
                Some(claim) => Some(claim),
                None => return refuse("a reset or recovery closed command admission".into()),
            }
        }
        None => None,
    };
    let Some(cmd) =
        command_at_dispatch_with_manual_origin(&state, held.command, held.recovery_origin)
    else {
        return refuse("command admission is closed".into());
    };
    if let Err(error) = crate::board_abort::validate_launch_revision(&state, held.launch_revision) {
        return refuse(error);
    }
    if !state.is_command_allowed(&cmd) {
        return refuse(format!("Command {cmd:?} blocked by sequence/key interlock"));
    }
    if crate::gse::manual_stop_pending(&state) {
        return refuse("Manual Stop is pending".into());
    }
    #[cfg(test)]
    held_launch_tests::before_send(&state);
    match cmd {
        TelemetryCommand::Launch => {
            #[cfg(all(feature = "test_fire_mode", not(feature = "hitl_mode")))]
            send_local_ground_station_launch(state, router, held.launch_revision, Some(held.pressed)).await;
            #[cfg(not(all(feature = "test_fire_mode", not(feature = "hitl_mode"))))]
            send_flight_computer_launch(state, router, held.launch_revision, Some(held.pressed)).await;
        }
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        TelemetryCommand::GroundStationLaunch => {
            send_local_ground_station_launch(state, router, held.launch_revision, Some(held.pressed)).await;
        }
        _ => {}
    }
}

/// Explicit inner-worker outcome. An outer Tokio join success is insufficient:
/// blocking worker joins can time out while the original producer remains alive.
#[derive(Clone, Copy, Debug)]
pub struct TelemetryShutdownOutcome {
    pub(crate) ingress_quiesced: bool,
}

pub async fn telemetry_task(
    state: Arc<AppState>,
    router: Arc<Router>,
    comms: Vec<CommsWorkerHandle>,
    mut rx: mpsc::Receiver<TelemetryCommand>,
    db_rx: mpsc::Receiver<DbQueueItem>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> TelemetryShutdownOutcome {
    crate::ignition_pair::observer::start(&state);
    let mut operator_queue_rx = state
        .operator_queue
        .take_receiver()
        .expect("operator queue receiver already owned");
    let mut handle_interval = interval(Duration::from_millis(1));
    let heartbeat_period_ms = std::env::var("GS_HEARTBEAT_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value >= 100)
        .unwrap_or(500);
    let mut heartbeat_cadence = periodic::ProtocolCadence::new(heartbeat_period_ms);
    let heartbeat_poll_ms = if crate::simulation_clock_ms().is_some() {
        1
    } else {
        heartbeat_period_ms
    };
    let mut heartbeat_interval = interval(Duration::from_millis(heartbeat_poll_ms));
    handle_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_failed = false;
    let mut last_backpressure_log_ms: u64 = 0;
    let mut packet_worker_failure_reported = false;
    let packet_work_queue_size = env_usize(
        "GS_PACKET_WORK_QUEUE_SIZE",
        PACKET_WORK_QUEUE_SIZE,
        1024,
        262_144,
    );
    let packet_enqueue_burst = env_usize("GS_PACKET_ENQUEUE_BURST", PACKET_ENQUEUE_BURST, 32, 4096);
    let (packet_tx, mut packet_rx) =
        mpsc::channel::<crate::ring_buffer::ReceivedPacket>(packet_work_queue_size);
    let db_overflow = DbOverflow;

    let db_worker = {
        let state = state.clone();
        let db_batch_max = env_usize("GS_DB_BATCH_MAX", DB_BATCH_MAX_DEFAULT, 1, 4096);
        let db_batch_wait_ms = std::env::var("GS_DB_BATCH_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DB_BATCH_WAIT_MS_DEFAULT)
            .clamp(1, 250);
        let db_shutdown_rx = state.shutdown_subscribe();
        tokio::spawn(recording_owner::run(
            state,
            db_rx,
            db_batch_max,
            db_batch_wait_ms,
            db_shutdown_rx,
        ))
    };

    let packet_worker = {
        let state = state.clone();
        let db_tx = state.db_queue_tx.clone();
        let db_overflow = db_overflow.clone();
        tokio::spawn(async move {
            while let Some(pkt) = packet_rx.recv().await {
                let rows = match pkt {
                    crate::ring_buffer::ReceivedPacket::Telemetry { packet: pkt, ingress_generation } => {
                        #[cfg(test)]
                        queued_phase_probe::before_bulk_entry(&state, &pkt, ingress_generation).await;
                        #[cfg(test)]
                        let probe_packet_id = pkt.packet_id();
                        let rows = handle_packet_at_ingress(&state, &db_tx, &db_overflow, pkt, ingress_generation).await;
                        #[cfg(test)]
                        queued_phase_probe::after_bulk_entry(&state, probe_packet_id);
                        rows
                    }
                    crate::ring_buffer::ReceivedPacket::AppliedStatus { packet, row } => {
                        persist_umbilical_status(&state, &db_tx, &db_overflow, &packet, &row).await;
                        Vec::new()
                    }
                };
                publish_received_telemetry_rows(&state, rows);
            }
        })
    };

    #[cfg(all(test, feature = "test_fire_mode"))]
    crate::recovery_pending_probe::packet_worker_registered(&state, packet_worker.abort_handle());
    let mut worker_startup_failed = false;
    let router_worker = match spawn_router_worker_thread(router.clone(), state.clone()) {
        Ok(worker) => Some(worker),
        Err(err) => {
            worker_startup_failed = true;
            eprintln!("Failed to spawn router worker thread: {err}");
            None
        }
    };

    let comms_workers: Vec<_> = comms
        .into_iter()
        .flat_map(|comms_handle| {
            let router = router.clone();
            let state = state.clone();
            spawn_comms_worker_threads(router, state, comms_handle).unwrap_or_else(|err| {
                worker_startup_failed = true;
                eprintln!("Failed to spawn comms worker thread: {err}");
                Vec::new()
            })
        })
        .collect();

    loop {
        tokio::select! {
            biased;

                Some(request) = operator_queue_rx.recv() => {
                    crate::operator_queue::serve(&state, &mut rx, request);
                }
                Some(cmd) = rx.recv() => {
                    if crate::fill_recording::dispatch_queued(&state, &cmd) { continue; }
                    if let TelemetryCommand::ColdFlowPanelRelease{key,generation,awaiting_at_edge}=cmd {
                        crate::gse::cold_flow::dispatch_panel(&state,crate::gse::cold_flow::PanelIntent{key,generation,awaiting_at_edge}).await;
                        continue;
                    }
                    let recovery_origin = match &cmd {
                        TelemetryCommand::QueuedOperator {recovery_origin,..}=>*recovery_origin,
                        _=>None,
                    };
                    let (cmd, _operator_claim, plumbing_origin, launch_revision, queued_generation) = match cmd {
                        TelemetryCommand::QueuedOperator { recovery_origin, launch_revision, generation, command, plumbing_origin } => {
                            if command.automatic_generation().is_some() || matches!(*command, TelemetryCommand::QueuedOperator { .. }) { continue; }
                            let Some(claim) = crate::gse::claim_manual_dispatch(&state, generation, recovery_origin) else { continue; };
                            (*command, Some(claim), plumbing_origin, launch_revision, Some(generation))
                        }
                        command => (command, None, None, None, None),
                    };
                    let sealed_phase=matches!(cmd,TelemetryCommand::OperatorPhase{..});
                    let cmd = if let TelemetryCommand::OperatorPhase { mut intent } = cmd {
                        if let Some(origin)=recovery_origin
                            && !intent.bind_manual_claim(&state,&origin) {continue;}
                        if !intent.validate(&state) {emit_notification_warning(&state,"Phase request changed or expired; choose a new phase action.");continue;}
                        if intent.local() {
                            if let Err(error)=crate::operator_phase::apply(&state,&intent){emit_notification_warning(&state,error);}
                            continue;
                        }
                        #[cfg(feature="hitl_mode")] {intent.original_command()}
                        #[cfg(not(feature="hitl_mode"))] {continue;}
                    } else {cmd};
                    #[cfg(feature="hitl_mode")]
                    if !sealed_phase && matches!(cmd,TelemetryCommand::AdvanceFlightState|TelemetryCommand::RewindFlightState) {
                        emit_notification_warning(&state,"Phase request has no original authority; make a new request from current primary controls.");continue;
                    }
                    #[cfg(not(feature="hitl_mode"))] let _=sealed_phase;
                    if let TelemetryCommand::FillWithRecording { origin } = cmd {
                        crate::fill_recording::spawn(state.clone(), origin);
                        continue;
                    }
                    if matches!(cmd, TelemetryCommand::StartFill) {
                        crate::fill_recording::notify_failure(&state, &crate::fill_recording::Error::Stale);
                        continue;
                    }
                    let automatic_generation = cmd.automatic_generation();
                    let automatic_supply_close = matches!(cmd, TelemetryCommand::AutomaticNitrogenClose { .. } | TelemetryCommand::AutomaticNitrousClose { .. });
                    let automatic_abort = matches!(cmd, TelemetryCommand::AutomaticAbort { .. });
                    let Some(cmd) = command_at_dispatch_with_manual_origin(&state, cmd, recovery_origin) else {
                        continue;
                    };
                    let _automatic_abort_claim = if automatic_abort {
                        let Some(claim) = crate::gse::claim_automatic_abort_for_generation(&state, automatic_generation.expect("automatic Abort retains original generation")) else { continue; };
                        Some(claim)
                    } else { None };
                    if matches!(cmd, TelemetryCommand::ResetSim) {
                        reset_testing_simulation(&state).await;
                        continue;
                    }
                    if crate::board_abort::launch_command(crate::sequences::command_name(&cmd))
                        && let Err(error)=crate::board_abort::validate_launch_revision(&state,launch_revision) {
                            emit_notification_warning(&state,error);continue;
                        }
                    if !state.is_command_allowed(&cmd) {
                        emit_notification_warning(
                            &state,
                            format!("Command {cmd:?} blocked by sequence/key interlock"),
                        );
                        continue;
                    }
                    if automatic_supply_close {
                        let (key, close) = if matches!(cmd, TelemetryCommand::NitrogenClose) {
                            (ActuatorBoardCommands::NitrogenOpen as u8, ActuatorBoardCommands::NitrogenClose as u8)
                        } else {
                            (ActuatorBoardCommands::NitrousOpen as u8, ActuatorBoardCommands::NitrousClose as u8)
                        };
                        match crate::gse::queue_automatic_supply_close(&state, &router, close, automatic_generation.expect("automatic close has generation")) {
                            Ok(true) => {
                                state.set_pending_umbilical_valve_state(key, false);
                                flush_command_tx(&router, "Automatic supply close");
                            }
                            Ok(false) => {}
                            Err(error) => log_telemetry_error("Automatic supply close", error),
                        }
                        continue;
                    }
                    if crate::gse::manual_stop_pending(&state)
                        && !matches!(cmd, TelemetryCommand::Abort | TelemetryCommand::NitrogenClose | TelemetryCommand::NitrousClose | TelemetryCommand::PauseFill | TelemetryCommand::CancelFill)
                        && crate::sequences::command_name(&cmd) != "EvaluationAbort"
                        && !crate::sequences::is_station_utility_command(crate::sequences::command_name(&cmd)) {
                        emit_notification_warning(&state, "Manual Stop is pending; new control action was not dispatched");
                        continue;
                    }
                    if matches!(cmd, TelemetryCommand::RetractPlumbing)
                        && let Err(message) = crate::actuator_plumbing::validate_motion_origin(&state, plumbing_origin) {
                            emit_notification_warning(&state, &message);
                            continue;
                        }
                    state.record_command_accepted(&cmd, get_current_timestamp_ms());
                    #[cfg(feature="hitl_mode")]
                    if let Some(index)=match &cmd {
                        TelemetryCommand::TogglePlumbingDelayInterlock=>Some(0),
                        TelemetryCommand::ToggleChecklistInterlock=>Some(1),
                        TelemetryCommand::ToggleSupplyInterlock=>Some(2),
                        TelemetryCommand::ToggleBoardStateProofInterlock=>Some(3),
                        _=>None,
                    } {
                        // Policy-only: keep an existing capture/fill untouched.
                        // The actual flag mutation advances Launch authority.
                        state.toggle_hitl_safety_interlock(index);
                        sequences::refresh_action_policy_now(&state);
                        state.broadcast_action_policy_snapshot();
                        continue;
                    }
                    if crate::gse::consume_fill_noop(&state, &cmd) {
                        continue;
                    }
                    state.auto_zero.lock().unwrap().cancel_for_command(&cmd);
                    if crate::auto_zero::before_fill(&state, &cmd) { continue; }
                    #[cfg(feature="testing")]
                    if matches!(cmd,TelemetryCommand::RetractPlumbing)&&flight_sim::sim_mode_enabled() {
                        if let Err(error)=crate::gse::admit_simulated_retraction_stamped(&state,plumbing_origin,|source,observed|flight_sim::handle_command_stamped(&cmd,source,observed)) {
                            emit_notification_warning(&state,error);
                        }
                        continue;
                    }
                    let gse_handled = crate::gse::handle_command_with_origins(&state, &cmd, plumbing_origin, launch_revision);
                    if gse_handled {
                        continue;
                    }
                    if matches!(cmd, TelemetryCommand::Abort) {
                        state.set_abort_indicator_latched(true);
                        sequences::refresh_action_policy_now(&state);
                        state.broadcast_action_policy_snapshot();
                    }
                    #[cfg(feature = "testing")]
                    if matches!(crate::sequences::command_name(&cmd), "Launch" | "GroundStationLaunch") && flight_sim::sim_mode_enabled() {
                        let original_utc=match crate::system_clock::required_control_network_utc_ms(&router){Ok(value)=>value,Err(error)=>{emit_notification_warning(&state,error.to_string());continue;}};
                        let ticket = match crate::gse::inhibit_automatic_actions_for_launch_revision(&state,launch_revision) {
                            Ok(ticket) => ticket,
                            Err(error) => { emit_notification_warning(&state, error); continue; }
                        };
                        let observed_utc=match crate::system_clock::required_control_network_utc_ms(&router){Ok(value)=>value,Err(error)=>{emit_notification_warning(&state,error.to_string());continue;}};
                        if let Err(error) = crate::gse::admit_simulated_launch(&state, ticket, || flight_sim::handle_command_stamped(&cmd,original_utc,observed_utc)) {
                            emit_notification_warning(&state, error);
                        }
                        // Even a declined model command cannot fall through
                        // into physical dispatch after durable admission.
                        continue;
                    }
                    if flight_sim::handle_command(&cmd) {
                        continue;
                    }
                    match cmd {
                        TelemetryCommand::OperatorPhase { .. } | TelemetryCommand::FillWithRecording { .. } | TelemetryCommand::ColdFlowPanelRelease { .. } | TelemetryCommand::QueuedOperator { .. } | TelemetryCommand::AutomaticAbort { .. } | TelemetryCommand::AutomaticNitrogenClose { .. } | TelemetryCommand::AutomaticNitrousClose { .. } => unreachable!("automatic intent checked before dispatch"),
                        TelemetryCommand::StartFill | TelemetryCommand::PauseFill
                        | TelemetryCommand::CancelFill | TelemetryCommand::ValveSelfTest
                        | TelemetryCommand::NitrogenTest | TelemetryCommand::Turbocharge => unreachable!("GSE commands handled before dispatch"),
                        TelemetryCommand::Dump => {
                                let key = ValveBoardCommands::DumpOpen as u8;
                                let is_on = effective_umbilical_valve_state(&state, key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::DumpClose
                                } else {
                                    ValveBoardCommands::DumpOpen
                                };
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("VALVE_COMMAND"),
                                    key,
                                    !is_on,
                                    cmd as u8,
                                    "Dump command",
                                );
                                gs_debug_println!("Dump command sent {:?}", cmd);
                            }
                        TelemetryCommand::Abort => {
                                state.clear_launch_sequence_command_pending();
                                let queued = if automatic_abort {
                                    crate::gse::queue_automatic_abort_for_generation(&state, &router,
                                        automatic_generation.expect("automatic abort has generation"),
                                        "Automatic zero failure abort")
                                } else {
                                    queue_abort_packet(&router, "Manual Abort Command Issued").map(|()| true)
                                };
                                if matches!(queued, Ok(false)) { continue; }
                                if let Err(e) = queued {
                                    log_telemetry_error("failed to log Abort command", e);
                                } else {
                                    flush_command_tx(&router, "Abort command tx");
                                    gs_debug_println!("Abort command accepted for delivery");
                                }
                                emit_error(&state, if automatic_abort { "Automatic zero failure abort triggered!" } else { "Manual abort triggered!" }.to_string());

                            }
                        TelemetryCommand::Igniter => {
                                let key = ActuatorBoardCommands::IgniterOn as u8;
                                let is_on = effective_umbilical_valve_state(&state, key).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::IgniterOff
                                } else {
                                    ActuatorBoardCommands::IgniterOn
                                };
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                                    key,
                                    !is_on,
                                    cmd as u8,
                                    "Igniter command",
                                );
                                gs_debug_println!("Igniter command sent {:?}", cmd);
                            }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::IgniterSequence => {
                                let payload = [ActuatorBoardCommands::IgniterSequence as u8];
                                match crate::gse::queue_controller_with_stop_fence(&state, &router, crate::telemetry_schema::data_type("ACTUATOR_COMMAND"), &payload) {
                                  Err(e) => log_telemetry_error("failed to log IgniterSequence command", e),
                                  Ok(false) => emit_notification_warning(&state, "Manual Stop is pending; igniter sequence not queued"),
                                  Ok(true) => {
                                    log_command_dispatch(
                                        "IgniterSequence command",
                                        "umbilical_comms",
                                        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                                        &payload,
                                    );
                                    flush_command_tx(&router, "IgniterSequence command tx");
                                  }
                                }
                                gs_debug_println!("IgniterSequence command processed");
                            }
                        TelemetryCommand::Pilot => {
                                let key = ValveBoardCommands::PilotOpen as u8;
                                let is_on = effective_umbilical_valve_state(&state, key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::PilotClose
                                } else {
                                    ValveBoardCommands::PilotOpen
                                };
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("VALVE_COMMAND"),
                                    key,
                                    !is_on,
                                    cmd as u8,
                                    "Pilot command",
                                );
                                gs_debug_println!("Pilot command sent {:?}", cmd);
                            }
                        TelemetryCommand::NormallyOpen => {
                                let key = ValveBoardCommands::NormallyOpenOpen as u8;
                                let effective_state = effective_umbilical_valve_state(&state, key);
                                let should_close_for_armed_launch = effective_state.is_none()
                                    && state.local_flight_state_snapshot() == FlightState::Armed;
                                let is_on = effective_state.unwrap_or(false);
                                let target = !(is_on || should_close_for_armed_launch);
                                let cmd = if target {
                                    ValveBoardCommands::NormallyOpenOpen
                                } else {
                                    ValveBoardCommands::NormallyOpenClose
                                };
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("VALVE_COMMAND"),
                                    key,
                                    target,
                                    cmd as u8,
                                    "NormallyOpen command",
                                );
                                gs_debug_println!("Tanks command sent {:?}", cmd);
                            }
                        TelemetryCommand::Nitrogen => {
                                let cmd_id = ActuatorBoardCommands::NitrogenOpen as u8;
                                let is_on = effective_umbilical_valve_state(&state, cmd_id).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::NitrogenClose
                                } else {
                                    ActuatorBoardCommands::NitrogenOpen
                                };
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                                    cmd_id,
                                    !is_on,
                                    cmd as u8,
                                    "Nitrogen command",
                                );
                                gs_debug_println!("Nitrogen command sent {:?}", cmd);
                            }
                        TelemetryCommand::NitrogenClose => {
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                                    ActuatorBoardCommands::NitrogenOpen as u8,
                                    false,
                                    ActuatorBoardCommands::NitrogenClose as u8,
                                    "NitrogenClose command",
                                );
                                gs_debug_println!("Nitrogen explicit close command sent");
                            }
                        TelemetryCommand::RetractPlumbing => {
                            queue_guarded_plumbing_motion(&state, &router, plumbing_origin);
                        }
                        TelemetryCommand::Nitrous => {
                                let cmd_id = ActuatorBoardCommands::NitrousOpen as u8;
                                let is_on = effective_umbilical_valve_state(&state, cmd_id).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::NitrousClose
                                } else {
                                    ActuatorBoardCommands::NitrousOpen
                                };
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                                    cmd_id,
                                    !is_on,
                                    cmd as u8,
                                    "Nitrous command",
                                );
                                gs_debug_println!("Nitrous command sent: {:?}", cmd);
                            }
                        TelemetryCommand::NitrousClose => {
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                                    ActuatorBoardCommands::NitrousOpen as u8,
                                    false,
                                    ActuatorBoardCommands::NitrousClose as u8,
                                    "NitrousClose command",
                                );
                                gs_debug_println!("Nitrous explicit close command sent");
                            }
                        TelemetryCommand::StartWritingNow => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::StartNow)).await;
                                gs_debug_println!("DB recording started without backfill");
                            }
                        TelemetryCommand::StartWritingLastTwoMinutes => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::StartWithRecent)).await;
                                gs_debug_println!("DB recording started with recent backfill");
                            }
                        TelemetryCommand::PauseWritingDb => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::Pause)).await;
                                gs_debug_println!("DB recording paused");
                            }
                        TelemetryCommand::StopWritingDb => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::Stop)).await;
                                gs_debug_println!("DB recording stopped");
                            }
                        TelemetryCommand::ResetSim => {}
                        TelemetryCommand::ContinueFillSequence => {
                                state.request_fill_sequence_continue();
                                gs_debug_println!("ContinueFillSequence command accepted");
                            }
                        TelemetryCommand::Postinit => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "Postinit command",
                                    &[FlightComputerCommands::Postinit as u8],
                                ) {
                                    log_telemetry_error("failed to log Postinit command", e);
                                } else {
                                    flush_command_tx(&router, "Postinit command tx");
                                }
                                gs_debug_println!("Postinit command sent to flight computer");
                            }
                        TelemetryCommand::Launch => {
                                if hold_launch_for_loadcell_zero(&state, &router, HeldLaunch {
                                    command: TelemetryCommand::Launch, launch_revision, recovery_origin, queued_generation,
                                    pressed: crate::gse::LaunchAdmission::capture(&state),
                                }) { continue; }
                                #[cfg(all(feature = "test_fire_mode", not(feature = "hitl_mode")))]
                                handle_local_ground_station_launch_command(state.clone(), router.clone(),launch_revision).await;
                                #[cfg(not(all(feature = "test_fire_mode", not(feature = "hitl_mode"))))]
                                handle_flight_computer_launch_command(state.clone(), router.clone(),launch_revision).await;
                            }
                        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
                        TelemetryCommand::GroundStationLaunch => {
                                if hold_launch_for_loadcell_zero(&state, &router, HeldLaunch {
                                    command: TelemetryCommand::GroundStationLaunch, launch_revision, recovery_origin, queued_generation,
                                    pressed: crate::gse::LaunchAdmission::capture(&state),
                                }) { continue; }
                                handle_local_ground_station_launch_command(state.clone(), router.clone(),launch_revision).await;
                            }
                        TelemetryCommand::MonitorAltitude => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "MonitorAltitude command",
                                    &[FlightComputerCommands::MonitorAltitude as u8],
                                ) {
                                    log_telemetry_error("failed to log MonitorAltitude command", e);
                                }
                                gs_debug_println!("MonitorAltitude command sent");
                            }
                        TelemetryCommand::RevokeMonitorAltitude => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "RevokeMonitorAltitude command",
                                    &[FlightComputerCommands::RevokeMonitorAltitude as u8],
                                ) {
                                    log_telemetry_error("failed to log RevokeMonitorAltitude command", e);
                                }
                                gs_debug_println!("RevokeMonitorAltitude command sent");
                            }
                        TelemetryCommand::ConsecutiveSamples => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "ConsecutiveSamples command",
                                    &[FlightComputerCommands::ConsecutiveSamples as u8],
                                ) {
                                    log_telemetry_error("failed to log ConsecutiveSamples command", e);
                                }
                                gs_debug_println!("ConsecutiveSamples command sent");
                            }
                        TelemetryCommand::RevokeConsecutiveSamples => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "RevokeConsecutiveSamples command",
                                    &[FlightComputerCommands::RevokeConsecutiveSamples as u8],
                                ) {
                                    log_telemetry_error("failed to log RevokeConsecutiveSamples command", e);
                                }
                                gs_debug_println!("RevokeConsecutiveSamples command sent");
                            }
                        TelemetryCommand::ResetFailures => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "ResetFailures command",
                                    &[FlightComputerCommands::ResetFailures as u8],
                                ) {
                                    log_telemetry_error("failed to log ResetFailures command", e);
                                }
                                gs_debug_println!("ResetFailures command sent");
                            }
                        TelemetryCommand::RevokeResetFailures => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "RevokeResetFailures command",
                                    &[FlightComputerCommands::RevokeResetFailures as u8],
                                ) {
                                    log_telemetry_error("failed to log RevokeResetFailures command", e);
                                }
                                gs_debug_println!("RevokeResetFailures command sent");
                            }
                        TelemetryCommand::ValidateMeasms => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "ValidateMeasms command",
                                    &[FlightComputerCommands::ValidateMeasms as u8],
                                ) {
                                    log_telemetry_error("failed to log ValidateMeasms command", e);
                                }
                                gs_debug_println!("ValidateMeasms command sent");
                            }
                        TelemetryCommand::RevokeValidateMeasms => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "RevokeValidateMeasms command",
                                    &[FlightComputerCommands::RevokeValidateMeasms as u8],
                                ) {
                                    log_telemetry_error("failed to log RevokeValidateMeasms command", e);
                                }
                                gs_debug_println!("RevokeValidateMeasms command sent");
                            }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ToggleButtonInterlock => {
                                let enabled = state.toggle_hitl_button_interlock();
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("HITL button interlock toggled: {enabled}");
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ToggleGroundStationControl => {
                            // Consumed by gse::handle_command before this match.
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ToggleLaunchInterlock => {
                                let enabled = state.toggle_hitl_launch_interlock();
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("HITL launch interlock toggled: {enabled}");
                        }
                        #[cfg(feature="hitl_mode")]
                        TelemetryCommand::TogglePlumbingDelayInterlock
                        |TelemetryCommand::ToggleChecklistInterlock
                        |TelemetryCommand::ToggleSupplyInterlock
                        |TelemetryCommand::ToggleBoardStateProofInterlock=>unreachable!("pure policy handled before capture/GSE handoff"),
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::TogglePhysicalLaunchMode => {
                                let uses_gs = state.toggle_hitl_physical_launch_mode();
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("HITL physical launch mode toggled: uses_ground_station={uses_gs}");
                        }
                        TelemetryCommand::ToggleAvBayUnderglow => {
                            match crate::network_variables::toggle_underglow(&router) {
                                Ok(enabled) => {
                                    sequences::refresh_action_policy_now(&state);
                                    state.broadcast_action_policy_snapshot();
                                    gs_debug_println!("AV bay underglow network variable toggled: {enabled}");
                                }
                                Err(err) => {
                                    emit_notification_warning(
                                        &state,
                                        format!("Failed to persist or publish AV bay underglow: {err}"),
                                    );
                                }
                            }
                        }
                        TelemetryCommand::ToggleFlightBuzzer => {
                            match crate::network_variables::toggle_flight_buzzer(&router) {
                                Ok(enabled) => {
                                    sequences::refresh_action_policy_now(&state);
                                    state.broadcast_action_policy_snapshot();
                                    gs_debug_println!("Flight buzzer network variable toggled: {enabled}");
                                }
                                Err(err) => {
                                    emit_notification_warning(
                                        &state,
                                        format!("Failed to persist or publish Flight buzzer: {err}"),
                                    );
                                }
                            }
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ResetLaunchLatch => {
                                state.set_launch_indicator_latched(false);
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("Launch indicator latch reset");
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ResetFlightState => {
                            reset_operator_flight_state(&state);
                        }
                        TelemetryCommand::ResetTClock => {
                            state.reset_launch_clock();
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::AdvanceFlightState => {
                            let current = *state.state.lock().unwrap();
                            let next = operator_mode_adjacent_flight_state(current, 1);
                            if let Some(cmd_id) = hitl_flight_command_id(&cmd) {
                                if let Err(e) = queue_locally_routed_flight_command_with_launch_revision(
                                    &state,
                                    &router,
                                    "AdvanceFlightState command",
                                    &[cmd_id],launch_revision,
                                ) {
                                    log_telemetry_error("failed to log AdvanceFlightState command", e);
                                } else {
                                    flush_command_tx(&router, "AdvanceFlightState command tx");
                                }
                                gs_debug_println!("AdvanceFlightState command sent to flight computer ({cmd_id})");
                            }
                            set_local_flight_state_for_operator_mode(&state, next,launch_revision).await;
                            gs_debug_println!("Operator-mode flight state advanced: {:?} -> {:?}", current, next);
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::RewindFlightState => {
                            let current = *state.state.lock().unwrap();
                            let next = operator_mode_adjacent_flight_state(current, -1);
                            if let Some(cmd_id) = hitl_flight_command_id(&cmd) {
                                if let Err(e) = queue_locally_routed_flight_command_with_launch_revision(
                                    &state,
                                    &router,
                                    "RewindFlightState command",
                                    &[cmd_id],launch_revision,
                                ) {
                                    log_telemetry_error("failed to log RewindFlightState command", e);
                                } else {
                                    flush_command_tx(&router, "RewindFlightState command tx");
                                }
                                gs_debug_println!("RewindFlightState command sent to flight computer ({cmd_id})");
                            }
                            set_local_flight_state_for_operator_mode(&state, next,launch_revision).await;
                            gs_debug_println!("Operator-mode flight state rewound: {:?} -> {:?}", current, next);
                        }
                        #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
                        TelemetryCommand::AdvanceFlightState => {
                                let current = *state.state.lock().unwrap();
                                let next = operator_mode_adjacent_flight_state(current, 1);
                                set_local_flight_state_for_operator_mode(&state, next,launch_revision).await;
                                gs_debug_println!("Operator-mode flight state advanced: {:?} -> {:?}", current, next);
                        }
                        #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
                        TelemetryCommand::RewindFlightState => {
                                let current = *state.state.lock().unwrap();
                                let next = operator_mode_adjacent_flight_state(current, -1);
                                set_local_flight_state_for_operator_mode(&state, next,launch_revision).await;
                                gs_debug_println!("Operator-mode flight state rewound: {:?} -> {:?}", current, next);
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::DeployParachute
                        | TelemetryCommand::ExpandParachute
                        | TelemetryCommand::EvaluationRelax
                        | TelemetryCommand::EvaluationFocus
                        | TelemetryCommand::EvaluationAbort
                        | TelemetryCommand::ReinitSensors
                        | TelemetryCommand::ReinitBarometer
                        | TelemetryCommand::EnableIMU
                        | TelemetryCommand::DisableIMU
                        | TelemetryCommand::AbortAfter100
                        | TelemetryCommand::AbortAfter40
                        | TelemetryCommand::AbortAfter250
                        | TelemetryCommand::ReinitAfter15
                        | TelemetryCommand::ReinitAfter30
                        | TelemetryCommand::ReinitAfter50 => {
                            if let Some(cmd_id) = hitl_flight_command_id(&cmd) {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &state,
                                    &router,
                                    "HITL flight command",
                                    &[cmd_id],
                                ) {
                                    log_telemetry_error("failed to log HITL flight command", e);
                                } else {
                                    flush_command_tx(&router, "HITL flight command tx");
                                }
                                gs_debug_println!("HITL flight command sent: {:?} ({cmd_id})", cmd);
                            }
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    if !heartbeat_cadence.due(crate::simulation_clock_ms()) { continue; }
                    // Retry only failed durable admission; stable clocks emit nothing.
                    let _ = crate::network_variables::retry_pending_daq_log_clock_for_state(&state, &router);
                    let heartbeat_accepted=heartbeat::queue_station_heartbeat(&router).is_ok();
                    let evidence=crate::alert_condition_producers::heartbeat_enqueue(heartbeat_accepted);
                    let heartbeat_condition=crate::alert_condition_runtime::observe(&state,evidence.key,evidence.state,evidence.message,get_current_timestamp_ms() as i64);
                    crate::alert_condition_runtime::request_repair(&state);
                    if heartbeat_accepted {
                        state.mark_board_seen(
                            Board::GroundStation.sender_id(),
                            get_current_timestamp_ms(),
                        );
                        heartbeat_failed = false;
                    } else if !heartbeat_failed {
                            crate::web::emit_warning_linked(
                                &state,
                                "Warning: Ground Station heartbeat send failed",
                                heartbeat_condition,
                            );
                            heartbeat_failed = true;

                    }
                }
                _ = handle_interval.tick() => {
                    if crate::network_variables::reconcile_owner_outputs_for_state(&state, &router) {
                        sequences::refresh_action_policy_now(&state);
                        state.broadcast_action_policy_snapshot();
                    }
                    for _ in 0..packet_enqueue_burst {
                        match packet_tx.try_reserve() {
                            Ok(permit) => {
                                let pkt = {
                                    let mut rb = state.ring_buffer.lock().unwrap();
                                    rb.pop_oldest()
                                };
                                let Some(pkt) = pkt else {
                                    break;
                                };
                                permit.send(pkt);
                            }
                            Err(TrySendError::Full(_)) => {
                                let now_ms = get_current_timestamp_ms();
                                if now_ms.saturating_sub(last_backpressure_log_ms)
                                    >= BACKPRESSURE_LOG_INTERVAL_MS
                                {
                                    eprintln!(
                                        "Telemetry ingest backpressured: processing queue is full"
                                    );
                                    last_backpressure_log_ms = now_ms;
                                }
                                break;
                            }
                            Err(TrySendError::Closed(_)) => {
                                if !packet_worker_failure_reported {
                                    emit_warning(
                                        &state,
                                        "Warning: telemetry processing worker stopped unexpectedly; restart backend",
                                    );
                                    packet_worker_failure_reported = true;
                                }
                                break;
                            }
                        }
                    }
                }
                recv = shutdown_rx.recv() => {
                    match recv {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
        }
    }

    let worker_shutdown_timeout = Duration::from_secs(10);
    let mut shutdown_outcome = TelemetryShutdownOutcome {
        ingress_quiesced: !worker_startup_failed,
    };

    for worker in comms_workers {
        match tokio::time::timeout(
            worker_shutdown_timeout,
            tokio::task::spawn_blocking(move || worker.join()),
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => {
                shutdown_outcome.ingress_quiesced = false;
                eprintln!("Comms worker panicked during shutdown");
            }
            Ok(Err(e)) => {
                shutdown_outcome.ingress_quiesced = false;
                eprintln!("Comms worker join task failed: {e}");
            }
            Err(_) => {
                shutdown_outcome.ingress_quiesced = false;
                eprintln!(
                    "Comms worker did not shut down within {:?}",
                    worker_shutdown_timeout
                );
            }
        }
    }

    if let Some(worker) = router_worker {
        match tokio::time::timeout(
            worker_shutdown_timeout,
            tokio::task::spawn_blocking(move || worker.join()),
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => {
                shutdown_outcome.ingress_quiesced = false;
                eprintln!("Router worker panicked during shutdown");
            }
            Ok(Err(e)) => {
                shutdown_outcome.ingress_quiesced = false;
                eprintln!("Router worker join task failed: {e}");
            }
            Err(_) => {
                shutdown_outcome.ingress_quiesced = false;
                eprintln!(
                    "Router worker did not shut down within {:?}",
                    worker_shutdown_timeout
                );
            }
        }
    }

    // Stop intake first, then wait for packet worker to drain packet queue.
    drop(packet_tx);
    match tokio::time::timeout(worker_shutdown_timeout, packet_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            shutdown_outcome.ingress_quiesced = false;
            eprintln!("Packet worker ended with error: {e}");
        }
        Err(_) => {
            shutdown_outcome.ingress_quiesced = false;
            eprintln!(
                "Packet worker did not shut down within {:?}",
                worker_shutdown_timeout
            );
        }
    }

    match tokio::time::timeout(worker_shutdown_timeout, db_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            shutdown_outcome.ingress_quiesced = false;
            eprintln!("DB worker ended with error: {e}");
        }
        Err(_) => {
            shutdown_outcome.ingress_quiesced = false;
            eprintln!(
                "DB worker did not shut down within {:?}",
                worker_shutdown_timeout
            );
        }
    }
    shutdown_outcome
}

fn umbilical_state_key(cmd_id: u8, on: bool) -> Option<(u8, bool)> {
    use ActuatorBoardCommands as A;
    use ValveBoardCommands as V;

    match cmd_id {
        x if x == V::PilotOpen as u8 => Some((V::PilotOpen as u8, on)),
        x if x == V::PilotClose as u8 => Some((V::PilotOpen as u8, false)),
        x if x == V::NormallyOpenOpen as u8 => Some((V::NormallyOpenOpen as u8, on)),
        x if x == V::NormallyOpenClose as u8 => Some((V::NormallyOpenOpen as u8, false)),
        x if x == V::DumpOpen as u8 => Some((V::DumpOpen as u8, on)),
        x if x == V::DumpClose as u8 => Some((V::DumpOpen as u8, false)),
        x if x == A::IgniterOn as u8 => Some((A::IgniterOn as u8, on)),
        x if x == A::IgniterOff as u8 => Some((A::IgniterOn as u8, false)),
        x if x == A::NitrogenOpen as u8 => Some((A::NitrogenOpen as u8, on)),
        x if x == A::NitrogenClose as u8 => Some((A::NitrogenOpen as u8, false)),
        x if x == A::NitrousOpen as u8 => Some((A::NitrousOpen as u8, on)),
        x if x == A::NitrousClose as u8 => Some((A::NitrousOpen as u8, false)),
        x if x == A::RetractPlumbing as u8 => Some((A::RetractPlumbing as u8, on)),
        _ => None,
    }
}

fn effective_umbilical_valve_state(state: &Arc<AppState>, key_cmd_id: u8) -> Option<bool> {
    state
        .get_pending_umbilical_valve_state(key_cmd_id)
        .or_else(|| state.get_umbilical_valve_state(key_cmd_id))
}

fn queue_guarded_plumbing_motion(
    state: &Arc<AppState>,
    router: &Router,
    origin: Option<crate::actuator_plumbing::MotionOrigin>,
) {
    let key = ActuatorBoardCommands::RetractPlumbing as u8;
    if state.get_umbilical_valve_state(key).is_none() {
        emit_notification_warning(
            state,
            "Plumbing state is unconfirmed; wait for fresh board feedback.",
        );
        return;
    }
    if state.get_pending_umbilical_valve_state(key) == Some(true)
        || effective_umbilical_valve_state(state, key) == Some(true)
    {
        return;
    }
    match crate::gse::queue_plumbing_with_stop_fence(state, router, origin) {
        Ok(()) => {}
        Err(message) => {
            emit_notification_warning(state, message);
            return;
        }
    }
    state.set_pending_umbilical_valve_state(key, true);
    flush_command_tx(router, "RetractPlumbing command tx");
    sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
}

fn queue_guarded_fill_command(
    state: &Arc<AppState>,
    router: &Router,
    data_type: DataType,
    key_cmd_id: u8,
    desired_state: bool,
    cmd_payload: u8,
    label: &str,
) {
    if desired_state
        && key_cmd_id != ValveBoardCommands::DumpOpen as u8
        && state.get_umbilical_valve_state(key_cmd_id).is_none()
    {
        emit_warning(
            state,
            "Valve state is unconfirmed; wait for fresh board feedback or use an explicit close/abort action",
        );
        return;
    }
    /* A second operator action must be able to reverse an unacknowledged
     * command. Previously any pending open suppressed the following close,
     * so the UI appeared to work in one direction only whenever the status
     * return path was delayed. Only deduplicate the same target. */
    if state.get_pending_umbilical_valve_state(key_cmd_id) == Some(desired_state) {
        return;
    }
    if effective_umbilical_valve_state(state, key_cmd_id) == Some(desired_state) {
        return;
    }
    match crate::gse::queue_controller_with_stop_fence(state, router, data_type, &[cmd_payload]) {
        Ok(true) => {}
        Ok(false) => {
            emit_notification_warning(
                state,
                "Manual Stop is pending; control command was not queued",
            );
            return;
        }
        Err(err) => {
            log_telemetry_error(&format!("failed to queue {label}"), err);
            return;
        }
    }
    log_command_dispatch(label, "umbilical_comms", data_type, &[cmd_payload]);
    state.set_pending_umbilical_valve_state(key_cmd_id, desired_state);
    flush_command_tx(router, &format!("{label} tx"));
    sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
}

const VALVE_STATE_DATA_TYPE: &str = "VALVE_STATE";

fn bool_to_f32(value: Option<bool>) -> Option<f32> {
    value.map(|v| if v { 1.0 } else { 0.0 })
}

fn valve_state_values(state: &AppState) -> [Option<f32>; 8] {
    use ActuatorBoardCommands as A;
    use ValveBoardCommands as V;

    [
        bool_to_f32(state.get_umbilical_valve_state(V::PilotOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(V::NormallyOpenOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(V::DumpOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::IgniterOn as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::NitrogenOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::NitrousOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::RetractPlumbing as u8)),
        None,
    ]
}

const DB_RETRIES: usize = 5;
const DB_RETRY_DELAY_MS: u64 = 50;

async fn insert_db_batch_once(
    db: &sqlx::SqlitePool,
    writes: &[DbWrite],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    if writes
        .iter()
        .any(|w| matches!(w, DbWrite::Calibration { .. }))
    {
        sqlx::query("CREATE TABLE IF NOT EXISTS calibration_history (timestamp_ms INTEGER NOT NULL, sender_id TEXT NOT NULL, sensor_id TEXT NOT NULL, config_json TEXT NOT NULL, PRIMARY KEY(timestamp_ms,sender_id,sensor_id))").execute(&mut *tx).await?;
    }
    for write in writes {
        match write {
            DbWrite::Calibration {
                timestamp_ms,
                sender_id,
                sensor_id,
                config_json,
            } => {
                sqlx::query("INSERT OR REPLACE INTO calibration_history VALUES (?,?,?,?)")
                    .bind(timestamp_ms)
                    .bind(sender_id)
                    .bind(sensor_id)
                    .bind(config_json)
                    .execute(&mut *tx)
                    .await?;
            }
            DbWrite::FlightState {
                timestamp_ms,
                state_code,
            } => {
                sqlx::query("INSERT INTO flight_state (timestamp_ms, f_state) VALUES (?, ?)")
                    .bind(*timestamp_ms)
                    .bind(*state_code)
                    .execute(&mut *tx)
                    .await?;
            }
            DbWrite::Message {
                id,
                timestamp_ms,
                message,
                action_label,
                action_cmd,
            } => {
                sqlx::query(
                    "INSERT OR REPLACE INTO messages (id, timestamp_ms, message, action_label, action_cmd) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(*id as i64)
                .bind(*timestamp_ms)
                .bind(message.as_str())
                .bind(action_label.as_deref())
                .bind(action_cmd.as_deref())
                .execute(&mut *tx)
                .await?;
            }
            DbWrite::Telemetry {
                timestamp_ms,
                source_timestamp_ms,
                data_type,
                sender_id,
                values_json,
                payload_json,
            } => {
                sqlx::query(
                    "INSERT INTO telemetry (timestamp_ms, source_timestamp_ms, data_type, sender_id, values_json, payload_json) VALUES (?, ?, ?, ?, ?, ?)",
                )
                    .bind(*timestamp_ms)
                    .bind(*source_timestamp_ms)
                    .bind(data_type.as_str())
                    .bind(sender_id.as_str())
                    .bind(values_json.as_deref())
                    .bind(payload_json.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            DbWrite::Alert {
                timestamp_ms,
                severity,
                message,
                condition,
            } => {
                sqlx::query(
                    "INSERT INTO alerts (timestamp_ms, severity, message, condition_json) VALUES (?, ?, ?, ?)",
                )
                .bind(*timestamp_ms)
                .bind(severity.as_str())
                .bind(message.as_str())
                .bind(condition.as_ref().map(|link|serde_json::to_string(link).expect("finite condition link")))
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await
}

async fn insert_db_batch_with_retry(
    db: &sqlx::SqlitePool,
    writes: &[DbWrite],
) -> Result<(), sqlx::Error> {
    let mut delay = DB_RETRY_DELAY_MS;
    let mut last_err: Option<sqlx::Error> = None;

    for _ in 0..=DB_RETRIES {
        let result = insert_db_batch_once(db, writes).await;
        match result {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay = (delay * 2).min(1000);
            }
        }
    }

    Err(last_err.unwrap())
}

async fn flush_recording_batch(
    active: Option<&sqlx::SqlitePool>,
    batch: &mut Vec<DbWrite>,
) -> bool {
    if batch.is_empty() {
        return true;
    }
    if let Some(active) = active
        && let Err(e) = insert_db_batch_with_retry(active, batch).await {
            eprintln!("DB insert failed after retry; preserving batch for retry: {e}");
            return false;
        }
    batch.clear();
    true
}

async fn rotate_placeholder_db(state: &Arc<AppState>, mode: RecordingMode) {
    let old_pool = state.telemetry_db_pool();
    let old_path = state.telemetry_db_path();
    if old_path != "sqlite::memory:" {
        close_and_finalize_sqlite(old_pool, &old_path).await;
        delete_empty_recording_if_needed(&old_path).await;
    } else {
        old_pool.close().await;
    }
    match open_in_memory_telemetry_db().await {
        Ok(db) => {
            state.replace_telemetry_db(db, "sqlite::memory:".to_string());
            state.set_recording_status(RecordingStatusMsg {
                mode: RecordingModeWire::from(mode),
                db_path: None,
            });
        }
        Err(err) => {
            emit_warning(
                state,
                format!("Failed to reopen in-memory telemetry DB: {err}"),
            );
        }
    }
}

async fn delete_empty_recording_if_needed(db_path: &str) -> bool {
    match delete_sqlite_if_empty(db_path).await {
        Ok(true) => {
            log::info!("deleted empty recording db={db_path}");
            true
        }
        Ok(false) => false,
        Err(err) => {
            log::error!("failed checking empty recording db {db_path}: {err}");
            false
        }
    }
}

async fn clear_telemetry_tables(db: &sqlx::SqlitePool) -> Result<(), sqlx::Error> {
    for stmt in [
        "DELETE FROM telemetry",
        "DELETE FROM alerts",
        "DELETE FROM flight_state",
        "DELETE FROM messages",
    ] {
        sqlx::query(stmt).execute(db).await?;
    }
    Ok(())
}

#[cfg(feature = "test_fire_mode")]
async fn export_test_fire_csv_if_enabled(state: &Arc<AppState>, db_path: &str) {
    let csv_path = test_fire_csv::csv_path_for_db_path(db_path);
    let calibration = state.loadcell_calibration.lock().unwrap().clone();
    match test_fire_csv::export_recording_csv(db_path, &csv_path, &calibration).await {
        Ok(()) => log::info!("exported test-fire csv {}", csv_path.display()),
        Err(err) => log::error!(
            "failed exporting test-fire csv for {}: {err}",
            csv_path.display()
        ),
    }
}

#[cfg(not(feature = "test_fire_mode"))]
async fn export_test_fire_csv_if_enabled(_state: &Arc<AppState>, _db_path: &str) {}

async fn queue_db_write(
    state: &AppState,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    write: DbWrite,
) {
    let _ = db_overflow;
    match db_tx.try_send(DbQueueItem::Write(write)) {
        Ok(()) => {}
        Err(TrySendError::Full(DbQueueItem::Write(write))) => {
            if db_tx.send(DbQueueItem::Write(write)).await.is_err() {
                emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
            }
        }
        Err(TrySendError::Full(
            DbQueueItem::Control(_) | DbQueueItem::EnsureRecording(_) | DbQueueItem::Condition(_),
        )) => {
            emit_warning(
                state,
                "Warning: telemetry DB worker control queue backpressured",
            );
        }
        Err(TrySendError::Closed(_)) => {
            emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
        }
    }
}

// Status ownership follows the raw firmware command, before close aliases are
// normalized. Relays preserve the original sender; compact addresses resolve
// through the router's discovered hostname before the owner comparison.
fn owned_umbilical_status(
    state: &AppState,
    pkt: &Packet,
) -> Result<(Board, u8, bool), &'static str> {
    let data = pkt
        .data_as_u8()
        .map_err(|_| "expected 2-byte umbilical status payload")?;
    if data.len() != 2 {
        return Err("expected 2-byte umbilical status payload");
    }
    if data[1] > 1 {
        return Err("umbilical status state must be 0 or 1");
    }
    let owner = match data[0] {
        0..=6 => Board::ValveBoard,
        7..=13 => Board::ActuatorBoard,
        _ => return Err("unknown umbilical status command"),
    };
    if state.board_from_network_sender(pkt.sender()) != Some(owner) {
        return Err("umbilical status sender does not own command");
    }
    Ok((owner, data[0], data[1] == 1))
}

/// Validated once at physical ingress. Fields stay private so later discovery
/// cannot replace its owner, command, packet identity or arrival clocks.
pub(crate) struct ValidatedUmbilicalStatus {
    packet: Packet,
    owner: Board,
    command: u8,
    on: bool,
    received_ms: u64,
    receipt: crate::gse::ValveReceipt,
}
impl ValidatedUmbilicalStatus {
    pub(crate) fn owner(&self) -> Board {
        self.owner
    }
    pub(crate) fn received_ms(&self) -> u64 {
        self.received_ms
    }
}
fn validate_umbilical_status(
    state: &AppState,
    pkt: &Packet,
) -> Result<ValidatedUmbilicalStatus, &'static str> {
    let monotonic_at = std::time::Instant::now();
    let simulation_ms = crate::simulation_clock_ms();
    let received_ms = get_current_timestamp_ms();
    let receipt = crate::gse::ValveReceipt {
        network_ms: recording_network_utc_ms().unwrap_or(received_ms as i64),
        monotonic_at,
        simulation_ms,
    };
    let (owner, command, on) = owned_umbilical_status(state, pkt)?;
    Ok(ValidatedUmbilicalStatus {
        packet: pkt.clone(),
        owner,
        command,
        on,
        received_ms,
        receipt,
    })
}
#[cfg(test)]
fn umbilical_status_is_owned(state: &AppState, pkt: &Packet) -> bool {
    owned_umbilical_status(state, pkt).is_ok()
}

/// Rejections use the existing recording-only envelope and retain their exact
/// packet/arrival decision even if discovery learns the owner before recording.
pub(crate) fn capture_umbilical_status_at_ingress(
    state: &Arc<AppState>,
    pkt: &Packet,
) -> Option<ValidatedUmbilicalStatus> {
    match validate_umbilical_status(state, pkt) {
        Ok(capture) => Some(capture),
        Err(detail) => {
            let sender_id = pkt.sender().to_string();
            report_parse_error(state, &sender_id, &pkt.data_type().as_str(), detail);
            let row = TelemetryRow {
                timestamp_ms: get_current_timestamp_ms() as i64,
                data_type: pkt.data_type().as_str().to_string(),
                sender_id,
                values: Vec::new(),
            };
            state.ring_buffer.lock().unwrap().push(
                crate::ring_buffer::ReceivedPacket::AppliedStatus {
                    packet: pkt.clone(),
                    row,
                },
            );
            None
        }
    }
}

/// Apply confirmations before the recording queue can delay them. Source
/// clocks may reset: ordered delivery, not timestamp comparison, governs state.
#[cfg(test)]
pub(crate) fn apply_umbilical_status_at_ingress(
    state: &Arc<AppState>,
    pkt: &Packet,
) -> Option<TelemetryRow> {
    match validate_umbilical_status(state, pkt) {
        Ok(capture) => apply_captured_umbilical_status(state, capture),
        Err(detail) => {
            report_parse_error(state, pkt.sender(), &pkt.data_type().as_str(), detail);
            None
        }
    }
}

pub(crate) fn apply_captured_umbilical_status(
    state: &Arc<AppState>,
    capture: ValidatedUmbilicalStatus,
) -> Option<TelemetryRow> {
    state.mark_board_seen_at(
        capture.owner.sender_id(),
        capture.received_ms,
        capture.receipt.monotonic_at,
        capture.receipt.simulation_ms,
    );
    apply_captured_status_inner(state, capture)
}
fn apply_captured_status_inner(
    state: &Arc<AppState>,
    capture: ValidatedUmbilicalStatus,
) -> Option<TelemetryRow> {
    let ValidatedUmbilicalStatus {
        packet: pkt,
        owner,
        command: cmd_id,
        on,
        received_ms,
        receipt,
    } = capture;
    let sender_id = owner.sender_id().to_string();
    {
        if cmd_id == ValveBoardCommands::Sequence as u8 {
            // Only this already validated VB owner can publish sequence activity.
            // Retained launch metadata and unrelated board states do not own it.
            if !state.operator_queue.observe_fill_launch_active(on,pkt.timestamp() as i64,pkt.packet_id(),receipt) {return None;}
            if on {
                start_launch_clock_from_valve_sequence_status(state);
            }
            return None;
        }
        if let Some((key_cmd_id, key_on)) = umbilical_state_key(cmd_id, on) {
            let fresh_confirmation = state.set_umbilical_valve_state_captured(
                key_cmd_id,
                key_on,
                pkt.timestamp() as i64,
                pkt.packet_id(),
                receipt,
            );
            if fresh_confirmation {
                crate::gse::qd_confirmation(state, &sender_id, key_cmd_id, key_on, pkt.timestamp());
                crate::gse::report_safety(state);
                state.reconcile_pending_umbilical_valve_state(
                    key_cmd_id,
                    key_on,
                    crate::state::UMBILICAL_PENDING_VALVE_TIMEOUT,
                );
                if key_cmd_id == ValveBoardCommands::PilotOpen as u8
                    && key_on
                    && crate::gse::pilot_report_can_advance_launch_clock(state)
                {
                    transition_launch_clock_to_t_plus_from_pilot_open(state);
                }
            }
            sequences::refresh_action_policy_now(state);
            state.broadcast_action_policy_snapshot();
            return Some(TelemetryRow {
                timestamp_ms: received_ms as i64,
                data_type: VALVE_STATE_DATA_TYPE.to_string(),
                sender_id,
                values: valve_state_values(state).into_iter().collect(),
            });
        }
    }
    report_parse_error(
        state,
        &sender_id,
        &pkt.data_type().as_str(),
        "expected 2-byte umbilical status payload",
    );
    None
}

async fn persist_umbilical_status(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    pkt: &Packet,
    row: &TelemetryRow,
) {
    let values_json = serde_json::to_string(
        &row.values
            .iter()
            .map(|v| v.map(|n| n as f64))
            .collect::<Vec<_>>(),
    )
    .ok();
    queue_db_write(
        state,
        db_tx,
        db_overflow,
        DbWrite::Telemetry {
            timestamp_ms: row.timestamp_ms,
            source_timestamp_ms: Some(pkt.timestamp() as i64),
            data_type: row.data_type.clone(),
            sender_id: row.sender_id.clone(),
            values_json,
            payload_json: payload_json_from_pkt(pkt),
        },
    )
    .await;
}

// The same fan-out feeds live clients and /api/recent after reconnect.
fn publish_received_telemetry_rows(state: &AppState, rows: Vec<TelemetryRow>) {
    for row in rows {
        state.cache_recent_telemetry(row.clone());
        let _ = state.ws_tx.send(row);
    }
}

#[cfg(test)]
mod local_stop_phase_tests;
#[cfg(test)]
pub(crate) mod queued_phase_probe;

/// Direct unit invocation has its own immediate entry; production bulk work
/// must supply the original immutable SDK ingress generation instead.
#[cfg(test)]
async fn handle_packet(
    state: &Arc<AppState>, db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow, pkt: Packet,
) -> Vec<TelemetryRow> {
    let original = crate::gse::automatic_generation(state);
    handle_packet_at_ingress(state, db_tx, db_overflow, pkt, original).await
}
async fn handle_packet_at_ingress(
    state: &Arc<AppState>, db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow, pkt: Packet, receive_generation: u64,
) -> Vec<TelemetryRow> {
    #[cfg(all(test, feature="test_fire_mode"))]
    crate::recovery_pending_probe::receive_entry(state, &pkt);
    #[cfg(test)]
    local_stop_phase_tests::after_original_entry(state).await;
    let primary_role = !crate::rrls::is_rrls();
    // Capture ORIGINAL Stop/generation before resolving a compact source:
    // identity lookup can wait on SDK state and must never freshen this owner.
    let original_entry = (receive_generation == crate::gse::automatic_generation(state))
        .then(|| crate::automatic_startup_idle::Captured::capture(state, primary_role)
            .filter(|captured| captured.generation == receive_generation)).flatten();
    let automatic_origin = original_entry.filter(|_| {
        matches!(state.board_from_network_sender(pkt.sender()), Some(board) if board != Board::GroundStation)
    });
    let source = i64::try_from(pkt.timestamp()).ok().and_then(|source| {
        crate::telemetry_provenance::canonical_source(source, procedure_clock::timestamp_ms())
    });
    let rows = handle_packet_inner(state, db_tx, db_overflow, pkt, primary_role, automatic_origin, receive_generation).await;
    for row in &rows {
        crate::telemetry_provenance::note(row, source);
    }
    rows
}

async fn handle_packet_inner(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    pkt: Packet,
    primary_role: bool,
    automatic_origin: Option<crate::automatic_startup_idle::Captured>,
    receive_generation: u64,
) -> Vec<TelemetryRow> {
    let umbilical_capture =
        if pkt.data_type() == crate::telemetry_schema::data_type("UMBILICAL_STATUS") {
            match validate_umbilical_status(state, &pkt) {
                Ok(capture) => Some(capture),
                Err(detail) => {
                    report_parse_error(state, pkt.sender(), &pkt.data_type().as_str(), detail);
                    queue_db_write(
                        state,
                        db_tx,
                        db_overflow,
                        DbWrite::Telemetry {
                            timestamp_ms: get_current_timestamp_ms() as i64,
                            source_timestamp_ms: Some(pkt.timestamp() as i64),
                            data_type: pkt.data_type().as_str().to_string(),
                            sender_id: pkt.sender().to_string(),
                            values_json: None,
                            payload_json: payload_json_from_pkt(&pkt),
                        },
                    )
                    .await;
                    return Vec::new();
                }
            }
        } else {
            None
        };
    if let Some(capture) = &umbilical_capture {
        state.mark_board_seen_at(
            capture.owner.sender_id(),
            capture.received_ms,
            capture.receipt.monotonic_at,
            capture.receipt.simulation_ms,
        );
    } else {
        state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
    }
    #[cfg(all(test, feature="test_fire_mode"))]
    crate::recovery_pending_probe::before_automatic(state, &pkt).await;
    if cfg!(feature = "test_fire_mode") && state.all_required_boards_seen()
        && let Some(original) = automatic_origin
    {
        crate::automatic_startup_idle::publish(state, original).await;
    }
    let sender_id = umbilical_capture
        .as_ref()
        .map(|capture| capture.owner.sender_id().to_string())
        .unwrap_or_else(|| state.canonical_network_sender_id(pkt.sender()));

    if pkt.data_type() == crate::telemetry_schema::data_type("DAQ_LAUNCH_LOG_STATUS") {
        if let Ok(words) = pkt.data_as_u32() {
            let accepted = state
                .network_health
                .lock()
                .unwrap()
                .observe_launch_log_status(
                    &sender_id,
                    &words,
                    std::time::Instant::now(),
                    get_current_timestamp_ms(),
                    pkt.timestamp(),
                    recording_network_utc_ms()
                        .and_then(|t| t.try_into().ok())
                        .unwrap_or_else(get_current_timestamp_ms),
                );
            if accepted {
                recording_diagnostics::raw_health(state, db_tx, &pkt, &sender_id);
            }
        }
        return Vec::new();
    }

    if pkt.data_type() == crate::telemetry_schema::data_type("DAQ_SD_HEALTH") {
        if let Ok(words) = pkt.data_as_u32() {
            let accepted = state.network_health.lock().unwrap().observe_sd_status(
                &sender_id,
                &words,
                std::time::Instant::now(),
                get_current_timestamp_ms(),
            );
            if accepted {
                recording_diagnostics::raw_health(state, db_tx, &pkt, &sender_id);
            }
        }
        return Vec::new();
    }

    if pkt.data_type() == crate::telemetry_schema::data_type("BOARD_HEALTH") {
        if let Ok(words) = pkt.data_as_u32() {
            let accepted = state.network_health.lock().unwrap().observe_diagnostics_with_source(
                &sender_id,
                &words,
                std::time::Instant::now(),
                get_current_timestamp_ms(),
                Some(pkt.timestamp()),
            );
            if accepted {
                recording_diagnostics::raw_health(state, db_tx, &pkt, &sender_id);
            }
        }
        return Vec::new();
    }

    if pkt.data_type() == crate::telemetry_schema::data_type("WARNING") {
        if let Ok(msg) = pkt.data_as_string() {
            if sender_id != Board::ValveBoard.sender_id() {
                emit_warning(state, msg.to_string());
            } else if let Some(shown) = crate::gse::overpressure::board_warning(state, &msg) {
                emit_warning(state, shown);
            }
        } else {
            emit_warning(state, "Warning packet with invalid UTF-8 payload");
            report_parse_error(
                state,
                &sender_id,
                &pkt.data_type().as_str(),
                "invalid UTF-8 payload",
            );
        }
        return Vec::new();
    }

    if pkt.data_type() == crate::telemetry_schema::data_type("MESSAGE_DATA")
        || pkt.data_type() == crate::telemetry_schema::data_type("ORDERED_MESSAGE")
    {
        let message = match pkt.data_as_string() {
            Ok(msg) => msg.to_string(),
            Err(_) => {
                report_parse_error(
                    state,
                    &sender_id,
                    &pkt.data_type().as_str(),
                    "invalid UTF-8 payload",
                );
                "Telemetry message with invalid UTF-8 payload".to_string()
            }
        };
        let message = format!("{sender_id}: {message}");
        let (message_id, inserted) = state.add_backend_message(message.clone());
        if inserted
            && let Err(err) = state
                .db_queue_tx
                .try_send(DbQueueItem::Write(DbWrite::Message {
                    id: message_id,
                    timestamp_ms: get_current_timestamp_ms() as i64,
                    message,
                    action_label: None,
                    action_cmd: None,
                }))
        {
            eprintln!("Failed to queue backend message DB write: {err}");
        }
        return Vec::new();
    }

    if pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE") {
        if !cfg!(feature = "testing") && !state.all_required_boards_seen() {
            return Vec::new();
        }
        let pkt_data = match pkt.data_as_u8() {
            Ok(data) => *data.first().expect("index 0 does not exist"),
            Err(_) => {
                report_parse_error(
                    state,
                    &sender_id,
                    &pkt.data_type().as_str(),
                    "expected u8 flight-state payload",
                );
                return Vec::new();
            }
        };
        let new_flight_state = match u8_to_flight_state(pkt_data) {
            Some(flight_state) => flight_state,
            None => {
                report_parse_error(
                    state,
                    &sender_id,
                    &pkt.data_type().as_str(),
                    "unknown flight-state code",
                );
                return Vec::new();
            }
        };
        // FC is the flight-state authority; local GS publications carry the host
        // state. Other boards retain their own state telemetry in every phase,
        // including Startup/Idle: their reports never rewrite the global master,
        // clock or an explicitly verified global reset.
        let authority = state.board_from_network_sender(&sender_id);
        let gs = authority == Some(Board::GroundStation);
        let fc = authority == Some(Board::FlightComputer);
        // Attribution only, outside every final owner gate. A relay's own
        // fallback/echo is no more phase authority than a primary's own echo.
        let self_echo = state.topology_router.get().is_some_and(|router| {
            router.sender_matches_hostname(pkt.sender(), router.hostname().as_ref())
        });
        let current = state.local_flight_state_snapshot();
        #[cfg(test)]
        local_stop_phase_tests::after_phase_snapshot(state).await;
        let prelaunch = |value| {
            matches!(
                value,
                FlightState::Startup
                    | FlightState::Idle
                    | FlightState::PreFill
                    | FlightState::FillTest
                    | FlightState::NitrogenFill
                    | FlightState::NitrousFill
                    | FlightState::Armed
            )
        };
        // Retired ingress and explicit-model local reflections remain raw
        // historical observations. They are not authoritative FlightState
        // events and cannot update the aggregate, clock or desired JSON.
        let retired_ingress = receive_generation != crate::gse::automatic_generation(state);
        if retired_ingress || ((self_echo || (gs && primary_role))
            && procedure_clock::now().model_monotonic_ms().is_some())
        {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: get_current_timestamp_ms() as i64,
                    source_timestamp_ms: Some(pkt.timestamp() as i64),
                    data_type: "FLIGHT_STATE".into(),
                    sender_id: sender_id.clone(),
                    values_json: Some(
                        serde_json::to_string(&[pkt_data as f32]).expect("finite flight state"),
                    ),
                    payload_json: payload_json_from_pkt(&pkt),
                },
            )
            .await;
        }
        if retired_ingress || self_echo || (gs && primary_role) || (!gs
            && (!fc || new_flight_state == FlightState::Aborted
                || (fc && prelaunch(new_flight_state) && crate::operator_phase::fc_prelaunch_is_raw(state))
                || (!prelaunch(current) && prelaunch(new_flight_state))
                || current == FlightState::Aborted))
        {
            return vec![TelemetryRow {
                timestamp_ms: get_current_timestamp_ms() as i64,
                data_type: "FLIGHT_STATE".into(),
                sender_id,
                values: vec![Some(pkt_data as f32)],
            }];
        }
        {
            let mut fs = state.state.lock().unwrap();
            // The original snapshot is not final authority across a local Stop
            // admission or reset. A delayed FC phase must not undo either owner.
            if receive_generation != crate::gse::automatic_generation(state)
                || (!gs && (!fc || new_flight_state == FlightState::Aborted
                    || (fc && prelaunch(new_flight_state) && crate::operator_phase::fc_prelaunch_is_raw(state))
                    || (!prelaunch(*fs) && prelaunch(new_flight_state))
                    || *fs == FlightState::Aborted)) {
                return vec![TelemetryRow {
                    timestamp_ms: get_current_timestamp_ms() as i64,
                    data_type: "FLIGHT_STATE".into(), sender_id,
                    values: vec![Some(pkt_data as f32)],
                }];
            }
            *fs = new_flight_state;
        }
        let purpose = procedure_clock::now();
        let explicit_model = purpose.model_monotonic_ms().is_some();
        // Recording/replay's primary timeline is receipt wall time. Keep the
        // model network UTC separately, just like sensor telemetry rows.
        let ts_ms = if explicit_model {
            get_current_timestamp_ms() as i64
        } else {
            pkt.timestamp() as i64
        };
        let launch_clock_ts_ms = if explicit_model {
            purpose.timestamp_ms()
        } else if sender_id == Board::GroundStation.sender_id() {
            ts_ms
        } else {
            get_current_timestamp_ms() as i64
        };
        // Only an actually received prelaunch phase can re-arm a reset clock;
        // a process's initial local Idle is not a new launch transition.
        if matches!(
            new_flight_state,
            FlightState::Startup
                | FlightState::Idle
                | FlightState::PreFill
                | FlightState::FillTest
                | FlightState::NitrogenFill
                | FlightState::NitrousFill
                | FlightState::Armed
        ) {
            state.confirm_new_launch_cycle();
        }
        state.update_launch_clock_for_state(new_flight_state, launch_clock_ts_ms);
        queue_db_write(
            state,
            db_tx,
            db_overflow,
            DbWrite::FlightState {
                timestamp_ms: ts_ms,
                state_code: pkt_data as i64,
            },
        )
        .await;

        if explicit_model {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: ts_ms,
                    source_timestamp_ms: Some(pkt.timestamp() as i64),
                    data_type: "FLIGHT_STATE".into(),
                    sender_id: sender_id.clone(),
                    values_json: Some(
                        serde_json::to_string(&[pkt_data as f32]).expect("finite flight state"),
                    ),
                    payload_json: payload_json_from_pkt(&pkt),
                },
            )
            .await;
        }

        let _ = state.state_tx.send(FlightStateMsg {
            state: new_flight_state,
        });
        state.broadcast_fill_targets_snapshot();
        return Vec::new();
    }

    if let Some(capture) = umbilical_capture {
        if let Some(row) = apply_captured_status_inner(state, capture) {
            persist_umbilical_status(state, db_tx, db_overflow, &pkt, &row).await;
            return vec![row];
        }
        return Vec::new();
    }

    let data_type_str = pkt.data_type().as_str().to_string();
    let ts_ms = get_current_timestamp_ms() as i64;

    let payload_json = payload_json_from_pkt(&pkt);

    if pkt.data_type() == crate::telemetry_schema::data_type("AB_IGNITER_SENSE") {
        // Keep boot/sequence words exact; f32 cannot carry their identities.
        let source_is_actuator = state.board_from_network_sender(&sender_id) == Some(Board::ActuatorBoard)
            && pkt.endpoints() == [crate::telemetry_schema::endpoint("GROUND_STATION")];
        let source_now = recording_network_utc_ms().and_then(|n| n.try_into().ok()).unwrap_or_else(get_current_timestamp_ms);
        let receipt = std::time::Instant::now();
        let observation = pkt.data_as_u32().ok().and_then(|words| {
            // All SDK/config/clock work precedes this short pure tracker lock.
            state.igniter_sense.lock().unwrap().observe(source_is_actuator,
                &words, pkt.timestamp(), source_now, receipt)
        });
        let mut rows = Vec::new();
        if let Some(observed) = observation {
            // Six16-bit limbs retain boot+sequence exactly in JSON f32 values;
            // display can pair a status with its original ADC sample.
            let owner = [0_u32,16,32,48].into_iter()
                .map(|shift|Some(((observed.boot >> shift)&0xffff) as f32))
                .chain([Some((observed.sequence&0xffff) as f32),Some((observed.sequence>>16) as f32)])
                .collect::<Vec<_>>();
            let mut status_values=vec![Some(observed.status as u8 as f32),
                observed.total_sample_age_ms.map(|n| n as f32),
                (observed.reference_age_ms != u32::MAX).then_some(observed.reference_age_ms as f32), observed.capability.map(f32::from)];
            status_values.extend(owner.iter().copied());
            status_values.extend([0_u32,16,32,48].into_iter()
                .map(|shift|Some(((source_now >> shift)&0xffff) as f32)));
            rows.push(TelemetryRow {
                timestamp_ms: ts_ms, data_type: "IGNITER_SENSE_STATUS".into(), sender_id: sender_id.clone(),
                // Capability1:model_supply_mismatch is information, not an Abort/error producer.
                values: status_values,
            });
            if let Some(signal) = observed.signal {
                let mut signal_values=vec![signal.raw_counts,signal.volts,None];
                signal_values.extend(owner.iter().copied());
                let row = TelemetryRow {
                    timestamp_ms: signal.source_utc_ms.unwrap_or(ts_ms),
                    data_type: "IGNITER_SENSE_SIGNAL".into(), sender_id: sender_id.clone(),
                    values: signal_values,
                };
                crate::telemetry_provenance::note(&row, signal.source_utc_ms);
                rows.push(row);
            }
        }
        // Exact original wire metadata is available to recording/export/replay.
        if should_persist_telemetry_sample(&data_type_str, &sender_id, ts_ms) {
            queue_db_write(state, db_tx, db_overflow, DbWrite::Telemetry {
                timestamp_ms: ts_ms, source_timestamp_ms: i64::try_from(pkt.timestamp()).ok(),
                data_type: data_type_str, sender_id: sender_id.clone(),
                values_json: None, payload_json: payload_json.clone(),
            }).await;
        }
        for row in &rows {
            if should_persist_telemetry_sample(&row.data_type, &sender_id, row.timestamp_ms) {
                queue_db_write(state, db_tx, db_overflow, DbWrite::Telemetry {
                    timestamp_ms: row.timestamp_ms,
                    source_timestamp_ms: if row.data_type == "IGNITER_SENSE_SIGNAL" { crate::telemetry_provenance::source(row) } else { i64::try_from(pkt.timestamp()).ok() },
                    data_type: row.data_type.clone(), sender_id: sender_id.clone(),
                    values_json: telemetry_values_json(&row.values), payload_json: payload_json.clone(),
                }).await;
            }
        }
        return rows;
    }

    if pkt.data_type() == crate::telemetry_schema::data_type("GPS_SATELLITE_NUMBER") {
        return handle_gps_satellite_count_packet(state, db_tx, db_overflow, &pkt, &payload_json)
            .await
            .into_iter()
            .collect();
    }

    if let Some(values_vec) = telemetry_f32_values(&pkt) {
        let rows = telemetry_rows_from_packet_values(state, &pkt, &sender_id, values_vec);
        let live_receipt = crate::gse::ValveReceipt {
            network_ms: recording_network_utc_ms().unwrap_or(ts_ms),
            monotonic_at: std::time::Instant::now(),
            simulation_ms: crate::simulation_clock_ms(),
        };
        // Known UTC data retains source age; receipt cannot turn a delayed
        // network batch into fresh independent tare observations. Unsynced
        // boot clocks retain receipt-time compatibility and PacketID dedup.
        let tare_receipt = live_receipt.network_ms;
        let tare_source_fresh = i64::try_from(pkt.timestamp()).ok().is_some_and(|source| {
            if source >= 1_577_836_800_000 && tare_receipt >= 1_577_836_800_000 {
                tare_receipt
                    .checked_sub(source)
                    .is_some_and(|age| (-100..=2000).contains(&age))
            } else {
                source >= 0
            }
        });
        let gateway_condition = if pkt.data_type()
            == crate::telemetry_schema::data_type("BATTERY_VOLTAGE")
            && is_fill_system_battery_sender(&sender_id)
        {
            crate::alert_condition_producers::gateway_voltage(
                &pkt,
                live_receipt.network_ms,
                state
                    .alert_conditions
                    .current_state(crate::alert_conditions::Key::GatewayLowVoltage),
                FILL_SYSTEM_LOW_VOLTAGE_WARN_THRESHOLD,
                FILL_SYSTEM_LOW_VOLTAGE_RELATCH_THRESHOLD,
            )
            .and_then(|evidence| {
                crate::alert_condition_runtime::observe(
                    state,
                    evidence.key,
                    evidence.state,
                    evidence.message,
                    ts_ms,
                )
            })
        } else {
            None
        };
        // Tare capture is live input, not a recording side effect. A full disk
        // queue must not delay its receipt clock or hide a valid sample.
        {
            let mut zero = state.auto_zero.lock().unwrap();
            for (kind, values) in &rows {
                if tare_source_fresh
                    && sender_id == Board::DaqBoard.sender_id()
                    && matches!(kind.as_str(), "KG1000" | "KG50")
                    && let Some(raw) = values.first().copied().flatten()
                {
                    zero.observe_packet(kind, raw, pkt.packet_id());
                }
            }
        }
        crate::gse::observe_valve_pressure_ingress(
            state,
            &sender_id,
            &rows,
            pkt.timestamp() as i64,
            live_receipt,
        );
        if sender_id == Board::ValveBoard.sender_id()
            && rows.iter().any(|(kind, _)| kind == "FUEL_TANK_PRESSURE")
        {
            crate::gse::overpressure::evaluate(state);
        }
        // Prepare every live mass/PT row before even the composite raw row
        // can await recorder capacity. Filters and calibration run once.
        let prepared: Vec<_> = rows
            .iter()
            .map(|(kind, values)| {
                if !matches!(
                    kind.as_str(),
                    loadcell::RAW_LOADCELL_DATA_TYPE_1000KG
                        | loadcell::RAW_LOADCELL_DATA_TYPE_50KG
                        | loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE
                        | "FUEL_TANK_PRESSURE"
                ) {
                    return None;
                }
                let raw_value = values.first().copied().flatten()?;
                prepare_derived_loadcell_rows(
                    state,
                    &DerivedLoadcellSample {
                        ts_ms,
                        source_timestamp_ms: Some(pkt.timestamp() as i64),
                        sender_id: &sender_id,
                        sensor_id: kind,
                        raw_value,
                        payload_json: &payload_json,
                        receipt: live_receipt,
                    },
                )
            })
            .collect();
        let mut out = Vec::with_capacity(rows.len());

        for ((row_data_type, row_values), prepared) in rows.into_iter().zip(prepared) {
            if should_persist_telemetry_sample(&row_data_type, &sender_id, ts_ms) {
                queue_db_write(
                    state,
                    db_tx,
                    db_overflow,
                    DbWrite::Telemetry {
                        timestamp_ms: ts_ms,
                        source_timestamp_ms: Some(pkt.timestamp() as i64),
                        data_type: row_data_type.clone(),
                        sender_id: sender_id.clone(),
                        values_json: telemetry_values_json(&row_values),
                        payload_json: payload_json.clone(),
                    },
                )
                .await;
            }

            if let Some(first_value) = row_values.first().copied().flatten() {
                if row_data_type == *crate::telemetry_schema::data_type("BATTERY_VOLTAGE").as_str()
                    && is_fill_system_battery_sender(&sender_id)
                    && update_fill_system_low_voltage_latch(first_value)
                {
                    crate::web::emit_warning_linked(
                        state,
                        FILL_SYSTEM_LOW_VOLTAGE_WARNING,
                        gateway_condition.clone(),
                    );
                }

                emit_derived_battery_rows(
                    state,
                    db_tx,
                    db_overflow,
                    ts_ms,
                    Some(pkt.timestamp() as i64),
                    &sender_id,
                    &row_data_type,
                    first_value,
                    &payload_json,
                )
                .await;

                if let Some(prepared) = prepared {
                    emit_derived_loadcell_rows(
                        state,
                        db_tx,
                        db_overflow,
                        DerivedLoadcellSample {
                            ts_ms,
                            source_timestamp_ms: Some(pkt.timestamp() as i64),
                            sender_id: &sender_id,
                            sensor_id: &row_data_type,
                            raw_value: first_value,
                            payload_json: &payload_json,
                            receipt: live_receipt,
                        },
                        prepared,
                    )
                    .await;
                }
            }

            if let Some(speed_mps) =
                update_vehicle_speed_estimate(&row_data_type, ts_ms, &row_values)
            {
                emit_derived_vehicle_speed_row(
                    state,
                    db_tx,
                    db_overflow,
                    ts_ms,
                    Some(pkt.timestamp() as i64),
                    speed_mps,
                    &payload_json,
                )
                .await;
            }

            let row = TelemetryRow {
                timestamp_ms: ts_ms,
                data_type: row_data_type,
                sender_id: sender_id.clone(),
                values: row_values,
            };
            if row.data_type == "GPS_DATA" {
                crate::antenna::observe_source_gps(&row, pkt.timestamp());
            }
            out.push(row);
        }

        out
    } else {
        if should_persist_telemetry_sample(&data_type_str, &sender_id, ts_ms) {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: ts_ms,
                    source_timestamp_ms: Some(pkt.timestamp() as i64),
                    data_type: data_type_str.clone(),
                    sender_id: sender_id.clone(),
                    values_json: None,
                    payload_json,
                },
            )
            .await;
        }

        if pkt.data_type() == crate::telemetry_schema::data_type("HEARTBEAT") {
            return vec![TelemetryRow {
                timestamp_ms: ts_ms,
                data_type: data_type_str,
                sender_id,
                values: Vec::new(),
            }];
        }

        report_parse_error(
            state,
            &sender_id,
            &pkt.data_type().as_str(),
            "unable to decode telemetry payload as f32 values",
        );

        Vec::new()
    }
}

pub fn get_current_timestamp_ms() -> u64 {
    // Frontend live telemetry freshness is validated against the client's wall clock.
    // Using router/network time here can make every websocket row look stale if the
    // synchronized network epoch drifts from host wall time. Keep UI-facing timestamps
    // on host wall clock.
    get_system_timestamp_ms()
}

fn get_system_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now();
    let duration_since_epoch = now.duration_since(UNIX_EPOCH).unwrap();
    duration_since_epoch.as_millis() as u64
}

fn log_telemetry_error(context: &str, err: sedsnet::TelemetryError) {
    eprintln!("{context}: {:?}", err);
}

fn log_router_rx_error(err: sedsnet::TelemetryError) {
    if matches!(
        err,
        sedsnet::TelemetryError::Unpack(_) | sedsnet::TelemetryError::InvalidType
    ) {
        log_router_decode_error(err);
    } else {
        log_telemetry_error("router rx queue processing failed", err);
    }
}

fn log_router_decode_error(err: sedsnet::TelemetryError) {
    let now_ms = get_current_timestamp_ms();
    let last_log_ms = ROUTER_DECODE_ERROR_LAST_LOG_MS.load(Ordering::Relaxed);
    if last_log_ms == 0 || now_ms.saturating_sub(last_log_ms) >= ROUTER_DECODE_ERROR_LOG_INTERVAL_MS
    {
        ROUTER_DECODE_ERROR_LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
        let suppressed = ROUTER_DECODE_ERROR_SUPPRESSED.swap(0, Ordering::Relaxed);
        if suppressed > 0 {
            eprintln!(
                "router rx queue decode failed: {:?} (suppressed {suppressed} repeated decode errors)",
                err
            );
        } else {
            eprintln!("router rx queue decode failed: {:?}", err);
        }
    } else {
        ROUTER_DECODE_ERROR_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
    }
}

fn process_router_queues(router: &Router) -> Result<(), sedsnet::TelemetryError> {
    if let Err(err) = router.process_tx_queue_with_timeout(ROUTER_TX_BUDGET_MS) {
        log_telemetry_error("router tx queue processing failed", err);
        return Ok(());
    }
    if ROUTER_RX_BUDGET_MS > 0
        && let Err(err) = router.process_rx_queue_with_timeout(ROUTER_RX_BUDGET_MS)
    {
        log_router_rx_error(err);
        return Ok(());
    }
    Ok(())
}

async fn reset_testing_simulation(state: &Arc<AppState>) {
    if !flight_sim::sim_mode_enabled() {
        emit_warning(
            state,
            "Reset Sim ignored: simulator mode is not enabled for this backend",
        );
        return;
    }

    #[cfg(feature = "testing")]
    crate::actuator_plumbing::invalidate_modeled_retraction(state);
    flight_sim::reset_simulation();
    state.clear_runtime_data_for_sim_reset();
    {
        let mut flight_state = state.state.lock().unwrap();
        *flight_state = FlightState::Idle;
    }
    // Simulation data/state reset is separate from the explicit ResetTClock utility.
    state.broadcast_fill_targets_snapshot();

    let now_ms = get_current_timestamp_ms() as i64;
    if let Err(err) = state
        .db_queue_tx
        .send(DbQueueItem::Control(RecordingCommand::ResetAll))
        .await
    {
        eprintln!("Reset Sim failed to enqueue DB reset: {err}");
    }
    state.set_messages_snapshot(Vec::new());
    if let Err(err) = state
        .db_queue_tx
        .send(DbQueueItem::Write(DbWrite::FlightState {
            timestamp_ms: now_ms,
            state_code: FlightState::Idle as i64,
        }))
        .await
    {
        eprintln!("Reset Sim failed to enqueue idle flight state write: {err}");
    }

    let _ = state.state_tx.send(FlightStateMsg {
        state: FlightState::Idle,
    });
    let now_ms = get_current_timestamp_ms();
    let _ = state
        .board_status_tx
        .send(state.board_status_snapshot(now_ms));
    state.broadcast_dashboard_reset();
}

fn payload_json_from_pkt(pkt: &Packet) -> String {
    let bytes = pkt.payload();
    serde_json::to_string(&bytes).unwrap_or_else(|_| "[]".to_string())
}

pub fn timesync_enabled() -> bool {
    if crate::rrls::is_rrls() {
        return false;
    }
    if cfg!(feature = "testing") {
        return std::env::var("GROUNDSTATION_TIMESYNC").ok().as_deref() == Some("1");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::fill_targets;
    use crate::gpio::GpioPins;
    use crate::loadcell;
    use crate::ring_buffer::RingBuffer;
    use crate::sequences::default_action_policy;
    use crate::telemetry_db::{LaunchClockMsg, RecordingModeWire};
    use crate::types::Board;
    use sqlx::SqlitePool;
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::{Notify, broadcast, mpsc};

    #[tokio::test]
    async fn shutdown_receipt_normal_drain_is_clean_with_retained_db_senders() {
        let (db_tx, db_rx) = mpsc::channel(8);
        let retained_sender = db_tx.clone();
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let worker = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            state.shutdown_subscribe(),
        ));
        tokio::task::yield_now().await;
        state.request_shutdown();
        let result = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("healthy shutdown was blocked by retained sender")
            .expect("outer telemetry task failed");
        assert!(
            result.ingress_quiesced,
            "actual inner worker drain was not clean"
        );
        drop(retained_sender);
    }

    #[tokio::test]
    async fn shutdown_receipt_propagates_actual_router_worker_panic() {
        let (db_tx, db_rx) = mpsc::channel(8);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let entered = Arc::new(AtomicBool::new(false));
        let observed = entered.clone();
        router.add_side_packed("shutdown_panic_fixture", move |_bytes| {
            observed.store(true, Ordering::Release);
            panic!("injected actual router worker panic");
        });
        router.poll_discovery().unwrap();
        let worker = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            state.shutdown_subscribe(),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !entered.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("actual router worker did not enter injected side");
        state.request_shutdown();
        let result = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("panicked router worker did not join")
            .expect("outer telemetry worker must preserve inner failure outcome");
        assert!(
            !result.ingress_quiesced,
            "outer task Ok hid actual inner panic"
        );
    }

    #[tokio::test]
    async fn shutdown_receipt_timeout_keeps_late_abort_observation_live() {
        struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let (db_tx, db_rx) = mpsc::channel(8);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let entered = Arc::new(AtomicBool::new(false));
        let observed = entered.clone();
        let release = Release(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
        let parked = release.0.clone();
        let source = state.clone();
        router.add_side_packed("shutdown_timeout_fixture", move |_bytes| {
            observed.store(true, Ordering::Release);
            let (lock, wake) = &*parked;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = wake.wait(ready).unwrap();
            }
            drop(ready);
            source
                .board_abort
                .observe_abort(Board::DaqBoard, std::time::Instant::now(), 123456);
            Ok(())
        });
        router.poll_discovery().unwrap();
        let worker = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            state.shutdown_subscribe(),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !entered.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("actual router worker did not enter blocked side");
        state.request_shutdown();
        let result = tokio::time::timeout(Duration::from_secs(12), worker)
            .await
            .expect("actual worker timeout did not propagate")
            .expect("outer telemetry worker must return its inner timeout outcome");
        assert!(
            !result.ingress_quiesced,
            "actual inner timeout became a clean receipt"
        );
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.board_abort.masks().0 & (1 << 6) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("late Abort was discarded after ambiguous producer shutdown");
    }

    #[tokio::test]
    async fn shutdown_receipt_real_receiver_and_owned_writer_clean_restart() {
        let path =
            std::env::temp_dir().join(format!("gs-actual-clean-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        let directory = path.join("history");
        // Start from an explicitly checked Clean fixture, not an unprovable
        // missing directory. Production first-install absence remains Unknown7.
        let mut checked =
            crate::board_abort_history::Store::provision(&directory, [43; 16], 0x7f).unwrap();
        checked.begin_session(0x7f).unwrap();
        checked
            .seal_clean(
                crate::board_abort_history::QuiescenceReceipt::checked(0, true, true, true)
                    .unwrap(),
            )
            .unwrap();
        drop(checked);
        let changed = Arc::new(Notify::new());
        let (restored, publisher, owner) =
            crate::board_abort_history_writer::Owner::open(&directory, [43; 16], changed.clone())
                .unwrap();
        let (db_tx, db_rx) = mpsc::channel(8);
        let (mut state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        Arc::get_mut(&mut state)
            .expect("fixture unexpectedly shared before producers")
            .board_abort = Arc::new(crate::board_abort::Runtime::restored(
            restored, publisher, changed,
        ));
        state.board_abort.publish_initial_history();
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let worker = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            state.shutdown_subscribe(),
        ));
        tokio::task::yield_now().await;
        state.request_shutdown();
        assert!(crate::board_recovery::seal_shutdown_and_take_owner(&state).is_none());
        assert!(crate::board_recovery::shutdown_clean_eligible(&state));
        let outcome = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.ingress_quiesced);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        let snapshot = state.board_abort.freeze_history_after_quiescence();
        tokio::task::spawn_blocking(move || owner.finish(snapshot, true))
            .await
            .unwrap()
            .unwrap();
        let changed = Arc::new(Notify::new());
        let (restored, _, owner) =
            crate::board_abort_history_writer::Owner::open(&directory, [43; 16], changed).unwrap();
        assert!(!restored.previous_session_unclean);
        assert_eq!(
            restored.snapshot.unknown, 0,
            "clean normal restart made optional boards mandatory"
        );
        owner.finish(restored.snapshot, false).unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }

    fn timesync_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[cfg(not(feature = "testing"))]
    #[test]
    fn timesync_defaults_on_without_testing_feature() {
        let _guard = timesync_env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var("GROUNDSTATION_TIMESYNC");
        }
        assert!(timesync_enabled());
    }

    #[test]
    fn timesync_env_can_still_enable_it() {
        let _guard = timesync_env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("GROUNDSTATION_TIMESYNC", "1");
        }
        assert!(timesync_enabled());
        unsafe {
            std::env::remove_var("GROUNDSTATION_TIMESYNC");
        }
    }

    fn test_db_overflow() -> DbOverflow {
        DbOverflow
    }

    #[test]
    fn parse_error_filter_skips_protocol_packets() {
        assert!(!should_report_parse_error("DISCOVERY_ANNOUNCE"));
        assert!(!should_report_parse_error("DISCOVERY_TIMESYNC_SOURCES"));
        assert!(!should_report_parse_error("TIME_SYNC_ANNOUNCE"));
        assert!(!should_report_parse_error("TIME_SYNC_REQUEST"));
        assert!(!should_report_parse_error("TIME_SYNC_RESPONSE"));
        assert!(!should_report_parse_error("VALVE_COMMAND"));
        assert!(!should_report_parse_error("FLIGHT_COMMAND"));
        assert!(!should_report_parse_error("ACTUATOR_COMMAND"));
        assert!(!should_report_parse_error("ABORT"));
        assert!(should_report_parse_error("KG1000"));
    }

    #[test]
    fn radio_command_log_line_detects_command_packets() {
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("VALVE_COMMAND"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([1_u8]),
        )
        .expect("failed to build valve command packet");
        let wire = serialize::pack_packet(&pkt);
        let msg = radio_command_log_line("radio TX sent", "rocket_comms", &wire)
            .expect("command packet should emit message");
        assert!(msg.contains("rocket_comms: radio TX sent VALVE_COMMAND"));

        let telemetry_pkt = Packet::new(
            crate::telemetry_schema::data_type("KG1000"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::DaqBoard.sender_id(),
            123,
            f32_payload(&[1.0]),
        )
        .expect("failed to build kg1000 packet");
        let telemetry_wire = serialize::pack_packet(&telemetry_pkt);
        assert!(radio_command_log_line("radio TX sent", "rocket_comms", &telemetry_wire).is_none());
    }

    #[test]
    fn flight_command_discriminants_match_current_layout() {
        assert_eq!(FlightComputerCommands::Postinit as u8, 0);
        assert_eq!(FlightComputerCommands::Launch as u8, 1);
        assert_eq!(FlightComputerCommands::MonitorAltitude as u8, 2);
        assert_eq!(FlightComputerCommands::RevokeMonitorAltitude as u8, 3);
        assert_eq!(FlightComputerCommands::ConsecutiveSamples as u8, 4);
        assert_eq!(FlightComputerCommands::RevokeConsecutiveSamples as u8, 5);
        assert_eq!(FlightComputerCommands::ResetFailures as u8, 6);
        assert_eq!(FlightComputerCommands::RevokeResetFailures as u8, 7);
        assert_eq!(FlightComputerCommands::ValidateMeasms as u8, 8);
        assert_eq!(FlightComputerCommands::RevokeValidateMeasms as u8, 9);
    }

    #[cfg(not(feature = "testing"))]
    #[tokio::test]
    async fn postinit_command_routes_flight_command_zero() {
        let (db_tx, db_rx) = mpsc::channel(8);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;

        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let (radio_tx, mut radio_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        router.add_side_packed("rocket_comms", move |bytes| {
            radio_tx
                .send(bytes.to_vec())
                .map_err(|_| sedsnet::TelemetryError::HandlerError("radio tx closed"))
        });

        let shutdown_rx = state.shutdown_subscribe();
        let telemetry = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            shutdown_rx,
        ));

        state
            .cmd_tx
            .send(TelemetryCommand::Postinit)
            .await
            .expect("failed to send postinit command");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let Some(wire) = radio_rx.recv().await else {
                    panic!("flight command tx channel closed");
                };
                let pkt = serialize::unpack_packet(&wire)
                    .expect("failed to deserialize flight command packet");
                if pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                    assert_eq!(pkt.payload(), &[FlightComputerCommands::Postinit as u8]);
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for postinit flight-command packet");

        state.request_shutdown();
        telemetry.await.expect("telemetry task join failed");
    }

    #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
    #[tokio::test]
    async fn test_fire_launch_uses_fill_system_without_av_bay() {
        if launch_core_fixture_in_fresh_process("telemetry_task::tests::test_fire_launch_uses_fill_system_without_av_bay") { return; }
        test_fire_launch_uses_fill_system_without_av_bay_case().await;
    }

    #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
    async fn test_fire_launch_uses_fill_system_without_av_bay_case() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        // No AV-bay command peer is present. AB/VB readiness is genuine SDK
        // QUERY evidence; GB/DAQ operational qualification remains explicitly
        // MODELED by the existing qualification fixture, not hardware proof.
        let router=Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            crate::ignition_pair::fixture::handler(&state, |_|Ok(())),
        ]).with_sender("GS")));
        let qualification=crate::gse::armed_fixture_with_real_pair_for_test(&state,&router);
        assert!(!state.board_abort.ignition_pair_current());
        let actual_pair=crate::ignition_pair::fixture::Peers::warm_pair_only(&state,router.clone()).await;
        assert!(state.board_abort.ignition_pair_current());
        let launch_revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        handle_local_ground_station_launch_command(state.clone(), router.clone(), launch_revision)
            .await;
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&qualification).unwrap()).unwrap();
        assert_eq!(saved["qualification"]["fire_started"], true);
        state.reset_launch_clock();
        start_launch_clock_from_valve_sequence_status(&state);
        assert_eq!(
            state.launch_clock_snapshot().kind,
            LaunchClockKind::Idle,
            "old sequence status cannot re-arm a reset launch clock"
        );
        state.confirm_new_launch_cycle();
        start_launch_clock_from_valve_sequence_status(&state);
        assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::TMinus);
        std::fs::remove_file(qualification).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        let sent = actual_pair.pending_transport_wires();
        let packets: Vec<_> = sent
            .iter()
            .filter_map(|wire| serialize::unpack_packet(wire).ok())
            .collect();
        assert!(packets.iter().any(|packet| packet.data_type()
            == crate::telemetry_schema::data_type("VALVE_COMMAND")
            && packet.payload() == [ValveBoardCommands::Sequence as u8]));
        assert!(
            packets
                .iter()
                .all(|packet| packet.data_type()
                    != crate::telemetry_schema::data_type("FLIGHT_COMMAND")),
            "test-fire launch must not send commands to an absent flight computer"
        );
    }

    #[cfg(feature = "hitl_mode")]
    #[test]
    fn hitl_flight_command_mapping_matches_current_layout() {
        let cases = [
            (
                TelemetryCommand::Postinit,
                FlightComputerCommands::Postinit as u8,
            ),
            (
                TelemetryCommand::DeployParachute,
                FlightComputerCommands::DeployParachute as u8,
            ),
            (
                TelemetryCommand::ExpandParachute,
                FlightComputerCommands::ExpandParachute as u8,
            ),
            (
                TelemetryCommand::EvaluationRelax,
                FlightComputerCommands::EvaluationRelax as u8,
            ),
            (
                TelemetryCommand::EvaluationFocus,
                FlightComputerCommands::EvaluationFocus as u8,
            ),
            (
                TelemetryCommand::EvaluationAbort,
                FlightComputerCommands::EvaluationAbort as u8,
            ),
            (
                TelemetryCommand::ReinitSensors,
                FlightComputerCommands::ReinitSensors as u8,
            ),
            (
                TelemetryCommand::ReinitBarometer,
                FlightComputerCommands::ReinitBarometer as u8,
            ),
            (
                TelemetryCommand::EnableIMU,
                FlightComputerCommands::EnableIMU as u8,
            ),
            (
                TelemetryCommand::DisableIMU,
                FlightComputerCommands::DisableIMU as u8,
            ),
            (
                TelemetryCommand::AdvanceFlightState,
                FlightComputerCommands::AdvanceFlightState as u8,
            ),
            (
                TelemetryCommand::RewindFlightState,
                FlightComputerCommands::RewindFlightState as u8,
            ),
            (
                TelemetryCommand::AbortAfter40,
                FlightComputerCommands::AbortAfter40 as u8,
            ),
            (
                TelemetryCommand::AbortAfter100,
                FlightComputerCommands::AbortAfter100 as u8,
            ),
            (
                TelemetryCommand::AbortAfter250,
                FlightComputerCommands::AbortAfter250 as u8,
            ),
            (
                TelemetryCommand::ReinitAfter15,
                FlightComputerCommands::ReinitAfter15 as u8,
            ),
            (
                TelemetryCommand::ReinitAfter30,
                FlightComputerCommands::ReinitAfter30 as u8,
            ),
            (
                TelemetryCommand::ReinitAfter50,
                FlightComputerCommands::ReinitAfter50 as u8,
            ),
        ];

        for (telemetry_cmd, expected_id) in cases {
            assert_eq!(hitl_flight_command_id(&telemetry_cmd), Some(expected_id));
        }
    }

    #[test]
    fn queue_abort_packet_transmits_to_router_side() {
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sent_clone = sent.clone();
        let router = Router::new(sedsnet::router::RouterConfig::new([]));
        router.add_side_packed_with_options(
            "umbilical_comms",
            move |bytes| {
                sent_clone
                    .lock()
                    .expect("failed to lock sent packets")
                    .push(bytes.to_vec());
                Ok(())
            },
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );

        queue_abort_packet(&router, "Manual Abort Command Issued")
            .expect("failed to queue abort packet");
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process router queues");

        let sent = sent.lock().expect("failed to lock sent packets");
        let abort_packets = sent
            .iter()
            .filter_map(|wire| serialize::unpack_packet(wire).ok())
            .filter(|pkt| pkt.data_type() == crate::telemetry_schema::data_type("ABORT"))
            .collect::<Vec<_>>();
        assert_eq!(abort_packets.len(), 1);
        assert_eq!(abort_packets[0].payload(), b"Manual Abort Command Issued");
    }

    #[tokio::test]
    async fn exposed_flight_commands_transmit_to_the_discovered_rocket_side() {
        if crate::operator_queue::isolated_native_case(module_path!(),"exposed_flight_commands_transmit_to_the_discovered_rocket_side"){return;}
        crate::telemetry_schema::initialize().unwrap();
        // Match FC's existing compatibility protocol, including every visible
        // threshold control. These are isolated packet captures, not outputs.
        for command in [
            FlightComputerCommands::Postinit,
            FlightComputerCommands::Launch,
            FlightComputerCommands::MonitorAltitude,
            FlightComputerCommands::RevokeMonitorAltitude,
            FlightComputerCommands::ConsecutiveSamples,
            FlightComputerCommands::RevokeConsecutiveSamples,
            FlightComputerCommands::ResetFailures,
            FlightComputerCommands::RevokeResetFailures,
            FlightComputerCommands::ValidateMeasms,
            FlightComputerCommands::RevokeValidateMeasms,
            FlightComputerCommands::DeployParachute,
            FlightComputerCommands::ExpandParachute,
            FlightComputerCommands::EvaluationRelax,
            FlightComputerCommands::EvaluationFocus,
            FlightComputerCommands::EvaluationAbort,
            FlightComputerCommands::ReinitSensors,
            FlightComputerCommands::ReinitBarometer,
            FlightComputerCommands::EnableIMU,
            FlightComputerCommands::DisableIMU,
            #[cfg(feature = "hitl_mode")]
            FlightComputerCommands::AdvanceFlightState,
            #[cfg(feature = "hitl_mode")]
            FlightComputerCommands::RewindFlightState,
            FlightComputerCommands::AbortAfter40,
            FlightComputerCommands::AbortAfter100,
            FlightComputerCommands::AbortAfter250,
            FlightComputerCommands::ReinitAfter15,
            FlightComputerCommands::ReinitAfter30,
            FlightComputerCommands::ReinitAfter50,
        ] {
            let (db_tx, _db_rx) = mpsc::channel(8);
            let state = test_app_state(db_tx).await;
            let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let sent_clone = sent.clone();
            let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([crate::ignition_pair::fixture::handler(&state, |_| Ok(()))])));
            let rocket_side = router.add_side_packed_with_options(
                "rocket_comms",
                move |bytes| {
                    sent_clone
                        .lock()
                        .expect("failed to lock sent packets")
                        .push(bytes.to_vec());
                    Ok(())
                },
                sedsnet::router::RouterSideOptions {
                    reliable_enabled: true,
                    link_local_enabled: true,
                    ..Default::default()
                },
            );
            let discovery = sedsnet::discovery::build_discovery_announce(
                Board::RFBoard.sender_id(),
                123,
                &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            )
            .expect("failed to build RF discovery");
            let discovery_wire = serialize::pack_packet(&discovery);
            router
                .rx_packed_from_side(&discovery_wire, rocket_side)
                .expect("failed to queue RF discovery");
            router
                .process_all_queues_with_timeout(0)
                .expect("failed to process RF discovery");

            let _actual_peers = if matches!(command, FlightComputerCommands::Launch) {
                Some(crate::ignition_pair::fixture::Peers::warm(&state, router.clone()).await)
            } else { None };

            queue_locally_routed_flight_command(
                &state,
                &router,
                "test flight command",
                &[command as u8],
            )
            .expect("failed to queue flight command");
            router
                .process_all_queues_with_timeout(0)
                .expect("failed to process router queues");

            let sent = sent.lock().expect("failed to lock sent packets");
            let flight_packets = sent
                .iter()
                .filter_map(|wire| serialize::unpack_packet(wire).ok())
                .filter(|pkt| {
                    pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND")
                })
                .collect::<Vec<_>>();
            assert!(!flight_packets.is_empty());
            assert_eq!(flight_packets[0].payload(), &[command as u8]);
            assert!(
                flight_packets[0]
                    .endpoints()
                    .contains(&crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"))
            );
        }
    }

    #[tokio::test]
    async fn pending_close_is_not_confirmation_for_a_new_open() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Router::new(sedsnet::router::RouterConfig::new([]));
        let key = ValveBoardCommands::PilotOpen as u8;
        state.set_pending_umbilical_valve_state(key, false);
        queue_guarded_fill_command(
            &state,
            &router,
            crate::telemetry_schema::data_type("VALVE_COMMAND"),
            key,
            true,
            ValveBoardCommands::PilotOpen as u8,
            "Pilot open test",
        );
        assert_eq!(state.get_pending_umbilical_valve_state(key), Some(false));
        assert_eq!(state.get_umbilical_valve_state(key), None);
    }

    #[tokio::test]
    async fn opposite_valve_command_supersedes_unacknowledged_pending_target() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Router::new(sedsnet::router::RouterConfig::new([]));
        let key = ValveBoardCommands::PilotOpen as u8;

        state.set_pending_umbilical_valve_state(key, true);
        queue_guarded_fill_command(
            &state,
            &router,
            crate::telemetry_schema::data_type("VALVE_COMMAND"),
            key,
            false,
            ValveBoardCommands::PilotClose as u8,
            "Pilot close test",
        );

        assert_eq!(
            state.get_pending_umbilical_valve_state(key),
            Some(false),
            "a pending open must not suppress the operator's close command"
        );
    }

    #[tokio::test]
    async fn launch_command_also_queues_valve_board_sequence() {
        if launch_core_fixture_in_fresh_process("telemetry_task::tests::launch_command_also_queues_valve_board_sequence") { return; }
        launch_queue_case(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn launch_actual_sdk_core_busy_keeps_inhibition_and_reports_zero_owners() {
        if launch_core_fixture_in_fresh_process("telemetry_task::tests::launch_actual_sdk_core_busy_keeps_inhibition_and_reports_zero_owners") { return; }
        launch_queue_case(true).await;
    }

    // These three legacy cases own fresh processes, so process-wide checklist
    // STORE initialization and prior background tasks are independent. Their
    // original case bodies and all deadlines stay unchanged.
    fn launch_core_fixture_in_fresh_process(name: &str) -> bool {
        const CHILD: &str = "GS_LAUNCH_CORE_FIXTURE_CHILD";
        if std::env::var(CHILD).as_deref() == Ok("1") { return false; }
        struct OwnedRoot(std::path::PathBuf);
        impl Drop for OwnedRoot {
            fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
        }
        let root=OwnedRoot(std::env::temp_dir().join(format!("gs-launch-core-child-{:032x}",rand::random::<u128>())));
        std::fs::create_dir(&root.0).unwrap();
        let config=root.0.join("config");std::fs::create_dir(&config).unwrap();
        for name in ["flight_setup.json","fill_targets.json","gse_sequence.json","calibration_config.json","station_positions.json"] {
            let source=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config").join(name);
            if source.exists() {std::fs::copy(source,config.join(name)).unwrap();}
        }
        let stdout=root.0.join("stdout.log");let stderr=root.0.join("stderr.log");
        let output=std::fs::OpenOptions::new().write(true).create_new(true).open(&stdout).unwrap();
        let errors=std::fs::OpenOptions::new().write(true).create_new(true).open(&stderr).unwrap();
        let mut child=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact",name,"--nocapture","--test-threads=1"])
            .env(CHILD,"1").env("GS_SIM_MODE","0")
            .env("GS_LOADCELL_CALIBRATION_PATH",root.0.join("loadcell.json"))
            .env("GS_CHECKLISTS_FILE",root.0.join("checklists.json"))
            .env("GS_FLIGHT_SETUP_CONFIG",config.join("flight_setup.json"))
            .env("GS_FILL_TARGETS_CONFIG",config.join("fill_targets.json"))
            .env("GS_GSE_CONFIG",config.join("gse_sequence.json"))
            .env("GS_AUTO_ZERO_CONFIG",config.join("auto_zero.json"))
            .env("GS_CALIBRATION_CONFIG_PATH",config.join("calibration_config.json"))
            .env("GS_STATION_POSITIONS_PATH",config.join("station_positions.json"))
            .env("GS_GSE_QUALIFICATION",config.join("qualification.json"))
            .env("GS_ZERO_CAPTURE_DIR",root.0.join("zero-captures"))
            .stdout(std::process::Stdio::from(output))
            .stderr(std::process::Stdio::from(errors))
            .spawn().unwrap();
        // This is only a process-leak guard, not the original prepared/SDK budget.
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(30);
        let mut exceeded=false;
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now()>deadline || [&stdout,&stderr].into_iter().any(|p|std::fs::metadata(p).unwrap().len()>1_048_576) {
                exceeded=true;let _=child.kill();break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let status=child.wait().unwrap();
        let read=|path:&std::path::Path|{
            use std::io::Read;
            let mut bytes=Vec::new();std::fs::File::open(path).unwrap().take(1_048_577).read_to_end(&mut bytes).unwrap();
            assert!(bytes.len()<=1_048_576,"owned child output exceeded bound");String::from_utf8_lossy(&bytes).into_owned()
        };
        let text=format!("{}{}",read(&stdout),read(&stderr));
        assert!(!exceeded,"owned Launch fixture exceeded30s/output guard: {text}");
        assert!(status.success(),"fresh-process actual Launch fixture failed: {text}");
        assert!(text.contains("1 passed"),"child did not execute original case: {text}");
        println!("{text}");true
    }

    async fn launch_queue_case(force_core_busy: bool) {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sent_clone = sent.clone();
        let router = if force_core_busy {
            let owner = Arc::as_ptr(&state) as usize;
            let clock = std::time::Instant::now();
            Arc::new(Router::new_with_clock(
                sedsnet::router::RouterConfig::new([crate::ignition_pair::fixture::handler(&state, |_| Ok(()))]),
                Box::new(move || {
                    crate::gse::nonwaiting_admission_tests::clock_pause(owner);
                    clock.elapsed().as_millis() as u64
                }),
            ))
        } else {
            Arc::new(Router::new(sedsnet::router::RouterConfig::new([crate::ignition_pair::fixture::handler(&state, |_| Ok(()))])))
        };
        let valve_side = router.add_side_packed_with_options(
            "umbilical_comms",
            move |bytes| {
                sent_clone
                    .lock()
                    .expect("failed to lock sent packets")
                    .push(bytes.to_vec());
                Ok(())
            },
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let discovery = sedsnet::discovery::build_discovery_announce(
            Board::ValveBoard.sender_id(),
            123,
            &[crate::telemetry_schema::endpoint("VALVE_BOARD")],
        )
        .expect("failed to build valve-board discovery");
        router
            .rx_packed_from_side(&serialize::pack_packet(&discovery), valve_side)
            .expect("failed to queue valve-board discovery");
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process valve-board discovery");

        #[cfg(not(feature = "hitl_mode"))]
        let qualification = crate::gse::armed_fixture_for_test(&state, &router);
        let _actual_peers=crate::ignition_pair::fixture::Peers::warm(&state,router.clone()).await;
        let launch_revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        if force_core_busy {
            if state.topology_router.get().is_none() {
                state.topology_router.set(router.clone()).unwrap();
            }
            let pause = crate::gse::nonwaiting_admission_tests::Pause::install(&state);
            let owned_state = state.clone();
            let owned_router = router.clone();
            let worker = tokio::spawn(async move {
                handle_flight_computer_launch_command(owned_state, owned_router, launch_revision).await;
            });
            pause.prepared.recv_timeout(Duration::from_secs(2)).unwrap();
            let seed = Packet::new(
                crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "GS", 100, Arc::from([0u8]),
            ).unwrap();
            let held = router.clone();
            let holder = std::thread::Builder::new().name("production-sdk-core-holder".into())
                .spawn(move || held.seed_managed_variable(seed)).unwrap();
            pause.core_entered.recv_timeout(Duration::from_secs(2)).unwrap();
            pause.proceed.send(()).unwrap();
            let refused = tokio::time::timeout(Duration::from_secs(2), async {
                while !state.notifications_snapshot().iter().any(|n|
                    n.message.contains("Launch SDK admission accepted 0/") &&
                    n.message.contains("router core busy")) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }).await;
            pause.release();
            holder.join().unwrap().unwrap();
            worker.await.unwrap();
            assert!(refused.is_ok(), "actual nonwaiting core refusal was not reported: {:?}", state.notifications_snapshot());
            assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());
            assert!(router.export_reliable_pending_stats().iter().all(|entry|
                entry.data_type != crate::telemetry_schema::data_type("FLIGHT_COMMAND") &&
                entry.data_type != crate::telemetry_schema::data_type("VALVE_COMMAND")));
            eprintln!("LAUNCH_CORE_BUSY_ZERO_OWNERS: notifications={:?} SDK={:?}",
                state.notifications_snapshot(), router.export_runtime_stats());
        } else {
            handle_flight_computer_launch_command(state.clone(), router.clone(), launch_revision).await;
        }
        #[cfg(not(feature = "hitl_mode"))]
        {
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&qualification).unwrap()).unwrap();
            assert_eq!(saved["qualification"]["fire_started"], true);
            std::fs::remove_file(qualification).unwrap();
        }
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process launch command queues");

        let diagnostic_notifications = state.notifications_snapshot();
        let diagnostic_sdk = router.export_runtime_stats();
        let sent = sent.lock().expect("failed to lock sent packets");
        let valve_sequence_seen = sent
            .iter()
            .filter_map(|wire| serialize::unpack_packet(wire).ok())
            .any(|pkt| {
                pkt.data_type() == crate::telemetry_schema::data_type("VALVE_COMMAND")
                    && pkt.payload() == [ValveBoardCommands::Sequence as u8]
                    && pkt
                        .endpoints()
                        .contains(&crate::telemetry_schema::endpoint("VALVE_BOARD"))
            });
        if force_core_busy {
            assert!(!valve_sequence_seen, "zero SDK admission cannot fabricate a sequence wire");
            assert!(!sent.iter().filter_map(|wire| serialize::unpack_packet(wire).ok()).any(|packet|
                packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") && packet.payload() == [1]));
        } else {
            assert!(valve_sequence_seen,
                "normal launch should also queue valve sequence; notifications={:?} SDK={:?}",
                diagnostic_notifications, diagnostic_sdk);
        }
    }

    type SchedulerObservations = Arc<Mutex<Vec<(u8, bool)>>>;
    type InjectedRadioTx = (mpsc::UnboundedSender<(u8, Vec<u8>)>, Vec<u8>);
    struct TestRadioComms {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        scheduler_status: Option<SchedulerObservations>,
        windows: VecDeque<crate::comms::RadioWindowUpdate>,
        inject_tx_on_recv: Option<InjectedRadioTx>,
        fail_sends: usize,
    }

    impl CommsDevice for TestRadioComms {
        fn recv_packet(
            &mut self,
            _router: &Router,
            _packet_tap: &mut dyn FnMut(&Packet),
        ) -> sedsnet::TelemetryResult<()> {
            Ok(())
        }

        fn send_data(
            &mut self,
            payload: &[u8],
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            if self.fail_sends > 0 {
                self.fail_sends -= 1;
                return Err("injected radio send failure".into());
            }
            self.sent
                .lock()
                .expect("failed to lock sent radio payloads")
                .push(payload.to_vec());
            Ok(())
        }

        fn set_side_id(&mut self, _side_id: RouterSideId) {}

        fn recv_serialized_packets_with_budget(
            &mut self,
            _packet_sink: &mut dyn FnMut(Vec<u8>),
            _timeout: Duration,
            _max_packets: usize,
        ) -> sedsnet::TelemetryResult<()> {
            if let Some((tx, payload)) = self.inject_tx_on_recv.take() {
                tx.send((0, payload))
                    .expect("failed to inject radio worker tx during recv");
            }
            Ok(())
        }

        fn take_radio_window_update(&mut self) -> Option<crate::comms::RadioWindowUpdate> {
            self.windows.pop_front()
        }

        fn send_radio_scheduler_status(
            &mut self,
            seq: u8,
            has_more: bool,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            if let Some(status) = &self.scheduler_status {
                status
                    .lock()
                    .expect("failed to lock scheduler statuses")
                    .push((seq, has_more));
            }
            Ok(())
        }
    }

    const RFBOARD_SCHED_DOWNLINK: u8 = 0;
    const RFBOARD_SCHED_UPLINK: u8 = 1;
    const RFBOARD_SCHED_FLAG_HAS_MORE: u8 = 0x01;
    const RFBOARD_SCHED_FLAG_YIELD: u8 = 0x02;
    const RFBOARD_SCHED_BURST_MESSAGES: usize = 5;
    const RFBOARD_SCHED_DOWNLINK_TIMEOUT_MS: u64 = 300;
    const RFBOARD_SCHED_UPLINK_TIMEOUT_MS: u64 = 4000;
    const RFBOARD_SCHED_IDLE_UPLINK_POLL_MS: u64 = 50;
    const RFBOARD_SCHED_GRANT_RETRY_MS: u64 = 50;
    const RFBOARD_SCHED_GRANT_REANNOUNCE_MS: u64 = 5000;
    const RFBOARD_SCHED_TURNAROUND_MS: u64 = 75;
    const RFBOARD_SCHED_UPLINK_TO_DOWNLINK_TURNAROUND_MS: u64 = 150;
    const RFBOARD_SCHED_GS_TX_TURNAROUND_MS: u64 = 600;

    struct RfBoardScheduler {
        radio_turn: u8,
        radio_turn_seq: u8,
        radio_turn_sent: usize,
        radio_turn_grant_sent: bool,
        radio_turn_started_ms: u64,
        next_control_retry_ms: u64,
        next_grant_reannounce_ms: u64,
        next_idle_uplink_poll_ms: u64,
        now_ms: u64,
        started_at: std::time::Instant,
        gs_seq: u8,
        gs_flags: u8,
        gs_seen: bool,
    }

    impl RfBoardScheduler {
        fn new() -> Self {
            Self {
                radio_turn: RFBOARD_SCHED_DOWNLINK,
                radio_turn_seq: 0,
                radio_turn_sent: 0,
                radio_turn_grant_sent: false,
                radio_turn_started_ms: 0,
                next_control_retry_ms: 0,
                next_grant_reannounce_ms: 0,
                next_idle_uplink_poll_ms: 0,
                now_ms: 0,
                started_at: std::time::Instant::now(),
                gs_seq: 0,
                gs_flags: 0,
                gs_seen: false,
            }
        }

        fn handle_ground_station_status(&mut self, seq: u8, has_more: bool) {
            self.gs_seq = seq;
            self.gs_flags = RFBOARD_SCHED_FLAG_YIELD
                | if has_more {
                    RFBOARD_SCHED_FLAG_HAS_MORE
                } else {
                    0
                };
            self.gs_seen = true;
        }

        fn step(
            &mut self,
            rf_tx_queue: &mut VecDeque<Vec<u8>>,
            updates: &mut VecDeque<crate::comms::RadioWindowUpdate>,
            packet_sink: &mut dyn FnMut(Vec<u8>),
            max_packets: usize,
        ) {
            self.now_ms = self.started_at.elapsed().as_millis().max(1) as u64;

            // Copied from rfboard26/Core/Src/telemetry_thread.c:
            // emit an initial downlink grant, alternate downlink/uplink turns,
            // retry/reannounce grants, and leave uplink when GS yields or times out.
            if self.radio_turn_started_ms == 0 {
                self.radio_turn_started_ms = self.now_ms;
                self.radio_turn_seq = self.radio_turn_seq.wrapping_add(1);
                if self.emit_radio_grant(rf_tx_queue, updates) {
                    self.radio_turn_grant_sent = true;
                    self.next_grant_reannounce_ms = self.now_ms + RFBOARD_SCHED_GRANT_REANNOUNCE_MS;
                } else {
                    self.radio_turn_grant_sent = false;
                    self.next_control_retry_ms = self.now_ms + RFBOARD_SCHED_GRANT_RETRY_MS;
                }
            }

            if self.radio_turn == RFBOARD_SCHED_DOWNLINK {
                let downlink_timeout = self.now_ms.saturating_sub(self.radio_turn_started_ms)
                    >= RFBOARD_SCHED_DOWNLINK_TIMEOUT_MS;
                if self.radio_turn_sent >= RFBOARD_SCHED_BURST_MESSAGES
                    || downlink_timeout
                    || (rf_tx_queue.is_empty() && self.now_ms >= self.next_idle_uplink_poll_ms)
                {
                    self.radio_turn = RFBOARD_SCHED_UPLINK;
                    self.radio_turn_sent = 0;
                    self.radio_turn_seq = self.radio_turn_seq.wrapping_add(1);
                    self.radio_turn_grant_sent = false;
                    self.radio_turn_started_ms = self.now_ms;
                    self.next_control_retry_ms = self.now_ms + RFBOARD_SCHED_TURNAROUND_MS;
                    self.next_grant_reannounce_ms = self.now_ms
                        + RFBOARD_SCHED_TURNAROUND_MS
                        + RFBOARD_SCHED_GRANT_REANNOUNCE_MS;
                    self.next_idle_uplink_poll_ms = self.now_ms + RFBOARD_SCHED_IDLE_UPLINK_POLL_MS;
                    self.gs_seen = false;
                } else if self.radio_turn_grant_sent && max_packets > 0
                    && let Some(payload) = rf_tx_queue.pop_front() {
                        packet_sink(payload);
                        self.radio_turn_sent += 1;
                    }
            } else {
                let gs_done = self.gs_seen
                    && self.gs_seq == self.radio_turn_seq
                    && (self.gs_flags & RFBOARD_SCHED_FLAG_YIELD) != 0;
                let uplink_timeout = self.now_ms.saturating_sub(self.radio_turn_started_ms)
                    >= RFBOARD_SCHED_UPLINK_TIMEOUT_MS;
                if gs_done || uplink_timeout {
                    self.radio_turn = RFBOARD_SCHED_DOWNLINK;
                    self.radio_turn_sent = 0;
                    self.radio_turn_seq = self.radio_turn_seq.wrapping_add(1);
                    self.radio_turn_grant_sent = false;
                    self.radio_turn_started_ms = self.now_ms;
                    self.next_control_retry_ms =
                        self.now_ms + RFBOARD_SCHED_UPLINK_TO_DOWNLINK_TURNAROUND_MS;
                    self.next_grant_reannounce_ms = self.now_ms
                        + RFBOARD_SCHED_UPLINK_TO_DOWNLINK_TURNAROUND_MS
                        + RFBOARD_SCHED_GRANT_REANNOUNCE_MS;
                    self.gs_seen = false;
                }
            }

            if !self.radio_turn_grant_sent && self.now_ms >= self.next_control_retry_ms {
                if self.emit_radio_grant(rf_tx_queue, updates) {
                    self.radio_turn_grant_sent = true;
                    self.next_grant_reannounce_ms = self.now_ms + RFBOARD_SCHED_GRANT_REANNOUNCE_MS;
                } else {
                    self.radio_turn_grant_sent = false;
                    self.next_control_retry_ms = self.now_ms + RFBOARD_SCHED_GRANT_RETRY_MS;
                }
            } else if self.radio_turn_grant_sent
                && self.now_ms >= self.next_grant_reannounce_ms
                && (self.radio_turn == RFBOARD_SCHED_UPLINK
                    || (self.radio_turn == RFBOARD_SCHED_DOWNLINK && rf_tx_queue.is_empty()))
            {
                if self.emit_radio_grant(rf_tx_queue, updates) {
                    self.next_grant_reannounce_ms = self.now_ms + RFBOARD_SCHED_GRANT_REANNOUNCE_MS;
                } else {
                    self.next_grant_reannounce_ms = self.now_ms + RFBOARD_SCHED_GRANT_RETRY_MS;
                }
            }
        }

        fn emit_radio_grant(
            &self,
            rf_tx_queue: &VecDeque<Vec<u8>>,
            updates: &mut VecDeque<crate::comms::RadioWindowUpdate>,
        ) -> bool {
            let _flags = if rf_tx_queue.is_empty() {
                0
            } else {
                RFBOARD_SCHED_FLAG_HAS_MORE
            };
            updates.push_back(crate::comms::RadioWindowUpdate {
                kind: if self.radio_turn == RFBOARD_SCHED_UPLINK {
                    crate::comms::RadioWindowKind::UplinkOpen
                } else {
                    crate::comms::RadioWindowKind::DownlinkOpen
                },
                seq: self.radio_turn_seq,
                credit: RFBOARD_SCHED_BURST_MESSAGES,
                turnaround_ms: RFBOARD_SCHED_GS_TX_TURNAROUND_MS,
                received_at: std::time::Instant::now(),
            });
            true
        }
    }

    struct RfBoardSchedulerComms {
        sent_from_ground: Arc<Mutex<Vec<Vec<u8>>>>,
        ground_tx_turns: Arc<Mutex<Vec<u8>>>,
        scheduler_status: Arc<Mutex<Vec<(u8, bool)>>>,
        rf_tx_queue: VecDeque<Vec<u8>>,
        updates: VecDeque<crate::comms::RadioWindowUpdate>,
        scheduler: RfBoardScheduler,
    }

    impl CommsDevice for RfBoardSchedulerComms {
        fn recv_packet(
            &mut self,
            _router: &Router,
            _packet_tap: &mut dyn FnMut(&Packet),
        ) -> sedsnet::TelemetryResult<()> {
            Ok(())
        }

        fn send_data(
            &mut self,
            payload: &[u8],
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.sent_from_ground
                .lock()
                .expect("failed to lock ground tx packets")
                .push(payload.to_vec());
            self.ground_tx_turns
                .lock()
                .expect("failed to lock ground tx turns")
                .push(self.scheduler.radio_turn);
            Ok(())
        }

        fn set_side_id(&mut self, _side_id: RouterSideId) {}

        fn recv_serialized_packets_with_budget(
            &mut self,
            packet_sink: &mut dyn FnMut(Vec<u8>),
            _timeout: Duration,
            max_packets: usize,
        ) -> sedsnet::TelemetryResult<()> {
            self.scheduler.step(
                &mut self.rf_tx_queue,
                &mut self.updates,
                packet_sink,
                max_packets,
            );
            Ok(())
        }

        fn take_radio_window_update(&mut self) -> Option<crate::comms::RadioWindowUpdate> {
            self.updates.pop_front()
        }

        fn send_radio_scheduler_status(
            &mut self,
            seq: u8,
            has_more: bool,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.scheduler.handle_ground_station_status(seq, has_more);
            self.scheduler_status
                .lock()
                .expect("failed to lock scheduler statuses")
                .push((seq, has_more));
            Ok(())
        }
    }

    #[tokio::test]
    async fn rfboard_scheduler_drives_web_button_command_and_rf_data_paths() {
        let (db_tx, db_rx) = mpsc::channel(32);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        let mut policy = state.action_policy_snapshot();
        for control in policy.controls.iter_mut() {
            if control.cmd == "MonitorAltitude" {
                control.enabled = true;
            }
        }
        state.set_action_policy(policy);
        let mut ws_rx = state.ws_tx.subscribe();

        let handler_state = state.clone();
        let ground_station_handler = sedsnet::router::EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            move |pkt: &Packet| {
                handler_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                handler_state.mark_packet_received(get_current_timestamp_ms());
                let ingress_generation = crate::gse::automatic_generation(&handler_state);
                handler_state.ring_buffer.lock().unwrap().push(crate::ring_buffer::ReceivedPacket::Telemetry { packet: pkt.clone(), ingress_generation });
                Ok(())
            },
        );
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            ground_station_handler,
        ])));
        let (radio_tx, radio_rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
        let side_id = {
            let radio_tx = radio_tx.clone();
            router.add_side_packed_with_priority_and_options(
                "rocket_comms",
                move |bytes, priority| {
                    radio_tx
                        .send((priority, bytes.to_vec()))
                        .map_err(|_| sedsnet::TelemetryError::HandlerError("radio tx closed"))?;
                    Ok(())
                },
                sedsnet::router::RouterSideOptions {
                    // Raw UART reliability is handled by the RF-board scheduler, matching main.
                    reliable_enabled: false,
                    link_local_enabled: true,
                    ..Default::default()
                },
            )
        };
        let rf_discovery = sedsnet::discovery::build_discovery_announce(
            Board::RFBoard.sender_id(),
            1,
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
        )
        .expect("failed to build RF discovery");
        router
            .rx_packed_from_side(&serialize::pack_packet(&rf_discovery), side_id)
            .expect("failed to queue RF discovery");
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process RF discovery");

        let gps_values = [31.7619_f32, -106.485_f32, 1412.5_f32];
        let gps_pkt = Packet::new(
            crate::telemetry_schema::data_type("GPS_DATA"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::RFBoard.sender_id(),
            123_456,
            f32_payload(&gps_values),
        )
        .expect("failed to build RF GPS packet");
        let sent_from_ground = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let ground_tx_turns = Arc::new(Mutex::new(Vec::<u8>::new()));
        let scheduler_status = Arc::new(Mutex::new(Vec::<(u8, bool)>::new()));
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(RfBoardSchedulerComms {
                sent_from_ground: sent_from_ground.clone(),
                ground_tx_turns: ground_tx_turns.clone(),
                scheduler_status: scheduler_status.clone(),
                rf_tx_queue: VecDeque::from([serialize::pack_packet(&gps_pkt).to_vec()]),
                updates: VecDeque::new(),
                scheduler: RfBoardScheduler::new(),
            })));
        let shutdown_rx = state.shutdown_subscribe();
        let telemetry = tokio::spawn(telemetry_task(
            state.clone(),
            router.clone(),
            vec![CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: radio_rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            }],
            cmd_rx,
            db_rx,
            shutdown_rx,
        ));

        #[cfg(feature = "testing")]
        queue_locally_routed_flight_command(
            &state,
            &router,
            "MonitorAltitude test command",
            &[FlightComputerCommands::MonitorAltitude as u8],
        )
        .expect("failed to queue test command through router");
        #[cfg(not(feature = "testing"))]
        {
            state
                .cmd_tx
                .send(TelemetryCommand::MonitorAltitude)
                .await
                .expect("failed to send command through AppState command channel");
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if state
                        .last_command_ms
                        .lock()
                        .expect("failed to lock last command map")
                        .contains_key("MonitorAltitude")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("timed out waiting for telemetry task to accept MonitorAltitude command");
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row = ws_rx.recv().await.expect("telemetry websocket closed");
                if row.sender_id == Board::RFBoard.sender_id()
                    && row.data_type == *crate::telemetry_schema::data_type("GPS_DATA").as_str()
                {
                    assert_eq!(row.values, gps_values.map(Some).to_vec());
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for RF-board downlink telemetry");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                {
                    let sent = sent_from_ground
                        .lock()
                        .expect("failed to lock ground tx packets");
                    let flight_command_seen = sent
                        .iter()
                        .filter_map(|payload| serialize::unpack_packet(payload).ok())
                        .any(|pkt| {
                            pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND")
                                && pkt.payload() == [FlightComputerCommands::MonitorAltitude as u8]
                                && pkt.endpoints().contains(&crate::telemetry_schema::endpoint(
                                    "FLIGHT_CONTROLLER",
                                ))
                        });
                    if flight_command_seen {
                        break;
                    }
                    drop(sent);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for GS command during RF-board uplink");

        state.request_shutdown();
        telemetry.await.expect("telemetry task join failed");

        assert_eq!(
            state
                .latest_gps_fix_by_sender
                .lock()
                .unwrap()
                .get(Board::RFBoard.sender_id())
                .cloned(),
            Some(gps_values.map(Some).to_vec())
        );
        assert!(
            ground_tx_turns
                .lock()
                .expect("failed to lock ground tx turns")
                .iter()
                .all(|turn| *turn == RFBOARD_SCHED_UPLINK),
            "ground station must only transmit during RF-board uplink turns"
        );
    }

    #[tokio::test]
    async fn dedicated_radio_worker_sends_flight_command_during_uplink_window() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: None,
                windows: VecDeque::from([crate::comms::RadioWindowUpdate {
                    kind: crate::comms::RadioWindowKind::UplinkOpen,
                    seq: 1,
                    credit: 5,
                    turnaround_ms: 0,
                    received_at: std::time::Instant::now(),
                }]),
                inject_tx_on_recv: None,
                fail_sends: 0,
            })));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::pack_packet(&pkt).to_vec();
        tx.send((0, wire.clone()))
            .expect("failed to queue flight command to radio worker");
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                {
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                if sent_guard.iter().any(|payload| payload == &wire) {
                    break;
                }
                drop(sent_guard);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for radio worker send_data");

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        let flight_packets = sent_guard
            .iter()
            .filter_map(|payload| serialize::unpack_packet(payload).ok())
            .filter(|pkt| pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND"))
            .collect::<Vec<_>>();
        assert!(!flight_packets.is_empty());
        assert_eq!(
            flight_packets[0].payload(),
            &[FlightComputerCommands::MonitorAltitude as u8]
        );
        assert!(
            flight_packets[0]
                .endpoints()
                .contains(&crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"))
        );
    }

    #[tokio::test]
    async fn dedicated_radio_worker_drains_command_arriving_during_window_poll_before_yield() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let scheduler_status = Arc::new(Mutex::new(Vec::<(u8, bool)>::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::pack_packet(&pkt).to_vec();
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: Some(scheduler_status.clone()),
                windows: VecDeque::from([crate::comms::RadioWindowUpdate {
                    kind: crate::comms::RadioWindowKind::UplinkOpen,
                    seq: 9,
                    credit: 5,
                    turnaround_ms: 0,
                    received_at: std::time::Instant::now(),
                }]),
                inject_tx_on_recv: Some((tx.clone(), wire.clone())),
                fail_sends: 0,
            })));
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                {
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                let statuses = scheduler_status
                    .lock()
                    .expect("failed to lock scheduler statuses");
                if sent_guard.iter().any(|payload| payload == &wire)
                    && statuses.contains(&(9, false))
                {
                    break;
                }
                drop(statuses);
                drop(sent_guard);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for command injected during radio poll");

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        assert_eq!(sent_guard.as_slice(), &[wire]);
        drop(sent_guard);

        let statuses = scheduler_status
            .lock()
            .expect("failed to lock scheduler statuses");
        assert!(statuses.contains(&(9, false)));
    }

    #[tokio::test]
    async fn dedicated_radio_worker_waits_for_uplink_window_before_flight_command() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: None,
                windows: VecDeque::new(),
                inject_tx_on_recv: None,
                fail_sends: 0,
            })));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::pack_packet(&pkt).to_vec();
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tx.send((0, wire.clone()))
            .expect("failed to queue flight command to radio worker");

        tokio::time::sleep(Duration::from_millis(100)).await;

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        assert!(sent_guard.is_empty());
    }

    #[tokio::test]
    async fn dedicated_radio_worker_does_not_send_flight_command_during_downlink_window() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: None,
                windows: VecDeque::from([crate::comms::RadioWindowUpdate {
                    kind: crate::comms::RadioWindowKind::DownlinkOpen,
                    seq: 1,
                    credit: 5,
                    turnaround_ms: 0,
                    received_at: std::time::Instant::now(),
                }]),
                inject_tx_on_recv: None,
                fail_sends: 0,
            })));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::pack_packet(&pkt).to_vec();
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tx.send((0, wire))
            .expect("failed to queue flight command to radio worker");
        tokio::time::sleep(Duration::from_millis(100)).await;

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        assert!(sent_guard.is_empty());
    }

    #[tokio::test]
    async fn dedicated_radio_worker_prioritizes_flight_command_over_telemetry() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: None,
                windows: VecDeque::from([crate::comms::RadioWindowUpdate {
                    kind: crate::comms::RadioWindowKind::UplinkOpen,
                    seq: 1,
                    credit: 1,
                    turnaround_ms: 0,
                    received_at: std::time::Instant::now(),
                }]),
                inject_tx_on_recv: None,
                fail_sends: 0,
            })));
        let (tx, rx) = mpsc::unbounded_channel();
        let telemetry_pkt = Packet::new(
            crate::telemetry_schema::data_type("HEARTBEAT"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([]),
        )
        .expect("failed to build telemetry packet");
        let telemetry_wire = serialize::pack_packet(&telemetry_pkt).to_vec();
        let command_pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            124,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let command_wire = serialize::pack_packet(&command_pkt).to_vec();
        tx.send((1, telemetry_wire))
            .expect("failed to queue telemetry to radio worker");
        tx.send((0, command_wire.clone()))
            .expect("failed to queue flight command to radio worker");
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                {
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                if !sent_guard.is_empty() {
                    break;
                }
                drop(sent_guard);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for radio worker send_data");

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        assert_eq!(sent_guard.first(), Some(&command_wire));
    }

    #[tokio::test]
    async fn dedicated_radio_worker_retries_flight_command_after_send_failure() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: None,
                windows: VecDeque::from([
                    crate::comms::RadioWindowUpdate {
                        kind: crate::comms::RadioWindowKind::UplinkOpen,
                        seq: 1,
                        credit: 1,
                        turnaround_ms: 0,
                        received_at: std::time::Instant::now(),
                    },
                    crate::comms::RadioWindowUpdate {
                        kind: crate::comms::RadioWindowKind::UplinkOpen,
                        seq: 2,
                        credit: 1,
                        turnaround_ms: 0,
                        received_at: std::time::Instant::now(),
                    },
                ]),
                inject_tx_on_recv: None,
                fail_sends: 1,
            })));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::pack_packet(&pkt).to_vec();
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tx.send((0, wire.clone()))
            .expect("failed to queue flight command to radio worker");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                {
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                if sent_guard.iter().any(|payload| payload == &wire) {
                    break;
                }
                drop(sent_guard);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for retried radio worker send_data");

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        assert_eq!(sent_guard.as_slice(), &[wire]);
    }

    #[tokio::test]
    async fn dedicated_radio_worker_drops_stale_uplink_window_before_sending_command() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let stale_received_at = std::time::Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("failed to build stale receive time");
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: None,
                windows: VecDeque::from([crate::comms::RadioWindowUpdate {
                    kind: crate::comms::RadioWindowKind::UplinkOpen,
                    seq: 11,
                    credit: 5,
                    turnaround_ms: 0,
                    received_at: stale_received_at,
                }]),
                inject_tx_on_recv: None,
                fail_sends: 0,
            })));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::pack_packet(&pkt).to_vec();
        tx.send((0, wire))
            .expect("failed to queue flight command to radio worker");

        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tokio::time::sleep(Duration::from_millis(100)).await;

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        assert!(sent_guard.is_empty());
    }

    #[tokio::test]
    async fn dedicated_radio_worker_yields_rfboard_uplink_after_credit_is_used() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let side_id = router.add_side_packed_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let scheduler_status = Arc::new(Mutex::new(Vec::<(u8, bool)>::new()));
        let comms: Arc<Mutex<Box<dyn CommsDevice>>> =
            Arc::new(Mutex::new(Box::new(TestRadioComms {
                sent: sent.clone(),
                scheduler_status: Some(scheduler_status.clone()),
                windows: VecDeque::from([crate::comms::RadioWindowUpdate {
                    kind: crate::comms::RadioWindowKind::UplinkOpen,
                    seq: 7,
                    credit: 5,
                    turnaround_ms: 0,
                    received_at: std::time::Instant::now(),
                }]),
                inject_tx_on_recv: None,
                fail_sends: 0,
            })));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::MonitorAltitude as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::pack_packet(&pkt).to_vec();
        let expected_window_sends = 5usize;
        for _ in 0..(expected_window_sends + 1) {
            tx.send((0, wire.clone()))
                .expect("failed to queue flight command to radio worker");
        }

        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                tx_budget: None,
                name: "rocket_comms",
                comms,
                tx_comms: None,
                side_id,
                tx_rx: rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: true,
            },
        )
        .expect("failed to spawn radio workers");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                {
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                let statuses = scheduler_status
                    .lock()
                    .expect("failed to lock scheduler statuses");
                if sent_guard.len() >= expected_window_sends
                    && statuses.contains(&(7, true))
                {
                    break;
                }
                drop(statuses);
                drop(sent_guard);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for RF-board uplink credit yield");

        state.request_shutdown();
        for worker in workers {
            worker.join().expect("radio worker panicked");
        }

        let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
        assert_eq!(sent_guard.len(), expected_window_sends);
        assert!(sent_guard.iter().all(|payload| payload == &wire));
        drop(sent_guard);

        let statuses = scheduler_status
            .lock()
            .expect("failed to lock scheduler statuses");
        assert!(statuses.contains(&(7, true)));
    }

    #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
    #[tokio::test]
    async fn advance_flight_state_command_reaches_remote_router_end_to_end() {
        let (db_tx, db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let remote_states = Arc::new(Mutex::new(Vec::<u8>::new()));
        let remote_states_handler = remote_states.clone();
        let mut policy = state.action_policy_snapshot();
        for control in policy.controls.iter_mut() {
            if matches!(
                control.cmd.as_str(),
                "AdvanceFlightState" | "RewindFlightState"
            ) {
                control.enabled = true;
            }
        }
        state.set_action_policy(policy);

        let gs_router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let timeline = flight_state_timeline::Trace::start(&gs_router);
        let peer_timeline = timeline.clone();
        let remote_router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            sedsnet::router::EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                move |pkt: &Packet| {
                    if let Some(state_code) = pkt.payload().first().copied() {
                        peer_timeline.packet(pkt, state_code);
                        remote_states_handler
                            .lock()
                            .expect("failed to lock remote states")
                            .push(state_code);
                    }
                    Ok(())
                },
            ),
        ])));
        // Production main establishes UTC before admitting state changes.
        refresh_host_network_time(&gs_router, get_current_timestamp_ms());
        assert!(crate::system_clock::required_control_network_utc_ms(&gs_router).is_ok());

        let gs_peer = Arc::new(Mutex::new(None::<(Arc<Router>, RouterSideId)>));
        let remote_peer = Arc::new(Mutex::new(None::<(Arc<Router>, RouterSideId)>));

        let gs_side = {
            let gs_peer = gs_peer.clone();
            let wire_trace = timeline.clone();
            gs_router.add_side_packed_with_options(
                "umbilical_comms",
                move |bytes| {
                    let (peer, ingress) = gs_peer
                        .lock()
                        .expect("failed to lock gs peer")
                        .clone()
                        .expect("gs peer not initialized");
                    let token = wire_trace.wire_begin("GS_to_peer", bytes);
                    let result = peer.rx_packed_from_side(bytes, ingress);
                    wire_trace.wire_end(token, &result);
                    result
                },
                sedsnet::router::RouterSideOptions {
                    reliable_enabled: true,
                    link_local_enabled: true,
                    ..Default::default()
                },
            )
        };
        let remote_side = {
            let remote_peer = remote_peer.clone();
            let wire_trace = timeline.clone();
            remote_router.add_side_packed_with_options(
                "gs_link",
                move |bytes| {
                    let (peer, ingress) = remote_peer
                        .lock()
                        .expect("failed to lock remote peer")
                        .clone()
                        .expect("remote peer not initialized");
                    let token = wire_trace.wire_begin("peer_to_GS", bytes);
                    let result = peer.rx_packed_from_side(bytes, ingress);
                    wire_trace.wire_end(token, &result);
                    result
                },
                sedsnet::router::RouterSideOptions {
                    reliable_enabled: true,
                    link_local_enabled: true,
                    ..Default::default()
                },
            )
        };
        *gs_peer.lock().expect("failed to lock gs peer") =
            Some((remote_router.clone(), remote_side));
        *remote_peer.lock().expect("failed to lock remote peer") =
            Some((gs_router.clone(), gs_side));

        remote_router
            .announce_discovery()
            .expect("failed to queue remote discovery");
        remote_router
            .process_all_queues_with_timeout(0)
            .expect("failed to process remote discovery");

        state
            .topology_router
            .set(gs_router.clone())
            .expect("failed to set topology router");

        let shutdown_rx = state.shutdown_subscribe();
        let telemetry = tokio::spawn(telemetry_task(
            state.clone(),
            gs_router,
            Vec::new(),
            cmd_rx,
            db_rx,
            shutdown_rx,
        ));

        tokio::time::sleep(Duration::from_millis(25)).await;
        let start_state = state.local_flight_state_snapshot();
        let commands = (0..16)
            .map(|idx| {
                if idx % 8 >= 4 {
                    TelemetryCommand::RewindFlightState
                } else {
                    TelemetryCommand::AdvanceFlightState
                }
            })
            .collect::<Vec<_>>();
        let mut expected_states = Vec::with_capacity(commands.len());
        let mut current = start_state;
        for cmd in &commands {
            current = match cmd {
                TelemetryCommand::AdvanceFlightState => {
                    operator_mode_adjacent_flight_state(current, 1)
                }
                TelemetryCommand::RewindFlightState => {
                    operator_mode_adjacent_flight_state(current, -1)
                }
                _ => current,
            };
            expected_states.push(current as u8);
        }

        for (cmd, expected) in commands.into_iter().zip(expected_states.iter()) {
            timeline.event("enqueue_begin", *expected, None, None, None);
            // Match authenticated ingress: relative state commands retain the
            // original guard revision and dispatch generation in the queue.
            let queued = TelemetryCommand::QueuedOperator {
            recovery_origin: None,
                generation: crate::gse::automatic_generation(&state),
                launch_revision: crate::board_abort::capture_launch_revision_named(
                    &state,
                    crate::sequences::command_name(&cmd),
                ),
                plumbing_origin: None,
                command: Box::new(cmd),
            };
            cmd_tx
                .send(queued)
                .await
                .expect("failed to send flight state command");
            timeline.event("enqueue_end", *expected, None, None, None);
        }

        timeline.event("wait_1500ms_begin", 255, None, None, None);
        let remote_result = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                // The GS has its real router worker above. Service the peer's
                // deferred ingress too, as a running remote board would.
                remote_router.process_all_queues_with_timeout(0)
                    .expect("failed to service remote flight-state ingress");
                {
                    let states = remote_states.lock().expect("failed to lock remote states");
                    if states.len() >= expected_states.len() {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;

        timeline.event("wait_1500ms_return", 255, None, None, Some(remote_result.is_ok()));
        state.request_shutdown();
        telemetry.await.expect("telemetry task join failed");
        timeline.event("post_shutdown_join", state.local_flight_state_snapshot() as u8, None, None, None);
        // Captured by the test harness on success, available for ANY unchanged
        // ordering/local-state/deadline assertion failure below.
        let report = timeline.report();
        flight_state_timeline::write_artifact_if_requested(&report)
            .expect("failed to write requested owned flight-state trace artifact");
        eprintln!("TF_STATE_ORIGINAL_DEADLINE_TIMELINE={}", report);

        assert_eq!(
            state.local_flight_state_snapshot() as u8,
            *expected_states.last().expect("expected states missing")
        );
        assert!(remote_result.is_ok(), "timed out waiting for remote flight state: expected={expected_states:?}, actual={:?}", remote_states.lock().expect("failed to lock remote states"));
        let states = remote_states.lock().expect("failed to lock remote states");
        assert_eq!(states.as_slice(), expected_states.as_slice());
    }

    #[cfg(feature = "hitl_mode")]
    #[tokio::test]
    async fn advance_flight_state_command_reaches_flight_computer_command_path() {
        let (db_tx, db_rx) = mpsc::channel(8);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        crate::gse::qualified_launch_proofs_for_test(&state);
        crate::gse::handle_command_with_origins(&state,&TelemetryCommand::ToggleGroundStationControl,None,None);
        let mut state_rx = state.state_tx.subscribe();
        let mut policy = state.action_policy_snapshot();
        for control in policy.controls.iter_mut() {
            if control.cmd == "AdvanceFlightState" {
                control.enabled = true;
            }
        }
        state.set_action_policy(policy);

        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let (radio_tx, mut radio_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        router.add_side_packed("rocket_comms", move |bytes| {
            radio_tx
                .send(bytes.to_vec())
                .map_err(|_| sedsnet::TelemetryError::HandlerError("radio tx closed"))
        });

        let shutdown_rx = state.shutdown_subscribe();
        let telemetry = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            shutdown_rx,
        ));

        state
            .cmd_tx
            .send(TelemetryCommand::QueuedOperator {
            recovery_origin: None,
                generation: crate::gse::automatic_generation(&state),
                launch_revision: crate::board_abort::capture_launch_revision_named(
                    &state,
                    "AdvanceFlightState",
                ),
                plumbing_origin: None,
                command: Box::new(crate::operator_phase::seal(&state,TelemetryCommand::AdvanceFlightState)),
            })
            .await
            .expect("failed to send advance flight state command");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let Some(wire) = radio_rx.recv().await else {
                    panic!("flight command tx channel closed");
                };
                let pkt = serialize::unpack_packet(&wire)
                    .expect("failed to deserialize flight command packet");
                if pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                    assert_eq!(
                        pkt.payload(),
                        &[FlightComputerCommands::AdvanceFlightState as u8]
                    );
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for advance flight state flight-command packet");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state.local_flight_state_snapshot() == FlightState::Idle {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for local flight state advance");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let msg = state_rx
                    .recv()
                    .await
                    .expect("flight state broadcast channel closed");
                if msg.state == FlightState::Idle {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for flight state broadcast");

        state.request_shutdown();
        telemetry.await.expect("telemetry task join failed");
    }

    #[tokio::test]
    async fn queued_automatic_abort_claim_reaches_packet_callback_without_gse_lock() {
        // Testing mode normally consumes Abort inside its synthetic FlightSim.
        // Disable only that simulator in a fresh process, before its OnceLock
        // initializes, so this remains an actual Router dispatch proof in HITL
        // and cannot race another test's process-global simulator state.
        #[cfg(feature = "testing")]
        {
            const CHILD: &str = "GS_AUTOMATIC_ABORT_ACTUAL_DISPATCH_CHILD";
            const NAME: &str = "telemetry_task::tests::queued_automatic_abort_claim_reaches_packet_callback_without_gse_lock";
            if std::env::var_os(CHILD).is_none() {
                let result = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                    .env(CHILD, "1")
                    .env("GS_SIM_MODE", "0")
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                return;
            }
            assert!(!flight_sim::sim_mode_enabled());
        }
        let (db_tx, db_rx) = mpsc::channel(16);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        let weak = Arc::downgrade(&state);
        let (delivered, mut received) = mpsc::unbounded_channel::<()>();
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            sedsnet::router::EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("ABORT"),
                move |packet| {
                    if packet.data_type() == crate::telemetry_schema::data_type("ABORT") {
                        assert!(
                            weak.upgrade().unwrap().gse.try_lock().is_ok(),
                            "automatic router callback held GSE lock"
                        );
                        delivered.send(()).unwrap();
                    }
                    Ok(())
                },
            ),
        ])));
        #[cfg(not(feature = "hitl_mode"))]
        let path = crate::gse::armed_fixture_for_test(&state, &router);
        #[cfg(feature = "hitl_mode")]
        state.set_local_flight_state(FlightState::Armed);
        let shutdown = state.shutdown_subscribe();
        let worker = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            shutdown,
        ));
        state
            .cmd_tx
            .send(TelemetryCommand::AutomaticAbort {
                generation: crate::gse::automatic_generation(&state),
            })
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), received.recv()).await;
        state.request_shutdown();
        worker.await.unwrap();
        assert_eq!(result.unwrap(), Some(()));
        assert!(state.is_command_allowed(&TelemetryCommand::Abort));
        #[cfg(not(feature = "hitl_mode"))]
        {
            let record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(record["automatic_abort_claimed"], true);
            assert_eq!(
                state.gse.lock().unwrap().engine.status.phase,
                gse_sequence::Phase::Fault
            );
            assert!(crate::gse::commit_fire_intent(&state).is_err());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn queued_automatic_abort_rechecks_launch_boundary_without_blocking_manual_abort() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        *state.state.lock().unwrap() = FlightState::Idle;
        assert!(matches!(
            command_at_dispatch(
                &state,
                TelemetryCommand::AutomaticAbort {
                    generation: crate::gse::automatic_generation(&state)
                }
            ),
            Some(TelemetryCommand::Abort)
        ));
        *state.state.lock().unwrap() = FlightState::Launch;
        assert!(
            command_at_dispatch(
                &state,
                TelemetryCommand::AutomaticAbort {
                    generation: crate::gse::automatic_generation(&state)
                }
            )
            .is_none(),
            "all modes must suppress automatic abort after Launch"
        );
        for command in [
            "AutomaticAbort",
            "AutomaticNitrogenClose",
            "AutomaticNitrousClose",
        ] {
            assert!(
                serde_json::from_value::<TelemetryCommand>(serde_json::json!(command)).is_err()
            );
        }
        assert!(matches!(
            command_at_dispatch(&state, TelemetryCommand::Abort),
            Some(TelemetryCommand::Abort)
        ));
        assert!(serde_json::from_str::<TelemetryCommand>("\"AutomaticAbort\"").is_err());
    }

    #[tokio::test]
    async fn actual_worker_retires_old_operator_valve_and_launch_before_accepting_current_intent() {
        let (db_tx, db_rx) = mpsc::channel(8);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        let old = crate::gse::automatic_generation(&state);
        state.automatic_generation.store(old + 1, Ordering::Release);
        *state.gse.lock().unwrap() = crate::gse::Runtime::default();
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        let worker = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            state.shutdown_subscribe(),
        ));
        for command in [TelemetryCommand::Launch, TelemetryCommand::Nitrogen] {
            state
                .cmd_tx
                .send(TelemetryCommand::QueuedOperator {
            recovery_origin: None,
                    launch_revision: None,
                    plumbing_origin: None,
                    generation: old,
                    command: Box::new(command),
                })
                .await
                .unwrap();
        }
        state
            .cmd_tx
            .send(TelemetryCommand::QueuedOperator {
            recovery_origin: None,
                launch_revision: None,
                plumbing_origin: None,
                generation: old + 1,
                command: Box::new(TelemetryCommand::Postinit),
            })
            .await
            .unwrap();
        let progress = tokio::time::timeout(Duration::from_secs(2), async {
            while !state
                .last_command_ms
                .lock()
                .unwrap()
                .contains_key("Postinit")
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        state.request_shutdown();
        worker.await.unwrap();
        progress.expect("current intent must reach the actual command worker");
        let accepted = state.last_command_ms.lock().unwrap();
        assert!(!accepted.contains_key("Launch"));
        assert!(!accepted.contains_key("Nitrogen"));
        assert!(accepted.contains_key("Postinit"));
        assert_eq!(crate::gse::automatic_generation(&state), old + 1);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }

    #[cfg(feature = "hitl_mode")]
    #[tokio::test]
    async fn explicit_fc_evaluation_abort_reaches_actual_worker_while_manual_stop_is_retained() {
        let (db_tx, db_rx) = mpsc::channel(8);
        let (state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
        if state.hitl_button_interlock_enabled() {
            state.toggle_hitl_button_interlock();
        }
        assert!(state.is_command_allowed(&TelemetryCommand::EvaluationAbort));
        state.manual_abort_intent.request();
        state.set_abort_indicator_latched(true);
        let (sent, mut received) = mpsc::unbounded_channel();
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            sedsnet::router::EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                move |packet| {
                    if packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                        let _ = sent.send(packet.payload().to_vec());
                    }
                    Ok(())
                },
            ),
        ])));
        state.topology_router.set(router.clone()).unwrap();
        let worker = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            cmd_rx,
            db_rx,
            state.shutdown_subscribe(),
        ));
        state
            .cmd_tx
            .send(TelemetryCommand::QueuedOperator {
            recovery_origin: None,
                launch_revision: None,
                plumbing_origin: None,
                generation: crate::gse::automatic_generation(&state),
                command: Box::new(TelemetryCommand::EvaluationAbort),
            })
            .await
            .unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(2), received.recv()).await;
        state.request_shutdown();
        worker.await.unwrap();
        assert_eq!(delivered.unwrap(), Some(vec![16]));
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }

    include!("igniter_sense_ingress_tests.rs");

    async fn test_app_state(db_tx: mpsc::Sender<DbQueueItem>) -> Arc<AppState> {
        test_app_state_with_cmd_rx(db_tx).await.0
    }

    async fn test_app_state_with_cmd_rx(
        db_tx: mpsc::Sender<DbQueueItem>,
    ) -> (Arc<AppState>, mpsc::Receiver<TelemetryCommand>) {
        reset_parse_error_reports();
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to open in-memory telemetry db");
        let auth_db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to open in-memory auth db");
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let (ws_tx, _ws_rx) = broadcast::channel(16);
        let (state_tx, _state_rx) = broadcast::channel(4);
        let (board_status_tx, _board_status_rx) = broadcast::channel(4);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(4);
        let (notifications_tx, _notifications_rx) = broadcast::channel(4);
        let (messages_tx, _messages_rx) = broadcast::channel(4);
        let (action_policy_tx, _action_policy_rx) = broadcast::channel(4);
        let (fill_targets_tx, _fill_targets_rx) = broadcast::channel(4);
        let (launch_clock_tx, _launch_clock_rx) = broadcast::channel(4);
        let (recording_status_tx, _recording_status_rx) = broadcast::channel(4);

        let mut board_status = HashMap::new();
        for board in Board::ALL {
            board_status.insert(
                *board,
                crate::state::BoardStatus {
                    packet_count: 0,
                    last_seen_ms: None,
                    last_seen_model_ms: None,
                    last_seen_instant: None,
                    ema_gap_ms: None,
                    warned: false,
                },
            );
        }

        let condition_runtime = Arc::new(crate::alert_condition_runtime::Runtime::for_test());
        let state = Arc::new(AppState {
            fill_recording: Default::default(),
            control_ingress: Arc::new(Default::default()),
            ring_buffer: Arc::new(Mutex::new(RingBuffer::new(128))),
            cmd_tx,
            ws_tx,
            warnings_tx: broadcast::channel(4).0,
            errors_tx: broadcast::channel(4).0,
            alert_ack_state: Arc::new(Mutex::new(crate::web::AlertAckStateMsg::default())),
            alert_conditions: condition_runtime.clone(),
            alert_ack_tx: broadcast::channel(4).0,
            dashboard_reset_tx: broadcast::channel(4).0,
            db: Arc::new(Mutex::new(db)),
            db_path: Arc::new(Mutex::new("sqlite::memory:".to_string())),
            placeholder_db_path: condition_runtime.test_placeholder_path(),
            db_queue_tx: db_tx,
            db_write_runtime: tokio::runtime::Handle::current(),
            auth_db,
            state: Arc::new(Mutex::new(FlightState::Startup)),
            state_tx,
            gpio: GpioPins::new(),
            board_status: Arc::new(Mutex::new(board_status)),
            board_status_tx,
            last_board_status_broadcast_ms: Arc::new(AtomicU64::new(0)),
            last_packet_rx_ms: Arc::new(AtomicU64::new(0)),
            umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
            pending_umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
            latest_fuel_tank_pressure: Arc::new(Mutex::new(None)),
            gse: Arc::new(Mutex::new(crate::gse::Runtime::default())),
        phase_control_selected: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        phase_control_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        phase_control_public: Arc::new(Mutex::new(None)),
            sequence_progress: Arc::new(crate::sequence_progress_service::Shared::new(
                rand::random::<u128>(),
            )),
            automatic_generation: Arc::new(AtomicU64::new(1)),
            operator_admission_barrier: Arc::new(AtomicU64::new(0)),
            operator_queue: Arc::new(crate::operator_queue::Control::default()),
            operator_dispatch_inflight: Arc::new(AtomicUsize::new(0)),
            manual_abort_intent: Arc::new(crate::manual_abort_intent::ManualAbortIntent::default()),
            deferred_abort: Arc::new(crate::deferred_abort::Inbox::default()),
            igniter_sense: Arc::new(std::sync::Mutex::new(crate::igniter_sense::Tracker::default())),
            board_recovery: Arc::new(crate::board_recovery::Runtime::default()),
            board_abort: Arc::new(crate::board_abort::Runtime::default()),
            launch_dedup_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            board_abort_tx: broadcast::channel(16).0,
            latest_fill_mass_kg: Arc::new(Mutex::new(None)),
            loadcell_processing: Arc::new(crate::loadcell_zero::Service::default()),
            network_traffic: Arc::new(Mutex::new(crate::network_traffic::Sampler::default())),
            network_health: Arc::new(Mutex::new(crate::network_health::Tracker::default())),
            loadcell_calibration: Arc::new(Mutex::new(loadcell::load_or_default())),
            auto_zero: Default::default(),
            daq_log_session: Default::default(),
            shutdown_tx,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            pending_db_writes: Arc::new(AtomicUsize::new(0)),
            ignition_observer: Arc::new(Default::default()),
        transport_tx_activity: Arc::new(Default::default()),
            db_write_notify: Arc::new(Notify::new()),
            notifications: Arc::new(Mutex::new(Vec::new())),
            notifications_tx,
            next_notification_id: Arc::new(AtomicU64::new(0)),
            messages: Arc::new(Mutex::new(Vec::new())),
            messages_tx,
            next_message_id: Arc::new(AtomicU64::new(0)),
            action_policy: Arc::new(Mutex::new(default_action_policy())),
            sequence_policy_state: Arc::new(Mutex::new(
                crate::sequences::SequencePolicyState::default(),
            )),
            action_policy_tx,
            fill_targets: Arc::new(Mutex::new(fill_targets::load_or_default())),
            fill_targets_tx,
            plumbing_drive: Arc::new(crate::actuator_plumbing::Runtime::default()),
            launch_history: Arc::new(Mutex::new(crate::launch_history::Store::ephemeral())),
            launch_clock: Arc::new(Mutex::new(LaunchClockMsg::idle())),
            launch_clock_tx,
            launch_sequence_command_pending: Arc::new(AtomicBool::new(false)),
            launch_indicator_latched: Arc::new(AtomicBool::new(false)),
            abort_indicator_latched: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "hitl_mode")]
            hitl_button_interlock_enabled: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "hitl_mode")]
            hitl_launch_interlock_enabled: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "hitl_mode")]
            hitl_safety_interlocks: crate::state::default_hitl_safety_interlocks(),
            #[cfg(feature = "hitl_mode")]
            hitl_physical_launch_uses_ground_station: Arc::new(AtomicBool::new(false)),
            recording_status: Arc::new(Mutex::new(RecordingStatusMsg {
                mode: RecordingModeWire::Idle,
                db_path: None,
            })),
            recording_status_tx,
            last_command_ms: Arc::new(Mutex::new(HashMap::new())),
            fill_sequence_continue_requests: Arc::new(AtomicU64::new(0)),
            recent_telemetry_cache: Arc::new(Mutex::new(VecDeque::new())),
            latest_gps_fix_by_sender: Arc::new(Mutex::new(HashMap::new())),
            latest_gps_satellites_by_sender: Arc::new(Mutex::new(HashMap::new())),
            recent_alerts_cache: Arc::new(Mutex::new(VecDeque::new())),
            av_bay_comms_connected: Arc::new(AtomicBool::new(false)),
            fill_comms_connected: Arc::new(AtomicBool::new(false)),
            topology_router: Arc::new(OnceLock::new()),
            kg50_input_sync: Default::default(),
            auth: Arc::new(AuthManager::new(PathBuf::from(
                "/tmp/groundstation-test-users.json",
            ))),
        });

        (state, cmd_rx)
    }

    fn f32_payload(values: &[f32]) -> Arc<[u8]> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>()
            .into()
    }

    #[tokio::test]
    async fn rf_gps_packets_become_graphable_telemetry_rows() {
        let (db_tx, mut db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let gps_values = [31.7619_f32, -106.485_f32, 1412.5_f32];
        let gps_pkt = Packet::new(
            crate::telemetry_schema::data_type("GPS_DATA"),
            &[
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                crate::telemetry_schema::endpoint("SD_CARD"),
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            ],
            Board::RFBoard.sender_id(),
            123_456,
            f32_payload(&gps_values),
        )
        .expect("failed to build RF GPS_DATA packet");

        let row = handle_packet(&state, &db_tx, &db_overflow, gps_pkt)
            .await
            .into_iter()
            .next()
            .expect("RF GPS_DATA packet should produce telemetry row");

        assert_eq!(
            row.data_type,
            crate::telemetry_schema::data_type("GPS_DATA")
                .as_str()
                .to_string()
        );
        assert_eq!(row.sender_id, Board::RFBoard.sender_id());
        assert_eq!(row.values.len(), 3);
        for (actual, expected) in row.values.iter().zip(gps_values) {
            assert_eq!(actual.unwrap(), expected);
        }
        assert_eq!(
            state
                .latest_gps_fix_by_sender
                .lock()
                .unwrap()
                .get(Board::RFBoard.sender_id())
                .cloned(),
            Some(row.values.clone())
        );

        let write = db_rx.recv().await.expect("GPS_DATA should queue DB write");
        match write {
            DbQueueItem::Write(DbWrite::Telemetry {
                data_type,
                sender_id,
                values_json,
                ..
            }) => {
                assert_eq!(
                    data_type,
                    crate::telemetry_schema::data_type("GPS_DATA")
                        .as_str()
                        .to_string()
                );
                assert_eq!(sender_id, Board::RFBoard.sender_id());
                assert_eq!(
                    serde_json::from_str::<Vec<Option<f32>>>(&values_json.unwrap()).unwrap(),
                    row.values
                );
            }
            other => panic!("unexpected DB item for GPS_DATA: {other:?}"),
        }

        let sat_pkt = Packet::new(
            crate::telemetry_schema::data_type("GPS_SATELLITE_NUMBER"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::RFBoard.sender_id(),
            123_789,
            Arc::from([14_u8]),
        )
        .expect("failed to build RF GPS_SATELLITE_NUMBER packet");

        let sat_row = handle_packet(&state, &db_tx, &db_overflow, sat_pkt)
            .await
            .into_iter()
            .next()
            .expect("RF GPS_SATELLITE_NUMBER packet should produce telemetry row");

        assert_eq!(sat_row.data_type, GPS_SATELLITES_DATA_TYPE);
        assert_eq!(sat_row.sender_id, Board::RFBoard.sender_id());
        assert_eq!(sat_row.values, vec![Some(14.0)]);
        assert_eq!(
            state
                .latest_gps_satellites_by_sender
                .lock()
                .unwrap()
                .get(Board::RFBoard.sender_id())
                .copied(),
            Some(14)
        );
    }

    #[tokio::test]
    async fn imu_packets_are_split_into_accel_and_gyro_rows() {
        let (db_tx, mut db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let imu_values = [0.2_f32, -0.1_f32, 9.91_f32, 1.5_f32, -2.5_f32, 3.5_f32];
        let imu_pkt = Packet::new(
            crate::telemetry_schema::data_type("IMU_DATA"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::FlightComputer.sender_id(),
            234_567,
            f32_payload(&imu_values),
        )
        .expect("failed to build IMU_DATA packet");

        let mut rows = handle_packet(&state, &db_tx, &db_overflow, imu_pkt).await;
        rows.sort_by(|a, b| a.data_type.cmp(&b.data_type));

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].data_type,
            crate::telemetry_schema::data_type("ACCEL_DATA")
                .as_str()
                .to_string()
        );
        assert_eq!(rows[0].values, vec![Some(0.2), Some(-0.1), Some(9.91)]);
        assert_eq!(
            rows[1].data_type,
            crate::telemetry_schema::data_type("GYRO_DATA")
                .as_str()
                .to_string()
        );
        assert_eq!(rows[1].values, vec![Some(1.5), Some(-2.5), Some(3.5)]);

        let mut writes = Vec::new();
        for _ in 0..2 {
            match db_rx.recv().await.expect("IMU rows should queue DB writes") {
                DbQueueItem::Write(DbWrite::Telemetry {
                    data_type,
                    sender_id,
                    values_json,
                    ..
                }) => {
                    writes.push((data_type, sender_id, values_json.unwrap()));
                }
                other => panic!("unexpected DB item for IMU split path: {other:?}"),
            }
        }
        writes.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            writes[0].0,
            crate::telemetry_schema::data_type("ACCEL_DATA")
                .as_str()
                .to_string()
        );
        assert_eq!(writes[0].1, Board::FlightComputer.sender_id());
        assert_eq!(
            serde_json::from_str::<Vec<Option<f32>>>(&writes[0].2).unwrap(),
            vec![Some(0.2), Some(-0.1), Some(9.91)]
        );
        assert_eq!(
            writes[1].0,
            crate::telemetry_schema::data_type("GYRO_DATA")
                .as_str()
                .to_string()
        );
        assert_eq!(writes[1].1, Board::FlightComputer.sender_id());
        assert_eq!(
            serde_json::from_str::<Vec<Option<f32>>>(&writes[1].2).unwrap(),
            vec![Some(1.5), Some(-2.5), Some(3.5)]
        );
    }

    #[tokio::test]
    async fn selected_1000kg_fill_uses_absolute_mass_without_changing_recorded_weight() {
        let (db_tx, _) = mpsc::channel(16);
        let state = test_app_state(db_tx.clone()).await;
        // This raw==kg numeric oracle owns its calibration; do not inherit a
        // physical installation's saved fit from the stage runtime directory.
        *state.loadcell_calibration.lock().unwrap() = loadcell::LoadcellCalibrationFile::default();
        state.loadcell_calibration.lock().unwrap().full_mass_kg = None;
        let mut targets = state.fill_targets_snapshot();
        targets.nitrous.target_mass_kg = 10.; // Original percentage oracle uses a 10 kg target.
        targets.fill_source = crate::fill_targets::FillSource::Kg1000Absolute;
        state.set_fill_targets(targets);
        let overflow = test_db_overflow();
        let mut ws = state.ws_tx.subscribe();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("KG1000"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::DaqBoard.sender_id(),
            123_400,
            f32_payload(&[-4.25]),
        )
        .unwrap();
        let raw = handle_packet(&state, &db_tx, &overflow, pkt).await;
        assert_eq!(raw[0].values, vec![Some(-4.25)]);
        assert_eq!(ws.try_recv().unwrap().values, vec![Some(-4.25)]);
        assert_eq!(ws.try_recv().unwrap().values, vec![Some(42.5)]);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(4.25));
        // A changed target changes only the derived percentage, never the
        // recorded signed sensor value or the selected absolute mass.
        let mut targets = state.fill_targets_snapshot();
        targets.nitrous.target_mass_kg = 8.;
        state.set_fill_targets(targets);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(4.25));
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("KG1000"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::DaqBoard.sender_id(),
            123_450,
            f32_payload(&[-4.25]),
        )
        .unwrap();
        let raw = handle_packet(&state, &db_tx, &overflow, pkt).await;
        assert_eq!(raw[0].values, vec![Some(-4.25)]);
        assert_eq!(ws.try_recv().unwrap().values, vec![Some(-4.25)]);
        assert_eq!(ws.try_recv().unwrap().values, vec![Some(53.125)]);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(4.25));
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("KG50"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::DaqBoard.sender_id(),
            123_500,
            f32_payload(&[20.]),
        )
        .unwrap();
        handle_packet(&state, &db_tx, &overflow, pkt).await;
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(4.25));
        let mut targets = state.fill_targets_snapshot();
        targets.fill_source = crate::fill_targets::FillSource::Kg50;
        state.set_fill_targets(targets);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn stable_zero_and_disabled_auto_zero_do_not_delay_start_fill() {
        let (db_tx, _) = mpsc::channel(4);
        let state = test_app_state(db_tx).await;
        // This raw==kg numeric oracle owns its calibration; do not inherit a
        // physical installation's saved fit from the stage runtime directory.
        *state.loadcell_calibration.lock().unwrap() = loadcell::LoadcellCalibrationFile::default();
        let mut targets = state.fill_targets_snapshot();
        targets.fill_source = crate::fill_targets::FillSource::Kg1000Absolute;
        state.set_fill_targets(targets);
        {
            let mut rt = state.auto_zero.lock().unwrap();
            *rt = crate::auto_zero::Runtime::testing(true);
            for _ in 0..200 {
                rt.observe("KG1000", 0.1);
            }
        }
        assert!(!crate::auto_zero::before_fill(
            &state,
            &TelemetryCommand::StartFill
        ));
        assert!(!state.auto_zero.lock().unwrap().pending());
        *state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(false);
        assert!(!crate::auto_zero::before_fill(
            &state,
            &TelemetryCommand::StartFill
        ));
        crate::auto_zero::launch(&state);
        assert!(!state.auto_zero.lock().unwrap().pending());
    }

    #[tokio::test]
    async fn launch_zero_capture_is_nonblocking_and_retains_tare_without_automatic_abort() {
        let (db_tx, _) = mpsc::channel(4);
        let (state, mut commands) = test_app_state_with_cmd_rx(db_tx).await;
        *state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
        let start = std::time::Instant::now();
        crate::auto_zero::launch(&state);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(state.auto_zero.lock().unwrap().pending());
        tokio::time::timeout(
            crate::auto_zero::acquisition_budget_for_test() + Duration::from_secs(1),
            async {
                while state.auto_zero.lock().unwrap().pending() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(commands.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "missing tare samples must never enqueue an automatic Abort"
        );
    }

    #[tokio::test]
    async fn kg50_packet_records_raw_and_calibrated_and_updates_fill_mass() {
        let (db_tx, mut db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        *state.latest_fill_mass_kg.lock().unwrap() = Some(123.0);
        state
            .loadcell_calibration
            .lock()
            .unwrap()
            .extra_channels
            .insert(
                "kg50".into(),
                loadcell::GenericCalibrationChannel {
                    linear: loadcell::ChannelLinear {
                        m: Some(2.0),
                        b: Some(1.0),
                    },
                    ..Default::default()
                },
            );
        let mut ws_rx = state.ws_tx.subscribe();
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("KG50"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::DaqBoard.sender_id(),
            456_789,
            f32_payload(&[4.25]),
        )
        .unwrap();
        let rows = handle_packet(&state, &db_tx, &db_overflow, pkt).await;
        assert_eq!(rows[0].data_type, "KG50");
        assert_eq!(rows[0].values, vec![Some(4.25)]);
        let calibrated = ws_rx.try_recv().unwrap();
        assert_eq!(calibrated.data_type, loadcell::DERIVED_WEIGHT_50_DATA_TYPE);
        assert_eq!(calibrated.values, vec![Some(9.5)]);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(9.5));
        for expected in [
            "KG50",
            loadcell::DERIVED_WEIGHT_50_DATA_TYPE,
            loadcell::DERIVED_FILL_PERCENT_DATA_TYPE,
        ] {
            match db_rx.try_recv().unwrap() {
                DbQueueItem::Write(DbWrite::Telemetry {
                    data_type,
                    source_timestamp_ms,
                    ..
                }) => {
                    assert_eq!(data_type, expected);
                    assert_eq!(source_timestamp_ms, Some(456_789));
                }
                other => panic!("unexpected DB item: {other:?}"),
            }
        }
        match db_rx.try_recv().unwrap() {
            DbQueueItem::Write(write @ DbWrite::Calibration { .. }) => {
                let db = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect("sqlite::memory:")
                    .await
                    .unwrap();
                insert_db_batch_once(&db, &[write]).await.unwrap();
                let json: String = sqlx::query_scalar(
                    "SELECT config_json FROM calibration_history WHERE sensor_id='KG50'",
                )
                .fetch_one(&db)
                .await
                .unwrap();
                let metadata: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(
                    metadata["calibration"]["extra_channels"]["kg50"]["linear"]["m"],
                    2.0
                );
                db.close().await;
            }
            other => panic!("missing recorded regression: {other:?}"),
        }
        assert!(
            state
                .recent_telemetry_snapshot()
                .iter()
                .any(|r| r.data_type == loadcell::DERIVED_WEIGHT_50_DATA_TYPE)
        );
    }

    #[cfg(feature = "test_fire_mode")]
    #[tokio::test]
    async fn chamber_testfire_csv_joins_actual_recorded_rows_without_duplicates_or_old_pressure() {
        let (db_tx, mut db_rx) = mpsc::channel(128);
        let state = test_app_state(db_tx.clone()).await;
        let directory = std::env::temp_dir().join(format!(
            "gs-chamber-csv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("samples.db");
        let csv_path = directory.join("samples.csv");
        let (db, _) = crate::telemetry_db::open_telemetry_db(&db_path)
            .await
            .unwrap();
        for (kind, source, values) in [
            ("KG1000", 100_u64, vec![1.0_f32]),
            ("DAQ_LOADCELL_CHAMBER", 200, vec![4.25, 1000.0, 5.0]),
            ("KG1000", 300, vec![2.0]),
            ("DAQ_LOADCELL_CHAMBER", 400, vec![3.0, f32::NAN, 5.0]),
        ] {
            let packet = Packet::new(
                crate::telemetry_schema::data_type(kind),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "DAQ",
                source,
                f32_payload(&values),
            )
            .unwrap();
            handle_packet(&state, &db_tx, &test_db_overflow(), packet).await;
        }
        let mut writes = Vec::new();
        while let Ok(DbQueueItem::Write(write)) = db_rx.try_recv() {
            writes.push(write);
        }
        insert_db_batch_once(&db, &writes).await.unwrap();
        let combined: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM telemetry WHERE data_type='DAQ_LOADCELL_CHAMBER'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let projected: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM telemetry WHERE data_type='COMBUSTION_CHAMBER_PRESSURE'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            (combined, projected),
            (2, 1),
            "actual ingestion stored both valid representations but no invalid PT projection"
        );
        let received: i64 = sqlx::query_scalar(
            "SELECT timestamp_ms FROM telemetry WHERE data_type='COMBUSTION_CHAMBER_PRESSURE'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let expected_rx: String =
            sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%f', ? / 1000.0, 'unixepoch')")
                .bind(received)
                .fetch_one(&db)
                .await
                .unwrap();
        // Duplicate projections are the same observation, not extra samples.
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) SELECT timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json FROM telemetry WHERE data_type IN ('COMBUSTION_CHAMBER_PRESSURE','COMBUSTION_CHAMBER_VOLTAGE')")
            .execute(&db).await.unwrap();
        // Equal clocks alone cannot join an unrelated original payload.
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) SELECT timestamp_ms,source_timestamp_ms,sender_id,data_type,'[1234.0]','[255]' FROM telemetry WHERE data_type='COMBUSTION_CHAMBER_PRESSURE' LIMIT 1")
            .execute(&db).await.unwrap();
        db.close().await;
        let mut calibration = crate::loadcell::LoadcellCalibrationFile::default();
        calibration.extra_channels.insert(
            "kg50".into(),
            crate::loadcell::GenericCalibrationChannel {
                linear: crate::loadcell::ChannelLinear {
                    m: Some(1.0),
                    b: Some(0.0),
                },
                ..Default::default()
            },
        );
        crate::test_fire_csv::export_recording_csv(
            db_path.to_str().unwrap(),
            &csv_path,
            &calibration,
        )
        .await
        .unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let header: Vec<_> = csv.lines().next().unwrap().split(',').collect();
        assert_eq!(
            header[..15].join(","),
            "Rx_Timestamp,Header,Seq,Timestamp,1000kg Raw,Tank Pressure Raw,Battery Voltage,CRC,1000kg Calibrated,Weight,Thrust,Tank Pressure Calibrated,50kg Raw,50kg Calibrated,ADC Temperature C"
        );
        assert_eq!(
            &header[15..],
            &["Chamber Pressure (psig)", "Chamber Connector Voltage (V)"]
        );
        let data: Vec<Vec<_>> = csv
            .lines()
            .skip(3)
            .map(|line| line.split(',').collect())
            .collect();
        assert_eq!(
            data.len(),
            4,
            "one original loadcell sample each; combined and projection are not extra samples"
        );
        assert!(data.iter().all(|row| row.len() == 17));
        assert_eq!(&data[0][15..], &["", ""], "absent pressure is not zero");
        assert_eq!(&data[1][15..], &["1000", "5"]);
        assert_eq!(
            data[1][0], expected_rx,
            "actual PT packet receive clock retained"
        );
        assert_eq!(data[1][3], "200", "actual PT packet source clock retained");
        assert_eq!(
            data[1][4], "4.25",
            "existing raw loadcell remains unchanged"
        );
        assert_eq!(
            &data[2][15..],
            &["", ""],
            "previous PT must not carry into a later independent sample"
        );
        assert_eq!(data[2][3], "300");
        assert_eq!(
            &data[3][15..],
            &["", ""],
            "invalid PT does not become a pressure or voltage measurement"
        );
        assert_eq!(
            data[3][4], "3",
            "invalid PT retains the independent loadcell sample"
        );
        // Conflicting values for the exact original sample cannot be resolved
        // by row order. Both PT columns must remain unknown, not last-writer-wins.
        let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
            .await
            .unwrap();
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) SELECT timestamp_ms,source_timestamp_ms,sender_id,data_type,'[999.0]',payload_json FROM telemetry WHERE data_type='COMBUSTION_CHAMBER_PRESSURE' AND payload_json != '[255]' LIMIT 1")
            .execute(&db).await.unwrap();
        db.close().await;
        crate::test_fire_csv::export_recording_csv(
            db_path.to_str().unwrap(),
            &csv_path,
            &calibration,
        )
        .await
        .unwrap();
        let conflict_csv = std::fs::read_to_string(&csv_path).unwrap();
        let conflict_rows: Vec<Vec<_>> = conflict_csv
            .lines()
            .skip(3)
            .map(|line| line.split(',').collect())
            .collect();
        assert_eq!(conflict_rows.len(), 4);
        assert_eq!(
            &conflict_rows[1][15..],
            &["", ""],
            "ambiguous same-packet PT is unknown"
        );
        assert_eq!(
            &conflict_rows[1][..15],
            &data[1][..15],
            "existing loadcell/calibration/clocks unchanged by PT ambiguity"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn chamber_packet_keeps_loadcell_calibration_and_persists_pressure_with_both_clocks() {
        let (db_tx, mut db_rx) = mpsc::channel(16);
        let state = test_app_state(db_tx.clone()).await;
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("DAQ_LOADCELL_CHAMBER"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            987654,
            f32_payload(&[4.25, 1000.0, 5.0]),
        )
        .unwrap();
        // Exercise the DAQ's actual packed f32 triplet before backend ingestion.
        let packed = sedsnet::wire_format::pack_packet(&pkt);
        let pkt = sedsnet::wire_format::unpack_packet(&packed).unwrap();
        let rows = handle_packet(&state, &db_tx, &test_db_overflow(), pkt).await;
        let mut ws = state.ws_tx.subscribe();
        publish_received_telemetry_rows(&state, rows.clone());
        let mut broadcast = Vec::new();
        while let Ok(row) = ws.try_recv() {
            broadcast.push(row);
        }
        for kind in [
            "KG1000",
            "COMBUSTION_CHAMBER_PRESSURE",
            "COMBUSTION_CHAMBER_VOLTAGE",
        ] {
            assert!(
                broadcast.iter().any(|row| row.data_type == kind),
                "missing live {kind}"
            );
            assert!(
                state
                    .recent_telemetry_snapshot()
                    .iter()
                    .any(|row| row.data_type == kind),
                "missing reconnect {kind}"
            );
        }
        assert!(rows.iter().any(
            |r| r.data_type == "COMBUSTION_CHAMBER_PRESSURE" && r.values == vec![Some(1000.0)]
        ));
        assert!(
            rows.iter()
                .any(|r| r.data_type == "KG1000" && r.values == vec![Some(4.25)])
        );
        let db = crate::telemetry_db::open_in_memory_telemetry_db()
            .await
            .unwrap();
        let mut writes = Vec::new();
        while let Ok(DbQueueItem::Write(write)) = db_rx.try_recv() {
            writes.push(write);
        }
        assert!(
            writes.len() >= 5,
            "combined,raw loadcell,derived mass,pressure,voltage"
        );
        insert_db_batch_once(&db, &writes).await.unwrap();
        let saved:(i64,i64,String,String,String)=sqlx::query_as(
            "SELECT timestamp_ms,source_timestamp_ms,sender_id,values_json,payload_json FROM telemetry WHERE data_type='COMBUSTION_CHAMBER_PRESSURE'")
            .fetch_one(&db).await.unwrap();
        assert!(saved.0 > 0);
        assert_eq!(saved.1, 987654);
        assert_eq!(saved.2, "DAQ");
        assert_eq!(
            serde_json::from_str::<Vec<f32>>(&saved.3).unwrap(),
            vec![1000.0]
        );
        assert!(!saved.4.is_empty());
        db.close().await;
    }

    #[tokio::test]
    async fn kg1000_packet_emits_state_tab_loadcell_rows() {
        let (db_tx, mut db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        // This raw==kg numeric oracle owns its calibration; do not inherit a
        // physical installation's saved fit from the stage runtime directory.
        *state.loadcell_calibration.lock().unwrap() = loadcell::LoadcellCalibrationFile::default();
        let db_overflow = test_db_overflow();
        let mut ws_rx = state.ws_tx.subscribe();

        let pkt = Packet::new(
            crate::telemetry_schema::data_type("KG1000"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::DaqBoard.sender_id(),
            456_789,
            f32_payload(&[4.25]),
        )
        .expect("failed to build KG1000 packet");

        let row = handle_packet(&state, &db_tx, &db_overflow, pkt)
            .await
            .into_iter()
            .next()
            .expect("KG1000 packet should produce raw telemetry row");

        assert_eq!(row.data_type, loadcell::RAW_LOADCELL_DATA_TYPE_1000KG);
        assert_eq!(row.sender_id, Board::DaqBoard.sender_id());
        assert_eq!(row.values, vec![Some(4.25)]);

        let mut broadcast_rows = Vec::new();
        for _ in 0..1 {
            broadcast_rows.push(
                ws_rx
                    .recv()
                    .await
                    .expect("derived loadcell rows should be broadcast"),
            );
        }
        broadcast_rows.sort_by(|a, b| a.data_type.cmp(&b.data_type));

        assert_eq!(
            broadcast_rows
                .iter()
                .map(|row| row.data_type.as_str())
                .collect::<Vec<_>>(),
            vec![loadcell::DERIVED_WEIGHT_DATA_TYPE]
        );
        assert_eq!(broadcast_rows[0].sender_id, Board::DaqBoard.sender_id());
        assert_eq!(broadcast_rows[0].values, vec![Some(4.25)]);
        assert_eq!(broadcast_rows[0].timestamp_ms, row.timestamp_ms);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), None);

        let cache = state.recent_telemetry_snapshot();
        assert!(
            cache
                .iter()
                .any(|row| row.data_type == loadcell::DERIVED_WEIGHT_DATA_TYPE)
        );
        assert!(
            !cache
                .iter()
                .any(|row| row.data_type == loadcell::DERIVED_FILL_PERCENT_DATA_TYPE)
        );

        let mut db_types = Vec::new();
        for _ in 0..2 {
            match db_rx
                .recv()
                .await
                .expect("telemetry DB write should be queued")
            {
                DbQueueItem::Write(DbWrite::Telemetry { data_type, .. }) => {
                    db_types.push(data_type);
                }
                other => panic!("unexpected DB item for KG1000 path: {other:?}"),
            }
        }
        db_types.sort();
        assert_eq!(
            db_types,
            vec![
                loadcell::RAW_LOADCELL_DATA_TYPE_1000KG.to_string(),
                loadcell::DERIVED_WEIGHT_DATA_TYPE.to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn kg1000_nonidentity_conversion_preserves_raw_row_and_calibrates_fill_mass() {
        let (db_tx, _) = mpsc::channel(16);
        let state = test_app_state(db_tx.clone()).await;
        let calibration = loadcell::LoadcellCalibrationFile {
            ch1: loadcell::ChannelLinear { m: Some(2.0), b: Some(1.0) },
            ch1_fit: None,
            ..Default::default()
        };
        *state.loadcell_calibration.lock().unwrap() = calibration;
        let mut targets = state.fill_targets_snapshot();
        targets.fill_source = crate::fill_targets::FillSource::Kg1000Absolute;
        targets.nitrous.target_mass_kg = 10.0;
        state.set_fill_targets(targets);
        let mut ws = state.ws_tx.subscribe();
        let packet = Packet::new(
            crate::telemetry_schema::data_type("KG1000"),
            &[crate::telemetry_schema::endpoints::GROUND_STATION],
            Board::DaqBoard.sender_id(), 456_789, f32_payload(&[4.25]),
        ).unwrap();
        let raw = handle_packet(&state, &db_tx, &test_db_overflow(), packet).await;
        assert_eq!(raw[0].values, vec![Some(4.25)]);
        assert_eq!(ws.try_recv().unwrap().values, vec![Some(9.5)]);
        assert_eq!(ws.try_recv().unwrap().values, vec![Some(95.0)]);
        assert_eq!(*state.latest_fill_mass_kg.lock().unwrap(), Some(9.5));
    }

    #[tokio::test]
    async fn stable_raw_stream_with_physical_polynomial_load_does_not_bypass_auto_zero_limit() {
        let (db_tx, _) = mpsc::channel(4);
        let state = test_app_state(db_tx).await;
        // Exact fit/zero from the physical seed in the retained failed run.
        // A stable raw voltage is not a near-zero calibrated load.
        let calibration: loadcell::LoadcellCalibrationFile = serde_json::from_str(r#"{
            "ch1":{"m":-1568.1659,"b":-3.873501},
            "ch1_fit":{"type":"poly2","a":4922774.0,"b":-1568.1659,"c":-3.873501},
            "ch1_zero_raw":0.00023022853
        }"#).unwrap();
        assert!(loadcell::calibrated_weight_kg(&calibration, "KG1000", 0.1).unwrap() > 49_000.0);
        let original = serde_json::to_value(&calibration).unwrap();
        *state.loadcell_calibration.lock().unwrap() = calibration;
        let mut targets = state.fill_targets_snapshot();
        targets.fill_source = crate::fill_targets::FillSource::Kg1000Absolute;
        state.set_fill_targets(targets);
        {
            let mut zero = state.auto_zero.lock().unwrap();
            *zero = crate::auto_zero::Runtime::testing(true);
            for _ in 0..200 { zero.observe("KG1000", 0.1); }
        }
        assert!(crate::auto_zero::before_fill(&state, &TelemetryCommand::StartFill),
            "a stable but loaded physical calibration must intercept StartFill");
        assert!(!state.auto_zero.lock().unwrap().pending(),
            "without an admitted fill continuation no automatic tare may be started");
        assert_eq!(serde_json::to_value(&*state.loadcell_calibration.lock().unwrap()).unwrap(), original);
    }

    #[tokio::test]
    async fn fuel_tank_pressure_updates_latest_with_calibrated_value() {
        let (db_tx, _db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let mut ws_rx = state.ws_tx.subscribe();

        {
            let mut cfg = state.loadcell_calibration.lock().unwrap();
            cfg.iadc.m = Some(2.0);
            cfg.iadc.b = Some(5.0);
            cfg.iadc_fit = None;
        }

        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FUEL_TANK_PRESSURE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::DaqBoard.sender_id(),
            567_890,
            f32_payload(&[370.0]),
        )
        .expect("failed to build fuel tank pressure packet");

        let row = handle_packet(&state, &db_tx, &db_overflow, pkt)
            .await
            .into_iter()
            .next()
            .expect("fuel tank pressure packet should produce raw telemetry row");

        assert_eq!(
            row.data_type,
            crate::telemetry_schema::data_type("FUEL_TANK_PRESSURE")
                .as_str()
                .to_string()
        );
        assert_eq!(row.values, vec![Some(370.0)]);
        assert!(
            row.timestamp_ms > 567_890,
            "raw pressure row should use ground-station ingest time, not stale board timestamp"
        );

        let derived = ws_rx
            .recv()
            .await
            .expect("derived calibrated pressure row should be broadcast");
        assert_eq!(
            derived.data_type,
            loadcell::DERIVED_PRESSURE_TRANSDUCER_CALIBRATED_DATA_TYPE
        );
        assert_eq!(derived.values, vec![Some(745.0)]);
        assert_eq!(derived.timestamp_ms, row.timestamp_ms);
        assert_eq!(
            *state.latest_fuel_tank_pressure.lock().unwrap(),
            Some(745.0)
        );
    }

    #[tokio::test]
    async fn gateway_battery_voltage_packet_matches_fill_box_layout_sender() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let mut ws_rx = state.ws_tx.subscribe();

        let pkt = Packet::new(
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GB",
            678_901,
            f32_payload(&[14.4]),
        )
        .expect("failed to build gateway BATTERY_VOLTAGE packet");

        let row = handle_packet(&state, &db_tx, &db_overflow, pkt)
            .await
            .into_iter()
            .next()
            .expect("gateway battery packet should produce raw telemetry row");

        assert_eq!(
            row.data_type,
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE")
                .as_str()
                .to_string()
        );
        assert_eq!(row.sender_id, Board::GatewayBoard.sender_id());
        assert_eq!(row.values, vec![Some(14.4)]);
        assert!(
            row.timestamp_ms > 678_901,
            "gateway battery row should use ground-station ingest time, not stale board timestamp"
        );

        let mut derived_rows = Vec::new();
        for _ in 0..3 {
            derived_rows.push(
                ws_rx
                    .recv()
                    .await
                    .expect("derived battery rows should be broadcast"),
            );
        }
        derived_rows.sort_by(|a, b| a.data_type.cmp(&b.data_type));

        assert_eq!(
            derived_rows
                .iter()
                .map(|row| row.data_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "FILL_BOX_POWER_DROP_RATE_V_PER_MIN",
                "FILL_BOX_POWER_PERCENT",
                "FILL_BOX_POWER_REMAINING_MINUTES",
            ]
        );
        assert!(derived_rows.iter().all(|row| row.sender_id == "GB"));
        assert!(
            derived_rows
                .iter()
                .all(|derived| derived.timestamp_ms == row.timestamp_ms)
        );
    }

    #[tokio::test]
    async fn compact_power_sender_is_resolved_before_battery_grouping() {
        let (db_tx, _db_rx) = mpsc::channel(16);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let mut ws_rx = state.ws_tx.subscribe();
        // Presentation mode precedence is Testing, HITL, TestFire, Normal.
        // A TestFire feature alone does not select that layout in mixed builds.
        let expected_mode = if cfg!(feature = "testing") {
            crate::station_mode::Mode::Testing
        } else if cfg!(feature = "hitl_mode") {
            crate::station_mode::Mode::Hitl
        } else if cfg!(feature = "test_fire_mode") {
            crate::station_mode::Mode::TestFire
        } else {
            crate::station_mode::Mode::Normal
        };
        assert_eq!(crate::station_mode::local_mode(), expected_mode);
        let selected_layout = crate::layout::load_layout().unwrap();
        assert_eq!(selected_layout.test_fire_mode, expected_mode == crate::station_mode::Mode::TestFire);
        let power_sources: Vec<_> = selected_layout.battery.sources.iter().filter(|source| {
            source.sender_id == "PB" && source.input_data_type == "BATTERY_VOLTAGE"
        }).collect();
        if expected_mode == crate::station_mode::Mode::TestFire {
            assert!(power_sources.is_empty(), "standalone TestFire omits the AV-bay battery source");
        } else {
            assert_eq!(power_sources.len(), 1, "full layouts configure one PB battery source");
            assert_eq!(power_sources[0].percent_data_type, "AV_BAY_BATTERY_PERCENT");
        }

        let ground_router = Arc::new(Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        ));
        let power_router = Arc::new(Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("PB"),
        ));
        let ground_peer = Arc::new(Mutex::new(None::<(Arc<Router>, RouterSideId)>));
        let power_peer = Arc::new(Mutex::new(None::<(Arc<Router>, RouterSideId)>));
        let ground_side = {
            let ground_peer = ground_peer.clone();
            ground_router.add_side_packed("rocket_comms", move |bytes| {
                let (peer, side) = ground_peer.lock().unwrap().clone().unwrap();
                peer.rx_packed_from_side(bytes, side)
            })
        };
        let power_side = {
            let power_peer = power_peer.clone();
            power_router.add_side_packed("can", move |bytes| {
                let (peer, side) = power_peer.lock().unwrap().clone().unwrap();
                peer.rx_packed_from_side(bytes, side)
            })
        };
        *ground_peer.lock().unwrap() = Some((power_router.clone(), power_side));
        *power_peer.lock().unwrap() = Some((ground_router.clone(), ground_side));
        state
            .topology_router
            .set(ground_router.clone())
            .expect("failed to install test topology router");
        power_router.announce_discovery().unwrap();
        power_router.process_all_queues_with_timeout(0).unwrap();
        let address = ground_router
            .resolve_hostname("PB")
            .expect("Power Board address was not learned")
            .address;

        let pkt = Packet::new(
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            &format!("@addr:{address}"),
            678_903,
            f32_payload(&[8.0]),
        )
        .expect("failed to build compact Power Board battery packet");

        let row = handle_packet(&state, &db_tx, &db_overflow, pkt)
            .await
            .into_iter()
            .next()
            .expect("battery packet should produce a raw row");
        assert_eq!(row.sender_id, "PB");

        // handle_packet has finished emitting rows. The selected standalone
        // TestFire layout omits AV-bay rows; mixed Testing/HITL layouts retain them.
        let mut derived_rows = Vec::new();
        while let Ok(derived) = ws_rx.try_recv() {
            assert_eq!(derived.sender_id, "PB");
            derived_rows.push(derived);
        }
        if expected_mode == crate::station_mode::Mode::TestFire {
            assert!(derived_rows.is_empty());
        } else {
            assert_eq!(derived_rows.len(), 6);
            assert!(
                derived_rows
                    .iter()
                    .any(|row| row.data_type == "AV_BAY_BATTERY_PERCENT")
            );
        }
    }

    #[tokio::test]
    async fn fill_box_battery_voltage_below_thirteen_emits_latched_warning() {
        reset_fill_system_low_voltage_latch_for_tests();
        let (db_tx, _db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let mut warnings_rx = state.warnings_tx.subscribe();

        let pkt = Packet::new(
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GB",
            678_902,
            f32_payload(&[12.9]),
        )
        .expect("failed to build gateway low BATTERY_VOLTAGE packet");

        let rows = handle_packet(&state, &db_tx, &db_overflow, pkt).await;
        assert_eq!(
            rows[0].data_type,
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE")
                .as_str()
                .to_string()
        );

        let warning = tokio::time::timeout(Duration::from_millis(100), warnings_rx.recv())
            .await
            .expect("low fill-box voltage should emit a warning")
            .expect("warnings channel should remain open");
        assert_eq!(warning.message, FILL_SYSTEM_LOW_VOLTAGE_WARNING);

        let pkt = Packet::new(
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GW",
            678_903,
            f32_payload(&[12.8]),
        )
        .expect("failed to build gateway repeated low BATTERY_VOLTAGE packet");

        let _ = handle_packet(&state, &db_tx, &db_overflow, pkt).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), warnings_rx.recv())
                .await
                .is_err(),
            "low fill-box voltage warning should stay latched until voltage recovers"
        );
    }

    #[tokio::test]
    async fn umbilical_status_refreshes_action_policy_immediately() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();

        let before = state
            .action_policy_snapshot()
            .controls
            .into_iter()
            .find(|control| control.cmd == "Nitrogen")
            .expect("Nitrogen control should exist");
        assert_ne!(before.actuated, Some(true));

        let pkt = Packet::new(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::ActuatorBoard.sender_id(),
            111_222,
            Arc::from([ActuatorBoardCommands::NitrogenOpen as u8, 1_u8]),
        )
        .expect("failed to build umbilical status packet");

        let rows = handle_packet(&state, &db_tx, &db_overflow, pkt).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].data_type, VALVE_STATE_DATA_TYPE);
        assert_eq!(
            state.get_umbilical_valve_state(ActuatorBoardCommands::NitrogenOpen as u8),
            Some(true)
        );

        let after = state
            .action_policy_snapshot()
            .controls
            .into_iter()
            .find(|control| control.cmd == "Nitrogen")
            .expect("Nitrogen control should exist");
        assert_eq!(after.actuated, Some(true));

        let close_pkt = Packet::new(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::ActuatorBoard.sender_id(),
            111_223,
            Arc::from([ActuatorBoardCommands::NitrogenClose as u8, 1_u8]),
        )
        .expect("failed to build umbilical close status packet");

        let rows = handle_packet(&state, &db_tx, &db_overflow, close_pkt).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            state.get_umbilical_valve_state(ActuatorBoardCommands::NitrogenOpen as u8),
            Some(false)
        );

        let closed = state
            .action_policy_snapshot()
            .controls
            .into_iter()
            .find(|control| control.cmd == "Nitrogen")
            .expect("Nitrogen control should exist");
        assert_eq!(closed.actuated, Some(false));
    }

    #[tokio::test]
    async fn full_recording_queue_does_not_delay_or_roll_back_live_valve_confirmation() {
        let (db_tx, mut db_rx) = mpsc::channel(1);
        let state = test_app_state(db_tx.clone()).await;
        let overflow = test_db_overflow();
        db_tx
            .send(DbQueueItem::Write(DbWrite::FlightState {
                timestamp_ms: 0,
                state_code: 0,
            }))
            .await
            .unwrap();
        let key = ActuatorBoardCommands::NitrousOpen as u8;
        let make = |on| {
            Packet::new(
                crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "AB",
                0,
                Arc::from([key, on]),
            )
            .unwrap()
        };
        let open = make(1);
        let row = apply_umbilical_status_at_ingress(&state, &open).unwrap();
        assert_eq!(state.get_umbilical_valve_state(key), Some(true));
        let mut pending = Box::pin(persist_umbilical_status(
            &state, &db_tx, &overflow, &open, &row,
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        let close = make(0);
        apply_umbilical_status_at_ingress(&state, &close).unwrap();
        assert_eq!(state.get_umbilical_valve_state(key), Some(false));
        db_rx.recv().await.unwrap();
        pending.await;
        // Finishing the old recording work must never reopen the live valve.
        assert_eq!(state.get_umbilical_valve_state(key), Some(false));
        match db_rx.recv().await.unwrap() {
            DbQueueItem::Write(DbWrite::Telemetry { values_json, .. }) => {
                let values: Vec<Option<f32>> = serde_json::from_str(&values_json.unwrap()).unwrap();
                assert_eq!(values[5], Some(1.0));
            }
            _ => panic!("expected original arrival-time valve snapshot"),
        }
    }

    #[tokio::test]
    async fn daq_launch_log_status_ingress_preserves_exact_storage_state_without_display_rows() {
        let (db_tx, _db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let overflow = test_db_overflow();
        for (name, words) in [
            ("BOARD_HEALTH", {
                let mut words = vec![0; 24];
                words[0] = 1;
                words[4] = 1000;
                words
            }),
            (
                "DAQ_LAUNCH_LOG_STATUS",
                vec![
                    u32::MAX - 1,
                    0x12345678,
                    99,
                    0xabcd,
                    u32::MAX,
                    0xfedcba98,
                    0,
                ],
            ),
        ] {
            let payload: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
            let packet = Packet::new(
                crate::telemetry_schema::data_type(name),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "DAQ",
                100,
                payload.into(),
            )
            .unwrap();
            assert!(
                handle_packet(&state, &db_tx, &overflow, packet)
                    .await
                    .is_empty()
            );
        }
        let rows = state
            .network_health
            .lock()
            .unwrap()
            .snapshot(std::time::Instant::now(), false);
        let status = rows
            .iter()
            .find(|r| r.sender_id == "DAQ")
            .unwrap()
            .diagnostics
            .as_ref()
            .unwrap()
            .launch_log
            .as_ref()
            .unwrap();
        assert_eq!(status.session, 0x12345678_fffffffe);
        assert_eq!(status.deadline_utc_ms, 0xabcd_00000063);
        assert_eq!(status.revision, 0xfedcba98_ffffffff);
        assert_eq!(
            status.flags, 0,
            "accepted clock must not invent recorder-ready/open flags"
        );
        let network_now = recording_network_utc_ms()
            .and_then(|t| u64::try_from(t).ok())
            .unwrap_or_else(get_current_timestamp_ms);
        let words = [
            u32::MAX - 1,
            0x12345678,
            99,
            0xabcd,
            u32::MAX,
            0xfedcba98,
            0x31,
        ];
        let payload: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let packet = Packet::new(
            crate::telemetry_schema::data_type("DAQ_LAUNCH_LOG_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            network_now - 30_000,
            payload.into(),
        )
        .unwrap();
        assert!(
            handle_packet(&state, &db_tx, &overflow, packet)
                .await
                .is_empty()
        );
        let rows = state
            .network_health
            .lock()
            .unwrap()
            .snapshot(std::time::Instant::now(), false);
        let delayed = rows
            .iter()
            .find(|r| r.sender_id == "DAQ")
            .unwrap()
            .diagnostics
            .as_ref()
            .unwrap()
            .launch_log
            .as_ref()
            .unwrap();
        assert!(
            delayed.received_age_ms >= 30_000,
            "queued old open-file report must arrive already stale"
        );
        assert_eq!(delayed.flags, 0x31);
    }
    #[tokio::test]
    async fn ordered_valve_confirmations_accept_rebooted_source_clock() {
        for (board, key) in [
            (
                Board::ActuatorBoard,
                ActuatorBoardCommands::NitrogenOpen as u8,
            ),
            (Board::ValveBoard, ValveBoardCommands::PilotOpen as u8),
        ] {
            let (db_tx, _db_rx) = mpsc::channel(32);
            let state = test_app_state(db_tx.clone()).await;
            let overflow = test_db_overflow();
            // Transport ordering belongs to SEDSNet. A board's clock may reset
            // before time sync after reboot; don't suppress a valid confirmation.
            for (stamp, on) in [(100, 1), (200, 0), (0, 1), (1, 0)] {
                let pkt = Packet::new(
                    crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    board.sender_id(),
                    stamp,
                    Arc::from([key, on]),
                )
                .unwrap();
                let rows = handle_packet(&state, &db_tx, &overflow, pkt).await;
                assert!(!rows.is_empty());
                assert_eq!(state.get_umbilical_valve_state(key), Some(on != 0));
            }
        }
    }

    #[tokio::test]
    async fn same_tick_distinct_valve_packets_update_once_and_replays_never_roll_back_ui_or_pending()
     {
        for stamp in [0, 1234] {
            let (db_tx, _db_rx) = mpsc::channel(32);
            let state = test_app_state(db_tx.clone()).await;
            let overflow = test_db_overflow();
            let key = ActuatorBoardCommands::NitrogenOpen as u8;
            let make = |value, nonce| {
                Packet::new_with_nonce(
                    crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    "AB",
                    stamp,
                    nonce,
                    Arc::from([key, value]),
                )
                .unwrap()
            };
            let open = make(1, 11);
            let close = make(0, 12);
            let replay_new_nonce = make(1, 13);
            assert_ne!(open.packet_id(), close.packet_id());
            assert_ne!(open.packet_id(), replay_new_nonce.packet_id());
            handle_packet(&state, &db_tx, &overflow, open.clone()).await;
            assert_eq!(state.get_umbilical_valve_state(key), Some(true));
            handle_packet(&state, &db_tx, &overflow, close).await;
            assert_eq!(state.get_umbilical_valve_state(key), Some(false));
            for replay in [open.clone(), replay_new_nonce.clone()] {
                let rows = handle_packet(&state, &db_tx, &overflow, replay).await;
                assert_eq!(rows[0].values[4], Some(0.0));
                assert_eq!(state.get_umbilical_valve_state(key), Some(false));
            }
            state.set_pending_umbilical_valve_state(key, true);
            for replay in [open, replay_new_nonce] {
                let rows = handle_packet(&state, &db_tx, &overflow, replay).await;
                assert_eq!(rows[0].values[4], None);
                assert_eq!(state.get_umbilical_valve_state(key), None);
                assert_eq!(state.get_pending_umbilical_valve_state(key), Some(true));
            }
        }
    }

    #[tokio::test]
    async fn unresolved_compact_actuator_addresses_cannot_confirm_from_command_bytes() {
        let (db_tx, _db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let overflow = test_db_overflow();
        for key in [
            ActuatorBoardCommands::NitrogenOpen as u8,
            ActuatorBoardCommands::NitrousOpen as u8,
        ] {
            // Neither address has retained owner identity in this fixture,
            // including the default AB wire hash. Command bytes cannot supply it.
            for (sender, on) in [("@addr:483403151", 1u8), ("@addr:483403152", 0u8)] {
                state.set_pending_umbilical_valve_state(key, on != 0);
                let packet = Packet::new(
                    crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    sender,
                    1234,
                    Arc::from([key, on]),
                )
                .unwrap();
                let rows = handle_packet(&state, &db_tx, &overflow, packet).await;
                assert!(rows.is_empty());
                assert_eq!(state.get_umbilical_valve_state(key), None);
                assert_eq!(state.get_pending_umbilical_valve_state(key), Some(on != 0));
            }
        }
    }

    #[tokio::test]
    async fn battery_voltage_db_bucketing_is_sender_aware() {
        let (db_tx, mut db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();

        let pb_pkt = Packet::new(
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "PB",
            700_000,
            f32_payload(&[7.8]),
        )
        .expect("failed to build PB battery packet");
        let gb_pkt = Packet::new(
            crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GB",
            700_000,
            f32_payload(&[14.2]),
        )
        .expect("failed to build GB battery packet");

        let _ = handle_packet(&state, &db_tx, &db_overflow, pb_pkt).await;
        let _ = handle_packet(&state, &db_tx, &db_overflow, gb_pkt).await;

        let mut writes = Vec::new();
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_millis(10), db_rx.recv()).await {
                Ok(Some(DbQueueItem::Write(DbWrite::Telemetry {
                    data_type,
                    sender_id,
                    values_json,
                    ..
                }))) if data_type.as_str()
                    == &*crate::telemetry_schema::data_type("BATTERY_VOLTAGE").as_str() =>
                {
                    writes.push((sender_id, values_json));
                    if writes.len() == 2 {
                        break;
                    }
                }
                _ => {}
            }
        }

        writes.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, "GB");
        assert_eq!(writes[1].0, "PB");
    }

    #[cfg(feature="hitl_mode")]
    #[tokio::test]
    async fn fc_prelaunch_reports_follow_explicit_global_master_selection() {
        let (db_tx,_db_rx)=mpsc::channel(64);
        let state=test_app_state(db_tx.clone()).await;
        for board in Board::ALL {state.mark_board_seen(board.sender_id(),get_current_timestamp_ms());}
        let overflow=test_db_overflow();
        *state.state.lock().unwrap()=FlightState::Armed;
        let mut events=state.state_tx.subscribe();
        let packet=||Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],"FC",1234,Arc::from([FlightState::Idle as u8])).unwrap();
        assert!(crate::operator_phase::local_mode(&state));
        assert_eq!(handle_packet(&state,&db_tx,&overflow,packet()).await.len(),1);
        assert_eq!(state.local_flight_state_snapshot(),FlightState::Armed);
        assert!(events.try_recv().is_err(),"FC's per-board Idle must not replace the selected GS master");
        crate::gse::handle_command_with_origins(&state,&TelemetryCommand::ToggleGroundStationControl,None,None);
        assert!(!crate::operator_phase::local_mode(&state));
        handle_packet(&state,&db_tx,&overflow,packet()).await;
        assert_eq!(state.local_flight_state_snapshot(),FlightState::Idle,"explicit FC mode retains FC phase authority");
        assert_eq!(events.try_recv().unwrap().state,FlightState::Idle);
    }

    #[tokio::test]
    async fn non_authoritative_prelaunch_reports_preserve_global_state_clock_and_ws() {
        let (db_tx, _db_rx) = mpsc::channel(256);
        let state = test_app_state(db_tx.clone()).await;
        let overflow = test_db_overflow();
        for board in Board::ALL { state.mark_board_seen(board.sender_id(), get_current_timestamp_ms()); }
        let mut global_events = state.state_tx.subscribe();
        let clock = state.launch_clock_snapshot();
        for master in [FlightState::Startup, FlightState::Idle, FlightState::NitrousFill, FlightState::Armed] {
            *state.state.lock().unwrap() = master;
            for board in [Board::ActuatorBoard, Board::ValveBoard, Board::DaqBoard,
                Board::GatewayBoard, Board::RFBoard, Board::PowerBoard] {
                for reported in [FlightState::Startup, FlightState::Idle, FlightState::Armed, FlightState::Launch] {
                    let packet = Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
                        &[crate::telemetry_schema::endpoint("GROUND_STATION")],board.sender_id(),1234,
                        Arc::from([reported as u8])).unwrap();
                    let rows=handle_packet(&state,&db_tx,&overflow,packet).await;
                    assert_eq!(rows.len(),1);
                    assert_eq!(rows[0].sender_id,board.sender_id());
                    assert_eq!(rows[0].values,vec![Some(reported as u8 as f32)]);
                    assert_eq!(state.local_flight_state_snapshot(),master,"{board:?} {reported:?} cannot replace global {master:?}");
                    assert_eq!(state.launch_clock_snapshot(),clock);
                    assert!(global_events.try_recv().is_err(),"per-board telemetry is not a global WS phase event");
                }
            }
        }
    }

    #[tokio::test]
    async fn board_abort_non_authoritative_postlaunch_keeps_global_state_clock_and_stop_epochs() {
        let (db_tx, _db_rx) = mpsc::channel(64);
        let state = test_app_state(db_tx.clone()).await;
        let overflow = test_db_overflow();
        for board in Board::ALL {
            state.mark_board_seen(board.sender_id(), get_current_timestamp_ms());
        }
        *state.state.lock().unwrap() = FlightState::Launch;
        let clock = state.launch_clock_snapshot();
        let manual = state.manual_abort_intent.epoch();
        let observed = state.deferred_abort.manual_epoch();
        for board in [
            Board::ActuatorBoard,
            Board::ValveBoard,
            Board::DaqBoard,
            Board::PowerBoard,
            Board::GatewayBoard,
            Board::RFBoard,
            Board::FlightComputer,
        ] {
            let packet = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                board.sender_id(),
                1234,
                Arc::from([FlightState::Aborted as u8]),
            )
            .unwrap();
            crate::board_abort::observe_raw(&state, &packet);
            let rows = handle_packet(&state, &db_tx, &overflow, packet).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(state.local_flight_state_snapshot(), FlightState::Launch);
            assert_eq!(state.launch_clock_snapshot(), clock);
            assert_eq!(state.manual_abort_intent.epoch(), manual);
            assert_eq!(state.deferred_abort.manual_epoch(), observed);
        }
        assert_eq!(state.board_abort.snapshot(0).active_errors.len(), 7);
        for reboot_phase in [FlightState::Startup, FlightState::Idle, FlightState::Armed] {
            let packet = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                Board::FlightComputer.sender_id(),
                1235,
                Arc::from([reboot_phase as u8]),
            )
            .unwrap();
            handle_packet(&state, &db_tx, &overflow, packet).await;
            assert_eq!(state.local_flight_state_snapshot(), FlightState::Launch);
            assert_eq!(
                state.launch_clock_snapshot(),
                clock,
                "FC reboot phase is not reset authority"
            );
        }
        // Preserve the actual normalized FC authority rather than evaluator enums.
        let packet = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::FlightComputer.sender_id(),
            1235,
            Arc::from([FlightState::Ascent as u8]),
        )
        .unwrap();
        handle_packet(&state, &db_tx, &overflow, packet).await;
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Ascent);
    }

    #[tokio::test]
    async fn primary_and_rrls_self_echo_cannot_reauthorize_phase_or_clock() {
        if crate::operator_queue::isolated_case(module_path!(), "primary_and_rrls_self_echo_cannot_reauthorize_phase_or_clock") { return; }
        for (hostname, primary) in [("GS", true), ("RRLS", false)] {
            let (db_tx, _db_rx) = mpsc::channel(32);
            let state = test_app_state(db_tx.clone()).await;
            let overflow = test_db_overflow();
            let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([]).with_sender(hostname)));
            state.topology_router.set(router.clone()).unwrap();
            for board in Board::ALL { if *board != Board::GroundStation {
                // Explicit native handler fixture, not MCU qualification.
                state.mark_board_seen(board.sender_id(), get_current_timestamp_ms());
            }}
            assert!(state.all_required_boards_seen());
            *state.state.lock().unwrap() = FlightState::Aborted;
            let clock = state.launch_clock_snapshot();
            for sender in [hostname.to_string(), format!("@addr:{}", router.current_address())] {
                assert!(router.sender_matches_hostname(&sender, hostname));
                let packet = Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")], &sender, 1234, Arc::from([1u8])).unwrap();
                let rows = handle_packet_inner(&state, &db_tx, &overflow, packet, primary, None, crate::gse::automatic_generation(&state)).await;
                assert_eq!(rows.len(), 1);
                assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
                assert_eq!(state.launch_clock_snapshot(), clock);
            }
        }
    }

    #[cfg(feature = "test_fire_mode")]
    #[tokio::test]
    async fn reflected_and_unknown_dequeues_cannot_qualify_cached_startup() {
        if crate::operator_queue::isolated_case(module_path!(), "reflected_and_unknown_dequeues_cannot_qualify_cached_startup") { return; }
        let _ = crate::network_variables::flight_state();
        let baseline = crate::network_variables::automatic_idle_test_snapshot();
        let (db_tx, mut db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS")));
        router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_000_000_000)).unwrap();
        router.enable_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap();
        state.topology_router.set(router.clone()).unwrap();
        crate::gse::initialize(&state);
        *state.state.lock().unwrap() = FlightState::Startup;
        for board in Board::ALL { if *board != Board::GroundStation {
            state.mark_board_seen(board.sender_id(), get_current_timestamp_ms());
        }}
        assert!(state.all_required_boards_seen());
        let clock = state.launch_clock_snapshot();
        let cached = router.get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap();
        for sender in ["GS".to_string(), "RRLS".to_string(), format!("@addr:{}",router.current_address()), "unknown-native-peer".to_string()] {
            let packet = Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")], &sender, 1234, Arc::<[u8]>::from([])).unwrap();
            handle_packet(&state, &db_tx, &test_db_overflow(), packet).await;
            // Also exercise the actual reflected FSTATE path, not only heartbeat.
            if sender == "GS" || sender.starts_with("@addr:") {
                let packet = Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")], &sender, 1235, Arc::from([1u8])).unwrap();
                handle_packet(&state, &db_tx, &test_db_overflow(), packet).await;
            }
            assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
            assert_eq!(state.launch_clock_snapshot(), clock);
            assert_eq!(crate::network_variables::automatic_idle_test_snapshot(), baseline);
            let after = router.get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap();
            assert_eq!(after.as_ref().map(|p|(p.packet_id(),p.timestamp())), cached.as_ref().map(|p|(p.packet_id(),p.timestamp())));
            assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire),0);
        }
        while let Ok(item) = db_rx.try_recv() {
            assert!(!matches!(item, DbQueueItem::Write(DbWrite::FlightState { .. })), "no phase recording from passive reflections");
        }
    }

    #[cfg(feature = "test_fire_mode")]
    async fn recognized_dequeue_qualifies_once(compact: bool) {
        let _ = crate::network_variables::flight_state();
        let baseline = crate::network_variables::automatic_idle_test_snapshot();
        let (db_tx, _db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS")));
        router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_000_000_000)).unwrap();
        router.enable_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap();
        state.topology_router.set(router.clone()).unwrap();
        crate::gse::initialize(&state);
        *state.state.lock().unwrap() = FlightState::Startup;
        for board in Board::ALL { if *board != Board::GroundStation {
            state.mark_board_seen(board.sender_id(), get_current_timestamp_ms());
        }}
        assert!(state.all_required_boards_seen());
        // Public discovery through a real decoded-packet side, never a fabricated
        // topology record. This fixture tests attribution, not physical transport.
        let peer = Router::new(sedsnet::router::RouterConfig::new([]).with_sender("DAQ").with_static_address(123));
        // Discovery must arrive through a registered remote side: local rx()
        // consumes built-ins without learning a peer route. The return sink
        // models packet transport acceptance only, never a generated ACK or
        // evidence that a remote application applied this host's publication.
        let host_side = router.add_side_packet("native-peer-return", |_| Ok(()));
        let incoming = router.clone();
        peer.add_side_packet("native-discovery", move |packet| incoming.rx_from_side(packet, host_side));
        peer.announce_discovery().unwrap();
        peer.process_all_queues().unwrap();
        // Assigned discovery address and compact packet source are distinct.
        // Derive the latter from the actual public encoder/header parser;
        // never assume that the peer's requested static address is on wire.
        let source_packet = Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], "DAQ", 1234, Arc::<[u8]>::from([])).unwrap();
        let wire = sedsnet::wire_format::pack_packet(&source_packet);
        let envelope = sedsnet::wire_format::peek_envelope(&wire).unwrap();
        assert_eq!(envelope.timestamp_ms, source_packet.timestamp());
        assert_eq!(envelope.ty, source_packet.data_type());
        let sender = if compact { format!("@addr:{}", envelope.source_address) } else { "DAQ".to_string() };
        eprintln!("NATIVE_COMPACT_IDENTITY requested_static=123 actual_packet_source={} sender={sender}", envelope.source_address);
        assert_eq!(state.board_from_network_sender(&sender), Some(Board::DaqBoard));
        let packet = Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], &sender, 1234, Arc::<[u8]>::from([])).unwrap();
        handle_packet(&state, &db_tx, &test_db_overflow(), packet).await;
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        assert_eq!(crate::network_variables::flight_state(), 0);
        assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&baseline));
        let cached = router.get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap().unwrap();
        assert_eq!(cached.payload(), &[1]);
        assert!(cached.timestamp() > 0);
        let original = (cached.packet_id(),cached.timestamp());
        let next = Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], &sender, 1235, Arc::<[u8]>::from([])).unwrap();
        handle_packet(&state, &db_tx, &test_db_overflow(), next).await;
        let cached = router.get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap().unwrap();
        assert_eq!((cached.packet_id(),cached.timestamp()),original);
    }

    #[cfg(feature = "test_fire_mode")]
    #[tokio::test]
    async fn recognized_mcu_dequeue_keeps_original_automatic_admission() {
        if crate::operator_queue::isolated_case(module_path!(), "recognized_mcu_dequeue_keeps_original_automatic_admission") { return; }
        recognized_dequeue_qualifies_once(false).await;
    }

    #[cfg(feature = "test_fire_mode")]
    #[tokio::test]
    async fn recognized_compact_mcu_dequeue_keeps_original_automatic_admission() {
        if crate::operator_queue::isolated_case(module_path!(), "recognized_compact_mcu_dequeue_keeps_original_automatic_admission") { return; }
        recognized_dequeue_qualifies_once(true).await;
    }

    #[tokio::test]
    async fn reflected_state_preserves_raw_model_recording_without_phase_authority() {
        const NAME: &str = "reflected_state_preserves_raw_model_recording_without_phase_authority";
        if crate::operator_queue::isolated_case(module_path!(), NAME) { return; }
        // The parent owns isolated config paths. A second fresh process binds
        // the existing explicit-file clock before its first clock observation.
        if std::env::var_os("GS_RAW_ECHO_MODEL_CHILD").is_none() {
            let root = std::path::PathBuf::from(std::env::var_os("GS_RECOVERY_ISOLATED_ROOT").unwrap());
            let clock = root.join("raw-echo-model-clock");
            std::fs::write(&clock, "100000").unwrap();
            let case = format!("{}::{NAME}", module_path!().split_once("::").unwrap().1);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &case, "--nocapture", "--test-threads=1"])
                .env("GS_RAW_ECHO_MODEL_CHILD", "1")
                .env("GS_SIM_CLOCK_FILE", clock)
                .env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000")
                .output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success() && stdout.contains("1 passed; 0 failed; 0 ignored;"), "{stdout}\n{stderr}");
            assert!(!stdout.contains("panicked at") && !stderr.contains("panicked at"));
            return;
        }
        let explicit_model = cfg!(any(feature = "hitl_mode", feature = "firmware_sim"));
        assert_eq!(procedure_clock::now().model_monotonic_ms(), explicit_model.then_some(100000));
        let _ = crate::network_variables::flight_state();
        let original_store = crate::network_variables::automatic_idle_test_snapshot();
        for (hostname, primary) in [("GS", true), ("RRLS", false)] {
            let (db_tx, mut db_rx) = mpsc::channel(32);
            let state = test_app_state(db_tx.clone()).await;
            let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([]).with_sender(hostname)));
            state.topology_router.set(router.clone()).unwrap();
            // Match the preexisting native self-echo control's contact-only
            // prerequisite. This supplies neither board floor/readiness proof
            // nor SDK ACKs; it lets the real FSTATE parser reach observation.
            for board in Board::ALL { if *board != Board::GroundStation {
                state.mark_board_seen(board.sender_id(), get_current_timestamp_ms());
            }}
            assert!(state.all_required_boards_seen());
            *state.state.lock().unwrap() = FlightState::Aborted;
            let clock = state.launch_clock_snapshot();
            let db = state.telemetry_db_pool();
            crate::telemetry_db::ensure_telemetry_schema(&db).await.unwrap();
            for (index, sender) in [hostname.to_string(), format!("@addr:{}", router.current_address())].into_iter().enumerate() {
                let source_utc = 1_767_225_699_123 + index as u64;
                let canonical_sender = state.canonical_network_sender_id(&sender);
                let packet = Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")], &sender,
                    source_utc, Arc::from([FlightState::Idle as u8])).unwrap();
                let expected_payload = payload_json_from_pkt(&packet);
                let before = get_current_timestamp_ms() as i64;
                let rows = handle_packet_inner(&state, &db_tx, &test_db_overflow(), packet, primary, None, crate::gse::automatic_generation(&state)).await;
                let after = get_current_timestamp_ms() as i64;
                assert_eq!(rows.len(), 1);
                let mut writes = Vec::new();
                while let Ok(item) = db_rx.try_recv() {
                    match item {
                        DbQueueItem::Write(write) => writes.push(write),
                        other => panic!("unexpected recording control: {other:?}"),
                    }
                }
                assert_eq!(writes.len(), usize::from(explicit_model),
                    "raw rows only under the real explicit-model condition, never a phase event");
                if explicit_model {
                    assert!(matches!(&writes[0], DbWrite::Telemetry { data_type, .. } if data_type == "FLIGHT_STATE"));
                }
                insert_db_batch_once(&db, &writes).await.unwrap();
                if explicit_model {
                    let recorded: (i64, i64, String, String) = sqlx::query_as(
                        "SELECT timestamp_ms,source_timestamp_ms,values_json,payload_json FROM telemetry WHERE data_type='FLIGHT_STATE' AND sender_id=? AND source_timestamp_ms=?")
                        .bind(&canonical_sender).bind(source_utc as i64).fetch_one(&db).await.unwrap();
                    assert!((before..=after).contains(&recorded.0));
                    assert_eq!(recorded.1, source_utc as i64);
                    assert_eq!(serde_json::from_str::<Vec<f32>>(&recorded.2).unwrap(), vec![FlightState::Idle as u8 as f32]);
                    assert_eq!(recorded.3, expected_payload);
                    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM telemetry WHERE data_type='FLIGHT_STATE' AND sender_id=? AND source_timestamp_ms=?")
                        .bind(&canonical_sender).bind(source_utc as i64).fetch_one(&db).await.unwrap();
                    assert_eq!(count, 1);
                } else {
                    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM telemetry WHERE data_type='FLIGHT_STATE'")
                        .fetch_one(&db).await.unwrap();
                    assert_eq!(count, 0, "ordinary TF has no file-model clock and must not invent raw model rows");
                }
                assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
                assert_eq!(state.launch_clock_snapshot(), clock);
                assert_eq!(crate::network_variables::automatic_idle_test_snapshot(), original_store);
            }
        }
    }

    #[tokio::test]
    async fn rrls_still_projects_real_primary_gs_phase() {
        if crate::operator_queue::isolated_case(module_path!(), "rrls_still_projects_real_primary_gs_phase") { return; }
        let (db_tx, _db_rx) = mpsc::channel(32);
        let state = test_app_state(db_tx.clone()).await;
        let overflow = test_db_overflow();
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([]).with_sender("RRLS")));
        state.topology_router.set(router).unwrap();
        for board in Board::ALL { if *board != Board::GroundStation {
            state.mark_board_seen(board.sender_id(), get_current_timestamp_ms());
        }}
        *state.state.lock().unwrap() = FlightState::Aborted;
        let packet = Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], "GS", 1234, Arc::from([1u8])).unwrap();
        handle_packet_inner(&state, &db_tx, &overflow, packet, false, None, crate::gse::automatic_generation(&state)).await;
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    }

    #[tokio::test]
    async fn unknown_flight_state_code_emits_backend_message() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        for board in Board::ALL {
            if *board != Board::GroundStation {
                state.mark_board_seen(board.sender_id(), get_current_timestamp_ms());
            }
        }
        let pkt = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            Board::FlightComputer.sender_id(),
            789_012,
            Arc::from([255_u8]),
        )
        .expect("failed to build invalid flight-state packet");

        let rows = handle_packet(&state, &db_tx, &db_overflow, pkt).await;

        assert!(rows.is_empty());
        let messages = state.messages_snapshot();
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]
                .message
                .contains("FLIGHT_STATE parse errors: 1 packet(s)"),
            "unexpected backend message: {}",
            messages[0].message
        );
    }
    mod long_capture_owner_tests {
        include!("telemetry_task/loadcell_owner_capture_tests.rs");
    }

    #[cfg(feature = "firmware_sim")]
    mod zero_ingress {
        include!("telemetry_task/loadcell_zero_ingress_tests.rs");
    }

    #[cfg(feature = "hitl_mode")]
    mod eval_abort_launch_ingress {
        use super::*;
        include!("telemetry_task/eval_abort_launch_diagnostic.rs");
    }
    #[cfg(feature = "hitl_mode")]
    mod launch_preflight {
        use super::*;
        include!("telemetry_task/launch_preflight_tests.rs");
    }
    #[cfg(feature = "hitl_mode")]
    mod launch_admission_latency {
        use super::*;
        include!("telemetry_task/launch_admission_latency_tests.rs");
    }
    mod launch_retry_controls {
        use super::*;
        include!("telemetry_task/launch_retry_controls.rs");
    }
    mod recording_diagnostic_capture {
        use super::*;
        include!("telemetry_task/recording_diagnostic_tests.rs");
    }
}
