use dioxus::prelude::*;
use dioxus_signals::Signal;
use std::collections::BTreeMap;

use super::types::{
    BoardStatusEntry, FlightState, NetworkTopologyMsg, NetworkTopologyNodeKind,
    NetworkTopologyStatus,
};
use super::{format_timestamp_ms_clock, AlertMsg, FrontendNetworkMetrics, PersistentNotification};

#[component]
pub fn DetailedTab(
    metrics: Signal<FrontendNetworkMetrics>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    network_topology: Signal<NetworkTopologyMsg>,
    flight_state: Signal<FlightState>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    notifications: Signal<Vec<PersistentNotification>>,
    network_time_display: Option<String>,
    network_clock_delta_ms: Option<i64>,
    network_time_age_ms: Option<i64>,
) -> Element {
    let metrics_snapshot = metrics.read().clone();
    let boards = board_status.read().clone();
    let topology = network_topology.read().clone();
    let warnings_count = warnings.read().len();
    let errors_count = errors.read().len();
    let notifications_count = notifications.read().len();
    let now_ms = current_wallclock_ms();

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
    let simulated_nodes = topology
        .nodes
        .iter()
        .filter(|node| node.status == NetworkTopologyStatus::Simulated)
        .count();
    let online_links = topology
        .links
        .iter()
        .filter(|link| link.status == NetworkTopologyStatus::Online)
        .count();
    let offline_links = topology
        .links
        .iter()
        .filter(|link| link.status == NetworkTopologyStatus::Offline)
        .count();
    let router_nodes = topology
        .nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Router)
        .count();
    let endpoint_nodes = topology
        .nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Endpoint)
        .count();
    let side_nodes = topology
        .nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Side)
        .count();
    let board_nodes = topology
        .nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Board)
        .count();
    let max_board_age_ms = boards.iter().filter_map(|board| board.age_ms).max();
    let min_board_age_ms = boards.iter().filter_map(|board| board.age_ms).min();
    let avg_bytes_per_msg = if metrics_snapshot.ws_messages_total > 0 {
        Some(metrics_snapshot.ws_bytes_total as f64 / metrics_snapshot.ws_messages_total as f64)
    } else {
        None
    };
    let avg_rows_per_batch = if metrics_snapshot.telemetry_batches_total > 0 {
        Some(
            metrics_snapshot.telemetry_rows_total as f64
                / metrics_snapshot.telemetry_batches_total as f64,
        )
    } else {
        None
    };
    let ws_idle_ms = metrics_snapshot
        .last_ws_message_wall_ms
        .map(|ts| now_ms.saturating_sub(ts));
    let ws_connected_for_ms = if metrics_snapshot.ws_connected {
        metrics_snapshot
            .last_connect_wall_ms
            .map(|ts| now_ms.saturating_sub(ts))
    } else {
        None
    };
    let topology_age_ms = if topology.generated_ms > 0 {
        Some(now_ms.saturating_sub(topology.generated_ms as i64))
    } else {
        None
    };
    let topology_links_preview = topology
        .links
        .iter()
        .take(8)
        .map(|link| {
            (
                link.source.clone(),
                link.target.clone(),
                link.label.clone().unwrap_or_else(|| "--".to_string()),
                link.status,
            )
        })
        .collect::<Vec<_>>();
    let topology_nodes_only = topology
        .nodes
        .iter()
        .filter(|node| node.show_in_details)
        .collect::<Vec<_>>();
    let endpoint_rows = collect_endpoint_rows(&topology.nodes);

    rsx! {
        div { style: "padding:18px; height:100%; overflow-y:auto; overflow-x:hidden; color:#dbe7f3;",
            div { style: "display:grid; gap:14px; grid-template-columns:repeat(auto-fit, minmax(220px, 1fr)); margin-bottom:14px; align-items:start;",
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
                        ("Avg bytes/msg", avg_bytes_per_msg.map(|v| format!("{v:.1} B")).unwrap_or_else(|| "--".to_string())),
                        ("Avg rows/batch", avg_rows_per_batch.map(|v| format!("{v:.1}")).unwrap_or_else(|| "--".to_string())),
                    ],
                )}
                {metric_card(
                    "Session",
                    vec![
                        ("Rows per second", format!("{:.1}/s", metrics_snapshot.rows_per_sec)),
                        ("WS disconnects", metrics_snapshot.ws_disconnects_total.to_string()),
                        ("Connected for", opt_i64_ms(ws_connected_for_ms)),
                        ("WS idle", opt_i64_ms(ws_idle_ms)),
                        ("Last WS message", opt_timestamp(metrics_snapshot.last_ws_message_wall_ms)),
                        ("Last disconnect", metrics_snapshot.last_disconnect_reason.clone().unwrap_or_else(|| "None".to_string())),
                        ("Last connect", opt_timestamp(metrics_snapshot.last_connect_wall_ms)),
                    ],
                )}
                {metric_card(
                    "Mission State",
                    vec![
                        ("Flight state", flight_state.read().to_string().to_string()),
                        ("Rocket time", network_time_display.unwrap_or_else(|| "Unavailable".to_string())),
                        ("Clock delta", opt_signed_ms(network_clock_delta_ms)),
                        ("Server time age", opt_i64_ms(network_time_age_ms)),
                        ("Warnings", warnings_count.to_string()),
                        ("Errors", errors_count.to_string()),
                        ("Notifications", notifications_count.to_string()),
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
                        ("Simulated nodes", simulated_nodes.to_string()),
                        ("Online links", online_links.to_string()),
                        ("Offline links", offline_links.to_string()),
                        ("Topology age", opt_i64_ms(topology_age_ms)),
                        ("Topology simulated", yes_no(topology.simulated)),
                    ],
                )}
                {metric_card(
                    "Node Mix",
                    vec![
                        ("Routers", router_nodes.to_string()),
                        ("Endpoints", endpoint_nodes.to_string()),
                        ("Sides", side_nodes.to_string()),
                        ("Boards", board_nodes.to_string()),
                        ("Fastest board", opt_u64_ms(min_board_age_ms)),
                        ("Slowest board", opt_u64_ms(max_board_age_ms)),
                    ],
                )}
            }

            div { style: "display:grid; gap:14px; grid-template-columns:repeat(auto-fit, minmax(min(100%, 360px), 1fr)); align-items:start; width:100%;",
                div { style: "display:flex; flex-direction:column; gap:14px; min-width:0;",
                    div { style: section_style(),
                    h3 { style: section_title_style(), "Board Latency Detail" }
                    div { style: "width:100%; overflow-x:auto;",
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
                                    td { style: td_style(), "{board.display_name()}" }
                                    td { style: td_style_mono(), "{board.sender_id}" }
                                    td { style: td_style(), if board.seen { "yes" } else { "no" } }
                                    td { style: td_style_mono(), "{opt_i64_ms(board.age_ms.map(|v| v as i64))}" }
                                    td { style: td_style_mono(), "{board.last_seen_ms.map(|ts| format_timestamp_ms_clock(ts as i64)).unwrap_or_else(|| \"--\".to_string())}" }
                                }
                            }
                        }
                    }
                    }
                }
                    div { style: section_style(),
                    h3 { style: section_title_style(), "Topology Nodes" }
                    div { style: "width:100%; overflow-x:auto;",
                    table { style: table_style(),
                        thead {
                            tr {
                                th { style: th_style(), "Node" }
                                th { style: th_style(), "Kind" }
                                th { style: th_style(), "Status" }
                                th { style: th_style(), "Group" }
                                th { style: th_style(), "Sender" }
                            }
                        }
                        tbody {
                            for node in topology_nodes_only.iter() {
                                tr {
                                    td { style: td_style(), "{node.label}" }
                                    td { style: td_style(), "{format_kind(node.kind)}" }
                                    td { style: td_style(), "{format_status(node.status)}" }
                                    td { style: td_style(), "{node.group}" }
                                    td { style: td_style_mono(), "{node.sender_id.clone().unwrap_or_else(|| \"--\".to_string())}" }
                                }
                            }
                        }
                        }
                    }
                    }
                }
                div { style: "display:flex; flex-direction:column; gap:14px; min-width:0;",
                    div { style: section_style(),
                        h3 { style: section_title_style(), "Endpoint Ownership" }
                        div { style: "width:100%; overflow-x:auto;",
                        table { style: table_style(),
                            thead {
                                tr {
                                    th { style: th_style(), "Endpoint" }
                                    th { style: th_style(), "Host" }
                                }
                            }
                            tbody {
                                for (endpoint, owners) in endpoint_rows.iter() {
                                    tr {
                                        td { style: td_style_mono(), "{endpoint}" }
                                        td { style: td_style(), "{owners.join(\", \")}" }
                                    }
                                }
                                if endpoint_rows.is_empty() {
                                    tr {
                                        td { style: td_style(), colspan: "2", "No endpoint ownership data available." }
                                    }
                                }
                            }
                        }
                        }
                    }
                    div { style: section_style(),
                        h3 { style: section_title_style(), "Topology Links" }
                        div { style: "width:100%; overflow-x:auto;",
                        table { style: table_style(),
                            thead {
                                tr {
                                    th { style: th_style(), "Source" }
                                    th { style: th_style(), "Target" }
                                    th { style: th_style(), "Label" }
                                    th { style: th_style(), "Status" }
                                }
                            }
                            tbody {
                                for (source, target, label, status) in topology_links_preview.iter() {
                                    tr {
                                        td { style: td_style_mono(), "{node_label(source, &topology.nodes)}" }
                                        td { style: td_style_mono(), "{node_label(target, &topology.nodes)}" }
                                        td { style: td_style(), "{label}" }
                                        td { style: td_style(), "{format_status(*status)}" }
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

fn metric_card(title: &'static str, rows: Vec<(&'static str, String)>) -> Element {
    rsx! {
        div { style: "border:1px solid #274154; border-radius:16px; padding:14px; background:linear-gradient(180deg, #071521 0%, #0d1b2a 100%); box-shadow:0 14px 30px rgba(2, 6, 23, 0.28); min-width:0;",
            h3 { style: "margin:0 0 10px 0; color:#f8fafc; font-size:15px; letter-spacing:0.02em;", "{title}" }
            div { style: "display:flex; flex-direction:column; gap:8px;",
                for (label, value) in rows {
                    div { style: "display:flex; justify-content:space-between; gap:16px; align-items:flex-start; min-width:0;",
                        span { style: "color:#8fb3c9; font-size:12px; flex:0 0 auto;", "{label}" }
                        span { style: "color:#eef6ff; font-size:13px; text-align:right; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace; font-variant-numeric:tabular-nums; flex:1 1 auto; min-width:0; overflow-wrap:anywhere; word-break:break-word;", "{value}" }
                    }
                }
            }
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

fn opt_u64_ms(value: Option<u64>) -> String {
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
    "border:1px solid #274154; border-radius:16px; padding:14px; background:#081521; min-width:0;"
}

fn section_title_style() -> &'static str {
    "margin:0 0 12px 0; color:#f8fafc; font-size:15px;"
}

fn table_style() -> &'static str {
    "width:100%; border-collapse:collapse; font-size:13px; table-layout:fixed;"
}

fn th_style() -> &'static str {
    "text-align:left; color:#8fb3c9; border-bottom:1px solid #274154; padding:8px 6px;"
}

fn td_style() -> &'static str {
    "padding:8px 6px; border-bottom:1px solid #132738; color:#dbe7f3;"
}

fn td_style_mono() -> &'static str {
    "padding:8px 6px; border-bottom:1px solid #132738; color:#dbe7f3; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace; font-variant-numeric:tabular-nums; white-space:normal; overflow-wrap:anywhere; word-break:break-word;"
}

fn format_status(status: NetworkTopologyStatus) -> &'static str {
    match status {
        NetworkTopologyStatus::Online => "online",
        NetworkTopologyStatus::Offline => "offline",
        NetworkTopologyStatus::Simulated => "simulated",
    }
}

fn format_kind(kind: NetworkTopologyNodeKind) -> &'static str {
    match kind {
        NetworkTopologyNodeKind::Router => "router",
        NetworkTopologyNodeKind::Endpoint => "endpoint",
        NetworkTopologyNodeKind::Side => "side",
        NetworkTopologyNodeKind::Board => "board",
    }
}

fn node_label(id: &str, nodes: &[super::types::NetworkTopologyNode]) -> String {
    nodes
        .iter()
        .find(|node| node.id == id)
        .map(|node| node.label.clone())
        .unwrap_or_else(|| id.to_string())
}

fn collect_endpoint_rows(
    nodes: &[super::types::NetworkTopologyNode],
) -> Vec<(String, Vec<String>)> {
    let mut by_endpoint = BTreeMap::<String, Vec<String>>::new();
    for node in nodes {
        let Some(owner) = endpoint_owner_label(node) else {
            continue;
        };
        for endpoint in &node.endpoints {
            by_endpoint
                .entry(endpoint.clone())
                .or_default()
                .push(owner.clone());
        }
    }

    by_endpoint
        .into_iter()
        .map(|(endpoint, mut owners)| {
            owners.sort();
            owners.dedup();
            (endpoint, owners)
        })
        .collect()
}

fn endpoint_owner_label(node: &super::types::NetworkTopologyNode) -> Option<String> {
    match node.kind {
        NetworkTopologyNodeKind::Router => Some(node.label.clone()),
        NetworkTopologyNodeKind::Board => Some(node.label.clone()),
        NetworkTopologyNodeKind::Endpoint | NetworkTopologyNodeKind::Side => None,
    }
}

fn yes_no(value: bool) -> String {
    if value {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

fn current_wallclock_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
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
