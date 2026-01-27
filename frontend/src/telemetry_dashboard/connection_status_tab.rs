use dioxus::prelude::*;
use dioxus_signals::Signal;
use groundstation_shared::BoardStatusEntry;
use std::collections::HashMap;

#[derive(Clone)]
struct LatencyPoint {
    t_ms: i64,
    age_ms: i64,
}

#[component]
pub fn ConnectionStatusTab(boards: Signal<Vec<BoardStatusEntry>>) -> Element {
    let mut show_board = use_signal(|| true);
    let mut board_fullscreen = use_signal(|| false);
    let mut show_latency = use_signal(|| true);
    let mut latency_fullscreen = use_signal(|| false);
    let history = use_signal(HashMap::<String, Vec<LatencyPoint>>::new);

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
                list.push(LatencyPoint {
                    t_ms: now_ms,
                    age_ms: age_ms as i64,
                });
                if list.len() > 240 {
                    let drain = list.len() - 240;
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

    rsx! {
        div { style: "padding:16px;",
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

fn render_latency_chart(points: Option<&Vec<LatencyPoint>>) -> Element {
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
    let pad_l = 60.0;
    let pad_r = 20.0;
    let pad_t = 20.0;
    let pad_b = 20.0;

    let t_min = points.first().unwrap().t_ms;
    let t_max = points.last().unwrap().t_ms;
    let t_span = (t_max - t_min).max(1) as f64;
    let (y_min, y_max) = points.iter().fold((i64::MAX, i64::MIN), |(mn, mx), p| {
        (mn.min(p.age_ms), mx.max(p.age_ms))
    });
    let mut y_span = (y_max - y_min) as f64;
    if y_span < 1.0 {
        y_span = 1.0;
    }

    let inner_w = width - pad_l - pad_r;
    let inner_h = height - pad_t - pad_b;

    let to_xy = |t: i64, y: i64| -> (f64, f64) {
        let x = pad_l + ((t - t_min) as f64 / t_span) * inner_w;
        let y_norm = (y - y_min) as f64 / y_span;
        let y_px = pad_t + (1.0 - y_norm) * inner_h;
        (x, y_px)
    };

    let mut poly = String::new();
    for (i, p) in points.iter().enumerate() {
        let (x, y) = to_xy(p.t_ms, p.age_ms);
        if i == 0 {
            poly.push_str(&format!("{x:.2},{y:.2}"));
        } else {
            poly.push_str(&format!(" {x:.2},{y:.2}"));
        }
    }

    let span_min = (t_span / 60_000.0).max(0.0);

    rsx! {
        svg {
            style: "width:100%; height:auto; display:block; background:#020617; border-radius:10px; border:1px solid #1f2937;",
            view_box: "0 0 {width} {height}",

            // gridlines
            for i in 1..=5 {
                line {
                    x1: "{pad_l}",
                    y1: "{pad_t + (inner_h / 6.0) * (i as f64)}",
                    x2: "{width - pad_r}",
                    y2: "{pad_t + (inner_h / 6.0) * (i as f64)}",
                    stroke: "#1f2937",
                    "stroke-width": "1"
                }
            }
            for i in 1..=5 {
                line {
                    x1: "{pad_l + (inner_w / 6.0) * (i as f64)}",
                    y1: "{pad_t}",
                    x2: "{pad_l + (inner_w / 6.0) * (i as f64)}",
                    y2: "{height - pad_b}",
                    stroke: "#1f2937",
                    "stroke-width": "1"
                }
            }

            // axes
            line { x1:"{pad_l}", y1:"{height - pad_b}", x2:"{width - pad_r}", y2:"{height - pad_b}", stroke:"#334155", "stroke-width":"1" }
            line { x1:"{pad_l}", y1:"{pad_t}", x2:"{pad_l}", y2:"{height - pad_b}", stroke:"#334155", "stroke-width":"1" }

            // y labels
            text { x:"10", y:"26", fill:"#94a3b8", "font-size":"10", {format!("{y_max}")} }
            text { x:"10", y:"184", fill:"#94a3b8", "font-size":"10", {format!("{}", (y_min + y_max) / 2)} }
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
                        if let Some(ts) = entry.last_seen_ms { "{ts}" } else { "—" }
                    }
                    div { style: "padding:8px; border-bottom:1px solid #1f2937;",
                        if let Some(age) = entry.age_ms { "{age}" } else { "—" }
                    }
                }
            }
        }
    }
}
