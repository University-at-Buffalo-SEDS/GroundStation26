// frontend/src/telemetry_dashboard/actions_tab.rs

use dioxus::prelude::*;

use super::layout::ActionsTabLayout;
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

#[component]
pub fn ActionsTab(layout: ActionsTabLayout, action_policy: Signal<ActionPolicyMsg>) -> Element {
    let mut redraw_tick = use_signal(|| 0u64);
    use_effect(move || {
        spawn(async move {
            loop {
                sleep_for_frame(target_frame_duration()).await;
                let next = redraw_tick.read().wrapping_add(1);
                redraw_tick.set(next);
            }
        });
    });
    let _blink_tick = *redraw_tick.read();
    let blink_now_ms = blink_epoch_ms();
    rsx! {
        div {
            style: "
                padding:16px;
                display:flex;
                flex-direction:column;
                gap:12px;
            ",
            h2 { style: "margin:0 0 8px 0; color:#e5e7eb;", "Actions" }
            p  { style: "margin:0 0 12px 0; color:#9ca3af; font-size:0.9rem;",
                "All available actions are available all the time, use with caution as improper use \
                can and will damage the system."
            }

            div {
                style: "
                    display:grid;
                    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
                    gap:12px;
                ",

                for action in layout.actions.iter() {
                    {
                        let software_buttons_enabled = action_policy.read().software_buttons_enabled;
                        let control = action_policy
                            .read()
                            .controls
                            .iter()
                            .find(|c| c.cmd == action.cmd)
                            .cloned();
                        let enabled = software_buttons_enabled
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
    }
}
