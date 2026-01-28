use dioxus::prelude::*;
use dioxus_signals::Signal;
use groundstation_shared::{BoardStatusEntry, FlightState, TelemetryRow};

use crate::telemetry_dashboard::data_chart::data_style_chart;
use crate::telemetry_dashboard::map_tab::MapTab;

#[component]
pub fn StateTab(
    flight_state: Signal<FlightState>,
    rows: Signal<Vec<TelemetryRow>>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Element {
    let state = *flight_state.read();
    let rows_snapshot = rows.read().clone();
    let boards_snapshot = board_status.read().clone();

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
                {data_style_chart(&rows_snapshot, "FUEL_TANK_PRESSURE", 260.0, Some("Fuel Tank Pressure"))}
            }
            {action_section(state)}
        },

        FlightState::Armed => rsx! {
            Section { title: "Pressure",
                {summary_row(&rows_snapshot, "FUEL_TANK_PRESSURE", &[("Tank Pressure", 0)])}
                {data_style_chart(&rows_snapshot, "FUEL_TANK_PRESSURE", 260.0, Some("Fuel Tank Pressure"))}
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
                {data_style_chart(&rows_snapshot, "BAROMETER_DATA", 280.0, Some("Barometer Data"))}
            }
            Section { title: "Acceleration",
                {summary_row(&rows_snapshot, "ACCEL_DATA", &[("Accel X", 0), ("Accel Y", 1), ("Accel Z", 2)])}
                {data_style_chart(&rows_snapshot, "ACCEL_DATA", 300.0, Some("Acceleration"))}
            }
            Section { title: "Kalman Filter",
                {summary_row(&rows_snapshot, "KALMAN_FILTER_DATA", &[("Kalman X", 0), ("Kalman Y", 1), ("Kalman Z", 2)])}
                {data_style_chart(&rows_snapshot, "KALMAN_FILTER_DATA", 300.0, Some("Kalman Filter"))}
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
        div { style: "padding:16px; display:flex; flex-direction:column; gap:16px;",
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
            ActionDef {
                label: "Dump",
                cmd: "Dump",
                border: "#ef4444",
                bg: "#450a0a",
                fg: "#fecaca",
            },
            ActionDef {
                label: "Tanks",
                cmd: "Tanks",
                border: "#f97316",
                bg: "#1f2937",
                fg: "#ffedd5",
            },
            ActionDef {
                label: "Pilot",
                cmd: "Pilot",
                border: "#a78bfa",
                bg: "#111827",
                fg: "#ddd6fe",
            },
            ActionDef {
                label: "Igniter",
                cmd: "Igniter",
                border: "#60a5fa",
                bg: "#0b1220",
                fg: "#bfdbfe",
            },
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
    let latest = items
        .iter()
        .map(|(label, idx)| (*label, latest_value(rows, dt, *idx)))
        .collect::<Vec<_>>();

    rsx! {
        div { style: "display:flex; flex-wrap:wrap; gap:10px; margin-bottom:12px;",
            for (label, value) in latest {
                SummaryCard { label: label, value: fmt_opt(value) }
            }
        }
    }
}

#[component]
fn SummaryCard(label: &'static str, value: String) -> Element {
    rsx! {
        div { style: "padding:10px; border-radius:12px; background:#0f172a; border:1px solid #334155; min-width:120px;",
            div { style: "font-size:12px; color:#93c5fd;", "{label}" }
            div { style: "font-size:18px; color:#e5e7eb;", "{value}" }
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
