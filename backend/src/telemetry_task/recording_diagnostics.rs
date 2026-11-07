//! Recording-only observations. SDK exports can wait and prune expired routes:
//! they run in one blocking worker, never under an application/recording lock.
//! Only the DB owner can accept its generation-fenced result into a file.
use super::*;
use crate::types::NetworkTopologyMsg;
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    time::Instant,
};
use tokio::task::JoinHandle;

pub(crate) const MAX_BATCH_BYTES: usize = 16 * 1024;
pub(crate) const NETWORK: &str = "REC_NETWORK_STATE";
pub(crate) const SIDE: &str = "REC_SIDE_COUNTERS";
pub(crate) const RATES: &str = "REC_SIDE_RATES";
pub(crate) const DIE: &str = "REC_BOARD_DIE_TEMPERATURE";
pub(crate) const CPU: &str = "REC_HOST_CPU_TEMPERATURE";
pub(crate) const GAP: &str = "REC_DIAGNOSTIC_GAP";

static RAW_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
struct Limited(Vec<u8>);
impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > MAX_BATCH_BYTES)
        {
            return Err(io::Error::other(
                "diagnostic payload exceeds recording bound",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn bounded_json(value: &Value) -> Result<String, String> {
    let mut out = Limited(Vec::new());
    serde_json::to_writer(&mut out, value).map_err(|e| e.to_string())?;
    String::from_utf8(out.0).map_err(|e| e.to_string())
}
fn portable_exact_integers(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                portable_exact_integers(item);
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                portable_exact_integers(item);
            }
        }
        Value::Number(n)
            if n.as_u64().is_some_and(|v| v > 9_007_199_254_740_991)
                || n.as_i64().is_some_and(|v| v < -9_007_199_254_740_991) =>
        {
            *value = Value::String(n.to_string());
        }
        _ => {}
    }
}
fn row(
    wall: i64,
    source: Option<i64>,
    sender: &str,
    kind: &str,
    values: Value,
    mut payload: Value,
) -> Result<DbWrite, String> {
    portable_exact_integers(&mut payload);
    Ok(DbWrite::Telemetry {
        timestamp_ms: wall,
        source_timestamp_ms: source,
        sender_id: sender.to_owned(),
        data_type: kind.to_owned(),
        values_json: Some(bounded_json(&values)?),
        payload_json: bounded_json(&payload)?,
    })
}
fn gap(wall: i64, process: &str, reason: &str, lost: u64) -> DbWrite {
    row(
        wall,
        None,
        if crate::rrls::is_rrls() { "RRLS" } else { "GS" },
        GAP,
        json!([null]),
        json!({"version":1,"process":process,
        "quality":"gap","reason":reason,"lost_batches_decimal":lost.to_string(),
        "raw_health_queue_drops_decimal":RAW_DROPS.load(std::sync::atomic::Ordering::Relaxed).to_string()}),
    )
    .expect("bounded gap literal")
}
fn batch_cost(writes: &[DbWrite]) -> usize {
    writes.iter().fold(0usize, |n, w| {
        n.saturating_add(match w {
            DbWrite::Telemetry {
                data_type,
                sender_id,
                values_json,
                payload_json,
                ..
            } => 128usize
                .saturating_add(data_type.len())
                .saturating_add(sender_id.len())
                .saturating_add(values_json.as_ref().map_or(0, String::len))
                .saturating_add(payload_json.len()),
            _ => MAX_BATCH_BYTES + 1,
        })
    })
}
pub(super) fn raw_health(
    _state: &AppState,
    tx: &mpsc::Sender<DbQueueItem>,
    pkt: &Packet,
    sender: &str,
) {
    // Called only after the existing packet-specific validation, at the existing
    // low-rate health producers. No per-DAQ-data-packet snapshot/serialization.
    let source = match pkt.timestamp() {
        0 | u64::MAX => None,
        v => i64::try_from(v).ok(),
    };
    let write = DbWrite::Telemetry {
        timestamp_ms: get_current_timestamp_ms() as i64,
        source_timestamp_ms: source,
        data_type: pkt.data_type().as_str().to_string(),
        sender_id: sender.to_owned(),
        values_json: Some("[]".into()),
        payload_json: payload_json_from_pkt(pkt),
    };
    if tx.try_send(DbQueueItem::Write(write)).is_err() {
        // Existing recording overflow accounting owns admission-loss reporting.
        let _ = RAW_DROPS.try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| Some(n.saturating_add(1)),
        );
    }
}

#[derive(Default)]
struct SampleHistory {
    traffic: crate::network_traffic::Sampler,
    sides: Vec<(usize, String)>,
    topology_shape: Option<String>,
}
struct Finished {
    generation: u64,
    sampler: SampleHistory,
    writes: Result<Vec<DbWrite>, String>,
}
/// One job at most, and only one recording owner consumes outcomes. A changed
/// mode/control retires the lease even if a blocking SDK export is still running.
pub(super) struct Collector {
    generation: Option<u64>,
    scope: Option<String>,
    process: String,
    origin: Instant,
    next: tokio::time::Instant,
    pending: Option<JoinHandle<Finished>>,
    sampler: SampleHistory,
    lost: u64,
}
impl Collector {
    pub(super) fn new() -> Self {
        Self {
            generation: Some(1),
            scope: None,
            process: format!("{:032x}", rand::random::<u128>()),
            origin: Instant::now(),
            next: tokio::time::Instant::now(),
            pending: None,
            sampler: Default::default(),
            lost: 0,
        }
    }
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.and_then(|n| n.checked_add(1));
        self.scope = None;
        self.sampler = Default::default();
    }
    pub(super) async fn poll(&mut self, state: &Arc<AppState>, path: Option<&str>) -> Vec<DbWrite> {
        if self.scope.as_deref() != path {
            self.invalidate();
            self.scope = path.map(str::to_owned);
        }
        let now = tokio::time::Instant::now();
        let due = now >= self.next;
        let mut out = Vec::new();
        let mut start_next = false;
        if due && self.pending.as_ref().is_some_and(JoinHandle::is_finished) {
            let finished = self.pending.take().unwrap().await;
            self.next = now + Duration::from_secs(1);
            start_next = true;
            match finished {
                Ok(result)
                    if Some(result.generation) == self.generation && self.scope.is_some() =>
                {
                    self.sampler = result.sampler;
                    match result.writes {
                        Ok(writes) if batch_cost(&writes) <= MAX_BATCH_BYTES => {
                            self.lost = 0;
                            out = writes;
                        }
                        Ok(_) => {
                            self.lost = self.lost.saturating_add(1);
                            out.push(gap(
                                get_current_timestamp_ms() as i64,
                                &self.process,
                                "internal batch bound refusal",
                                self.lost,
                            ));
                        }
                        Err(reason) => {
                            self.lost = self.lost.saturating_add(1);
                            out.push(gap(
                                get_current_timestamp_ms() as i64,
                                &self.process,
                                &reason,
                                self.lost,
                            ));
                        }
                    }
                }
                _ => self.lost = self.lost.saturating_add(1),
            }
        } else if due && self.scope.is_some() && self.pending.is_some() {
            self.next = now + Duration::from_secs(1);
            out.push(gap(
                get_current_timestamp_ms() as i64,
                &self.process,
                "SDK snapshot pending; no fresh network capture",
                self.lost,
            ));
        }
        if let Some(generation) = self.generation
            && self.scope.is_some()
            && self.pending.is_none()
            && (due || start_next)
        {
            self.next = now + Duration::from_secs(1);
            let state = state.clone();
            let process = self.process.clone();
            let origin = self.origin;
            let prior_lost = self.lost;
            let mut sampler = std::mem::take(&mut self.sampler);
            self.pending = Some(tokio::task::spawn_blocking(move || {
                let writes = capture(
                    &state,
                    &process,
                    origin,
                    &mut sampler,
                    generation,
                    prior_lost,
                );
                Finished {
                    generation,
                    sampler,
                    writes,
                }
            }));
        }
        if self.generation.is_none() && self.scope.is_some() && due {
            self.next = now + Duration::from_secs(1);
            out.push(gap(
                get_current_timestamp_ms() as i64,
                &self.process,
                "recording generation exhausted; collector disabled",
                self.lost,
            ));
        }
        out
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        if let Some(job) = self.pending.take() {
            job.abort();
        }
    }
}

// Lossless recording-local projection: never merge different node facts merely
// because sender names match. All exact variants remain in the table.
fn node_value(node: &sedsnet::discovery::TopologyBoardNode) -> Value {
    json!({"sender":node.sender_id,"connections":node.connections,
        "reachable_endpoints":node.reachable_endpoints.iter().map(|v|v.as_u32()).collect::<Vec<_>>(),
        "timesync_sources":node.reachable_timesync_sources})
}
fn intern_node(table: &mut Vec<Value>, node: Value) -> usize {
    if let Some(index) = table.iter().position(|old| old == &node) {
        index
    } else {
        table.push(node);
        table.len() - 1
    }
}
fn omit_unknown_optionals(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, v| !v.is_null());
            for v in fields.values_mut() {
                omit_unknown_optionals(v);
            }
        }
        Value::Array(items) => {
            for v in items {
                omit_unknown_optionals(v);
            }
        }
        _ => {}
    }
}
fn remove_route_ages(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.remove("age_ms");
            fields.remove("last_seen_router_ms");
            for v in fields.values_mut() {
                remove_route_ages(v);
            }
        }
        Value::Array(items) => {
            for v in items {
                remove_route_ages(v);
            }
        }
        _ => {}
    }
}
fn qualified_die_temperature(
    reported: Option<i32>,
    wire_age: u64,
    receipt_age: u64,
    header: Option<u64>,
    network: Option<i64>,
) -> (Option<i32>, u64, &'static str) {
    let receipt_total = receipt_age.saturating_add(wire_age);
    if reported.is_none() {
        return (None, receipt_total, "sensor_unavailable");
    }
    let Some(source) = header.filter(|v| *v != 0 && *v != u64::MAX && i64::try_from(*v).is_ok())
    else {
        return (None, receipt_total, "source_clock_unknown");
    };
    let Some(now) = network
        .and_then(|v| u64::try_from(v).ok())
        .filter(|v| *v > 0)
    else {
        return (None, receipt_total, "network_clock_unknown");
    };
    let Some(delay) = now.checked_sub(source) else {
        return (None, receipt_total, "source_clock_future");
    };
    let age = receipt_age.max(delay).saturating_add(wire_age);
    if age > 15000 {
        (None, age, "stale")
    } else {
        (reported, age, "observed")
    }
}
fn admit_observation_groups(
    wall: i64,
    source: Option<i64>,
    sender: &str,
    context: Value,
    network: Result<DbWrite, String>,
    fallback: Value,
    base: Vec<DbWrite>,
) -> Vec<DbWrite> {
    if let Ok(network) = network
        && batch_cost(std::slice::from_ref(&network)).saturating_add(batch_cost(&base))
            <= MAX_BATCH_BYTES
    {
        let mut result = vec![network];
        result.extend(base);
        return result;
    }
    let rich_gap = row(wall, source, sender, GAP, json!([null]), fallback);
    if let Ok(gap) = rich_gap
        && batch_cost(std::slice::from_ref(&gap)).saturating_add(batch_cost(&base))
            <= MAX_BATCH_BYTES
    {
        let mut result = vec![gap];
        result.extend(base);
        return result;
    }
    let mut metadata = json!({"context":context,"topology_valid":false,"quality":"gap","scope":"capture",
        "reason":"Recording batch oversized; no partial graph; fitting temperatures/counters retained",
        "health_snapshot_omitted":true,"omitted_stream_rows":base.len()});
    let witness = row(wall, source, sender, GAP, json!([null]), metadata.clone())
        .expect("fixed context witness fits");
    let reserve = batch_cost(std::slice::from_ref(&witness));
    let (temperatures, others): (Vec<_>, Vec<_>) = base.into_iter().partition(
        |w| matches!(w,DbWrite::Telemetry{data_type,..}if data_type==DIE||data_type==CPU),
    );
    let total = temperatures.len() + others.len();
    let mut selected = Vec::new();
    let mut used = reserve;
    for write in temperatures.into_iter().chain(others) {
        let cost = batch_cost(std::slice::from_ref(&write));
        if used.saturating_add(cost) <= MAX_BATCH_BYTES {
            used = used.saturating_add(cost);
            selected.push(write);
        }
    }
    metadata["omitted_stream_rows"] = (total - selected.len()).into();
    let witness = row(wall, source, sender, GAP, json!([null]), metadata)
        .expect("fixed context witness fits");
    let mut result = vec![witness];
    result.extend(selected);
    result
}
// A host cache reading carries monotonic age, not an original packet UTC.
fn cached_host_row(
    wall: i64,
    board: &crate::types::NetworkBoardHealthEntry,
    context: &Value,
) -> Result<DbWrite, String> {
    let value = board
        .cpu_temperature_centi_c
        .filter(|_| board.cpu_temperature_age_ms <= 15000);
    row(
        wall,
        None,
        &board.sender_id,
        CPU,
        json!([value.map(|n| n as f64 / 100.)]),
        json!({"context":context,"centi_c":value,"sample_age_ms":board.cpu_temperature_age_ms,
            "valid":value.is_some(),"source_clock":"cached_host_monotonic_age; original UTC unavailable",
            "quality":if value.is_some(){"observed"}else{"stale_or_unsupported"}}),
    )
}
fn capture(
    state: &AppState,
    process: &str,
    origin: Instant,
    sampler: &mut SampleHistory,
    generation: u64,
    prior_lost: u64,
) -> Result<Vec<DbWrite>, String> {
    let started_wall = get_current_timestamp_ms() as i64;
    let started = origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let router = state
        .topology_router
        .get()
        .ok_or("network router unavailable")?;
    // Both exports use SDK Core locks and normal topology pruning. No AppState
    // lock is held. There is no outer trylock masquerading as nonwaiting SDK IO.
    let topology = router.export_topology();
    let runtime = router.export_runtime_stats();
    let network = router
        .network_time()
        .and_then(|t| t.unix_time_ms)
        .and_then(|v| i64::try_from(v).ok())
        .filter(|v| *v > 0);
    let wall = get_current_timestamp_ms() as i64;
    let (mut health, source_stamps) = {
        let mut tracker = state
            .network_health
            .try_lock()
            .map_err(|_| "health snapshot busy")?;
        (
            tracker.recording_snapshot(Instant::now()),
            tracker.recording_source_stamps(),
        )
    };
    crate::host_temperature::append_local(
        &mut health,
        wall.max(0) as u64,
        crate::flight_sim::sim_mode_enabled(),
    );
    let mut mirrored = NetworkTopologyMsg {
        generated_ms: wall.max(0) as u64,
        simulated: crate::flight_sim::sim_mode_enabled(),
        health,
        nodes: Vec::new(),
        links: Vec::new(),
        traffic: None,
    };
    crate::primary_mirror::project_topology(state, &mut mirrored);
    health = mirrored.health;
    let traffic = sampler.traffic.sample(Instant::now(), &runtime.sides);
    let sender = router.sender().to_string();
    let mut node_table = Vec::new();
    let nodes: Vec<_> = topology
        .routers
        .iter()
        .map(|n| intern_node(&mut node_table, node_value(n)))
        .collect();
    let routes:Vec<_>=topology.routes.iter().map(|route| {
        let announcers:Vec<_>=route.announcers.iter().map(|announcer| {
            let router_refs:Vec<_>=announcer.routers.iter().map(|n|intern_node(&mut node_table,node_value(n))).collect();
            json!({"sender":announcer.sender_id,"age_ms":announcer.age_ms,"last_seen_router_ms":announcer.last_seen_ms,
                "reachable_endpoints":announcer.reachable_endpoints.iter().map(|v|v.as_u32()).collect::<Vec<_>>(),
                "reachable_network_variables":announcer.reachable_network_variables.iter().map(|v|v.as_u32()).collect::<Vec<_>>(),
                "timesync_sources":announcer.reachable_timesync_sources,"router_refs":router_refs})
        }).collect();
        json!({"side_id":route.side_id,"side_name":route.side_name,"age_ms":route.age_ms,"last_seen_router_ms":route.last_seen_ms,
            "reachable_endpoints":route.reachable_endpoints.iter().map(|v|v.as_u32()).collect::<Vec<_>>(),
            "reachable_network_variables":route.reachable_network_variables.iter().map(|v|v.as_u32()).collect::<Vec<_>>(),
            "timesync_sources":route.reachable_timesync_sources,"announcers":announcers})
    }).collect();
    let links: Vec<_> = topology
        .links
        .iter()
        .map(|l| json!({"source":l.source,"target":l.target}))
        .collect();
    let mut shape = json!({"nodes":nodes,"node_table":node_table,"links":links,"routes":routes});
    remove_route_ages(&mut shape);
    let topology_shape = bounded_json(&shape);
    let topology_changed = topology_shape
        .as_ref()
        .ok()
        .map(|shape| sampler.topology_shape.as_ref() != Some(shape));
    if let Ok(shape) = topology_shape {
        sampler.topology_shape = Some(shape);
    }
    let snapshot_id = format!("{process}:{generation}:{started}");
    let context = json!({"version":1,"process":process,"snapshot_id":snapshot_id,"prior_lost_batches_decimal":prior_lost.to_string(),"received_utc_ms":wall,
        "network_utc_ms":network,"capture_started_received_utc_ms":started_wall,"recording_generation_decimal":generation.to_string(),"capture_monotonic_ms_decimal":started.to_string(),
        "capture_duration_ms_decimal":origin.elapsed().as_millis().saturating_sub(u128::from(started)).to_string(),
        "raw_health_queue_drops_decimal":RAW_DROPS.load(std::sync::atomic::Ordering::Relaxed).to_string(),
        "simulated":mirrored.simulated,"expected_launch_journal_match":"unknown_not_sampled","quality":"observed_snapshot"});
    let mut health_json = serde_json::to_value(&health).map_err(|e| e.to_string())?;
    for entry in health_json.as_array_mut().into_iter().flatten() {
        if let Some(sender) = entry["sender_id"].as_str()
            && let Some((_, stamp)) = source_stamps.iter().find(|(s, _)| s == sender)
        {
            entry["diagnostic_header_utc_decimal"] = stamp
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null);
        }
    }
    omit_unknown_optionals(&mut health_json);
    let network_payload = json!({"context":context,"topology_valid":true,
            "advertised_endpoints":topology.advertised_endpoints.iter().map(|v|v.as_u32()).collect::<Vec<_>>(),
            "advertised_timesync_sources":topology.advertised_timesync_sources,
            "router_clock_fields":"last_seen_router_ms and announce deadlines are SDK router-monotonic, not UTC",
            "announce_interval_ms":topology.current_announce_interval_ms,"next_announce_router_ms":topology.next_announce_ms,"changed_since_previous_observation":topology_changed,"topology_encoding":"node-table-v1","nodes":nodes,"node_table":node_table,"links":links,"routes":routes,
            "mirrored_nodes":mirrored.nodes,"mirrored_links":mirrored.links,
            "health_encoding":"sparse-optional-v1; absent optional field means Unknown, never zero",
            "health":health_json,"queues":{"tx_len":runtime.queues.tx_len,"rx_len":runtime.queues.rx_len,
                "shared_bytes":runtime.queues.shared_queue_bytes_used,"replay_len":runtime.queues.replay_len,
                "tx_bytes":runtime.queues.tx_bytes,"rx_bytes":runtime.queues.rx_bytes,"replay_bytes":runtime.queues.replay_bytes,
                "recent_rx_len":runtime.queues.recent_rx_len,"recent_rx_bytes":runtime.queues.recent_rx_bytes,
                "reliable_rx_buffered_len":runtime.queues.reliable_rx_buffered_len,"reliable_rx_buffered_bytes":runtime.queues.reliable_rx_buffered_bytes},
            "reliable":{"return_routes":runtime.reliable.reliable_return_route_count,"end_to_end_pending":runtime.reliable.end_to_end_pending_count,
                "pending_destinations":runtime.reliable.end_to_end_pending_destination_count,"acked_cache":runtime.reliable.end_to_end_acked_cache_count},
            "handler_failures_decimal":runtime.total_handler_failures.to_string(),
            "handler_retries_decimal":runtime.total_handler_retries.to_string()});
    let graph_fields = [
        "nodes",
        "node_table",
        "links",
        "routes",
        "mirrored_nodes",
        "mirrored_links",
        "advertised_endpoints",
        "advertised_timesync_sources",
        "topology_encoding",
    ];
    let mut fallback_payload = Value::Object(
        network_payload
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| !graph_fields.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    fallback_payload["topology_valid"] = false.into();
    fallback_payload["quality"] = "gap".into();
    fallback_payload["scope"] = "network_topology".into();
    fallback_payload["reason"] =
        "Network graph omitted at recording16KiB bound; other fitting observations retained".into();
    let network_row = row(
        wall,
        network,
        &sender,
        NETWORK,
        json!([nodes.len(), links.len(), runtime.sides.len()]),
        network_payload,
    );
    let mut writes = Vec::new();
    // Full immutable capture timing/health is stored once in NETWORK. Other
    // rows retain both DB clocks and an explicit snapshot reference, not copied
    // full topology metadata per source/channel.
    let context = json!({"version":1,"snapshot_id":snapshot_id});
    for side in &runtime.sides {
        let key = format!("{sender}/{}:{}", side.side_id, side.side_name);
        let observed = traffic
            .sides
            .iter()
            .find(|v| v.side_id == side.side_id && v.name == side.side_name);
        let totals = json!({"tx_packets":side.tx_packets.to_string(),"rx_packets":side.rx_packets.to_string(),
            "tx_bytes":side.tx_bytes.to_string(),"rx_bytes":side.rx_bytes.to_string(),
            "tx_retries":side.tx_retries.to_string(),"handler_failures":side.tx_handler_failures.to_string()});
        writes.push(row(wall,network,&key,SIDE,json!([side.tx_packets,side.rx_packets,side.tx_bytes,side.rx_bytes,side.tx_retries,side.tx_handler_failures]),
            json!({"context":context,"side_id":side.side_id,"side_name":side.side_name,
                "ingress_enabled":side.ingress_enabled,"egress_enabled":side.egress_enabled,"totals_decimal":totals,
                "reliable_enabled":side.reliable_enabled,"link_local_enabled":side.link_local_enabled,
                "header_template_enabled":side.header_template_enabled,"transport_profile":side.side_transport_profile,
                "max_frame_bytes":side.max_frame_bytes,
                "delta_valid":observed.is_some_and(|v|v.delta.is_some()),"interval_ms":traffic.interval_ms,
                "delta_quality":if !sampler.sides.iter().any(|(id,_)|*id==side.side_id){"baseline"}
                    else if !sampler.sides.iter().any(|(id,name)|*id==side.side_id&&name==&side.side_name){"side_identity_changed"}
                    else if observed.is_some_and(|v|v.delta.is_some()){"observed"}else{"counter_reset_or_discontinuity"}}))?);
        let rates = observed
            .and_then(|v| v.delta)
            .filter(|_| traffic.interval_ms > 0)
            .map(|d| {
                let sec = traffic.interval_ms as f64 / 1000.;
                json!([
                    d.packets_sent as f64 / sec,
                    d.packets_received as f64 / sec,
                    d.bytes_sent as f64 / sec,
                    d.bytes_received as f64 / sec
                ])
            })
            .unwrap_or_else(|| json!([null, null, null, null]));
        writes.push(row(wall,network,&key,RATES,rates,json!({"context":context,"interval_ms":traffic.interval_ms,"quality":"numeric_projection"}))?);
    }
    for board in &health {
        if let Some(d) = &board.diagnostics {
            let header = source_stamps
                .iter()
                .find(|(sender, _)| sender == &board.sender_id)
                .and_then(|(_, stamp)| *stamp);
            let (value, age, quality) = qualified_die_temperature(
                d.die_temperature_centi_c,
                u64::from(d.die_temperature_sample_age_ms),
                d.received_age_ms,
                header,
                network,
            );
            let original_source = header
                .filter(|v| *v != 0 && *v != u64::MAX)
                .and_then(|v| i64::try_from(v).ok());
            writes.push(row(wall,original_source,&board.sender_id,DIE,json!([value.map(|n|n as f64/100.)]),
                json!({"context":context,"centi_c":value,"reported_centi_c":d.die_temperature_centi_c,
                    "reported_header_utc_decimal":header.map(|v|v.to_string()),"sample_age_ms":age,
                    "valid":value.is_some(),"quality":quality}))?);
        }
        if board.sender_id == "GS" || board.sender_id == "RRLS" {
            writes.push(cached_host_row(wall, board, &context)?);
        }
    }
    sampler.sides = runtime
        .sides
        .iter()
        .map(|s| (s.side_id, s.side_name.clone()))
        .collect();
    Ok(admit_observation_groups(
        wall,
        network,
        &sender,
        fallback_payload["context"].clone(),
        network_row,
        fallback_payload,
        writes,
    ))
}

pub(crate) fn descriptors() -> Value {
    json!({"version":1,"counter_encoding":"payload decimal strings are exact; numeric plots may round",
        "clock_policy":"DB receive/source clocks retained on every row; context.snapshot_id references full NETWORK or context-bearing GAP capture timing/health; original board clocks remain in raw reports",
    "streams":[{"data_type":NETWORK,"channels":["discovered routers","links","sides"]},
    {"data_type":SIDE,"channels":["TX packets","RX packets","TX bytes","RX bytes","TX retries","TX handler failures"]},
    {"data_type":RATES,"channels":["TX packets/s","RX packets/s","TX bytes/s","RX bytes/s"]},
    {"data_type":DIE,"channels":["MCU die temperature"],"unit":"°C","sensor":"internal MCU die, not ambient"},
    {"data_type":CPU,"channels":["Host CPU/package temperature"],"unit":"°C","sensor":"host CPU/package, not ambient"},
    {"data_type":GAP,"channels":["capture unavailable"],"null_means":"unknown, never zero"}]})
}

#[cfg(test)]
mod tests {
    include!("recording_diagnostics_tests.rs");
}
