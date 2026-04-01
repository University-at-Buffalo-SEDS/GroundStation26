use dioxus::prelude::*;
use dioxus_signals::Signal;
use std::collections::{BTreeMap, HashSet};

use super::types::{
    BoardStatusEntry, FlightState, NetworkTopologyMsg, NetworkTopologyNodeKind,
    NetworkTopologyStatus,
};
use super::{
    AlertMsg, FrontendNetworkMetrics, NetworkTimeSync, PersistentNotification,
    compensated_network_time_ms, format_network_time, format_timestamp_ms_clock, monotonic_now_ms,
    translate_text,
};

#[component]
pub fn DetailedTab(
    metrics: Signal<FrontendNetworkMetrics>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    network_topology: Signal<NetworkTopologyMsg>,
    flight_state: Signal<FlightState>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    notifications: Signal<Vec<PersistentNotification>>,
    network_time: Signal<Option<NetworkTimeSync>>,
) -> Element {
    let tick = use_signal(|| 0u64);
    {
        let mut tick = tick;
        use_effect(move || {
            spawn(async move {
                loop {
                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(100).await;

                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                    let next_tick = {
                        let current_tick = *tick.read();
                        current_tick.wrapping_add(1)
                    };
                    tick.set(next_tick);
                }
            });
        });
    }
    let _tick_snapshot = *tick.read();
    let metrics_snapshot = metrics.read().clone();
    let boards = board_status.read().clone();
    let seen_boards = boards
        .iter()
        .filter(|board| board.seen)
        .cloned()
        .collect::<Vec<_>>();
    let topology = network_topology.read().clone();
    let network_time_snapshot = *network_time.read();
    let visible_topology_nodes = visible_topology_nodes(&topology.nodes);
    let visible_topology_links = collapse_visible_links(&topology.nodes, &topology.links);
    let warnings_count = warnings.read().len();
    let errors_count = errors.read().len();
    let notifications_count = notifications.read().len();
    let now_ms = current_wallclock_ms();

    let board_seen = seen_boards.len();
    let online_nodes = visible_topology_nodes
        .iter()
        .filter(|node| node.status == NetworkTopologyStatus::Online)
        .count();
    let offline_nodes = visible_topology_nodes
        .iter()
        .filter(|node| node.status == NetworkTopologyStatus::Offline)
        .count();
    let simulated_nodes = visible_topology_nodes
        .iter()
        .filter(|node| node.status == NetworkTopologyStatus::Simulated)
        .count();
    let online_links = visible_topology_links
        .iter()
        .filter(|link| link.status == NetworkTopologyStatus::Online)
        .count();
    let offline_links = visible_topology_links
        .iter()
        .filter(|link| link.status == NetworkTopologyStatus::Offline)
        .count();
    let router_nodes = visible_topology_nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Router)
        .count();
    let board_nodes = visible_topology_nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Board)
        .count();
    let max_board_age_ms = seen_boards.iter().filter_map(|board| board.age_ms).max();
    let min_board_age_ms = seen_boards.iter().filter_map(|board| board.age_ms).min();
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
    let network_time_display = network_time_snapshot
        .map(compensated_network_time_ms)
        .map(format_network_time);
    let network_clock_delta_ms = network_time_snapshot
        .map(compensated_network_time_ms)
        .map(|ms| current_wallclock_ms().saturating_sub(ms));
    let network_time_age_ms = network_time_snapshot.map(|sync| {
        (monotonic_now_ms() - sync.received_mono_ms)
            .max(0.0)
            .round() as i64
    });
    let topology_age_ms = if topology.generated_ms > 0 {
        Some(now_ms.saturating_sub(topology.generated_ms as i64))
    } else {
        None
    };
    let topology_links_preview = visible_topology_links
        .iter()
        .take(12)
        .map(|link| (link.source.clone(), link.target.clone(), link.status))
        .collect::<Vec<_>>();
    let endpoint_rows = collect_endpoint_rows(&topology.nodes, &topology.links);
    let board_route_rows =
        collect_board_route_rows(&visible_topology_nodes, &visible_topology_links);

    rsx! {
        div { style: "padding:18px; height:100%; overflow-y:auto; overflow-x:hidden; color:#dbe7f3;",
            div { style: "display:grid; gap:14px; grid-template-columns:repeat(auto-fit, minmax(220px, 1fr)); margin-bottom:14px; align-items:start;",
                {metric_card(
                    "Frontend ↔ Backend",
                    vec![
                        ("Status", if metrics_snapshot.ws_connected { translate_text("Connected") } else { translate_text("Disconnected") }),
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
                        ("Last disconnect", metrics_snapshot.last_disconnect_reason.clone().map(|v| translate_text(&v)).unwrap_or_else(|| translate_text("None"))),
                        ("Last connect", opt_timestamp(metrics_snapshot.last_connect_wall_ms)),
                    ],
                )}
                {metric_card(
                    "Mission State",
                    vec![
                        ("Flight state", translate_text(&flight_state.read().to_string())),
                        ("Rocket time", network_time_display.unwrap_or_else(|| translate_text("Unavailable"))),
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
                        ("Boards seen", board_seen.to_string()),
                        ("Visible nodes", visible_topology_nodes.len().to_string()),
                        ("Visible links", visible_topology_links.len().to_string()),
                        ("Routers", router_nodes.to_string()),
                        ("Boards", board_nodes.to_string()),
                        ("Online nodes", online_nodes.to_string()),
                        ("Offline nodes", offline_nodes.to_string()),
                        ("Simulated nodes", simulated_nodes.to_string()),
                        ("Online links", online_links.to_string()),
                        ("Offline links", offline_links.to_string()),
                        ("Topology age", opt_i64_ms(topology_age_ms)),
                        ("Topology simulated", translate_text(&yes_no(topology.simulated))),
                    ],
                )}
                {metric_card(
                    "Board Timing",
                    vec![
                        ("Fastest board", opt_u64_ms(min_board_age_ms)),
                        ("Slowest board", opt_u64_ms(max_board_age_ms)),
                    ],
                )}
            }

            div { style: "display:grid; gap:14px; grid-template-columns:repeat(auto-fit, minmax(min(100%, 340px), 1fr)); align-items:start; width:100%;",
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
                            for board in seen_boards.iter() {
                                tr {
                                    td { style: td_style(), "{board.display_name()}" }
                                    td { style: td_style_mono(), "{board.sender_id}" }
                                    td { style: td_style(), "yes" }
                                    td { style: td_style_mono(), "{opt_i64_ms(board.age_ms.map(|v| v as i64))}" }
                                    td { style: td_style_mono(), "{board.last_seen_ms.map(|ts| format_timestamp_ms_clock(ts as i64)).unwrap_or_else(|| \"--\".to_string())}" }
                                }
                            }
                            if seen_boards.is_empty() {
                                tr {
                                    td { style: td_style(), colspan: "5", "No boards have been observed yet." }
                                }
                            }
                        }
                    }
                    }
                }
                    div { style: section_style(),
                    h3 { style: section_title_style(), "Board Routes" }
                    div { style: "width:100%; overflow-x:auto;",
                    table { style: table_style(),
                        thead {
                            tr {
                                th { style: th_style(), "Board" }
                                th { style: th_style(), "Upstream" }
                                th { style: th_style(), "Status" }
                                th { style: th_style(), "Sender" }
                            }
                        }
                        tbody {
                            for (label, upstream, status, sender_id) in board_route_rows.iter() {
                                tr {
                                    td { style: td_style(), "{label}" }
                                    td { style: td_style(), "{upstream}" }
                                    td { style: td_style(), "{format_status(*status)}" }
                                    td { style: td_style_mono(), "{sender_id}" }
                                }
                            }
                            if board_route_rows.is_empty() {
                                tr {
                                    td { style: td_style(), colspan: "4", "No board routes are visible yet." }
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
                                    th { style: th_style(), "Path" }
                                    th { style: th_style(), "Status" }
                                }
                            }
                            tbody {
                                for (source, target, status) in topology_links_preview.iter() {
                                    tr {
                                        td { style: td_style_mono(), "{node_label(source, &visible_topology_nodes)} -> {node_label(target, &visible_topology_nodes)}" }
                                        td { style: td_style(), "{format_status(*status)}" }
                                    }
                                }
                                if topology_links_preview.is_empty() {
                                    tr {
                                        td { style: td_style(), colspan: "2", "No topology links are visible yet." }
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

fn node_label(id: &str, nodes: &[super::types::NetworkTopologyNode]) -> String {
    nodes
        .iter()
        .find(|node| node.id == id)
        .map(|node| node.label.clone())
        .unwrap_or_else(|| id.to_string())
}

fn visible_topology_nodes(
    nodes: &[super::types::NetworkTopologyNode],
) -> Vec<super::types::NetworkTopologyNode> {
    nodes
        .iter()
        .filter(|node| {
            matches!(
                node.kind,
                NetworkTopologyNodeKind::Router | NetworkTopologyNodeKind::Board
            )
        })
        .cloned()
        .collect()
}

fn collapse_visible_links(
    nodes: &[super::types::NetworkTopologyNode],
    links: &[super::types::NetworkTopologyLink],
) -> Vec<super::types::NetworkTopologyLink> {
    let visible = visible_topology_nodes(nodes);
    let visible_ids = visible
        .iter()
        .map(|node| node.id.clone())
        .collect::<HashSet<_>>();
    let mut collapsed = BTreeMap::<(String, String), NetworkTopologyStatus>::new();
    for link in links {
        if !visible_ids.contains(&link.source) || !visible_ids.contains(&link.target) {
            continue;
        }
        let key = if link.source < link.target {
            (link.source.clone(), link.target.clone())
        } else {
            (link.target.clone(), link.source.clone())
        };
        collapsed
            .entry(key)
            .and_modify(|existing| *existing = merge_link_status(*existing, link.status))
            .or_insert(link.status);
    }

    collapsed
        .into_iter()
        .map(
            |((source, target), status)| super::types::NetworkTopologyLink {
                source,
                target,
                label: None,
                status,
            },
        )
        .collect()
}

fn merge_link_status(a: NetworkTopologyStatus, b: NetworkTopologyStatus) -> NetworkTopologyStatus {
    use NetworkTopologyStatus::{Offline, Online, Simulated};

    match (a, b) {
        (Offline, _) | (_, Offline) => Offline,
        (Simulated, _) | (_, Simulated) => Simulated,
        _ => Online,
    }
}

fn collect_endpoint_rows(
    nodes: &[super::types::NetworkTopologyNode],
    links: &[super::types::NetworkTopologyLink],
) -> Vec<(String, Vec<String>)> {
    let mut by_endpoint = BTreeMap::<String, Vec<String>>::new();
    let mut adjacency = BTreeMap::<String, Vec<String>>::new();
    for link in links {
        adjacency
            .entry(link.source.clone())
            .or_default()
            .push(link.target.clone());
        adjacency
            .entry(link.target.clone())
            .or_default()
            .push(link.source.clone());
    }

    for node in nodes {
        for endpoint in &node.endpoints {
            if let Some(owner) = endpoint_owner_label(node, endpoint) {
                by_endpoint
                    .entry(endpoint.clone())
                    .or_default()
                    .push(owner.clone());
            }
        }

        if node.kind != NetworkTopologyNodeKind::Endpoint {
            continue;
        }

        let endpoint_name = node
            .endpoints
            .first()
            .cloned()
            .unwrap_or_else(|| node.label.clone());
        for owner in endpoint_route_owners(node, nodes, &adjacency, &endpoint_name) {
            by_endpoint
                .entry(endpoint_name.clone())
                .or_default()
                .push(owner);
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

fn endpoint_route_owners(
    endpoint_node: &super::types::NetworkTopologyNode,
    nodes: &[super::types::NetworkTopologyNode],
    adjacency: &BTreeMap<String, Vec<String>>,
    endpoint_name: &str,
) -> Vec<String> {
    let mut owners = Vec::new();
    let mut queue = std::collections::VecDeque::<String>::new();
    let mut visited = HashSet::<String>::new();
    visited.insert(endpoint_node.id.clone());

    if let Some(neighbors) = adjacency.get(&endpoint_node.id) {
        for neighbor in neighbors {
            queue.push_back(neighbor.clone());
        }
    }

    while let Some(current) = queue.pop_front() {
        if !visited.insert(current.clone()) {
            continue;
        }
        let Some(node) = nodes.iter().find(|node| node.id == current) else {
            continue;
        };
        if let Some(owner) = endpoint_owner_label(node, endpoint_name) {
            owners.push(owner);
            continue;
        }
        if let Some(neighbors) = adjacency.get(&current) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    owners.sort();
    owners.dedup();
    owners
}

fn collect_board_route_rows(
    nodes: &[super::types::NetworkTopologyNode],
    links: &[super::types::NetworkTopologyLink],
) -> Vec<(String, String, NetworkTopologyStatus, String)> {
    let labels = nodes
        .iter()
        .map(|node| (node.id.clone(), node.label.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut adjacency = BTreeMap::<String, Vec<(String, NetworkTopologyStatus)>>::new();
    for link in links {
        adjacency
            .entry(link.source.clone())
            .or_default()
            .push((link.target.clone(), link.status));
        adjacency
            .entry(link.target.clone())
            .or_default()
            .push((link.source.clone(), link.status));
    }

    let mut rows = nodes
        .iter()
        .filter(|node| node.kind == NetworkTopologyNodeKind::Board)
        .map(|node| {
            let upstream = adjacency.get(&node.id).and_then(|neighbors| {
                neighbors
                    .iter()
                    .find(|(neighbor, _)| {
                        nodes.iter().any(|candidate| {
                            candidate.id == *neighbor
                                && matches!(
                                    candidate.kind,
                                    NetworkTopologyNodeKind::Router
                                        | NetworkTopologyNodeKind::Board
                                )
                        })
                    })
                    .cloned()
            });
            let (upstream_label, status) = upstream
                .map(|(neighbor, status)| {
                    (labels.get(&neighbor).cloned().unwrap_or(neighbor), status)
                })
                .unwrap_or_else(|| ("--".to_string(), node.status));
            (
                node.label.clone(),
                upstream_label,
                status,
                node.sender_id.clone().unwrap_or_else(|| "--".to_string()),
            )
        })
        .collect::<Vec<_>>();

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn endpoint_owner_label(
    node: &super::types::NetworkTopologyNode,
    endpoint_name: &str,
) -> Option<String> {
    match node.kind {
        NetworkTopologyNodeKind::Router | NetworkTopologyNodeKind::Board
            if node
                .endpoints
                .iter()
                .any(|endpoint| endpoint == endpoint_name) =>
        {
            Some(node.label.clone())
        }
        NetworkTopologyNodeKind::Endpoint | NetworkTopologyNodeKind::Side => None,
        _ => None,
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
