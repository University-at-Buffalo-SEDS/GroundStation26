use dioxus::prelude::*;
use dioxus_signals::Signal;
use super::AlertMsg;

#[component]
pub fn ErrorsTab(errors: Signal<Vec<AlertMsg>>) -> Element {
    rsx! {
        div { style: "padding:16px;",
            h2 { style: "margin:0 0 12px 0;", "Errors" }

            div { style: "display:flex; flex-direction:column; gap:10px;",
                for e in errors.read().iter() {
                    div {
                        style: "border:1px solid #ef4444; background:#450a0a; color:#fecaca; padding:12px; border-radius:12px;",
                        div { style: "font-size:12px; opacity:0.85;", "{e.timestamp_ms}" }
                        div { style: "font-size:14px;", "{e.message}" }
                    }
                }
                if errors.read().is_empty() {
                    div { style: "color:#94a3b8;", "No errors." }
                }
            }
        }
    }
}
