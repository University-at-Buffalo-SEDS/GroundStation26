// frontend/src/telemetry_dashboard/actions_tab.rs

use dioxus::prelude::*;

fn btn_style(border: &str, bg: &str, fg: &str) -> String {
    format!(
        "padding:0.65rem 1rem; border-radius:0.75rem; cursor:pointer; width:100%; \
         text-align:left; border:1px solid {border}; background:{bg}; color:{fg}; \
         font-weight:800; box-shadow:0 10px 25px rgba(0,0,0,0.25);"
    )
}

#[component]
pub fn ActionsTab() -> Element {
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
                "Non-emergency actions live here. Abort is always available in the header."
            }

            div {
                style: "
                    display:grid;
                    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
                    gap:12px;
                ",

                // Replace/extend these with your real commands (everything except Abort)
                button {
                    style: "{btn_style(\"#22c55e\", \"#022c22\", \"#bbf7d0\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("Launch"),
                    "Launch"
                }
                button {
                    style: "{btn_style(\"#ef4444\", \"#450a0a\", \"#fecaca\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("Dump"),
                    "Dump"
                }

                // Examples
                button {
                    style: "{btn_style(\"#60a5fa\", \"#0b1220\", \"#bfdbfe\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("Igniter"),
                    "Igniter"
                }
                button {
                    style: "{btn_style(\"#a78bfa\", \"#111827\", \"#ddd6fe\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("Pilot"),
                    "Pilot"
                }
                button {
                    style: "{btn_style(\"#f97316\", \"#1f2937\", \"#ffedd5\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("Tanks"),
                    "NormallyOpen"
                }
                button {
                    style: "{btn_style(\"#22d3ee\", \"#0b1220\", \"#cffafe\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("Nitrogen"),
                    "Nitrogen"
                }
                button {
                    style: "{btn_style(\"#a3e635\", \"#111827\", \"#ecfccb\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("Nitrous"),
                    "Nitrous"
                }
                button {
                    style: "{btn_style(\"#eab308\", \"#1f2937\", \"#fef9c3\")}",
                    onclick: move |_| crate::telemetry_dashboard::send_cmd("RetractPlumbing"),
                    "Fill Lines"
                }
            }
        }
    }
}
