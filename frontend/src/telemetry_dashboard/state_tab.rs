use dioxus::prelude::*;
use dioxus_signals::Signal;
use groundstation_shared::FlightState;

#[component]
pub fn StateTab(flight_state: Signal<FlightState>) -> Element {
    rsx! {
        div { style: "padding:16px;",
            h2 { style: "margin:0 0 12px 0;", "State" }
            div { style: "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
                div { style: "font-size:14px; color:#94a3b8;", "Current Flight State" }
                div { style: "font-size:22px; font-weight:700; margin-top:6px;",
                    "{flight_state.read().to_string()}"
                }
            }
        }
    }
}
