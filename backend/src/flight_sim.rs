#[cfg(feature = "testing")]
use crate::fill_targets;
#[cfg(feature = "testing")]
use crate::loadcell;
#[cfg(feature = "testing")]
use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
#[cfg(feature = "testing")]
use crate::telemetry_task::get_current_timestamp_ms;
use crate::types::TelemetryCommand;
#[cfg(feature = "testing")]
use crate::types::{Board, FlightState};
#[cfg(feature = "testing")]
use rand::RngExt;
use sedsprintf_rs_2026::TelemetryResult;
#[cfg(feature = "testing")]
use sedsprintf_rs_2026::config::{DataEndpoint, DataType};
use sedsprintf_rs_2026::packet::Packet;
#[cfg(feature = "testing")]
use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
#[cfg(feature = "testing")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "testing")]
const BASE_LAT: f32 = 31.7619;
#[cfg(feature = "testing")]
const BASE_LON: f32 = -106.485;

#[cfg(feature = "testing")]
const SENSOR_PERIOD_MS: u64 = 25;
#[cfg(feature = "testing")]
const FLIGHT_STATE_PERIOD_MS: u64 = 1_000;
#[cfg(feature = "testing")]
const LAUNCH_COUNTDOWN_DURATION_MS: u64 = 10_000;
#[cfg(feature = "testing")]
const HOUSEKEEPING_PERIOD_MS: u64 = 900;
#[cfg(feature = "testing")]
const AV_BAY_BATTERY_CUTOFF_V: f32 = 6.3;
#[cfg(feature = "testing")]
const AV_BAY_BATTERY_MAX_V: f32 = 8.4;
#[cfg(feature = "testing")]
const VALVE_BOARD_BATTERY_CUTOFF_V: f32 = 6.3;
#[cfg(feature = "testing")]
const VALVE_BOARD_BATTERY_MAX_V: f32 = 8.4;
#[cfg(feature = "testing")]
const GROUND_STATION_BATTERY_CUTOFF_V: f32 = 13.3;
#[cfg(feature = "testing")]
const GROUND_STATION_BATTERY_MAX_V: f32 = 15.5;
#[cfg(feature = "testing")]
const LOADCELL_NOISE_KG: f32 = 0.01;
#[cfg(feature = "testing")]
const NITROGEN_PRESSURE_MAX_PSI: f32 = 125.0;
#[cfg(feature = "testing")]
const NITROUS_ROOM_TEMP_SATURATION_PSI: f32 = 745.0;
#[cfg(feature = "testing")]
const NITROUS_NEAR_EMPTY_PSI: f32 = 20.0;
#[cfg(feature = "testing")]
const NITROUS_LIQUID_HOLDUP_FRACTION: f32 = 0.12;
#[cfg(feature = "testing")]
const NITROUS_PRESSURE_RESPONSE_PER_S: f32 = 1.3;
#[cfg(feature = "testing")]
const NITROGEN_PRESSURE_RESPONSE_PER_S: f32 = 1.0;
#[cfg(feature = "testing")]
const NITROGEN_MASS_GAIN_KG_PER_S: f32 = 0.08;
#[cfg(feature = "testing")]
const GRAVITY_FPS2: f32 = 32.174;

#[cfg(feature = "testing")]
fn sim_full_mass_kg() -> f32 {
    sim_full_mass_kg_from(
        &fill_targets::load_or_default(),
        &loadcell::load_or_default(),
    )
}

#[cfg(feature = "testing")]
fn sim_full_mass_kg_from(
    fill_cfg: &fill_targets::FillTargetsConfig,
    loadcell_cfg: &loadcell::LoadcellCalibrationFile,
) -> f32 {
    fill_cfg
        .nitrous
        .target_mass_kg
        .max(
            loadcell_cfg
                .full_mass_kg
                .unwrap_or(loadcell::DEFAULT_FULL_MASS_KG),
        )
        .max(0.1)
}

#[cfg(feature = "testing")]
fn sim_nitrogen_target_mass_kg() -> f32 {
    sim_nitrogen_target_mass_kg_from(&fill_targets::load_or_default())
}

#[cfg(feature = "testing")]
fn sim_nitrogen_target_mass_kg_from(fill_cfg: &fill_targets::FillTargetsConfig) -> f32 {
    std::env::var("GS_SEQUENCE_NITROGEN_TARGET_MASS_KG")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or_else(|| fill_cfg.nitrogen.target_mass_kg.max(0.1))
}

#[cfg(feature = "testing")]
fn sim_nitrogen_pressure_ceiling_psi() -> f32 {
    sim_nitrogen_pressure_ceiling_psi_from(&fill_targets::load_or_default())
}

#[cfg(feature = "testing")]
fn sim_nitrogen_pressure_ceiling_psi_from(fill_cfg: &fill_targets::FillTargetsConfig) -> f32 {
    std::env::var("GS_SEQUENCE_NITROGEN_PRESSURE_TARGET_PSI")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0)
        .map(|target| target + 5.0)
        .unwrap_or(fill_cfg.nitrogen.target_pressure_psi + 5.0)
        .max(NITROGEN_PRESSURE_MAX_PSI)
}

#[cfg(feature = "testing")]
fn sim_nitrogen_pressure_setpoint_psi_from(fill_cfg: &fill_targets::FillTargetsConfig) -> f32 {
    std::env::var("GS_SEQUENCE_NITROGEN_PRESSURE_TARGET_PSI")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(fill_cfg.nitrogen.target_pressure_psi)
        .max(0.0)
}

#[cfg(feature = "testing")]
pub fn effective_fill_targets(
    mut cfg: fill_targets::FillTargetsConfig,
    loadcell_cfg: &loadcell::LoadcellCalibrationFile,
    flight_state: FlightState,
) -> fill_targets::FillTargetsConfig {
    if !sim_mode_enabled() {
        return cfg;
    }

    cfg.nitrogen.target_mass_kg = sim_nitrogen_target_mass_kg_from(&cfg);
    cfg.nitrogen.target_pressure_psi = sim_nitrogen_pressure_setpoint_psi_from(&cfg);
    cfg.nitrous.target_mass_kg = sim_full_mass_kg_from(&cfg, loadcell_cfg);
    cfg.nitrous.target_pressure_psi = cfg
        .nitrous
        .target_pressure_psi
        .max(NITROUS_ROOM_TEMP_SATURATION_PSI);

    if matches!(
        flight_state,
        FlightState::PreFill | FlightState::FillTest | FlightState::NitrogenFill
    ) {
        cfg.nitrous = cfg.nitrogen.clone();
    }

    cfg
}

#[cfg(not(feature = "testing"))]
pub fn effective_fill_targets(
    cfg: crate::fill_targets::FillTargetsConfig,
    _loadcell_cfg: &crate::loadcell::LoadcellCalibrationFile,
    _flight_state: crate::types::FlightState,
) -> crate::fill_targets::FillTargetsConfig {
    cfg
}

#[cfg(feature = "testing")]
fn nitrogen_pressure_target_psi(loadcell_mass_kg: f32) -> f32 {
    let mass_target = sim_nitrogen_target_mass_kg().max(0.1);
    let fill_frac = (loadcell_mass_kg / mass_target).clamp(0.0, 1.0);
    5.0 + fill_frac * (sim_nitrogen_pressure_ceiling_psi() - 5.0)
}

#[cfg(feature = "testing")]
fn nitrous_equilibrium_pressure_psi(loadcell_mass_kg: f32) -> f32 {
    let fill_target = sim_full_mass_kg();
    let fill_frac = (loadcell_mass_kg / fill_target).clamp(0.0, 1.0);
    if fill_frac >= NITROUS_LIQUID_HOLDUP_FRACTION {
        NITROUS_ROOM_TEMP_SATURATION_PSI
    } else {
        let liquid_frac = (fill_frac / NITROUS_LIQUID_HOLDUP_FRACTION).clamp(0.0, 1.0);
        NITROUS_NEAR_EMPTY_PSI
            + liquid_frac * (NITROUS_ROOM_TEMP_SATURATION_PSI - NITROUS_NEAR_EMPTY_PSI)
    }
}

#[cfg(feature = "testing")]
fn vented_pressure_target_psi(loadcell_mass_kg: f32, nitrous_loaded: bool) -> f32 {
    if loadcell_mass_kg <= 0.01 {
        return 0.0;
    }
    if nitrous_loaded {
        let fill_frac = (loadcell_mass_kg / sim_full_mass_kg()).clamp(0.0, 1.0);
        NITROUS_ROOM_TEMP_SATURATION_PSI * fill_frac
    } else {
        nitrogen_pressure_target_psi(loadcell_mass_kg)
    }
}

#[cfg(feature = "testing")]
#[derive(Debug)]
struct FlightSimState {
    flight_state: FlightState,
    launch_sequence_started_ms: Option<u64>,
    launch_time_ms: Option<u64>,
    last_state_emit_ms: u64,
    last_sensor_emit_ms: u64,
    last_housekeeping_emit_ms: u64,
    next_sensor_idx: usize,
    next_valve_emit_idx: usize,
    fuel_tank_pressure_psi: f32,
    fuel_flow_lpm: f32,
    battery_v: f32,
    battery_a: f32,
    altitude_ft: f32,
    velocity_fps: f32,
    accel_g: f32,
    roll_dps: f32,
    pitch_dps: f32,
    yaw_dps: f32,
    last_physics_ms: u64,
    av_bay_battery_v: f32,
    valve_board_battery_v: f32,
    valve_board_battery_a: f32,
    ground_station_battery_v: f32,
    ground_station_battery_a: f32,
    next_battery_sender_idx: usize,
    last_battery_sender: Board,
    loadcell_mass_kg: f32,
    valves: HashMap<u8, bool>,
    saw_dump_open_after_n2: bool,
    saw_dump_closed_after_n2: bool,
    nitrous_fill_started_ms: Option<u64>,
    queued: VecDeque<Packet>,
}

#[cfg(feature = "testing")]
fn valve_board_disconnected_for_state(state: FlightState) -> bool {
    matches!(
        state,
        FlightState::Launch
            | FlightState::Ascent
            | FlightState::Coast
            | FlightState::Apogee
            | FlightState::ParachuteDeploy
            | FlightState::Descent
            | FlightState::Landed
            | FlightState::Recovery
    )
}

#[cfg(feature = "testing")]
impl FlightSimState {
    fn new() -> Self {
        let mut valves = HashMap::new();
        // Start in idle with fill lines installed and both NO + dump open.
        // Closing both is required before entering fill sequence.
        valves.insert(ValveBoardCommands::NormallyOpenOpen as u8, true);
        valves.insert(ValveBoardCommands::DumpOpen as u8, true);
        valves.insert(ActuatorBoardCommands::RetractPlumbing as u8, false);

        Self {
            flight_state: FlightState::Idle,
            launch_sequence_started_ms: None,
            launch_time_ms: None,
            last_state_emit_ms: 0,
            last_sensor_emit_ms: 0,
            last_housekeeping_emit_ms: 0,
            next_sensor_idx: 0,
            next_valve_emit_idx: 0,
            fuel_tank_pressure_psi: 5.0,
            fuel_flow_lpm: 0.0,
            battery_v: AV_BAY_BATTERY_MAX_V,
            battery_a: 1.2,
            altitude_ft: 0.0,
            velocity_fps: 0.0,
            accel_g: 1.0,
            roll_dps: 0.0,
            pitch_dps: 0.0,
            yaw_dps: 0.0,
            last_physics_ms: 0,
            av_bay_battery_v: AV_BAY_BATTERY_MAX_V,
            valve_board_battery_v: VALVE_BOARD_BATTERY_MAX_V,
            valve_board_battery_a: 0.55,
            ground_station_battery_v: GROUND_STATION_BATTERY_MAX_V,
            ground_station_battery_a: 0.7,
            next_battery_sender_idx: 0,
            last_battery_sender: Board::PowerBoard,
            loadcell_mass_kg: 0.0,
            valves,
            saw_dump_open_after_n2: false,
            saw_dump_closed_after_n2: false,
            nitrous_fill_started_ms: None,
            queued: VecDeque::new(),
        }
    }

    fn valve_on(&self, cmd_id: u8) -> bool {
        self.valves.get(&cmd_id).copied().unwrap_or(false)
    }

    fn set_flight_state(&mut self, fs: FlightState, now_ms: u64) {
        if self.flight_state == fs {
            return;
        }
        self.flight_state = fs;
        if !matches!(fs, FlightState::FillTest) {
            self.saw_dump_open_after_n2 = false;
            self.saw_dump_closed_after_n2 = false;
        }
        if !matches!(fs, FlightState::NitrousFill | FlightState::Armed) {
            self.nitrous_fill_started_ms = None;
        }
        self.queue_flight_state(now_ms);
    }

    fn queue_flight_state(&mut self, now_ms: u64) {
        if let Ok(pkt) = Packet::new(
            DataType::FlightState,
            &[DataEndpoint::GroundStation, DataEndpoint::FlightState],
            Board::FlightComputer.sender_id(),
            now_ms,
            Arc::from([self.flight_state as u8]),
        ) {
            self.queued.push_back(pkt);
        }
    }

    fn pop_next_queued(&mut self) -> Option<Packet> {
        let flight_state_idx = self
            .queued
            .iter()
            .position(|pkt| pkt.data_type() == DataType::FlightState);
        flight_state_idx
            .and_then(|idx| self.queued.remove(idx))
            .or_else(|| self.queued.pop_front())
    }

    fn queue_abort(&mut self, board: Board, reason: &str, now_ms: u64) {
        if let Ok(pkt) = Packet::new(
            DataType::Abort,
            &[DataEndpoint::Abort],
            board.sender_id(),
            now_ms,
            Arc::<[u8]>::from(reason.as_bytes().to_vec()),
        ) {
            self.queued.push_back(pkt);
        }
    }

    fn queue_umbilical_status(&mut self, cmd_id: u8, on: bool, now_ms: u64) {
        let sender = if is_valve_board_command(cmd_id) {
            Board::ValveBoard.sender_id()
        } else {
            Board::ActuatorBoard.sender_id()
        };
        if let Ok(pkt) = Packet::new(
            DataType::UmbilicalStatus,
            &[DataEndpoint::GroundStation],
            sender,
            now_ms,
            Arc::from([cmd_id, if on { 1 } else { 0 }]),
        ) {
            self.queued.push_back(pkt);
        }
    }

    fn queue_board_heartbeat(&mut self, board: Board, now_ms: u64) {
        if let Ok(pkt) = Packet::new(
            DataType::Heartbeat,
            &[DataEndpoint::GroundStation],
            board.sender_id(),
            now_ms,
            Arc::from(Vec::<u8>::new()),
        ) {
            self.queued.push_back(pkt);
        }
    }

    fn queue_scalar_f32(&mut self, dtype: DataType, sender: Board, value: f32, now_ms: u64) {
        let bytes = value.to_le_bytes();
        if let Ok(pkt) = Packet::new(
            dtype,
            &[DataEndpoint::GroundStation],
            sender.sender_id(),
            now_ms,
            Arc::from(bytes.as_slice()),
        ) {
            self.queued.push_back(pkt);
        }
    }

    fn queue_housekeeping(&mut self, now_ms: u64) {
        let valve_board_online = !valve_board_disconnected_for_state(self.flight_state);
        for board in Board::ALL {
            if *board == Board::ValveBoard && !valve_board_online {
                continue;
            }
            self.queue_board_heartbeat(*board, now_ms);
        }

        let keys: &[u8] = if valve_board_online {
            &[
                ValveBoardCommands::PilotOpen as u8,
                ValveBoardCommands::NormallyOpenOpen as u8,
                ValveBoardCommands::DumpOpen as u8,
                ActuatorBoardCommands::IgniterOn as u8,
                ActuatorBoardCommands::NitrogenOpen as u8,
                ActuatorBoardCommands::NitrousOpen as u8,
                ActuatorBoardCommands::RetractPlumbing as u8,
            ]
        } else {
            &[
                ActuatorBoardCommands::IgniterOn as u8,
                ActuatorBoardCommands::NitrogenOpen as u8,
                ActuatorBoardCommands::NitrousOpen as u8,
                ActuatorBoardCommands::RetractPlumbing as u8,
            ]
        };
        let key = keys[self.next_valve_emit_idx % keys.len()];
        self.next_valve_emit_idx = (self.next_valve_emit_idx + 1) % keys.len();
        let on = self.valve_on(key);
        self.queue_umbilical_status(key, on, now_ms);

        // Keep battery telemetry present even when other sensor traffic is sparse.
        self.queue_scalar_f32(
            DataType::BatteryVoltage,
            Board::PowerBoard,
            self.av_bay_battery_v,
            now_ms,
        );
        self.queue_scalar_f32(
            DataType::BatteryCurrent,
            Board::PowerBoard,
            self.battery_a,
            now_ms,
        );
        if valve_board_online {
            self.queue_scalar_f32(
                DataType::BatteryVoltage,
                Board::ValveBoard,
                self.valve_board_battery_v,
                now_ms,
            );
            self.queue_scalar_f32(
                DataType::BatteryCurrent,
                Board::ValveBoard,
                self.valve_board_battery_a,
                now_ms,
            );
        }
        self.queue_scalar_f32(
            DataType::BatteryVoltage,
            Board::GatewayBoard,
            self.ground_station_battery_v,
            now_ms,
        );
        self.queue_scalar_f32(
            DataType::BatteryCurrent,
            Board::GatewayBoard,
            self.ground_station_battery_a,
            now_ms,
        );
    }

    fn apply_command(&mut self, cmd: &TelemetryCommand, now_ms: u64) {
        match cmd {
            TelemetryCommand::Abort => {
                self.launch_sequence_started_ms = None;
                self.launch_time_ms = None;
                self.valves
                    .insert(ActuatorBoardCommands::IgniterOn as u8, false);
                self.queue_umbilical_status(ActuatorBoardCommands::IgniterOn as u8, false, now_ms);
                self.valves
                    .insert(ValveBoardCommands::PilotOpen as u8, false);
                self.queue_umbilical_status(ValveBoardCommands::PilotOpen as u8, false, now_ms);
                self.queue_abort(Board::ValveBoard, "simulated valve board abort", now_ms);
                self.queue_abort(
                    Board::ActuatorBoard,
                    "simulated actuator board abort",
                    now_ms,
                );
                self.set_flight_state(FlightState::Aborted, now_ms);
            }
            TelemetryCommand::Launch => {
                if self.flight_state == FlightState::Armed
                    && self.launch_sequence_started_ms.is_none()
                {
                    self.launch_sequence_started_ms = Some(now_ms);
                    self.valves
                        .insert(ActuatorBoardCommands::IgniterOn as u8, true);
                    self.queue_umbilical_status(
                        ActuatorBoardCommands::IgniterOn as u8,
                        true,
                        now_ms,
                    );
                    self.set_flight_state(FlightState::Launch, now_ms);
                }
            }
            TelemetryCommand::Dump => {
                let key = ValveBoardCommands::DumpOpen as u8;
                let next = !self.valve_on(key);
                self.valves.insert(key, next);
                self.queue_umbilical_status(key, next, now_ms);
                if self.flight_state == FlightState::FillTest && next {
                    self.saw_dump_open_after_n2 = true;
                }
                if self.flight_state == FlightState::FillTest
                    && !next
                    && self.saw_dump_open_after_n2
                {
                    self.saw_dump_closed_after_n2 = true;
                }
            }
            TelemetryCommand::NormallyOpen => {
                let key = ValveBoardCommands::NormallyOpenOpen as u8;
                let next = !self.valve_on(key);
                self.valves.insert(key, next);
                self.queue_umbilical_status(key, next, now_ms);
            }
            TelemetryCommand::Pilot => {
                let key = ValveBoardCommands::PilotOpen as u8;
                let next = !self.valve_on(key);
                self.valves.insert(key, next);
                self.queue_umbilical_status(key, next, now_ms);
            }
            TelemetryCommand::Igniter => {
                let key = ActuatorBoardCommands::IgniterOn as u8;
                let next = !self.valve_on(key);
                self.valves.insert(key, next);
                self.queue_umbilical_status(key, next, now_ms);
            }
            TelemetryCommand::RetractPlumbing => {
                let key = ActuatorBoardCommands::RetractPlumbing as u8;
                self.valves.insert(key, true);
                self.queue_umbilical_status(key, true, now_ms);
            }
            TelemetryCommand::Nitrogen => {
                let key = ActuatorBoardCommands::NitrogenOpen as u8;
                let next = !self.valve_on(key);
                self.valves.insert(key, next);
                self.queue_umbilical_status(key, next, now_ms);
            }
            TelemetryCommand::NitrogenClose => {
                let key = ActuatorBoardCommands::NitrogenOpen as u8;
                self.valves.insert(key, false);
                self.queue_umbilical_status(key, false, now_ms);
            }
            TelemetryCommand::Nitrous => {
                let key = ActuatorBoardCommands::NitrousOpen as u8;
                let next = !self.valve_on(key);
                self.valves.insert(key, next);
                self.queue_umbilical_status(key, next, now_ms);
            }
            TelemetryCommand::NitrousClose => {
                let key = ActuatorBoardCommands::NitrousOpen as u8;
                self.valves.insert(key, false);
                self.queue_umbilical_status(key, false, now_ms);
            }
            TelemetryCommand::ContinueFillSequence => {
                if self.flight_state == FlightState::FillTest {
                    self.nitrous_fill_started_ms.get_or_insert(now_ms);
                    self.set_flight_state(FlightState::NitrousFill, now_ms);
                }
            }
            TelemetryCommand::StartWritingNow
            | TelemetryCommand::StartWritingLastTwoMinutes
            | TelemetryCommand::PauseWritingDb
            | TelemetryCommand::StopWritingDb => {
                // Recording controls are backend-local and do not affect simulator state.
            }
            #[cfg(feature = "hitl_mode")]
            TelemetryCommand::DeployParachute
            | TelemetryCommand::ExpandParachute
            | TelemetryCommand::ReinitSensors
            | TelemetryCommand::LaunchSignal
            | TelemetryCommand::EvaluationRelax
            | TelemetryCommand::EvaluationFocus
            | TelemetryCommand::EvaluationAbort
            | TelemetryCommand::ReinitBarometer
            | TelemetryCommand::EnableIMU
            | TelemetryCommand::DisableIMU
            | TelemetryCommand::MonitorAltitude
            | TelemetryCommand::RevokeMonitorAltitude
            | TelemetryCommand::ConsecutiveSamples
            | TelemetryCommand::RevokeConsecutiveSamples
            | TelemetryCommand::ResetFailures
            | TelemetryCommand::RevokeResetFailures
            | TelemetryCommand::ValidateMeasms
            | TelemetryCommand::RevokeValidateMeasms
            | TelemetryCommand::AbortAfter15
            | TelemetryCommand::AbortAfter40
            | TelemetryCommand::AbortAfter70
            | TelemetryCommand::ReinitAfter12
            | TelemetryCommand::ReinitAfter26
            | TelemetryCommand::ReinitAfter44
            | TelemetryCommand::AdvanceFlightState
            | TelemetryCommand::RewindFlightState => {
                // No-op in simulator mode; these commands are forwarded by telemetry_task.
            }
        }

        self.update_launch_sequence(now_ms);
        self.update_ground_sequence(now_ms);
    }

    fn update_launch_sequence(&mut self, now_ms: u64) {
        let Some(sequence_start_ms) = self.launch_sequence_started_ms else {
            return;
        };

        if self.flight_state == FlightState::Aborted {
            self.launch_sequence_started_ms = None;
            self.launch_time_ms = None;
            return;
        }

        if self.launch_time_ms.is_none()
            && now_ms.saturating_sub(sequence_start_ms) >= LAUNCH_COUNTDOWN_DURATION_MS
        {
            let pilot_key = ValveBoardCommands::PilotOpen as u8;
            if !self.valve_on(pilot_key) {
                self.valves.insert(pilot_key, true);
                self.queue_umbilical_status(pilot_key, true, now_ms);
            }
            self.launch_time_ms = Some(now_ms);
            self.set_flight_state(FlightState::Ascent, now_ms);
        }

        if now_ms.saturating_sub(sequence_start_ms) >= LAUNCH_COUNTDOWN_DURATION_MS {
            let igniter_key = ActuatorBoardCommands::IgniterOn as u8;
            if self.valve_on(igniter_key) {
                self.valves.insert(igniter_key, false);
                self.queue_umbilical_status(igniter_key, false, now_ms);
            }
            self.launch_sequence_started_ms = None;
        }
    }

    fn update_ground_sequence(&mut self, now_ms: u64) {
        if self.launch_sequence_started_ms.is_some() || self.launch_time_ms.is_some() {
            return;
        }

        let no_open = !self.valve_on(ValveBoardCommands::NormallyOpenOpen as u8);
        let dump_closed = !self.valve_on(ValveBoardCommands::DumpOpen as u8);
        let n2_open = self.valve_on(ActuatorBoardCommands::NitrogenOpen as u8);
        let n2o_open = self.valve_on(ActuatorBoardCommands::NitrousOpen as u8);
        let fill_lines_removed = self.valve_on(ActuatorBoardCommands::RetractPlumbing as u8);

        match self.flight_state {
            FlightState::Idle => {
                if no_open && dump_closed {
                    self.set_flight_state(FlightState::PreFill, now_ms);
                }
            }
            FlightState::PreFill => {
                if n2_open {
                    self.set_flight_state(FlightState::NitrogenFill, now_ms);
                }
            }
            FlightState::NitrogenFill => {
                if !n2_open {
                    self.set_flight_state(FlightState::FillTest, now_ms);
                }
            }
            FlightState::FillTest => {
                if self.saw_dump_open_after_n2 && self.saw_dump_closed_after_n2 && n2o_open {
                    self.set_flight_state(FlightState::NitrousFill, now_ms);
                    self.nitrous_fill_started_ms.get_or_insert(now_ms);
                }
            }
            FlightState::NitrousFill => {
                if n2o_open {
                    self.nitrous_fill_started_ms.get_or_insert(now_ms);
                }
                if !n2o_open
                    && fill_lines_removed
                    && self
                        .nitrous_fill_started_ms
                        .is_some_and(|t0| now_ms.saturating_sub(t0) >= 30_000)
                {
                    self.set_flight_state(FlightState::Armed, now_ms);
                }
            }
            _ => {}
        }
    }

    fn update_physics(&mut self, now_ms: u64) {
        self.update_launch_sequence(now_ms);

        let dt_s = if self.last_physics_ms == 0 {
            SENSOR_PERIOD_MS as f32 / 1000.0
        } else {
            ((now_ms.saturating_sub(self.last_physics_ms)) as f32 / 1000.0).clamp(0.0, 1.0)
        };
        self.last_physics_ms = now_ms;

        let n2_open = self.valve_on(ActuatorBoardCommands::NitrogenOpen as u8);
        let n2o_open = self.valve_on(ActuatorBoardCommands::NitrousOpen as u8);
        let no_open = self.valve_on(ValveBoardCommands::NormallyOpenOpen as u8);
        let dump_open = self.valve_on(ValveBoardCommands::DumpOpen as u8);
        let nitrous_loaded = self.nitrous_fill_started_ms.is_some();
        let fill_target = sim_full_mass_kg();
        let nitrogen_target = sim_nitrogen_target_mass_kg();

        if n2_open && !dump_open {
            self.loadcell_mass_kg =
                (self.loadcell_mass_kg + dt_s * NITROGEN_MASS_GAIN_KG_PER_S).min(nitrogen_target);
        } else if n2o_open && !dump_open {
            self.loadcell_mass_kg =
                (self.loadcell_mass_kg + dt_s * (fill_target / 18.0)).min(fill_target);
        } else if dump_open || no_open {
            self.loadcell_mass_kg = (self.loadcell_mass_kg - dt_s * 0.35).max(0.0);
        }

        if dump_open || no_open {
            let target_psi = vented_pressure_target_psi(self.loadcell_mass_kg, nitrous_loaded);
            let vent_response = if dump_open && no_open { 3.0 } else { 2.1 };
            let max_step = vent_response * dt_s.max(0.02) * 20.0;
            let delta = target_psi - self.fuel_tank_pressure_psi;
            self.fuel_tank_pressure_psi += delta.clamp(-max_step, max_step);
        } else if n2_open {
            let target_psi = nitrogen_pressure_target_psi(self.loadcell_mass_kg);
            let delta = target_psi - self.fuel_tank_pressure_psi;
            let max_step = NITROGEN_PRESSURE_RESPONSE_PER_S * dt_s.max(0.02) * 20.0;
            self.fuel_tank_pressure_psi += delta.clamp(-max_step, max_step);
        } else if n2o_open {
            // Nitrous is self-pressurizing: pressure trends toward equilibrium
            // while the actual quantity is determined by the loadcell mass.
            let target_psi = nitrous_equilibrium_pressure_psi(self.loadcell_mass_kg);
            let delta = target_psi - self.fuel_tank_pressure_psi;
            let max_step = NITROUS_PRESSURE_RESPONSE_PER_S * dt_s.max(0.02) * 20.0;
            self.fuel_tank_pressure_psi += delta.clamp(-max_step, max_step);
        } else {
            // Once nitrous is loaded, tank pressure should remain near vapor equilibrium
            // even after the valve closes; otherwise hold the existing pressure.
            if nitrous_loaded && self.loadcell_mass_kg > 0.05 {
                let target_psi = nitrous_equilibrium_pressure_psi(self.loadcell_mass_kg);
                let delta = target_psi - self.fuel_tank_pressure_psi;
                let max_step = (NITROUS_PRESSURE_RESPONSE_PER_S * 0.35) * dt_s.max(0.02) * 20.0;
                self.fuel_tank_pressure_psi += delta.clamp(-max_step, max_step);
            }
            self.fuel_tank_pressure_psi = self.fuel_tank_pressure_psi.max(0.0);
        }

        if let Some(t0_ms) = self.launch_time_ms {
            let t = (now_ms.saturating_sub(t0_ms) as f32) / 1000.0;
            self.apply_flight_profile(t, dt_s, now_ms);
        } else {
            self.altitude_ft = (self.altitude_ft - 0.5).max(0.0);
            self.velocity_fps = 0.0;
            self.accel_g = 1.0;
            self.roll_dps = 0.2;
            self.pitch_dps = 0.2;
            self.yaw_dps = 0.3;
            self.fuel_flow_lpm = if n2_open {
                6.0
            } else if n2o_open {
                3.2
            } else {
                0.0
            };
        }

        self.battery_a = (1.0 + self.fuel_flow_lpm * 0.12).min(35.0);
        let valve_board_online = !valve_board_disconnected_for_state(self.flight_state);
        self.valve_board_battery_a = if valve_board_online {
            if self.launch_time_ms.is_some() {
                0.85
            } else {
                0.55
            }
        } else {
            0.0
        };
        self.ground_station_battery_a = if self.launch_time_ms.is_some() {
            0.95
        } else {
            0.7
        };

        // Make simulated pack drain visible over test sessions instead of effectively flat.
        let av_bay_drop_v_per_s = 0.00035 + self.battery_a * 0.00012;
        let valve_board_drop_v_per_s = 0.00024 + self.valve_board_battery_a * 0.00010;
        let gs_drop_v_per_s = 0.00020 + self.ground_station_battery_a * 0.00008;

        self.av_bay_battery_v =
            (self.av_bay_battery_v - av_bay_drop_v_per_s * dt_s).max(AV_BAY_BATTERY_CUTOFF_V);
        self.valve_board_battery_v = (self.valve_board_battery_v - valve_board_drop_v_per_s * dt_s)
            .max(VALVE_BOARD_BATTERY_CUTOFF_V);
        self.ground_station_battery_v = (self.ground_station_battery_v - gs_drop_v_per_s * dt_s)
            .max(GROUND_STATION_BATTERY_CUTOFF_V);
        self.battery_v = self.av_bay_battery_v;
    }

    fn apply_flight_profile(&mut self, t: f32, dt_s: f32, now_ms: u64) {
        let dt_s = dt_s.clamp(0.0, 0.2);
        let current_net_accel_fps2 = (self.accel_g - 1.0) * GRAVITY_FPS2;
        let (state, target_accel_fps2, flow_lpm) = if t < 2.0 {
            (FlightState::Launch, 52.0, 45.0)
        } else if t < 10.0 {
            let p = (t - 2.0) / 8.0;
            (FlightState::Ascent, 32.0 - 10.0 * p, 58.0)
        } else if t < 18.0 {
            let p = (t - 10.0) / 8.0;
            (FlightState::Ascent, 22.0 - 20.0 * p, 34.0 * (1.0 - p))
        } else if t < 43.0 {
            (
                FlightState::Coast,
                -24.0 - self.velocity_fps.max(0.0) * 0.035,
                0.0,
            )
        } else if t < 46.0 {
            (
                FlightState::Apogee,
                -16.0 - self.velocity_fps.abs() * 0.02,
                0.0,
            )
        } else if t < 54.0 {
            let chute_terminal_fps = -55.0;
            (
                FlightState::ParachuteDeploy,
                (chute_terminal_fps - self.velocity_fps) * 1.4,
                0.0,
            )
        } else if self.altitude_ft > 4.0 {
            let terminal_fps = -52.0;
            (
                FlightState::Descent,
                (terminal_fps - self.velocity_fps) * 0.65,
                0.0,
            )
        } else if t < 182.0 {
            (FlightState::Landed, 0.0, 0.0)
        } else {
            (FlightState::Recovery, 0.0, 0.0)
        };

        let accel_alpha = if dt_s <= 0.0 {
            1.0
        } else {
            (1.0 - f32::exp(-dt_s * 3.5)).clamp(0.0, 1.0)
        };
        let net_accel_fps2 =
            current_net_accel_fps2 + (target_accel_fps2 - current_net_accel_fps2) * accel_alpha;

        self.velocity_fps += net_accel_fps2 * dt_s;
        self.altitude_ft = (self.altitude_ft + self.velocity_fps * dt_s).max(0.0);

        if matches!(state, FlightState::Landed | FlightState::Recovery) {
            self.altitude_ft = 0.0;
            self.velocity_fps = 0.0;
        }

        self.set_flight_state(state, now_ms);
        self.accel_g = 1.0 + net_accel_fps2 / GRAVITY_FPS2;
        self.fuel_flow_lpm = flow_lpm;

        let mut rng = rand::rng();
        self.roll_dps = rng.random_range(-2.0..2.0);
        self.pitch_dps = rng.random_range(-2.0..2.0);
        self.yaw_dps = rng.random_range(-6.0..6.0);
    }

    fn next_sensor_packet(&mut self, now_ms: u64) -> TelemetryResult<Packet> {
        self.update_physics(now_ms);

        let seq = [
            DataType::GyroData,
            DataType::AccelData,
            DataType::KalmanFilterData,
            DataType::BarometerData,
            DataType::FuelTankPressure,
            DataType::FuelFlow,
            DataType::BatteryVoltage,
            DataType::BatteryCurrent,
            DataType::GpsData,
            DataType::GpsSatelliteNumber,
            DataType::KG1000,
        ];
        let dtype = seq[self.next_sensor_idx % seq.len()];
        self.next_sensor_idx = (self.next_sensor_idx + 1) % seq.len();

        let mut rng = rand::rng();
        let mut sender = sender_for_datatype(dtype);
        let bytes: Vec<u8> = match dtype {
            DataType::GyroData => vec![
                self.roll_dps + rng.random_range(-0.15..0.15),
                self.pitch_dps + rng.random_range(-0.15..0.15),
                self.yaw_dps + rng.random_range(-0.45..0.45),
            ]
            .into_iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
            DataType::AccelData => {
                let az = self.accel_g * 9.80665 + rng.random_range(-0.25..0.25);
                vec![
                    rng.random_range(-0.35..0.35),
                    rng.random_range(-0.35..0.35),
                    az,
                ]
                .into_iter()
                .flat_map(|v| v.to_le_bytes())
                .collect()
            }
            DataType::KalmanFilterData => vec![
                self.altitude_ft * 0.3048,
                self.velocity_fps * 0.3048,
                self.accel_g,
            ]
            .into_iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
            DataType::BarometerData => {
                let altitude_m = self.altitude_ft * 0.3048;
                let pressure_pa = 101_325.0_f32 * f32::powf(1.0 - altitude_m / 44_330.0, 5.255);
                let temp_c = (24.0 - altitude_m * 0.0065).clamp(-20.0, 35.0);
                vec![pressure_pa, temp_c, altitude_m]
                    .into_iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect()
            }
            DataType::FuelTankPressure => vec![self.fuel_tank_pressure_psi]
                .into_iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
            DataType::FuelFlow => vec![self.fuel_flow_lpm]
                .into_iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
            DataType::BatteryVoltage => {
                let valve_board_online = !valve_board_disconnected_for_state(self.flight_state);
                let battery_sources: &[(Board, f32)] = if valve_board_online {
                    &[
                        (Board::PowerBoard, self.av_bay_battery_v),
                        (Board::ValveBoard, self.valve_board_battery_v),
                        (Board::GatewayBoard, self.ground_station_battery_v),
                    ]
                } else {
                    &[
                        (Board::PowerBoard, self.av_bay_battery_v),
                        (Board::GatewayBoard, self.ground_station_battery_v),
                    ]
                };
                let (board, voltage) =
                    battery_sources[self.next_battery_sender_idx % battery_sources.len()];
                self.next_battery_sender_idx =
                    (self.next_battery_sender_idx + 1) % battery_sources.len();
                self.last_battery_sender = board;
                sender = board.sender_id();
                vec![voltage]
            }
            .into_iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
            DataType::BatteryCurrent => {
                let current = match self.last_battery_sender {
                    Board::GatewayBoard => {
                        sender = Board::GatewayBoard.sender_id();
                        self.ground_station_battery_a
                    }
                    Board::ValveBoard => {
                        sender = Board::ValveBoard.sender_id();
                        self.valve_board_battery_a
                    }
                    _ => {
                        sender = Board::PowerBoard.sender_id();
                        self.battery_a
                    }
                };
                vec![current]
            }
            .into_iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
            DataType::GpsData => {
                let dlat_deg = (self.altitude_ft / 5_280.0) * 0.00001;
                let dlon_deg = dlat_deg * 0.8;
                vec![
                    BASE_LAT + dlat_deg + rng.random_range(-0.00002..0.00002),
                    BASE_LON + dlon_deg + rng.random_range(-0.00002..0.00002),
                    self.altitude_ft * 0.3048,
                ]
                .into_iter()
                .flat_map(|v| v.to_le_bytes())
                .collect()
            }
            DataType::GpsSatelliteNumber => {
                let satellites = 10 + ((now_ms / 5_000) % 6) as u8;
                vec![satellites]
            }
            DataType::KG1000 => {
                let raw_kg = (self.loadcell_mass_kg
                    + rng.random_range(-LOADCELL_NOISE_KG..LOADCELL_NOISE_KG))
                .max(0.0)
                .min(sim_full_mass_kg());
                vec![raw_kg]
                    .into_iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect()
            }
            _ => vec![0.0_f32]
                .into_iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        };

        Packet::new(
            dtype,
            &[DataEndpoint::GroundStation],
            sender,
            now_ms,
            Arc::from(bytes.as_slice()),
        )
    }
}

#[cfg(feature = "testing")]
fn sender_for_datatype(dtype: DataType) -> &'static str {
    match dtype {
        DataType::GyroData
        | DataType::AccelData
        | DataType::KalmanFilterData
        | DataType::FlightState => Board::FlightComputer.sender_id(),
        DataType::BarometerData | DataType::FuelFlow | DataType::FuelTankPressure => {
            Board::DaqBoard.sender_id()
        }
        DataType::KG1000 => Board::DaqBoard.sender_id(),
        DataType::BatteryVoltage | DataType::BatteryCurrent => Board::PowerBoard.sender_id(),
        DataType::GpsData | DataType::GpsSatelliteNumber => Board::GatewayBoard.sender_id(),
        _ => Board::GroundStation.sender_id(),
    }
}

#[cfg(feature = "testing")]
fn is_valve_board_command(cmd_id: u8) -> bool {
    matches!(
        cmd_id,
        x if x == ValveBoardCommands::PilotOpen as u8
            || x == ValveBoardCommands::NormallyOpenOpen as u8
            || x == ValveBoardCommands::DumpOpen as u8
            || x == ValveBoardCommands::PilotClose as u8
            || x == ValveBoardCommands::NormallyOpenClose as u8
            || x == ValveBoardCommands::DumpClose as u8
    )
}

#[cfg(feature = "testing")]
fn sim() -> &'static Mutex<FlightSimState> {
    static SIM: OnceLock<Mutex<FlightSimState>> = OnceLock::new();
    SIM.get_or_init(|| Mutex::new(FlightSimState::new()))
}

pub fn sim_mode_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        if !cfg!(feature = "testing") {
            return false;
        }
        std::env::var("GS_SIM_MODE")
            .ok()
            .as_deref()
            .map(|v| !matches!(v, "0" | "false" | "FALSE" | "off" | "OFF"))
            .unwrap_or(true)
    })
}

#[cfg(feature = "testing")]
pub fn simulated_board_endpoints(board: Board) -> Vec<String> {
    let mut endpoints = match board {
        Board::GroundStation => Vec::new(),
        Board::FlightComputer => vec![
            DataEndpoint::FlightController.as_str().to_string(),
            DataEndpoint::FlightState.as_str().to_string(),
            DataEndpoint::SdCard.as_str().to_string(),
        ],
        Board::RFBoard => vec![
            DataEndpoint::FlightController.as_str().to_string(),
            DataEndpoint::FlightState.as_str().to_string(),
        ],
        Board::PowerBoard => Vec::new(),
        Board::ValveBoard => vec![
            DataEndpoint::ValveBoard.as_str().to_string(),
            DataEndpoint::Abort.as_str().to_string(),
            DataEndpoint::FlightState.as_str().to_string(),
        ],
        Board::GatewayBoard => vec![
            DataEndpoint::ValveBoard.as_str().to_string(),
            DataEndpoint::ActuatorBoard.as_str().to_string(),
            DataEndpoint::Abort.as_str().to_string(),
        ],
        Board::ActuatorBoard => vec![
            DataEndpoint::ActuatorBoard.as_str().to_string(),
            DataEndpoint::Abort.as_str().to_string(),
            DataEndpoint::FlightState.as_str().to_string(),
        ],
        Board::DaqBoard => Vec::new(),
    };
    endpoints.sort();
    endpoints.dedup();
    endpoints
}

#[cfg(not(feature = "testing"))]
pub fn simulated_board_endpoints(_board: crate::types::Board) -> Vec<String> {
    Vec::new()
}

#[cfg(feature = "testing")]
pub fn handle_command(cmd: &TelemetryCommand) -> bool {
    if !sim_mode_enabled() {
        return false;
    }
    let now_ms = get_current_timestamp_ms();
    let mut s = sim().lock().expect("flight sim mutex poisoned");
    s.apply_command(cmd, now_ms);
    true
}

#[cfg(feature = "testing")]
pub fn sync_local_flight_state(next_state: FlightState) {
    if !sim_mode_enabled() {
        return;
    }
    let now_ms = get_current_timestamp_ms();
    let mut s = sim().lock().expect("flight sim mutex poisoned");
    s.set_flight_state(next_state, now_ms);
}

#[cfg(not(feature = "testing"))]
pub fn sync_local_flight_state(_next_state: crate::types::FlightState) {}

#[cfg(feature = "testing")]
pub fn _next_state_aware_packet() -> TelemetryResult<Packet> {
    let now_ms = get_current_timestamp_ms();
    let mut s = sim().lock().expect("flight sim mutex poisoned");

    if let Some(pkt) = s.pop_next_queued() {
        return Ok(pkt);
    }

    if now_ms.saturating_sub(s.last_housekeeping_emit_ms) >= HOUSEKEEPING_PERIOD_MS {
        s.last_housekeeping_emit_ms = now_ms;
        s.queue_housekeeping(now_ms);
        if let Some(pkt) = s.pop_next_queued() {
            return Ok(pkt);
        }
    }

    if now_ms.saturating_sub(s.last_state_emit_ms) >= FLIGHT_STATE_PERIOD_MS {
        s.last_state_emit_ms = now_ms;
        s.queue_flight_state(now_ms);
        if let Some(pkt) = s.pop_next_queued() {
            return Ok(pkt);
        }
    }

    if now_ms.saturating_sub(s.last_sensor_emit_ms) < SENSOR_PERIOD_MS {
        // Keep packets flowing even under very fast poll cadence.
        let pkt = s.next_sensor_packet(now_ms)?;
        return Ok(s.pop_next_queued().unwrap_or(pkt));
    }

    s.last_sensor_emit_ms = now_ms;
    let pkt = s.next_sensor_packet(now_ms)?;
    Ok(s.pop_next_queued().unwrap_or(pkt))
}

#[cfg(not(feature = "testing"))]
pub fn handle_command(_cmd: &TelemetryCommand) -> bool {
    false
}

#[cfg(not(feature = "testing"))]
pub fn _next_state_aware_packet() -> TelemetryResult<Packet> {
    unreachable!("flight sim only available with testing feature")
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "testing")]
    #[test]
    fn flight_computer_simulated_endpoints_include_sd_card() {
        let endpoints = super::simulated_board_endpoints(crate::types::Board::FlightComputer);
        assert!(endpoints.iter().any(|endpoint| {
            endpoint == sedsprintf_rs_2026::config::DataEndpoint::SdCard.as_str()
        }));
    }

    #[cfg(feature = "testing")]
    #[test]
    fn queued_flight_state_is_prioritized_for_launch_clock_sync() {
        let mut sim = super::FlightSimState::new();
        sim.queue_umbilical_status(super::ActuatorBoardCommands::IgniterOn as u8, true, 1_000);
        sim.queue_flight_state(1_000);

        let pkt = sim
            .pop_next_queued()
            .expect("queued flight state packet should be returned");

        assert_eq!(
            pkt.data_type(),
            sedsprintf_rs_2026::config::DataType::FlightState
        );
    }
}
