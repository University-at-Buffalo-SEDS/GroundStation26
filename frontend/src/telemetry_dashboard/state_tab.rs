// frontend/src/telemetry_dashboard/state_tab.rs

use dioxus::prelude::*;
use dioxus_signals::Signal;

use super::layout::{
    ActionSpec, ActionsTabLayout, BooleanLabels, StateSection, StateTabLayout, StateWidget,
    StateWidgetKind, SummaryItem, ValveColor, ValveColorSet,
};
use super::types::{BoardStatusEntry, FlightState, TelemetryRow};

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
    layout: StateTabLayout,
    actions: ActionsTabLayout,
    default_valve_labels: Option<BooleanLabels>,
) -> Element {
    // ------------------------------------------------------------
    // Redraw driver for charts on State tab
    // ------------------------------------------------------------
    let redraw_tick = use_signal(|| 0u64);
    #[cfg(target_arch = "wasm32")]
    let raf_running = use_signal(|| std::rc::Rc::new(std::cell::Cell::new(true)));
    #[cfg(target_arch = "wasm32")]
    let raf_id = use_signal(|| std::rc::Rc::new(std::cell::Cell::new(None::<i32>)));

    use_effect({
        let mut redraw_tick = redraw_tick;
        #[cfg(target_arch = "wasm32")]
        let raf_running = raf_running.read().clone();
        #[cfg(target_arch = "wasm32")]
        let raf_id = raf_id.read().clone();

        move || {
            #[cfg(target_arch = "wasm32")]
            {
                use std::cell::RefCell;
                use std::rc::Rc;
                use wasm_bindgen::JsCast;
                use wasm_bindgen::closure::Closure;

                let cb: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
                let cb2 = cb.clone();
                let raf_running_cb = raf_running.clone();
                let raf_id_cb = raf_id.clone();
                let raf_id_start = raf_id.clone();

                *cb2.borrow_mut() = Some(Closure::wrap(Box::new(move |_ts: f64| {
                    if !raf_running_cb.get() {
                        return;
                    }
                    let next = redraw_tick.read().wrapping_add(1);
                    redraw_tick.set(next);

                    if let Some(win) = web_sys::window() {
                        if let Some(cb_ref) = cb.borrow().as_ref() {
                            if let Ok(id) = win.request_animation_frame(cb_ref.as_ref().unchecked_ref())
                            {
                                raf_id_cb.set(Some(id));
                            }
                        }
                    }
                }) as Box<dyn FnMut(f64)>));

                if let Some(win) = web_sys::window() {
                    if let Some(cb_ref) = cb2.borrow().as_ref() {
                        if let Ok(id) = win.request_animation_frame(cb_ref.as_ref().unchecked_ref()) {
                            raf_id_start.set(Some(id));
                        }
                    }
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

    #[cfg(target_arch = "wasm32")]
    {
        let raf_running = raf_running.read().clone();
        let raf_id = raf_id.read().clone();
        use_drop(move || {
            raf_running.set(false);
            if let Some(win) = web_sys::window() {
                if let Some(id) = raf_id.get() {
                    let _ = win.cancel_animation_frame(id);
                }
            }
        });
    }

    // Force rerender when redraw driver ticks
    let _ = *redraw_tick.read();

    let state = *flight_state.read();
    let rows_snapshot = rows.read();
    let boards_snapshot = board_status.read();
    let actions_snapshot = actions.actions.clone();

    let content = if let Some(state_layout) = layout
        .states
        .iter()
        .find(|entry| entry.states.contains(&state))
    {
        rsx! {
            for section in state_layout.sections.iter() {
                {render_state_section(
                    section,
                    &rows_snapshot,
                    &boards_snapshot,
                    &actions_snapshot,
                    default_valve_labels.as_ref(),
                    rocket_gps,
                    user_gps
                )}
            }
        }
    } else {
        rsx! { div { style: "color:#94a3b8; font-size:12px;", "No layout for this flight state." } }
    };

    rsx! {
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
fn Section(title: String, children: Element) -> Element {
    rsx! {
        div { style: "padding:14px; border:1px solid #334155; border-radius:14px; background:#0b1220;",
            div { style: "font-size:15px; color:#cbd5f5; font-weight:600; margin-bottom:10px;", "{title}" }
            {children}
        }
    }
}

fn render_state_section(
    section: &StateSection,
    rows: &[TelemetryRow],
    boards: &[BoardStatusEntry],
    actions: &[ActionSpec],
    default_valve_labels: Option<&BooleanLabels>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Element {
    if !section_has_content(section, actions) {
        return rsx! { div {} };
    }
    let title = section.title.clone().unwrap_or_else(|| "Section".to_string());

    rsx! {
        Section { title: title,
            for widget in section.widgets.iter() {
                {render_state_widget(
                    widget,
                    rows,
                    boards,
                    actions,
                    default_valve_labels,
                    rocket_gps,
                    user_gps
                )}
            }
        }
    }
}

fn render_state_widget(
    widget: &StateWidget,
    rows: &[TelemetryRow],
    boards: &[BoardStatusEntry],
    actions: &[ActionSpec],
    default_valve_labels: Option<&BooleanLabels>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Element {
    match widget.kind {
        StateWidgetKind::BoardStatus => rsx! { {board_status_table(boards)} },
        StateWidgetKind::Summary => {
            let dt = widget.data_type.as_deref().unwrap_or("");
            let items = widget.items.as_deref().unwrap_or(&[]);
            if dt.is_empty() {
                rsx! { div { style: "color:#94a3b8; font-size:12px;", "Missing summary data_type" } }
            } else {
                rsx! { {summary_row(rows, dt, items)} }
            }
        }
        StateWidgetKind::Chart => {
            let dt = widget.data_type.as_deref().unwrap_or("");
            if dt.is_empty() {
                rsx! { div { style: "color:#94a3b8; font-size:12px;", "Missing chart data_type" } }
            } else {
                let w = widget.width.unwrap_or(1200.0);
                let h = widget.height.unwrap_or(260.0);
                rsx! { {data_style_chart_cached(dt, w, h, widget.chart_title.as_deref())} }
            }
        }
        StateWidgetKind::ValveState => {
            let labels = widget.boolean_labels.as_ref().or(default_valve_labels);
            rsx! { {valve_state_grid(
                rows,
                widget.valves.as_deref(),
                widget.valve_colors.as_ref(),
                labels,
                widget.valve_labels.as_deref(),
            )} }
        }
        StateWidgetKind::Map => rsx! { MapTab { rocket_gps: rocket_gps, user_gps: user_gps } },
        StateWidgetKind::Actions => rsx! { {action_section(actions, widget.actions.as_deref())} },
    }
}

fn section_has_content(section: &StateSection, actions: &[ActionSpec]) -> bool {
    if section.widgets.is_empty() {
        return false;
    }
    let has_actions = !actions.is_empty();
    for widget in section.widgets.iter() {
        match widget.kind {
            StateWidgetKind::Actions => {
                if has_actions && has_any_actions(actions, widget.actions.as_deref()) {
                    return true;
                }
            }
            _ => return true,
        }
    }
    false
}

// ============================================================
// cached chart renderer (uses charts_cache_get)
// ============================================================

fn data_style_chart_cached(dt: &str, view_w: f64, view_h: f64, title: Option<&str>) -> Element {
    let w = view_w as f32;
    let h = view_h as f32;

    let (paths, y_min, y_max, span_min) = charts_cache_get(dt, w, h);
    let labels: Vec<String> = labels_for_datatype(dt).iter().map(|s| s.to_string()).collect();

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

                for (ch, path_d) in paths.iter().enumerate() {
                    if !path_d.is_empty() {
                        path {
                            d: "{path_d}",
                            fill: "none",
                            stroke: "{series_color(ch)}",
                            stroke_width: "2",
                            stroke_linecap: "round",
                        }
                    }
                }
            }

            div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:6px 10px; background:rgba(2,6,23,0.75); border:1px solid #1f2937; border-radius:10px;",
                for (i, label) in labels.iter().enumerate() {
                    if !label.is_empty() {
                        div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:#cbd5f5;",
                            svg { width:"26", height:"8", view_box:"0 0 26 8",
                                line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(i)}", stroke_width:"2", stroke_linecap:"round" }
                            }
                            "{label}"
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

fn valve_state_grid(
    rows: &[TelemetryRow],
    valves: Option<&[SummaryItem]>,
    colors: Option<&ValveColorSet>,
    labels: Option<&BooleanLabels>,
    valve_labels: Option<&[BooleanLabels]>,
) -> Element {
    let latest = rows
        .iter()
        .filter(|r| r.data_type == "VALVE_STATE")
        .max_by_key(|r| r.timestamp_ms);

    let Some(row) = latest else {
        return rsx! { div { style: "color:#94a3b8; font-size:12px;", "No valve state yet." } };
    };

    let default_items = [
        SummaryItem { label: "Pilot".to_string(), index: 0 },
        SummaryItem { label: "NormallyOpen".to_string(), index: 1 },
        SummaryItem { label: "Dump".to_string(), index: 2 },
        SummaryItem { label: "Igniter".to_string(), index: 3 },
        SummaryItem { label: "Nitrogen".to_string(), index: 4 },
        SummaryItem { label: "Nitrous".to_string(), index: 5 },
        SummaryItem { label: "Fill Lines".to_string(), index: 6 },
    ];

    let items: Vec<(String, Option<f32>)> = match valves {
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|item| (item.label.clone(), value_at(row, item.index)))
            .collect(),
        _ => default_items
            .iter()
            .map(|item| (item.label.clone(), value_at(row, item.index)))
            .collect(),
    };

    let (open, closed, unknown) = valve_colors(colors);

    rsx! {
        div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(150px, 1fr)); gap:10px; margin-bottom:12px;",
            for (idx, (label, value)) in items.iter().enumerate() {
                ValveStateCard {
                    label: label.clone(),
                    value: *value,
                    open: open.clone(),
                    closed: closed.clone(),
                    unknown: unknown.clone(),
                    labels: widget_valve_labels_at(labels, valve_labels, idx),
                }
            }
        }
    }
}

#[component]
fn ValveStateCard(
    label: String,
    value: Option<f32>,
    open: ValveColor,
    closed: ValveColor,
    unknown: ValveColor,
    labels: Option<BooleanLabels>,
) -> Element {
    let true_label = labels
        .as_ref()
        .map(|l| l.true_label.as_str())
        .unwrap_or("Open");
    let false_label = labels
        .as_ref()
        .map(|l| l.false_label.as_str())
        .unwrap_or("Closed");
    let unknown_label = labels
        .as_ref()
        .and_then(|l| l.unknown_label.as_deref())
        .unwrap_or("Unknown");

    let (bg, border, fg, text) = match value {
        Some(v) if v >= 0.5 => {
            (open.bg.as_str(), open.border.as_str(), open.fg.as_str(), true_label)
        }
        Some(_) => {
            (closed.bg.as_str(), closed.border.as_str(), closed.fg.as_str(), false_label)
        }
        None => (
            unknown.bg.as_str(),
            unknown.border.as_str(),
            unknown.fg.as_str(),
            unknown_label,
        ),
    };

    rsx! {
        div { style: "padding:10px; border-radius:12px; background:{bg}; border:1px solid {border};",
            div { style: "font-size:12px; color:{fg};", "{label}" }
            div { style: "font-size:18px; font-weight:700; color:{fg};", "{text}" }
        }
    }
}

fn valve_colors(colors: Option<&ValveColorSet>) -> (ValveColor, ValveColor, ValveColor) {
    let default_open = ValveColor {
        bg: "#052e16".to_string(),
        border: "#22c55e".to_string(),
        fg: "#bbf7d0".to_string(),
    };
    let default_closed = ValveColor {
        bg: "#1f2937".to_string(),
        border: "#94a3b8".to_string(),
        fg: "#e2e8f0".to_string(),
    };
    let default_unknown = ValveColor {
        bg: "#0b1220".to_string(),
        border: "#475569".to_string(),
        fg: "#94a3b8".to_string(),
    };

    let open = colors.and_then(|c| c.open.clone()).unwrap_or(default_open);
    let closed = colors.and_then(|c| c.closed.clone()).unwrap_or(default_closed);
    let unknown = colors
        .and_then(|c| c.unknown.clone())
        .unwrap_or(default_unknown);
    (open, closed, unknown)
}

fn widget_valve_labels_at<'a>(
    default_labels: Option<&'a BooleanLabels>,
    valve_labels: Option<&'a [BooleanLabels]>,
    idx: usize,
) -> Option<BooleanLabels> {
    if let Some(list) = valve_labels {
        if idx < list.len() {
            return Some(list[idx].clone());
        }
    }
    default_labels.cloned()
}

fn action_section(actions: &[ActionSpec], selection: Option<&[String]>) -> Element {
    let filtered = filter_actions(actions, selection);
    if filtered.is_empty() {
        return rsx! { div {} };
    }

    rsx! {
        div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(180px, 1fr)); gap:10px;",
            for action in filtered.iter() {
                button {
                    style: action_style(&action.border, &action.bg, &action.fg),
                    onclick: {
                        let cmd = action.cmd.clone();
                        move |_| crate::telemetry_dashboard::send_cmd(&cmd)
                    },
                    "{action.label}"
                }
            }
        }
    }
}

fn filter_actions<'a>(
    actions: &'a [ActionSpec],
    selection: Option<&[String]>,
) -> Vec<&'a ActionSpec> {
    let Some(selected) = selection else {
        return actions.iter().collect();
    };
    if selected.is_empty() {
        return actions.iter().collect();
    }
    let mut filtered = Vec::with_capacity(selected.len());
    for cmd in selected {
        if let Some(action) = actions.iter().find(|a| &a.cmd == cmd) {
            filtered.push(action);
        }
    }
    filtered
}

fn has_any_actions(actions: &[ActionSpec], selection: Option<&[String]>) -> bool {
    let Some(selected) = selection else {
        return !actions.is_empty();
    };
    if selected.is_empty() {
        return !actions.is_empty();
    }
    selected.iter().any(|cmd| actions.iter().any(|a| &a.cmd == cmd))
}

fn action_style(border: &str, bg: &str, fg: &str) -> String {
    format!(
        "padding:0.6rem 0.9rem; border-radius:0.75rem; cursor:pointer; width:100%; \
         text-align:left; border:1px solid {border}; background:{bg}; color:{fg}; \
         font-weight:700;"
    )
}

fn summary_row(rows: &[TelemetryRow], dt: &str, items: &[SummaryItem]) -> Element {
    let want_minmax = dt != "VALVE_STATE" && dt != "GPS_DATA";

    let (chan_min, chan_max) = if want_minmax {
        charts_cache_get_channel_minmax(dt, 1200.0, 300.0)
    } else {
        (Vec::new(), Vec::new())
    };

    let latest = items
        .iter()
        .map(|item| (item.label.clone(), item.index, latest_value(rows, dt, item.index)))
        .collect::<Vec<_>>();

    rsx! {
        div { style: "display:grid; gap:10px; margin-bottom:12px; grid-template-columns:repeat(auto-fit, minmax(140px, 1fr)); width:100%;",
            for (label, idx, value) in latest {
                SummaryCard {
                    label: label,
                    value: fmt_opt(value),
                    min: if want_minmax { chan_min.get(idx).copied().flatten().map(|v| format!("{v:.4}")) } else { None },
                    max: if want_minmax { chan_max.get(idx).copied().flatten().map(|v| format!("{v:.4}")) } else { None },
                }
            }
        }
    }
}

#[component]
fn SummaryCard(label: String, value: String, min: Option<String>, max: Option<String>) -> Element {
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
    row.values.get(idx).copied().flatten()
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
