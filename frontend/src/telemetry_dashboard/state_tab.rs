// frontend/src/telemetry_dashboard/state_tab.rs

use dioxus::prelude::*;
use dioxus_signals::Signal;
use groundstation_shared::{BoardStatusEntry, FlightState, TelemetryRow};

use crate::telemetry_dashboard::data_chart::{
    charts_cache_get, charts_cache_get_channel_minmax, labels_for_datatype, series_color,
};
use crate::telemetry_dashboard::map_tab::MapTab;

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

#[component]
pub fn StateTab(
    flight_state: Signal<FlightState>,
    rows: Signal<Vec<TelemetryRow>>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Element {
    // ------------------------------------------------------------
    // Redraw driver for charts on State tab
    // ------------------------------------------------------------
    let redraw_tick = use_signal(|| 0u64);

    use_effect({
        let mut redraw_tick = redraw_tick;

        move || {
            #[cfg(target_arch = "wasm32")]
            {
                use std::cell::RefCell;
                use std::rc::Rc;
                use wasm_bindgen::JsCast;
                use wasm_bindgen::closure::Closure;

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

    let state = *flight_state.read();
    let rows_snapshot = rows.read();
    let boards_snapshot = board_status.read();

    let content = match state {
        FlightState::Startup => rsx! {
            Section { title: "Connected Devices",
                {board_status_table(&boards_snapshot)}
            }
        },

        FlightState::PreFill
        | FlightState::FillTest
        | FlightState::NitrogenFill
        | FlightState::NitrousFill => rsx! {
            Section { title: "Pressure",
                {summary_row(&rows_snapshot, "FUEL_TANK_PRESSURE", &[("Tank Pressure", 0)])}
                {data_style_chart_cached("FUEL_TANK_PRESSURE", 1200.0, 260.0, Some("Fuel Tank Pressure"))}
            }
            Section { title: "Valve States",
                {valve_state_grid(&rows_snapshot)}
            }
            {action_section(state)}
        },

        FlightState::Armed => rsx! {
            Section { title: "Pressure",
                {summary_row(&rows_snapshot, "FUEL_TANK_PRESSURE", &[("Tank Pressure", 0)])}
                {data_style_chart_cached("FUEL_TANK_PRESSURE", 1200.0, 260.0, Some("Fuel Tank Pressure"))}
            }
            Section { title: "Valve States",
                {valve_state_grid(&rows_snapshot)}
            }
            {action_section(state)}
        },

        FlightState::Launch
        | FlightState::Ascent
        | FlightState::Coast
        | FlightState::Apogee
        | FlightState::ParachuteDeploy
        | FlightState::Descent => rsx! {
            Section { title: "Altitude",
                {summary_row(&rows_snapshot, "BAROMETER_DATA", &[("Altitude", 2), ("Pressure", 0), ("Temp", 1)])}
                {data_style_chart_cached("BAROMETER_DATA", 1200.0, 280.0, Some("Barometer Data"))}
            }
            Section { title: "Acceleration",
                {summary_row(&rows_snapshot, "ACCEL_DATA", &[("Accel X", 0), ("Accel Y", 1), ("Accel Z", 2)])}
                {data_style_chart_cached("ACCEL_DATA", 1200.0, 300.0, Some("Acceleration"))}
            }
            Section { title: "Kalman Filter",
                {summary_row(&rows_snapshot, "KALMAN_FILTER_DATA", &[("Kalman X", 0), ("Kalman Y", 1), ("Kalman Z", 2)])}
                {data_style_chart_cached("KALMAN_FILTER_DATA", 1200.0, 300.0, Some("Kalman Filter"))}
            }
            {action_section(state)}
        },

        FlightState::Landed | FlightState::Recovery => rsx! {
            Section { title: "Recovery Map",
                MapTab { rocket_gps: rocket_gps, user_gps: user_gps }
            }
            {action_section(state)}
        },

        FlightState::Idle | FlightState::Aborted => rsx! {
            {action_section(state)}
        },
    };

    rsx! {
        // ✅ Make StateTab scrollable + add bottom padding and a spacer div
        div { style: "padding:16px; height:100%; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:auto; display:flex; flex-direction:column; gap:16px; padding-bottom:100px;",
            h2 { style: "margin:0; color:#e5e7eb;", "State" }
            div { style: "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
                div { style: "font-size:14px; color:#94a3b8;", "Current Flight State" }
                div { style: "font-size:22px; font-weight:700; margin-top:6px; color:#e5e7eb;",
                    "{state.to_string()}"
                }
            }
            {content}

        }
    }
}

#[component]
fn Section(title: &'static str, children: Element) -> Element {
    rsx! {
        div { style: "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
            div { style: "font-size:15px; color:#cbd5f5; font-weight:600; margin-bottom:10px;", "{title}" }
            {children}
        }
    }
}

// ============================================================
// cached chart renderer (uses charts_cache_get)
// ============================================================

fn data_style_chart_cached(dt: &str, view_w: f64, view_h: f64, title: Option<&str>) -> Element {
    let w = view_w as f32;
    let h = view_h as f32;

    let (paths, y_min, y_max, span_min) = charts_cache_get(dt, w, h);
    let labels = labels_for_datatype(dt);

    let left = 60.0_f64;
    let right = view_w - 20.0_f64;
    let top = 20.0_f64;
    let bottom = view_h - 20.0_f64;

    let inner_w = right - left;
    let inner_h = bottom - top;

    let grid_x_step = inner_w / 6.0;
    let grid_y_step = inner_h / 6.0;

    let y_mid = (y_min + y_max) * 0.5;

    rsx! {
        div { style: "width:100%; background:#020617; border-radius:14px; border:1px solid #334155; padding:12px; display:flex; flex-direction:column; gap:8px;",
            if let Some(t) = title {
                div { style: "color:#e5e7eb; font-weight:700; font-size:14px;", "{t}" }
            }

            svg {
                style: "width:100%; height:auto; display:block;",
                view_box: "0 0 {view_w} {view_h}",

                for i in 1..=5 {
                    line {
                        x1:"{left}", y1:"{top + grid_y_step * (i as f64)}",
                        x2:"{right}", y2:"{top + grid_y_step * (i as f64)}",
                        stroke:"#1f2937", "stroke-width":"1"
                    }
                }
                for i in 1..=5 {
                    line {
                        x1:"{left + grid_x_step * (i as f64)}", y1:"{top}",
                        x2:"{left + grid_x_step * (i as f64)}", y2:"{bottom}",
                        stroke:"#1f2937", "stroke-width":"1"
                    }
                }

                line { x1:"{left}", y1:"{top}",    x2:"{left}",  y2:"{bottom}", stroke:"#334155", stroke_width:"1" }
                line { x1:"{left}", y1:"{bottom}", x2:"{right}", y2:"{bottom}", stroke:"#334155", stroke_width:"1" }

                text { x:"10", y:"{top + 6.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_max)} }
                text { x:"10", y:"{top + inner_h / 2.0 + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_mid)} }
                text { x:"10", y:"{bottom + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_min)} }

                text { x:"{left + 10.0}",  y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min)} }
                text { x:"{view_w * 0.5}", y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min * 0.5)} }
                text { x:"{right - 60.0}", y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", "now" }

                for ch in 0..8usize {
                    if !paths[ch].is_empty() {
                        path {
                            d: "{paths[ch]}",
                            fill: "none",
                            stroke: "{series_color(ch)}",
                            stroke_width: "2",
                            stroke_linecap: "round",
                        }
                    }
                }
            }

            div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:6px 10px; background:rgba(2,6,23,0.75); border:1px solid #1f2937; border-radius:10px;",
                for i in 0..8usize {
                    if !labels[i].is_empty() {
                        div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:#cbd5f5;",
                            svg { width:"26", height:"8", view_box:"0 0 26 8",
                                line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(i)}", stroke_width:"2", stroke_linecap:"round" }
                            }
                            "{labels[i]}"
                        }
                    }
                }
            }
        }
    }
}

// ============================================================
// Existing StateTab helpers (mostly unchanged)
// ============================================================

fn valve_state_grid(rows: &[TelemetryRow]) -> Element {
    let latest = rows
        .iter()
        .filter(|r| r.data_type == "VALVE_STATE")
        .max_by_key(|r| r.timestamp_ms);

    let Some(row) = latest else {
        return rsx! { div { style: "color:#94a3b8; font-size:12px;", "No valve state yet." } };
    };

    let items = [
        ("Pilot", row.v0),
        ("NormallyOpen", row.v1),
        ("Dump", row.v2),
        ("Igniter", row.v3),
        ("Nitrogen", row.v4),
        ("Nitrous", row.v5),
        ("Fill Lines", row.v6),
    ];

    rsx! {
        div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(150px, 1fr)); gap:10px; margin-bottom:12px;",
            for (label, value) in items {
                ValveStateCard { label: label, value: value, is_fill_lines: label == "Fill Lines" }
            }
        }
    }
}

#[component]
fn ValveStateCard(label: &'static str, value: Option<f32>, is_fill_lines: bool) -> Element {
    let (bg, border, fg, text) = match value {
        Some(v) if v >= 0.5 => {
            if is_fill_lines {
                ("#052e16", "#22c55e", "#bbf7d0", "Installed")
            } else {
                ("#052e16", "#22c55e", "#bbf7d0", "Open")
            }
        }
        Some(_) => {
            if is_fill_lines {
                ("#1f2937", "#94a3b8", "#e2e8f0", "Removed")
            } else {
                ("#1f2937", "#94a3b8", "#e2e8f0", "Closed")
            }
        }
        None => ("#0b1220", "#475569", "#94a3b8", "Unknown"),
    };

    rsx! {
        div { style: "padding:10px; border-radius:12px; background:{bg}; border:1px solid {border};",
            div { style: "font-size:12px; color:{fg};", "{label}" }
            div { style: "font-size:18px; font-weight:700; color:{fg};", "{text}" }
        }
    }
}

fn action_section(state: FlightState) -> Element {
    let actions = actions_for_state(state);
    if actions.is_empty() {
        return rsx! { div {} };
    }

    rsx! {
        Section { title: "Actions",
            div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(180px, 1fr)); gap:10px;",
                for action in actions {
                    button {
                        style: action_style(action.border, action.bg, action.fg),
                        onclick: move |_| crate::telemetry_dashboard::send_cmd(action.cmd),
                        "{action.label}"
                    }
                }
            }
        }
    }
}

struct ActionDef {
    label: &'static str,
    cmd: &'static str,
    border: &'static str,
    bg: &'static str,
    fg: &'static str,
}

fn actions_for_state(state: FlightState) -> Vec<ActionDef> {
    match state {
        FlightState::Armed => vec![
            ActionDef {
                label: "Launch",
                cmd: "Launch",
                border: "#22c55e",
                bg: "#022c22",
                fg: "#bbf7d0",
            },
            ActionDef {
                label: "Dump",
                cmd: "Dump",
                border: "#ef4444",
                bg: "#450a0a",
                fg: "#fecaca",
            },
        ],
        FlightState::Idle
        | FlightState::PreFill
        | FlightState::FillTest
        | FlightState::NitrogenFill
        | FlightState::NitrousFill => vec![
            ActionDef { label: "Dump", cmd: "Dump", border: "#ef4444", bg: "#450a0a", fg: "#fecaca" },
            ActionDef { label: "NormallyOpen", cmd: "NormallyOpen", border: "#f97316", bg: "#1f2937", fg: "#ffedd5" },
            ActionDef { label: "Pilot", cmd: "Pilot", border: "#a78bfa", bg: "#111827", fg: "#ddd6fe" },
            ActionDef { label: "Igniter", cmd: "Igniter", border: "#60a5fa", bg: "#0b1220", fg: "#bfdbfe" },
            ActionDef { label: "Nitrogen", cmd: "Nitrogen", border: "#22d3ee", bg: "#0b1220", fg: "#cffafe" },
            ActionDef { label: "Nitrous", cmd: "Nitrous", border: "#a3e635", bg: "#111827", fg: "#ecfccb" },
            ActionDef { label: "Fill Lines", cmd: "RetractPlumbing", border: "#eab308", bg: "#1f2937", fg: "#fef9c3" },
        ],
        FlightState::Startup => vec![],
        FlightState::Launch
        | FlightState::Ascent
        | FlightState::Coast
        | FlightState::Apogee
        | FlightState::ParachuteDeploy
        | FlightState::Descent
        | FlightState::Landed
        | FlightState::Recovery
        | FlightState::Aborted => vec![],
    }
}

fn action_style(border: &str, bg: &str, fg: &str) -> String {
    format!(
        "padding:0.6rem 0.9rem; border-radius:0.75rem; cursor:pointer; width:100%; \
         text-align:left; border:1px solid {border}; background:{bg}; color:{fg}; \
         font-weight:700;"
    )
}

fn summary_row(rows: &[TelemetryRow], dt: &str, items: &[(&'static str, usize)]) -> Element {
    let want_minmax = dt != "VALVE_STATE" && dt != "GPS_DATA";

    let (chan_min, chan_max) = if want_minmax {
        charts_cache_get_channel_minmax(dt, 1200.0, 300.0)
    } else {
        ([None; 8], [None; 8])
    };

    let latest = items
        .iter()
        .map(|(label, idx)| (*label, *idx, latest_value(rows, dt, *idx)))
        .collect::<Vec<_>>();

    rsx! {
        div { style: "display:grid; gap:10px; margin-bottom:12px; grid-template-columns:repeat(auto-fit, minmax(140px, 1fr)); width:100%;",
            for (label, idx, value) in latest {
                SummaryCard {
                    label: label,
                    value: fmt_opt(value),
                    min: if want_minmax { chan_min[idx].map(|v| format!("{v:.4}")) } else { None },
                    max: if want_minmax { chan_max[idx].map(|v| format!("{v:.4}")) } else { None },
                }
            }
        }
    }
}

#[component]
fn SummaryCard(label: &'static str, value: String, min: Option<String>, max: Option<String>) -> Element {
    let mm = match (min.as_deref(), max.as_deref()) {
        (Some(mi), Some(ma)) => Some(format!("min {mi} • max {ma}")),
        _ => None,
    };

    rsx! {
        div { style: "padding:10px; border-radius:12px; background:#0f172a; border:1px solid #334155; width:100%; min-width:0; box-sizing:border-box;",
            div { style: "font-size:12px; color:#93c5fd;", "{label}" }
            div { style: "font-size:18px; color:#e5e7eb; line-height:1.1;", "{value}" }
            if let Some(t) = mm {
                div { style: "font-size:11px; color:#94a3b8; margin-top:4px;", "{t}" }
            }
        }
    }
}

fn latest_value(rows: &[TelemetryRow], dt: &str, idx: usize) -> Option<f32> {
    rows.iter()
        .filter(|r| r.data_type == dt)
        .max_by_key(|r| r.timestamp_ms)
        .and_then(|r| value_at(r, idx))
}

fn value_at(row: &TelemetryRow, idx: usize) -> Option<f32> {
    match idx {
        0 => row.v0,
        1 => row.v1,
        2 => row.v2,
        3 => row.v3,
        4 => row.v4,
        5 => row.v5,
        6 => row.v6,
        7 => row.v7,
        _ => None,
    }
}

fn fmt_opt(v: Option<f32>) -> String {
    match v {
        Some(x) => format!("{x:.3}"),
        None => "-".to_string(),
    }
}

fn board_status_table(boards: &[BoardStatusEntry]) -> Element {
    if boards.is_empty() {
        return rsx! { div { style: "color:#94a3b8;", "No board status yet." } };
    }

    rsx! {
        div { style: "border:1px solid #1f2937; border-radius:10px; overflow:hidden;",
            div { style: "display:grid; grid-template-columns:1.4fr 0.8fr 0.6fr 0.8fr 0.8fr; background:#020617;",
                div { style: header_cell_style(), "Board" }
                div { style: header_cell_style(), "Sender ID" }
                div { style: header_cell_style(), "Seen" }
                div { style: header_cell_style(), "Last Seen (ms)" }
                div { style: header_cell_style(), "Age (ms)" }
            }
            for entry in boards.iter() {
                div { style: "display:grid; grid-template-columns:1.4fr 0.8fr 0.6fr 0.8fr 0.8fr; background:#020617;",
                    div { style: cell_style(), "{entry.board.as_str()}" }
                    div { style: cell_style(), "{entry.sender_id}" }
                    div { style: cell_style(), if entry.seen { "yes" } else { "no" } }
                    div { style: cell_style(), "{entry.last_seen_ms.map(|v| v.to_string()).unwrap_or_else(|| \"-\".into())}" }
                    div { style: cell_style(), "{entry.age_ms.map(|v| v.to_string()).unwrap_or_else(|| \"-\".into())}" }
                }
            }
        }
    }
}

fn header_cell_style() -> &'static str {
    "font-weight:600; color:#e2e8f0; padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937;"
}

fn cell_style() -> &'static str {
    "padding:8px; border-bottom:1px solid #1f2937; border-right:1px solid #1f2937; color:#e5e7eb;"
}
