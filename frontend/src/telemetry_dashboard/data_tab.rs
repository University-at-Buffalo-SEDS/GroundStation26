// frontend/src/telemetry_dashboard/data_tab.rs
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use groundstation_shared::TelemetryRow;

#[component]
pub fn DataTab(rows: Signal<Vec<TelemetryRow>>, active_tab: Signal<String>) -> Element {
    // Collect unique data types (for buttons)
    let mut types: Vec<String> = rows.read().iter().map(|r| r.data_type.clone()).collect();
    types.sort();
    types.dedup();
    //

    let current = active_tab.read().clone();

    let filtered: Vec<TelemetryRow> = rows
        .read()
        .iter()
        .rev() // newest first
        .filter(|r| r.data_type == current)
        .take(300)
        .cloned()
        .collect();

    rsx! {
        div { style: "padding:16px; height:100%; display:flex; flex-direction:column; gap:12px;",
            h2 { style: "margin:0;", "Data" }

            // type selector
            div { style: "display:flex; gap:8px; flex-wrap:wrap;",
                for t in types.iter().take(24) {
                    button {
                        style: if *t == current {
                            "padding:6px 10px; border-radius:999px; border:1px solid #60a5fa; background:#0b2a55; color:#dbeafe; cursor:pointer;"
                        } else {
                            "padding:6px 10px; border-radius:999px; border:1px solid #334155; background:#0b1220; color:#e5e7eb; cursor:pointer;"
                        },
                        onclick: {
                            let t = t.clone();
                            let mut active_tab2 = active_tab;
                            move |_| active_tab2.set(t.clone())
                        },
                        "{t}"
                    }
                }
            }

            // table
            div { style: "flex:1; overflow:auto; border:1px solid #334155; border-radius:14px; background:#0b1220;",
                table { style: "width:100%; border-collapse:collapse; font-size:12px;",
                    thead {
                        tr {
                            th { style: "text-align:left; padding:10px; border-bottom:1px solid #334155; color:#94a3b8;", "ts" }
                            th { style: "text-align:left; padding:10px; border-bottom:1px solid #334155; color:#94a3b8;", "v0" }
                            th { style: "text-align:left; padding:10px; border-bottom:1px solid #334155; color:#94a3b8;", "v1" }
                            th { style: "text-align:left; padding:10px; border-bottom:1px solid #334155; color:#94a3b8;", "v2" }
                            th { style: "text-align:left; padding:10px; border-bottom:1px solid #334155; color:#94a3b8;", "v3" }
                        }
                    }
                    tbody {
                        for r in filtered.iter() {
                            tr {
                                td { style: "padding:8px 10px; border-bottom:1px solid #1f2937; white-space:nowrap;", "{r.timestamp_ms}" }
                                td { style: "padding:8px 10px; border-bottom:1px solid #1f2937;", "{fmt_opt(r.v0)}" }
                                td { style: "padding:8px 10px; border-bottom:1px solid #1f2937;", "{fmt_opt(r.v1)}" }
                                td { style: "padding:8px 10px; border-bottom:1px solid #1f2937;", "{fmt_opt(r.v2)}" }
                                td { style: "padding:8px 10px; border-bottom:1px solid #1f2937;", "{fmt_opt(r.v3)}" }
                            }
                        }
                        if filtered.is_empty() {
                            tr {
                                td { colspan: "5", style: "padding:14px; color:#94a3b8;", "No rows for this data type yet." }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn fmt_opt(v: Option<f32>) -> String {
    match v {
        Some(x) => format!("{x:.4}"),
        None => "-".to_string(),
    }
}
