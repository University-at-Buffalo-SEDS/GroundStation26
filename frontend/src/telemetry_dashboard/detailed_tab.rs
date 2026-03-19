use dioxus::prelude::*;
use dioxus_signals::Signal;

use super::types::{BoardStatusEntry, FlightState, NetworkTopologyMsg, NetworkTopologyStatus};
use super::{FrontendNetworkMetrics, format_timestamp_ms_clock};

#[component]
pub fn DetailedTab(
    metrics: Signal<FrontendNetworkMetrics>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    network_topology: Signal<NetworkTopologyMsg>,
    flight_state: Signal<FlightState>,
    network_time_display: Option<String>,
    network_clock_delta_ms: Option<i64>,
    network_time_age_ms: Option<i64>,
) -> Element {
    let metrics_snapshot = metrics.read().clone();
    let boards = board_status.read().clone();
    let topology = network_topology.read().clone();

    let board_seen = boards.iter().filter(|board| board.seen).count();
    let board_total = boards.len();
    let online_nodes = topology
        .nodes
        .iter()
        .filter(|node| node.status == NetworkTopologyStatus::Online)
        .count();
    let offline_nodes = topology
        .nodes
        .iter()
        .filter(|node| node.status == NetworkTopologyStatus::Offline)
        .count();

    rsx! {
        div { style: "padding:18px; height:100%; overflow-y:auto; overflow-x:hidden; color:#dbe7f3;",
            div { style: "display:grid; gap:14px; grid-template-columns:repeat(auto-fit, minmax(260px, 1fr)); margin-bottom:14px;",
                {metric_card(
                    "Frontend ↔ Backend",
                    vec![
                        ("Status", if metrics_snapshot.ws_connected { "Connected".to_string() } else { "Disconnected".to_string() }),
                        ("Base URL", metrics_snapshot.base_http.clone()),
                        ("WebSocket", metrics_snapshot.ws_url.clone()),
                        ("HTTP RTT", opt_ms(metrics_snapshot.http_rtt_ms)),
                        ("HTTP RTT EMA", opt_ms(metrics_snapshot.http_rtt_ema_ms)),
                        ("WS epoch", metrics_snapshot.ws_epoch.to_string()),
                    ],
                )}
                {metric_card(
                    "Traffic",
                    vec![
                        ("Inbound messages", metrics_snapshot.ws_messages_total.to_string()),
                        ("Inbound bytes", human_bytes(metrics_snapshot.ws_bytes_total)),
                        ("Telemetry rows", metrics_snapshot.telemetry_rows_total.to_string()),
                        ("Telemetry batches", metrics_snapshot.telemetry_batches_total.to_string()),
                        ("Msg rate", format!("{:.1}/s", metrics_snapshot.msgs_per_sec)),
                        ("Bandwidth", format!("{}/s", human_bytes_f64(metrics_snapshot.bytes_per_sec))),
                    ],
                )}
                {metric_card(
                    "Freshness",
                    vec![
                        ("Rows per second", format!("{:.1}/s", metrics_snapshot.rows_per_sec)),
                        ("Last WS message", opt_timestamp(metrics_snapshot.last_ws_message_wall_ms)),
                        ("Last disconnect", metrics_snapshot.last_disconnect_reason.clone().unwrap_or_else(|| "None".to_string())),
                        ("Flight state", flight_state.read().to_string()),
                        ("Rocket time", network_time_display.unwrap_or_else(|| "Unavailable".to_string())),
                        ("Clock delta", opt_signed_ms(network_clock_delta_ms)),
                    ],
                )}
                {metric_card(
                    "Topology",
                    vec![
                        ("Boards seen", format!("{board_seen}/{board_total}")),
                        ("Topology nodes", topology.nodes.len().to_string()),
                        ("Topology links", topology.links.len().to_string()),
                        ("Online nodes", online_nodes.to_string()),
                        ("Offline nodes", offline_nodes.to_string()),
                        ("Server time age", opt_i64_ms(network_time_age_ms)),
                    ],
                )}
            }

            div { style: "display:grid; gap:14px; grid-template-columns:minmax(320px, 1.2fr) minmax(320px, 1fr);",
                div { style: section_style(),
                    h3 { style: section_title_style(), "Board Latency Detail" }
                    table { style: table_style(),
                        thead {
                            tr {
                                th { style: th_style(), "Board" }
                                th { style: th_style(), "Sender" }
                                th { style: th_style(), "Seen" }
                                th { style: th_style(), "Age" }
                                th { style: th_style(), "Last Seen" }
                            }
                        }
                        tbody {
                            for board in boards.iter() {
                                tr {
                                    td { style: td_style(), "{board.board.as_str()}" }
                                    td { style: td_style_mono(), "{board.sender_id}" }
                                    td { style: td_style(), if board.seen { "yes" } else { "no" } }
                                    td { style: td_style_mono(), "{opt_i64_ms(board.age_ms.map(|v| v as i64))}" }
                                    td { style: td_style_mono(), "{board.last_seen_ms.map(|ts| format_timestamp_ms_clock(ts as i64)).unwrap_or_else(|| \"--\".to_string())}" }
                                }
                            }
                        }
                    }
                }

                div { style: section_style(),
                    h3 { style: section_title_style(), "Network Notes" }
                    div { style: "display:flex; flex-direction:column; gap:10px; font-size:13px; color:#cbd5e1;",
                        {info_line("HTTP RTT", "Measured by polling `/api/network_time`; this is the best current frontend ↔ backend round-trip estimate.")}
                        {info_line("WS bandwidth", "Computed locally from incoming WebSocket payload sizes seen by the frontend.")}
                        {info_line("Board age", "Derived from backend board-status packets; large values usually indicate radio-side latency or missing telemetry.")}
                        {info_line("Clock delta", "Local wall-clock minus backend-reported network time. Useful for spotting machine clock drift.")}
                        {info_line("Topology", "Uses the backend discovery snapshot plus physical link health flags.")}
                    }
                }
            }
        }
    }
}

fn metric_card(title: &'static str, rows: Vec<(&'static str, String)>) -> Element {
    rsx! {
        div { style: "border:1px solid #274154; border-radius:16px; padding:14px; background:linear-gradient(180deg, #071521 0%, #0d1b2a 100%); box-shadow:0 14px 30px rgba(2, 6, 23, 0.28);",
            h3 { style: "margin:0 0 10px 0; color:#f8fafc; font-size:15px; letter-spacing:0.02em;", "{title}" }
            div { style: "display:flex; flex-direction:column; gap:8px;",
                for (label, value) in rows {
                    div { style: "display:flex; justify-content:space-between; gap:16px; align-items:baseline;",
                        span { style: "color:#8fb3c9; font-size:12px;", "{label}" }
                        span { style: "color:#eef6ff; font-size:13px; text-align:right; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace; font-variant-numeric:tabular-nums;", "{value}" }
                    }
                }
            }
        }
    }
}

fn info_line(label: &'static str, text: &'static str) -> Element {
    rsx! {
        div {
            strong { style: "color:#f8fafc; margin-right:8px;", "{label}:" }
            span { "{text}" }
        }
    }
}

fn opt_ms(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.1} ms"))
        .unwrap_or_else(|| "--".to_string())
}

fn opt_signed_ms(value: Option<i64>) -> String {
    value
        .map(|v| format!("{v:+} ms"))
        .unwrap_or_else(|| "--".to_string())
}

fn opt_i64_ms(value: Option<i64>) -> String {
    value
        .map(|v| format!("{v} ms"))
        .unwrap_or_else(|| "--".to_string())
}

fn opt_timestamp(value: Option<i64>) -> String {
    value
        .map(format_timestamp_ms_clock)
        .unwrap_or_else(|| "--".to_string())
}

fn human_bytes(bytes: u64) -> String {
    human_bytes_f64(bytes as f64)
}

fn human_bytes_f64(bytes: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.2} {}", units[unit])
    }
}

fn section_style() -> &'static str {
    "border:1px solid #274154; border-radius:16px; padding:14px; background:#081521;"
}

fn section_title_style() -> &'static str {
    "margin:0 0 12px 0; color:#f8fafc; font-size:15px;"
}

fn table_style() -> &'static str {
    "width:100%; border-collapse:collapse; font-size:13px;"
}

fn th_style() -> &'static str {
    "text-align:left; color:#8fb3c9; border-bottom:1px solid #274154; padding:8px 6px;"
}

fn td_style() -> &'static str {
    "padding:8px 6px; border-bottom:1px solid #132738; color:#dbe7f3;"
}

fn td_style_mono() -> &'static str {
    "padding:8px 6px; border-bottom:1px solid #132738; color:#dbe7f3; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace; font-variant-numeric:tabular-nums;"
}
