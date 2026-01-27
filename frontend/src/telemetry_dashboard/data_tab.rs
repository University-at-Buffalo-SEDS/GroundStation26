// frontend/src/telemetry_dashboard/data_tab.rs
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use groundstation_shared::TelemetryRow;

use super::HISTORY_MS;

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
    let last_saved = use_signal(|| String::new());

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

    // Build graph polylines (8 series) + y-range + span
    let view_w = 1200.0_f64;
    let view_h = 360.0_f64;
    let view_h_full = 600.0_f64;
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

    let (paths, y_min, y_max, span_min) =
        build_polylines(&tab_rows, view_w as f32, view_h as f32);
    let (paths_full, _, _, _) = build_polylines(&tab_rows, view_w as f32, view_h_full as f32);
    let y_mid = (y_min + y_max) * 0.5;

    // Labels for cards and legend
    let labels = labels_for_datatype(&current);
    let legend_items: Vec<(usize, &'static str)> = labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| if l.is_empty() { None } else { Some((i, *l)) })
        .collect();
    let legend_w = 280.0_f64;
    let legend_row_h = 14.0_f64;
    let legend_h = (legend_items.len() as f64 * legend_row_h + 10.0)
        .min(140.0_f64)
        .max(24.0_f64);
    let legend_x = view_w - 20.0_f64 - legend_w;
    let legend_y = 20.0_f64;
    let legend_rows: Vec<(usize, &'static str, f64)> = legend_items
        .iter()
        .enumerate()
        .map(|(row, (i, label))| (*i, *label, legend_y + 10.0 + (row as f64) * legend_row_h))
        .collect();

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

            // -------- Big centered graph --------
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
                        svg {
                            style: "width:100%; height:auto; display:block; background:#020617; border-radius:14px; border:1px solid #334155;",
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

                            // Legend (inside graph box)
                            if !legend_items.is_empty() {
                                g {
                                    rect { x:"{legend_x}", y:"{legend_y}", width:"{legend_w}", height:"{legend_h}",
                                        rx:"6", ry:"6", fill:"#0b1220", stroke:"#1f2937"
                                    }
                                    for (i, label, y) in legend_rows.iter() {
                                        line { x1:"{legend_x + 10.0}", y1:"{y}", x2:"{legend_x + 46.0}", y2:"{y}",
                                            stroke:"{series_color(*i)}", stroke_width:"2", stroke_linecap:"round"
                                        }
                                        text { x:"{legend_x + 54.0}", y:"{y + 4.0}", fill:"#cbd5f5", "font-size":"12",
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

        if *is_fullscreen.read() {
            div { style: "position:fixed; inset:0; z-index:9998; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px;",
                div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px;",
                    h2 { style: "margin:0; color:#f97316;", "Data Graph" }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                        onclick: on_toggle_fullscreen,
                        "Exit Fullscreen"
                    }
                }
                div { style: "flex:1; min-height:0; width:100%;",
                    svg {
                        style: "width:100%; height:100%; display:block; background:#020617; border-radius:14px; border:1px solid #334155;",
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

                        // Legend (inside graph box)
                        if !legend_items.is_empty() {
                            g {
                                rect { x:"{legend_x}", y:"{legend_y}", width:"{legend_w}", height:"{legend_h}",
                                    rx:"6", ry:"6", fill:"#0b1220", stroke:"#1f2937"
                                }
                                for (i, label, y) in legend_rows.iter() {
                                    line { x1:"{legend_x + 10.0}", y1:"{y}", x2:"{legend_x + 46.0}", y2:"{y}",
                                        stroke:"{series_color(*i)}", stroke_width:"2", stroke_linecap:"round"
                                    }
                                    text { x:"{legend_x + 54.0}", y:"{y + 4.0}", fill:"#cbd5f5", "font-size":"12",
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

fn series_color(i: usize) -> &'static str {
    match i {
        0 => "#f97316",
        1 => "#22d3ee",
        2 => "#a3e635",
        _ => "#9ca3af",
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

fn labels_for_datatype(dt: &str) -> [&'static str; 8] {
    match dt {
        "GYRO_DATA" => ["Roll", "Pitch", "Yaw", "", "", "", "", ""],
        "ACCEL_DATA" => ["X Accel", "Y Accel", "Z Accel", "", "", "", "", ""],
        "BAROMETER_DATA" => ["Pressure", "Temp", "Altitude", "", "", "", "", ""],
        "BATTERY_VOLTAGE" => ["Voltage", "", "", "", "", "", "", ""],
        "BATTERY_CURRENT" => ["Current", "", "", "", "", "", "", ""],
        "KALMAN_FILTER_DATA" => ["X", "Y", "Z", "", "", "", "", ""],
        "GPS_DATA" => ["Latitude", "Longitude", "", "", "", "", "", ""],
        "FUEL_FLOW" => ["Flow Rate", "", "", "", "", "", "", ""],
        "FUEL_TANK_PRESSURE" => ["Pressure", "", "", "", "", "", "", ""],
        _ => ["", "", "", "", "", "", "", ""],
    }
}

/// Build eight SVG polyline point strings (v0..v7),
/// plus y-min, y-max, and span_minutes (0–HISTORY_MS).
///
/// `paths[i]` is `"x,y x,y x,y ..."`, suitable for `<polyline points=... />`.
fn build_polylines(rows: &[TelemetryRow], width: f32, height: f32) -> ([String; 8], f32, f32, f32) {
    if rows.is_empty() {
        return (std::array::from_fn(|_| String::new()), 0.0, 1.0, 0.0);
    }

    // 1) time window & span
    let newest_ts = rows.iter().map(|r| r.timestamp_ms).max().unwrap_or(0);
    let oldest_ts = rows.iter().map(|r| r.timestamp_ms).min().unwrap_or(newest_ts);

    let raw_span_ms = (newest_ts - oldest_ts).max(1);
    let effective_span_ms = raw_span_ms.min(HISTORY_MS);
    let span_minutes = effective_span_ms as f32 / 60_000.0;

    let window_start = newest_ts.saturating_sub(effective_span_ms);
    let window_end = newest_ts;

    // 2) rows in window
    let mut window_rows: Vec<&TelemetryRow> = rows
        .iter()
        .filter(|r| r.timestamp_ms >= window_start)
        .collect();
    if window_rows.is_empty() {
        return (std::array::from_fn(|_| String::new()), 0.0, 1.0, span_minutes);
    }
    window_rows.sort_by_key(|r| r.timestamp_ms);

    // 3) min/max across windowed rows
    let mut min_v: Option<f32> = None;
    let mut max_v: Option<f32> = None;

    for r in &window_rows {
        for x in [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7]
            .into_iter()
            .flatten()
        {
            min_v = Some(min_v.map(|m| m.min(x)).unwrap_or(x));
            max_v = Some(max_v.map(|m| m.max(x)).unwrap_or(x));
        }
    }

    let (min_v, mut max_v) = match (min_v, max_v) {
        (Some(a), Some(b)) => (a, b),
        _ => return (std::array::from_fn(|_| String::new()), 0.0, 1.0, span_minutes),
    };

    if (max_v - min_v).abs() < 1e-6 {
        max_v = min_v + 1.0;
    }

    // 4) plot geometry
    let left = 60.0;
    let right = width - 20.0;
    let top = 20.0;
    let bottom = height - 20.0;

    let plot_w = right - left;
    let plot_h = bottom - top;

    let map_y = |v: f32| bottom - ((v - min_v) / (max_v - min_v)) * plot_h;

    // 5) downsample into time buckets (stable across scroll pauses)
    let max_points: usize = 2000;

    #[derive(Default, Clone)]
    struct BucketAcc {
        v_sum: [f64; 8],
        v_count: [u64; 8],
    }

    let mut buckets = vec![BucketAcc::default(); max_points];
    let span_ms = (window_end - window_start).max(1) as f64;

    for r in &window_rows {
        let t = (r.timestamp_ms - window_start) as f64;
        let mut bi = ((t / span_ms) * (max_points as f64 - 1.0)).floor() as isize;
        if bi < 0 {
            bi = 0;
        }
        if bi as usize >= max_points {
            bi = (max_points - 1) as isize;
        }
        let b = &mut buckets[bi as usize];

        let vals = [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7];
        for (j, opt) in vals.iter().enumerate() {
            if let Some(x) = opt {
                b.v_sum[j] += *x as f64;
                b.v_count[j] += 1;
            }
        }
    }

    // 6) build polyline strings with time-based x-spacing
    let mut out: [String; 8] = std::array::from_fn(|_| String::new());

    for (idx, b) in buckets.iter().enumerate() {
        let t = if max_points > 1 {
            idx as f32 / (max_points as f32 - 1.0)
        } else {
            0.0
        };
        let x = left + plot_w * t;

        for ch in 0..8usize {
            if b.v_count[ch] > 0 {
                let v = (b.v_sum[ch] / b.v_count[ch] as f64) as f32;
                let y = map_y(v);
                let s = &mut out[ch];
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format!("{x:.2},{y:.2}"));
            }
        }
    }

    (out, min_v, max_v, span_minutes)
}
