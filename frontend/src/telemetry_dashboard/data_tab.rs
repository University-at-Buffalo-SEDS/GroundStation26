// frontend/src/telemetry_dashboard/data_tab.rs
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use groundstation_shared::TelemetryRow;

use super::data_chart::{build_polylines, labels_for_datatype, series_color};

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
    let mut is_fullscreen = use_signal(|| false);
    let mut show_chart = use_signal(|| true);
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

    let is_valve_state = current == "VALVE_STATE";
    let has_telemetry = latest_row.is_some();
    let is_graph_allowed = has_telemetry && current != "GPS_DATA" && !is_valve_state;

    // Labels for cards and legend
    let labels = labels_for_datatype(&current);

    // Build graph polylines (8 series) + y-range + span
    let view_w = 1200.0_f64;
    let view_h = 360.0_f64;
    let view_h_full = fullscreen_view_height().max(260.0);
    let left = 60.0_f64;
    let right = view_w - 20.0_f64;
    let pad_top = 20.0_f64;
    let pad_bottom = 20.0_f64;
    let inner_w = right - left;
    let grid_x_step = inner_w / 6.0_f64;

    let inner_h = view_h - pad_top - pad_bottom;
    let inner_h_full = view_h_full - pad_top - pad_bottom;
    let grid_y_step = inner_h / 6.0_f64;
    let grid_y_step_full = inner_h_full / 6.0_f64;

    let (paths, y_min, y_max, span_min) = build_polylines(&tab_rows, view_w as f32, view_h as f32);
    let (paths_full, _, _, _) = build_polylines(&tab_rows, view_w as f32, view_h_full as f32);
    let y_mid = (y_min + y_max) * 0.5;

    let legend_items: Vec<(usize, &'static str)> = labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| if l.is_empty() { None } else { Some((i, *l)) })
        .collect();
    let legend_rows: Vec<(usize, &'static str)> =
        legend_items.iter().map(|(i, label)| (*i, *label)).collect();

    let on_toggle_fullscreen = move |_| {
        let next = !*is_fullscreen.read();
        is_fullscreen.set(next);
    };
    let on_toggle_chart = move |_| {
        let next = !*show_chart.read();
        show_chart.set(next);
    };

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
                                    if !labels[i].is_empty() {
                                        SummaryCard {
                                            label: labels[i],
                                            value: if is_valve_state {
                                                valve_state_text(vals[i], labels[i] == "Fill Lines")
                                            } else {
                                                fmt_opt(vals[i])
                                            },
                                            color: summary_color(i),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // -------- Big centered graph --------
            if is_graph_allowed {
                div { style: "flex:0; display:flex; align-items:center; justify-content:center; margin-top:6px;",
                    div { style: "width:100%; max-width:1200px;",
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
                                svg {
                                    style: "width:100%; height:auto; display:block;",
                                    view_box: "0 0 {view_w} {view_h}",

                                // gridlines
                                for i in 1..=5 {
                                    line {
                                        x1:"{left}", y1:"{pad_top + grid_y_step * (i as f64)}",
                                        x2:"{right}", y2:"{pad_top + grid_y_step * (i as f64)}",
                                        stroke:"#1f2937", "stroke-width":"1"
                                    }
                                }
                                for i in 1..=5 {
                                    line {
                                        x1:"{left + grid_x_step * (i as f64)}", y1:"{pad_top}",
                                        x2:"{left + grid_x_step * (i as f64)}", y2:"{view_h - pad_bottom}",
                                        stroke:"#1f2937", "stroke-width":"1"
                                    }
                                }

                                // axes
                                line { x1:"{left}", y1:"{pad_top}",  x2:"{left}",   y2:"{view_h - pad_bottom}", stroke:"#334155", stroke_width:"1" }
                                line { x1:"{left}", y1:"{view_h - pad_bottom}", x2:"{right}", y2:"{view_h - pad_bottom}", stroke:"#334155", stroke_width:"1" }

                                // y labels
                                text { x:"10", y:"{pad_top + 6.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_max)} }
                                text { x:"10", y:"{pad_top + inner_h / 2.0 + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_mid)} }
                                text { x:"10", y:"{view_h - pad_bottom + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_min)} }

                                // x labels (span in minutes)
                                text { x:"{left + 10.0}",   y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min)} }
                                text { x:"{view_w * 0.5}",  y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min * 0.5)} }
                                text { x:"{right - 60.0}", y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", "now" }

                                // series
                                for (i, pts) in paths.iter().enumerate() {
                                    if !pts.is_empty() {
                                        polyline {
                                            points: "{pts}",
                                            fill: "none",
                                            stroke: "{series_color(i)}",
                                            stroke_width: "2",
                                            stroke_linejoin: "round",
                                            stroke_linecap: "round",
                                        }
                                    }
                                }
                            }

                            if !legend_rows.is_empty() {
                                div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:6px 10px; background:rgba(2,6,23,0.75); border:1px solid #1f2937; border-radius:10px;",
                                    for (i, label) in legend_rows.iter() {
                                        div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:#cbd5f5;",
                                            svg { width:"26", height:"8", view_box:"0 0 26 8",
                                                line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(*i)}", stroke_width:"2", stroke_linecap:"round" }
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

        if is_graph_allowed && *is_fullscreen.read() {
            div { style: "position:fixed; inset:0; z-index:9998; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px;",
                div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px;",
                    h2 { style: "margin:0; color:#f97316;", "Data Graph" }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                        onclick: on_toggle_fullscreen,
                        "Exit Fullscreen"
                    }
                }
                div { style: "flex:1; min-height:0; width:100%; background:#020617; border-radius:14px; border:1px solid #334155; padding:12px; display:flex; flex-direction:column; gap:8px;",
                    svg {
                        style: "width:100%; flex:1; min-height:0; display:block;",
                        view_box: "0 0 {view_w} {view_h_full}",

                        // gridlines
                        for i in 1..=5 {
                            line {
                                x1:"{left}", y1:"{pad_top + grid_y_step_full * (i as f64)}",
                                x2:"{right}", y2:"{pad_top + grid_y_step_full * (i as f64)}",
                                stroke:"#1f2937", "stroke-width":"1"
                            }
                        }
                        for i in 1..=5 {
                            line {
                                x1:"{left + grid_x_step * (i as f64)}", y1:"{pad_top}",
                                x2:"{left + grid_x_step * (i as f64)}", y2:"{view_h_full - pad_bottom}",
                                stroke:"#1f2937", "stroke-width":"1"
                            }
                        }

                        // axes
                        line { x1:"{left}", y1:"{pad_top}",  x2:"{left}",   y2:"{view_h_full - pad_bottom}", stroke:"#334155", stroke_width:"1" }
                        line { x1:"{left}", y1:"{view_h_full - pad_bottom}", x2:"{right}", y2:"{view_h_full - pad_bottom}", stroke:"#334155", stroke_width:"1" }

                        // y labels
                        text { x:"10", y:"{pad_top + 6.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_max)} }
                        text { x:"10", y:"{pad_top + inner_h_full / 2.0 + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_mid)} }
                        text { x:"10", y:"{view_h_full - pad_bottom + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_min)} }

                        // x labels (span in minutes)
                        text { x:"{left + 10.0}",   y:"{view_h_full - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min)} }
                        text { x:"{view_w * 0.5}",  y:"{view_h_full - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min * 0.5)} }
                        text { x:"{right - 60.0}", y:"{view_h_full - 5.0}", fill:"#94a3b8", "font-size":"10", "now" }

                        // series
                        for (i, pts) in paths_full.iter().enumerate() {
                            if !pts.is_empty() {
                                polyline {
                                    points: "{pts}",
                                    fill: "none",
                                    stroke: "{series_color(i)}",
                                    stroke_width: "2",
                                    stroke_linejoin: "round",
                                    stroke_linecap: "round",
                                }
                            }
                        }

                    }
                    if !legend_rows.is_empty() {
                        div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:8px 12px; background:rgba(2,6,23,0.75); border:1px solid #1f2937; border-radius:10px;",
                            for (i, label) in legend_rows.iter() {
                                div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:#cbd5f5;",
                                    svg { width:"26", height:"8", view_box:"0 0 26 8",
                                        line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(*i)}", stroke_width:"2", stroke_linecap:"round" }
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

fn valve_state_text(v: Option<f32>, is_fill_lines: bool) -> String {
    match v {
        Some(val) if val >= 0.5 => {
            if is_fill_lines {
                "Installed".to_string()
            } else {
                "Open".to_string()
            }
        }
        Some(_) => {
            if is_fill_lines {
                "Removed".to_string()
            } else {
                "Closed".to_string()
            }
        }
        None => "Unknown".to_string(),
    }
}

fn fullscreen_view_height() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        let h = web_sys::window()
            .and_then(|w| w.inner_height().ok())
            .and_then(|v| v.as_f64())
            .unwrap_or(700.0);
        // Account for padding + header row in fullscreen overlay.
        (h - 140.0).max(360.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        600.0
    }
}
