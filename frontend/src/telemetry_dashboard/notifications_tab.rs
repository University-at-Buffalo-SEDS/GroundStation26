use super::{format_timestamp_ms_clock, PersistentNotification};
use dioxus::prelude::*;

#[component]
pub fn NotificationsTab(history: Signal<Vec<PersistentNotification>>) -> Element {
    rsx! {
        div { style: "padding:16px;",
            h2 { style: "margin:0 0 12px 0;", "Notifications History" }

            div { style: "display:flex; flex-direction:column; gap:10px;",
                for n in history.read().iter() {
                    div {
                        style: "border:1px solid #2563eb; background:#0b1f4d; color:#bfdbfe; padding:12px; border-radius:12px;",
                        div { style: "font-size:12px; opacity:0.85;", "{format_timestamp_ms_clock(n.timestamp_ms)}" }
                        div { style: "font-size:14px;", "{n.message}" }
                    }
                }
                if history.read().is_empty() {
                    div { style: "color:#94a3b8;", "No notifications yet." }
                }
            }
        }
    }
}
