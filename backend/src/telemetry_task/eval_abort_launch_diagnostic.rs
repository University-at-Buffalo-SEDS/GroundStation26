use axum::http::{Request, StatusCode};
use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};
use std::sync::{Condvar, atomic::AtomicU8};
use tower::ServiceExt;

struct ReleaseHeldCallback(Arc<(Mutex<bool>, Condvar)>);
impl Drop for ReleaseHeldCallback {
    fn drop(&mut self) {
        let (lock, cv) = &*self.0;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct ObservedControl {
    id: u64,
    ty: u32,
    payload: Vec<u8>,
    owners: Vec<u64>,
}

async fn wait_until(label: &str, ready: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout: {label}"));
}

async fn public_command_response(state: Arc<AppState>, command: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/command")
        .header("Authorization", "Bearer eval-launch-test")
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(command).unwrap()))
        .unwrap();
    let response = crate::web::router(state, String::new())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}
async fn public_command(state: Arc<AppState>, command: &str) -> StatusCode {
    let (status, body) = public_command_response(state, command).await;
    eprintln!("PUBLIC_COMMAND_RESPONSE: command={command} status={status} body={body}");
    status
}


// Real recovery endpoints supplement the existing instrumented command peers.
// Replies are queued outside endpoint callbacks, exactly as on the SDK wire.
struct ReadinessPeer {
    board: crate::types::Board,
    router: Arc<Router>,
    requests: Arc<Mutex<Vec<sedsnet::packet::Packet>>>,
}
fn pump_eval_readiness(host: &Router, peers: &[Arc<Router>], readbacks: &[ReadinessPeer]) {
    for _ in 0..12 {
        host.process_all_queues_with_timeout(0).unwrap();
        for peer in peers {
            peer.process_all_queues_with_timeout(0).unwrap();
        }
        for peer in readbacks {
            let requests = std::mem::take(&mut *peer.requests.lock().unwrap());
            for request in requests {
                peer.router.tx_queue(crate::ignition_pair::fixture::query_reply(peer.board, &request).unwrap()).unwrap();
            }
            peer.router.process_all_queues_with_timeout(0).unwrap();
        }
        host.process_all_queues_with_timeout(0).unwrap();
    }
}
async fn warm_eval_ignition_pair(state: &Arc<AppState>, host: &Arc<Router>, peers: &[Arc<Router>], readbacks: &[ReadinessPeer]) {
    host.announce_discovery().unwrap();
    for peer in peers {
        peer.announce_discovery().unwrap();
    }
    crate::ignition_pair::observer::start(state);
    for board in [crate::types::Board::ActuatorBoard, crate::types::Board::ValveBoard] {
        pump_eval_readiness(host, peers, readbacks);
        assert_eq!(crate::board_recovery::request_board_query(state, board).status(), StatusCode::OK, "actual instrumented peer must admit one fresh QUERY");
        let nonce = crate::board_recovery::query_nonce_for_test(state, board).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                pump_eval_readiness(host, peers, readbacks);
                if crate::board_recovery::query_reply_current_for_test(state, board, nonce) { break; }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await.expect("actual same-Router peer must return this fresh nonce");
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            pump_eval_readiness(host, peers, readbacks);
            if state.board_abort.ignition_pair_current() && crate::ignition_pair::require_routes(state).is_ok() { break; }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }).await.expect("actual queried AB/VB and observed command routes must be current");
}

async fn run_case(settle_before_launch: bool, hold_retry: bool) -> bool {
    crate::telemetry_schema::initialize().unwrap();
    let root = crate::recording_export::tests::TestDirectory::new();
    let (db_tx, db_rx) = mpsc::channel(64);
    let (mut state, cmd_rx) = test_app_state_with_cmd_rx(db_tx).await;
    crate::ensure_auth_sessions_table(&state.auth_db)
        .await
        .unwrap();
    let auth_path = root.0.join("auth.json");
    std::fs::write(&auth_path, serde_json::json!({
        "version":1,"anonymous":{"view_data":true,"send_commands":false},
        "users":[{"username":"eval-operator","password":{"iterations":120000,"salt_b64":"","hash_b64":""},
        "permissions":{"view_data":true,"send_commands":true},
        "command_access":{"allowed_commands":["EvaluationAbort","Launch"]}}]
    }).to_string()).unwrap();
    let journal = root.0.join("launch-journal.json");
    {
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.auth = Arc::new(AuthManager::new(auth_path));
        inner.launch_history = Arc::new(Mutex::new(
            crate::launch_history::Store::open(
                journal.clone(),
                false,
                None,
                get_current_timestamp_ms() as i64,
                0,
            )
            .unwrap(),
        ));
    }
    let utc = get_current_timestamp_ms() as i64;
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('eval-launch-test','eval-operator','session',1,1,?,?)")
        .bind(utc).bind(utc + 60_000).execute(&state.auth_db).await.unwrap();
    if state.hitl_button_interlock_enabled() {
        state.toggle_hitl_button_interlock();
    }
    if state.hitl_launch_interlock_enabled() {
        state.toggle_hitl_launch_interlock();
    }
    assert!(
        crate::checklists::launch_block().is_none(),
        "existing HITL checklist gate must permit the fixture"
    );

    let now = Arc::new(AtomicU64::new(1000));
    let clock = now.clone();
    // Keep the explicit fault steps below, while SDK retry/deferred service
    // continues to see real monotonic progress between those steps. A frozen
    // clock can leave a +1ms queue refusal permanently ineligible after the
    // first retained owner is observed but its IO callback has not finished.
    let clock_origin = std::time::Instant::now();
    let host = Arc::new(Router::new_with_clock(
        RouterConfig::new([crate::ignition_pair::fixture::handler(&state, |_| Ok(()))]).with_sender("GS"),
        Box::new(move || {
            clock.load(Ordering::SeqCst).saturating_add(
                u64::try_from(clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX),
            )
        }),
    ));
    state.topology_router.set(host.clone()).unwrap();
    let fc_type = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
    let valve_type = crate::telemetry_schema::data_type("VALVE_COMMAND");
    let fc_ep = crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER");
    let valve_ep = crate::telemetry_schema::endpoint("VALVE_BOARD");
    let actuator_ep = crate::telemetry_schema::endpoint("ACTUATOR_BOARD");
    let wire_controls = Arc::new(Mutex::new(Vec::<ObservedControl>::new()));
    let delivered = Arc::new(Mutex::new(Vec::<ObservedControl>::new()));
    //0=refuse EvalAbort,1=hold EvalAbort retry,2=deliver normally.
    let mode = Arc::new(AtomicU8::new(0));
    let entered = Arc::new(AtomicBool::new(false));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let _release_on_failure = ReleaseHeldCallback(gate.clone());
    let mut peers = Vec::new();
    let mut readiness_peers = Vec::new();
    for (name, ep) in [("FC", fc_ep), ("VB", valve_ep), ("AB", actuator_ep)] {
        let output = delivered.clone();
        let mut handlers = vec![EndpointHandler::new_packet_handler(ep, move |packet| {
                output.lock().unwrap().push(ObservedControl {
                    id: packet.packet_id(),
                    ty: packet.data_type().as_u32(),
                    payload: packet.payload().to_vec(),
                    owners: sedsnet::wire_format::peek_envelope(
                        &sedsnet::wire_format::pack_packet(packet),
                    )
                    .unwrap()
                    .target_senders
                    .to_vec(),
                });
                Ok(())
            })];
        let board = match name {
            "AB" => Some(crate::types::Board::ActuatorBoard),
            "VB" => Some(crate::types::Board::ValveBoard),
            _ => None,
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        if let Some(board) = board {
            let capture = requests.clone();
            let recovery = crate::telemetry_schema::endpoint(if board == crate::types::Board::ActuatorBoard { "BOARD_RECOVERY_AB" } else { "BOARD_RECOVERY_VB" });
            handlers.push(EndpointHandler::new_packet_handler(recovery, move |packet| {
                capture.lock().unwrap().push(packet.clone());
                Ok(())
            }));
        }
        let peer = Arc::new(Router::new(RouterConfig::new(handlers).with_sender(name)));
        if let Some(board) = board {
            readiness_peers.push(ReadinessPeer { board, router: peer.clone(), requests });
        }
        let host_side = Arc::new(OnceLock::new());
        let to_host = host_side.clone();
        let ground = Arc::downgrade(&host);
        let options = RouterSideOptions {
            reliable_enabled: false,
            link_local_enabled: true,
            ..Default::default()
        };
        let peer_side = peer.add_side_packed_with_options(
            "to-GS",
            move |bytes| {
                ground
                    .upgrade()
                    .unwrap()
                    .rx_packed_from_side(bytes, *to_host.get().unwrap())
            },
            options.clone(),
        );
        let output = wire_controls.clone();
        let receiver = peer.clone();
        let mode = mode.clone();
        let entered = entered.clone();
        let hold = gate.clone();
        let side = host.add_side_packed_with_options(
            name,
            move |bytes| {
                if let Ok(packet) = sedsnet::wire_format::unpack_packet(bytes) {
                    if packet.data_type() == fc_type || packet.data_type() == valve_type {
                        output.lock().unwrap().push(ObservedControl {
                            id: packet.packet_id(),
                            ty: packet.data_type().as_u32(),
                            payload: packet.payload().to_vec(),
                            owners: sedsnet::wire_format::peek_envelope(
                                &sedsnet::wire_format::pack_packet(&packet),
                            )
                            .unwrap()
                            .target_senders
                            .to_vec(),
                        });
                    }
                    if packet.data_type() == fc_type && packet.payload() == [16] {
                        if mode.load(Ordering::SeqCst) == 0 {
                            return Err(sedsnet::TelemetryError::Io("diagnostic FC16 refused"));
                        }
                        if mode.load(Ordering::SeqCst) == 1 {
                            entered.store(true, Ordering::SeqCst);
                            let (lock, cv) = &*hold;
                            let mut released = lock.lock().unwrap();
                            while !*released {
                                released = cv.wait(released).unwrap();
                            }
                        }
                    }
                }
                receiver.rx_packed_from_side(bytes, peer_side)
            },
            options,
        );
        host_side.set(side).unwrap();
        peer.announce_discovery().unwrap();
        peers.push(peer);
    }
    for _ in 0..12 {
        for peer in &peers {
            peer.process_all_queues_with_timeout(0).unwrap();
        }
        host.process_all_queues_with_timeout(0).unwrap();
    }
    // Test-only readiness premise: genuine AB plus the SAME instrumented VB.
    crate::telemetry_task::refresh_host_network_time(&host, get_current_timestamp_ms());
    assert!(!state.board_abort.ignition_pair_current());
    warm_eval_ignition_pair(&state, &host, &peers, &readiness_peers).await;
    //Use the current supported HITL state/interlocks. This is not FC ready evidence.
    *state.state.lock().unwrap() = FlightState::Armed;
    state.set_action_policy(default_action_policy());
    assert!(state.is_command_allowed(&TelemetryCommand::EvaluationAbort));
    assert!(state.is_command_allowed(&TelemetryCommand::Launch));
    assert!(
        !state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    let worker_state = state.clone();
    let worker_host = host.clone();
    let worker = tokio::spawn(async move {
        telemetry_task(
            worker_state.clone(),
            worker_host,
            Vec::new(),
            cmd_rx,
            db_rx,
            worker_state.shutdown_subscribe(),
        )
        .await
    });
    assert_eq!(
        public_command(state.clone(), "EvaluationAbort").await,
        StatusCode::OK
    );
    wait_until("actual SDK retained FC16 owner", || {
        let controls = wire_controls.lock().unwrap();
        controls.iter().any(|row| {
            row.ty == fc_type.as_u32()
                && row.payload == [16]
                && host
                    .export_reliable_pending_stats()
                    .iter()
                    .any(|owner| owner.packet_id == row.id && owner.data_type == fc_type)
        })
    })
    .await;
    let stop = wire_controls
        .lock()
        .unwrap()
        .iter()
        .find(|row| row.ty == fc_type.as_u32() && row.payload == [16])
        .unwrap()
        .clone();
    let fc_owner = sedsnet::packet::hash_bytes_u64(0x517C_C1B7_2722_0A95, b"FC");
    let retained = host.export_reliable_pending_stats();
    assert!(retained.iter().any(|row| row.packet_id == stop.id
        && row.data_type == fc_type
        && row.pending_destinations == vec![fc_owner]));
    assert_eq!(stop.owners, vec![fc_owner]);
    assert!(state.manual_abort_intent.pending_epoch().is_none());
    assert!(!crate::gse::manual_stop_pending(&state));
    if settle_before_launch {
        mode.store(2, Ordering::SeqCst);
        now.store(2000, Ordering::SeqCst);
        for _ in 0..24 {
            let _ = host.periodic(0);
            let _ = host.process_all_queues_with_timeout(0);
            for peer in &peers {
                peer.process_all_queues_with_timeout(0).unwrap();
            }
        }
        assert!(
            host.export_reliable_pending_stats()
                .iter()
                .all(|row| row.packet_id != stop.id)
        );
        assert!(
            delivered
                .lock()
                .unwrap()
                .iter()
                .any(|row| row.id == stop.id)
        );
    }
    let retry = if hold_retry {
        mode.store(1, Ordering::SeqCst);
        now.store(2000, Ordering::SeqCst);
        let held = host.clone();
        let task = std::thread::spawn(move || {
            let _ = held.periodic(0);
            let _ = held.process_all_queues_with_timeout(0);
        });
        wait_until("actual SDK held FC16 retry callback", || {
            entered.load(Ordering::SeqCst)
        })
        .await;
        Some(task)
    } else {
        None
    };
    let before = host.export_reliable_pending_stats();
    let response = public_command(state.clone(), "Launch").await;
    assert_eq!(
        response,
        StatusCode::OK,
        "actual public ingress must admit the fixture request before worker policy"
    );
    wait_until("actual worker consumed Launch", || {
        state.last_command_ms.lock().unwrap().contains_key("Launch")
    })
    .await;
    //Wait for either durable launch admission or the existing worker rejection notification.
    wait_until("launch decision", || {
        state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
            || state.notifications.lock().unwrap().iter().any(|n| {
                n.message.contains("launch was not dispatched")
                    || n.message.contains("Launch already admitted")
            })
    })
    .await;
    let admitted = state
        .launch_history
        .lock()
        .unwrap()
        .automatic_actions_inhibited();
    if admitted {
        wait_until("real valve Sequence frame", || {
            wire_controls
                .lock()
                .unwrap()
                .iter()
                .any(|row| row.ty == valve_type.as_u32() && row.payload == [6])
        })
        .await;
    }
    let snapshot = host.export_reliable_pending_stats();
    let journal_json: serde_json::Value = std::fs::read(&journal)
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        .unwrap_or(serde_json::Value::Null);
    if admitted {
        assert_eq!(journal_json["automatic_actions_inhibited"], true);
    }
    let controls_at_decision = wire_controls.lock().unwrap().clone();
    if !settle_before_launch {
        assert!(
            snapshot.iter().any(|row| row.packet_id == stop.id
                && row.data_type == fc_type
                && row.pending_destinations == vec![fc_owner]),
            "exact unretired FC16 owner must still exist at launch decision"
        );
    }
    if !settle_before_launch {
        assert!(
            !admitted,
            "pending FC16 must deny before journal inhibition"
        );
        assert_ne!(journal_json["automatic_actions_inhibited"], true);
        assert!(
            snapshot
                .iter()
                .filter(|row| row.data_type == fc_type || row.data_type == valve_type)
                .all(|row| row.packet_id == stop.id),
            "no new FCLaunch or VBSequence reliable owner may be admitted"
        );
        assert!(
            !controls_at_decision
                .iter()
                .any(|row| (row.ty == fc_type.as_u32() && row.payload == [1])
                    || (row.ty == valve_type.as_u32() && row.payload == [6])),
            "no launch wire may precede FC16 receipt"
        );
    }
    let delivered_at_decision = delivered.lock().unwrap().clone();
    macro_rules! evidence_rows { ($rows:expr) => { $rows.iter().map(|row| serde_json::json!({"packet_id":row.packet_id,"type":row.data_type.as_u32(),"owners":row.pending_destinations,"retries":row.retries})).collect::<Vec<_>>() }; }
    let proof = serde_json::json!({"settled_before_launch":settle_before_launch,"held_retry":hold_retry,"fc16":stop,"owners_before":evidence_rows!(before),"http_status":response.as_u16(),"journal_inhibited":admitted,"journal":journal_json,"owners_at_decision":evidence_rows!(snapshot),"wire_at_decision":controls_at_decision,"synthetic_peer_delivered_at_decision":delivered_at_decision,"manual_epoch":state.manual_abort_intent.epoch(),"manual_pending":state.manual_abort_intent.pending_epoch(),"scope":"actualGSpublichandler+actor+SDK; syntheticFC/VBendpointreceipts, no firmware/output/hardwareclaim"});
    if let Some(path) = std::env::var_os("GS_EVAL_LAUNCH_PROOF") {
        std::fs::write(path, serde_json::to_vec_pretty(&proof).unwrap()).unwrap();
    }

    eprintln!(
        "EVAL16_LAUNCH_CAUSAL: settled={settle_before_launch} held_retry={hold_retry} fc16={stop:?} owners_before={before:?} http={response} admitted={admitted} journal={journal_json} owners_at_decision={snapshot:?} wire_at_decision={controls_at_decision:?}"
    );
    mode.store(2, Ordering::SeqCst);
    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    if let Some(task) = retry {
        task.join().unwrap();
    }
    now.store(4000, Ordering::SeqCst);
    for _ in 0..24 {
        let _ = host.periodic(0);
        let _ = host.process_all_queues_with_timeout(0);
        for peer in &peers {
            peer.process_all_queues_with_timeout(0).unwrap();
        }
    }
    if !settle_before_launch {
        // The real router worker may still own its nonblocking processing
        // turn after the foreground pump. Let that worker and its exact ACK
        // finish at the SAME virtual time, under the existing bounded wait.
        // No retry deadline or launch-admission assertion is relaxed here.
        wait_until("exact FC16 receipt after retry", || {
            let _ = host.process_all_queues_with_timeout(0);
            for peer in &peers {
                peer.process_all_queues_with_timeout(0).unwrap();
            }
            host.export_reliable_pending_stats().iter().all(|row| row.packet_id != stop.id)
        }).await;
        assert!(
            host.export_reliable_pending_stats()
                .iter()
                .all(|row| row.packet_id != stop.id),
            "retry requires exact FC16 owner retired by receipt"
        );
        assert_eq!(
            delivered
                .lock()
                .unwrap()
                .iter()
                .filter(|row| row.id == stop.id)
                .count(),
            1,
            "actual SDK peer must accept FC16 exactly once"
        );
        eprintln!(
            "EVAL16_RETRY_QUIESCENCE_BEFORE: fc_pending={} counters={:?} owners={:?} state={:?} clock={:?} allowed={} notifications={:?}",
            host.has_pending_reliable_tx_type(fc_type),
            host.queue_counters(),
            host.export_reliable_pending_stats(),
            state.local_flight_state_snapshot(),
            state.launch_clock_snapshot(),
            state.is_command_allowed(&TelemetryCommand::Launch),
            state.notifications_snapshot()
        );
        wait_until(
            "exact FC16 accepted type fully quiescent after receipt",
            || !host.has_pending_reliable_tx_type(fc_type),
        )
        .await;
        eprintln!(
            "EVAL16_RETRY_QUIESCENCE_AFTER: fc_pending={} counters={:?}",
            host.has_pending_reliable_tx_type(fc_type),
            host.queue_counters()
        );
        // Explicit new operator retry follows a fresh real readiness check;
        // no queries run inside the held-FC16 admission decision above.
        warm_eval_ignition_pair(&state, &host, &peers, &readiness_peers).await;
        let retry_start = std::time::Instant::now();
        let mut public_duplicates = 0usize;
        let retry_body = loop {
            let (status, body) = public_command_response(state.clone(), "Launch").await;
            assert_eq!(status, StatusCode::OK);
            if body != "duplicate ignored" {
                break body;
            }
            public_duplicates += 1;
            assert!(
                retry_start.elapsed() < Duration::from_secs(8),
                "existing public debounce must eventually admit a fresh retry"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        eprintln!(
            "PUBLIC_FRESH_RETRY_RESPONSE: duplicates_ignored={public_duplicates} fresh_body={retry_body}"
        );
        let retry_result = tokio::time::timeout(Duration::from_secs(8), async {
            while !state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        if retry_result.is_err() {
            eprintln!(
                "EVAL16_RETRY_FAILURE_STATE: fc_pending={} counters={:?} owners={:?} state={:?} clock={:?} allowed={} notifications={:?}",
                host.has_pending_reliable_tx_type(fc_type),
                host.queue_counters(),
                host.export_reliable_pending_stats(),
                state.local_flight_state_snapshot(),
                state.launch_clock_snapshot(),
                state.is_command_allowed(&TelemetryCommand::Launch),
                state.notifications_snapshot()
            );
        }
        assert!(
            retry_result.is_ok(),
            "fresh retry must be admitted after exact FC16 receipt and accepted-type quiescence"
        );
        wait_until("fresh retry admitted after exact FC16 receipt", || {
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        })
        .await;
        let wire_wait = std::panic::AssertUnwindSafe(wait_until(
            "retry FCLaunch and VBSequence wire owners", || {
                let rows = wire_controls.lock().unwrap();
                rows.iter()
                    .any(|row| row.ty == fc_type.as_u32() && row.payload == [1])
                    && rows
                        .iter()
                        .any(|row| row.ty == valve_type.as_u32() && row.payload == [6])
            },
        ));
        if let Err(panic) = futures::FutureExt::catch_unwind(wire_wait).await {
            let wire = wire_controls.lock().unwrap().clone();
            let received = delivered.lock().unwrap().clone();
            let journal_at_failure = std::fs::read(&journal)
                .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).unwrap())
                .unwrap_or(serde_json::Value::Null);
            eprintln!(
                "EVAL16_WIRE_TIMEOUT: network_utc={:?} wall_utc={} explicit_monotonic_offset={} elapsed_ms={} fc_pending={} vb_pending={} queue_counters={:?} reliable={:?} wire={:?} delivered={:?} state={:?} clock={:?} allowed={} notifications={:?} journal={}",
                host.network_time().and_then(|time| time.unix_time_ms),
                get_current_timestamp_ms(),
                now.load(Ordering::SeqCst),
                clock_origin.elapsed().as_millis(),
                host.has_pending_reliable_tx_type(fc_type),
                host.has_pending_reliable_tx_type(valve_type),
                host.queue_counters(),
                host.export_reliable_pending_stats(),
                wire, received,
                state.local_flight_state_snapshot(),
                state.launch_clock_snapshot(),
                state.is_command_allowed(&TelemetryCommand::Launch),
                state.notifications_snapshot(),
                journal_at_failure,
            );
            eprintln!("EVAL16_WIRE_RUNTIME_STATS: {:?}", host.export_runtime_stats());
            // Preserve the identical original timeout and panic. Diagnostics
            // cannot create a retry, rebase intent or turn failure into success.
            std::panic::resume_unwind(panic);
        }
        let retry_journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        assert_eq!(retry_journal["automatic_actions_inhibited"], true);
        let retry_proof = serde_json::json!({"fc16_id":stop.id,"exact_fc16_owner_retired":true,"synthetic_fc16_receipt_count":1,"retry_http_status":200,"public_duplicate_responses_ignored":public_duplicates,"retry_response_body":retry_body,"retry_journal":retry_journal,"retry_wire":*wire_controls.lock().unwrap(),"retry_owners":evidence_rows!(host.export_reliable_pending_stats())});
        if let Some(path) = std::env::var_os("GS_EVAL_LAUNCH_PROOF") {
            let mut retry_path = path;
            retry_path.push(".retry.json");
            std::fs::write(retry_path, serde_json::to_vec_pretty(&retry_proof).unwrap()).unwrap();
        }
        eprintln!("EVAL16_SETTLED_RETRY_CONTROL: {retry_proof}");
    }
    eprintln!(
        "EVAL16_LAUNCH_FINAL_CONTROLS: wire={:?} delivered={:?} owners={:?}",
        *wire_controls.lock().unwrap(),
        *delivered.lock().unwrap(),
        host.export_reliable_pending_stats()
    );
    if admitted || !settle_before_launch {
        let controls = wire_controls.lock().unwrap();
        assert!(
            controls.iter().any(|row| row.ty == fc_type.as_u32()
                && row.payload == [1]
                && row.owners == vec![fc_owner]),
            "actual previously admitted FC Launch packet must eventually dispatch with exact owner"
        );
        let vb_owner = sedsnet::packet::hash_bytes_u64(0x517C_C1B7_2722_0A95, b"VB");
        assert!(controls.iter().any(|row| row.ty == valve_type.as_u32()
            && row.payload == [6]
            && row.owners == vec![vb_owner]));
    }
    assert!(
        !crate::gse::automatic_abort_allowed(&state),
        "Launch must keep automatic Abort inhibited"
    );
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(8), worker)
        .await
        .unwrap()
        .unwrap();
    if settle_before_launch {
        assert!(
            admitted,
            "settled FC control must pass existing public Launch path"
        );
    }
    admitted
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eval_abort_retained_owner_must_block_public_fresh_launch() {
    // A non-HITL test inside the HITL-only module: no build compiled it
    // before modes moved to run time, so no mode runs it.
    if !crate::runtime_mode::test_support::suite(&[]) {
        return;
    }
    assert!(
        !run_case(false, false).await,
        "retained exact FC16 owner did not block public Launch journal admission"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eval_abort_held_retry_must_block_public_fresh_launch() {
    // A non-HITL test inside the HITL-only module: no build compiled it
    // before modes moved to run time, so no mode runs it.
    if !crate::runtime_mode::test_support::suite(&[]) {
        return;
    }
    assert!(
        !run_case(false, true).await,
        "held exact FC16 retry did not block public Launch journal admission"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eval_abort_settled_owner_public_fresh_launch_control() {
    crate::mode_suite!(Hitl);
    assert!(run_case(true, false).await);
}
