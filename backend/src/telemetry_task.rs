mod prelude;
use prelude::*;

mod commands;
use commands::{flush_command_tx, log_command_dispatch, queue_locally_routed_flight_command};
pub(crate) use commands::{queue_abort_packet, register_flight_command_tx_side};
mod radio_io;
pub use radio_io::CommsWorkerHandle;
#[cfg(test)]
use radio_io::{
    is_fill_system_command_payload, radio_command_log_line, spawn_dedicated_radio_io_threads,
};
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
        TelemetryCommand::EnableIMU => FlightComputerCommands::ReinitIMU as u8,
        TelemetryCommand::DisableIMU => FlightComputerCommands::DisableIMU as u8,
        TelemetryCommand::AbortAfter40 => FlightComputerCommands::AbortAfter40 as u8,
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
async fn set_local_flight_state_for_operator_mode(state: &Arc<AppState>, next_state: FlightState) {
    state.set_local_flight_state(next_state);
}

mod derived;
use derived::*;

pub fn set_network_time_router(router: Arc<Router>) {
    let _ = NETWORK_TIME_ROUTER.set(router);
}

fn send_valve_launch_sequence_command(router: &Router) -> bool {
    let payload = [ValveBoardCommands::Sequence as u8];
    if let Err(e) = router.log_queue(DataType::ValveCommand, &payload) {
        log_telemetry_error("failed to log valve launch sequence command", e);
        false
    } else {
        log_command_dispatch(
            "Valve launch sequence command",
            "umbilical_comms",
            DataType::ValveCommand,
            &payload,
        );
        flush_command_tx(router, "Valve launch sequence command tx");
        true
    }
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
    if matches!(
        state.launch_clock_snapshot().kind,
        LaunchClockKind::TMinus | LaunchClockKind::TPlus
    ) {
        return;
    }

    state.clear_launch_sequence_command_pending();
    let now_ms = get_current_timestamp_ms() as i64;
    state.set_launch_clock(launch_countdown_clock(now_ms));

    gs_debug_println!(
        "Valve-board sequence started launch clock; T-0 in {} ms",
        crate::state::LAUNCH_COUNTDOWN_DURATION_MS
    );
}

fn transition_launch_clock_to_t_plus_from_pilot_open(state: &Arc<AppState>) {
    if state.launch_clock_snapshot().kind != LaunchClockKind::TMinus {
        return;
    }

    state.set_launch_clock(LaunchClockMsg {
        kind: LaunchClockKind::TPlus,
        anchor_timestamp_ms: Some(get_current_timestamp_ms() as i64),
        duration_ms: None,
    });
    gs_debug_println!("Pilot valve open status received; launch clock switched to T+");
}

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
async fn handle_local_ground_station_launch_command(state: Arc<AppState>, router: Arc<Router>) {
    if vent_valve_known_open_for_launch(&state) {
        warn_launch_blocked_by_vent_valve(&state);
        sequences::refresh_action_policy_now(&state);
        state.broadcast_action_policy_snapshot();
        gs_debug_println!(
            "Ground-station launch command ignored because vent valve is reported open"
        );
        return;
    }
    if matches!(
        state.launch_clock_snapshot().kind,
        LaunchClockKind::TMinus | LaunchClockKind::TPlus
    ) {
        gs_debug_println!(
            "Ground-station launch command ignored because launch clock is already running"
        );
        return;
    }
    if !state.try_begin_launch_sequence_command() {
        gs_debug_println!(
            "Ground-station launch command ignored because valve-board launch sequence command is already pending"
        );
        return;
    }
    if state.recording_status_snapshot().mode != RecordingModeWire::Recording {
        let _ = state
            .db_queue_tx
            .send(DbQueueItem::Control(RecordingCommand::StartNow))
            .await;
        gs_debug_println!("Ground-station launch auto-started DB recording");
    }
    if !send_valve_launch_sequence_command(&router) {
        state.clear_launch_sequence_command_pending();
        return;
    }
    state.set_launch_indicator_latched(true);
    sequences::refresh_action_policy_now(&state);
    state.broadcast_action_policy_snapshot();
    gs_debug_println!(
        "Ground-station launch sequence command sent; waiting for valve-board clock start"
    );
}

async fn handle_flight_computer_launch_command(state: Arc<AppState>, router: Arc<Router>) {
    if vent_valve_known_open_for_launch(&state) {
        warn_launch_blocked_by_vent_valve(&state);
        sequences::refresh_action_policy_now(&state);
        state.broadcast_action_policy_snapshot();
        gs_debug_println!("Launch command ignored because vent valve is reported open");
        return;
    }
    if matches!(
        state.launch_clock_snapshot().kind,
        LaunchClockKind::TMinus | LaunchClockKind::TPlus
    ) {
        gs_debug_println!("Launch command ignored because launch clock is already running");
        return;
    }
    if state.recording_status_snapshot().mode != RecordingModeWire::Recording {
        let _ = state
            .db_queue_tx
            .send(DbQueueItem::Control(RecordingCommand::StartNow))
            .await;
        gs_debug_println!("Launch auto-started DB recording");
    }
    let now_ms = get_current_timestamp_ms() as i64;
    state.set_launch_clock(launch_countdown_clock(now_ms));
    let flight_command_sent = if let Err(e) = queue_locally_routed_flight_command(
        &router,
        "Launch command",
        &[FlightComputerCommands::Launch as u8],
    ) {
        log_telemetry_error("failed to log Launch command", e);
        false
    } else {
        flush_command_tx(&router, "Launch command tx");
        true
    };
    let valve_command_sent = send_valve_launch_sequence_command(&router);
    if flight_command_sent || valve_command_sent {
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

pub async fn telemetry_task(
    state: Arc<AppState>,
    router: Arc<Router>,
    comms: Vec<CommsWorkerHandle>,
    mut rx: mpsc::Receiver<TelemetryCommand>,
    mut db_rx: mpsc::Receiver<DbQueueItem>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut handle_interval = interval(Duration::from_millis(1));
    let mut heartbeat_interval = interval(Duration::from_millis(500));
    handle_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_failed = false;
    let mut last_backpressure_log_ms: u64 = 0;
    let packet_work_queue_size = env_usize(
        "GS_PACKET_WORK_QUEUE_SIZE",
        PACKET_WORK_QUEUE_SIZE,
        1024,
        262_144,
    );
    let packet_enqueue_burst = env_usize("GS_PACKET_ENQUEUE_BURST", PACKET_ENQUEUE_BURST, 32, 4096);
    let (packet_tx, mut packet_rx) = mpsc::channel::<Packet>(packet_work_queue_size);
    let db_overflow = DbOverflow;

    let db_worker = {
        let state = state.clone();
        let db_batch_max = env_usize("GS_DB_BATCH_MAX", DB_BATCH_MAX_DEFAULT, 1, 4096);
        let db_batch_wait_ms = std::env::var("GS_DB_BATCH_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DB_BATCH_WAIT_MS_DEFAULT)
            .clamp(1, 250);
        tokio::spawn(async move {
            log::info!("telemetry db worker started");
            let mut db_shutdown_rx = state.shutdown_subscribe();
            struct ActiveRecording {
                pool: sqlx::SqlitePool,
                path: String,
            }

            let placeholder_path = state.placeholder_db_path.clone();
            let recordings_dir = std::path::Path::new(&placeholder_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf();
            let mut recent_writes: VecDeque<DbWrite> = VecDeque::new();
            let mut active_recording: Option<ActiveRecording> = None;
            let mut mode = RecordingMode::Idle;
            let mut last_recording_end_ms: Option<i64> = None;

            let mut pending: Vec<DbWrite> = Vec::with_capacity(db_batch_max);
            let mut deferred_control_carry: Option<RecordingCommand> = None;
            let mut shutting_down = false;
            loop {
                let first = if shutting_down {
                    match db_rx.try_recv() {
                        Ok(item) => item,
                        Err(MpscTryRecvError::Empty | MpscTryRecvError::Disconnected) => break,
                    }
                } else {
                    tokio::select! {
                        recv = db_rx.recv() => {
                            let Some(item) = recv else { break; };
                            item
                        }
                        recv = db_shutdown_rx.recv() => {
                            match recv {
                                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
                                    shutting_down = true;
                                    continue;
                                }
                            }
                        }
                    }
                };

                let mut deferred_control: Option<RecordingCommand> = deferred_control_carry.take();
                match first {
                    DbQueueItem::Write(write) => {
                        let newest_ts = write.timestamp_ms();
                        recent_writes.push_back(write.clone());
                        prune_recent_writes(&mut recent_writes, newest_ts);
                        if matches!(mode, RecordingMode::Recording) {
                            pending.push(write);
                        }
                    }
                    DbQueueItem::Control(cmd) => deferred_control = Some(cmd),
                }

                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(db_batch_wait_ms);
                while pending.len() < db_batch_max && deferred_control.is_none() {
                    match db_rx.try_recv() {
                        Ok(DbQueueItem::Write(write)) => {
                            let newest_ts = write.timestamp_ms();
                            recent_writes.push_back(write.clone());
                            prune_recent_writes(&mut recent_writes, newest_ts);
                            if matches!(mode, RecordingMode::Recording) {
                                pending.push(write);
                            }
                        }
                        Ok(DbQueueItem::Control(cmd)) => {
                            deferred_control = Some(cmd);
                        }
                        Err(MpscTryRecvError::Disconnected) => break,
                        Err(MpscTryRecvError::Empty) => {
                            let now = tokio::time::Instant::now();
                            if now >= deadline {
                                break;
                            }
                            let remaining = deadline.saturating_duration_since(now);
                            match tokio::time::timeout(remaining, db_rx.recv()).await {
                                Ok(Some(DbQueueItem::Write(write))) => {
                                    let newest_ts = write.timestamp_ms();
                                    recent_writes.push_back(write.clone());
                                    prune_recent_writes(&mut recent_writes, newest_ts);
                                    if matches!(mode, RecordingMode::Recording) {
                                        pending.push(write);
                                    }
                                }
                                Ok(Some(DbQueueItem::Control(cmd))) => {
                                    deferred_control = Some(cmd);
                                }
                                Ok(None) => break,
                                Err(_) => break,
                            }
                        }
                    }
                }

                let flush_ok =
                    flush_recording_batch(active_recording.as_ref().map(|a| &a.pool), &mut pending)
                        .await;
                if !flush_ok {
                    deferred_control_carry = deferred_control;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }

                if let Some(cmd) = deferred_control {
                    match cmd {
                        RecordingCommand::StartNow | RecordingCommand::StartWithRecent => {
                            let started_at_ms = get_current_timestamp_ms() as i64;
                            let target_path = session_db_path(&recordings_dir, started_at_ms);
                            match open_telemetry_db(&target_path).await {
                                Ok((new_pool, new_path)) => {
                                    if let Some(active) = active_recording.take() {
                                        log::info!("recording session closing db={}", active.path);
                                        close_and_finalize_sqlite(active.pool, &active.path).await;
                                        export_test_fire_csv_if_enabled(&state, &active.path).await;
                                        last_recording_end_ms =
                                            Some(get_current_timestamp_ms() as i64);
                                    }
                                    log::info!("recording session started db={new_path}");
                                    state.replace_telemetry_db(new_pool.clone(), new_path.clone());
                                    state.set_recording_status(RecordingStatusMsg {
                                        mode: RecordingModeWire::Recording,
                                        db_path: Some(new_path.clone()),
                                    });
                                    active_recording = Some(ActiveRecording {
                                        pool: new_pool,
                                        path: new_path,
                                    });
                                    mode = RecordingMode::Recording;

                                    if matches!(cmd, RecordingCommand::StartWithRecent)
                                        && let Some(active) = &active_recording
                                    {
                                        let cutoff = started_at_ms.saturating_sub(120_000);
                                        let min_ts = last_recording_end_ms
                                            .map(|prev| prev.max(cutoff))
                                            .unwrap_or(cutoff);
                                        let backfill: Vec<DbWrite> = recent_writes
                                            .iter()
                                            .filter(|write| write.timestamp_ms() > min_ts)
                                            .cloned()
                                            .collect();
                                        if let Err(err) =
                                            insert_db_batch_with_retry(&active.pool, &backfill)
                                                .await
                                        {
                                            eprintln!("Failed to backfill recent writes: {err}");
                                        }
                                    }
                                }
                                Err(err) => {
                                    emit_warning(
                                        &state,
                                        format!("Failed to open recording DB session: {err}"),
                                    );
                                }
                            }
                        }
                        RecordingCommand::Pause | RecordingCommand::Stop => {
                            if let Some(active) = active_recording.take() {
                                log::info!("recording session closing db={}", active.path);
                                close_and_finalize_sqlite(active.pool, &active.path).await;
                                let deleted = delete_empty_recording_if_needed(&active.path).await;
                                if !deleted {
                                    export_test_fire_csv_if_enabled(&state, &active.path).await;
                                }
                                last_recording_end_ms = Some(get_current_timestamp_ms() as i64);
                            }
                            mode = if matches!(cmd, RecordingCommand::Pause) {
                                RecordingMode::Paused
                            } else {
                                RecordingMode::Idle
                            };
                            rotate_placeholder_db(&state, mode).await;
                            state.set_recording_status(RecordingStatusMsg {
                                mode: RecordingModeWire::from(mode),
                                db_path: None,
                            });
                        }
                        RecordingCommand::ResetAll => {
                            pending.clear();
                            recent_writes.clear();
                            let db = active_recording
                                .as_ref()
                                .map(|active| active.pool.clone())
                                .unwrap_or_else(|| state.telemetry_db_pool());
                            if let Err(err) = clear_telemetry_tables(&db).await {
                                log::error!("failed clearing telemetry tables during reset: {err}");
                            }
                        }
                    }
                }
            }

            if let Some(active) = active_recording.take() {
                log::info!("recording worker finalizing active db={}", active.path);
                close_and_finalize_sqlite(active.pool, &active.path).await;
                let deleted = delete_empty_recording_if_needed(&active.path).await;
                if !deleted {
                    export_test_fire_csv_if_enabled(&state, &active.path).await;
                }
            }
            log::info!("telemetry db worker stopped");
        })
    };

    let packet_worker = {
        let state = state.clone();
        let db_tx = state.db_queue_tx.clone();
        let db_overflow = db_overflow.clone();
        tokio::spawn(async move {
            while let Some(pkt) = packet_rx.recv().await {
                for row in handle_packet(&state, &db_tx, &db_overflow, pkt).await {
                    state.cache_recent_telemetry(row.clone());
                    let _ = state.ws_tx.send(row);
                }
            }
        })
    };

    let router_worker = match spawn_router_worker_thread(router.clone(), state.clone()) {
        Ok(worker) => Some(worker),
        Err(err) => {
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
                eprintln!("Failed to spawn comms worker thread: {err}");
                Vec::new()
            })
        })
        .collect();

    loop {
        tokio::select! {
            biased;

                Some(cmd) = rx.recv() => {
                    if matches!(cmd, TelemetryCommand::ResetSim) {
                        reset_testing_simulation(&state).await;
                        continue;
                    }
                    if !state.is_command_allowed(&cmd) {
                        emit_notification_warning(
                            &state,
                            format!("Command {cmd:?} blocked by sequence/key interlock"),
                        );
                        continue;
                    }
                    state.record_command_accepted(&cmd, get_current_timestamp_ms());
                    if matches!(cmd, TelemetryCommand::Abort) {
                        state.set_abort_indicator_latched(true);
                        sequences::refresh_action_policy_now(&state);
                        state.broadcast_action_policy_snapshot();
                    }
                    if flight_sim::handle_command(&cmd) {
                        continue;
                    }
                    match cmd {
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
                                    DataType::ValveCommand,
                                    key,
                                    !is_on,
                                    cmd as u8,
                                    "Dump command",
                                );
                                gs_debug_println!("Dump command sent {:?}", cmd);
                            }
                        TelemetryCommand::Abort => {
                                state.clear_launch_sequence_command_pending();
                                if let Err(e) = queue_abort_packet(
                                    &router,
                                    "Manual Abort Command Issued",
                                ) {
                                    log_telemetry_error("failed to log Abort command", e);
                                } else {
                                    flush_command_tx(&router, "Abort command tx");
                                }
                                emit_error(&state, "Manual abort triggered!".to_string());
                                gs_debug_println!("Abort command sent");
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
                                    DataType::ActuatorCommand,
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
                                if let Err(e) = router.log_queue(DataType::ActuatorCommand, &payload) {
                                    log_telemetry_error("failed to log IgniterSequence command", e);
                                } else {
                                    log_command_dispatch(
                                        "IgniterSequence command",
                                        "umbilical_comms",
                                        DataType::ActuatorCommand,
                                        &payload,
                                    );
                                    flush_command_tx(&router, "IgniterSequence command tx");
                                }
                                gs_debug_println!("IgniterSequence command sent");
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
                                    DataType::ValveCommand,
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
                                    DataType::ValveCommand,
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
                                    DataType::ActuatorCommand,
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
                                    DataType::ActuatorCommand,
                                    ActuatorBoardCommands::NitrogenOpen as u8,
                                    false,
                                    ActuatorBoardCommands::NitrogenClose as u8,
                                    "NitrogenClose command",
                                );
                                gs_debug_println!("Nitrogen explicit close command sent");
                            }
                        TelemetryCommand::RetractPlumbing => {
                                queue_guarded_fill_command(
                                    &state,
                                    &router,
                                    DataType::ActuatorCommand,
                                    ActuatorBoardCommands::RetractPlumbing as u8,
                                    true,
                                    ActuatorBoardCommands::RetractPlumbing as u8,
                                    "RetractPlumbing command",
                                );
                                gs_debug_println!("RetractPlumbing command sent");
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
                                    DataType::ActuatorCommand,
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
                                    DataType::ActuatorCommand,
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
                        TelemetryCommand::Launch => {
                                handle_flight_computer_launch_command(state.clone(), router.clone()).await;
                            }
                        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
                        TelemetryCommand::GroundStationLaunch => {
                                handle_local_ground_station_launch_command(state.clone(), router.clone()).await;
                            }
                        TelemetryCommand::VigilantMode => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &router,
                                    "VigilantMode",
                                    &[FlightComputerCommands::VigilantMode as u8],
                                ) {
                                    log_telemetry_error("failed to log VigilantMode command", e);
                                }
                                gs_debug_println!("VigilantMode command sent");
                            }
                        TelemetryCommand::RevokeVigilantMode => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &router,
                                    "RevokeVigilantMode",
                                    &[FlightComputerCommands::RevokeVigilantMode as u8],
                                ) {
                                    log_telemetry_error("failed to log RevokeVigilantMode command", e);
                                }
                                gs_debug_println!("RevokeVigilantMode command sent");
                            }
                        TelemetryCommand::MeasmReports => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &router,
                                    "MeasmReports",
                                    &[FlightComputerCommands::MeasmReports as u8],
                                ) {
                                    log_telemetry_error("failed to log MeasmReports command", e);
                                }
                                gs_debug_println!("MeasmReports command sent");
                            }
                        TelemetryCommand::RevokeMeasmReports => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &router,
                                    "RevokeMeasmReports",
                                    &[FlightComputerCommands::RevokeMeasmReports as u8],
                                ) {
                                    log_telemetry_error("failed to log RevokeMeasmReports command", e);
                                }
                                gs_debug_println!("RevokeMeasmReports command sent");
                            }
                        TelemetryCommand::VelocityChecks => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &router,
                                    "VelocityChecks",
                                    &[FlightComputerCommands::MeasmReports as u8],
                                ) {
                                    log_telemetry_error("failed to log VelocityChecks command", e);
                                }
                                gs_debug_println!("MeasmReports command sent");
                            }
                        TelemetryCommand::RevokeVelocityChecks => {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &router,
                                    "RevokeVelocityChecks",
                                    &[FlightComputerCommands::RevokeVelocityChecks as u8],
                                ) {
                                    log_telemetry_error("failed to log RevokeVelocityChecks command", e);
                                }
                                gs_debug_println!("RevokeMeasmReports command sent");
                            }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ToggleButtonInterlock => {
                                let enabled = state.toggle_hitl_button_interlock();
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("HITL button interlock toggled: {enabled}");
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ToggleLaunchInterlock => {
                                let enabled = state.toggle_hitl_launch_interlock();
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("HITL launch interlock toggled: {enabled}");
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::TogglePhysicalLaunchMode => {
                                let uses_gs = state.toggle_hitl_physical_launch_mode();
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("HITL physical launch mode toggled: uses_ground_station={uses_gs}");
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::ResetLaunchLatch => {
                                state.set_launch_indicator_latched(false);
                                sequences::refresh_action_policy_now(&state);
                                state.broadcast_action_policy_snapshot();
                                gs_debug_println!("Launch indicator latch reset");
                        }
                        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
                        TelemetryCommand::AdvanceFlightState => {
                                let current = *state.state.lock().unwrap();
                                let next = operator_mode_adjacent_flight_state(current, 1);
                                set_local_flight_state_for_operator_mode(&state, next).await;
                                gs_debug_println!("Operator-mode flight state advanced: {:?} -> {:?}", current, next);
                        }
                        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
                        TelemetryCommand::RewindFlightState => {
                                let current = *state.state.lock().unwrap();
                                let next = operator_mode_adjacent_flight_state(current, -1);
                                set_local_flight_state_for_operator_mode(&state, next).await;
                                gs_debug_println!("Operator-mode flight state rewound: {:?} -> {:?}", current, next);
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::EvalSuccessive
                        | TelemetryCommand::RevokeEvalSuccessive
                        | TelemetryCommand::ResetFailures
                        | TelemetryCommand::RevokeResetFailures
                        | TelemetryCommand::DeployParachute
                        | TelemetryCommand::ExpandParachute
                        | TelemetryCommand::EvaluationRelax
                        | TelemetryCommand::EvaluationFocus
                        | TelemetryCommand::EvaluationAbort
                        | TelemetryCommand::ReinitSensors
                        | TelemetryCommand::ReinitBarometer
                        | TelemetryCommand::EnableIMU
                        | TelemetryCommand::DisableIMU
                        | TelemetryCommand::AbortAfter15
                        | TelemetryCommand::AbortAfter40
                        | TelemetryCommand::AbortAfter70
                        | TelemetryCommand::ReinitAfter12
                        | TelemetryCommand::ReinitAfter26
                        | TelemetryCommand::ReinitAfter44 => {
                            if let Some(cmd_id) = hitl_flight_command_id(&cmd) {
                                if let Err(e) = queue_locally_routed_flight_command(
                                    &router,
                                    "HITL flight command",
                                    &[cmd_id],
                                ) {
                                    log_telemetry_error("failed to log HITL flight command", e);
                                }
                                gs_debug_println!("HITL flight command sent: {:?} ({cmd_id})", cmd);
                            }
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    if router.log_queue::<u8>(DataType::Heartbeat, &[]).is_ok() {
                        state.mark_board_seen(
                            Board::GroundStation.sender_id(),
                            get_current_timestamp_ms(),
                        );
                        heartbeat_failed = false;
                    } else if !heartbeat_failed {
                            emit_warning(
                                &state,
                                "Warning: Ground Station heartbeat send failed",
                            );
                            heartbeat_failed = true;

                    }
                }
                _ = handle_interval.tick() => {
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
                                emit_warning(
                                    &state,
                                    "Warning: telemetry processing worker stopped unexpectedly",
                                );
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

    for worker in comms_workers {
        match tokio::time::timeout(
            worker_shutdown_timeout,
            tokio::task::spawn_blocking(move || worker.join()),
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => eprintln!("Comms worker panicked during shutdown"),
            Ok(Err(e)) => eprintln!("Comms worker join task failed: {e}"),
            Err(_) => eprintln!(
                "Comms worker did not shut down within {:?}",
                worker_shutdown_timeout
            ),
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
            Ok(Ok(Err(_))) => eprintln!("Router worker panicked during shutdown"),
            Ok(Err(e)) => eprintln!("Router worker join task failed: {e}"),
            Err(_) => eprintln!(
                "Router worker did not shut down within {:?}",
                worker_shutdown_timeout
            ),
        }
    }

    // Stop intake first, then wait for packet worker to drain packet queue.
    drop(packet_tx);
    match tokio::time::timeout(worker_shutdown_timeout, packet_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Packet worker ended with error: {e}"),
        Err(_) => eprintln!(
            "Packet worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }

    match tokio::time::timeout(worker_shutdown_timeout, db_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("DB worker ended with error: {e}"),
        Err(_) => eprintln!(
            "DB worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }
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

fn queue_guarded_fill_command(
    state: &Arc<AppState>,
    router: &Router,
    data_type: DataType,
    key_cmd_id: u8,
    desired_state: bool,
    cmd_payload: u8,
    label: &str,
) {
    if state
        .get_pending_umbilical_valve_state(key_cmd_id)
        .is_some()
    {
        return;
    }
    if effective_umbilical_valve_state(state, key_cmd_id) == Some(desired_state) {
        return;
    }
    if let Err(err) = router.log_queue(data_type, &[cmd_payload]) {
        log_telemetry_error(&format!("failed to queue {label}"), err);
        return;
    }
    log_command_dispatch(label, "umbilical_comms", data_type, &[cmd_payload]);
    state.set_pending_umbilical_valve_state(key_cmd_id, desired_state);
    sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
}

const VALVE_STATE_DATA_TYPE: &str = "VALVE_STATE";
const UMBILICAL_PENDING_VALVE_MISMATCH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);

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
    for write in writes {
        match write {
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
            } => {
                sqlx::query(
                    "INSERT INTO alerts (timestamp_ms, severity, message) VALUES (?, ?, ?)",
                )
                .bind(*timestamp_ms)
                .bind(severity.as_str())
                .bind(message.as_str())
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
    if let Some(active) = active {
        if let Err(e) = insert_db_batch_with_retry(active, batch).await {
            eprintln!("DB insert failed after retry; preserving batch for retry: {e}");
            return false;
        }
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
        Err(TrySendError::Full(DbQueueItem::Control(_))) => {
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

async fn handle_packet(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    pkt: Packet,
) -> Vec<TelemetryRow> {
    state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
    if cfg!(feature = "test_fire_mode")
        && *state.state.lock().unwrap() == FlightState::Startup
        && state.all_required_boards_seen()
    {
        state.set_local_flight_state(FlightState::Idle);
        state.set_sequence_policy_state(crate::sequences::SequencePolicyState::default());
        sequences::refresh_action_policy_now(state);
        state.broadcast_action_policy_snapshot();
    }
    let sender_id = canonical_sender_id(pkt.sender()).to_string();

    if pkt.data_type() == DataType::Warning {
        if let Ok(msg) = pkt.data_as_string() {
            emit_warning(state, msg.to_string());
        } else {
            emit_warning(state, "Warning packet with invalid UTF-8 payload");
            report_parse_error(
                state,
                &sender_id,
                pkt.data_type().as_str(),
                "invalid UTF-8 payload",
            );
        }
        return Vec::new();
    }

    if pkt.data_type() == DataType::MessageData || pkt.data_type() == DataType::OrderedMessage {
        let message = match pkt.data_as_string() {
            Ok(msg) => msg.to_string(),
            Err(_) => {
                report_parse_error(
                    state,
                    &sender_id,
                    pkt.data_type().as_str(),
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

    if pkt.data_type() == DataType::FlightState {
        if !cfg!(feature = "testing") && !state.all_required_boards_seen() {
            return Vec::new();
        }
        let pkt_data = match pkt.data_as_u8() {
            Ok(data) => *data.first().expect("index 0 does not exist"),
            Err(_) => {
                report_parse_error(
                    state,
                    &sender_id,
                    pkt.data_type().as_str(),
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
                    pkt.data_type().as_str(),
                    "unknown flight-state code",
                );
                return Vec::new();
            }
        };
        {
            let mut fs = state.state.lock().unwrap();
            *fs = new_flight_state;
        }
        let ts_ms = pkt.timestamp() as i64;
        let launch_clock_ts_ms = if sender_id == Board::GroundStation.sender_id() {
            ts_ms
        } else {
            get_current_timestamp_ms() as i64
        };
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

        let _ = state.state_tx.send(FlightStateMsg {
            state: new_flight_state,
        });
        state.broadcast_fill_targets_snapshot();
        return Vec::new();
    }

    if pkt.data_type() == DataType::UmbilicalStatus {
        if let Ok(data) = pkt.data_as_u8()
            && data.len() == 2
        {
            let cmd_id = data[0];
            let on = data[1] != 0;
            if cmd_id == ValveBoardCommands::Sequence as u8 {
                if on {
                    start_launch_clock_from_valve_sequence_status(state);
                }
                return Vec::new();
            }
            if let Some((key_cmd_id, key_on)) = umbilical_state_key(cmd_id, on) {
                state.set_umbilical_valve_state(key_cmd_id, key_on);
                state.reconcile_pending_umbilical_valve_state(
                    key_cmd_id,
                    key_on,
                    UMBILICAL_PENDING_VALVE_MISMATCH_TIMEOUT,
                );
                if key_cmd_id == ValveBoardCommands::PilotOpen as u8 && key_on {
                    transition_launch_clock_to_t_plus_from_pilot_open(state);
                }
                sequences::refresh_action_policy_now(state);
                state.broadcast_action_policy_snapshot();

                let ts_ms = get_current_timestamp_ms() as i64;
                let values = valve_state_values(state);
                let values_vec: Vec<Option<f32>> = values.into_iter().collect();
                let values_json = serde_json::to_string(
                    &values_vec
                        .iter()
                        .map(|v| v.map(|n| n as f64))
                        .collect::<Vec<_>>(),
                )
                .ok();
                let payload_json = payload_json_from_pkt(&pkt);

                queue_db_write(
                    state,
                    db_tx,
                    db_overflow,
                    DbWrite::Telemetry {
                        timestamp_ms: ts_ms,
                        source_timestamp_ms: Some(pkt.timestamp() as i64),
                        data_type: VALVE_STATE_DATA_TYPE.to_string(),
                        sender_id: sender_id.clone(),
                        values_json,
                        payload_json,
                    },
                )
                .await;

                let row = TelemetryRow {
                    timestamp_ms: ts_ms,
                    data_type: VALVE_STATE_DATA_TYPE.to_string(),
                    sender_id,
                    values: values_vec,
                };
                return vec![row];
            }
        }
        report_parse_error(
            state,
            &sender_id,
            pkt.data_type().as_str(),
            "expected 2-byte umbilical status payload",
        );
        return Vec::new();
    }

    let data_type_str = pkt.data_type().as_str().to_string();
    let ts_ms = get_current_timestamp_ms() as i64;

    let payload_json = payload_json_from_pkt(&pkt);

    if pkt.data_type() == DataType::GpsSatelliteNumber {
        return handle_gps_satellite_count_packet(state, db_tx, db_overflow, &pkt, &payload_json)
            .await
            .into_iter()
            .collect();
    }

    if let Some(values_vec) = telemetry_f32_values(&pkt) {
        let rows = telemetry_rows_from_packet_values(state, &pkt, &sender_id, values_vec);
        let mut out = Vec::with_capacity(rows.len());

        for (row_data_type, row_values) in rows {
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
                if row_data_type == DataType::BatteryVoltage.as_str()
                    && is_fill_system_battery_sender(&sender_id)
                    && update_fill_system_low_voltage_latch(first_value)
                {
                    emit_warning(state, FILL_SYSTEM_LOW_VOLTAGE_WARNING);
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

                if matches!(
                    row_data_type.as_str(),
                    loadcell::RAW_LOADCELL_DATA_TYPE_1000KG
                        | loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE
                        | "FUEL_TANK_PRESSURE"
                ) {
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
                        },
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

            out.push(TelemetryRow {
                timestamp_ms: ts_ms,
                data_type: row_data_type,
                sender_id: sender_id.clone(),
                values: row_values,
            });
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

        if pkt.data_type() == DataType::Heartbeat {
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
            pkt.data_type().as_str(),
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

fn log_telemetry_error(context: &str, err: sedsprintf_rs_2026::TelemetryError) {
    eprintln!("{context}: {:?}", err);
}

fn log_router_rx_error(err: sedsprintf_rs_2026::TelemetryError) {
    if matches!(
        err,
        sedsprintf_rs_2026::TelemetryError::Deserialize(_)
            | sedsprintf_rs_2026::TelemetryError::InvalidType
    ) {
        log_router_decode_error(err);
    } else {
        log_telemetry_error("router rx queue processing failed", err);
    }
}

fn log_router_decode_error(err: sedsprintf_rs_2026::TelemetryError) {
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

fn process_router_queues(router: &Router) -> Result<(), sedsprintf_rs_2026::TelemetryError> {
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

    flight_sim::reset_simulation();
    state.clear_runtime_data_for_sim_reset();
    {
        let mut flight_state = state.state.lock().unwrap();
        *flight_state = FlightState::Idle;
    }
    state.reset_launch_clock();
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

    fn timesync_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

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
            DataType::ValveCommand,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([1_u8]),
        )
        .expect("failed to build valve command packet");
        let wire = serialize::serialize_packet(&pkt);
        let msg = radio_command_log_line("radio TX sent", "rocket_comms", &wire)
            .expect("command packet should emit message");
        assert!(msg.contains("rocket_comms: radio TX sent ValveCommand"));

        let telemetry_pkt = Packet::new(
            DataType::KG1000,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
            Board::DaqBoard.sender_id(),
            123,
            f32_payload(&[1.0]),
        )
        .expect("failed to build kg1000 packet");
        let telemetry_wire = serialize::serialize_packet(&telemetry_pkt);
        assert!(radio_command_log_line("radio TX sent", "rocket_comms", &telemetry_wire).is_none());
    }

    #[test]
    fn rocket_drop_filter_matches_fill_system_commands_only() {
        let actuator_pkt = Packet::new(
            DataType::ActuatorCommand,
            &[sedsprintf_rs_2026::config::DataEndpoint::ActuatorBoard],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([9_u8]),
        )
        .expect("failed to build actuator command packet");
        assert!(is_fill_system_command_payload(
            &serialize::serialize_packet(&actuator_pkt)
        ));

        let valve_pkt = Packet::new(
            DataType::ValveCommand,
            &[sedsprintf_rs_2026::config::DataEndpoint::ValveBoard],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([1_u8]),
        )
        .expect("failed to build valve command packet");
        assert!(is_fill_system_command_payload(
            &serialize::serialize_packet(&valve_pkt)
        ));

        let abort_pkt = Packet::new(
            DataType::Abort,
            &[sedsprintf_rs_2026::config::DataEndpoint::Abort],
            Board::GroundStation.sender_id(),
            123,
            Arc::from("Manual Abort Command Issued".as_bytes()),
        )
        .expect("failed to build abort packet");
        assert!(is_fill_system_command_payload(
            &serialize::serialize_packet(&abort_pkt)
        ));

        let flight_pkt = Packet::new(
            DataType::FlightCommand,
            &[sedsprintf_rs_2026::config::DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([0_u8]),
        )
        .expect("failed to build flight command packet");
        assert!(!is_fill_system_command_payload(
            &serialize::serialize_packet(&flight_pkt)
        ));
    }

    #[test]
    fn queue_abort_packet_transmits_to_router_side() {
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sent_clone = sent.clone();
        let router = Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        );
        router.add_side_serialized_with_options(
            "umbilical_comms",
            move |bytes| {
                sent_clone
                    .lock()
                    .expect("failed to lock sent packets")
                    .push(bytes.to_vec());
                Ok(())
            },
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            .filter_map(|wire| serialize::deserialize_packet(wire).ok())
            .filter(|pkt| pkt.data_type() == DataType::Abort)
            .collect::<Vec<_>>();
        assert_eq!(abort_packets.len(), 1);
        assert_eq!(abort_packets[0].payload(), b"Manual Abort Command Issued");
    }

    #[test]
    fn flight_command_transmits_to_rocket_side_when_fc_is_discovered_there() {
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sent_clone = sent.clone();
        let router = Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        );
        let rocket_side = router.add_side_serialized_with_options(
            "rocket_comms",
            move |bytes| {
                sent_clone
                    .lock()
                    .expect("failed to lock sent packets")
                    .push(bytes.to_vec());
                Ok(())
            },
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
            },
        );
        let discovery = sedsprintf_rs_2026::discovery::build_discovery_announce(
            Board::RFBoard.sender_id(),
            123,
            &[DataEndpoint::FlightController],
        )
        .expect("failed to build RF discovery");
        let discovery_wire = serialize::serialize_packet(&discovery);
        router
            .rx_serialized_from_side(&discovery_wire, rocket_side)
            .expect("failed to queue RF discovery");
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process RF discovery");

        queue_locally_routed_flight_command(
            &router,
            "test flight command",
            &[FlightComputerCommands::VigilantMode as u8],
        )
        .expect("failed to queue flight command");
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process router queues");

        let sent = sent.lock().expect("failed to lock sent packets");
        let flight_packets = sent
            .iter()
            .filter_map(|wire| serialize::deserialize_packet(wire).ok())
            .filter(|pkt| pkt.data_type() == DataType::FlightCommand)
            .collect::<Vec<_>>();
        assert!(!flight_packets.is_empty());
        assert_eq!(
            flight_packets[0].payload(),
            &[FlightComputerCommands::VigilantMode as u8]
        );
        assert!(
            flight_packets[0]
                .endpoints()
                .contains(&DataEndpoint::FlightController)
        );
    }

    #[tokio::test]
    async fn launch_command_also_queues_valve_board_sequence() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sent_clone = sent.clone();
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let valve_side = router.add_side_serialized_with_options(
            "umbilical_comms",
            move |bytes| {
                sent_clone
                    .lock()
                    .expect("failed to lock sent packets")
                    .push(bytes.to_vec());
                Ok(())
            },
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
            },
        );
        let discovery = sedsprintf_rs_2026::discovery::build_discovery_announce(
            Board::ValveBoard.sender_id(),
            123,
            &[DataEndpoint::ValveBoard],
        )
        .expect("failed to build valve-board discovery");
        router
            .rx_serialized_from_side(&serialize::serialize_packet(&discovery), valve_side)
            .expect("failed to queue valve-board discovery");
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process valve-board discovery");

        handle_flight_computer_launch_command(state, router.clone()).await;
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process launch command queues");

        let sent = sent.lock().expect("failed to lock sent packets");
        let valve_sequence_seen = sent
            .iter()
            .filter_map(|wire| serialize::deserialize_packet(wire).ok())
            .any(|pkt| {
                pkt.data_type() == DataType::ValveCommand
                    && pkt.payload() == &[ValveBoardCommands::Sequence as u8]
                    && pkt.endpoints().contains(&DataEndpoint::ValveBoard)
            });
        assert!(
            valve_sequence_seen,
            "normal launch should also queue the valve-board sequence command"
        );
    }

    struct TestRadioComms {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        scheduler_status: Option<Arc<Mutex<Vec<(u8, bool)>>>>,
        windows: VecDeque<crate::comms::RadioWindowUpdate>,
        inject_tx_on_recv: Option<(mpsc::UnboundedSender<Vec<u8>>, Vec<u8>)>,
        fail_sends: usize,
    }

    impl CommsDevice for TestRadioComms {
        fn recv_packet(
            &mut self,
            _router: &Router,
            _packet_tap: &mut dyn FnMut(&Packet),
        ) -> sedsprintf_rs_2026::TelemetryResult<()> {
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
        ) -> sedsprintf_rs_2026::TelemetryResult<()> {
            if let Some((tx, payload)) = self.inject_tx_on_recv.take() {
                tx.send(payload)
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
                } else if self.radio_turn_grant_sent && max_packets > 0 {
                    if let Some(payload) = rf_tx_queue.pop_front() {
                        packet_sink(payload);
                        self.radio_turn_sent += 1;
                    }
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
        ) -> sedsprintf_rs_2026::TelemetryResult<()> {
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
        ) -> sedsprintf_rs_2026::TelemetryResult<()> {
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
            if control.cmd == "VigilantMode" {
                control.enabled = true;
            }
        }
        state.set_action_policy(policy);
        let mut ws_rx = state.ws_tx.subscribe();

        let handler_state = state.clone();
        let ground_station_handler =
            sedsprintf_rs_2026::router::EndpointHandler::new_packet_handler(
                DataEndpoint::GroundStation,
                move |pkt: &Packet| {
                    handler_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                    handler_state.mark_packet_received(get_current_timestamp_ms());
                    handler_state.ring_buffer.lock().unwrap().push(pkt.clone());
                    Ok(())
                },
            );
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([ground_station_handler]),
        ));
        let (radio_tx, radio_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        register_flight_command_tx_side("rocket_comms", radio_tx.clone());
        let side_id = {
            let radio_tx = radio_tx.clone();
            router.add_side_serialized_with_options(
                "rocket_comms",
                move |bytes| {
                    radio_tx.send(bytes.to_vec()).map_err(|_| {
                        sedsprintf_rs_2026::TelemetryError::HandlerError("radio tx closed")
                    })?;
                    Ok(())
                },
                sedsprintf_rs_2026::router::RouterSideOptions {
                    reliable_enabled: true,
                    link_local_enabled: true,
                },
            )
        };
        let rf_discovery = sedsprintf_rs_2026::discovery::build_discovery_announce(
            Board::RFBoard.sender_id(),
            1,
            &[DataEndpoint::FlightController],
        )
        .expect("failed to build RF discovery");
        router
            .rx_serialized_from_side(&serialize::serialize_packet(&rf_discovery), side_id)
            .expect("failed to queue RF discovery");
        router
            .process_all_queues_with_timeout(0)
            .expect("failed to process RF discovery");

        let gps_values = [31.7619_f32, -106.485_f32, 1412.5_f32];
        let gps_pkt = Packet::new(
            DataType::GpsData,
            &[DataEndpoint::GroundStation],
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
                rf_tx_queue: VecDeque::from([serialize::serialize_packet(&gps_pkt).to_vec()]),
                updates: VecDeque::new(),
                scheduler: RfBoardScheduler::new(),
            })));
        let shutdown_rx = state.shutdown_subscribe();
        let telemetry = tokio::spawn(telemetry_task(
            state.clone(),
            router,
            vec![CommsWorkerHandle {
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

        state
            .cmd_tx
            .send(TelemetryCommand::VigilantMode)
            .await
            .expect("failed to send command through AppState command channel");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state
                    .last_command_ms
                    .lock()
                    .expect("failed to lock last command map")
                    .contains_key("VigilantMode")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timed out waiting for telemetry task to accept VigilantMode command");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row = ws_rx.recv().await.expect("telemetry websocket closed");
                if row.sender_id == Board::RFBoard.sender_id()
                    && row.data_type == DataType::GpsData.as_str()
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
                let sent = sent_from_ground
                    .lock()
                    .expect("failed to lock ground tx packets");
                let flight_command_seen = sent
                    .iter()
                    .filter_map(|payload| serialize::deserialize_packet(payload).ok())
                    .any(|pkt| {
                        pkt.data_type() == DataType::FlightCommand
                            && pkt.payload() == &[FlightComputerCommands::VigilantMode as u8]
                            && pkt.endpoints().contains(&DataEndpoint::FlightController)
                    });
                if flight_command_seen {
                    break;
                }
                drop(sent);
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
        assert!(
            scheduler_status
                .lock()
                .expect("failed to lock scheduler statuses")
                .iter()
                .any(|(_, has_more)| !*has_more),
            "GS should yield the RF-board uplink window after sending the command"
        );
    }

    #[tokio::test]
    async fn dedicated_radio_worker_sends_flight_command_during_uplink_window() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::serialize_packet(&pkt).to_vec();
        tx.send(wire.clone())
            .expect("failed to queue flight command to radio worker");
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
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
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                if sent_guard.iter().any(|payload| payload == &wire) {
                    break;
                }
                drop(sent_guard);
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
            .filter_map(|payload| serialize::deserialize_packet(payload).ok())
            .filter(|pkt| pkt.data_type() == DataType::FlightCommand)
            .collect::<Vec<_>>();
        assert!(!flight_packets.is_empty());
        assert_eq!(
            flight_packets[0].payload(),
            &[FlightComputerCommands::VigilantMode as u8]
        );
        assert!(
            flight_packets[0]
                .endpoints()
                .contains(&DataEndpoint::FlightController)
        );
    }

    #[tokio::test]
    async fn dedicated_radio_worker_drains_command_arriving_during_window_poll_before_yield() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
            },
        );
        let sent = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let scheduler_status = Arc::new(Mutex::new(Vec::<(u8, bool)>::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let pkt = Packet::new(
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::serialize_packet(&pkt).to_vec();
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
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                let statuses = scheduler_status
                    .lock()
                    .expect("failed to lock scheduler statuses");
                if sent_guard.iter().any(|payload| payload == &wire)
                    && statuses.iter().any(|status| *status == (9, false))
                {
                    break;
                }
                drop(statuses);
                drop(sent_guard);
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
        assert!(statuses.iter().any(|status| *status == (9, false)));
    }

    #[tokio::test]
    async fn dedicated_radio_worker_waits_for_uplink_window_before_flight_command() {
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx).await;
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::serialize_packet(&pkt).to_vec();
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
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

        tx.send(wire.clone())
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
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::serialize_packet(&pkt).to_vec();
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
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

        tx.send(wire)
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
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            DataType::Heartbeat,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([]),
        )
        .expect("failed to build telemetry packet");
        let telemetry_wire = serialize::serialize_packet(&telemetry_pkt).to_vec();
        let command_pkt = Packet::new(
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            124,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let command_wire = serialize::serialize_packet(&command_pkt).to_vec();
        tx.send(telemetry_wire)
            .expect("failed to queue telemetry to radio worker");
        tx.send(command_wire.clone())
            .expect("failed to queue flight command to radio worker");
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
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
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                if !sent_guard.is_empty() {
                    break;
                }
                drop(sent_guard);
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
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::serialize_packet(&pkt).to_vec();
        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
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

        tx.send(wire.clone())
            .expect("failed to queue flight command to radio worker");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                if sent_guard.iter().any(|payload| payload == &wire) {
                    break;
                }
                drop(sent_guard);
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
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::serialize_packet(&pkt).to_vec();
        tx.send(wire)
            .expect("failed to queue flight command to radio worker");

        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
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
        let router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));
        let side_id = router.add_side_serialized_with_options(
            "rocket_comms",
            |_bytes| Ok(()),
            sedsprintf_rs_2026::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
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
            DataType::FlightCommand,
            &[DataEndpoint::FlightController],
            Board::GroundStation.sender_id(),
            123,
            Arc::from([FlightComputerCommands::VigilantMode as u8]),
        )
        .expect("failed to build flight command packet");
        let wire = serialize::serialize_packet(&pkt).to_vec();
        let expected_window_sends = 5usize;
        for _ in 0..(expected_window_sends + 1) {
            tx.send(wire.clone())
                .expect("failed to queue flight command to radio worker");
        }

        let workers = spawn_dedicated_radio_io_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
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
                let sent_guard = sent.lock().expect("failed to lock sent radio payloads");
                let statuses = scheduler_status
                    .lock()
                    .expect("failed to lock scheduler statuses");
                if sent_guard.len() >= expected_window_sends
                    && statuses.iter().any(|status| *status == (7, true))
                {
                    break;
                }
                drop(statuses);
                drop(sent_guard);
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
        assert!(statuses.iter().any(|status| *status == (7, true)));
    }

    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
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

        let remote_router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([
                sedsprintf_rs_2026::router::EndpointHandler::new_packet_handler(
                    DataEndpoint::FlightState,
                    move |pkt: &Packet| {
                        if let Some(state_code) = pkt.payload().first().copied() {
                            remote_states_handler
                                .lock()
                                .expect("failed to lock remote states")
                                .push(state_code);
                        }
                        Ok(())
                    },
                ),
            ]),
        ));
        let gs_router = Arc::new(Router::new(
            sedsprintf_rs_2026::router::RouterMode::Relay,
            sedsprintf_rs_2026::router::RouterConfig::new([]),
        ));

        let gs_peer = Arc::new(Mutex::new(None::<(Arc<Router>, RouterSideId)>));
        let remote_peer = Arc::new(Mutex::new(None::<(Arc<Router>, RouterSideId)>));

        let gs_side = {
            let gs_peer = gs_peer.clone();
            gs_router.add_side_serialized_with_options(
                "umbilical_comms",
                move |bytes| {
                    let (peer, ingress) = gs_peer
                        .lock()
                        .expect("failed to lock gs peer")
                        .clone()
                        .expect("gs peer not initialized");
                    peer.rx_serialized_from_side(bytes, ingress)
                },
                sedsprintf_rs_2026::router::RouterSideOptions {
                    reliable_enabled: true,
                    link_local_enabled: true,
                },
            )
        };
        let remote_side = {
            let remote_peer = remote_peer.clone();
            remote_router.add_side_serialized_with_options(
                "gs_link",
                move |bytes| {
                    let (peer, ingress) = remote_peer
                        .lock()
                        .expect("failed to lock remote peer")
                        .clone()
                        .expect("remote peer not initialized");
                    peer.rx_serialized_from_side(bytes, ingress)
                },
                sedsprintf_rs_2026::router::RouterSideOptions {
                    reliable_enabled: true,
                    link_local_enabled: true,
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

        for cmd in commands {
            cmd_tx
                .send(cmd)
                .await
                .expect("failed to send flight state command");
        }

        let remote_result = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                let states = remote_states.lock().expect("failed to lock remote states");
                if states.len() >= expected_states.len() {
                    break;
                }
                drop(states);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;

        state.request_shutdown();
        telemetry.await.expect("telemetry task join failed");

        assert_eq!(
            state.local_flight_state_snapshot() as u8,
            *expected_states.last().expect("expected states missing")
        );
        remote_result.expect("timed out waiting for remote flight state");
        let states = remote_states.lock().expect("failed to lock remote states");
        assert_eq!(states.as_slice(), expected_states.as_slice());
    }

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
                    last_seen_instant: None,
                    ema_gap_ms: None,
                    warned: false,
                },
            );
        }

        let state = Arc::new(AppState {
            ring_buffer: Arc::new(Mutex::new(RingBuffer::new(128))),
            cmd_tx,
            ws_tx,
            warnings_tx: broadcast::channel(4).0,
            errors_tx: broadcast::channel(4).0,
            alert_ack_state: Arc::new(Mutex::new(crate::web::AlertAckStateMsg::default())),
            alert_ack_tx: broadcast::channel(4).0,
            dashboard_reset_tx: broadcast::channel(4).0,
            db: Arc::new(Mutex::new(db)),
            db_path: Arc::new(Mutex::new("sqlite::memory:".to_string())),
            placeholder_db_path: "sqlite::memory:".to_string(),
            db_queue_tx: db_tx,
            auth_db,
            state: Arc::new(Mutex::new(FlightState::Startup)),
            state_tx,
            last_flight_state_packet_ts_ms: Arc::new(AtomicU64::new(0)),
            gpio: GpioPins::new(),
            board_status: Arc::new(Mutex::new(board_status)),
            board_status_tx,
            last_board_status_broadcast_ms: Arc::new(AtomicU64::new(0)),
            last_packet_rx_ms: Arc::new(AtomicU64::new(0)),
            umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
            pending_umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
            latest_fuel_tank_pressure: Arc::new(Mutex::new(None)),
            latest_fill_mass_kg: Arc::new(Mutex::new(None)),
            loadcell_calibration: Arc::new(Mutex::new(loadcell::load_or_default())),
            shutdown_tx,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            pending_db_writes: Arc::new(AtomicUsize::new(0)),
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
            DataType::GpsData,
            &[
                sedsprintf_rs_2026::config::DataEndpoint::GroundStation,
                sedsprintf_rs_2026::config::DataEndpoint::SdCard,
                sedsprintf_rs_2026::config::DataEndpoint::FlightController,
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

        assert_eq!(row.data_type, DataType::GpsData.as_str());
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
                assert_eq!(data_type, DataType::GpsData.as_str());
                assert_eq!(sender_id, Board::RFBoard.sender_id());
                assert_eq!(
                    serde_json::from_str::<Vec<Option<f32>>>(&values_json.unwrap()).unwrap(),
                    row.values
                );
            }
            other => panic!("unexpected DB item for GPS_DATA: {other:?}"),
        }

        let sat_pkt = Packet::new(
            DataType::GpsSatelliteNumber,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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
            DataType::IMUData,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
            Board::FlightComputer.sender_id(),
            234_567,
            f32_payload(&imu_values),
        )
        .expect("failed to build IMU_DATA packet");

        let mut rows = handle_packet(&state, &db_tx, &db_overflow, imu_pkt).await;
        rows.sort_by(|a, b| a.data_type.cmp(&b.data_type));

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].data_type, DataType::AccelData.as_str());
        assert_eq!(rows[0].values, vec![Some(0.2), Some(-0.1), Some(9.91)]);
        assert_eq!(rows[1].data_type, DataType::GyroData.as_str());
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

        assert_eq!(writes[0].0, DataType::AccelData.as_str());
        assert_eq!(writes[0].1, Board::FlightComputer.sender_id());
        assert_eq!(
            serde_json::from_str::<Vec<Option<f32>>>(&writes[0].2).unwrap(),
            vec![Some(0.2), Some(-0.1), Some(9.91)]
        );
        assert_eq!(writes[1].0, DataType::GyroData.as_str());
        assert_eq!(writes[1].1, Board::FlightComputer.sender_id());
        assert_eq!(
            serde_json::from_str::<Vec<Option<f32>>>(&writes[1].2).unwrap(),
            vec![Some(1.5), Some(-2.5), Some(3.5)]
        );
    }

    #[tokio::test]
    async fn kg1000_packet_emits_state_tab_loadcell_rows() {
        let (db_tx, mut db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let mut ws_rx = state.ws_tx.subscribe();

        let pkt = Packet::new(
            DataType::KG1000,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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
        for _ in 0..2 {
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
            vec![
                loadcell::DERIVED_FILL_PERCENT_DATA_TYPE,
                loadcell::DERIVED_WEIGHT_DATA_TYPE
            ]
        );
        assert_eq!(broadcast_rows[0].sender_id, Board::DaqBoard.sender_id());
        assert_eq!(broadcast_rows[1].sender_id, Board::DaqBoard.sender_id());
        assert_eq!(broadcast_rows[0].values, vec![Some(42.5)]);
        assert_eq!(broadcast_rows[1].values, vec![Some(4.25)]);
        assert_eq!(broadcast_rows[0].timestamp_ms, row.timestamp_ms);
        assert_eq!(broadcast_rows[1].timestamp_ms, row.timestamp_ms);

        let cache = state.recent_telemetry_snapshot();
        assert!(
            cache
                .iter()
                .any(|row| row.data_type == loadcell::DERIVED_WEIGHT_DATA_TYPE)
        );
        assert!(
            cache
                .iter()
                .any(|row| row.data_type == loadcell::DERIVED_FILL_PERCENT_DATA_TYPE)
        );

        let mut db_types = Vec::new();
        for _ in 0..3 {
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
                loadcell::DERIVED_FILL_PERCENT_DATA_TYPE.to_string(),
                loadcell::DERIVED_WEIGHT_DATA_TYPE.to_string(),
            ]
        );
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
            DataType::FuelTankPressure,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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

        assert_eq!(row.data_type, DataType::FuelTankPressure.as_str());
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
            DataType::BatteryVoltage,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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

        assert_eq!(row.data_type, DataType::BatteryVoltage.as_str());
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
    async fn fill_box_battery_voltage_below_thirteen_emits_latched_warning() {
        reset_fill_system_low_voltage_latch_for_tests();
        let (db_tx, _db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let mut warnings_rx = state.warnings_tx.subscribe();

        let pkt = Packet::new(
            DataType::BatteryVoltage,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
            "GB",
            678_902,
            f32_payload(&[12.9]),
        )
        .expect("failed to build gateway low BATTERY_VOLTAGE packet");

        let rows = handle_packet(&state, &db_tx, &db_overflow, pkt).await;
        assert_eq!(rows[0].data_type, DataType::BatteryVoltage.as_str());

        let warning = tokio::time::timeout(Duration::from_millis(100), warnings_rx.recv())
            .await
            .expect("low fill-box voltage should emit a warning")
            .expect("warnings channel should remain open");
        assert_eq!(warning.message, FILL_SYSTEM_LOW_VOLTAGE_WARNING);

        let pkt = Packet::new(
            DataType::BatteryVoltage,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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
            DataType::UmbilicalStatus,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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
            DataType::UmbilicalStatus,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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
    async fn battery_voltage_db_bucketing_is_sender_aware() {
        let (db_tx, mut db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();

        let pb_pkt = Packet::new(
            DataType::BatteryVoltage,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
            "PB",
            700_000,
            f32_payload(&[7.8]),
        )
        .expect("failed to build PB battery packet");
        let gb_pkt = Packet::new(
            DataType::BatteryVoltage,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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
                }))) if data_type == DataType::BatteryVoltage.as_str() => {
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
            DataType::FlightState,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
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
}
