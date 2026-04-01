// frontend/src/telemetry_dashboard/actions_tab.rs

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::auth;

use super::layout::{ActionsTabLayout, ThemeConfig};
use super::{ActionPolicyMsg, BlinkMode};

#[cfg(target_arch = "wasm32")]
fn blink_epoch_ms() -> u64 {
    js_sys::Date::now().max(0.0) as u64
}

#[cfg(not(target_arch = "wasm32"))]
fn blink_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn blink_opacity(blink_now_ms: u64, blink: BlinkMode, actuated: Option<bool>) -> Option<f32> {
    let (period_ms, dim, bright, invert) = match (blink, actuated.unwrap_or(false)) {
        (BlinkMode::None, _) => return None,
        (BlinkMode::Slow, false) => (1_800, 0.2, 1.0, false),
        (BlinkMode::Slow, true) => (1_800, 0.25, 1.0, true),
        (BlinkMode::Fast, false) => (600, 0.15, 1.0, false),
        (BlinkMode::Fast, true) => (600, 0.2, 1.0, true),
    };
    let phase = (blink_now_ms % period_ms) as f32 / period_ms as f32;
    let wave = 0.5 - 0.5 * f32::cos(std::f32::consts::TAU * phase);
    let pulse = if invert { 1.0 - wave } else { wave };
    Some(dim + (bright - dim) * pulse)
}

#[cfg(not(target_arch = "wasm32"))]
fn target_frame_duration() -> std::time::Duration {
    let fps: u64 = std::env::var("GS_UI_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    let fps = fps.clamp(1, 480);
    std::time::Duration::from_micros(1_000_000 / fps)
}

#[cfg(target_arch = "wasm32")]
fn target_frame_duration() -> std::time::Duration {
    std::time::Duration::from_millis(16)
}

#[cfg(target_arch = "wasm32")]
async fn sleep_for_frame(duration: std::time::Duration) {
    let millis = duration.as_millis().clamp(0, u32::MAX as u128) as u32;
    gloo_timers::future::sleep(std::time::Duration::from_millis(millis as u64)).await;
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_for_frame(duration: std::time::Duration) {
    tokio::time::sleep(duration).await;
}

fn action_opacity(
    blink_now_ms: u64,
    enabled: bool,
    recommended: bool,
    blink: BlinkMode,
    actuated: Option<bool>,
) -> f32 {
    if !enabled {
        0.45
    } else if recommended {
        blink_opacity(blink_now_ms, blink, actuated).unwrap_or(1.0)
    } else {
        0.62
    }
}

fn btn_style(
    border: &str,
    bg: &str,
    fg: &str,
    blink_now_ms: u64,
    enabled: bool,
    blink: BlinkMode,
    actuated: Option<bool>,
) -> String {
    let cursor = if enabled { "pointer" } else { "not-allowed" };
    let recommended = enabled && blink != BlinkMode::None;
    let opacity = action_opacity(blink_now_ms, enabled, recommended, blink, actuated);
    let filter = if !enabled {
        "grayscale(0.25) brightness(0.9)"
    } else if recommended {
        "none"
    } else {
        "saturate(0.58) brightness(0.82)"
    };
    let box_shadow = if recommended {
        "0 10px 25px rgba(0,0,0,0.25)"
    } else {
        "0 4px 12px rgba(0,0,0,0.16)"
    };
    format!(
        "padding:0.65rem 1rem; border-radius:0.75rem; cursor:{cursor}; opacity:{opacity}; filter:{filter}; width:100%; \
         text-align:left; border:1px solid {border}; background:{bg}; color:{fg}; \
         font-weight:800; box-shadow:{box_shadow};"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct KalmanFilterConstants {
    process_position_variance: f32,
    process_velocity_variance: f32,
    accel_variance: f32,
    baro_altitude_variance: f32,
    gps_altitude_variance: f32,
    gps_velocity_variance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FlightProfileConfig {
    id: String,
    label: String,
    wind_level: u8,
    kalman: KalmanFilterConstants,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FlightSetupConfig {
    version: u32,
    selected_profile_id: String,
    profiles: Vec<FlightProfileConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlightSetupApplyResponse {
    selected_profile_id: String,
    wind_level: u8,
    payload_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
struct EmptyApplyReq {}

fn selected_profile(cfg: &FlightSetupConfig) -> Option<&FlightProfileConfig> {
    cfg.profiles
        .iter()
        .find(|profile| profile.id == cfg.selected_profile_id)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FluidFillTarget {
    target_mass_kg: f32,
    target_pressure_psi: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FillTargetsConfig {
    version: u32,
    nitrogen: FluidFillTarget,
    nitrous: FluidFillTarget,
}

fn setup_panel_style(theme: &ThemeConfig) -> String {
    format!(
        "padding:14px; border-radius:14px; border:1px solid {}; background:{}; display:flex; flex-direction:column; gap:12px;",
        theme.border, theme.panel_background
    )
}

fn input_style(theme: &ThemeConfig) -> String {
    format!(
        "width:100%; padding:10px 12px; border-radius:10px; border:1px solid {}; background:{}; color:{};",
        theme.border, theme.panel_background_alt, theme.text_primary
    )
}

fn apply_button_style(theme: &ThemeConfig, enabled: bool) -> String {
    let opacity = if enabled { "1.0" } else { "0.55" };
    let cursor = if enabled { "pointer" } else { "not-allowed" };
    format!(
        "padding:10px 14px; border-radius:10px; border:1px solid {}; background:{}; color:{}; font-weight:700; cursor:{}; opacity:{};",
        theme.button_border, theme.button_background, theme.button_text, cursor, opacity
    )
}

#[component]
pub fn ActionsTab(
    layout: ActionsTabLayout,
    action_policy: Signal<ActionPolicyMsg>,
    abort_only_mode: bool,
    theme: ThemeConfig,
) -> Element {
    let mut redraw_tick = use_signal(|| 0u64);
    let mut flight_setup = use_signal(|| None::<FlightSetupConfig>);
    let mut flight_setup_status = use_signal(String::new);
    let mut flight_setup_busy = use_signal(|| false);
    let mut fill_targets = use_signal(|| None::<FillTargetsConfig>);
    let mut fill_targets_status = use_signal(String::new);
    let mut fill_targets_busy = use_signal(|| false);
    use_effect(move || {
        spawn(async move {
            loop {
                sleep_for_frame(target_frame_duration()).await;
                let next = redraw_tick.read().wrapping_add(1);
                redraw_tick.set(next);
            }
        });
    });
    use_effect(move || {
        spawn(async move {
            match crate::telemetry_dashboard::http_get_json::<FlightSetupConfig>(
                "/api/flight_setup",
            )
            .await
            {
                Ok(cfg) => flight_setup.set(Some(cfg)),
                Err(err) => flight_setup_status.set(format!("Flight setup load failed: {err}")),
            }
        });
    });
    use_effect(move || {
        spawn(async move {
            match crate::telemetry_dashboard::http_get_json::<FillTargetsConfig>(
                "/api/fill_targets",
            )
            .await
            {
                Ok(cfg) => fill_targets.set(Some(cfg)),
                Err(err) => fill_targets_status.set(format!("Fill targets load failed: {err}")),
            }
        });
    });
    let _blink_tick = *redraw_tick.read();
    let blink_now_ms = blink_epoch_ms();
    let visible_actions = layout
        .actions
        .iter()
        .filter(|action| auth::can_send_command(action.cmd.as_str()))
        .collect::<Vec<_>>();
    let software_buttons_enabled = action_policy.read().software_buttons_enabled;
    let fill_targets_editable = if layout.fill_targets_require_actions_enabled {
        software_buttons_enabled && !abort_only_mode
    } else {
        true
    };
    rsx! {
        div {
            style: "
                padding:16px;
                display:flex;
                flex-direction:column;
                gap:12px;
            ",
            h2 { style: "margin:0 0 8px 0; color:{theme.text_primary};", "Actions" }
            p  { style: "margin:0 0 12px 0; color:{theme.text_soft}; font-size:0.9rem;",
                "All available actions are available all the time, use with caution as improper use \
                can and will damage the system."
            }
            if abort_only_mode {
                div {
                    style: "margin:0; padding:6px 10px; border-radius:8px; border:1px solid {theme.error_border}; background:{theme.error_background}; color:{theme.error_text}; font-size:11px; line-height:1.25;",
                    "Disable Actions is enabled. All action and flight-state buttons except Abort are disabled."
                }
            }
            if visible_actions.is_empty() {
                div {
                    style: "padding:12px; border:1px solid {theme.border}; border-radius:12px; background:{theme.panel_background}; color:{theme.text_muted}; font-size:13px;",
                    "No actions are available for this user."
                }
            } else {
                div {
                    style: "
                        display:grid;
                        grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
                        gap:12px;
                    ",

                    for action in visible_actions.iter() {
                        {
                            let software_buttons_enabled = action_policy.read().software_buttons_enabled;
                            let control = action_policy
                                .read()
                                .controls
                                .iter()
                                .find(|c| c.cmd == action.cmd)
                                .cloned();
                            let enabled = software_buttons_enabled
                                && auth::can_send_command(action.cmd.as_str())
                                && (!abort_only_mode || action.cmd == "Abort")
                                && control
                                    .as_ref()
                                    .map(|c| c.enabled)
                                    .unwrap_or(action.cmd == "Abort");
                            let blink = control.as_ref().map(|c| c.blink).unwrap_or(BlinkMode::None);
                            let actuated = control.as_ref().and_then(|c| c.actuated);
                            rsx! {
                                button {
                                    style: "{btn_style(&action.border, &action.bg, &action.fg, blink_now_ms, enabled, blink, actuated)}",
                                    disabled: !enabled,
                                    onclick: {
                                        let cmd = action.cmd.clone();
                                        move |_| {
                                            if enabled {
                                                crate::telemetry_dashboard::send_cmd(&cmd)
                                            }
                                        }
                                    },
                                    "{action.label}"
                                }
                            }
                        }
                    }
                }
            }
            div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(min(100%, 320px), 1fr)); gap:12px; align-items:start;",
                if layout.show_flight_setup {
                    div { style: "{setup_panel_style(&theme)}",
                    h3 { style: "margin:0; color:{theme.text_primary};", "Flight Setup" }
                    if let Some(cfg) = flight_setup.read().clone() {
                        div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(min(100%, 280px), 1fr)); gap:12px; align-items:start; min-width:0;",
                            div { style: "display:flex; flex-direction:column; gap:8px; min-width:0;",
                                label { style: "font-size:12px; color:{theme.text_muted}; text-transform:uppercase; letter-spacing:0.08em;", "Flight profile" }
                                select {
                                    style: "{input_style(&theme)} max-width:100%; min-width:0;",
                                    value: "{cfg.selected_profile_id}",
                                    onchange: {
                                        move |evt| {
                                            let next_id = evt.value();
                                            let Some(mut next_cfg) = flight_setup.read().clone() else {
                                                return;
                                            };
                                            next_cfg.selected_profile_id = next_id.clone();
                                            flight_setup.set(Some(next_cfg.clone()));
                                            flight_setup_status.set("Saving flight setup…".to_string());
                                            spawn(async move {
                                                match crate::telemetry_dashboard::http_post_json::<FlightSetupConfig, FlightSetupConfig>(
                                                    "/api/flight_setup",
                                                    &next_cfg,
                                                ).await {
                                                    Ok(saved) => {
                                                        flight_setup.set(Some(saved));
                                                        flight_setup_status.set(format!("Selected profile {next_id}."));
                                                    }
                                                    Err(err) => {
                                                        flight_setup_status.set(format!("Flight setup save failed: {err}"));
                                                    }
                                                }
                                            });
                                        }
                                    },
                                    for profile in cfg.profiles.iter() {
                                        option {
                                            value: "{profile.id}",
                                            "{profile.label} (wind {profile.wind_level})"
                                        }
                                    }
                                }
                                button {
                                    style: "{apply_button_style(&theme, !*flight_setup_busy.read() && !abort_only_mode)}",
                                    disabled: *flight_setup_busy.read() || abort_only_mode,
                                    onclick: move |_| {
                                        if *flight_setup_busy.read() || abort_only_mode {
                                            return;
                                        }
                                        flight_setup_busy.set(true);
                                        flight_setup_status.set("Applying flight setup…".to_string());
                                        spawn(async move {
                                            let body = EmptyApplyReq {};
                                            match crate::telemetry_dashboard::http_post_json::<EmptyApplyReq, FlightSetupApplyResponse>(
                                                "/api/flight_setup/apply",
                                                &body,
                                            ).await {
                                                Ok(resp) => {
                                                    flight_setup_status.set(format!(
                                                        "Applied wind {} profile ({} bytes queued).",
                                                        resp.wind_level, resp.payload_bytes
                                                    ));
                                                }
                                                Err(err) => {
                                                    flight_setup_status.set(format!("Flight setup apply failed: {err}"));
                                                }
                                            }
                                            flight_setup_busy.set(false);
                                        });
                                    },
                                    "Apply To Flight Computer"
                                }
                                if !flight_setup_status.read().is_empty() {
                                    div { style: "font-size:12px; color:{theme.text_muted};", "{flight_setup_status.read().clone()}" }
                                }
                            }
                            div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(min(100%, 180px), 1fr)); gap:10px; min-width:0;",
                                if let Some(profile) = selected_profile(&cfg) {
                                    {flight_setup_metric("Profile", profile.label.clone(), &theme)}
                                    {flight_setup_metric("Wind", format!("{}", profile.wind_level), &theme)}
                                    {flight_setup_metric("Q Position", format!("{:.3}", profile.kalman.process_position_variance), &theme)}
                                    {flight_setup_metric("Q Velocity", format!("{:.3}", profile.kalman.process_velocity_variance), &theme)}
                                    {flight_setup_metric("Accel R", format!("{:.3}", profile.kalman.accel_variance), &theme)}
                                    {flight_setup_metric("Baro R", format!("{:.3}", profile.kalman.baro_altitude_variance), &theme)}
                                    {flight_setup_metric("GPS Alt R", format!("{:.3}", profile.kalman.gps_altitude_variance), &theme)}
                                    {flight_setup_metric("GPS Vel R", format!("{:.3}", profile.kalman.gps_velocity_variance), &theme)}
                                }
                            }
                        }
                    } else {
                        div { style: "font-size:13px; color:{theme.text_muted};", "Loading flight setup…" }
                    }
                }
                }
                if layout.show_fill_targets {
                    div { style: "{setup_panel_style(&theme)}",
                    h3 { style: "margin:0; color:{theme.text_primary};", "Fill Targets" }
                    if let Some(cfg) = fill_targets.read().clone() {
                        div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(min(100%, 220px), 1fr)); gap:10px;",
                            {fill_target_editor("Nitrogen", "nitrogen", &cfg.nitrogen, &theme, fill_targets, fill_targets_status, fill_targets_editable)}
                            {fill_target_editor("Nitrous", "nitrous", &cfg.nitrous, &theme, fill_targets, fill_targets_status, fill_targets_editable)}
                        }
                        button {
                            style: "{apply_button_style(&theme, !*fill_targets_busy.read() && fill_targets_editable)}",
                            disabled: *fill_targets_busy.read() || !fill_targets_editable,
                            onclick: move |_| {
                                if *fill_targets_busy.read() || !fill_targets_editable {
                                    return;
                                }
                                let Some(next_cfg) = fill_targets.read().clone() else {
                                    return;
                                };
                                fill_targets_busy.set(true);
                                fill_targets_status.set("Saving fill targets…".to_string());
                                spawn(async move {
                                    match crate::telemetry_dashboard::http_post_json::<FillTargetsConfig, FillTargetsConfig>(
                                        "/api/fill_targets",
                                        &next_cfg,
                                    ).await {
                                        Ok(saved) => {
                                            fill_targets.set(Some(saved));
                                            fill_targets_status.set("Fill targets saved.".to_string());
                                        }
                                        Err(err) => {
                                            fill_targets_status.set(format!("Fill targets save failed: {err}"));
                                        }
                                    }
                                    fill_targets_busy.set(false);
                                });
                            },
                            "Save Fill Targets"
                        }
                        if !fill_targets_editable {
                            div { style: "font-size:12px; color:{theme.text_muted};", "Enable actions to edit fill targets." }
                        }
                        if !fill_targets_status.read().is_empty() {
                            div { style: "font-size:12px; color:{theme.text_muted};", "{fill_targets_status.read().clone()}" }
                        }
                    } else {
                        div { style: "font-size:13px; color:{theme.text_muted};", "Loading fill targets…" }
                    }
                }
                }
            }
        }
    }
}

fn flight_setup_metric(label: &str, value: String, theme: &ThemeConfig) -> Element {
    rsx! {
        div { style: "padding:10px 12px; border-radius:10px; border:1px solid {theme.border}; background:{theme.panel_background_alt};",
            div { style: "font-size:11px; color:{theme.text_muted}; text-transform:uppercase; letter-spacing:0.08em; margin-bottom:4px;", "{label}" }
            div { style: "font-size:14px; color:{theme.text_primary}; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace;", "{value}" }
        }
    }
}

fn fill_target_editor(
    title: &'static str,
    field: &'static str,
    target: &FluidFillTarget,
    theme: &ThemeConfig,
    mut fill_targets: Signal<Option<FillTargetsConfig>>,
    mut fill_targets_status: Signal<String>,
    enabled: bool,
) -> Element {
    let mass_value = format!("{:.2}", target.target_mass_kg);
    let pressure_value = format!("{:.1}", target.target_pressure_psi);
    let cursor = if enabled { "text" } else { "not-allowed" };
    let opacity = if enabled { "1.0" } else { "0.6" };
    rsx! {
        div { style: "padding:12px; border-radius:12px; border:1px solid {theme.border}; background:{theme.panel_background_alt}; display:flex; flex-direction:column; gap:10px; opacity:{opacity};",
            div { style: "font-size:14px; font-weight:700; color:{theme.text_primary};", "{title}" }
            div { style: "display:flex; flex-direction:column; gap:6px;",
                label { style: "font-size:12px; color:{theme.text_muted}; text-transform:uppercase; letter-spacing:0.08em;", "Target mass (kg)" }
                input {
                    r#type: "number",
                    step: "0.01",
                    min: "0",
                    disabled: !enabled,
                    style: "{input_style(theme)} cursor:{cursor};",
                    value: "{mass_value}",
                    oninput: move |evt| {
                        if !enabled {
                            return;
                        }
                        let Some(mut next_cfg) = fill_targets.read().clone() else {
                            return;
                        };
                        if let Ok(value) = evt.value().parse::<f32>() {
                            match field {
                                "nitrogen" => next_cfg.nitrogen.target_mass_kg = value.max(0.01),
                                "nitrous" => next_cfg.nitrous.target_mass_kg = value.max(0.01),
                                _ => {}
                            }
                            fill_targets.set(Some(next_cfg));
                            fill_targets_status.set("Unsaved fill target changes.".to_string());
                        }
                    }
                }
            }
            div { style: "display:flex; flex-direction:column; gap:6px;",
                label { style: "font-size:12px; color:{theme.text_muted}; text-transform:uppercase; letter-spacing:0.08em;", "Target pressure (psi)" }
                input {
                    r#type: "number",
                    step: "0.1",
                    min: "0",
                    disabled: !enabled,
                    style: "{input_style(theme)} cursor:{cursor};",
                    value: "{pressure_value}",
                    oninput: move |evt| {
                        if !enabled {
                            return;
                        }
                        let Some(mut next_cfg) = fill_targets.read().clone() else {
                            return;
                        };
                        if let Ok(value) = evt.value().parse::<f32>() {
                            match field {
                                "nitrogen" => next_cfg.nitrogen.target_pressure_psi = value.max(0.0),
                                "nitrous" => next_cfg.nitrous.target_pressure_psi = value.max(0.0),
                                _ => {}
                            }
                            fill_targets.set(Some(next_cfg));
                            fill_targets_status.set("Unsaved fill target changes.".to_string());
                        }
                    }
                }
            }
        }
    }
}
