use crate::gpio::Trigger;
use crate::sequences::{ActionPolicyMsg, BlinkMode};
use crate::state::AppState;
use crate::types::TelemetryCommand;
use crate::web::{emit_error, emit_warning};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;

//####################################################################
// The values assigned here are GPIO pin numbers on the Raspberry Pi
//####################################################################
// TODO: Set the correct GPIO pin numbers, all current numbers are placeholders.
pub const IGNITION_PIN: u8 = 5;
#[allow(dead_code)]
pub const IGNITION_PIN_LED: u8 = 6;

pub const ABORT_PIN: u8 = 9;
pub const ABORT_PIN_LED: u8 = 10;

pub const LAUNCH_PIN: u8 = 20;
pub const LAUNCH_PIN_LED: u8 = 11;

pub const DUMP_PIN: u8 = 4;
pub const DUMP_PIN_LED: u8 = 12;

pub const RETRACT_PIN: u8 = 17;
pub const RETRACT_PIN_LED: u8 = 18;

pub const PILOT_VALVE_PIN: u8 = 27;
pub const PILOT_VALVE_LED: u8 = 16; // was 28 (invalid)

pub const NITROGEN_TANK_VALVE_PIN: u8 = 22;
pub const NITROGEN_TANK_VALVE_LED: u8 = 23;

pub const NITROUS_TANK_VALVE_PIN: u8 = 24;
pub const NITROUS_TANK_VALVE_LED: u8 = 13;

pub const NORMALLY_OPEN_PIN: u8 = 26;
pub const NORMALLY_OPEN_LED: u8 = 19;

const LED_FRAME_MS: u64 = 16;
const LED_DISABLED_BRIGHTNESS: f64 = 0.0;
const LED_ENABLED_IDLE_BRIGHTNESS: f64 = 0.62;

//####################################################################

#[derive(Clone, Copy, Debug, Default)]
struct AllowedActions {
    abort: bool,
    launch: bool,
    dump: bool,
    normally_open: bool,
    pilot: bool,
    nitrogen: bool,
    nitrous: bool,
    fill_lines: bool,
}

pub fn setup_gpio_panel(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
    let gpio = state.gpio.clone();
    let allowed = Arc::new(Mutex::new(AllowedActions::default()));

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
    gpio.setup_led_pin(ABORT_PIN_LED)?;
    gpio.setup_led_pin(LAUNCH_PIN_LED)?;
    gpio.setup_led_pin(DUMP_PIN_LED)?;
    gpio.setup_led_pin(NORMALLY_OPEN_LED)?;
    gpio.setup_led_pin(RETRACT_PIN_LED)?;
    gpio.setup_led_pin(PILOT_VALVE_LED)?;
    gpio.setup_led_pin(NITROGEN_TANK_VALVE_LED)?;
    gpio.setup_led_pin(NITROUS_TANK_VALVE_LED)?;

    setup_callbacks(&state, allowed.clone())?;

    tokio::spawn(gpio_led_task(state, allowed));

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
        state.clone(),
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        LAUNCH_PIN,
        |a| a.launch,
        TelemetryCommand::Launch,
        debounce,
    )?;
    setup_button_callback(
        state.clone(),
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        DUMP_PIN,
        |a| a.dump,
        TelemetryCommand::Dump,
        debounce,
    )?;
    setup_button_callback(
        state.clone(),
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        NORMALLY_OPEN_PIN,
        |a| a.normally_open,
        TelemetryCommand::NormallyOpen,
        debounce,
    )?;
    setup_button_callback(
        state.clone(),
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        PILOT_VALVE_PIN,
        |a| a.pilot,
        TelemetryCommand::Pilot,
        debounce,
    )?;
    setup_button_callback(
        state.clone(),
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        NITROGEN_TANK_VALVE_PIN,
        |a| a.nitrogen,
        TelemetryCommand::Nitrogen,
        debounce,
    )?;
    setup_button_callback(
        state.clone(),
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        NITROUS_TANK_VALVE_PIN,
        |a| a.nitrous,
        TelemetryCommand::Nitrous,
        debounce,
    )?;
    setup_button_callback(
        state.clone(),
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

#[allow(clippy::too_many_arguments)]
fn setup_button_callback<F>(
    state: Arc<AppState>,
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
    static LAST_WARN_MS_BY_CMD: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    static WARN_INTERVAL_MS: AtomicU64 = AtomicU64::new(3_000);

    gpio.setup_callback_input_pin(pin, Trigger::RisingEdge, debounce, move |is_high| {
        if !is_high {
            return;
        }
        if !can_press(&allowed.lock().unwrap()) {
            let policy = state.action_policy_snapshot();
            if !policy.key_enabled {
                let now_ms = crate::telemetry_task::get_current_timestamp_ms();
                let warn_map = LAST_WARN_MS_BY_CMD.get_or_init(|| Mutex::new(HashMap::new()));
                let mut guard = warn_map.lock().unwrap();
                let cmd_name = format!("{cmd:?}");
                let last = guard.get(&cmd_name).copied().unwrap_or(0);
                if now_ms.saturating_sub(last) >= WARN_INTERVAL_MS.load(Ordering::Relaxed) {
                    guard.insert(cmd_name.clone(), now_ms);
                    drop(guard);
                    emit_warning(
                        &state,
                        format!(
                            "Ignored {cmd_name} button press: safety key is not installed/enabled"
                        ),
                    );
                }
            }
            return;
        }
        if tx.try_send(cmd.clone()).is_err() {
            eprintln!("GPIO button pin {pin}: failed to send command");
        }
    })?;
    Ok(())
}

async fn gpio_led_task(state: Arc<AppState>, allowed: Arc<Mutex<AllowedActions>>) {
    let mut tick = interval(Duration::from_millis(LED_FRAME_MS));
    let mut last_levels: HashMap<u8, u8> = HashMap::new();
    loop {
        tick.tick().await;
        let now_ms = crate::telemetry_task::get_current_timestamp_ms();

        let policy = state.action_policy_snapshot();
        let actions = allowed_from_policy(&policy);

        {
            let mut slot = allowed.lock().unwrap();
            *slot = actions;
        }

        let gpio = &state.gpio;
        set_led(
            gpio,
            &mut last_levels,
            ABORT_PIN_LED,
            led_for(&policy, "Abort", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            LAUNCH_PIN_LED,
            led_for(&policy, "Launch", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            DUMP_PIN_LED,
            led_for(&policy, "Dump", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            NORMALLY_OPEN_LED,
            led_for(&policy, "NormallyOpen", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            PILOT_VALVE_LED,
            led_for(&policy, "Pilot", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            NITROGEN_TANK_VALVE_LED,
            led_for(&policy, "Nitrogen", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            NITROUS_TANK_VALVE_LED,
            led_for(&policy, "Nitrous", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            RETRACT_PIN_LED,
            led_for(&policy, "RetractPlumbing", now_ms),
        );
    }
}

fn allowed_from_policy(policy: &ActionPolicyMsg) -> AllowedActions {
    let enabled = |cmd: &str| {
        policy
            .controls
            .iter()
            .find(|c| c.cmd == cmd)
            .map(|c| c.enabled)
            .unwrap_or(false)
    };

    AllowedActions {
        abort: enabled("Abort"),
        launch: enabled("Launch"),
        dump: enabled("Dump"),
        normally_open: enabled("NormallyOpen"),
        pilot: enabled("Pilot"),
        nitrogen: enabled("Nitrogen"),
        nitrous: enabled("Nitrous"),
        fill_lines: enabled("RetractPlumbing"),
    }
}

fn led_for(policy: &ActionPolicyMsg, cmd: &str, now_ms: u64) -> f64 {
    let Some(control) = policy.controls.iter().find(|c| c.cmd == cmd) else {
        return LED_DISABLED_BRIGHTNESS;
    };
    if !control.enabled {
        return LED_DISABLED_BRIGHTNESS;
    }
    let recommended = !matches!(control.blink, BlinkMode::None);
    if recommended {
        blink_brightness(control.blink.clone(), control.actuated, now_ms)
    } else if control.actuated.unwrap_or(false) {
        1.0
    } else {
        LED_ENABLED_IDLE_BRIGHTNESS
    }
}

fn blink_brightness(blink: BlinkMode, actuated: Option<bool>, now_ms: u64) -> f64 {
    let (period_ms, dim, bright, invert) = match (blink, actuated.unwrap_or(false)) {
        (BlinkMode::None, _) => return 1.0,
        (BlinkMode::Slow, false) => (1_800_u64, 0.2, 1.0, false),
        (BlinkMode::Slow, true) => (1_800_u64, 0.25, 1.0, true),
        (BlinkMode::Fast, false) => (600_u64, 0.15, 1.0, false),
        (BlinkMode::Fast, true) => (600_u64, 0.2, 1.0, true),
    };
    let phase = (now_ms % period_ms) as f64 / period_ms as f64;
    let wave = 0.5 - 0.5 * (std::f64::consts::TAU * phase).cos();
    let pulse = if invert { 1.0 - wave } else { wave };
    dim + (bright - dim) * pulse
}

fn set_led(
    gpio: &crate::gpio::GpioPins,
    last_levels: &mut HashMap<u8, u8>,
    pin: u8,
    brightness: f64,
) {
    let quantized = (brightness.clamp(0.0, 1.0) * 255.0).round() as u8;
    if last_levels.get(&pin).copied() == Some(quantized) {
        return;
    }
    last_levels.insert(pin, quantized);
    if let Err(e) = gpio.write_led_brightness(pin, f64::from(quantized) / 255.0) {
        eprintln!("GPIO LED pin {pin} PWM write failed: {e}");
    }
}
