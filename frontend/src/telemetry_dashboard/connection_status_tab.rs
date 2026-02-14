use dioxus::prelude::*;
use dioxus_signals::Signal;
use std::collections::HashMap;

use super::layout::{ConnectionSectionKind, ConnectionTabLayout};
use super::types::BoardStatusEntry;

const LATENCY_WINDOW_MS: i64 = 20 * 60_000;
const LATENCY_MAX_POINTS: usize = 2000;

const SCROLL_TRIGGER_THRESHOLD_MS: i64 = 200;

#[component]
pub fn ConnectionStatusTab(
    boards: Signal<Vec<BoardStatusEntry>>,
    layout: ConnectionTabLayout,
) -> Element {
    let mut show_board = use_signal(|| true);
    let mut board_fullscreen = use_signal(|| false);
    let mut show_latency = use_signal(|| true);
    let mut latency_fullscreen = use_signal(|| false);
    let history = use_signal(HashMap::<String, Vec<(i64, f64)>>::new);

    {
        let boards = boards;
        let mut history = history;
        let show_latency = show_latency;
        let latency_fullscreen = latency_fullscreen;
        use_effect(move || {
            spawn(async move {
                loop {
                    if !*show_latency.read() && !*latency_fullscreen.read() {
                        #[cfg(target_arch = "wasm32")]
                        gloo_timers::future::TimeoutFuture::new(50).await;

                        #[cfg(not(target_arch = "wasm32"))]
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }

                    let now_ms = js_now_ms();
                    let mut map = history.read().clone();

                    for entry in boards.read().iter() {
                        let Some(age_ms) = entry.age_ms else {
                            continue;
                        };
                        let key = entry.sender_id.clone();
                        let list = map.entry(key).or_default();
                        list.push((now_ms, age_ms as f64));
                        if let Some(&(newest, _)) = list.last() {
                            let cutoff = newest.saturating_sub(LATENCY_WINDOW_MS);
                            let split = list.partition_point(|(t, _)| *t < cutoff);
                            if split > 0 {
                                list.drain(0..split);
                            }
                        }
                        if list.len() > LATENCY_MAX_POINTS {
                            let drain = list.len() - LATENCY_MAX_POINTS;
                            list.drain(0..drain);
                        }
                    }

                    history.set(map);

                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(50).await;

                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            });
        });
    }

    let toggle_latency = move |_| {
        let next = !*show_latency.read();
        show_latency.set(next);
    };
    let toggle_board = move |_| {
        let next = !*show_board.read();
        show_board.set(next);
    };
    let toggle_board_fullscreen = move |_| {
        let next = !*board_fullscreen.read();
        board_fullscreen.set(next);
    };
    let toggle_latency_fullscreen = move |_| {
        let next = !*latency_fullscreen.read();
        latency_fullscreen.set(next);
    };

    rsx! {
        div { style: "padding:16px; height:100%; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:auto;",
            h2 { style: "margin:0 0 12px 0;", "Connection Status" }
            for (idx, section) in layout.sections.iter().enumerate() {
                match section.kind {
                    ConnectionSectionKind::BoardStatus => rsx! {
                        div { style: {
                                let top_margin = if idx == 0 { "" } else { "margin-top:16px;" };
                                format!(
                                    "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;{}",
                                    top_margin
                                )
                            },
                            div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px; margin-bottom:8px;",
                                div { style: "font-size:14px; color:#94a3b8;", "{section.title.clone().unwrap_or_else(|| \"Board Status\".to_string())}" }
                                div { style: "display:flex; gap:8px; flex-wrap:wrap;",
                                    button {
                                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                                        onclick: toggle_board,
                                        if *show_board.read() { "Collapse" } else { "Expand" }
                                    }
                                    button {
                                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                                        onclick: toggle_board_fullscreen,
                                        "Fullscreen"
                                    }
                                }
                            }
                            if *show_board.read() {
                                {render_board_table(&boards.read())}
                            }
                        }
                    },
                    ConnectionSectionKind::Latency => rsx! {
                        div { style: {
                                let top_margin = if idx == 0 { "" } else { "margin-top:16px;" };
                                format!(
                                    "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;{}",
                                    top_margin
                                )
                            },
                            div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px; margin-bottom:8px;",
                                div { style: "font-size:14px; color:#94a3b8;", "{section.title.clone().unwrap_or_else(|| \"Packet Age (ms)\".to_string())}" }
                                div { style: "display:flex; gap:8px; flex-wrap:wrap;",
                                    button {
                                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                                        onclick: toggle_latency,
                                        if *show_latency.read() { "Collapse" } else { "Expand" }
                                    }
                                    button {
                                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                                        onclick: toggle_latency_fullscreen,
                                        "Fullscreen"
                                    }
                                }
                            }

                            if *show_latency.read() {
                                div { style: "display:flex; flex-direction:column; gap:10px;",
                                    for entry in boards.read().iter() {
                                        div { style: "padding:10px; border:1px solid #1f2937; border-radius:10px; background:#020617;",
                                            div { style: "font-size:12px; color:#94a3b8; margin-bottom:6px;",
                                                "{entry.board.as_str()} ({entry.sender_id})"
                                            }
                                            {render_latency_chart(history.read().get(&entry.sender_id), 360.0_f64)}
                                        }
                                    }
                                }
                            }
                        }
                    },
                }
            }
        }

        if *board_fullscreen.read() {
            div { style: "position:fixed; inset:0; z-index:9998; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px; overflow:auto;",
                div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px;",
                    h2 { style: "margin:0; color:#e2e8f0;", "Board Status" }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                        onclick: toggle_board_fullscreen,
                        "Exit Fullscreen"
                    }
                }
                {render_board_table(&boards.read())}
            }
        }

        if *latency_fullscreen.read() {
            div { style: "position:fixed; inset:0; z-index:9998; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px; overflow:auto;",
                div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px;",
                    h2 { style: "margin:0; color:#e2e8f0;", "Packet Age (ms)" }
                    button {
                        style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; font-size:0.85rem; cursor:pointer;",
                        onclick: toggle_latency_fullscreen,
                        "Exit Fullscreen"
                    }
                }
                div { style: "display:flex; flex-direction:column; gap:10px;",
                    for entry in boards.read().iter() {
                        div { style: "padding:10px; border:1px solid #1f2937; border-radius:10px; background:#020617;",
                            div { style: "font-size:12px; color:#94a3b8; margin-bottom:6px;",
                                "{entry.board.as_str()} ({entry.sender_id})"
                            }
                            {render_latency_chart(
                                history.read().get(&entry.sender_id),
                                fullscreen_latency_height(boards.read().len()),
                            )}
                        }
                    }
                }
            }
        }
    }
}

fn js_now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        return js_sys::Date::now() as i64;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

fn render_latency_chart(points: Option<&Vec<(i64, f64)>>, height: f64) -> Element {
    let Some(points) = points else {
        return rsx! {
            div { style: "color:#64748b; font-size:12px;", "No data yet" }
        };
    };

    if points.len() < 2 {
        return rsx! {
            div { style: "color:#64748b; font-size:12px;", "Collecting…" }
        };
    }

    let width = 1200.0_f64;
    let left = 60.0_f64;
    let right = width - 20.0_f64;
    let pad_top = 20.0_f64;
    let pad_bottom = 20.0_f64;
    let inner_w = right - left;
    let inner_h = height - pad_top - pad_bottom;
    let grid_x_step = inner_w / 6.0_f64;
    let grid_y_step = inner_h / 6.0_f64;
    let (solid, dotted, y_min, y_max, span_min) =
        build_latency_polylines(points.as_slice(), width, height, Some(LATENCY_WINDOW_MS));
    if solid.is_empty() && dotted.is_empty() {
        return rsx! {
            div { style: "color:#64748b; font-size:12px;", "Collecting…" }
        };
    }

    rsx! {
        div { style: "display:flex; flex-direction:column;",
            svg {
                style: "width:100%; height:auto; display:block; background:#020617; border-radius:10px; border:1px solid #1f2937;",
                view_box: "0 0 {width} {height}",

                // gridlines
                for i in 1..=5 {
                    line {
                        x1:"{left}", y1:"{pad_top + grid_y_step * (i as f64)}",
                        x2:"{right}", y2:"{pad_top + grid_y_step * (i as f64)}",
                        stroke: "#1f2937",
                        "stroke-width": "1"
                    }
                }
                for i in 1..=5 {
                    line {
                        x1:"{left + grid_x_step * (i as f64)}", y1:"{pad_top}",
                        x2:"{left + grid_x_step * (i as f64)}", y2:"{height - pad_bottom}",
                        stroke: "#1f2937",
                        "stroke-width": "1"
                    }
                }

                // axes
                line { x1:"{left}", y1:"{height - pad_bottom}", x2:"{right}", y2:"{height - pad_bottom}", stroke:"#334155", "stroke-width":"1" }
                line { x1:"{left}", y1:"{pad_top}",  x2:"{left}",   y2:"{height - pad_bottom}", stroke:"#334155", "stroke-width":"1" }

                // y labels
                text { x:"10", y:"{pad_top + 6.0}", fill:"#94a3b8", "font-size":"10", {format!("{y_max}")} }
                text { x:"10", y:"{pad_top + inner_h / 2.0 + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{}", (y_min + y_max) / 2f64)} }
                text { x:"10", y:"{height - pad_bottom + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{y_min}")} }

                // x labels (span in minutes)
                text { x:"{left + 10.0}",   y:"{height - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min)} }
                text { x:"{width * 0.5}",  y:"{height - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min * 0.5)} }
                text { x:"{right - 60.0}", y:"{height - 5.0}", fill:"#94a3b8", "font-size":"10", "now" }

                for pts in solid.iter() {
                    if !pts.is_empty() {
                        polyline {
                            points: "{pts}",
                            fill: "none",
                            stroke: "#22d3ee",
                            "stroke-width": "2",
                            "stroke-linejoin": "round",
                            "stroke-linecap": "round",
                        }
                    }
                }
                for pts in dotted.iter() {
                    if !pts.is_empty() {
                        polyline {
                            points: "{pts}",
                            fill: "none",
                            stroke: "#fbbf24",
                            "stroke-width": "2",
                            stroke_dasharray: "4 4",
                            "stroke-linejoin": "round",
                            "stroke-linecap": "round",
                        }
                    }
                }
            }
            div { style: "margin-top:8px; display:flex; gap:12px; align-items:center; font-size:12px; color:#cbd5f5;",
                div { style: "display:flex; align-items:center; gap:6px;",
                    svg { width:"26", height:"8", view_box:"0 0 26 8",
                        line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"#22d3ee", stroke_width:"2", stroke_linecap:"round" }
                    }
                    "Actual"
                }
                div { style: "display:flex; align-items:center; gap:6px;",
                    svg { width:"26", height:"8", view_box:"0 0 26 8",
                        line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"#fbbf24", stroke_width:"2", stroke_dasharray:"4 4", stroke_linecap:"round" }
                    }
                    "Interpolated"
                }
            }
        }
    }
}

fn fullscreen_latency_height(_count: usize) -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        let h = web_sys::window()
            .and_then(|w| w.inner_height().ok())
            .and_then(|v| v.as_f64())
            .unwrap_or(700.0);
        let header = 80.0;
        let padding = 32.0;
        let gap = 10.0;
        let n = _count.max(1) as f64;
        let available = (h - header - padding - gap * (n - 1.0)).max(260.0);
        (available / n).max(220.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        360.0
    }
}

fn build_latency_polylines(
    points: &[(i64, f64)],
    width: f64,
    height: f64,
    window_ms: Option<i64>,
) -> (Vec<String>, Vec<String>, f64, f64, f64) {
    if points.len() < 2 {
        return (Vec::new(), Vec::new(), 0.0, 0.0, 0.0);
    }

    let mut pts: Vec<(i64, f64)> = points.to_vec();
    pts.sort_by_key(|(t, _)| *t);

    if let Some(win) = window_ms
        && let Some(&(newest, _)) = pts.last()
    {
        let start = newest.saturating_sub(win);
        let first_in = pts.partition_point(|(t, _)| *t < start);
        if first_in > 0 {
            pts.drain(0..first_in);
        }
    }

    if pts.len() < 2 {
        return (Vec::new(), Vec::new(), 0.0, 0.0, 0.0);
    }

    let (t_min, t_max) = pts.iter().fold((i64::MAX, i64::MIN), |(mn, mx), (t, _)| {
        (mn.min(*t), mx.max(*t))
    });
    let (y_min, y_max) = pts
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(mn, mx), (_, y)| {
            (mn.min(*y), mx.max(*y))
        });

    let t_span = (t_max - t_min).max(1) as f64;
    let mut y_span = y_max - y_min;
    if !y_span.is_finite() || y_span.abs() < 1e-9 {
        y_span = 1.0;
    }

    let pad_l = 60.0;
    let pad_r = 20.0;
    let pad_t = 20.0;
    let pad_b = 20.0;
    let inner_w = width - pad_l - pad_r;
    let inner_h = height - pad_t - pad_b;

    let to_xy = |t: i64, y: f64| -> (f64, f64) {
        let x = pad_l + ((t - t_min) as f64 / t_span) * inner_w;
        let y_norm = (y - y_min) / y_span;
        let y_px = pad_t + (1.0 - y_norm) * inner_h;
        (x, y_px)
    };

    // Detect large gaps (scroll pauses) and only interpolate those.
    let mut deltas: Vec<i64> = pts.windows(2).map(|w| (w[1].0 - w[0].0).max(0)).collect();
    deltas.sort_unstable();
    let median_dt = if deltas.is_empty() {
        0
    } else {
        deltas[deltas.len() / 2]
    };
    let gap_threshold_ms = median_dt.saturating_mul(5).max(SCROLL_TRIGGER_THRESHOLD_MS);

    let mut solid: Vec<String> = Vec::new();
    let mut dotted: Vec<String> = Vec::new();
    let mut cur_solid = String::new();

    for (idx, (t, y)) in pts.iter().enumerate() {
        let (x, yy) = to_xy(*t, *y);
        if idx > 0 {
            let (pt, py) = pts[idx - 1];
            let dt = (*t - pt).max(0);
            if dt > gap_threshold_ms {
                if !cur_solid.is_empty() {
                    solid.push(std::mem::take(&mut cur_solid));
                }
                let (x0, y0) = to_xy(pt, py);
                dotted.push(format!("{x0:.2},{y0:.2} {x:.2},{yy:.2}"));
            }
        }

        if !cur_solid.is_empty() {
            cur_solid.push(' ');
        }
        cur_solid.push_str(&format!("{x:.2},{yy:.2}"));
    }

    if !cur_solid.is_empty() {
        solid.push(cur_solid);
    }

    let span_min = t_span / 60_000.0;
    (solid, dotted, y_min, y_max, span_min)
}

fn render_board_table(boards: &[BoardStatusEntry]) -> Element {
    if boards.is_empty() {
        return rsx! {
            div { style: "color:#94a3b8;", "No board status yet." }
        };
    }

    rsx! {
        div { style: "border:1px solid #1f2937; border-radius:10px; overflow:hidden;",
            div { style: "display:grid; grid-template-columns: 1.1fr 1.1fr 0.7fr 1fr 1fr; font-size:13px; color:#cbd5f5;",
                div { style: "font-weight:600; color:#e2e8f0; padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;", "Board" }
                div { style: "font-weight:600; color:#e2e8f0; padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;", "Sender ID" }
                div { style: "font-weight:600; color:#e2e8f0; padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;", "Seen" }
                div { style: "font-weight:600; color:#e2e8f0; padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;", "Last Seen (ms)" }
                div { style: "font-weight:600; color:#e2e8f0; padding:8px; border-bottom:1px solid #1f2937;", "Age (ms)" }

                for entry in boards.iter() {
                    div { style: "padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;", "{entry.board.as_str()}" }
                    div { style: "padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;", "{entry.sender_id}" }
                    div { style: "padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;", if entry.seen { "yes" } else { "no" } }
                    div { style: "padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;",
                        "{format_last_seen(entry.last_seen_ms)}"
                    }
                    div { style: "padding:8px; border-bottom:1px solid #1f2937;",
                        if let Some(age) = entry.age_ms { "{age}" } else { "—" }
                    }
                }
            }
        }
    }
}

fn format_last_seen(last_seen_ms: Option<u64>) -> String {
    let Some(ts) = last_seen_ms else {
        return "—".to_string();
    };

    // Heuristic: if it's Unix-epoch ms (>= ~2017-07-14), render human time.
    if ts >= 1_500_000_000_000 {
        #[cfg(target_arch = "wasm32")]
        {
            let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64));
            return d.to_string().into();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::time::{Duration, UNIX_EPOCH};
            let t = UNIX_EPOCH + Duration::from_millis(ts);
            let dt: chrono::DateTime<chrono::Local> = t.into();
            return dt.format("%Y-%m-%d %H:%M:%S").to_string();
        }
    }

    format!("{ts} ms")
}
