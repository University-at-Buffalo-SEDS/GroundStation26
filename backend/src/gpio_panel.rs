use crate::gpio::Trigger;
use crate::sequences::{ActionPolicyMsg, BlinkMode};
use crate::state::AppState;
use crate::telemetry_task::queue_abort_packet;
use crate::types::TelemetryCommand;
use crate::web::{emit_error, emit_notification_warning};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

//####################################################################
// The values assigned here are GPIO pin numbers on the Raspberry Pi
//####################################################################
pub const IGNITER_PIN: u8 = 5;
pub const IGNITER_PIN_LED: u8 = 0;

pub const LAUNCH_ARM_PIN: u8 = 8;
#[allow(dead_code)]
pub const ALL_BUTTONS_ENABLE_PIN: u8 = 9;

pub const ABORT_PIN: u8 = 7;
pub const ABORT_PIN_LED: u8 = 1;

pub const LAUNCH_PIN: u8 = 11;
pub const LAUNCH_PIN_LED: u8 = 10;

pub const DUMP_PIN: u8 = 12;
pub const DUMP_PIN_LED: u8 = 16;

pub const RETRACT_PIN: u8 = 22;
pub const RETRACT_PIN_LED: u8 = 27;

pub const PILOT_VALVE_PIN: u8 = 13;
pub const PILOT_VALVE_LED: u8 = 6;

pub const NITROGEN_TANK_VALVE_PIN: u8 = 23;
pub const NITROGEN_TANK_VALVE_LED: u8 = 18;

pub const NITROUS_TANK_VALVE_PIN: u8 = 17;
pub const NITROUS_TANK_VALVE_LED: u8 = 4;

pub const NORMALLY_OPEN_PIN: u8 = 20;
pub const NORMALLY_OPEN_LED: u8 = 21;

pub const WARNING_ACK_PIN: u8 = 26;
pub const MASTER_ALARM_LED: u8 = 19;
pub const MASTER_ALARM_BUZZER: u8 = 24;

const LED_FRAME_MS: u64 = 16;
const LED_DISABLED_BRIGHTNESS: f64 = 0.0;

//####################################################################

#[derive(Clone, Copy, Debug, Default)]
struct AllowedActions {
    launch: bool,
    dump: bool,
    igniter: bool,
    normally_open: bool,
    pilot: bool,
    nitrogen: bool,
    nitrous: bool,
    fill_lines: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AlarmSeverity {
    Warning,
    Error,
}

pub fn setup_gpio_panel(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
    let gpio = state.gpio.clone();
    let allowed = Arc::new(Mutex::new(AllowedActions::default()));

    // Inputs (buttons)
    gpio.setup_input_pulldown_pin(ABORT_PIN)?;
    eprintln!("GPIO abort button configured on BCM GPIO {ABORT_PIN} as active-high pulldown input");
    gpio.setup_input_pin(LAUNCH_PIN)?;
    gpio.setup_input_pin(IGNITER_PIN)?;
    gpio.setup_input_pin(LAUNCH_ARM_PIN)?;
    gpio.setup_input_pin(ALL_BUTTONS_ENABLE_PIN)?;
    gpio.setup_input_pin(DUMP_PIN)?;
    gpio.setup_input_pin(NORMALLY_OPEN_PIN)?;
    gpio.setup_input_pin(PILOT_VALVE_PIN)?;
    gpio.setup_input_pin(NITROGEN_TANK_VALVE_PIN)?;
    gpio.setup_input_pin(NITROUS_TANK_VALVE_PIN)?;
    gpio.setup_input_pin(RETRACT_PIN)?;
    gpio.setup_input_pin(WARNING_ACK_PIN)?;

    // Outputs (LEDs only)
    gpio.setup_led_pin(IGNITER_PIN_LED)?;
    gpio.setup_led_pin(ABORT_PIN_LED)?;
    gpio.setup_led_pin(LAUNCH_PIN_LED)?;
    gpio.setup_led_pin(DUMP_PIN_LED)?;
    gpio.setup_led_pin(NORMALLY_OPEN_LED)?;
    gpio.setup_led_pin(RETRACT_PIN_LED)?;
    gpio.setup_led_pin(PILOT_VALVE_LED)?;
    gpio.setup_led_pin(NITROGEN_TANK_VALVE_LED)?;
    gpio.setup_led_pin(NITROUS_TANK_VALVE_LED)?;
    gpio.setup_led_pin(MASTER_ALARM_LED)?;
    gpio.setup_output_pin(MASTER_ALARM_BUZZER)?;

    setup_callbacks(&state, allowed.clone())?;

    spawn_gpio_led_thread(state, allowed);

    Ok(())
}

fn setup_callbacks(
    state: &Arc<AppState>,
    allowed: Arc<Mutex<AllowedActions>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let tx = state.cmd_tx.clone();
    let gpio = state.gpio.clone();
    let debounce = Duration::from_millis(50);

    let tx_abort = tx.clone();
    let state_abort = state.clone();
    gpio.setup_callback_input_pin(ABORT_PIN, Trigger::RisingEdge, debounce, move |is_high| {
        eprintln!("GPIO abort button interrupt on BCM GPIO {ABORT_PIN}: is_high={is_high}");
        if !is_high {
            eprintln!("GPIO abort button interrupt ignored because active-high input is low");
            return;
        }

        eprintln!("GPIO abort button pressed: latching abort indicator and dispatching abort");
        state_abort.set_abort_indicator_latched(true);
        crate::sequences::refresh_action_policy_now(&state_abort);
        state_abort.broadcast_action_policy_snapshot();

        if let Some(router) = state_abort.topology_router.get() {
            if let Err(err) = queue_abort_packet(router, "Manual GPIO Abort Button Pressed") {
                eprintln!("GPIO abort button: failed to queue abort packet: {err}");
            } else if let Err(err) = router.process_all_queues_with_timeout(3) {
                eprintln!("GPIO abort button: failed to flush abort packet: {err}");
            } else {
                eprintln!("GPIO abort button: abort packet queued and flushed");
            }
        } else {
            eprintln!("GPIO abort button: router unavailable, falling back to command queue");
        }

        match tx_abort.try_send(TelemetryCommand::Abort) {
            Ok(()) => eprintln!("GPIO abort button: Abort command queued to telemetry task"),
            Err(err) => eprintln!("GPIO abort button: failed to send command: {err}"),
        }
        emit_error(&state_abort, "Manual abort button pressed!".to_string());
    })?;

    let allowed_launch = allowed.clone();
    let tx_launch = tx.clone();
    let state_launch = state.clone();
    let gpio_launch = gpio.clone();
    gpio.setup_callback_input_pin(LAUNCH_PIN, Trigger::RisingEdge, debounce, move |is_high| {
        if !is_high {
            return;
        }
        if !allowed_launch.lock().unwrap().launch {
            return;
        }
        #[cfg(feature = "hitl_mode")]
        if state_launch.hitl_button_interlock_enabled()
            && !is_input_enabled(&gpio_launch, ALL_BUTTONS_ENABLE_PIN)
        {
            emit_notification_warning(
                &state_launch,
                "Ignored launch button press: button interlock is enabled".to_string(),
            );
            return;
        }
        #[cfg(feature = "hitl_mode")]
        let launch_interlock_ok = if state_launch.hitl_launch_interlock_enabled() {
            is_input_enabled(&gpio_launch, LAUNCH_ARM_PIN)
        } else {
            true
        };
        #[cfg(not(feature = "hitl_mode"))]
        let launch_interlock_ok = is_input_enabled(&gpio_launch, LAUNCH_ARM_PIN);
        if !launch_interlock_ok {
            emit_notification_warning(
                &state_launch,
                "Ignored launch button press: launch arm signal is not enabled".to_string(),
            );
            return;
        }
        #[cfg(feature = "hitl_mode")]
        let launch_command = if state_launch.hitl_physical_launch_uses_ground_station() {
            TelemetryCommand::GroundStationLaunch
        } else {
            TelemetryCommand::Launch
        };
        #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
        let launch_command = TelemetryCommand::GroundStationLaunch;
        #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
        let launch_command = TelemetryCommand::Launch;

        if tx_launch.try_send(launch_command).is_err() {
            eprintln!("GPIO launch button: failed to send command");
        }
    })?;
    setup_button_callback(
        state.clone(),
        gpio.clone(),
        allowed.clone(),
        tx.clone(),
        IGNITER_PIN,
        |a| a.igniter,
        TelemetryCommand::Igniter,
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

    let state_warning_ack = state.clone();
    gpio.setup_callback_input_pin(
        WARNING_ACK_PIN,
        Trigger::RisingEdge,
        debounce,
        move |is_high| {
            if !is_high {
                return;
            }
            let now_ms = crate::telemetry_task::get_current_timestamp_ms() as i64;
            state_warning_ack.acknowledge_alerts_through(now_ms, now_ms);
        },
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
    let _gpio_for_callback = gpio.clone();

    gpio.setup_callback_input_pin(pin, Trigger::RisingEdge, debounce, move |is_high| {
        if !is_high {
            return;
        }
        #[cfg(feature = "hitl_mode")]
        if state.hitl_button_interlock_enabled()
            && !is_input_enabled(&_gpio_for_callback, ALL_BUTTONS_ENABLE_PIN)
        {
            let now_ms = crate::telemetry_task::get_current_timestamp_ms();
            let warn_map = LAST_WARN_MS_BY_CMD.get_or_init(|| Mutex::new(HashMap::new()));
            let mut guard = warn_map.lock().unwrap();
            let cmd_name = format!("{cmd:?}");
            let last = guard.get(&cmd_name).copied().unwrap_or(0);
            if now_ms.saturating_sub(last) >= WARN_INTERVAL_MS.load(Ordering::Relaxed) {
                guard.insert(cmd_name.clone(), now_ms);
                drop(guard);
                emit_notification_warning(
                    &state,
                    format!("Ignored {cmd_name} button press: button interlock is enabled"),
                );
            }
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
                    emit_notification_warning(
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

fn spawn_gpio_led_thread(state: Arc<AppState>, allowed: Arc<Mutex<AllowedActions>>) {
    let mut shutdown_rx = state.shutdown_subscribe();
    thread::spawn(move || {
        gpio_led_task(state, allowed, &mut shutdown_rx);
    });
}

fn gpio_led_task(
    state: Arc<AppState>,
    allowed: Arc<Mutex<AllowedActions>>,
    shutdown_rx: &mut tokio::sync::broadcast::Receiver<()>,
) {
    let mut last_levels: HashMap<u8, u8> = HashMap::new();
    loop {
        match shutdown_rx.try_recv() {
            Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
            | Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        }
        let frame_started = Instant::now();
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
            IGNITER_PIN_LED,
            led_for(&state, &policy, "Igniter", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            ABORT_PIN_LED,
            led_for(&state, &policy, "Abort", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            LAUNCH_PIN_LED,
            led_for(&state, &policy, "Launch", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            DUMP_PIN_LED,
            led_for(&state, &policy, "Dump", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            NORMALLY_OPEN_LED,
            led_for(&state, &policy, "NormallyOpen", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            PILOT_VALVE_LED,
            led_for(&state, &policy, "Pilot", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            NITROGEN_TANK_VALVE_LED,
            led_for(&state, &policy, "Nitrogen", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            NITROUS_TANK_VALVE_LED,
            led_for(&state, &policy, "Nitrous", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            RETRACT_PIN_LED,
            led_for(&state, &policy, "RetractPlumbing", now_ms),
        );
        set_led(
            gpio,
            &mut last_levels,
            MASTER_ALARM_LED,
            master_alarm_led_for(&state, now_ms),
        );
        set_binary_output(
            gpio,
            &mut last_levels,
            MASTER_ALARM_BUZZER,
            master_alarm_buzzer_for(&state, now_ms),
        );

        let frame_budget = Duration::from_millis(LED_FRAME_MS);
        let elapsed = frame_started.elapsed();
        if elapsed < frame_budget {
            thread::sleep(frame_budget - elapsed);
        }
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
        launch: enabled("Launch"),
        dump: enabled("Dump"),
        igniter: enabled("Igniter"),
        normally_open: enabled("NormallyOpen"),
        pilot: enabled("Pilot"),
        nitrogen: enabled("Nitrogen"),
        nitrous: enabled("Nitrous"),
        fill_lines: enabled("RetractPlumbing"),
    }
}

fn led_for(state: &AppState, policy: &ActionPolicyMsg, cmd: &str, now_ms: u64) -> f64 {
    if cmd == "Launch" && state.launch_indicator_latched() {
        return 1.0;
    }
    if cmd == "Abort" && state.abort_indicator_latched() {
        return 1.0;
    }
    let Some(control) = policy.controls.iter().find(|c| c.cmd == cmd) else {
        return LED_DISABLED_BRIGHTNESS;
    };
    #[cfg(feature = "hitl_mode")]
    if cmd == "Launch"
        && control.enabled
        && (state.hitl_button_interlock_enabled() || state.hitl_launch_interlock_enabled())
        && state.hitl_button_interlock_satisfied()
        && state.hitl_launch_interlock_satisfied()
    {
        return 1.0;
    }
    if !control.enabled {
        return LED_DISABLED_BRIGHTNESS;
    }
    let recommended = !matches!(control.blink, BlinkMode::None);
    if recommended {
        blink_brightness(control.blink.clone(), control.actuated, now_ms)
    } else if control.actuated.unwrap_or(false) {
        1.0
    } else {
        LED_DISABLED_BRIGHTNESS
    }
}

fn current_unacked_alarm_severity(state: &AppState) -> Option<AlarmSeverity> {
    let ack = state.alert_ack_state_snapshot();
    let mut has_warning = false;
    let mut has_error = false;

    for alert in state.recent_alerts_snapshot() {
        match alert.severity.as_str() {
            "warning" if alert.timestamp_ms > ack.warning_ack_timestamp_ms => {
                has_warning = true;
            }
            "error" if alert.timestamp_ms > ack.error_ack_timestamp_ms => {
                has_error = true;
            }
            _ => {}
        }
    }

    if has_error {
        Some(AlarmSeverity::Error)
    } else if has_warning {
        Some(AlarmSeverity::Warning)
    } else {
        None
    }
}

fn master_alarm_led_for(state: &AppState, now_ms: u64) -> f64 {
    let Some(severity) = current_unacked_alarm_severity(state) else {
        return 0.0;
    };
    alarm_led_brightness(severity, now_ms)
}

fn master_alarm_buzzer_for(state: &AppState, now_ms: u64) -> bool {
    let Some(severity) = current_unacked_alarm_severity(state) else {
        return false;
    };
    alarm_active_window(severity, now_ms)
}

fn alarm_active_window(severity: AlarmSeverity, now_ms: u64) -> bool {
    let (cycle_ms, on_ms) = match severity {
        AlarmSeverity::Error => (8_000_u64, 5_000_u64),
        AlarmSeverity::Warning => (3_500_u64, 500_u64),
    };
    now_ms % cycle_ms < on_ms
}

fn alarm_led_brightness(severity: AlarmSeverity, now_ms: u64) -> f64 {
    match severity {
        AlarmSeverity::Warning => {
            let period_ms = 3_000_u64;
            let phase = (now_ms % period_ms) as f64 / period_ms as f64;
            0.5 - 0.5 * (std::f64::consts::TAU * phase).cos()
        }
        AlarmSeverity::Error => {
            let cycle_ms = 900_u64;
            let phase_ms = now_ms % cycle_ms;
            for offset_ms in [0_u64, 180, 360] {
                if let Some(pulse) = error_blink_pulse(phase_ms, offset_ms) {
                    return pulse;
                }
            }
            0.0
        }
    }
}

fn error_blink_pulse(phase_ms: u64, offset_ms: u64) -> Option<f64> {
    let rise_ms = 35_u64;
    let hold_ms = 100_u64;
    let fall_ms = 35_u64;
    let pulse_ms = rise_ms + hold_ms + fall_ms;
    if phase_ms < offset_ms || phase_ms >= offset_ms + pulse_ms {
        return None;
    }
    let local_ms = phase_ms - offset_ms;
    if local_ms < rise_ms {
        Some(local_ms as f64 / rise_ms as f64)
    } else if local_ms < rise_ms + hold_ms {
        Some(1.0)
    } else {
        let fade_ms = local_ms - rise_ms - hold_ms;
        Some(1.0 - (fade_ms as f64 / fall_ms as f64))
    }
}

fn is_input_enabled(gpio: &crate::gpio::GpioPins, pin: u8) -> bool {
    #[cfg(feature = "raspberry_pi")]
    {
        return gpio.read_input_pin(pin).unwrap_or(false);
    }

    #[cfg(not(feature = "raspberry_pi"))]
    {
        let _ = (gpio, pin);
        true
    }
}

fn blink_brightness(blink: BlinkMode, actuated: Option<bool>, now_ms: u64) -> f64 {
    let (period_ms, dim, bright, invert) = match (blink, actuated.unwrap_or(false)) {
        (BlinkMode::None, _) => return 1.0,
        (BlinkMode::Slow, false) => (1_800_u64, 0.16, 0.82, false),
        (BlinkMode::Slow, true) => (1_800_u64, 0.2, 0.82, true),
        (BlinkMode::Fast, false) => (600_u64, 0.12, 0.82, false),
        (BlinkMode::Fast, true) => (600_u64, 0.16, 0.82, true),
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

fn set_binary_output(
    gpio: &crate::gpio::GpioPins,
    last_levels: &mut HashMap<u8, u8>,
    pin: u8,
    enabled: bool,
) {
    let next = if enabled { 255 } else { 0 };
    if last_levels.get(&pin).copied() == Some(next) {
        return;
    }
    last_levels.insert(pin, next);
    if let Err(e) = gpio.write_output_pin(pin, enabled) {
        eprintln!("GPIO output pin {pin} write failed: {e}");
    }
}
