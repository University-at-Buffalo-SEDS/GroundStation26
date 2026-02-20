use super::{format_timestamp_ms_clock, AlertMsg};
use dioxus::prelude::*;

#[component]
pub fn WarningsTab(warnings: Signal<Vec<AlertMsg>>) -> Element {
    rsx! {
        div { style: "padding:16px;",
            h2 { style: "margin:0 0 12px 0;", "Warnings" }

            div { style: "display:flex; flex-direction:column; gap:10px;",
                for w in warnings.read().iter() {
                    div {
                        style: "border:1px solid #a16207; background:#2a1a04; color:#fde68a; padding:12px; border-radius:12px;",
                        div { style: "font-size:12px; opacity:0.85;", "{format_timestamp_ms_clock(w.timestamp_ms)}" }
                        div { style: "font-size:14px;", "{w.message}" }
                    }
                }
                if warnings.read().is_empty() {
                    div { style: "color:#94a3b8;", "No warnings." }
                }
            }
        }
    }
}
