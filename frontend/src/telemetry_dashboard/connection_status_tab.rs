use dioxus::prelude::*;
use dioxus_signals::Signal;
use groundstation_shared::BoardStatusEntry;

#[component]
pub fn ConnectionStatusTab(boards: Signal<Vec<BoardStatusEntry>>) -> Element {
    rsx! {
        div { style: "padding:16px;",
            h2 { style: "margin:0 0 12px 0;", "Connection Status" }
            div { style: "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
                div { style: "font-size:14px; color:#94a3b8; margin-bottom:8px;", "Board Status" }
                if boards.read().is_empty() {
                    div { style: "color:#94a3b8;", "No board status yet." }
                } else {
                    div { style: "display:grid; grid-template-columns: 1.1fr 1.1fr 0.7fr 1fr 1fr; gap:8px; font-size:13px; color:#cbd5f5;",
                        div { style: "font-weight:600; color:#e2e8f0;", "Board" }
                        div { style: "font-weight:600; color:#e2e8f0;", "Sender ID" }
                        div { style: "font-weight:600; color:#e2e8f0;", "Seen" }
                        div { style: "font-weight:600; color:#e2e8f0;", "Last Seen (ms)" }
                        div { style: "font-weight:600; color:#e2e8f0;", "Age (ms)" }
                        for entry in boards.read().iter() {
                            div { "{entry.board.as_str()}" }
                            div { "{entry.sender_id}" }
                            div { if entry.seen { "yes" } else { "no" } }
                            div {
                                if let Some(ts) = entry.last_seen_ms {
                                    "{ts}"
                                } else {
                                    "—"
                                }
                            }
                            div {
                                if let Some(age) = entry.age_ms {
                                    "{age}"
                                } else {
                                    "—"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
