use crate::gpio::Trigger;
use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
use crate::state::AppState;
use crate::web::emit_error;
use groundstation_shared::{FlightState, TelemetryCommand};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::interval;

//####################################################################
// The values assigned here are GPIO pin numbers on the Raspberry Pi
//####################################################################
// TODO: Set the correct GPIO pin numbers, all current numbers are placeholders.
pub const IGNITION_PIN: u8 = 5;
#[allow(dead_code)]
//TODO: finish gpio setup
pub const IGNITION_PIN_LED: u8 = 6;
pub const ABORT_PIN: u8 = 9;
pub const ABORT_PIN_LED: u8 = 10;
pub const LAUNCH_PIN: u8 = 3;
pub const LAUNCH_PIN_LED: u8 = 11;
pub const DUMP_PIN: u8 = 4;
pub const DUMP_PIN_LED: u8 = 12;
pub const RETRACT_PIN: u8 = 17;
pub const RETRACT_PIN_LED: u8 = 18;
pub const PILOT_VALVE_PIN: u8 = 27;
pub const PILOT_VALVE_LED: u8 = 28;
pub const NITROGEN_TANK_VALVE_PIN: u8 = 22;
pub const NITROGEN_TANK_VALVE_LED: u8 = 23;
pub const NITROUS_TANK_VALVE_PIN: u8 = 24;
pub const NITROUS_TANK_VALVE_LED: u8 = 25;
pub const NORMALLY_OPEN_PIN: u8 = 26;
pub const NORMALLY_OPEN_LED: u8 = 29;
//####################################################################

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FillStep {
    CloseNormallyOpen,
    CloseDump,
    OpenNitrogen,
    WaitForPressure,
    CloseNitrogen,
    LeakCheck,
    OpenDump,
    DumpWait,
    OpenNitrous,
    ReadyToLaunch,
}

#[derive(Clone, Copy, Debug)]
struct PanelConfig {
    leak_check: Duration,
    dump_wait: Duration,
    pressure_threshold_psi: f32,
}

impl PanelConfig {
    fn from_env() -> Self {
        let leak_check = std::env::var("GPIO_LEAK_CHECK_SEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(10));
        let dump_wait = std::env::var("GPIO_DUMP_WAIT_SEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(5));
        let pressure_threshold_psi = std::env::var("GPIO_PRESSURE_THRESHOLD_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(10.0);

        Self {
            leak_check,
            dump_wait,
            pressure_threshold_psi,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AllowedActions {
    abort: bool,
    launch: bool,
    dump: bool,
    normally_open: bool,
    pilot: bool,
    igniter: bool,
    nitrogen: bool,
    nitrous: bool,
    fill_lines: bool,
}

#[derive(Debug)]
struct SequenceState {
    step: FillStep,
    step_started_at: Option<Instant>,
}

pub fn setup_gpio_panel(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
    let gpio = state.gpio.clone();
    let cfg = PanelConfig::from_env();
    let allowed = Arc::new(Mutex::new(AllowedActions::default()));
    let seq = Arc::new(Mutex::new(SequenceState {
        step: FillStep::CloseNormallyOpen,
        step_started_at: None,
    }));

    // Inputs (buttons)
    gpio.setup_input_pin(ABORT_PIN)?;
    gpio.setup_input_pin(LAUNCH_PIN)?;
    gpio.setup_input_pin(DUMP_PIN)?;
    gpio.setup_input_pin(NORMALLY_OPEN_PIN)?;
    gpio.setup_input_pin(PILOT_VALVE_PIN)?;
    gpio.setup_input_pin(NITROGEN_TANK_VALVE_PIN)?;
    gpio.setup_input_pin(NITROUS_TANK_VALVE_PIN)?;
    gpio.setup_input_pin(RETRACT_PIN)?;

    // Outputs (LEDs + ignition line)
    gpio.setup_output_pin(IGNITION_PIN)?;
    gpio.setup_output_pin(ABORT_PIN_LED)?;
    gpio.setup_output_pin(LAUNCH_PIN_LED)?;
    gpio.setup_output_pin(DUMP_PIN_LED)?;
    gpio.setup_output_pin(NORMALLY_OPEN_LED)?;
    gpio.setup_output_pin(RETRACT_PIN_LED)?;
    gpio.setup_output_pin(PILOT_VALVE_LED)?;
    gpio.setup_output_pin(NITROGEN_TANK_VALVE_LED)?;
    gpio.setup_output_pin(NITROUS_TANK_VALVE_LED)?;

    setup_callbacks(&state, allowed.clone())?;

    tokio::spawn(gpio_led_task(state, cfg, allowed, seq));

    Ok(())
}

fn setup_callbacks(
    state: &Arc<AppState>,
    allowed: Arc<Mutex<AllowedActions>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let tx = state.cmd_tx.clone();
    let gpio = state.gpio.clone();
    let debounce = Duration::from_millis(50);

    let allowed_abort = allowed.clone();
    let tx_abort = tx.clone();
    let state_abort = state.clone();
    gpio.setup_callback_input_pin(ABORT_PIN, Trigger::RisingEdge, debounce, move |is_high| {
        if !is_high {
            return;
        }
        if !allowed_abort.lock().unwrap().abort {
            return;
        }
        if tx_abort.try_send(TelemetryCommand::Abort).is_err() {
            eprintln!("GPIO abort button: failed to send command");
        }
        emit_error(&state_abort, "Manual abort button pressed!".to_string());
    })?;

    setup_button_callback(
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        LAUNCH_PIN,
        |a| a.launch,
        TelemetryCommand::Launch,
        debounce,
    )?;
    setup_button_callback(
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        DUMP_PIN,
        |a| a.dump,
        TelemetryCommand::Dump,
        debounce,
    )?;
    setup_button_callback(
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        NORMALLY_OPEN_PIN,
        |a| a.normally_open,
        TelemetryCommand::NormallyOpen,
        debounce,
    )?;
    setup_button_callback(
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        PILOT_VALVE_PIN,
        |a| a.pilot,
        TelemetryCommand::Pilot,
        debounce,
    )?;
    setup_button_callback(
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        NITROGEN_TANK_VALVE_PIN,
        |a| a.nitrogen,
        TelemetryCommand::Nitrogen,
        debounce,
    )?;
    setup_button_callback(
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        NITROUS_TANK_VALVE_PIN,
        |a| a.nitrous,
        TelemetryCommand::Nitrous,
        debounce,
    )?;
    setup_button_callback(
        gpio.clone(),
        allowed.clone(),
        tx,
        RETRACT_PIN,
        |a| a.fill_lines,
        TelemetryCommand::RetractPlumbing,
        debounce,
    )?;

    Ok(())
}

fn setup_button_callback<F>(
    gpio: Arc<crate::gpio::GpioPins>,
    allowed: Arc<Mutex<AllowedActions>>,
    tx: mpsc::Sender<TelemetryCommand>,
    pin: u8,
    can_press: F,
    cmd: TelemetryCommand,
    debounce: Duration,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn(&AllowedActions) -> bool + Send + Sync + 'static,
{
    gpio.setup_callback_input_pin(pin, Trigger::RisingEdge, debounce, move |is_high| {
        if !is_high {
            return;
        }
        if !can_press(&allowed.lock().unwrap()) {
            return;
        }
        if tx.try_send(cmd.clone()).is_err() {
            eprintln!("GPIO button pin {pin}: failed to send command");
        }
    })?;
    Ok(())
}

async fn gpio_led_task(
    state: Arc<AppState>,
    cfg: PanelConfig,
    allowed: Arc<Mutex<AllowedActions>>,
    seq: Arc<Mutex<SequenceState>>,
) {
    let mut tick = interval(Duration::from_millis(200));
    loop {
        tick.tick().await;

        let flight_state = *state.state.lock().unwrap();
        update_sequence(&state, &cfg, &seq, flight_state);
        let actions = compute_allowed_actions(&state, flight_state, &cfg, &seq);

        {
            let mut slot = allowed.lock().unwrap();
            *slot = actions;
        }

        let gpio = &state.gpio;
        set_led(gpio, ABORT_PIN_LED, actions.abort);
        set_led(gpio, LAUNCH_PIN_LED, actions.launch);
        set_led(gpio, DUMP_PIN_LED, actions.dump);
        set_led(gpio, NORMALLY_OPEN_LED, actions.normally_open);
        set_led(gpio, PILOT_VALVE_LED, actions.pilot);
        set_led(gpio, NITROGEN_TANK_VALVE_LED, actions.nitrogen);
        set_led(gpio, NITROUS_TANK_VALVE_LED, actions.nitrous);
        set_led(gpio, RETRACT_PIN_LED, actions.fill_lines);
    }
}

fn update_sequence(
    state: &AppState,
    cfg: &PanelConfig,
    seq: &Arc<Mutex<SequenceState>>,
    flight_state: FlightState,
) {
    if !is_fill_state(flight_state) {
        let mut s = seq.lock().unwrap();
        s.step = FillStep::CloseNormallyOpen;
        s.step_started_at = None;
        return;
    }

    let now = Instant::now();
    let valve = |cmd| state.get_umbilical_valve_state(cmd);
    let normally_open = valve(ValveBoardCommands::NormallyOpenOpen as u8);
    let dump_open = valve(ValveBoardCommands::DumpOpen as u8);
    let nitrogen_open = valve(ActuatorBoardCommands::NitrogenOpen as u8);
    let nitrous_open = valve(ActuatorBoardCommands::NitrousOpen as u8);
    let pressure = *state.latest_fuel_tank_pressure.lock().unwrap();

    let mut s = seq.lock().unwrap();
    match s.step {
        FillStep::CloseNormallyOpen => {
            if normally_open == Some(false) {
                s.step = FillStep::CloseDump;
            }
        }
        FillStep::CloseDump => {
            if dump_open == Some(false) {
                s.step = FillStep::OpenNitrogen;
            }
        }
        FillStep::OpenNitrogen => {
            if nitrogen_open == Some(true) {
                s.step = FillStep::WaitForPressure;
            }
        }
        FillStep::WaitForPressure => {
            if pressure.is_some_and(|p| p >= cfg.pressure_threshold_psi) {
                s.step = FillStep::CloseNitrogen;
            }
        }
        FillStep::CloseNitrogen => {
            if nitrogen_open == Some(false) {
                s.step = FillStep::LeakCheck;
                s.step_started_at = Some(now);
            }
        }
        FillStep::LeakCheck => {
            let elapsed = s.step_started_at.map(|t| now.saturating_duration_since(t));
            if elapsed.is_some_and(|d| d >= cfg.leak_check) {
                s.step = FillStep::OpenDump;
                s.step_started_at = None;
            }
        }
        FillStep::OpenDump => {
            if dump_open == Some(true) {
                s.step = FillStep::DumpWait;
                s.step_started_at = Some(now);
            }
        }
        FillStep::DumpWait => {
            let elapsed = s.step_started_at.map(|t| now.saturating_duration_since(t));
            if elapsed.is_some_and(|d| d >= cfg.dump_wait) {
                s.step = FillStep::OpenNitrous;
                s.step_started_at = None;
            }
        }
        FillStep::OpenNitrous => {
            if nitrous_open == Some(true) {
                s.step = FillStep::ReadyToLaunch;
            }
        }
        FillStep::ReadyToLaunch => {}
    }
}

fn compute_allowed_actions(
    state: &AppState,
    flight_state: FlightState,
    cfg: &PanelConfig,
    seq: &Arc<Mutex<SequenceState>>,
) -> AllowedActions {
    let mut actions = AllowedActions::default();
    actions.abort = true;

    if flight_state == FlightState::Armed {
        actions.launch = true;
        actions.dump = true;
        return actions;
    }

    if !is_fill_state(flight_state) {
        return actions;
    }

    let valve = |cmd| state.get_umbilical_valve_state(cmd);
    let normally_open = valve(ValveBoardCommands::NormallyOpenOpen as u8);
    let dump_open = valve(ValveBoardCommands::DumpOpen as u8);
    let nitrogen_open = valve(ActuatorBoardCommands::NitrogenOpen as u8);
    let nitrous_open = valve(ActuatorBoardCommands::NitrousOpen as u8);
    let pressure = *state.latest_fuel_tank_pressure.lock().unwrap();

    let step = seq.lock().unwrap().step;

    match step {
        FillStep::CloseNormallyOpen => {
            actions.normally_open = normally_open != Some(false);
        }
        FillStep::CloseDump => {
            actions.dump = dump_open != Some(false);
        }
        FillStep::OpenNitrogen => {
            actions.nitrogen = nitrogen_open != Some(true);
        }
        FillStep::WaitForPressure => {
            let _ = pressure.filter(|p| *p >= cfg.pressure_threshold_psi);
        }
        FillStep::CloseNitrogen => {
            actions.nitrogen = nitrogen_open != Some(false);
        }
        FillStep::LeakCheck => {}
        FillStep::OpenDump => {
            actions.dump = dump_open != Some(true);
        }
        FillStep::DumpWait => {}
        FillStep::OpenNitrous => {
            actions.nitrous = nitrous_open != Some(true);
        }
        FillStep::ReadyToLaunch => {}
    }

    // Keep extra buttons aligned with frontend availability during fill states.
    actions.pilot = true;
    actions.igniter = true;
    actions.fill_lines = true;

    actions
}

fn is_fill_state(state: FlightState) -> bool {
    matches!(
        state,
        FlightState::PreFill
            | FlightState::FillTest
            | FlightState::NitrogenFill
            | FlightState::NitrousFill
    )
}

fn set_led(gpio: &crate::gpio::GpioPins, pin: u8, on: bool) {
    if let Err(e) = gpio.write_output_pin(pin, on) {
        eprintln!("GPIO LED pin {pin} write failed: {e}");
    }
}
