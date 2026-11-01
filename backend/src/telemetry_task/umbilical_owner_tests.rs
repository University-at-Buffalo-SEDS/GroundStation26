use super::*;
use sedsnet::{
    router::{EndpointHandler, RouterConfig},
    wire_format,
};

type Seen = Arc<Mutex<Vec<(String, String, bool)>>>;
struct Ingress {
    state: Arc<AppState>,
    router: Arc<Router>,
    side: usize,
    seen: Seen,
    nonce: u16,
}
impl Ingress {
    async fn new() -> Self {
        let state = crate::state::tests::test_app_state().await;
        let seen: Seen = Default::default();
        let sink = seen.clone();
        let app = state.clone();
        let endpoint = EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            move |packet: &Packet| {
                if packet.data_type() == crate::telemetry_schema::data_type("UMBILICAL_STATUS") {
                    let capture = capture_umbilical_status_at_ingress(&app, packet);
                    let identity = capture
                        .as_ref()
                        .map(|c| c.owner().sender_id().to_string())
                        .unwrap_or_else(|| packet.sender().to_string());
                    sink.lock().unwrap().push((
                        packet.sender().to_string(),
                        identity,
                        capture.is_some(),
                    ));
                    // The exact single-capture and application path used by main.
                    if let Some(capture) = capture {
                        let _ = apply_captured_umbilical_status(&app, capture);
                    }
                }
                Ok(())
            },
        );
        let router = Arc::new(Router::new(RouterConfig::new([endpoint]).with_sender("GS")));
        state.topology_router.set(router.clone()).unwrap();
        let side = router.add_side_packed_with_options(
            "fill-uart",
            |_| Ok(()),
            sedsnet::router::RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: true,
                ..Default::default()
            },
        );
        for (name, endpoint) in [
            ("VB", "VALVE_BOARD"),
            ("AB", "ACTUATOR_BOARD"),
            ("DAQ", "BOARD_RECOVERY_DAQ"),
            ("GB", "BOARD_RECOVERY_GB"),
        ] {
            let discovery = sedsnet::discovery::build_discovery_announce(
                name,
                123,
                &[crate::telemetry_schema::endpoint(endpoint)],
            )
            .unwrap();
            router
                .rx_packed_from_side(&wire_format::pack_packet(&discovery), side)
                .unwrap();
            router.process_all_queues_with_timeout(0).unwrap();
        }
        Self {
            state,
            router,
            side,
            seen,
            nonce: 0,
        }
    }
    fn packet(&mut self, sender: &str, command: u8, value: u8, source: u64) -> Packet {
        self.nonce += 1;
        Packet::new_with_nonce(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            sender,
            source,
            self.nonce,
            Arc::from([command, value]),
        )
        .unwrap()
    }
    fn send(&mut self, sender: &str, command: u8, value: u8, source: u64) {
        let packet = self.packet(sender, command, value, source);
        let before = self.seen.lock().unwrap().len();
        self.router
            .rx_packed_from_side(&wire_format::pack_packet(&packet), self.side)
            .unwrap();
        for _ in 0..20 {
            self.router.process_all_queues_with_timeout(0).unwrap();
            if self.seen.lock().unwrap().len() > before {
                return;
            }
        }
        panic!("status did not reach actual SDK endpoint: {sender} {command} {value}");
    }
}

#[tokio::test]
async fn umbilical_owner_sdk_wrong_sender_unknown_command_and_nonbinary_are_inert() {
    let mut ingress = Ingress::new().await;
    let state = ingress.state.clone();
    let now = get_current_timestamp_ms();
    let boards = serde_json::to_value(state.board_status_snapshot(now)).unwrap();
    state.set_pending_umbilical_valve_state(ValveBoardCommands::PilotOpen as u8, false);
    state.set_pending_umbilical_valve_state(ActuatorBoardCommands::RetractPlumbing as u8, true);
    state
        .launch_sequence_command_pending
        .store(true, Ordering::Relaxed);
    let clock = state.launch_clock_snapshot();
    for (source, sender, command, value) in [
        (1000, "DAQ", 0, 0),
        (1001, "DAQ", 8, 1),
        (1002, "AB", 0, 1),
        (1003, "VB", 9, 1),
        (1004, "GB", 6, 1),
        (1005, "@addr:3735928559", 6, 1),
        (1006, "VB", 14, 1),
        (1007, "AB", 255, 1),
        (1008, "VB", 0, 2),
        (1009, "VB", 6, 255),
        (1010, "AB", 8, 2),
    ] {
        ingress.send(sender, command, value, source);
    }
    assert_eq!(
        ingress.seen.lock().unwrap().len(),
        11,
        "all negatives reach the actual SDK GS endpoint"
    );
    assert!(
        ingress
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(_, _, owned)| !owned)
    );
    assert_eq!(
        state.ring_buffer.lock().unwrap().len(),
        11,
        "every rejected raw packet remains on the existing bounded forensic path"
    );
    assert_eq!(state.last_packet_received_ms(), 0);
    assert_eq!(
        serde_json::to_value(state.board_status_snapshot(now)).unwrap(),
        boards,
        "invalid status must not renew any expected board's liveness"
    );
    assert_eq!(state.gse.lock().unwrap().mirror_valves(), [None; 5]);
    assert_eq!(state.get_umbilical_valve_state(0), None);
    assert_eq!(state.get_umbilical_valve_state(8), None);
    assert_eq!(state.get_pending_umbilical_valve_state(0), Some(false));
    assert_eq!(state.get_pending_umbilical_valve_state(8), Some(true));
    assert!(
        state
            .launch_sequence_command_pending
            .load(Ordering::Relaxed)
    );
    assert_eq!(
        serde_json::to_value(state.launch_clock_snapshot()).unwrap(),
        serde_json::to_value(clock).unwrap()
    );
    // Rejection cannot poison the source replay history or consume the pending ACK.
    ingress.send("VB", 0, 0, 1000);
    assert_eq!(state.get_umbilical_valve_state(0), Some(false));
    assert_eq!(state.get_pending_umbilical_valve_state(0), None);
    println!(
        "PASS actual packed SDK negatives: owner, unknown raw commands and strict bool; no liveness/capture/pending/clock mutation or replay poisoning"
    );
}

#[tokio::test]
async fn umbilical_owner_sdk_accepts_all_firmware_statuses_and_compact_aliases() {
    // Each command is an independent physical report case. Do not accumulate
    // nitrogen+nitrous open states that correctly trigger existing safety policy.
    for command in 0..=13u8 {
        let mut ingress = Ingress::new().await;
        let owner = if command <= 6 { "VB" } else { "AB" };
        for value in 0..=1u8 {
            ingress.send(
                owner,
                command,
                value,
                2000 + command as u64 * 2 + value as u64,
            );
            assert!(ingress.seen.lock().unwrap().last().unwrap().2);
            if command != ValveBoardCommands::Sequence as u8 {
                let (key, expected) = umbilical_state_key(command, value == 1).unwrap();
                assert_eq!(
                    ingress.state.get_umbilical_valve_state(key),
                    Some(expected),
                    "raw command={command}, value={value}"
                );
            }
        }
    }
    let mut ingress = Ingress::new().await;
    ingress.send("VB", ValveBoardCommands::Sequence as u8, 1, 2800);
    ingress.send("VB", ValveBoardCommands::PilotOpen as u8, 1, 2900);
    assert_eq!(
        ingress.state.launch_clock_snapshot().kind,
        LaunchClockKind::TPlus,
        "correct VB Sequence then correct VB pilot report retain the existing Tclock behavior"
    );
    for (owner, command) in [("VB", 1u8), ("AB", 8u8)] {
        let address = ingress.router.resolve_hostname(owner).unwrap().address;
        let alias = format!("@addr:{address}");
        let original = ingress.packet(owner, command, 1, 3000 + command as u64);
        let compact_wire_alias = wire_format::unpack_packet(&wire_format::pack_packet(&original))
            .unwrap()
            .sender()
            .to_string();
        for (offset, sender) in [&alias, &compact_wire_alias].into_iter().enumerate() {
            ingress.send(sender, command, 1, 3100 + command as u64 + offset as u64);
            let last = ingress.seen.lock().unwrap().last().unwrap().clone();
            assert_eq!(last.1, owner);
            assert!(last.2);
            assert_eq!(ingress.state.get_umbilical_valve_state(command), Some(true));
        }
    }
    println!(
        "PASS actual packed SDK correct VB0..6/AB7..13 boolean reports, close aliases, Sequence/pilot clocks and discovered compact-address owners"
    );
}

#[tokio::test]
async fn umbilical_owner_relay_preserves_original_vb_ab_owners() {
    let ingress = Ingress::new().await;
    let relay = sedsnet::relay::Relay::new_with_config(
        sedsnet::relay::RelayConfig::new().with_sender("GB"),
        Box::new(|| 0u64),
    );
    let input = relay.add_side_packed("board-can", |_| Ok(()));
    let ground = ingress.router.clone();
    let ground_side = ingress.side;
    let output = relay.add_side_packed("ground-uart", move |bytes| {
        ground.rx_packed_from_side(bytes, ground_side)
    });
    let discovery = sedsnet::discovery::build_discovery_announce(
        "GS",
        123,
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
    )
    .unwrap();
    relay
        .rx_packed_from_side(output, &wire_format::pack_packet(&discovery))
        .unwrap();
    relay.process_all_queues().unwrap();
    for (owner, endpoint, command) in [("VB", "VALVE_BOARD", 0u8), ("AB", "ACTUATOR_BOARD", 8u8)] {
        let discovery = sedsnet::discovery::build_discovery_announce(
            owner,
            123,
            &[crate::telemetry_schema::endpoint(endpoint)],
        )
        .unwrap();
        relay
            .rx_packed_from_side(input, &wire_format::pack_packet(&discovery))
            .unwrap();
        relay.process_all_queues().unwrap();
        ingress.router.process_all_queues().unwrap();
        let packet = Packet::new(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            owner,
            4000 + command as u64,
            Arc::from([command, 1]),
        )
        .unwrap();
        relay
            .rx_packed_from_side(input, &wire_format::pack_packet(&packet))
            .unwrap();
        relay.process_all_queues().unwrap();
        ingress.router.process_all_queues().unwrap();
        assert_eq!(ingress.state.get_umbilical_valve_state(command), Some(true));
        let last = ingress.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(last.1, owner);
        assert!(last.2);
    }
    println!("PASS actual SDK GB relay forwards packed VB/AB owner identity unchanged");
}

#[tokio::test]
async fn umbilical_owner_bulk_fallback_rejects_before_liveness_and_db_backpressure() {
    let mut ingress = Ingress::new().await;
    let (db_tx, mut db_rx) = mpsc::channel(1);
    db_tx
        .send(DbQueueItem::Write(DbWrite::FlightState {
            timestamp_ms: 0,
            state_code: 0,
        }))
        .await
        .unwrap();
    let now = get_current_timestamp_ms();
    let boards = serde_json::to_value(ingress.state.board_status_snapshot(now)).unwrap();
    let packet = ingress.packet("VB", 8, 1, 5000);
    let overflow = DbOverflow;
    let mut work = Box::pin(handle_packet(&ingress.state, &db_tx, &overflow, packet));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut work)
            .await
            .is_err(),
        "raw forensic recording may wait behind DB; rejection has already happened"
    );
    assert_eq!(ingress.state.get_umbilical_valve_state(8), None);
    assert_eq!(
        serde_json::to_value(ingress.state.board_status_snapshot(now)).unwrap(),
        boards
    );
    assert!(db_rx.try_recv().is_ok());
    assert!(
        work.await.is_empty(),
        "rejected status has no live/derived valve row"
    );
    match db_rx.recv().await.unwrap() {
        DbQueueItem::Write(DbWrite::Telemetry {
            data_type,
            sender_id,
            source_timestamp_ms,
            values_json,
            payload_json,
            ..
        }) => {
            assert_eq!(data_type, "UMBILICAL_STATUS");
            assert_eq!(sender_id, "VB");
            assert_eq!(source_timestamp_ms, Some(5000));
            assert_eq!(values_json, None);
            assert_eq!(
                serde_json::from_str::<Vec<u8>>(&payload_json).unwrap(),
                vec![8, 1]
            );
        }
        _ => panic!("expected original rejected raw payload only"),
    }
    assert!(db_rx.try_recv().is_err());
    println!(
        "PASS bulk fallback rejects before liveness/effects and preserves raw forensic payload through the existing full DB queue without a VALVE_STATE row"
    );
}

#[tokio::test]
async fn hitl_ingress_owner_red_wrong_board_status_cannot_mint_valve_or_qd_confirmation() {
    let state = crate::state::tests::test_app_state().await;
    let received = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let seen = received.clone();
    let ingress_state = state.clone();
    // This GS endpoint executes the same actual ingress function as main.rs.
    // Main's preceding UMBILICAL_STATUS branch only logs/probes and has no owner guard.
    let endpoint = sedsnet::router::EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("GROUND_STATION"),
        move |packet: &Packet| {
            seen.lock().unwrap().push((
                packet.data_type().as_str().to_string(),
                ingress_state.canonical_network_sender_id(packet.sender()),
            ));
            if packet.data_type() == crate::telemetry_schema::data_type("UMBILICAL_STATUS") {
                if let Some(row) = apply_umbilical_status_at_ingress(&ingress_state, packet) {
                    ingress_state.cache_recent_telemetry(row);
                }
            }
            Ok(())
        },
    );
    let router = Arc::new(sedsnet::router::Router::new(
        sedsnet::router::RouterConfig::new([endpoint]).with_sender("GS"),
    ));
    state.topology_router.set(router.clone()).unwrap();
    let side = router.add_side_packed_with_options(
        "audit-fill-link",
        |_| Ok(()),
        sedsnet::router::RouterSideOptions {
            reliable_enabled: true,
            link_local_enabled: true,
            ..Default::default()
        },
    );
    // Register a real canonical DAQ owner through SDK discovery first. It owns
    // neither ValveBoard pilot nor ActuatorBoard QD status.
    let discovery = sedsnet::discovery::build_discovery_announce(
        "DAQ",
        123,
        &[crate::telemetry_schema::endpoint("BOARD_RECOVERY_DAQ")],
    )
    .unwrap();
    router
        .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&discovery), side)
        .unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    let pilot = ValveBoardCommands::PilotOpen as u8;
    let qd = ActuatorBoardCommands::RetractPlumbing as u8;
    for (index, key, on) in [(0, pilot, 0), (1, qd, 1)] {
        let packet = Packet::new(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            1000 + index,
            Arc::from([key, on]),
        )
        .unwrap();
        router
            .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&packet), side)
            .unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
    }
    let actual = [
        state.get_umbilical_valve_state(pilot),
        state.get_umbilical_valve_state(qd),
    ];
    let accepted = received.lock().unwrap().clone();
    assert_eq!(
        accepted,
        vec![
            ("UMBILICAL_STATUS".into(), "DAQ".into()),
            ("UMBILICAL_STATUS".into(), "DAQ".into())
        ],
        "prove schema-valid wrong-owner packets traversed SDK side ingress and canonical GS endpoint"
    );
    eprintln!(
        "ACTUAL_WRONG_OWNER_INGRESS: canonical_sender=DAQ, endpoint=GROUND_STATION, pilot={:?}, qd_display={:?}; no HITL QD coordinator exists, no launch attempted",
        actual[0], actual[1]
    );
    assert_eq!(
        actual,
        [None, None],
        "held launch must never derive owner evidence from these generic confirmation caches"
    );
}

#[tokio::test]
async fn umbilical_owner_rejection_stays_raw_after_discovery_then_fresh_owner_confirms() {
    let mut ingress = Ingress::new().await;
    let address = 42424242u32;
    let alias = format!("@addr:{address}");
    assert_eq!(ingress.state.board_from_network_sender(&alias), None);
    ingress.state.set_pending_umbilical_valve_state(8, true);
    ingress.send(&alias, 8, 1, 6000);
    let retained = ingress
        .state
        .ring_buffer
        .lock()
        .unwrap()
        .pop_oldest()
        .unwrap();
    let (packet, row) = match retained {
        crate::ring_buffer::ReceivedPacket::AppliedStatus { packet, row } => (packet, row),
        _ => panic!("rejection must retain an immutable raw recording snapshot"),
    };
    assert_eq!(row.data_type, "UMBILICAL_STATUS");
    assert!(row.values.is_empty());
    assert_eq!(row.sender_id, alias);
    let ground = ingress.router.clone();
    let side = ingress.side;
    let endpoint = EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
        |_| Ok(()),
    );
    let owner = Router::new_with_clock(
        RouterConfig::new([endpoint])
            .with_sender("AB")
            .with_static_address(address),
        Box::new(|| 20000u64),
    );
    owner.add_side_packed("ground-uart", move |bytes| {
        ground.rx_packed_from_side(bytes, side)
    });
    owner.announce_discovery().unwrap();
    for _ in 0..20 {
        owner.process_all_queues().unwrap();
        ingress.router.process_all_queues().unwrap();
    }
    assert_eq!(
        ingress.state.board_from_network_sender(&alias),
        Some(Board::ActuatorBoard)
    );
    assert!(
        umbilical_status_is_owned(&ingress.state, &packet),
        "demonstrate why the old queued packet must never be revalidated as live"
    );
    let (db_tx, mut db_rx) = mpsc::channel(2);
    persist_umbilical_status(&ingress.state, &db_tx, &DbOverflow, &packet, &row).await;
    match db_rx.recv().await.unwrap() {
        DbQueueItem::Write(DbWrite::Telemetry {
            data_type,
            sender_id,
            values_json,
            payload_json,
            ..
        }) => {
            assert_eq!(data_type, "UMBILICAL_STATUS");
            assert_eq!(sender_id, alias);
            assert_eq!(
                serde_json::from_str::<Vec<Option<f32>>>(values_json.as_deref().unwrap()).unwrap(),
                Vec::<Option<f32>>::new()
            );
            assert_eq!(
                serde_json::from_str::<Vec<u8>>(&payload_json).unwrap(),
                vec![8, 1]
            );
            let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
            sqlx::query("CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)").execute(&db).await.unwrap();
            sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(1000,6000,?,?,?,?)")
                .bind(&sender_id).bind(&data_type).bind(&values_json).bind(&payload_json).execute(&db).await.unwrap();
            let mut exported = crate::recording_replay::test_export_from(&db)
                .await
                .unwrap();
            assert_eq!(exported["samples"][0]["data_type"], "UMBILICAL_STATUS");
            assert_eq!(exported["samples"][0]["values"], serde_json::json!([]));
            assert!(exported["samples"][0]["phase"].is_null());
            assert!(
                exported["samples"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["data_type"] != "VALVE_STATE" && r["data_type"] != "FLIGHT_STATE")
            );
            // Attach configuration exactly as the UI does after the actual backend export.
            exported["mode"] = "flight".into();
            exported["layout"] = serde_json::json!({});
            exported["view_config"] = serde_json::json!({"stages":[],"ground_systems":[]});
            exported["valve_mappings"] = serde_json::json!([
                {"label":"Confirmed pilot","data_type":"VALVE_STATE","index":0},
                {"label":"Raw diagnostic binding","data_type":"UMBILICAL_STATUS","index":1}
            ]);
            if let Some(path) = std::env::var_os("GS_REJECTED_STATUS_REPLAY_PROOF") {
                std::fs::write(path, serde_json::to_vec_pretty(&exported).unwrap()).unwrap();
            }
        }
        _ => panic!("expected raw forensic telemetry only"),
    }
    assert_eq!(ingress.state.get_umbilical_valve_state(8), None);
    assert_eq!(
        ingress.state.get_pending_umbilical_valve_state(8),
        Some(true)
    );
    ingress.send(&alias, 8, 1, 6001);
    assert_eq!(ingress.state.get_umbilical_valve_state(8), Some(true));
    assert_eq!(ingress.state.get_pending_umbilical_valve_state(8), None);
    println!(
        "PASS unknown-to-discovered AB address: old retained rejection remains raw/unconfirmed; distinct fresh owner report confirms"
    );
}

#[tokio::test]
async fn umbilical_owner_alias_remap_between_capture_and_application_keeps_original_owner() {
    let ingress = Ingress::new().await;
    let address = 42424242u32;
    let announce = |owner_name: &'static str, address, clock| {
        let endpoint_name = if owner_name == "AB" {
            "ACTUATOR_BOARD"
        } else {
            "VALVE_BOARD"
        };
        let endpoint = EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint(endpoint_name),
            |_| Ok(()),
        );
        let peer = Router::new_with_clock(
            RouterConfig::new([endpoint])
                .with_sender(owner_name)
                .with_static_address(address),
            Box::new(move || clock),
        );
        let ground = ingress.router.clone();
        let side = ingress.side;
        peer.add_side_packed("ground-uart", move |bytes| {
            ground.rx_packed_from_side(bytes, side)
        });
        peer.announce_discovery().unwrap();
        for _ in 0..20 {
            peer.process_all_queues().unwrap();
            ingress.router.process_all_queues().unwrap();
        }
        peer
    };
    let _first = announce("AB", address, 20000u64);
    let alias = format!("@addr:{address}");
    assert_eq!(
        ingress.state.board_from_network_sender(&alias),
        Some(Board::ActuatorBoard)
    );
    let packet = Packet::new(
        crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        &alias,
        7000,
        Arc::from([8, 1]),
    )
    .unwrap();
    let capture = capture_umbilical_status_at_ingress(&ingress.state, &packet).unwrap();
    let received_ms = capture.received_ms();
    let received_at = capture.receipt.monotonic_at;
    let received_model_ms = capture.receipt.simulation_ms;
    assert_eq!(capture.owner(), Board::ActuatorBoard);
    assert_eq!(capture.packet.packet_id(), packet.packet_id());
    assert_eq!(capture.packet.timestamp(), 7000);
    let _moved = announce("AB", address + 1, 30000u64);
    let _replacement = announce("VB", address, 40000u64);
    assert_eq!(
        ingress.state.board_from_network_sender(&alias),
        Some(Board::ValveBoard)
    );
    assert!(
        !umbilical_status_is_owned(&ingress.state, &packet),
        "a second classification now changes the decision, proving this interleaving matters"
    );
    assert_eq!(
        capture.owner(),
        Board::ActuatorBoard,
        "simulator probes use captured owner too"
    );
    ingress.state.mark_packet_received(capture.received_ms());
    let row = apply_captured_umbilical_status(&ingress.state, capture).unwrap();
    assert_eq!(row.sender_id, "AB");
    assert_eq!(row.timestamp_ms, received_ms as i64);
    assert_eq!(ingress.state.get_umbilical_valve_state(8), Some(true));
    let boards = ingress.state.board_status_snapshot(received_ms);
    let actuator = boards
        .boards
        .iter()
        .find(|b| b.board == Board::ActuatorBoard)
        .unwrap();
    let valve = boards
        .boards
        .iter()
        .find(|b| b.board == Board::ValveBoard)
        .unwrap();
    assert_eq!(actuator.packet_count, 1);
    assert_eq!(actuator.last_seen_ms, Some(received_ms));
    assert_eq!(valve.packet_count, 0);
    assert_eq!(valve.last_seen_ms, None);
    assert_eq!(ingress.state.last_packet_received_ms(), received_ms);
    let tracked = ingress.state.board_status.lock().unwrap();
    assert_eq!(
        tracked[&Board::ActuatorBoard].last_seen_instant,
        Some(received_at)
    );
    assert_eq!(
        tracked[&Board::ActuatorBoard].last_seen_model_ms,
        received_model_ms
    );
    println!(
        "PASS actual SDK AB→VB alias remap between capture/application: original AB owner, packet identity, liveness/traffic and arrival time remain frozen"
    );
}

#[tokio::test]
async fn umbilical_owner_ordering_old_capture_cannot_regress_newer_liveness() {
    let mut ingress = Ingress::new().await;
    let packet = ingress.packet("AB", 8, 1, 8000);
    let capture = capture_umbilical_status_at_ingress(&ingress.state, &packet).unwrap();
    let t1 = capture.received_ms();
    let t2 = t1 + 20;
    ingress.state.mark_board_seen("AB", t2);
    ingress.state.mark_packet_received(t2);
    let before = {
        let mut boards = ingress.state.board_status.lock().unwrap();
        let board = boards.get_mut(&Board::ActuatorBoard).unwrap();
        board.ema_gap_ms = Some(321);
        board.warned = true;
        board.clone()
    };
    assert!(
        before.last_seen_instant.unwrap() > capture.receipt.monotonic_at,
        "actual heartbeat is newer than the retained status capture"
    );
    ingress.state.mark_packet_received(capture.received_ms());
    apply_captured_umbilical_status(&ingress.state, capture).unwrap();
    let after = ingress.state.board_status.lock().unwrap()[&Board::ActuatorBoard].clone();
    eprintln!(
        "ACTUAL_OLD_CAPTURE_LIVENESS: global_before={t2}, global_after={}, old_replaced_new_monotonic={}, ema_before={:?}, ema_after={:?}, warned_before={}, warned_after={}",
        ingress.state.last_packet_received_ms(),
        after.last_seen_instant != before.last_seen_instant,
        before.ema_gap_ms,
        after.ema_gap_ms,
        before.warned,
        after.warned
    );
    assert_eq!(after.last_seen_instant, before.last_seen_instant);
    assert_eq!(after.last_seen_model_ms, before.last_seen_model_ms);
    assert_eq!(after.last_seen_ms, before.last_seen_ms);
    assert_eq!(after.ema_gap_ms, before.ema_gap_ms);
    assert_eq!(after.warned, before.warned);
    assert_eq!(after.packet_count, before.packet_count + 1);
    assert_eq!(ingress.state.last_packet_received_ms(), t2);
}

#[tokio::test]
async fn umbilical_owner_ordering_pre_target_capture_cannot_clear_new_pending() {
    let mut ingress = Ingress::new().await;
    let packet = ingress.packet("VB", 0, 0, 9000);
    let capture = capture_umbilical_status_at_ingress(&ingress.state, &packet).unwrap();
    ingress.state.set_pending_umbilical_valve_state(0, false);
    assert_eq!(
        ingress.state.get_pending_umbilical_valve_state(0),
        Some(false)
    );
    let before = ingress.state.gse.lock().unwrap().mirror_valves();
    apply_captured_umbilical_status(&ingress.state, capture).unwrap();
    assert_eq!(ingress.state.gse.lock().unwrap().mirror_valves(), before, "rejected pre-target capture must not change GSE live evidence");
    eprintln!(
        "ACTUAL_PRE_TARGET_CAPTURE: pending={:?}, confirmed={:?}",
        ingress.state.get_pending_umbilical_valve_state(0),
        ingress.state.get_umbilical_valve_state(0)
    );
    assert_eq!(
        ingress.state.get_pending_umbilical_valve_state(0),
        Some(false),
        "a pre-target source capture cannot confirm a later matching target"
    );
    assert_eq!(ingress.state.get_umbilical_valve_state(0), None);
    ingress.send("VB", 0, 0, 9001);
    assert_eq!(
        ingress.state.get_pending_umbilical_valve_state(0),
        None,
        "a distinct fresh post-target periodic owner report still confirms"
    );
    assert_eq!(ingress.state.get_umbilical_valve_state(0), Some(false));
}
