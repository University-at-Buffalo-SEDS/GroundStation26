use dioxus::prelude::*;
use dioxus_signals::Signal;
use groundstation_shared::BoardStatusEntry;
use crate::telemetry_dashboard::chart::build_time_polyline;
use std::collections::HashMap;

const LATENCY_WINDOW_MS: i64 = 20 * 60_000;
const LATENCY_MAX_POINTS: usize = 2000;

#[component]
pub fn ConnectionStatusTab(boards: Signal<Vec<BoardStatusEntry>>) -> Element {
    let mut show_board = use_signal(|| true);
    let mut board_fullscreen = use_signal(|| false);
    let mut show_latency = use_signal(|| true);
    let mut latency_fullscreen = use_signal(|| false);
    let history = use_signal(HashMap::<String, Vec<(i64, f64)>>::new);

    {
        let boards = boards;
        let mut history = history;
        use_effect(move || {
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

    let on_scroll = {
        let boards = boards;
        let mut history = history;
        move |_| {
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
        }
    };

    rsx! {
        div { style: "padding:16px; flex:1; min-height:0; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:touch;",
            onscroll: on_scroll,
            h2 { style: "margin:0 0 12px 0;", "Connection Status" }
            div { style: "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
                div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px; margin-bottom:8px;",
                    div { style: "font-size:14px; color:#94a3b8;", "Board Status" }
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

            div { style: "margin-top:16px; padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
                div { style: "display:flex; align-items:center; justify-content:space-between; gap:12px; margin-bottom:8px;",
                    div { style: "font-size:14px; color:#94a3b8;", "Packet Age (ms)" }
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
                                {render_latency_chart(history.read().get(&entry.sender_id))}
                            }
                        }
                    }
                }
            }
        }

        if *board_fullscreen.read() {
            div { style: "position:fixed; inset:0; z-index:9998; padding:16px; background:#020617; display:flex; flex-direction:column; gap:12px; overflow:auto;",
                onscroll: on_scroll,
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
                onscroll: on_scroll,
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
                            {render_latency_chart(history.read().get(&entry.sender_id))}
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
        return SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
    }
}

fn render_latency_chart(points: Option<&Vec<(i64, f64)>>) -> Element {
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
    let height = 360.0_f64;
    let (poly, y_min, y_max, span_min) =
        build_time_polyline(points.as_slice(), width, height, Some(LATENCY_WINDOW_MS));
    if poly.is_empty() {
        return rsx! {
            div { style: "color:#64748b; font-size:12px;", "Collecting…" }
        };
    }

    rsx! {
        svg {
            style: "width:100%; height:auto; display:block; background:#020617; border-radius:10px; border:1px solid #1f2937;",
            view_box: "0 0 {width} {height}",

            // gridlines
            for i in 1..=5 {
                line {
                    x1:"60", y1:"{20.0 + (320.0 / 6.0) * (i as f64)}",
                    x2:"1180", y2:"{20.0 + (320.0 / 6.0) * (i as f64)}",
                    stroke: "#1f2937",
                    "stroke-width": "1"
                }
            }
            for i in 1..=5 {
                line {
                    x1:"{60.0 + (1120.0 / 6.0) * (i as f64)}", y1:"20",
                    x2:"{60.0 + (1120.0 / 6.0) * (i as f64)}", y2:"340",
                    stroke: "#1f2937",
                    "stroke-width": "1"
                }
            }

            // axes
            line { x1:"60", y1:"340", x2:"1180", y2:"340", stroke:"#334155", "stroke-width":"1" }
            line { x1:"60", y1:"20",  x2:"60",   y2:"340", stroke:"#334155", "stroke-width":"1" }

            // y labels
            text { x:"10", y:"26", fill:"#94a3b8", "font-size":"10", {format!("{y_max}")} }
            text { x:"10", y:"184", fill:"#94a3b8", "font-size":"10", {format!("{}", (y_min + y_max) / 2f64)} }
            text { x:"10", y:"344", fill:"#94a3b8", "font-size":"10", {format!("{y_min}")} }

            // x labels (span in minutes)
            text { x:"70",   y:"355", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min)} }
            text { x:"600",  y:"355", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min * 0.5)} }
            text { x:"1120", y:"355", fill:"#94a3b8", "font-size":"10", "now" }

            polyline {
                points: "{poly}",
                fill: "none",
                stroke: "#22d3ee",
                "stroke-width": "2",
                "stroke-linejoin": "round",
                "stroke-linecap": "round",
            }
        }
    }
}

fn render_board_table(boards: &Vec<BoardStatusEntry>) -> Element {
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
