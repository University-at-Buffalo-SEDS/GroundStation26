//! Actual deployed Dummy transport and production HTTP QUERY / Exchange owner path.
use super::*;
use crate::comms::{CommsDevice, DummyComms};
use sedsnet::router::{EndpointHandler, Router, RouterConfig};
use std::collections::VecDeque;
type MeshOutput = Arc<Mutex<VecDeque<(usize, Vec<u8>)>>>;

struct Mesh {
    state: Arc<AppState>,
    host: Arc<Router>,
    links: Vec<DummyComms>,
    sides: Vec<usize>,
    out: MeshOutput,
    received: Arc<Mutex<Vec<Packet>>>,
}
impl Mesh {
    fn new(state: Arc<AppState>, actual: u8, unknown: u8) -> Self {
        crate::telemetry_schema::initialize().unwrap();
        crate::flight_sim::initialize_model_owners(&state, actual, unknown);
        let received = Arc::new(Mutex::new(Vec::new()));
        let capture = received.clone();
        let observed = state.clone();
        let observe = move |packet: &Packet| -> sedsnet::TelemetryResult<()> {
            if packet.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS") {
                if let Some(board) = observed.board_from_network_sender(packet.sender()) {
                    observed.mark_board_seen(
                        board.sender_id(),
                        crate::telemetry_task::get_current_timestamp_ms(),
                    );
                }
                receive_status(&observed, packet);
            }
            crate::board_abort::observe_raw(&observed, packet);
            if packet.data_type() == crate::telemetry_schema::data_type("ABORT") {
                crate::deferred_abort::observe(
                    &observed,
                    packet,
                    crate::gse::automatic_generation(&observed),
                );
            }
            capture.lock().unwrap().push(packet.clone());
            Ok(())
        };
        let host = Arc::new(Router::new(
            RouterConfig::new([
                EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoints::GROUND_STATION,
                    observe.clone(),
                ),
                EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoints::ABORT,
                    observe,
                ),
            ])
            .with_sender("GS"),
        ));
        host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            crate::telemetry_task::get_current_timestamp_ms(),
        ))
        .unwrap();
        assert!(state.topology_router.set(host.clone()).is_ok());
        let out = Arc::new(Mutex::new(VecDeque::new()));
        let mut links = Vec::new();
        let mut sides = Vec::new();
        for (index, name) in ["Rocket Comms", "Umbilical Comms"].into_iter().enumerate() {
            let capture = out.clone();
            let side = host.add_side_packed_with_options(
                name,
                move |bytes| {
                    capture.lock().unwrap().push_back((index, bytes.to_vec()));
                    Ok(())
                },
                sedsnet::router::RouterSideOptions {
                    reliable_enabled: true,
                    ..Default::default()
                }
                .with_small_packet_transport(if index == 0 { 192 } else { 1024 }),
            );
            let mut link = DummyComms::new(name);
            link.set_side_id(side);
            links.push(link);
            sides.push(side);
        }
        crate::testing_board_lifecycle::bind_selected(
            links[0].testing_model_link(),
            links[1].testing_model_link(),
        );
        assert!(crate::testing_board_lifecycle::enabled());
        Self {
            state,
            host,
            links,
            sides,
            out,
            received,
        }
    }
    fn pump(&mut self) {
        for _ in 0..80 {
            self.host.process_all_queues().unwrap();
            loop {
                let row = self.out.lock().unwrap().pop_front();
                let Some((index, bytes)) = row else {
                    break;
                };
                self.links[index].send_data(&bytes).unwrap();
            }
            for link in &mut self.links {
                link.recv_packet(&self.host, &mut |_| {}).unwrap();
            }
        }
    }
    fn pump_wire_once(&mut self) {
        loop {
            let row = self.out.lock().unwrap().pop_front();
            let Some((index, bytes)) = row else {
                break;
            };
            self.links[index].send_data(&bytes).unwrap();
        }
        for link in &mut self.links {
            link.recv_packet(&self.host, &mut |_| {}).unwrap();
        }
    }
    fn churn_sdk_dedup(&mut self) {
        let count = sedsnet::config::RuntimeMemoryConfig::default().max_recent_rx_ids + 32;
        for i in 0..count {
            let packet = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoints::GROUND_STATION],
                "FC",
                12345 + i as u64,
                Arc::from([0]),
            )
            .unwrap();
            self.host.rx_queue_from_side(packet, self.sides[0]).unwrap();
            self.host.process_all_queues().unwrap();
        }
    }
    async fn query(&mut self, board: Board) -> Observation {
        self.pump();
        let response = tests::http(
            self.state.clone(),
            "POST",
            "/api/board-recovery/query",
            Some("operator"),
            json!({"board":board.sender_id()}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "HTTP query {board:?}");
        self.pump();
        self.state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()]
            .clone()
            .expect("actual nonce-correlated status")
    }
    async fn recover(&mut self, board: Board) -> Observation {
        std::thread::sleep(Duration::from_millis(3));
        let boundary = exchange::ConfirmBoundary::capture(&self.host).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let query = self.query(board).await;
        let initial_epoch = query.value.epoch;
        let boot = query.value.boot;
        let mut rpc = exchange::Exchange::prepare_at(
            board,
            &query,
            query.value.nonce.wrapping_add(1).max(1),
            0,
            Stamp::now(),
            self.host.clone(),
            boundary,
        )
        .unwrap();
        let original = rpc.packet(Stamp::now()).unwrap().clone();
        let start = self.received.lock().unwrap().len();
        self.host.tx_queue(original.clone()).unwrap();
        self.pump();
        let rows = self.received.lock().unwrap()[start..].to_vec();
        let mut outcomes = Vec::new();
        for packet in rows {
            if packet.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS")
                && packet.sender() == board.sender_id()
                && packet.payload()[4..12] == rpc.nonce.to_le_bytes()
            {
                outcomes.push(rpc.observe(&packet, Stamp::now()).unwrap());
            }
        }
        assert!(
            outcomes.contains(&exchange::Outcome::Pending),
            "{board:?} requires actual Pending"
        );
        assert_eq!(
            outcomes.last(),
            Some(&exchange::Outcome::Applied),
            "{board:?}: {outcomes:?}"
        );
        // Identical SDK envelope is ACK-only; polling needs a fresh envelope.
        let before = self
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS"))
            .count();
        self.host.tx_queue(original.clone()).unwrap();
        self.pump();
        assert_eq!(
            self.received
                .lock()
                .unwrap()
                .iter()
                .filter(
                    |p| p.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS")
                )
                .count(),
            before
        );
        for (delta, expected) in [(0, 0), (1, 6)] {
            let retry = Packet::new(
                original.data_type(),
                original.endpoints(),
                original.sender(),
                original.timestamp() + delta,
                Arc::from(original.payload()),
            )
            .unwrap();
            let start = self.received.lock().unwrap().len();
            self.host.tx_queue(retry).unwrap();
            self.pump();
            let rows = self.received.lock().unwrap();
            let reply = rows[start..]
                .iter()
                .find(|p| {
                    p.sender() == board.sender_id()
                        && p.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS")
                        && p.payload()[4..12] == rpc.nonce.to_le_bytes()
                })
                .expect("application duplicate reply");
            let row = Readback::decode(reply.payload()).unwrap();
            assert_eq!(row.result, expected);
            assert_eq!(row.epoch, initial_epoch + 1);
            assert_eq!(row.boot, boot);
        }
        let final_query = self.query(board).await;
        assert_eq!(final_query.value.boot, boot);
        assert_eq!(final_query.value.epoch, initial_epoch + 1);
        assert_eq!(final_query.value.state, Some(0));
        assert_eq!(final_query.value.result, 0);
        assert_eq!(
            final_query.value.flags,
            if matches!(
                board,
                Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
            ) {
                0x7e
            } else {
                0x2c
            }
        );
        final_query
    }
}
#[tokio::test]
async fn actual_dummy_http_query_and_all_seven_rpc_recovery() {
    if std::env::var_os("MODEL_RPC_CHILD").is_none() {
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","board_recovery::testing_model_tests::actual_dummy_http_query_and_all_seven_rpc_recovery","--nocapture"]).env("MODEL_RPC_CHILD","1").env("GS_SIM_MODE","1").env("GS_RRLS_ROLE","primary").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let case = std::env::var("MODEL_RPC_CASE").unwrap_or_else(|_| "all7".into());
    let fixture = tests::http_fixture().await;
    let mut mesh = Mesh::new(
        fixture.state.clone(),
        if case == "unknown" { 0 } else { 0x7f },
        if case == "unknown" { 0x7f } else { 0 },
    );
    if case == "unknown" {
        for board in crate::testing_board_lifecycle::BOARDS {
            let q = mesh.query(board).await;
            assert_eq!(q.value.result, 0);
            assert_eq!(q.value.state, Some(0));
            assert_eq!(q.value.flags & 1, 0);
            assert_ne!(q.value.boot, 0);
            mesh.recover(board).await;
        }
    } else if case == "held" {
        crate::flight_sim::reset_simulation();
        let held = (0..40)
            .find_map(|_| {
                crate::flight_sim::_next_state_aware_packet()
                    .unwrap()
                    .filter(|p| {
                        p.sender() == "FC"
                            && p.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE")
                            && p.payload() == [15]
                    })
            })
            .expect("actual queued FC Abort state");
        mesh.host
            .rx_queue_from_side(held.clone(), mesh.sides[0])
            .unwrap();
        mesh.pump();
        mesh.recover(Board::FlightComputer).await;
        mesh.churn_sdk_dedup();
        let revision = mesh.state.board_abort.query_revision(Board::FlightComputer);
        let before = mesh
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.packet_id() == held.packet_id())
            .count();
        mesh.host
            .rx_queue_from_side(held.clone(), mesh.sides[0])
            .unwrap();
        mesh.pump();
        assert!(
            mesh.received
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.packet_id() == held.packet_id())
                .count()
                > before,
            "real SDK duplicate window actually turned over"
        );
        assert_eq!(
            mesh.state.board_abort.query_revision(Board::FlightComputer),
            revision,
            "exact held old model FC15 is obsolete"
        );
        let bytes = sedsnet::wire_format::pack_packet(&held);
        let unregistered = sedsnet::wire_format::unpack_packet(&bytes).unwrap();
        assert_eq!(unregistered.packet_id(), held.packet_id());
        assert_eq!(sedsnet::wire_format::pack_packet(&unregistered), bytes);
        assert_ne!(unregistered.endpoints().as_ptr(), held.endpoints().as_ptr());
        mesh.churn_sdk_dedup();
        mesh.host
            .rx_queue_from_side(unregistered, mesh.sides[0])
            .unwrap();
        mesh.pump();
        assert!(
            mesh.state.board_abort.query_revision(Board::FlightComputer) > revision,
            "unregistered real/raw state stays conservative"
        );
        // A real command is never routed through the state-provenance filter.
        crate::telemetry_task::queue_manual_abort_wire(
            &mesh.host,
            crate::manual_abort_intent::WirePolicy::Untimed,
        )
        .unwrap();
        mesh.pump();
        let q = mesh.query(Board::FlightComputer).await;
        assert_eq!(q.value.state, Some(15));
        assert_ne!(q.value.flags & 1, 0);
    } else if case == "gse" {
        for board in crate::testing_board_lifecycle::BOARDS {
            mesh.recover(board).await;
        }
        std::thread::sleep(Duration::from_millis(3));
        crate::gse::apply_model_effects_for_test(
            &mesh.state,
            vec![(3, true), (4, true), (0, true)],
        );
        mesh.pump();
        assert_eq!(
            mesh.query(Board::ActuatorBoard).await.value.outputs & 6,
            6,
            "actual GSE tagged N2/N2O open"
        );
        assert_eq!(
            mesh.query(Board::ValveBoard).await.value.outputs & 1,
            1,
            "actual GSE tagged pilot open"
        );
        crate::gse::apply_model_effects_for_test(
            &mesh.state,
            vec![(3, false), (4, false), (0, false)],
        );
        mesh.pump();
        assert_eq!(
            mesh.query(Board::ActuatorBoard).await.value.outputs & 6,
            0,
            "actual GSE tagged close"
        );
        assert_eq!(mesh.query(Board::ValveBoard).await.value.outputs & 1, 0);
    } else if case == "unselected" {
        crate::testing_board_lifecycle::bind_selected(true, false);
        assert!(!crate::testing_board_lifecycle::enabled());
        assert!(!crate::flight_sim::handle_command(
            &crate::types::TelemetryCommand::Launch
        ));
        assert!(!crate::flight_sim::handle_command_stamped(
            &crate::types::TelemetryCommand::RetractPlumbing,
            10000,
            10000
        ));
        let q = mesh.query(Board::ActuatorBoard).await;
        assert_eq!(q.value.result, 7);
        assert_eq!(q.value.boot, 0);
        assert_eq!(q.value.flags, 0);
    } else {
        for board in crate::testing_board_lifecycle::BOARDS {
            let q = mesh.query(board).await;
            assert_eq!(q.value.result, 0);
            assert_eq!(q.value.state, Some(15));
            mesh.recover(board).await;
        }
    }
}
fn run_case(case: &str) {
    let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","board_recovery::testing_model_tests::actual_dummy_http_query_and_all_seven_rpc_recovery","--nocapture"]).env("MODEL_RPC_CHILD","1").env("MODEL_RPC_CASE",case).env("GS_SIM_MODE","1").env("GS_RRLS_ROLE","primary").output().unwrap();
    assert!(
        output.status.success(),
        "{case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn actual_unknown_receipt_boot_and_rpc() {
    run_case("unknown");
}
#[test]
fn actual_held_fc15_across_rpc_completion() {
    run_case("held");
}
#[test]
fn actual_unselected_transport_cannot_authorize() {
    run_case("unselected");
}

#[test]
fn actual_gse_tagged_effects_reach_modeled_owner() {
    run_case("gse");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_http_confirm_uses_deployed_dummy_and_receiver_owned_journal() {
    if std::env::var_os("MODEL_CONFIRM_CHILD").is_none() {
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","board_recovery::testing_model_tests::actual_http_confirm_uses_deployed_dummy_and_receiver_owned_journal","--nocapture"]).env("MODEL_CONFIRM_CHILD","1").env("GS_SIM_MODE","1").env("GS_RRLS_ROLE","primary").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let mut fixture = tests::http_fixture().await;
    let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(8);
    Arc::get_mut(&mut fixture.state).unwrap().cmd_tx = commands_tx;
    let state = fixture.state.clone();
    *state.launch_history.lock().unwrap() = crate::launch_history::Store::open(
        fixture.directory.join("clock.json"),
        false,
        None,
        10000,
        0,
    )
    .unwrap();
    let (owner, generation) = state
        .launch_history
        .lock()
        .unwrap()
        .restored_recovery_barrier();
    state
        .operator_admission_barrier
        .store(owner, std::sync::atomic::Ordering::Release);
    state
        .automatic_generation
        .store(generation, std::sync::atomic::Ordering::Release);
    crate::gse::initialize(&state);
    let mesh = Arc::new(Mutex::new(Mesh::new(state.clone(), 0, 0)));
    {
        let mut wire = mesh.lock().unwrap();
        for (ty, endpoint, value) in [
            ("FLIGHT_STATE", "FLIGHT_STATE", 6),
            ("FLIGHT_COMMAND", "FLIGHT_CONTROLLER", 1),
            ("FLIGHT_STATE", "FLIGHT_STATE", 7),
        ] {
            let packet = Packet::new(
                crate::telemetry_schema::data_type(ty),
                &[
                    crate::telemetry_schema::endpoint(endpoint),
                    crate::telemetry_schema::endpoints::GROUND_STATION,
                ],
                "GS",
                wire.host.network_time_ms().unwrap(),
                Arc::from([value]),
            )
            .unwrap();
            wire.host.tx_queue(packet).unwrap();
            wire.pump();
        }
    }
    let host = mesh.lock().unwrap().host.clone();
    crate::flight_sim::observe_model_clock(host.network_time_ms());
    assert!(crate::flight_sim::handle_command(
        &crate::types::TelemetryCommand::Abort
    ));
    let mut delayed_diagnostics = Vec::new();
    for _ in 0..100 {
        if let Some(packet) = crate::flight_sim::_next_state_aware_packet().unwrap()
            && packet.data_type() == crate::telemetry_schema::data_type("ABORT")
        {
            assert!(packet.payload().starts_with(b"SEDS_AUTO_ABORT_V1:"));
            assert_ne!(packet.timestamp(), 0);
            delayed_diagnostics.push(packet);
        }
        if delayed_diagnostics.len() == 2 {
            break;
        }
    }
    assert_eq!(
        delayed_diagnostics.len(),
        2,
        "actual model cause reports held outside producer queue"
    );
    let (observations, observations_stop) = crate::deferred_abort::spawn(state.clone());
    let pumping = mesh.clone();
    let pump = tokio::spawn(async move {
        loop {
            pumping.lock().unwrap().pump_wire_once();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
    let (_db, db_rx) = tokio::sync::mpsc::channel(8);
    let receiver = tokio::spawn(crate::telemetry_task::telemetry_task(
        state.clone(),
        host.clone(),
        vec![],
        commands_rx,
        db_rx,
        state.shutdown_subscribe(),
    ));
    async fn body(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    for board in crate::testing_board_lifecycle::BOARDS {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if endpoint_verified(
                    &state,
                    board,
                    &state.topology_router.get().unwrap().export_topology(),
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let response = tests::http(
            state.clone(),
            "POST",
            "/api/board-recovery/query",
            Some("operator"),
            json!({"board":board.sender_id()}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()]
                    .as_ref()
                    .is_some_and(|v| v.value.state == Some(15) && v.fresh(Stamp::now()))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }
    let response=tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","board":null,"mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
    let status = response.status();
    let ticket = body(response).await;
    assert_eq!(status, StatusCode::OK, "{ticket}");
    let response=tests::http(state.clone(),"POST","/api/board-recovery/confirm",Some("operator"),json!({"ticket":ticket["ticket"],"backend_instance":ticket["backend_instance"],"principal_context":ticket["principal_context"],"reset_epoch":ticket["reset_epoch"],"confirmation":ticket["confirmation"]})).await;
    let status = response.status();
    let accepted = body(response).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let operation = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(!pump.is_finished() && !receiver.is_finished());
            if let Some(op) = state.board_recovery.coordinator.operation()
                && op["completed"] == true
            {
                break op;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(operation["success"], true, "{operation}");
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .is_some_and(|p| p.published && p.verified),
        "actual host durable verification/publication required"
    );
    for board in crate::testing_board_lifecycle::BOARDS {
        let row = state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()]
            .clone()
            .unwrap();
        assert_eq!(row.value.result, 0);
        assert_eq!(row.value.state, Some(0));
        assert_eq!(row.value.flags & 1, 0);
    }
    crate::gse::apply_model_effects_for_test(&state, vec![(3, true), (0, true)]);
    for (board, bit) in [(Board::ActuatorBoard, 2), (Board::ValveBoard, 1)] {
        let response = tests::http(
            state.clone(),
            "POST",
            "/api/board-recovery/query",
            Some("operator"),
            json!({"board":board.sender_id()}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let nonce = state.board_recovery.inner.lock().unwrap().queries[index(board).unwrap()]
            .as_ref()
            .unwrap()
            .nonce;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()]
                    .as_ref()
                    .is_some_and(|v| {
                        v.value.nonce == nonce
                            && v.value.result == 0
                            && v.value.outputs & bit == bit
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("verified postlaunch recovery must rearm fresh tagged GSE output");
    }
    let completed_boundary = state
        .launch_history
        .lock()
        .unwrap()
        .completed_abort_boundary()
        .unwrap();
    let manual_epoch = state.deferred_abort.manual_epoch();
    let local_before_delayed_reports = state.local_flight_state_snapshot();
    assert_ne!(
        local_before_delayed_reports,
        crate::types::FlightState::Aborted
    );
    let side = mesh.lock().unwrap().sides[0];
    for packet in delayed_diagnostics {
        assert!(packet.timestamp() < completed_boundary);
        let id = packet.packet_id();
        let before = mesh
            .lock()
            .unwrap()
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.packet_id() == id)
            .count();
        host.rx_queue_from_side(packet, side).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mesh
                    .lock()
                    .unwrap()
                    .received
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|p| p.packet_id() == id)
                    .count()
                    > before
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        state.deferred_abort.manual_epoch(),
        manual_epoch,
        "old generated reports never become new manual Stop"
    );
    assert_eq!(
        state.local_flight_state_snapshot(),
        local_before_delayed_reports
    );
    let manual = Packet::new(
        crate::telemetry_schema::data_type("ABORT"),
        &[crate::telemetry_schema::endpoints::ABORT],
        "GS",
        0,
        Arc::from(b"SEDS_MANUAL_ABORT_V1:".as_slice()),
    )
    .unwrap();
    host.rx_queue_from_side(manual, side).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state.deferred_abort.manual_epoch() > manual_epoch && state.abort_indicator_latched()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    state.request_shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(3), receiver).await;
    pump.abort();
    let _ = observations_stop.send(());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), observations)
            .await
            .unwrap()
            .unwrap()
            .quiesced
    );
}
