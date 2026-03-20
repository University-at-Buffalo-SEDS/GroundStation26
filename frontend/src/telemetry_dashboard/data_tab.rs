use super::layout::{BatterySourceConfig, BooleanLabels, DataTabLayout};
use super::types::TelemetryRow;
// frontend/src/telemetry_dashboard/data_tab.rs
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};

use super::data_chart::{
    charts_cache_get, charts_cache_get_channel_minmax, combined_battery_chart_key, series_color,
    ChartCanvas,
};
use super::{latest_telemetry_row, latest_telemetry_value, TELEMETRY_RENDER_EPOCH};

const _ACTIVE_TAB_STORAGE_KEY: &str = "gs26_active_tab";
const BATTERY_RUNTIME_TAB_ID: &str = "BATTERY_RUNTIME";
const LOADCELL_TAB_ID: &str = "LOADCELL";

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
pub fn DataTab(
    active_tab: Signal<String>,
    layout: DataTabLayout,
    battery_sources: Vec<BatterySourceConfig>,
) -> Element {
    let _ = *TELEMETRY_RENDER_EPOCH.read();
    let mut is_fullscreen = use_signal(|| false);
    let mut show_chart = use_signal(|| true);

    // -------- Restore + persist active tab --------
    let did_restore = use_signal(|| false);
    let last_saved = use_signal(String::new);

    // Restore ONCE
    use_effect({
        let mut active_tab = active_tab;
        let mut did_restore = did_restore;
        let layout_tabs = layout.tabs.clone();

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

            // 2) Fallback: if empty, pick first layout tab
            if active_tab.read().is_empty()
                && let Some(first) = layout_tabs.first()
            {
                active_tab.set(first.id.clone());
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

    // Layout-defined data types (for buttons)
    let types = layout.tabs.clone();
    let current = active_tab.read().clone();
    let is_battery_raw_tab = matches!(current.as_str(), "BATTERY_VOLTAGE" | "BATTERY_CURRENT");
    let is_battery_runtime_tab = current == BATTERY_RUNTIME_TAB_ID;
    let is_loadcell_tab = current == LOADCELL_TAB_ID;

    let current_tab = types.iter().find(|t| t.id == current);

    let mut labels: Vec<String> = Vec::new();
    if let Some(tab) = current_tab {
        labels = tab.channels.clone();
    }

    let chart_enabled = current_tab
        .and_then(|tab| tab.chart.as_ref().map(|c| c.enabled))
        .unwrap_or(true);

    // Latest row for summary cards (scan backward; no sort/filter allocations)
    let latest_row = latest_telemetry_row(&current, None);
    let battery_card_rows: Vec<(&BatterySourceConfig, Option<TelemetryRow>)> = battery_sources
        .iter()
        .filter(|source| {
            (is_battery_raw_tab && source.input_data_type == "BATTERY_VOLTAGE")
                || source.input_data_type == current
                || source.percent_data_type == current
                || source.drop_rate_data_type == current
                || source.remaining_minutes_data_type == current
        })
        .map(|source| {
            let row = latest_telemetry_row(&current, Some(&source.sender_id));
            (source, row)
        })
        .collect();
    let battery_graph_sources: Vec<&BatterySourceConfig> = battery_sources
        .iter()
        .filter(|source| {
            (is_battery_raw_tab && source.input_data_type == "BATTERY_VOLTAGE")
                || source.input_data_type == current
        })
        .collect();
    let synthetic_cards = if is_battery_runtime_tab {
        battery_runtime_cards(&battery_sources)
    } else if is_loadcell_tab {
        loadcell_cards()
    } else {
        Vec::new()
    };

    let is_valve_state = current == "VALVE_STATE";
    let boolean_labels = current_tab.and_then(|t| t.boolean_labels.as_ref());
    let channel_boolean_labels = current_tab.and_then(|t| t.channel_boolean_labels.as_ref());
    let has_telemetry = if battery_card_rows.is_empty() {
        latest_row.is_some() || !synthetic_cards.is_empty()
    } else {
        battery_card_rows.iter().any(|(_, row)| row.is_some())
    };
    let is_graph_allowed = chart_enabled
        && has_telemetry
        && current != "GPS_DATA"
        && !is_valve_state
        && !is_battery_runtime_tab
        && !is_loadcell_tab;

    // Viewport constants
    let view_w = 1200.0_f64;
    let view_h = 360.0_f64;
    let view_h_full = fullscreen_view_height().max(260.0);

    let left = 60.0_f64;
    let right = view_w - 20.0_f64;
    let pad_top = 20.0_f64;
    let pad_bottom = 20.0_f64;

    let inner_h = view_h - pad_top - pad_bottom;
    let inner_h_full = view_h_full - pad_top - pad_bottom;

    let chart_key = if is_battery_raw_tab {
        combined_battery_chart_key(&current).unwrap_or_else(|| current.clone())
    } else {
        current.clone()
    };
    // Cache fetch (NON-FULLSCREEN)
    //
    // IMPORTANT: We do NOT fetch fullscreen geometry here anymore.
    // That avoids doing two cache builds every frame (w,h=360 and w,h=full),
    // which can interact badly with "window span" behavior and costs extra CPU.
    let (chunks, y_min, y_max, span_min) =
        charts_cache_get(&chart_key, view_w as f32, view_h as f32);
    let (chan_min, chan_max) =
        charts_cache_get_channel_minmax(&chart_key, view_w as f32, view_h as f32);
    let y_mid = (y_min + y_max) * 0.5;
    let y_max_s = format!("{:.2}", y_max);
    let y_mid_s = format!("{:.2}", y_mid);
    let y_min_s = format!("{:.2}", y_min);
    let x_left_s = fmt_span(span_min);
    let x_mid_s = fmt_span(span_min * 0.5);
    let x_pct = |x: f64, total: f64| format!("{:.4}%", (x / total) * 100.0);
    let y_pct = |y: f64, total: f64| format!("{:.4}%", (y / total) * 100.0);
    let legend_items: Vec<(usize, &str)> =
        if is_battery_raw_tab && !battery_graph_sources.is_empty() {
            battery_graph_sources
                .iter()
                .enumerate()
                .map(|(i, source)| (i, source.label.as_str()))
                .collect()
        } else {
            labels
                .iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    if l.is_empty() {
                        None
                    } else {
                        Some((i, l.as_str()))
                    }
                })
                .collect()
        };
    let legend_rows: Vec<(usize, &str)> =
        legend_items.iter().map(|(i, label)| (*i, *label)).collect();
    let on_toggle_fullscreen = move |_| {
        let next = !*is_fullscreen.read();
        is_fullscreen.set(next);
    };
    let on_toggle_chart = move |_| {
        let next = !*show_chart.read();
        show_chart.set(next);
    };
    let summary_content = if !synthetic_cards.is_empty() {
        rsx! {
            div {
                style: "display:grid; gap:10px; align-items:stretch; grid-template-columns:repeat(auto-fit, minmax(150px, 1fr)); width:100%;",
                for (i, (label, value)) in synthetic_cards.iter().enumerate() {
                    SummaryCard {
                        label: label.clone(),
                        min: None,
                        max: None,
                        value: value.clone(),
                        color: summary_color(i),
                    }
                }
            }
        }
    } else {
        match latest_row {
            None => rsx! {
                div { style: "color:#94a3b8; padding:2px 2px;", "Waiting for telemetry…" }
            },
            Some(row) => {
                let vals = row.values.clone();
                rsx! {
                    div {
                        style: "display:grid; gap:10px; align-items:stretch; grid-template-columns:repeat(auto-fit, minmax(110px, 1fr)); width:100%;",
                        if battery_card_rows.is_empty() {
                            for (i, label) in labels.iter().enumerate() {
                                if !label.is_empty() {
                                    SummaryCard {
                                        label: label.clone(),
                                        min: if is_graph_allowed { chan_min.get(i).copied().flatten().map(|v| format!("{v:.4}")) } else { None },
                                        max: if is_graph_allowed { chan_max.get(i).copied().flatten().map(|v| format!("{v:.4}")) } else { None },
                                        value: if let Some(lbls) = channel_boolean_labels
                                            .and_then(|list| list.get(i))
                                        {
                                            boolean_value_text(vals.get(i).copied().flatten(), Some(lbls))
                                        } else if is_valve_state || boolean_labels.is_some() {
                                            boolean_value_text(vals.get(i).copied().flatten(), boolean_labels)
                                        } else {
                                            fmt_opt(vals.get(i).copied().flatten())
                                        },
                                        color: summary_color(i),
                                    }
                                }
                            }
                        } else {
                            for (i, (source, battery_row)) in battery_card_rows.iter().enumerate() {
                                SummaryCard {
                                    label: source.label.clone(),
                                    min: None,
                                    max: None,
                                    value: battery_row
                                        .as_ref()
                                        .and_then(|row| row.values.first().copied().flatten())
                                        .map(|v| format!("{v:.4}"))
                                        .unwrap_or_else(|| "—".to_string()),
                                    color: summary_color(i),
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        div {
            style: "padding:16px; height:100%; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:auto; display:flex; flex-direction:column; gap:12px; padding-bottom:10px;",

            div { style: "display:flex; flex-direction:column; gap:10px;",

                div { style: "display:flex; gap:8px; flex-wrap:wrap; align-items:center;",
                    for t in types.iter().take(32) {
                        button {
                            style: if t.id == current {
                                "padding:6px 10px; border-radius:999px; border:1px solid #f97316; background:#111827; color:#f97316; cursor:pointer;"
                            } else {
                                "padding:6px 10px; border-radius:999px; border:1px solid #334155; background:#0b1220; color:#e5e7eb; cursor:pointer;"
                            },
                            onclick: {
                                let t = t.id.clone();
                                let mut active_tab2 = active_tab;
                                move |_| active_tab2.set(t.clone())
                            },
                            "{t.label}"
                        }
                    }
                }

                {summary_content}
            }

            // =========================
            // Graph (non-fullscreen)
            // =========================
            if is_graph_allowed {
                div { style: "flex:0; width:100%; margin-top:6px;",
                    div { style: "width:100%;",

                        div { style: "display:flex; justify-content:flex-end; gap:8px; margin-bottom:6px;",
                            button {
                                style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                                onclick: on_toggle_chart,
                                if *show_chart.read() { "Collapse" } else { "Expand" }
                            }
                            button {
                                style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                                onclick: on_toggle_fullscreen,
                                "Fullscreen"
                            }
                        }

                        if *show_chart.read() {
                            div { style: "width:100%; background:#020617; border-radius:14px; border:1px solid #334155; padding:12px; display:flex; flex-direction:column; gap:8px;",
                                div { style: "position:relative; width:100%; aspect-ratio:{view_w}/{view_h};",
                                    ChartCanvas {
                                        view_w: view_w,
                                        view_h: view_h,
                                        chunks: chunks.clone(),
                                        style: "position:absolute; inset:0; width:100%; height:100%; display:block;".to_string(),
                                    }
                                    div { style: "position:absolute; inset:0; pointer-events:none; font-size:10px; color:#94a3b8;",
                                        span { style: "position:absolute; left:10px; top:{y_pct(pad_top + 6.0, view_h)};", "{y_max_s.clone()}" }
                                        span { style: "position:absolute; left:10px; top:{y_pct(pad_top + inner_h / 2.0 + 4.0, view_h)}; transform:translateY(-50%);", "{y_mid_s.clone()}" }
                                        span { style: "position:absolute; left:10px; top:{y_pct(view_h - pad_bottom + 4.0, view_h)}; transform:translateY(-100%);", "{y_min_s.clone()}" }
                                        span { style: "position:absolute; left:{x_pct(left + 10.0, view_w)}; bottom:5px;", "{x_left_s.clone()}" }
                                        span { style: "position:absolute; left:{x_pct(view_w * 0.5, view_w)}; bottom:5px; transform:translateX(-50%);", "{x_mid_s.clone()}" }
                                        span { style: "position:absolute; left:{x_pct(right - 60.0, view_w)}; bottom:5px;", "now" }
                                    }
                                }
                                if !legend_rows.is_empty() {
                                    div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:6px 10px; background:rgba(2,6,23,0.75); border:1px solid #1f2937; border-radius:10px;",
                                        for (i, label) in legend_rows.iter() {
                                            div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:#cbd5f5;",
                                                svg { width:"26", height:"8", view_box:"0 0 26 8",
                                                    line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(*i)}", "stroke-width":"2", "stroke-linecap":"round" }
                                                }
                                                "{label}"
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

        // =========================
        // Fullscreen
        // =========================
        if is_graph_allowed && *is_fullscreen.read() {
            {
                let (chunks_full, _y_min2, _y_max2, _span_min2) =
                    charts_cache_get(&chart_key, view_w as f32, view_h_full as f32);

                rsx! {
                    div { style: "position:fixed; inset:0; z-index:9998; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px;",
                        div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px;",
                            h2 { style: "margin:0; color:#f97316;", "Data Graph" }
                            button {
                                style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                                onclick: on_toggle_fullscreen,
                                "Exit Fullscreen"
                            }
                        }

                        div {
                            style: "flex:1; min-height:0; width:100%; background:#020617; border-radius:14px; border:1px solid #334155; padding:12px; display:flex; flex-direction:column; align-items:stretch; gap:8px;",
                            div { style: "position:relative; flex:1; min-height:0; width:100%;",
                                ChartCanvas {
                                    view_w: view_w,
                                    view_h: view_h_full,
                                    chunks: chunks_full,
                                    style: "position:absolute; inset:0; width:100%; height:100%; display:block;".to_string(),
                                }
                                div { style: "position:absolute; inset:0; pointer-events:none; font-size:10px; color:#94a3b8;",
                                    span { style: "position:absolute; left:10px; top:{y_pct(pad_top + 6.0, view_h_full)};", "{y_max_s.clone()}" }
                                    span { style: "position:absolute; left:10px; top:{y_pct(pad_top + inner_h_full / 2.0 + 4.0, view_h_full)}; transform:translateY(-50%);", "{y_mid_s.clone()}" }
                                    span { style: "position:absolute; left:10px; top:{y_pct(view_h_full - pad_bottom + 4.0, view_h_full)}; transform:translateY(-100%);", "{y_min_s.clone()}" }
                                    span { style: "position:absolute; left:{x_pct(left + 10.0, view_w)}; bottom:5px;", "{x_left_s.clone()}" }
                                    span { style: "position:absolute; left:{x_pct(view_w * 0.5, view_w)}; bottom:5px; transform:translateX(-50%);", "{x_mid_s.clone()}" }
                                    span { style: "position:absolute; left:{x_pct(right - 60.0, view_w)}; bottom:5px;", "now" }
                                }
                            }
                            if !legend_rows.is_empty() {
                                div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:8px 12px; background:rgba(2,6,23,0.75); border:1px solid #1f2937; border-radius:10px;",
                                    for (i, label) in legend_rows.iter() {
                                        div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:#cbd5f5;",
                                            svg { width:"26", height:"8", view_box:"0 0 26 8",
                                                line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(*i)}", "stroke-width":"2", "stroke-linecap":"round" }
                                            }
                                            "{label}"
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
    series_color(i)
}

fn latest_value_for(data_type: &str, sender_id: Option<&str>) -> Option<f32> {
    latest_telemetry_value(data_type, sender_id, 0)
}

fn battery_runtime_cards(battery_sources: &[BatterySourceConfig]) -> Vec<(String, String)> {
    let mut cards = Vec::new();
    for source in battery_sources {
        let prefix = source.label.clone();
        cards.push((
            format!("{prefix} %"),
            fmt_opt(latest_value_for(
                &source.percent_data_type,
                Some(&source.sender_id),
            )),
        ));
        cards.push((
            format!("{prefix} Drop"),
            fmt_opt(latest_value_for(
                &source.drop_rate_data_type,
                Some(&source.sender_id),
            )),
        ));
        cards.push((
            format!("{prefix} Runtime"),
            fmt_opt(latest_value_for(
                &source.remaining_minutes_data_type,
                Some(&source.sender_id),
            )),
        ));
    }
    cards
}

fn loadcell_cards() -> Vec<(String, String)> {
    vec![
        (
            "Raw Loadcell".to_string(),
            fmt_opt(latest_value_for("KG1000", None)),
        ),
        (
            "Weight (kg)".to_string(),
            fmt_opt(latest_value_for("LOADCELL_WEIGHT_KG", None)),
        ),
        (
            "Fill %".to_string(),
            fmt_opt(latest_value_for("LOADCELL_FILL_PERCENT", None)),
        ),
    ]
}

#[component]
fn SummaryCard(
    label: String,
    value: String,
    min: Option<String>,
    max: Option<String>,
    color: &'static str,
) -> Element {
    let mm = match (min.as_deref(), max.as_deref()) {
        (Some(mi), Some(ma)) => Some(format!("min {mi} • max {ma}")),
        _ => None,
    };

    rsx! {
        div { style: "padding:10px; border-radius:12px; background:#0f172a; border:1px solid #334155; width:100%; min-width:0; box-sizing:border-box;",
            div { style: "font-size:12px; color:{color};", "{label}" }
            div { style: "font-size:18px; color:#e5e7eb; line-height:1.1;", "{value}" }
            if let Some(t) = mm {
                div { style: "font-size:11px; color:#94a3b8; margin-top:4px;", "{t}" }
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

fn boolean_value_text(v: Option<f32>, labels: Option<&BooleanLabels>) -> String {
    let true_label = labels.map(|l| l.true_label.as_str()).unwrap_or("Open");
    let false_label = labels.map(|l| l.false_label.as_str()).unwrap_or("Closed");
    let unknown_label = labels
        .and_then(|l| l.unknown_label.as_deref())
        .unwrap_or("Unknown");
    match v {
        Some(val) if val >= 0.5 => true_label.to_string(),
        Some(_) => false_label.to_string(),
        None => unknown_label.to_string(),
    }
}

fn fullscreen_view_height() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        let h = web_sys::window()
            .and_then(|w| w.inner_height().ok())
            .and_then(|v| v.as_f64())
            .unwrap_or(700.0);
        (h - 140.0).max(360.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        600.0
    }
}

fn fmt_span(span_min: f32) -> String {
    if !span_min.is_finite() || span_min <= 0.0 {
        "-0 s".to_string()
    } else if span_min < 1.0 {
        format!("-{:.0} s", span_min * 60.0)
    } else {
        format!("-{:.1} min", span_min)
    }
}
