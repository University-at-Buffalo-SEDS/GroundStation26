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

fn blink_animation(blink: BlinkMode, actuated: Option<bool>) -> (&'static str, u64) {
    match (blink, actuated.unwrap_or(false)) {
        (BlinkMode::None, _) => ("none", 0),
        (BlinkMode::Slow, false) => ("gs26-blink-slow-off 1.8s linear infinite", 1_800),
        (BlinkMode::Slow, true) => ("gs26-blink-slow-on 1.8s linear infinite", 1_800),
        (BlinkMode::Fast, false) => ("gs26-blink-fast-off 0.6s linear infinite", 600),
        (BlinkMode::Fast, true) => ("gs26-blink-fast-on 0.6s linear infinite", 600),
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
    let opacity = if !enabled {
        "0.45"
    } else if recommended {
        "1.0"
    } else {
        "0.62"
    };
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
    let (animation, period_ms) = blink_animation(blink, actuated);
    let animation_delay = if period_ms == 0 {
        "0s".to_string()
    } else {
        format!("-{}s", (blink_now_ms % period_ms) as f64 / 1000.0)
    };
    format!(
        "padding:0.65rem 1rem; border-radius:0.75rem; cursor:{cursor}; opacity:{opacity}; filter:{filter}; animation:{animation}; animation-delay:{animation_delay}; width:100%; \
         text-align:left; border:1px solid {border}; background:{bg}; color:{fg}; \
         font-weight:800; box-shadow:{box_shadow};"
    )
}

#[component]
pub fn ActionsTab(layout: ActionsTabLayout, action_policy: Signal<ActionPolicyMsg>) -> Element {
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
                        let control = action_policy
                            .read()
                            .controls
                            .iter()
                            .find(|c| c.cmd == action.cmd)
                            .cloned();
                        let enabled = control
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
