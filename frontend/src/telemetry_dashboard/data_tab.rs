// frontend/src/telemetry_dashboard/data_tab.rs
use dioxus::prelude::*;
use dioxus_signals::{ReadableExt, Signal, WritableExt};
use groundstation_shared::TelemetryRow;

use super::data_chart::{
    charts_cache_get, charts_cache_get_channel_minmax, labels_for_datatype, series_color,
};

const _ACTIVE_TAB_STORAGE_KEY: &str = "gs26_active_tab";

#[cfg(target_arch = "wasm32")]
fn localstorage_get(key: &str) -> Option<String> {
    use web_sys::window;
    let w = window()?;
    let ls = w.local_storage().ok()??;
    ls.get_item(key).ok().flatten()
}

#[cfg(not(target_arch = "wasm32"))]
fn target_frame_duration() -> std::time::Duration {
    // Default 240fps; override with GS_UI_FPS=60 etc.
    let fps: u64 = std::env::var("GS_UI_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);

    let fps = fps.clamp(1, 480);
    std::time::Duration::from_micros(1_000_000 / fps)
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

    // Cached unique data types list (avoid O(n) scan every render)
    let types_cache = use_signal(Vec::<String>::new);
    let types_cache_len = use_signal(|| 0usize);

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

    // Update cached type list only when rows length changes
    use_effect({
        let rows = rows;
        let mut types_cache = types_cache;
        let mut types_cache_len = types_cache_len;
        move || {
            let len = rows.read().len();
            if len == *types_cache_len.read() {
                return;
            }
            types_cache_len.set(len);
            let mut types: Vec<String> = rows.read().iter().map(|r| r.data_type.clone()).collect();
            types.sort();
            types.dedup();
            types_cache.set(types);
        }
    });

    // ------------------------------------------------------------
    // Redraw driver (START ONCE)
    // - wasm32: requestAnimationFrame
    // - native: ~timer (GS_UI_FPS)
    // ------------------------------------------------------------
    let redraw_tick = use_signal(|| 0u64);
    let started_redraw = use_signal(|| false);

    use_effect({
        let mut redraw_tick = redraw_tick;
        let mut started_redraw = started_redraw;

        move || {
            if *started_redraw.read() {
                return;
            }
            started_redraw.set(true);

            #[cfg(target_arch = "wasm32")]
            {
                use std::cell::RefCell;
                use std::rc::Rc;
                use wasm_bindgen::closure::Closure;
                use wasm_bindgen::JsCast;

                let cb: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
                let cb2 = cb.clone();

                *cb2.borrow_mut() = Some(Closure::wrap(Box::new(move |_ts: f64| {
                    let next = redraw_tick.read().wrapping_add(1);
                    redraw_tick.set(next);

                    if let Some(win) = web_sys::window() {
                        let _ = win.request_animation_frame(
                            cb.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
                        );
                    }
                }) as Box<dyn FnMut(f64)>));

                if let Some(win) = web_sys::window() {
                    let _ = win.request_animation_frame(
                        cb2.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
                    );
                }

                std::mem::forget(cb2);
            }

            #[cfg(not(target_arch = "wasm32"))]
            {
                let frame = target_frame_duration();
                spawn(async move {
                    loop {
                        tokio::time::sleep(frame).await;
                        let next = redraw_tick.read().wrapping_add(1);
                        redraw_tick.set(next);
                    }
                });
            }
        }
    });

    // Force rerender when redraw driver ticks
    let _ = *redraw_tick.read();

    // Collect unique data types (for buttons)
    let types = types_cache.read().clone();
    let current = active_tab.read().clone();

    // Latest row for summary cards (scan backward; no sort/filter allocations)
    let latest_row = rows
        .read()
        .iter()
        .rev()
        .find(|r| r.data_type == current)
        .cloned();

    let is_valve_state = current == "VALVE_STATE";
    let has_telemetry = latest_row.is_some();
    let is_graph_allowed = has_telemetry && current != "GPS_DATA" && !is_valve_state;

    // Labels for cards and legend
    let labels = labels_for_datatype(&current);

    // Viewport constants
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

    // Cache fetch (NON-FULLSCREEN)
    //
    // IMPORTANT: We do NOT fetch fullscreen geometry here anymore.
    // That avoids doing two cache builds every frame (w,h=360 and w,h=full),
    // which can interact badly with "window span" behavior and costs extra CPU.
    let (paths, y_min, y_max, span_min) = charts_cache_get(&current, view_w as f32, view_h as f32);
    let (chan_min, chan_max) =
        charts_cache_get_channel_minmax(&current, view_w as f32, view_h as f32);

    let y_mid = (y_min + y_max) * 0.5;

    let y_max_s = format!("{:.2}", y_max);
    let y_mid_s = format!("{:.2}", y_mid);
    let y_min_s = format!("{:.2}", y_min);

    let x_left_s = fmt_span(span_min);
    let x_mid_s = fmt_span(span_min * 0.5);

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
        div {
            style: "padding:16px; height:100%; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:auto; display:flex; flex-direction:column; gap:12px; padding-bottom:10px;",

            div { style: "display:flex; flex-direction:column; gap:10px;",

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

                match latest_row {
                    None => rsx! {
                        div { style: "color:#94a3b8; padding:2px 2px;", "Waiting for telemetry…" }
                    },
                    Some(row) => {
                        let vals = [row.v0, row.v1, row.v2, row.v3, row.v4, row.v5, row.v6, row.v7];

                        rsx! {
                            div {
                                style: "display:grid; gap:10px; align-items:stretch; grid-template-columns:repeat(auto-fit, minmax(110px, 1fr)); width:100%;",
                                for i in 0..8usize {
                                    if !labels[i].is_empty() {
                                        SummaryCard {
                                            label: labels[i],
                                            min: if is_graph_allowed { chan_min[i].map(|v| format!("{v:.4}")) } else { None },
                                            max: if is_graph_allowed { chan_max[i].map(|v| format!("{v:.4}")) } else { None },
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
                                svg {
                                    style: "width:100%; height:auto; display:block; max-width:100%;",
                                    view_box: "0 0 {view_w} {view_h}",

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

                                    line { x1:"{left}", y1:"{pad_top}", x2:"{left}",  y2:"{view_h - pad_bottom}", stroke:"#334155", "stroke-width":"1" }
                                    line { x1:"{left}", y1:"{view_h - pad_bottom}", x2:"{right}", y2:"{view_h - pad_bottom}", stroke:"#334155", "stroke-width":"1" }

                                    text { x:"10", y:"{pad_top + 6.0}", fill:"#94a3b8", "font-size":"10", {y_max_s.clone()} }
                                    text { x:"10", y:"{pad_top + inner_h / 2.0 + 4.0}", fill:"#94a3b8", "font-size":"10", {y_mid_s.clone()} }
                                    text { x:"10", y:"{view_h - pad_bottom + 4.0}", fill:"#94a3b8", "font-size":"10", {y_min_s.clone()} }

                                    text { x:"{left + 10.0}",  y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {x_left_s.clone()} }
                                    text { x:"{view_w * 0.5}", y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {x_mid_s.clone()} }
                                    text { x:"{right - 60.0}", y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", "now" }

                                    for i in 0..8usize {
                                        if !paths[i].is_empty() {
                                            path {
                                                d: "{paths[i]}",
                                                fill: "none",
                                                stroke: "{series_color(i)}",
                                                "stroke-width": "2",
                                                "stroke-linejoin": "round",
                                                "stroke-linecap": "round",
                                            }
                                        }
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
                let (paths_full, _y_min2, _y_max2, _span_min2) =
                    charts_cache_get(&current, view_w as f32, view_h_full as f32);

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
                            svg {
                                style: "width:100%; height:auto; display:block;",
                                view_box: "0 0 {view_w} {view_h_full}",
                                preserve_aspect_ratio: "none",

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

                                line { x1:"{left}", y1:"{pad_top}", x2:"{left}", y2:"{view_h_full - pad_bottom}", stroke:"#334155", "stroke-width":"1" }
                                line { x1:"{left}", y1:"{view_h_full - pad_bottom}", x2:"{right}", y2:"{view_h_full - pad_bottom}", stroke:"#334155", "stroke-width":"1" }

                                text { x:"10", y:"{pad_top + 6.0}", fill:"#94a3b8", "font-size":"10", {y_max_s.clone()} }
                                text { x:"10", y:"{pad_top + inner_h_full / 2.0 + 4.0}", fill:"#94a3b8", "font-size":"10", {y_mid_s.clone()} }
                                text { x:"10", y:"{view_h_full - pad_bottom + 4.0}", fill:"#94a3b8", "font-size":"10", {y_min_s.clone()} }

                                text { x:"{left + 10.0}",  y:"{view_h_full - 5.0}", fill:"#94a3b8", "font-size":"10", {x_left_s.clone()} }
                                text { x:"{view_w * 0.5}", y:"{view_h_full - 5.0}", fill:"#94a3b8", "font-size":"10", {x_mid_s.clone()} }
                                text { x:"{right - 60.0}", y:"{view_h_full - 5.0}", fill:"#94a3b8", "font-size":"10", "now" }

                                for i in 0..8usize {
                                    if !paths_full[i].is_empty() {
                                        path {
                                            d: "{paths_full[i]}",
                                            fill: "none",
                                            stroke: "{series_color(i)}",
                                            "stroke-width": "2",
                                            "stroke-linejoin": "round",
                                            "stroke-linecap": "round",
                                        }
                                    }
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

#[component]
fn SummaryCard(
    label: &'static str,
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
