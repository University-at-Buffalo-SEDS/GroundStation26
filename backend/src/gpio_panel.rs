use crate::gpio::Trigger;
use crate::sequences::{ActionPolicyMsg, BlinkMode};
use crate::state::AppState;
use crate::web::{emit_error, emit_warning};
use groundstation_shared::TelemetryCommand;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
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

pub const LAUNCH_PIN: u8 = 3;
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
pub const NITROUS_TANK_VALVE_LED: u8 = 14;

pub const NORMALLY_OPEN_PIN: u8 = 26;
pub const NORMALLY_OPEN_LED: u8 = 15; // was 7 (spi0 CS1 used)

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
    gpio.setup_output_pin(ABORT_PIN_LED)?;
    gpio.setup_output_pin(LAUNCH_PIN_LED)?;
    gpio.setup_output_pin(DUMP_PIN_LED)?;
    gpio.setup_output_pin(NORMALLY_OPEN_LED)?;
    gpio.setup_output_pin(RETRACT_PIN_LED)?;
    gpio.setup_output_pin(PILOT_VALVE_LED)?;
    gpio.setup_output_pin(NITROGEN_TANK_VALVE_LED)?;
    gpio.setup_output_pin(NITROUS_TANK_VALVE_LED)?;

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
    let mut tick = interval(Duration::from_millis(200));
    let mut tick_count: u64 = 0;
    loop {
        tick.tick().await;
        tick_count = tick_count.wrapping_add(1);

        let policy = state.action_policy_snapshot();
        let actions = allowed_from_policy(&policy);

        {
            let mut slot = allowed.lock().unwrap();
            *slot = actions;
        }

        let gpio = &state.gpio;
        set_led(gpio, ABORT_PIN_LED, led_for(&policy, "Abort", tick_count));
        set_led(gpio, LAUNCH_PIN_LED, led_for(&policy, "Launch", tick_count));
        set_led(gpio, DUMP_PIN_LED, led_for(&policy, "Dump", tick_count));
        set_led(
            gpio,
            NORMALLY_OPEN_LED,
            led_for(&policy, "NormallyOpen", tick_count),
        );
        set_led(gpio, PILOT_VALVE_LED, led_for(&policy, "Pilot", tick_count));
        set_led(
            gpio,
            NITROGEN_TANK_VALVE_LED,
            led_for(&policy, "Nitrogen", tick_count),
        );
        set_led(
            gpio,
            NITROUS_TANK_VALVE_LED,
            led_for(&policy, "Nitrous", tick_count),
        );
        set_led(
            gpio,
            RETRACT_PIN_LED,
            led_for(&policy, "RetractPlumbing", tick_count),
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

fn led_for(policy: &ActionPolicyMsg, cmd: &str, tick_count: u64) -> bool {
    let Some(control) = policy.controls.iter().find(|c| c.cmd == cmd) else {
        return false;
    };
    if !control.enabled {
        return false;
    }
    blink_to_level(
        control.blink.clone(),
        control.actuated.unwrap_or(false),
        tick_count,
    )
}

fn blink_to_level(blink: BlinkMode, actuated: bool, tick_count: u64) -> bool {
    match blink {
        BlinkMode::None => true,
        BlinkMode::Slow => {
            let phase = tick_count % 10;
            if actuated { phase < 8 } else { phase < 2 }
        }
        BlinkMode::Fast => {
            let phase = tick_count % 4;
            if actuated { phase < 3 } else { phase < 2 }
        }
    }
}

fn set_led(gpio: &crate::gpio::GpioPins, pin: u8, on: bool) {
    if let Err(e) = gpio.write_output_pin(pin, on) {
        eprintln!("GPIO LED pin {pin} write failed: {e}");
    }
}
