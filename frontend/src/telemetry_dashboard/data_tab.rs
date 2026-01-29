// frontend/src/telemetry_dashboard/data_tab.rs
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use groundstation_shared::TelemetryRow;

use super::data_chart::labels_for_datatype;

const _ACTIVE_TAB_STORAGE_KEY: &str = "gs26_active_tab";

#[cfg(target_arch = "wasm32")]
fn localstorage_get(key: &str) -> Option<String> {
    use web_sys::window;
    let w = window()?;
    let ls = w.local_storage().ok()??;
    ls.get_item(key).ok().flatten()
}

#[cfg(target_arch = "wasm32")]
fn localstorage_set(key: &str, value: &str) {
    use web_sys::window;
    if let Some(w) = window() {
        if let Ok(Some(ls)) = w.local_storage() {
            let _ = ls.set_item(key, value);
        }
    }
}

#[component]
pub fn DataTab(rows: Signal<Vec<TelemetryRow>>, active_tab: Signal<String>) -> Element {
    // -------- Restore + persist active tab --------
    let did_restore = use_signal(|| false);
    let last_saved = use_signal(String::new);

    // Restore ONCE
    use_effect({
        let rows = rows;
        let mut active_tab = active_tab;
        let mut did_restore = did_restore;

        move || {
            if *did_restore.read() {
                return;
            }
            did_restore.set(true);

            // 1) Try localStorage
            #[cfg(target_arch = "wasm32")]
            if let Some(saved) = localstorage_get(_ACTIVE_TAB_STORAGE_KEY) {
                if !saved.is_empty() {
                    active_tab.set(saved);
                    return;
                }
            }

            // 2) Fallback: if empty, pick first observed datatype
            if active_tab.read().is_empty() {
                let mut types: Vec<String> =
                    rows.read().iter().map(|r| r.data_type.clone()).collect();
                types.sort();
                types.dedup();
                if let Some(first) = types.first() {
                    active_tab.set(first.clone());
                }
            }
        }
    });

    // Persist whenever it changes (avoid rewriting same value)
    use_effect({
        let active_tab = active_tab;
        let mut last_saved = last_saved;

        move || {
            let cur = active_tab.read().clone();
            if cur.is_empty() || cur == *last_saved.read() {
                return;
            }
            last_saved.set(cur.clone());

            #[cfg(target_arch = "wasm32")]
            localstorage_set(_ACTIVE_TAB_STORAGE_KEY, &cur);
        }
    });

    // Collect unique data types (for buttons)
    let mut types: Vec<String> = rows.read().iter().map(|r| r.data_type.clone()).collect();
    types.sort();
    types.dedup();

    let current = active_tab.read().clone();

    // Filter rows for selected datatype, chronological (oldest..newest)
    let mut tab_rows: Vec<TelemetryRow> = rows
        .read()
        .iter()
        .filter(|r| r.data_type == current)
        .cloned()
        .collect();
    tab_rows.sort_by_key(|r| r.timestamp_ms);

    // Latest row for summary cards
    let latest_row = tab_rows.last().cloned();

    // Labels for cards and legend
    let labels = labels_for_datatype(&current);

    rsx! {
        div { style: "padding:16px; height:100%; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:auto; display:flex; flex-direction:column; gap:12px;",

            // -------- Top area: Tabs row THEN cards row (always below) --------
            div { style: "display:flex; flex-direction:column; gap:10px;",

                // Tabs row
                div { style: "display:flex; gap:8px; flex-wrap:wrap; align-items:center;",
                    for t in types.iter().take(32) {
                        button {
                            style: if *t == current {
                                "padding:6px 10px; border-radius:999px; border:1px solid #f97316; background:#111827; color:#f97316; cursor:pointer;"
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

                // Cards row (ALWAYS below tabs)
                match latest_row {
                    None => rsx! {
                        div { style: "color:#94a3b8; padding:2px 2px;", "Waiting for telemetry…" }
                    },
                    Some(row) => {
                        let vals = [row.v0, row.v1, row.v2, row.v3, row.v4, row.v5, row.v6, row.v7];

                        rsx! {
                            div { style: "display:flex; gap:10px; flex-wrap:wrap; align-items:flex-start;",
                                for i in 0..8usize {
                                    if !labels[i].is_empty() && vals[i].is_some() {
                                        SummaryCard {
                                            label: labels[i],
                                            value: fmt_opt(vals[i]),
                                            color: summary_color(i),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

        }
    }
}

fn summary_color(i: usize) -> &'static str {
    match i {
        0 => "#f97316",
        1 => "#22d3ee",
        2 => "#a3e635",
        _ => "#9ca3af",
    }
}

#[component]
fn SummaryCard(label: &'static str, value: String, color: &'static str) -> Element {
    rsx! {
        div { style: "padding:10px; border-radius:12px; background:#0f172a; border:1px solid #334155; min-width:92px;",
            div { style: "font-size:12px; color:{color};", "{label}" }
            div { style: "font-size:18px; color:#e5e7eb;", "{value}" }
        }
    }
}

fn fmt_opt(v: Option<f32>) -> String {
    match v {
        Some(x) => format!("{x:.4}"),
        None => "-".to_string(),
    }
}
