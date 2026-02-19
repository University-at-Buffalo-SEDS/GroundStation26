#[cfg(feature = "testing")]
use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
#[cfg(feature = "testing")]
use crate::telemetry_task::get_current_timestamp_ms;
use groundstation_shared::TelemetryCommand;
#[cfg(feature = "testing")]
use groundstation_shared::{Board, FlightState};
#[cfg(feature = "testing")]
use rand::RngExt;
#[cfg(feature = "testing")]
use sedsprintf_rs_2026::config::{DataEndpoint, DataType};
#[cfg(feature = "testing")]
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
#[cfg(feature = "testing")]
use sedsprintf_rs_2026::TelemetryResult;
#[cfg(feature = "testing")]
use std::collections::{HashMap, VecDeque};
#[cfg(feature = "testing")]
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "testing")]
const BASE_LAT: f32 = 31.7619;
#[cfg(feature = "testing")]
const BASE_LON: f32 = -106.4850;

#[cfg(feature = "testing")]
const SENSOR_PERIOD_MS: u64 = 25;
#[cfg(feature = "testing")]
const FLIGHT_STATE_PERIOD_MS: u64 = 1_000;
#[cfg(feature = "testing")]
const HOUSEKEEPING_PERIOD_MS: u64 = 900;

#[cfg(feature = "testing")]
#[derive(Debug)]
struct FlightSimState {
    flight_state: FlightState,
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
    valves: HashMap<u8, bool>,
    saw_dump_open_after_n2: bool,
    saw_dump_closed_after_n2: bool,
    queued: VecDeque<TelemetryPacket>,
}

#[cfg(feature = "testing")]
impl FlightSimState {
    fn new() -> Self {
        Self {
            flight_state: FlightState::Idle,
            launch_time_ms: None,
            last_state_emit_ms: 0,
            last_sensor_emit_ms: 0,
            last_housekeeping_emit_ms: 0,
            next_sensor_idx: 0,
            next_valve_emit_idx: 0,
            fuel_tank_pressure_psi: 5.0,
            fuel_flow_lpm: 0.0,
            battery_v: 12.4,
            battery_a: 1.2,
            altitude_ft: 0.0,
            velocity_fps: 0.0,
            accel_g: 1.0,
            roll_dps: 0.0,
            pitch_dps: 0.0,
            yaw_dps: 0.0,
            valves: HashMap::new(),
            saw_dump_open_after_n2: false,
            saw_dump_closed_after_n2: false,
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
        self.queue_flight_state(now_ms);
    }

    fn queue_flight_state(&mut self, now_ms: u64) {
        if let Ok(pkt) = TelemetryPacket::new(
            DataType::FlightState,
            &[DataEndpoint::GroundStation],
            Board::FlightComputer.sender_id(),
            now_ms,
            Arc::from([self.flight_state as u8]),
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
        if let Ok(pkt) = TelemetryPacket::new(
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
        if let Ok(pkt) = TelemetryPacket::new(
            DataType::Heartbeat,
            &[DataEndpoint::GroundStation],
            board.sender_id(),
            now_ms,
            Arc::from(Vec::<u8>::new()),
        ) {
            self.queued.push_back(pkt);
        }
    }

    fn queue_housekeeping(&mut self, now_ms: u64) {
        for board in Board::ALL {
            self.queue_board_heartbeat(*board, now_ms);
        }

        let keys = [
            ValveBoardCommands::PilotOpen as u8,
            ValveBoardCommands::NormallyOpenOpen as u8,
            ValveBoardCommands::DumpOpen as u8,
            ActuatorBoardCommands::IgniterOn as u8,
            ActuatorBoardCommands::NitrogenOpen as u8,
            ActuatorBoardCommands::NitrousOpen as u8,
            ActuatorBoardCommands::RetractPlumbing as u8,
        ];
        let key = keys[self.next_valve_emit_idx % keys.len()];
        self.next_valve_emit_idx = (self.next_valve_emit_idx + 1) % keys.len();
        let on = self.valve_on(key);
        self.queue_umbilical_status(key, on, now_ms);
    }

    fn apply_command(&mut self, cmd: &TelemetryCommand, now_ms: u64) {
        match cmd {
            TelemetryCommand::Abort => {
                self.launch_time_ms = None;
                self.set_flight_state(FlightState::Aborted, now_ms);
            }
            TelemetryCommand::Launch => {
                if self.flight_state == FlightState::Armed {
                    self.launch_time_ms = Some(now_ms);
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
            TelemetryCommand::Nitrous => {
                let key = ActuatorBoardCommands::NitrousOpen as u8;
                let next = !self.valve_on(key);
                self.valves.insert(key, next);
                self.queue_umbilical_status(key, next, now_ms);
            }
        }

        self.update_ground_sequence(now_ms);
    }

    fn update_ground_sequence(&mut self, now_ms: u64) {
        if self.launch_time_ms.is_some() {
            return;
        }

        let no_open = !self.valve_on(ValveBoardCommands::NormallyOpenOpen as u8);
        let dump_closed = !self.valve_on(ValveBoardCommands::DumpOpen as u8);
        let n2_open = self.valve_on(ActuatorBoardCommands::NitrogenOpen as u8);
        let n2o_open = self.valve_on(ActuatorBoardCommands::NitrousOpen as u8);

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
                    self.set_flight_state(FlightState::Armed, now_ms);
                }
            }
            _ => {}
        }
    }

    fn update_physics(&mut self, now_ms: u64) {
        let n2_open = self.valve_on(ActuatorBoardCommands::NitrogenOpen as u8);
        let n2o_open = self.valve_on(ActuatorBoardCommands::NitrousOpen as u8);
        let dump_open = self.valve_on(ValveBoardCommands::DumpOpen as u8);

        if n2_open {
            self.fuel_tank_pressure_psi = (self.fuel_tank_pressure_psi + 0.9).min(125.0);
        } else if n2o_open && !dump_open {
            self.fuel_tank_pressure_psi = (self.fuel_tank_pressure_psi + 0.45).min(210.0);
        } else if dump_open {
            self.fuel_tank_pressure_psi = (self.fuel_tank_pressure_psi - 1.8).max(0.0);
        } else {
            self.fuel_tank_pressure_psi = (self.fuel_tank_pressure_psi - 0.03).max(0.0);
        }

        if let Some(t0_ms) = self.launch_time_ms {
            let t = (now_ms.saturating_sub(t0_ms) as f32) / 1000.0;
            self.apply_flight_profile(t, now_ms);
        } else {
            self.altitude_ft = (self.altitude_ft - 0.5).max(0.0);
            self.velocity_fps = 0.0;
            self.accel_g = 1.0;
            self.roll_dps = 0.2;
            self.pitch_dps = 0.2;
            self.yaw_dps = 0.3;
            self.fuel_flow_lpm = if n2_open || n2o_open { 6.0 } else { 0.0 };
        }

        self.battery_a = (1.0 + self.fuel_flow_lpm * 0.12).min(35.0);
        self.battery_v = (12.6 - self.battery_a * 0.03).max(10.5);
    }

    fn apply_flight_profile(&mut self, t: f32, now_ms: u64) {
        let (state, alt, vel, accel_g, flow_lpm) = if t < 2.0 {
            (FlightState::Launch, 150.0 * (t / 2.0), 90.0, 3.2, 45.0)
        } else if t < 34.0 {
            let p = (t - 2.0) / 32.0;
            (
                FlightState::Ascent,
                150.0 + 9_850.0 * p,
                330.0 * (1.0 - 0.2 * p),
                2.1,
                58.0,
            )
        } else if t < 43.0 {
            let p = (t - 34.0) / 9.0;
            (
                FlightState::Coast,
                10_000.0 + 500.0 * p,
                120.0 * (1.0 - p),
                1.0,
                0.0,
            )
        } else if t < 46.0 {
            (FlightState::Apogee, 10_500.0, 0.0, 1.0, 0.0)
        } else if t < 54.0 {
            let p = (t - 46.0) / 8.0;
            (
                FlightState::ParachuteDeploy,
                10_500.0 - 700.0 * p,
                -80.0,
                0.7,
                0.0,
            )
        } else if t < 174.0 {
            let p = (t - 54.0) / 120.0;
            (
                FlightState::Descent,
                (9_800.0 * (1.0 - p)).max(0.0),
                -85.0,
                0.95,
                0.0,
            )
        } else if t < 182.0 {
            (FlightState::Landed, 0.0, 0.0, 1.0, 0.0)
        } else {
            (FlightState::Recovery, 0.0, 0.0, 1.0, 0.0)
        };

        self.set_flight_state(state, now_ms);
        self.altitude_ft = alt;
        self.velocity_fps = vel;
        self.accel_g = accel_g;
        self.fuel_flow_lpm = flow_lpm;

        let mut rng = rand::rng();
        self.roll_dps = rng.random_range(-2.0..2.0);
        self.pitch_dps = rng.random_range(-2.0..2.0);
        self.yaw_dps = rng.random_range(-6.0..6.0);
    }

    fn next_sensor_packet(&mut self, now_ms: u64) -> TelemetryResult<TelemetryPacket> {
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
        ];
        let dtype = seq[self.next_sensor_idx % seq.len()];
        self.next_sensor_idx = (self.next_sensor_idx + 1) % seq.len();

        let mut rng = rand::rng();
        let values: Vec<f32> = match dtype {
            DataType::GyroData => vec![
                self.roll_dps + rng.random_range(-0.15..0.15),
                self.pitch_dps + rng.random_range(-0.15..0.15),
                self.yaw_dps + rng.random_range(-0.45..0.45),
            ],
            DataType::AccelData => {
                let az = self.accel_g * 9.80665 + rng.random_range(-0.25..0.25);
                vec![
                    rng.random_range(-0.35..0.35),
                    rng.random_range(-0.35..0.35),
                    az,
                ]
            }
            DataType::KalmanFilterData => vec![
                self.altitude_ft * 0.3048,
                self.velocity_fps * 0.3048,
                self.accel_g,
            ],
            DataType::BarometerData => {
                let altitude_m = self.altitude_ft * 0.3048;
                let pressure_pa = 101_325.0_f32 * f32::powf(1.0 - altitude_m / 44_330.0, 5.255);
                let temp_c = (24.0 - altitude_m * 0.0065).clamp(-20.0, 35.0);
                vec![pressure_pa, temp_c, altitude_m]
            }
            DataType::FuelTankPressure => vec![self.fuel_tank_pressure_psi],
            DataType::FuelFlow => vec![self.fuel_flow_lpm],
            DataType::BatteryVoltage => vec![self.battery_v],
            DataType::BatteryCurrent => vec![self.battery_a],
            DataType::GpsData => {
                let dlat_deg = (self.altitude_ft / 5_280.0) * 0.00001;
                let dlon_deg = dlat_deg * 0.8;
                vec![
                    BASE_LAT + dlat_deg + rng.random_range(-0.00002..0.00002),
                    BASE_LON + dlon_deg + rng.random_range(-0.00002..0.00002),
                    self.altitude_ft * 0.3048,
                ]
            }
            _ => vec![0.0],
        };

        let mut bytes = Vec::with_capacity(values.len() * 4);
        for v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }

        TelemetryPacket::new(
            dtype,
            &[DataEndpoint::GroundStation],
            sender_for_datatype(dtype),
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
        DataType::BatteryVoltage | DataType::BatteryCurrent => Board::PowerBoard.sender_id(),
        DataType::GpsData => Board::GatewayBoard.sender_id(),
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

#[cfg(feature = "testing")]
pub fn handle_command(cmd: &TelemetryCommand) -> bool {
    let now_ms = get_current_timestamp_ms();
    let mut s = sim().lock().expect("flight sim mutex poisoned");
    s.apply_command(cmd, now_ms);
    true
}

#[cfg(feature = "testing")]
pub fn next_state_aware_packet() -> TelemetryResult<TelemetryPacket> {
    let now_ms = get_current_timestamp_ms();
    let mut s = sim().lock().expect("flight sim mutex poisoned");

    if let Some(pkt) = s.queued.pop_front() {
        return Ok(pkt);
    }

    if now_ms.saturating_sub(s.last_housekeeping_emit_ms) >= HOUSEKEEPING_PERIOD_MS {
        s.last_housekeeping_emit_ms = now_ms;
        s.queue_housekeeping(now_ms);
        if let Some(pkt) = s.queued.pop_front() {
            return Ok(pkt);
        }
    }

    if now_ms.saturating_sub(s.last_state_emit_ms) >= FLIGHT_STATE_PERIOD_MS {
        s.last_state_emit_ms = now_ms;
        s.queue_flight_state(now_ms);
        if let Some(pkt) = s.queued.pop_front() {
            return Ok(pkt);
        }
    }

    if now_ms.saturating_sub(s.last_sensor_emit_ms) < SENSOR_PERIOD_MS {
        // Keep packets flowing even under very fast poll cadence.
        return s.next_sensor_packet(now_ms);
    }

    s.last_sensor_emit_ms = now_ms;
    s.next_sensor_packet(now_ms)
}

#[cfg(not(feature = "testing"))]
pub fn handle_command(_cmd: &TelemetryCommand) -> bool {
    false
}

#[cfg(not(feature = "testing"))]
pub fn next_state_aware_packet() -> TelemetryResult<TelemetryPacket> {
    unreachable!("flight sim only available with testing feature")
}
