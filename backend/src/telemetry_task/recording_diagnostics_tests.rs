struct OwnedDirectory(std::path::PathBuf);
impl OwnedDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gs-recording-diagnostics-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
use super::*;
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn installed(state: &AppState) -> Arc<Router> {
    let start = Instant::now();
    let router = Arc::new(Router::new_with_clock(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        Box::new(move || start.elapsed().as_millis() as u64),
    ));
    state
        .topology_router
        .set(router.clone())
        .unwrap_or_else(|_| panic!("fixture router already installed"));
    router
}
fn payload(write: &DbWrite) -> Value {
    let DbWrite::Telemetry { payload_json, .. } = write else {
        panic!("expected telemetry")
    };
    serde_json::from_str(payload_json).unwrap()
}
#[test]
fn bounded_payload_rejects_oversize_and_wide_counters_survive_portable_json() {
    let result = row(
        10,
        None,
        "GS",
        SIDE,
        json!([u64::MAX]),
        json!({"counter":u64::MAX,"nested":[9_007_199_254_740_993u64]}),
    )
    .unwrap();
    assert_eq!(payload(&result)["counter"], u64::MAX.to_string());
    assert_eq!(payload(&result)["nested"][0], "9007199254740993");
    let DbWrite::Telemetry { values_json, .. } = result else {
        unreachable!()
    };
    assert_eq!(values_json.unwrap(), format!("[{}]", u64::MAX)); // Python CSV retains exact integer.
    assert!(bounded_json(&json!({"edges":["x".repeat(MAX_BATCH_BYTES)]})).is_err());
    assert!(payload(&gap(1, "p", "oversize", 1))["quality"] == "gap");
}
#[tokio::test]
async fn actual_snapshot_without_ui_keeps_stale_temperature_unknown() {
    let state = crate::state::tests::test_app_state().await;
    installed(&state);
    let mut words = vec![0u32; 24];
    words[0] = 1;
    words[4] = 1000;
    words[1] = 0x100 | ((4532u32) << 16);
    assert!(state.network_health.lock().unwrap().observe_diagnostics(
        "FC",
        &words,
        Instant::now() - Duration::from_secs(16),
        100
    ));
    let writes = capture(&state, "p", Instant::now(), &mut Default::default(), 7, 0).unwrap();
    let network = writes
        .iter()
        .find(|w| matches!(w,DbWrite::Telemetry{data_type,..}if data_type==NETWORK))
        .unwrap();
    assert_eq!(payload(network)["topology_valid"], true);
    let die=writes.iter().find(|w|matches!(w,DbWrite::Telemetry{data_type,sender_id,..}if data_type==DIE&&sender_id=="FC")).unwrap();
    let DbWrite::Telemetry { values_json, .. } = die else {
        unreachable!()
    };
    assert_eq!(values_json.as_deref(), Some("[null]"));
    assert_eq!(payload(die)["valid"], false);
    assert_eq!(
        payload(network)["context"]["recording_generation_decimal"],
        "7"
    );
    assert_eq!(
        payload(die)["context"]["snapshot_id"],
        payload(network)["context"]["snapshot_id"]
    );
}

struct Hold {
    entries: AtomicUsize,
    armed: AtomicBool,
    entered: AtomicBool,
    released: Mutex<bool>,
    condition: Condvar,
}
impl Hold {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.condition.notify_all();
    }
}
struct Release(Arc<Hold>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        self.release();
    }
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("bounded owner progress");
}
#[tokio::test]
async fn actual_recording_owner_collects_without_ui_and_retires_old_session_result() {
    let directory = OwnedDirectory::new();
    let mut state = crate::state::tests::test_app_state().await;
    Arc::get_mut(&mut state)
        .expect("unshared new test state")
        .placeholder_db_path = directory
        .path()
        .join("placeholder.db")
        .to_string_lossy()
        .into_owned();
    let hold = Arc::new(Hold {
        entries: AtomicUsize::new(0),
        armed: AtomicBool::new(false),
        entered: AtomicBool::new(false),
        released: Mutex::new(false),
        condition: Condvar::new(),
    });
    let _release = Release(hold.clone());
    let h = hold.clone();
    let started = Instant::now();
    let router = Arc::new(Router::new_with_clock(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        Box::new(move || {
            if h.armed.load(Ordering::SeqCst) {
                h.entries.fetch_add(1, Ordering::SeqCst);
                h.entered.store(true, Ordering::SeqCst);
                let mut ready = h.released.lock().unwrap();
                while !*ready {
                    ready = h.condition.wait(ready).unwrap();
                }
            }
            started.elapsed().as_millis() as u64
        }),
    ));
    state
        .topology_router
        .set(router)
        .unwrap_or_else(|_| panic!("router already installed"));
    let (tx, rx) = mpsc::channel(32);
    let (shutdown, shutdown_rx) = broadcast::channel(1);
    let worker = tokio::spawn(super::super::recording_owner::run(
        state.clone(),
        rx,
        32,
        5,
        shutdown_rx,
    ));
    tx.send(DbQueueItem::Control(RecordingCommand::StartNow))
        .await
        .unwrap();
    until(|| {
        matches!(
            state.recording_status_snapshot().mode,
            RecordingModeWire::Recording
        )
    })
    .await;
    let first = state.recording_status_snapshot().db_path.unwrap();
    // Wait for an actual independent collection and actual SQLite commit.
    let first_generation=tokio::time::timeout(Duration::from_secs(4),async{
        loop{
            let db=state.telemetry_db_pool();
            let row:Option<String>=sqlx::query_scalar("SELECT payload_json FROM telemetry WHERE data_type='REC_NETWORK_STATE' ORDER BY id LIMIT 1").fetch_optional(&db).await.unwrap();
            if let Some(row)=row{break serde_json::from_str::<Value>(&row).unwrap()["context"]["recording_generation_decimal"].clone();}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    hold.armed.store(true, Ordering::SeqCst);
    until(|| hold.entered.load(Ordering::SeqCst)).await;
    assert!(
        state.network_health.try_lock().is_ok(),
        "SDK worker must not hold health lock"
    );
    // Same-file ResetAll must invalidate the held capture as well as file switches.
    tx.send(DbQueueItem::Control(RecordingCommand::ResetAll))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM telemetry WHERE data_type='REC_NETWORK_STATE'",
            )
            .fetch_one(&state.telemetry_db_pool())
            .await
            .unwrap();
            if count == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state.recording_status_snapshot().db_path.as_deref(),
        Some(first.as_str())
    );
    assert_eq!(
        hold.entries.load(Ordering::SeqCst),
        1,
        "same-file reset cannot create overlapping SDK jobs"
    );
    hold.armed.store(false, Ordering::SeqCst);
    hold.release();
    let reset_generation=tokio::time::timeout(Duration::from_secs(4),async{
        loop {
            let row:Option<String>=sqlx::query_scalar("SELECT payload_json FROM telemetry WHERE data_type='REC_NETWORK_STATE' ORDER BY id LIMIT 1")
                .fetch_optional(&state.telemetry_db_pool()).await.unwrap();
            if let Some(row)=row { break serde_json::from_str::<Value>(&row).unwrap()["context"]["recording_generation_decimal"].clone(); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    assert_ne!(reset_generation, first_generation);
    *hold.released.lock().unwrap() = false;
    hold.entered.store(false, Ordering::SeqCst);
    hold.armed.store(true, Ordering::SeqCst);
    until(|| hold.entered.load(Ordering::SeqCst)).await;
    // Real pause/close and start proceed while the old SDK provider is paused.
    tx.send(DbQueueItem::Control(RecordingCommand::Pause))
        .await
        .unwrap();
    until(|| {
        matches!(
            state.recording_status_snapshot().mode,
            RecordingModeWire::Paused
        )
    })
    .await;
    tx.send(DbQueueItem::Control(RecordingCommand::StartNow))
        .await
        .unwrap();
    until(|| {
        matches!(
            state.recording_status_snapshot().mode,
            RecordingModeWire::Recording
        ) && state.recording_status_snapshot().db_path.as_deref() != Some(first.as_str())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        hold.entries.load(Ordering::SeqCst),
        2,
        "Pause/Start cannot abandon the second blocked physical job"
    );
    hold.release();
    tokio::time::timeout(Duration::from_secs(4),async{
        loop{
            let db=state.telemetry_db_pool();
            let rows:Vec<String>=sqlx::query_scalar("SELECT payload_json FROM telemetry WHERE data_type='REC_NETWORK_STATE'").fetch_all(&db).await.unwrap();
            if !rows.is_empty(){for row in rows{assert_ne!(serde_json::from_str::<Value>(&row).unwrap()["context"]["recording_generation_decimal"],first_generation);}break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    tx.send(DbQueueItem::Control(RecordingCommand::Stop))
        .await
        .unwrap();
    until(|| {
        matches!(
            state.recording_status_snapshot().mode,
            RecordingModeWire::Idle
        )
    })
    .await;
    shutdown.send(()).unwrap();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn recording_seven_board_four_public_side_envelope_fits_bound_and_first_delta_is_unknown() {
    let state = crate::state::tests::test_app_state().await;
    let router = installed(&state);
    for name in ["radio", "umbilical", "primary-lan", "backup-lan"] {
        router.add_side_packet(name, |_| Ok(()));
    }
    for name in ["FC", "RF", "PB", "DAQ", "GB", "VB", "AB"] {
        let mut words = vec![123u32; 24];
        words[0] = 1;
        words[1] = 0xff | 0x100 | (4532u32 << 16);
        words[3] = 0;
        words[4] = 1000;
        assert!(state.network_health.lock().unwrap().observe_diagnostics(
            name,
            &words,
            Instant::now(),
            100
        ));
    }
    let mut history = SampleHistory::default();
    let writes = capture(&state, "budget", Instant::now(), &mut history, 1, 0)
        .expect("representative serialization envelope must fit16KiB");
    eprintln!("SEVEN_BOARD_FOUR_SIDE_BATCH_BYTES={}", batch_cost(&writes));
    assert!(batch_cost(&writes) <= MAX_BATCH_BYTES);
    assert_eq!(
        writes
            .iter()
            .filter(|r| matches!(r,DbWrite::Telemetry{data_type,..}if data_type==SIDE))
            .count(),
        4
    );
    assert_eq!(
        writes
            .iter()
            .filter(|r| matches!(r,DbWrite::Telemetry{data_type,..}if data_type==DIE))
            .count(),
        7
    );
    for write in writes
        .iter()
        .filter(|r| matches!(r,DbWrite::Telemetry{data_type,..}if data_type==SIDE))
    {
        assert_eq!(payload(write)["delta_quality"], "baseline");
        assert_eq!(payload(write)["delta_valid"], false);
    }
    tokio::time::sleep(Duration::from_millis(1010)).await;
    let next = capture(&state, "budget", Instant::now(), &mut history, 1, 0).unwrap();
    for write in next
        .iter()
        .filter(|r| matches!(r,DbWrite::Telemetry{data_type,..}if data_type==SIDE))
    {
        assert_eq!(payload(write)["delta_quality"], "observed");
        assert_eq!(payload(write)["delta_valid"], true); // Real newly created side totals stayed0.
    }
}
#[tokio::test]
async fn exhausted_recording_generation_reports_gap_without_wrapping_or_sdk_job() {
    let state = crate::state::tests::test_app_state().await;
    let mut collector = Collector::new();
    collector.generation = Some(u64::MAX);
    collector.invalidate();
    let writes = collector
        .poll(&state, Some("closed-overflow-generation"))
        .await;
    assert!(collector.generation.is_none());
    assert!(collector.pending.is_none());
    assert_eq!(writes.len(), 1);
    assert!(
        payload(&writes[0])["reason"]
            .as_str()
            .unwrap()
            .contains("generation exhausted")
    );
    collector.next = tokio::time::Instant::now() + Duration::from_secs(3600);
    assert!(
        collector
            .poll(&state, Some("closed-overflow-generation"))
            .await
            .is_empty()
    );
}

#[test]
fn recording_temperature_requires_original_compatible_clock_and_receipt_freshness() {
    assert_eq!(
        qualified_die_temperature(Some(-123), 10, 20, Some(1000), Some(1100)),
        (Some(-123), 110, "observed")
    );
    for header in [None, Some(0), Some(u64::MAX)] {
        assert_eq!(
            qualified_die_temperature(Some(-123), 0, 0, header, Some(1100)).0,
            None
        );
    }
    assert_eq!(
        qualified_die_temperature(Some(-123), 0, 0, Some(1200), Some(1100)).2,
        "source_clock_future"
    );
    assert_eq!(
        qualified_die_temperature(Some(-123), 0, 16000, Some(1000), Some(1100)).2,
        "stale"
    );
    assert_eq!(
        qualified_die_temperature(Some(-123), 0, 0, Some(1000), None).2,
        "network_clock_unknown"
    );
    assert_eq!(
        qualified_die_temperature(Some(-123), 0, 0, Some(1000), Some(17000)).0,
        None
    );
}
#[test]
fn recording_node_table_preserves_contradictions_and_unknown_array_positions() {
    let mut table = Vec::new();
    let a = json!({"sender":"FC","connections":["GS"]});
    assert_eq!(intern_node(&mut table, a.clone()), 0);
    assert_eq!(intern_node(&mut table, a), 0);
    assert_eq!(
        intern_node(&mut table, json!({"sender":"FC","connections":[]})),
        1
    );
    let mut sparse = json!({"unknown":null,"actual_zero":0,"positions":[null,0,{"missing":null}]});
    omit_unknown_optionals(&mut sparse);
    assert_eq!(sparse, json!({"actual_zero":0,"positions":[null,0,{}]}));
}
#[test]
fn recording_oversize_topology_keeps_temperature_counter_and_resolved_gap_context() {
    let context =
        json!({"version":1,"snapshot_id":"original:9:1","recording_generation_decimal":"9"});
    let brief = json!({"version":1,"snapshot_id":"original:9:1"});
    let temp = row(
        123,
        Some(100),
        "FC",
        DIE,
        json!([-123]),
        json!({"context":brief,"centi_c":-123,"valid":true}),
    )
    .unwrap();
    let side = row(
        123,
        Some(100),
        "GS",
        SIDE,
        json!([1]),
        json!({"context":brief,"totals_decimal":{"rx":"18446744073709551615"}}),
    )
    .unwrap();
    let oversized = row(
        123,
        Some(100),
        "GS",
        NETWORK,
        json!([null]),
        json!({"nodes":["x".repeat(MAX_BATCH_BYTES)]}),
    );
    assert!(oversized.is_err());
    let writes = admit_observation_groups(
        123,
        Some(100),
        "GS",
        context.clone(),
        oversized,
        json!({"context":context,"topology_valid":false,"quality":"gap","scope":"network_topology","reason":"oversize"}),
        vec![temp, side],
    );
    assert!(batch_cost(&writes) <= MAX_BATCH_BYTES);
    assert_eq!(writes.len(), 3);
    let witness = payload(&writes[0]);
    assert_eq!(witness["topology_valid"], false);
    assert!(witness.get("nodes").is_none());
    for row in &writes[1..] {
        assert_eq!(
            payload(row)["context"]["snapshot_id"],
            witness["context"]["snapshot_id"]
        );
    }
    assert_eq!(payload(&writes[1])["centi_c"], -123);
    assert_eq!(
        payload(&writes[2])["totals_decimal"]["rx"],
        u64::MAX.to_string()
    );
}

#[tokio::test]
async fn recording_populated_eight_public_router_shared_bus_keeps_truthful_bounded_groups() {
    use std::collections::VecDeque;
    let state = crate::state::tests::test_app_state().await;
    let bus = Arc::new(Mutex::new(VecDeque::<(usize, Vec<u8>)>::new()));
    let mut peers = Vec::new();
    let mut sides = Vec::new();
    for (index, name) in ["GS", "FC", "RF", "PB", "DAQ", "GB", "VB", "AB"]
        .iter()
        .enumerate()
    {
        let start = Instant::now();
        let router = Arc::new(Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]).with_sender(name),
            Box::new(move || start.elapsed().as_millis() as u64),
        ));
        let bus = bus.clone();
        let options = sedsnet::router::RouterSideOptions {
            reliable_enabled: false,
            ..Default::default()
        };
        let side = router.add_side_packed_with_options(
            "shared-rfd",
            move |bytes| {
                bus.lock().unwrap().push_back((index, bytes.to_vec()));
                Ok(())
            },
            options,
        );
        sides.push(side);
        peers.push(router);
    }
    state
        .topology_router
        .set(peers[0].clone())
        .unwrap_or_else(|_| panic!("router installed"));
    for peer in &peers {
        peer.announce_discovery().unwrap();
    }
    // Fixed work budget; no negotiation-until-pass loop or synthetic routes.
    let mut delivered = 0usize;
    for _ in 0..16 {
        for _ in 0..256 {
            let frame = bus.lock().unwrap().pop_front();
            let Some((from, bytes)) = frame else { break };
            for (index, peer) in peers.iter().enumerate() {
                if index != from {
                    peer.rx_packed_from_side(&bytes, sides[index]).unwrap();
                }
            }
            delivered += 1;
        }
        for peer in &peers {
            peer.process_tx_queue_with_timeout(1).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let topology = peers[0].export_topology();
    assert!(
        topology.routers.len() >= 8,
        "real discovery graph premise: {} nodes, {delivered} frames",
        topology.routers.len()
    );
    let rows = capture(
        &state,
        "eight-real-routers",
        Instant::now(),
        &mut Default::default(),
        1,
        0,
    )
    .unwrap();
    assert!(batch_cost(&rows) <= MAX_BATCH_BYTES);
    let full = rows
        .iter()
        .find(|row| matches!(row,DbWrite::Telemetry{data_type,..}if data_type==NETWORK));
    if let Some(full) = full {
        let d = payload(full);
        assert_eq!(d["topology_valid"], true);
        assert!(d["nodes"].as_array().unwrap().len() >= 8);
        let table = d["node_table"].as_array().unwrap();
        for index in d["nodes"].as_array().unwrap() {
            assert!(table.get(index.as_u64().unwrap() as usize).is_some());
        }
    } else {
        let witness = rows
            .iter()
            .find(|row| matches!(row,DbWrite::Telemetry{data_type,..}if data_type==GAP))
            .expect("oversize must record truthful GAP");
        assert_eq!(payload(witness)["topology_valid"], false);
        assert!(payload(witness).get("nodes").is_none());
    }
    // Add the same seven decoded health reports and four actual GS sides to
    // the discovered graph. Oversize may omit topology, never its context or
    // otherwise-fitting temperature/counter streams.
    for name in ["umbilical", "primary-lan", "backup-lan"] {
        peers[0].add_side_packet(name, |_| Ok(()));
    }
    for name in ["FC", "RF", "PB", "DAQ", "GB", "VB", "AB"] {
        let mut words = vec![123u32; 24];
        words[0] = 1;
        words[1] = 0xff | 0x100 | (4532u32 << 16);
        words[3] = 0;
        words[4] = 1000;
        assert!(state.network_health.lock().unwrap().observe_diagnostics(
            name,
            &words,
            Instant::now(),
            100
        ));
    }
    let combined = capture(
        &state,
        "eight-real-routers",
        Instant::now(),
        &mut Default::default(),
        2,
        0,
    )
    .unwrap();
    assert!(batch_cost(&combined) <= MAX_BATCH_BYTES);
    assert_eq!(
        combined
            .iter()
            .filter(|r| matches!(r,DbWrite::Telemetry{data_type,..}if data_type==DIE))
            .count(),
        7
    );
    assert_eq!(
        combined
            .iter()
            .filter(|r| matches!(r,DbWrite::Telemetry{data_type,..}if data_type==SIDE))
            .count(),
        4
    );
    let witness = combined
        .iter()
        .find(|r| matches!(r,DbWrite::Telemetry{data_type,..}if data_type==NETWORK||data_type==GAP))
        .unwrap();
    for r in &combined {
        assert_eq!(
            payload(r)["context"]["snapshot_id"],
            payload(witness)["context"]["snapshot_id"]
        );
    }
    eprintln!(
        "EIGHT_ROUTER_SEVEN_HEALTH_FOUR_SIDE_BYTES={} quality={}",
        batch_cost(&combined),
        payload(witness)["topology_valid"]
    );
    eprintln!(
        "EIGHT_PUBLIC_SHARED_RFD_ROUTERS nodes={} delivered_frames={delivered} recorded_bytes={} complete_graph={}",
        topology.routers.len(),
        batch_cost(&rows),
        full.is_some()
    );
}

#[test]
fn recording_cached_host_projection_never_invents_original_utc() {
    let board = crate::types::NetworkBoardHealthEntry {
        sender_id: "GS".into(),
        cpu_temperature_centi_c: Some(-123),
        cpu_temperature_age_ms: 5000,
        ..Default::default()
    };
    let write =
        cached_host_row(999999, &board, &json!({"version":1,"snapshot_id":"p:1:1"})).unwrap();
    let DbWrite::Telemetry {
        timestamp_ms,
        source_timestamp_ms,
        values_json,
        ..
    } = &write
    else {
        unreachable!()
    };
    assert_eq!(*timestamp_ms, 999999);
    assert_eq!(*source_timestamp_ms, None);
    assert_eq!(values_json.as_deref(), Some("[-1.23]"));
    assert_eq!(payload(&write)["sample_age_ms"], 5000);
    assert_eq!(payload(&write)["valid"], true);
    assert_eq!(
        payload(&write)["source_clock"],
        "cached_host_monotonic_age; original UTC unavailable"
    );
    let stale = crate::types::NetworkBoardHealthEntry {
        cpu_temperature_age_ms: 15001,
        ..board
    };
    let write = cached_host_row(999999, &stale, &json!({"version":1})).unwrap();
    assert_eq!(payload(&write)["valid"], false);
    assert!(payload(&write)["centi_c"].is_null());
}
