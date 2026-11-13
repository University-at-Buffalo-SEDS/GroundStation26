// TEST ONLY. Native encoded board models are not MCU storage/floor proofs.
use super::*;
use crate::recovery_pending_probe::{ClockSource, Probe};
use std::time::Instant;

const FILL: [Board; 4] = [
    Board::ActuatorBoard,
    Board::ValveBoard,
    Board::GatewayBoard,
    Board::DaqBoard,
];
fn separate(name: &str) -> bool {
    crate::operator_queue::isolated_case(module_path!(), name)
}
async fn until(limit: Duration, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(limit, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bounded native prerequisite not reached");
}
async fn native(divisor: Option<u64>, reject: bool) -> (Fixture, Arc<Probe>) {
    // Match actual main1344: desired cache is loaded before packet workers.
    let _ = crate::network_variables::flight_state();
    sedsnet::config::set_runtime_tuning_config(crate::network_delivery_tuning()).unwrap();
    let tuning = sedsnet::config::runtime_tuning_config();
    assert_eq!(tuning.reliable_retransmit_ms, 400);
    assert_eq!(tuning.reliable_max_retries, 11);
    let probe = Probe::new(divisor);
    probe.reject_gb.store(reject, Ordering::Release);
    probe.install();
    let clock = probe
        .clock
        .clone()
        .map(|clock| Box::new(ClockSource(clock)) as Box<dyn sedsnet::router::Clock + Send + Sync>);
    let fixture = Fixture::from_saved_with_clock_and_services(None, None, clock, true).await;
    assert_eq!(
        fixture.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    // Explicit native QUERY model setup. No SDK ACK/floor flags are supplied.
    for (board, m) in &fixture.models {
        let mut m = m.lock().unwrap();
        m.state = 0;
        if *board == Board::GatewayBoard && reject {
            m.boot = u64::MAX;
            m.result = 7;
        }
    }
    (fixture, probe)
}
async fn heartbeat(f: &Fixture, board: Board) -> u64 {
    let router = &f.native_peers.iter().find(|(b, _)| *b == board).unwrap().1;
    assert_eq!(router.sender().as_ref(), board.sender_id(), "actual heartbeat producer identity");
    let timestamp = f
        .state()
        .topology_router
        .get()
        .unwrap()
        .network_time_ms()
        .unwrap();
    let packet = Packet::new(
        crate::telemetry_schema::data_type("HEARTBEAT"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        board.sender_id(),
        timestamp,
        Arc::<[u8]>::from([]),
    )
    .unwrap();
    let id = packet.packet_id();
    router.tx_queue(packet).unwrap();
    id
}
fn pending(f: &Fixture) -> Vec<sedsnet::diagnostics::ReliablePendingStats> {
    f.state()
        .topology_router
        .get()
        .unwrap()
        .export_reliable_pending_stats()
        .into_iter()
        .filter(|r| r.data_type == crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .collect()
}
async fn automatic_idle(f: &Fixture, p: &Probe) {
    let initial_ready = native_contact_prelude(f,p,PreludeGoal::ReceivedAndIntentionalRefusal).await;
    if initial_ready.is_err() {
        // Observation ONLY after the original prerequisite has expired. No
        // additional producer, resend, clock adjustment or new intent.
        let state = f.state();
        let (received_count, received_first64) = {
            let rows = p.received.lock().unwrap();
            (rows.len(), rows.iter().take(64).map(|row| json!({
                "id":row.id,"utc":row.utc,"value":row.value,"manual":row.manual
            })).collect::<Vec<_>>())
        };
        let owners = pending(f);
        let pending_first32 = owners.iter().take(32)
            .map(|owner|format!("{owner:?}")).collect::<Vec<_>>();
        let peer_memory = f.native_peers.iter().map(|(board,router)|json!({
            "board":board.sender_id(),"memory":router.export_memory_layout_json()
        })).collect::<Vec<_>>();
        let packet_worker_finished = p.packet_worker_abort.lock().unwrap()
            .as_ref().map(tokio::task::AbortHandle::is_finished);
        let diagnostic = json!({
            "state":format!("{:?}",state.local_flight_state_snapshot()),
            "all_required_seen":state.all_required_boards_seen(),
            "generation":crate::gse::automatic_generation(&state),
            "manual_epoch":state.manual_abort_intent.epoch(),
            "received_count":received_count,"received_first64":received_first64,
            "refusal_errors":p.refusal_errors.load(Ordering::Acquire),
            "automatic_completed":p.automatic_completed.load(Ordering::Acquire),
            "observations_dropped":p.observations_dropped.load(Ordering::Acquire),
            "entries_dropped":p.entries_dropped.load(Ordering::Acquire),
            "wire_dropped":p.wire_dropped.load(Ordering::Acquire),
            "pump_finished":f.pump.is_finished(),"receiver_finished":f.receiver.is_finished(),
            "packet_worker_finished":packet_worker_finished,
            "pending_count":owners.len(),"pending_first32":pending_first32,
            "host_memory":state.topology_router.get().unwrap().export_memory_layout_json(),
            "peer_memory":peer_memory
        });
        eprintln!("R2_INITIAL_PREREQUISITE_DIAGNOSTIC {diagnostic}");
    }
    initial_ready.expect("bounded native prerequisite not reached");
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Idle
    );
    let rows = p.received.lock().unwrap();
    let first = &rows[0];
    assert!(first.utc > 0);
    assert_eq!(first.value, Some(1));
    assert!(rows.iter().all(|r| r.id == first.id && r.utc == first.utc));
    eprintln!(
        "NATIVE_ACTUAL_AUTO_PUBLISH id={:#x} originalUTC={} observations={} generation={}",
        first.id,
        first.utc,
        rows.len(),
        crate::gse::automatic_generation(&f.state())
    );
}
async fn r2_automatic_idle_with_finite_modeled_inputs(f: &Fixture, p: &Probe) {
    // Native modeled stimulus only, not a hardware HEARTBEAT-rate claim.
    // No task is spawned. Drop/cancellation stops future production; already
    // accepted original frames remain owned and are explicitly NOT drained.
    let state = f.state();
    let began = Instant::now();
    let mut next_send = began;
    let mut windows = 0usize;
    let mut minted = Vec::<(String, u64, u64)>::with_capacity(48);
    let mut producer_stopped = false;
    let mut stop_observation = None;
    let initial_ready = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let actual = state.local_flight_state_snapshot();
            if !producer_stopped && actual != crate::types::FlightState::Startup {
                producer_stopped = true;
                stop_observation = Some((format!("{actual:?}"), began.elapsed()));
            }
            if !p.received.lock().unwrap().is_empty()
                && p.refusal_errors.load(Ordering::Acquire) > 0 {
                    break;
                }
            if !producer_stopped && windows < 12 && Instant::now() >= next_send {
                windows += 1;
                for board in FILL {
                    // No owner lock is held over SDK. A concurrent admission
                    // after this observation can leave an original queued tail.
                    let actual = state.local_flight_state_snapshot();
                    if actual != crate::types::FlightState::Startup {
                        producer_stopped = true;
                        stop_observation = Some((format!("{actual:?}"), began.elapsed()));
                        break;
                    }
                    let peer = &f.native_peers.iter().find(|(b, _)| *b == board).unwrap().1;
                    assert_eq!(peer.sender().as_ref(), board.sender_id());
                    let utc = state.topology_router.get().unwrap().network_time_ms().unwrap();
                    let packet = Packet::new(
                        crate::telemetry_schema::data_type("HEARTBEAT"),
                        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                        board.sender_id(), utc, Arc::<[u8]>::from([]),
                    ).unwrap();
                    assert!(minted.len() < 48);
                    assert!(!minted.iter().any(|(_, id, _)| *id == packet.packet_id()),
                        "each modeled input must be a distinct original");
                    minted.push((board.sender_id().to_owned(), packet.packet_id(), packet.timestamp()));
                    peer.tx_queue(packet).unwrap();
                }
                // Schedule from NOW, not the prior due time: no catch-up burst.
                next_send = Instant::now() + Duration::from_millis(250);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await;
    eprintln!("R2_FINITE_MODELED_PRELUDE elapsed={:?} windows={} fresh_inputs={} stop={:?} already_queued_residuals=NOT_DRAINED inputs={minted:?}",
        began.elapsed(), windows, minted.len(), stop_observation);
    assert!(windows <= 12 && minted.len() <= 48);
    if initial_ready.is_err() {
        // Observation ONLY after the original prerequisite has expired. No
        // additional producer, resend, clock adjustment or new intent.
        let state = f.state();
        let (received_count, received_first64) = {
            let rows = p.received.lock().unwrap();
            (rows.len(), rows.iter().take(64).map(|row| json!({
                "id":row.id,"utc":row.utc,"value":row.value,"manual":row.manual
            })).collect::<Vec<_>>())
        };
        let owners = pending(f);
        let pending_first32 = owners.iter().take(32)
            .map(|owner|format!("{owner:?}")).collect::<Vec<_>>();
        let peer_memory = f.native_peers.iter().map(|(board,router)|json!({
            "board":board.sender_id(),"memory":router.export_memory_layout_json()
        })).collect::<Vec<_>>();
        let packet_worker_finished = p.packet_worker_abort.lock().unwrap()
            .as_ref().map(tokio::task::AbortHandle::is_finished);
        let diagnostic = json!({
            "state":format!("{:?}",state.local_flight_state_snapshot()),
            "all_required_seen":state.all_required_boards_seen(),
            "generation":crate::gse::automatic_generation(&state),
            "manual_epoch":state.manual_abort_intent.epoch(),
            "received_count":received_count,"received_first64":received_first64,
            "refusal_errors":p.refusal_errors.load(Ordering::Acquire),
            "automatic_completed":p.automatic_completed.load(Ordering::Acquire),
            "observations_dropped":p.observations_dropped.load(Ordering::Acquire),
            "entries_dropped":p.entries_dropped.load(Ordering::Acquire),
            "wire_dropped":p.wire_dropped.load(Ordering::Acquire),
            "pump_finished":f.pump.is_finished(),"receiver_finished":f.receiver.is_finished(),
            "packet_worker_finished":packet_worker_finished,
            "pending_count":owners.len(),"pending_first32":pending_first32,
            "host_memory":state.topology_router.get().unwrap().export_memory_layout_json(),
            "peer_memory":peer_memory
        });
        eprintln!("R2_INITIAL_PREREQUISITE_DIAGNOSTIC {diagnostic}");
    }
    initial_ready.expect("bounded native prerequisite not reached");
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Idle
    );
    let rows = p.received.lock().unwrap();
    let first = &rows[0];
    assert!(first.utc > 0);
    assert_eq!(first.value, Some(1));
    assert!(rows.iter().all(|r| r.id == first.id && r.utc == first.utc));
    eprintln!(
        "NATIVE_ACTUAL_AUTO_PUBLISH id={:#x} originalUTC={} observations={} generation={}",
        first.id,
        first.utc,
        rows.len(),
        crate::gse::automatic_generation(&f.state())
    );
}
async fn prepare_reset_response(f: &Fixture) -> Response {
    let s = f.state();
    // Explicit optional participation never removes the mode's required base.
    crate::board_recovery::tests::http(s.clone(), "POST", "/api/board-recovery/prepare", Some("operator"), json!({
        "action":"reset_startup", "boards":FILL.iter().map(|board| board.sender_id()).collect::<Vec<_>>(),
        "board":null, "mode":null, "backend_instance":hex(&s.board_recovery.identity.unwrap()),
        "reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&s))
    })).await
}
async fn prepare_selected_reset(f: &Fixture) -> Value {
    let response = prepare_reset_response(f).await;
    let code = response.status();
    let value = body(response).await;
    assert_eq!(code, StatusCode::OK, "{value}");
    let required: Vec<Board> = match crate::station_mode::local_mode() {
        crate::station_mode::Mode::Hitl => Vec::new(),
        crate::station_mode::Mode::TestFire => FILL.to_vec(),
        crate::station_mode::Mode::Testing | crate::station_mode::Mode::Normal => BOARDS.to_vec(),
    };
    let expected: Vec<_> = BOARDS.iter().filter(|board| required.contains(board) || FILL.contains(board))
        .map(|board| board.sender_id()).collect();
    assert_eq!(value["targets"], json!(expected));
    // No board is required: the reset covers whichever are available.
    assert_eq!(value["required_proof_boards"], json!(Vec::<&str>::new()));
    value
}
async fn operation(f: &Fixture, limit: Duration) -> Value {
    tokio::time::timeout(limit, async {
        loop {
            assert!(
                !f.pump.is_finished() && !f.receiver.is_finished(),
                "native actual wire owners died"
            );
            if let Some(o) = f.state().board_recovery.coordinator.operation()
                && o["completed"] == true
            {
                return o;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("native operation completion bound")
}
fn applies(f: &Fixture) -> usize {
    f.models
        .iter()
        .map(|(_, m)| m.lock().unwrap().applied)
        .sum()
}
async fn hold_last_required(f: &Fixture, p: &Probe) {
    p.capture_entries.store(true, Ordering::Release);
    let mut originals = Vec::new();
    // DAQ may be optional in HITL+TestFire; Gateway never is. Keep the
    // original receiver ownership pause on a genuinely last-required input.
    for board in [Board::ActuatorBoard, Board::ValveBoard, Board::DaqBoard] {
        originals.push(heartbeat(f, board).await);
    }
    until(Duration::from_secs(2), || {
        let entries = p.entries.lock().unwrap();
        originals
            .iter()
            .all(|id| entries.iter().any(|r| r.0 == *id))
    })
    .await;
    assert!(!f.state().all_required_boards_seen(), "last required GB receipt must still be absent");
    assert!(f.state().board_required_for_progression(Board::GatewayBoard));
    p.before_auto.store(true, Ordering::Release);
    let expected = (
        crate::gse::automatic_generation(&f.state()),
        f.state().manual_abort_intent.epoch(),
    );
    let original_last = heartbeat(f, Board::GatewayBoard).await;
    tokio::time::timeout(Duration::from_secs(2), p.auto_entered.notified())
        .await
        .unwrap();
    p.capture_entries.store(false, Ordering::Release);
    assert_eq!(
        p.entries_dropped.load(Ordering::Acquire),
        0,
        "required entry evidence lost"
    );
    let entry = p
        .entries
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.0 == original_last)
        .copied()
        .expect("exact original Gateway entry");
    assert_eq!((entry.1, entry.2), expected);
    eprintln!(
        "NATIVE_ORIGINAL_ENTRY id={:#x} generation={} manual={} dropped=0",
        entry.0, entry.1, entry.2
    );
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
}
fn cached_fstate(f: &Fixture) -> Option<Packet> {
    f.state()
        .topology_router
        .get()
        .unwrap()
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap()
}
struct Release(Arc<Probe>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r1_actual_auto_publisher_and_initial_recovery_owner() {
    crate::mode_suite!(TestFire);
    if separate("r1_actual_auto_publisher_and_initial_recovery_owner") {
        return;
    }
    let (f, p) = native(None, true).await;
    automatic_idle(&f, &p).await;
    let rows = pending(&f);
    assert!(!rows.is_empty());
    let pid = p.received.lock().unwrap()[0].id;
    assert!(rows.iter().any(|r| r.packet_id == pid));
    let ticket = prepare_selected_reset(&f).await;
    assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
    // Do not assert it must fail: natural 4.5s retirement may allow commissioning.
    let o = operation(&f, Duration::from_secs(16)).await;
    eprintln!("R1_ACTUAL_OUTCOME {o}");
    assert_ne!(
        o["success"], true,
        "commissioning IO must never be reset success"
    );
    if o["message"].as_str().unwrap().contains("recovery waits until it is delivered") {
        assert_eq!(applies(&f), 0);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r2_slow_model_natural_exhaustion_keeps_original_identity() {
    crate::mode_suite!(TestFire);
    if separate("r2_slow_model_natural_exhaustion_keeps_original_identity") {
        return;
    }
    let (f, p) = native(Some(10), true).await;
    r2_automatic_idle_with_finite_modeled_inputs(&f, &p).await;
    let original = p.received.lock().unwrap()[0].clone();
    let original_cache = cached_fstate(&f).expect("admitted original must be SDK-retained");
    assert_eq!((original_cache.packet_id(), original_cache.timestamp(), original_cache.payload()),
        (original.id, original.utc, &[1][..]));
    let ticket = prepare_selected_reset(&f).await;
    assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
    let o = operation(&f, Duration::from_secs(16)).await;
    assert_eq!(o["success"], false);
    // The Idle publication the Gateway Board never acknowledges no longer
    // holds the reset: after a bounded wait the recovery supersedes it and
    // goes on to the boards, where GB's own commissioning keeps it from
    // completing.
    assert!(!o["message"].as_str().unwrap().contains("undelivered FLIGHT_STATE"), "{o}");
    // This is a FAILED reset: the barrier remains closed and no Startup was
    // applied. Queued prelude tails are not claimed empty or drained.
    assert_ne!(f.state().operator_admission_barrier.load(Ordering::Acquire), 0);
    assert_eq!(f.state().local_flight_state_snapshot(), crate::types::FlightState::Idle);
    let after_failure = cached_fstate(&f).unwrap();
    assert_eq!((after_failure.packet_id(), after_failure.timestamp(), after_failure.payload()),
        (original.id, original.utc, original_cache.payload()));
    let start = Instant::now();
    until(Duration::from_secs(55), || pending(&f).is_empty()).await;
    assert!(
        !f.pump.is_finished() && !f.receiver.is_finished(),
        "native wire/telemetry worker died"
    );
    assert_eq!(p.entries_dropped.load(Ordering::Acquire), 0);
    assert_eq!(p.observations_dropped.load(Ordering::Acquire), 0);
    assert_ne!(f.state().operator_admission_barrier.load(Ordering::Acquire), 0);
    assert_eq!(f.state().local_flight_state_snapshot(), crate::types::FlightState::Idle);
    let after_retirement = cached_fstate(&f).unwrap();
    assert_eq!((after_retirement.packet_id(), after_retirement.timestamp(), after_retirement.payload()),
        (original.id, original.utc, original_cache.payload()));
    let rows = p.received.lock().unwrap();
    let attempts = sedsnet::config::runtime_tuning_config().reliable_max_retries as usize + 1;
    assert_eq!(
        rows.len(),
        attempts,
        "original initial plus every retransmission required"
    );
    assert!(
        rows.iter()
            .all(|r| r.id == original.id && r.utc == original.utc)
    );
    eprintln!(
        "R2_NATURAL_RETIREMENT after_failed_operation_wall={:?} rows={} originalid={:#x} UTC={} ACKs_not_synthesized=true",
        start.elapsed(),
        rows.len(),
        original.id,
        original.utc
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_managed_get_api_attempt_and_pending_owner_inventory() {
    crate::mode_suite!(TestFire);
    if separate("r3_managed_get_api_attempt_and_pending_owner_inventory") {
        return;
    }
    let (f, p) = native(None, true).await;
    automatic_idle(&f, &p).await;
    p.refresh.store(true, Ordering::Release);
    let started = Instant::now();
    let mut ledger = Vec::new();
    while started.elapsed() < Duration::from_secs(11) {
        let rows = pending(&f);
        ledger.push((
            started.elapsed().as_millis(),
            rows.iter()
                .map(|r| (r.packet_id, r.retries, r.last_send_ms))
                .collect::<Vec<_>>(),
        ));
        assert!(ledger.len() < 600);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eprintln!("R3_REAL_GET_LEDGER {ledger:?}");
    assert!(!p.received.lock().unwrap().is_empty());
    // This is a measured inventory, not an assertion that quiet must occur.
    assert!(ledger.iter().any(|(_, rows)| !rows.is_empty()));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_new_abort_episode_refuses_second_prepare_after_actual_quiet() {
    crate::mode_suite!(TestFire);
    if separate("r4_new_abort_episode_refuses_second_prepare_after_actual_quiet") {
        return;
    }
    let (f, p) = native(None, true).await;
    automatic_idle(&f, &p).await;
    p.refresh.store(true, Ordering::Release);
    let first = prepare_selected_reset(&f).await;
    assert_eq!(f.confirm(&first).await.status(), StatusCode::OK);
    let old = operation(&f, Duration::from_secs(16)).await;
    assert_ne!(old["success"], true);
    let start = Instant::now();
    let mut quiet = None;
    while start.elapsed() < Duration::from_secs(7) {
        let s = f.state();
        let r = s.topology_router.get().unwrap();
        // A momentary SDK core-busy try is unknown, not pending: the managed
        // refresh of this test-fire cohort keeps the core briefly busy. Only
        // an actually pending control frame breaks the quiet window.
        let pending = s.has_pending_recovery_control_tx()
            || crate::telemetry_task::RECOVERY_CONTROL_TYPES.iter().any(|name| {
                matches!(r.try_has_pending_reliable_tx_type(crate::telemetry_schema::data_type(name)), Ok(true))
            });
        if !pending {
            let since = quiet.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(1250) {
                break;
            }
        } else {
            quiet = None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        quiet.is_some_and(|t| t.elapsed() >= Duration::from_millis(1250)),
        "UNAVAILABLE_PREREQUISITE: real managed refresh did not permit quiet; no new ticket was invented"
    );
    let state = f.state();
    let old_pending = state.launch_history.lock().unwrap().recovery_pending();
    let journal = std::fs::read(f.http.directory.join("clock.json")).unwrap();
    // Actual observational Store snapshot includes values, revision, volatile
    // identity and the bytes (or absence) at its real canonical path.
    let stored_before = crate::network_variables::automatic_idle_test_snapshot();
    let cached_before = {
        let packet = cached_fstate(&f).expect("actual original cached FSTATE before second PREPARE");
        (packet.packet_id(), packet.timestamp(), packet.sender().to_owned(), packet.payload().to_vec())
    };
    let startup_publications_before = f.incoming_startup.lock().unwrap().iter()
        .filter(|(_, packet)| packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE")
            && packet.payload() == [0]).count();
    let applied = applies(&f);
    let controls = f.controller_receipts.lock().unwrap().len();
    let generation = crate::gse::automatic_generation(&state);
    let (before_scope, _, _) = state.board_abort.capture_reset_context(|| 0).unwrap();
    let before_errors = state.board_abort.snapshot(0).active_errors;
    assert!(!before_errors.iter().any(|error| error.board == "GB"));
    let old_gb_queries = f.models.iter().find(|(board, _)| *board == Board::GatewayBoard).unwrap().1
        .lock().unwrap().requests.len();
    let response = prepare_reset_response(&f).await;
    let code = response.status();
    let refused = body(response).await;
    assert_eq!(code, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "recovery_unavailable");
    assert_eq!(refused["message"], "Scoped reset policy or Abort episode changed; prepare a new intent");
    assert!(refused.get("ticket").is_none(), "refused PREPARE must not mint a ticket");
    let (after_scope, _, _) = state.board_abort.capture_reset_context(|| 0).unwrap();
    assert_ne!(before_scope, after_scope, "actual queried Abort episode must fence the original selection");
    assert!(state.board_abort.snapshot(0).active_errors.iter().any(|error| error.board == "GB"));
    let model = f.models.iter().find(|(board, _)| *board == Board::GatewayBoard).unwrap().1.lock().unwrap();
    assert!(model.requests.len() > old_gb_queries);
    let query = model.requests.last().unwrap().clone();
    assert_eq!(query.payload()[1], 0, "second PREPARE only queried; no Apply");
    assert!(model.retained_first.is_some() && model.applied == 0);
    let expected_boot_epoch = (model.boot, model.epoch);
    drop(model);
    let observation = state.board_recovery.inner.lock().unwrap().values[index(Board::GatewayBoard).unwrap()]
        .as_ref().expect("actual correlated GB readback").clone();
    assert!(observation.fresh(Stamp::now()));
    assert_eq!(observation.value.nonce, u64::from_le_bytes(query.payload()[4..12].try_into().unwrap()));
    assert_eq!((observation.value.boot, observation.value.epoch), expected_boot_epoch);
    assert_eq!(observation.value.issuer, u64::from_le_bytes(query.payload()[24..32].try_into().unwrap()));
    assert_ne!(observation.value.flags & 1, 0, "actual model reply raised Abort, not fabricated host state");
    assert_eq!(applies(&f), applied);
    assert_eq!(f.controller_receipts.lock().unwrap().len(), controls);
    assert_eq!(crate::gse::automatic_generation(&state), generation);
    assert_eq!(state.launch_history.lock().unwrap().recovery_pending(), old_pending);
    assert_eq!(std::fs::read(f.http.directory.join("clock.json")).unwrap(), journal);
    assert_eq!(crate::network_variables::automatic_idle_test_snapshot(), stored_before,
        "protective refusal must not change actual Store/canonical bytes");
    let cached_after = {
        let packet = cached_fstate(&f).expect("actual original cached FSTATE after protective refusal");
        (packet.packet_id(), packet.timestamp(), packet.sender().to_owned(), packet.payload().to_vec())
    };
    assert_eq!(cached_after, cached_before, "no new FSTATE identity/source UTC/payload in SDK cache");
    assert_eq!(f.incoming_startup.lock().unwrap().iter()
        .filter(|(_, packet)| packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE")
            && packet.payload() == [0]).count(), startup_publications_before,
        "no global Startup publication after the refused second PREPARE");
    eprintln!("R4_PROTECTED_SECOND_PREPARE old={old} refused={refused}; no third confirmation");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_unshippable_queue_clear_does_not_retire_owner() {
    crate::mode_suite!(TestFire);
    if separate("r5_unshippable_queue_clear_does_not_retire_owner") {
        return;
    }
    let (f, p) = native(None, true).await;
    automatic_idle(&f, &p).await;
    let before = pending(&f);
    assert!(!before.is_empty());
    f.state().topology_router.get().unwrap().clear_tx_queue();
    let after = pending(&f);
    assert!(
        before
            .iter()
            .any(|a| after.iter().any(|b| a.packet_id == b.packet_id)),
        "clearing queued work must not falsely certify retained ownership retirement"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r6_abort_launch_and_unknown_origin_owners_not_discarded() {
    crate::mode_suite!(TestFire);
    if separate("r6_abort_launch_and_unknown_origin_owners_not_discarded") {
        return;
    }
    let (f, p) = native(None, true).await;
    let s = f.state();
    let router = s.topology_router.get().unwrap();
    for value in [15u8, 7u8, 1u8] {
        let packet = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("FLIGHT_STATE")],
            "GS",
            router.network_time_ms().unwrap(),
            Arc::<[u8]>::from([value]),
        )
        .unwrap();
        let id = packet.packet_id();
        router.set_network_variable(packet).unwrap();
        until(Duration::from_secs(2), || {
            pending(&f).iter().any(|r| r.packet_id == id)
        })
        .await;
        assert!(pending(&f).iter().any(|r| r.packet_id == id));
    }
    // Actual manual-intent primitive; explicitly not an HTTP or actuator proof.
    let epoch = s.manual_abort_intent.request();
    assert_eq!(s.manual_abort_intent.pending_epoch(), Some(epoch));
    assert!(!pending(&f).is_empty());
    assert_eq!(applies(&f), 0);
    assert!(p.received.lock().unwrap().iter().all(|r| r.utc > 0));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r7_old_auto_publication_must_not_cross_closed_recovery() {
    crate::mode_suite!(TestFire);
    if separate("r7_old_auto_publication_must_not_cross_closed_recovery") {
        return;
    }
    let (f, p) = native(None, true).await;
    let release = Release(p.clone());
    hold_last_required(&f, &p).await;
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let cache = cached_fstate(&f);
    let s = f.state();
    let g = crate::gse::automatic_generation(&s);
    crate::gse::reset_generation::close_operator_admission(&s, g, 77).unwrap();
    p.release();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(release);
    assert_eq!(
        s.local_flight_state_snapshot(),
        crate::types::FlightState::Startup,
        "EXPECTED_OLD_RED: automatic receive publication crossed closed admission"
    );
    assert!(p.received.lock().unwrap().is_empty());
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    assert!(
        !s.topology_router
            .get()
            .unwrap()
            .has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_STATE"))
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r8_original_receive_must_not_survive_real_reset_aba() {
    crate::mode_suite!(TestFire);
    if separate("r8_original_receive_must_not_survive_real_reset_aba") {
        return;
    }
    let (f, p) = native(None, false).await;
    let release = Release(p.clone());
    hold_last_required(&f, &p).await;
    let old = crate::gse::automatic_generation(&f.state());
    let ticket = prepare_selected_reset(&f).await;
    assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
    let result = operation(&f, Duration::from_secs(12)).await;
    assert_eq!(
        result["success"], true,
        "native encoded context prerequisite {result}"
    );
    assert!(crate::gse::automatic_generation(&f.state()) > old);
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let cache = cached_fstate(&f);
    p.release();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(release);
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup,
        "EXPECTED_OLD_RED: old captured receive crossed actual reset-generation ABA"
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    assert!(
        !p.received
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.value == Some(1)),
        "no new old-event Idle owner"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r9_actual_auto_publication_requires_inflight_claim() {
    crate::mode_suite!(TestFire);
    if separate("r9_actual_auto_publication_requires_inflight_claim") {
        return;
    }
    let (f, p) = native(None, true).await;
    let release = Release(p.clone());
    p.block_commit.store(true, Ordering::Release);
    native_contact_prelude(&f,&p,PreludeGoal::ActualPreparedHook).await.unwrap();
    let count = f.state().operator_dispatch_inflight.load(Ordering::Acquire);
    p.release();
    drop(release);
    assert!(
        count > 0,
        "EXPECTED_OLD_RED: actual automatic publisher performing durable IO owns no counted dispatch claim"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r10_actual_stop_during_state_commit_must_prevent_late_idle() {
    crate::mode_suite!(TestFire);
    if separate("r10_actual_stop_during_state_commit_must_prevent_late_idle") {
        return;
    }
    let (f, p) = native(None, true).await;
    let release = Release(p.clone());
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let cache = cached_fstate(&f);
    p.block_commit.store(true, Ordering::Release);
    native_contact_prelude(&f,&p,PreludeGoal::ActualPreparedHook).await.unwrap();
    let epoch = f.state().manual_abort_intent.request();
    p.release();
    drop(release);
    // RAM-only successor: pause the real prepared-admission boundary, not a
    // removed automatic disk commit. Completion must be observed before the
    // no-effects assertion; absence of incoming traffic alone proves nothing.
    until(Duration::from_secs(3), || {
        p.automatic_completed.load(Ordering::Acquire)
    })
    .await;
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(crate::network_variables::flight_state(), 0);
    assert!(pending(&f).is_empty());
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    assert!(
        !p.received
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.value == Some(1) && r.manual >= epoch),
        "EXPECTED_OLD_RED: old automatic Idle reached SDK after independently retained Manual Stop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r11_cancel_prepared_receiver_has_no_late_automatic_effects() {
    crate::mode_suite!(TestFire);
    if separate("r11_cancel_prepared_receiver_has_no_late_automatic_effects") {
        return;
    }
    let (f, p) = native(None, true).await;
    let release = Release(p.clone());
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let cache = cached_fstate(&f);
    p.block_commit.store(true, Ordering::Release);
    native_contact_prelude(&f,&p,PreludeGoal::ActualPreparedHook).await.unwrap();
    assert!(f.state().operator_dispatch_inflight.load(Ordering::Acquire) > 0);
    // telemetry_task is a parent wrapper; cancel the exact owning packet
    // worker, not its JoinHandle-owning parent. Outer cancellation alone does
    // NOT revoke a valid counted child intent (prior failure retained).
    let cancellation = p
        .packet_worker_abort
        .lock()
        .unwrap()
        .clone()
        .expect("actual packet worker registered");
    cancellation.abort();
    p.release();
    drop(release);
    until(Duration::from_secs(3), || cancellation.is_finished()).await;
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    assert!(pending(&f).is_empty());
    assert!(p.received.lock().unwrap().is_empty());
    assert_eq!(
        f.state().operator_dispatch_inflight.load(Ordering::Acquire),
        0
    );
}

fn r12_failure_snapshot(phase: &'static str, f: &Fixture, p: &Probe) {
    let state=f.state();
    let local=state.state.try_lock().map(|v|format!("{v:?}"))
        .unwrap_or_else(|e|format!("unavailable:{e:?}"));
    let contacts=match state.board_status.try_lock() {
        Ok(map)=>json!(FILL.iter().map(|board|json!({
            "board":board.sender_id(),"last_seen_ms":map.get(board).and_then(|r|r.last_seen_ms)
        })).collect::<Vec<_>>()),
        Err(error)=>json!({"unavailable":format!("{error:?}")}),
    };
    let received=match p.received.try_lock() {
        Ok(rows)=>json!({"count":rows.len(),"first8":rows.iter().take(8).map(|r|
            json!({"id":r.id,"utc":r.utc,"value":r.value,"manual":r.manual})).collect::<Vec<_>>()}),
        Err(error)=>json!({"unavailable":format!("{error:?}")}),
    };
    let packet_worker=match p.packet_worker_abort.try_lock() {
        Ok(handle)=>json!(handle.as_ref().map(tokio::task::AbortHandle::is_finished)),
        Err(error)=>json!({"unavailable":format!("{error:?}")}),
    };
    let snapshot=json!({"phase":phase,"local_state":local,"contacts":contacts,
        "automatic_completed":p.automatic_completed.load(Ordering::Acquire),
        "final_commit_outcomes":p.final_commit_counts(),
        "final_outcome_observer_losses":crate::recovery_pending_probe::final_outcome_observer_losses(),
        "received":received,"refusal_errors":p.refusal_errors.load(Ordering::Acquire),
        "observations_dropped":p.observations_dropped.load(Ordering::Acquire),
        "entries_dropped":p.entries_dropped.load(Ordering::Acquire),
        "wire_dropped":p.wire_dropped.load(Ordering::Acquire),
        "sequence_ticks":p.sequence_ticks.load(Ordering::Acquire),
        "generation":crate::gse::automatic_generation(&state),
        "operator_inflight":state.operator_dispatch_inflight.load(Ordering::Acquire),
        "barrier":state.operator_admission_barrier.load(Ordering::Acquire),
        "shutdown_requested":state.shutdown_requested.load(Ordering::Acquire),
        "pump_finished":f.pump.is_finished(),"receiver_finished":f.receiver.is_finished(),
        "packet_worker_finished":packet_worker,
        "pending":"not_sampled_no_nonblocking_SDK_export",
        "scope":"post-expired-original/non-atomic/try-only; no extra heartbeat, query, SDK export, provider, clock or retry; completed is returned-not-success"});
    eprintln!("R12_ORIGINAL_PREREQUISITE_FAILURE {snapshot}");

}

// R12-only observation. Same original limit/20ms predicate cadence. A failed
// original wait is never retried, extended or repaired by this diagnostic.
async fn r12_until(
    phase: &'static str,
    f: &Fixture,
    p: &Probe,
    mut condition: impl FnMut() -> bool,
) {
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await;
    if result.is_err() { r12_failure_snapshot(phase, f, p); }
    result.unwrap_or_else(|error|panic!("R12 original three-second prerequisite {phase} failed: {error}"));
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum PreludeGoal {
    ReceivedAndIntentionalRefusal,
    ReturnedAndActualGbReceipt,
    ActualPreparedHook,
}
impl PreludeGoal {
    fn label(self)->&'static str {
        match self {
            Self::ReceivedAndIntentionalRefusal=>"actual_GB_receipt_and_intentional_peer_refusal",
            Self::ReturnedAndActualGbReceipt=>"automatic_return_and_actual_GB_receipt",
            Self::ActualPreparedHook=>"actual_prepared_commit_Notify",
        }
    }
}
// Caller-owned finite stimulus only: each packet retains its newly sampled
// original UTC/ID. No captured publisher or unadmitted old packet is retried.
async fn native_contact_window(
    f:&Fixture,p:&Probe,goal:PreludeGoal,deadline:Option<tokio::time::Instant>,
    inputs:&mut Vec<(String,u64,u64)>,stopped:&mut bool,
) {
    for board in FILL {
        if f.state().local_flight_state_snapshot()!=crate::types::FlightState::Startup
            || goal==PreludeGoal::ActualPreparedHook && !p.block_commit.load(Ordering::Acquire) {
            // The original flag is consumed AT the actual prepared hook, before
            // its Notify. Stop minting now, but NEVER credit this flag as Notify.
            *stopped=true;
            break;
        }
        if deadline.is_some_and(|end|tokio::time::Instant::now()>=end){break;}
        let peer=&f.native_peers.iter().find(|(b,_)|*b==board).unwrap().1;
        assert_eq!(peer.sender().as_ref(),board.sender_id());
        let utc=f.state().topology_router.get().unwrap().network_time_ms().unwrap();
        let packet=Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],board.sender_id(),
            utc,Arc::<[u8]>::from([])).unwrap();
        assert!(inputs.len()<48);
        assert!(!inputs.iter().any(|(_,id,_)|*id==packet.packet_id()),
            "fresh modeled contact must have a distinct original identity");
        inputs.push((board.sender_id().into(),packet.packet_id(),packet.timestamp()));
        peer.tx_queue(packet).unwrap();
    }
}
async fn native_contact_prelude(
    f:&Fixture,p:&Probe,goal:PreludeGoal,
)->Result<(),tokio::time::error::Elapsed> {
    native_contact_prelude_gated(f,p,goal,None).await
}
/// As `native_contact_prelude`, but a later window is sent only once `gate`
/// is set. A caller that must act between the first window and the next one
/// fences that order explicitly instead of racing the 250 ms window timer,
/// which a loaded host can overrun.
async fn native_contact_prelude_gated(
    f:&Fixture,p:&Probe,goal:PreludeGoal,gate:Option<&std::sync::atomic::AtomicBool>,
)->Result<(),tokio::time::error::Elapsed> {
    let state=f.state();
    let required:Vec<_>=Board::ALL.iter().copied()
        .filter(|board|state.board_required_for_progression(*board)).collect();
    assert!(required.contains(&Board::GatewayBoard));
    assert!(required.iter().all(|board|FILL.contains(board)));
    if goal==PreludeGoal::ActualPreparedHook {
        assert!(p.block_commit.load(Ordering::Acquire),"prepared goal requires the original armed hook");
    }
    let mut inputs=Vec::with_capacity(48);
    let mut stopped=false;
    // Same original initial window, BEFORE each original three-second wait.
    native_contact_window(f,p,goal,None,&mut inputs,&mut stopped).await;
    let deadline=tokio::time::Instant::now()+Duration::from_secs(3);
    let mut windows=1usize;
    let mut next_send=tokio::time::Instant::now()+Duration::from_millis(250);
    let entered=p.commit_entered.notified();
    tokio::pin!(entered);
    let mut actual_prepared_notify=false;
    let result=tokio::time::timeout_at(deadline,async {
        loop {
            if state.local_flight_state_snapshot()!=crate::types::FlightState::Startup
                || goal==PreludeGoal::ActualPreparedHook && !p.block_commit.load(Ordering::Acquire) {
                stopped=true;
            }
            let ready=match goal {
                PreludeGoal::ReceivedAndIntentionalRefusal=>
                    !p.received.lock().unwrap().is_empty() && p.refusal_errors.load(Ordering::Acquire)>0,
                PreludeGoal::ReturnedAndActualGbReceipt=>
                    p.automatic_completed.load(Ordering::Acquire) && !p.received.lock().unwrap().is_empty(),
                PreludeGoal::ActualPreparedHook=>false,
            };
            if ready {break;}
            if !stopped && windows<12 && tokio::time::Instant::now()>=next_send
                && gate.is_none_or(|gate|gate.load(Ordering::Acquire)) {
                windows+=1;
                native_contact_window(f,p,goal,Some(deadline),&mut inputs,&mut stopped).await;
                // Schedule from NOW, never catch up an old due time in a burst.
                next_send=tokio::time::Instant::now()+Duration::from_millis(250);
            }
            tokio::select! {
                biased;
                _=&mut entered,if goal==PreludeGoal::ActualPreparedHook=>{
                    actual_prepared_notify=true;
                    stopped=true;
                    break;
                }
                _=tokio::time::sleep(Duration::from_millis(20))=>{}
            }
        }
    }).await;
    if result.is_ok() && goal==PreludeGoal::ActualPreparedHook {
        assert!(actual_prepared_notify,"only the original actual Notify proves prepared entry");
        assert!(!p.block_commit.load(Ordering::Acquire));
        // R9/R10/R11 use a fresh Probe and serial actual packet worker. This
        // exact publisher is STILL held in the original synchronous pause;
        // callers release it only AFTER this function returns. Earlier safe
        // preparation refusals may have returned, so their boolean completion
        // cannot certify this paused original's subsequent Stop/refusal.
        // Reset ONLY the observer flag, never counters/owners/IDs/epochs/Store.
        p.automatic_completed.store(false,Ordering::Release);
    }
    eprintln!("NATIVE_FINITE_CONTACT_PRELUDE goal={} windows={windows} fresh_inputs={} producer_stopped={stopped} actual_prepared_notify={actual_prepared_notify} already_queued_residuals=NOT_DRAINED inputs={inputs:?}",goal.label(),inputs.len());
    assert!(windows<=12 && inputs.len()<=48);
    if result.is_err(){r12_failure_snapshot(goal.label(),f,p);}
    result
}
async fn r12_healthy_contacts(f:&Fixture,p:&Probe) {
    native_contact_prelude(f,p,PreludeGoal::ReturnedAndActualGbReceipt).await
        .expect("original three-second automatic/GB prerequisite failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r12_healthy_actual_sdk_idle_is_live_only_one_original() {
    crate::mode_suite!(TestFire);
    if separate("r12_healthy_actual_sdk_idle_is_live_only_one_original") {
        return;
    }
    let (f, p) = native(None, false).await;
    let before = crate::network_variables::automatic_idle_test_snapshot();
    r12_healthy_contacts(&f, &p).await;
    r12_until("original_FSTATE_owner_retirement", &f, &p, || pending(&f).is_empty()).await;
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Idle
    );
    assert_eq!(crate::network_variables::flight_state(), 0);
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
    let cache = cached_fstate(&f).unwrap();
    assert_eq!(cache.payload(), &[1]);
    let rows = p.received.lock().unwrap();
    assert!(rows.iter().all(|row| row.id == cache.packet_id()
        && row.utc == cache.timestamp()
        && row.value == Some(1)));
    assert!(cache.timestamp() > 0);
    assert_eq!(p.observations_dropped.load(Ordering::Acquire), 0);
    assert_eq!(crate::recovery_pending_probe::final_outcome_observer_losses(),0);
    assert_eq!(p.final_commit_counts()[0],1,"exactly one accepted original final commit");
    assert!(!f.pump.is_finished() && !f.receiver.is_finished());
}

// A real policy owner controls the actual final try-lock. The bounded thread
// auto-releases only for cleanup; a release before refusal invalidates proof.
struct R12PolicyOwner {
    release:Option<std::sync::mpsc::Sender<()>>,
    thread:Option<std::thread::JoinHandle<()>>,
    released:Arc<std::sync::atomic::AtomicBool>,
}
impl R12PolicyOwner {
    fn start(state:Arc<AppState>)->Self {
        let (release,rx)=std::sync::mpsc::channel();
        let (held,ready)=std::sync::mpsc::channel();
        let released=Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed=released.clone();
        let thread=std::thread::spawn(move||{
            let guard=state.sequence_policy_state.lock().unwrap();
            held.send(()).unwrap();
            let _=rx.recv_timeout(Duration::from_secs(3));
            drop(guard);
            observed.store(true,Ordering::Release);
        });
        let mut owner=Self{release:Some(release),thread:Some(thread),released};
        if let Err(error)=ready.recv_timeout(Duration::from_secs(2)) {
            owner.finish();panic!("real R12 policy owner missing: {error}");
        }
        owner
    }
    fn finish(&mut self) {
        if let Some(release)=self.release.take(){let _=release.send(());}
        if let Some(thread)=self.thread.take(){
            let end=Instant::now()+Duration::from_secs(1);
            while !thread.is_finished() && Instant::now()<end {std::thread::sleep(Duration::from_millis(1));}
            assert!(thread.is_finished(),"R12 policy owner did not terminate in cleanup-only 1s");
            thread.join().unwrap();
        }
    }
}
impl Drop for R12PolicyOwner {fn drop(&mut self){self.finish();}}

#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn r12b_actual_policy_busy_refuses_one_prepared_original_without_effects() {
    crate::mode_suite!(TestFire);
    if separate("r12b_actual_policy_busy_refuses_one_prepared_original_without_effects"){return;}
    let (f,p)=native(None,false).await;
    let state=f.state();
    let before=crate::network_variables::automatic_idle_test_snapshot();
    let cache=cached_fstate(&f);
    let receipts=f.managed_state_receipts.lock().unwrap().len();
    let original_epochs=(crate::gse::automatic_generation(&state),state.manual_abort_intent.epoch(),state.deferred_abort.manual_epoch());
    p.block_commit.store(true,Ordering::Release);
    hold_last_required(&f,&p).await;
    p.auto_release.notify_one();
    // Consume the existing real prepared-commit pause, not a synthetic Busy flag.
    tokio::time::timeout(Duration::from_secs(3),p.commit_entered.notified()).await
        .expect("actual original must reach prepared commit before policy contention");
    let mut owner=R12PolicyOwner::start(state.clone());
    p.release();
    r12_until("real_policy_Busy_original_return",&f,&p,||p.automatic_completed.load(Ordering::Acquire)).await;
    assert!(!owner.released.load(Ordering::Acquire),"refusal must precede owner release/cleanup");
    assert_eq!(crate::recovery_pending_probe::final_outcome_observer_losses(),0);
    assert_eq!(p.final_commit_counts(),[0,1,0,0,0,0],"actual final Error::Busy exactly once");
    assert_eq!(state.local_flight_state_snapshot(),crate::types::FlightState::Startup);
    assert_eq!(crate::network_variables::automatic_idle_test_snapshot(),before);
    assert_eq!(cached_fstate(&f),cache);
    assert!(pending(&f).is_empty());
    assert!(p.received.lock().unwrap().is_empty());
    assert_eq!(f.managed_state_receipts.lock().unwrap().len(),receipts);
    assert_eq!((crate::gse::automatic_generation(&state),state.manual_abort_intent.epoch(),state.deferred_abort.manual_epoch()),original_epochs);
    assert_eq!(p.observations_dropped.load(Ordering::Acquire),0);
    owner.finish();
    assert_eq!(state.local_flight_state_snapshot(),crate::types::FlightState::Startup);
    assert_eq!(p.final_commit_counts(),[0,1,0,0,0,0]);
}


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r13_policy_refresh_cannot_promote_cached_contacts_while_stop_is_retained() {
    crate::mode_suite!(TestFire);
    if separate("r13_policy_refresh_cannot_promote_cached_contacts_while_stop_is_retained") {
        return;
    }
    let (f, p) = native(None, true).await;
    let release = Release(p.clone());
    hold_last_required(&f, &p).await;
    assert!(f.state().all_required_boards_seen());
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let cache = cached_fstate(&f);
    let stop = f.state().manual_abort_intent.request();
    crate::sequences::refresh_action_policy_now(&f.state());
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    p.release();
    drop(release);
    until(Duration::from_secs(3), || {
        p.automatic_completed.load(Ordering::Acquire)
    })
    .await;
    assert_eq!(f.state().manual_abort_intent.pending_epoch(), Some(stop));
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    assert!(p.received.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r14_policy_refresh_is_observational_without_eligible_fresh_receipt() {
    crate::mode_suite!(TestFire);
    if separate("r14_policy_refresh_is_observational_without_eligible_fresh_receipt") {
        return;
    }
    let (f, _p) = native(None, true).await;
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let cache = cached_fstate(&f);
    // Actual authenticated QUERY replies establish cached contacts in this
    // native fixture; no HEARTBEAT is submitted to the dequeue owner.
    let _unconfirmed = prepare_selected_reset(&f).await;
    assert!(f.state().all_required_boards_seen());
    crate::sequences::refresh_action_policy_now(&f.state());
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(cached_fstate(&f), cache);
    let generation = crate::gse::automatic_generation(&f.state());
    crate::gse::reset_generation::close_operator_admission(&f.state(), generation, 77).unwrap();
    crate::sequences::refresh_action_policy_now(&f.state());
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    assert!(pending(&f).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r15_actual_200ms_sequence_loop_cannot_publish_readiness_from_cached_contacts() {
    crate::mode_suite!(TestFire);
    if separate("r15_actual_200ms_sequence_loop_cannot_publish_readiness_from_cached_contacts") {
        return;
    }
    let (f, p) = native(None, true).await;
    let _unconfirmed = prepare_selected_reset(&f).await;
    assert!(f.state().all_required_boards_seen());
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let cache = cached_fstate(&f);
    let (shutdown, receiver) = tokio::sync::broadcast::channel(2);
    let worker = crate::sequences::start_sequence_task(f.state(), receiver);
    until(Duration::from_secs(2), || {
        p.sequence_ticks.load(Ordering::Acquire) >= 3
    })
    .await;
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    let stop = f.state().manual_abort_intent.request();
    let after_stop = p.sequence_ticks.load(Ordering::Acquire);
    until(Duration::from_secs(2), || {
        p.sequence_ticks.load(Ordering::Acquire) >= after_stop + 3
    })
    .await;
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    let generation = crate::gse::automatic_generation(&f.state());
    crate::gse::reset_generation::close_operator_admission(&f.state(), generation, 77).unwrap();
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(!worker.is_finished());
    assert_eq!(f.state().manual_abort_intent.pending_epoch(), Some(stop));
    assert_eq!(
        f.state().local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(cached_fstate(&f), cache);
    assert!(p.received.lock().unwrap().is_empty());
    assert!(pending(&f).is_empty());
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
}

fn decoded_wire(
    p: &Probe,
    stage: &str,
    ty: sedsnet::config::DataType,
) -> Vec<(crate::recovery_pending_probe::WireObservation, Packet)> {
    let rows = p.wire.lock().unwrap().clone();
    rows.into_iter().filter_map(|row| {
        if row.stage != stage || row.board != Board::GatewayBoard { return None; }
        // These are actual native fixture SDK frames, not a radio/modem decoder.
        let envelope = sedsnet::wire_format::peek_envelope(&row.bytes).ok()?;
        if envelope.ty != ty { return None; }
        let packet = sedsnet::wire_format::unpack_packet(&row.bytes)
            .expect("matched native SDK wire frame must decode");
        Some((row, packet))
    }).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r16_single_public_get_wire_waits_for_gs_then_returns_original_observation() {
    crate::mode_suite!(TestFire);
    if separate("r16_single_public_get_wire_waits_for_gs_then_returns_original_observation") { return; }
    let (f, p) = native(None, false).await;
    r12_healthy_contacts(&f,&p).await;
    until(Duration::from_secs(3), || pending(&f).is_empty()).await;
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let request_ty = sedsnet::config::DataType::ManagedVariableRequest;
    let original = cached_fstate(&f).expect("actual healthy original cached observation");
    assert_eq!(original.payload(), &[1]);
    assert!(original.timestamp() > 0);
    assert!(!p.refresh.load(Ordering::Acquire), "single public request, no periodic API attempts");
    let peer = &f.native_peers.iter().find(|(board, _)| *board == Board::GatewayBoard).unwrap().1;
    assert_eq!(peer.sender().as_ref(), "GB");
    let release = Release(p.clone());
    p.hold_get.store(true, Ordering::Release);
    p.trace_wire.store(true, Ordering::Release);
    // The official SDK builds the request and chooses its own ID/raw clock stamp.
    peer.request_managed_variable(ty).unwrap();
    until(Duration::from_secs(2), || p.held_get_id.lock().unwrap().is_some()).await;
    let held = p.held_get_id.lock().unwrap().expect("actual held GET identity");
    let emitted = decoded_wire(&p, "peer_emit", request_ty);
    assert_eq!(emitted.len(), 1, "exactly one real GET reached the wire");
    let (request_wire, request) = &emitted[0];
    assert_eq!(request.packet_id(), held);
    assert_eq!(sedsnet::discovery::decode_managed_variable_request(request).unwrap(), ty);
    let request_source = sedsnet::wire_format::peek_envelope(&request_wire.bytes).unwrap().source_address;
    let state = f.state();
    let host = state.topology_router.get().unwrap();
    assert_eq!(host.resolve_address(request_source).expect("actual discovered request identity").hostname.as_ref(), "GB");
    assert!(decoded_wire(&p, "host_ingress_accepted", request_ty).is_empty(), "withheld original never entered GS SDK");
    assert!(decoded_wire(&p, "host_emit", ty).is_empty(), "no response before the original GET reaches GS");
    assert!(pending(&f).is_empty(), "no retained state retry can explain the response");
    p.hold_get.store(false, Ordering::Release);
    until(Duration::from_secs(3), || {
        decoded_wire(&p, "host_ingress_accepted", request_ty).iter().any(|(_, packet)| packet.packet_id() == held)
            && decoded_wire(&p, "host_emit", ty).iter().any(|(_, packet)| packet.packet_id() == original.packet_id())
            && decoded_wire(&p, "peer_ingress_accepted", ty).iter().any(|(_, packet)| packet.packet_id() == original.packet_id())
    }).await;
    let accepted = decoded_wire(&p, "host_ingress_accepted", request_ty);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].1.packet_id(), held);
    assert_eq!(accepted[0].1.timestamp(), request.timestamp(), "raw request-clock stamp unchanged");
    assert_eq!(accepted[0].0.bytes, request_wire.bytes, "same captured original frame released, no rebuilding");
    let responses = decoded_wire(&p, "host_emit", ty);
    assert_eq!(responses.len(), 1, "one processed GET returns one original observation");
    let (response_wire, response) = &responses[0];
    assert!(response_wire.sequence > request_wire.sequence);
    assert_eq!(response.packet_id(), original.packet_id());
    assert_eq!(response.timestamp(), original.timestamp(), "original value/source UTC, not request raw clock");
    assert_eq!(response.payload(), original.payload());
    let response_source = sedsnet::wire_format::peek_envelope(&response_wire.bytes).unwrap().source_address;
    assert_eq!(peer.resolve_address(response_source).expect("actual discovered writer identity").hostname.as_ref(), "GS");
    let received = decoded_wire(&p, "peer_ingress_accepted", ty);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].1.packet_id(), original.packet_id());
    assert_eq!(received[0].1.timestamp(), original.timestamp());
    assert!(received[0].0.sequence > response_wire.sequence);
    assert_eq!(p.wire_dropped.load(Ordering::Acquire), 0);
    assert_eq!(p.observations_dropped.load(Ordering::Acquire), 0);
    assert_eq!(p.entries_dropped.load(Ordering::Acquire), 0);
    assert!(!f.pump.is_finished() && !f.receiver.is_finished());
    assert_eq!(f.state().local_flight_state_snapshot(), crate::types::FlightState::Idle);
    assert!(pending(&f).is_empty());
    eprintln!("R16_ACTUAL_GET requestID={held:#x} rawRequestClock={} originalValueID={:#x} originalValueUTC={}; GS SDK processing/response only, no echoed request-ID or physical effect proof",
        request.timestamp(), original.packet_id(), original.timestamp());
    drop(release);
}


// A new coverage case, scoped to the exact non-HITL contract: DAQ is required
// and is last in the original FILL window. Thus the first actual Busy return
// proves all four initial contacts have been processed, leaving no optional
// first-window event that could publish after owner release. Existing HITL
// cases/bodies are untouched; their optional-DAQ contract is not claimed here.
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn r17_real_policy_busy_then_later_fresh_window_accepts_one_original() {
    crate::mode_suite!(TestFire);
    if separate("r17_real_policy_busy_then_later_fresh_window_accepts_one_original"){return;}
    let (f,p)=native(None,false).await;
    let state=f.state();
    assert!(state.board_required_for_progression(Board::DaqBoard),"exact last-required initial-window premise");
    let before=crate::network_variables::automatic_idle_test_snapshot();
    let epochs=(crate::gse::automatic_generation(&state),state.manual_abort_intent.epoch(),state.deferred_abort.manual_epoch());
    p.capture_entries.store(true,Ordering::Release);
    let mut owner=R12PolicyOwner::start(state.clone());
    let began=Instant::now();
    // Poll ONE original shared positive future. Its original 3s timeout is
    // created once, never refreshed by this external observational controller.
    // The next window waits for the owner release below, so the first Busy
    // is observed with exactly the four initial contacts processed.
    let next_window=std::sync::atomic::AtomicBool::new(false);
    let healthy=native_contact_prelude_gated(&f,&p,PreludeGoal::ReturnedAndActualGbReceipt,Some(&next_window));
    tokio::pin!(healthy);
    let mut busy_proof=None;
    let mut released_at=None;
    let result=loop {
        tokio::select! {
            biased;
            result=&mut healthy=>break result,
            _=tokio::time::sleep(Duration::from_millis(1))=>{
                if released_at.is_none() {
                    let outcomes=p.final_commit_counts();
                    let completed=p.automatic_completed.load(Ordering::Acquire);
                    if outcomes[1]>0 && completed {
                        // DAQ-last + its real Busy return fences the entire
                        // initial four-contact processing before owner release.
                        assert_eq!(outcomes,[0,1,0,0,0,0]);
                        assert!(!owner.released.load(Ordering::Acquire));
                        let entries=p.entries.lock().unwrap();
                        let unique=entries.iter().map(|r|r.0).collect::<std::collections::HashSet<_>>();
                        assert_eq!(entries.len(),4);
                        assert_eq!(unique.len(),4);
                        drop(entries);
                        assert_eq!(state.local_flight_state_snapshot(),crate::types::FlightState::Startup);
                        assert!(p.received.lock().unwrap().is_empty(),"no real accepted Idle receipt may be invented from an early returned flag");
                        busy_proof=Some(json!({"counts":outcomes,"automatic_completed":completed,
                            "initial_processed_unique_ids":unique,"elapsed_us":began.elapsed().as_micros()}));
                        owner.finish();
                        let released=began.elapsed();
                        assert!(owner.released.load(Ordering::Acquire));
                        // Only now may the next fresh window be sent: the real
                        // owner is released before any later contact arrives.
                        next_window.store(true,Ordering::Release);
                        released_at=Some(released);
                    } else {
                        // No later window is sent before the release, so the
                        // premise holds however slowly the host runs; only the
                        // original 3s shared positive bounds the wait.
                        assert!(began.elapsed()<Duration::from_secs(3),
                            "first real Busy/processed-window premise was not observed within the original 3s window");
                    }
                }
            }
        }
    };
    let accepted_elapsed=began.elapsed();
    result.expect("same original 3s shared positive must recover using fresh contacts");
    assert!(busy_proof.is_some() && released_at.is_some(),"an old returned flag/other receipt cannot prematurely complete the healthy goal");
    assert!(accepted_elapsed<Duration::from_secs(3));
    r12_until("later_window_original_FSTATE_owner_retirement",&f,&p,||pending(&f).is_empty()).await;
    assert_eq!(state.local_flight_state_snapshot(),crate::types::FlightState::Idle);
    assert_eq!(crate::network_variables::flight_state(),0);
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
    assert_eq!((crate::gse::automatic_generation(&state),state.manual_abort_intent.epoch(),state.deferred_abort.manual_epoch()),epochs);
    let counts=p.final_commit_counts();
    assert_eq!([counts[0],counts[1],counts[2],counts[3],counts[4]],[1,1,0,0,0],
        "one actual Busy followed by exactly one accepted original: {counts:?}");
    // A later fresh contact may meet the SDK core or schema momentarily held
    // elsewhere; that nonwaiting attempt changes nothing and the next fresh
    // contact qualifies. Any other SDK refusal still fails.
    assert_eq!(counts[5],p.sdk_contention_count(),"only transient SDK lock contention may refuse a later fresh contact: {counts:?}");
    assert_eq!(crate::recovery_pending_probe::final_outcome_observer_losses(),0);
    let cache=cached_fstate(&f).expect("actual later-window original cache");
    assert_eq!(cache.payload(),&[1]);
    assert!(cache.timestamp()>0);
    let rows=p.received.lock().unwrap();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r|r.id==cache.packet_id() && r.utc==cache.timestamp() && r.value==Some(1)));
    drop(rows);
    let entries=p.entries.lock().unwrap();
    let unique=entries.iter().map(|r|r.0).collect::<std::collections::HashSet<_>>();
    // The unchanged shared producer emits <=4 per window. More than four
    // ACTUAL unique dequeued inputs proves >=2 real windows, not a timer tick.
    assert!(unique.len()>4 && unique.len()<=48);
    assert_eq!(entries.len(),unique.len());
    let actual_inputs=unique.len();
    drop(entries);
    assert_eq!(p.entries_dropped.load(Ordering::Acquire),0);
    assert_eq!(p.observations_dropped.load(Ordering::Acquire),0);
    assert_eq!(p.wire_dropped.load(Ordering::Acquire),0);
    assert!(!f.pump.is_finished() && !f.receiver.is_finished());
    eprintln!("R17_LATER_WINDOW_PROOF {}",json!({"actual_first_busy":busy_proof,
        "released_elapsed_us":released_at.unwrap().as_micros(),
        "accepted_elapsed_us":accepted_elapsed.as_micros(),
        "final_commit_counts":p.final_commit_counts(),"actual_unique_dequeued_inputs":actual_inputs,
        "accepted_original_id":cache.packet_id(),"accepted_original_utc":cache.timestamp(),
        "source":"same shared helper/new fresh contacts; no state seeding, packet replay, queue clear or timeout renewal"}));
}
