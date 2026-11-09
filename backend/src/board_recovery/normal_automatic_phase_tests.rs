// NONSHIPPING Normal causal oracles. Real HTTP/SDK/coordinator owners; native
// encoded board readbacks remain models, not MCU NVM or hardware qualification.
use super::*;
use crate::normal_publication_probe::{Pause as CausalPause, Site};
use crate::types::FlightState;
use tower::ServiceExt;

fn separate(name: &str) -> bool {
    crate::operator_queue::isolated_case(module_path!(), name)
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("causal premise not reached; not an SDK/runtime RED");
}
// All stale-owner/refusal assertions precede this helper. It supplies the
// actual periodic Normal owner deliberately absent after the old worker joined.
// No direct policy call, model proof refresh, state assignment or old-intent retry.
struct AbortWorkerOnDrop(tokio::task::AbortHandle);
impl Drop for AbortWorkerOnDrop {
    fn drop(&mut self) { self.0.abort(); }
}
async fn healthy_current_service(state: &Arc<AppState>) {
    let generation = crate::gse::automatic_generation(state);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
    let canonical_before = crate::network_variables::automatic_idle_test_snapshot();
    let already_idle = state.local_flight_state_snapshot() == FlightState::Idle;
    let mut ready_entries = 0;
    let mut worker_abort_requested = false;
    if !already_idle {
        let pause = CausalPause::install(state, Site::SafetyReady);
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let mut worker = tokio::spawn(crate::safety_task::safety_task(
            state.clone(), state.topology_router.get().unwrap().clone(), rx,
        ));
        let _worker_scope = AbortWorkerOnDrop(worker.abort_handle());
        let progress = tokio::time::timeout(Duration::from_secs(3), async {
            pause.entered().await;
            assert_eq!(crate::gse::automatic_generation(state), generation);
            pause.release();
            while state.local_flight_state_snapshot() != FlightState::Idle {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await;
        // Release and join even if the bounded current-owner premise failed.
        pause.release();
        let _ = shutdown.send(());
        // Graceful shutdown and any abort confirmation share ONE original
        // 2s deadline; never detach a consumed JoinHandle on timeout.
        let join_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        match tokio::time::timeout_at(join_deadline - Duration::from_secs(1), &mut worker).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                worker_abort_requested = true;
                worker.abort();
                match tokio::time::timeout_at(join_deadline, &mut worker).await {
                    Ok(Ok(())) => (),
                    Ok(Err(error)) if error.is_cancelled() => (),
                    Ok(Err(error)) => panic!("actual current Safety worker panicked during cleanup: {error}"),
                    Err(error) => panic!("current Safety worker unreaped within original total 2s shutdown budget: {error}"),
                }
            }
        }
        ready_entries = pause.calls();
        assert!(progress.is_ok(), "actual current Safety owner did not reach healthy Idle within existing 3s bound");
        assert!(ready_entries >= 1, "only actual observed Safety-ready entries are credited");
    }
    let after = crate::network_variables::automatic_idle_test_snapshot();
    assert!(after.durable_unchanged(&canonical_before));
    let identity = current_idle_identity(state, generation);
    eprintln!("NORMAL_HEALTHY_SERVICE {}", json!({"already_idle":already_idle,"worker_spawned":!already_idle,
        "observed_ready_entries":ready_entries,"worker_abort_requested":worker_abort_requested,"worker_reaped":!already_idle,"generation":generation,
        "accepted_id":identity.original.packet_id.to_string(),"original_utc":identity.original.timestamp.to_string()}));
}

fn current_idle_identity(state: &Arc<AppState>, generation: u64) -> crate::network_variables::AutomaticPhaseIdentity {
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    let identity = crate::network_variables::automatic_idle_test_snapshot().live_phase_identity().expect("Idle has actual automatic accepted-owner provenance");
    assert_eq!(identity.phase, FlightState::Idle as u8);
    assert_eq!(identity.original.generation, generation);
    assert_eq!(crate::gse::automatic_generation(state), generation);
    let packet = state.topology_router.get().unwrap()
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap().unwrap();
    assert_eq!((packet.packet_id(), packet.timestamp(), packet.payload()),
        (identity.original.packet_id, identity.original.timestamp, &[FlightState::Idle as u8][..]));
    assert_eq!(crate::network_variables::flight_state(), 0);
    identity
}

async fn normal() -> Fixture {
    // Same initialized desired store as actual main, not a private SDK source.
    let _ = crate::network_variables::flight_state();
    let fixture = Fixture::new().await;
    // Configure the existing encoded board model, then obtain contacts and
    // readiness through actual correlated public SDK queries, not host flags.
    for (_, model) in &fixture.models {
        model.lock().unwrap().state = 0;
    }
    for board in BOARDS {
        fixture.query(board).await;
    }
    let state = fixture.state();
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert!(state.all_required_boards_seen());
    assert!(!crate::gse::claimed(&state));
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
    fixture
}
async fn actual_stop(state: Arc<AppState>) {
    let epoch = state.manual_abort_intent.epoch();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/command")
        .header("content-type", "application/json")
        .header("authorization", "Bearer operator")
        .body(axum::body::Body::from("\"Abort\""))
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        crate::web::router(state.clone(), String::new()).oneshot(request),
    )
    .await
    .expect("original actual HTTP Stop response budget")
    .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(state.manual_abort_intent.epoch() > epoch);
    assert!(state.manual_abort_intent.pending_epoch().is_some());
}
fn facts(state: &Arc<AppState>, label: &str) -> Value {
    let router = state.topology_router.get().unwrap();
    let cached = router
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap();
    json!({"label":label,"state":state.local_flight_state_snapshot() as u8,
        "generation":crate::gse::automatic_generation(state),
        "barrier":state.operator_admission_barrier.load(Ordering::Acquire),
        "inflight":state.operator_dispatch_inflight.load(Ordering::Acquire),
        "manual_epoch":state.manual_abort_intent.epoch(),"manual_pending":state.manual_abort_intent.pending_epoch(),
        "observed_manual_epoch":state.deferred_abort.manual_epoch(),
        "abort_indicator":state.abort_indicator_latched.load(Ordering::Acquire),
        "canonical_flight_state":crate::network_variables::flight_state(),
        "managed":cached.map(|p|json!({"id":p.packet_id().to_string(),"utc":p.timestamp().to_string(),"payload":p.payload()}))})
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n1_actual_http_stop_then_policy_refresh_must_not_publish_idle() {
    crate::mode_suite!(Normal, Testing);
    if separate("n1_actual_http_stop_then_policy_refresh_must_not_publish_idle") {
        return;
    }
    let fixture = normal().await;
    let state = fixture.state();
    actual_stop(state.clone()).await;
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let old = facts(&state, "after_actual_http_stop");
    crate::sequences::refresh_action_policy_now(&state);
    let after = facts(&state, "after_actual_policy_refresh");
    println!("NORMAL_CAUSAL {}", json!({"before":old,"after":after}));
    assert_eq!(
        state.local_flight_state_snapshot(),
        FlightState::Startup,
        "retained Stop forbids new implicit Idle; does not assert physical actuation"
    );
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n2_healthy_normal_policy_keeps_live_idle_and_canonical_startup() {
    crate::mode_suite!(Normal, Testing);
    if separate("n2_healthy_normal_policy_keeps_live_idle_and_canonical_startup") {
        return;
    }
    let fixture = normal().await;
    let state = fixture.state();
    assert!(state.manual_abort_intent.pending_epoch().is_none());
    let before = crate::network_variables::automatic_idle_test_snapshot();
    crate::sequences::refresh_action_policy_now(&state);
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
    println!("NORMAL_CAUSAL {}", facts(&state, "healthy_normal"));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    assert_eq!(
        crate::network_variables::flight_state(),
        FlightState::Startup as u8
    );
    assert_eq!(
        state
            .topology_router
            .get()
            .unwrap()
            .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap()
            .unwrap()
            .payload(),
        &[1]
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n3_actual_counted_safety_blocks_reset_then_old_work_cannot_publish_while_closed() {
    crate::mode_suite!(Normal, Testing);
    if separate("n3_actual_counted_safety_blocks_reset_then_old_work_cannot_publish_while_closed") {
        return;
    }
    let fixture = normal().await;
    let state = fixture.state();
    let generation = crate::gse::automatic_generation(&state);
    let pause = CausalPause::install(&state, Site::SafetyReady);
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let worker = tokio::spawn(crate::safety_task::safety_task(
        state.clone(),
        state.topology_router.get().unwrap().clone(),
        rx,
    ));
    pause.entered().await;
    assert_eq!(
        state.operator_dispatch_inflight.load(Ordering::Acquire),
        1,
        "new actual Safety owner, never dropped to manufacture old milestone"
    );
    let final_gate = PauseGuard::install(&FINAL, &state);
    let ticket = fixture.prepare("reset_startup", None).await;
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    until(|| state.operator_admission_barrier.load(Ordering::Acquire) != 0).await;
    assert_eq!(crate::gse::automatic_generation(&state), generation);
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .is_none()
    );
    let mut publications = state.state_tx.subscribe();
    shutdown.send(()).unwrap();
    pause.release();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pause.calls(), 1);
    assert_eq!(pause.publication_refusal(),Some(((generation,0,0),crate::normal_publication_probe::ClaimRefusal::AdmissionClosed)),"actual old publication rejected at original closed-admission decision");
    final_gate.entered().await;
    let mut old_idle = false;
    loop {
        match publications.try_recv() {
            Ok(value) => {
                if value.state == FlightState::Idle {
                    old_idle = true;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(error) => panic!("state evidence lost/closed before original oracle: {error:?}"),
        }
    }
    println!(
        "NORMAL_SUCCESSOR {}",
        json!({"closed":facts(&state,"after_counted_safety_retired_before_actual_finalize"),"old_idle_publication":old_idle})
    );
    final_gate.release();
    let terminal = fixture.terminal().await;
    assert_eq!(terminal["success"], true, "{terminal}");
    assert!(
        !old_idle,
        "old Safety must retire while admissionclosed, not publish then be overwritten by Startup"
    );
    drop(pause);
    healthy_current_service(&state).await;
    assert_eq!(
        state.local_flight_state_snapshot(),
        FlightState::Idle,
        "fresh legitimate policy after actual ACK/activation"
    );
    assert_eq!(
        crate::network_variables::flight_state(),
        0,
        "automatic current Idle is NOT canonical restart authority"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n4_counted_tick_blocks_actual_reset_epoch_until_it_retires() {
    crate::mode_suite!(Normal, Testing);
    if separate("n4_counted_tick_blocks_actual_reset_epoch_until_it_retires") {
        return;
    }
    let fixture = normal().await;
    let state = fixture.state();
    let old_generation = crate::gse::automatic_generation(&state);
    let pause = CausalPause::install(&state, Site::CountedTick);
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let worker = crate::sequences::start_sequence_task(state.clone(), rx);
    pause.entered().await;
    assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
    let ticket = fixture.prepare("reset_startup", None).await;
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    until(|| state.operator_admission_barrier.load(Ordering::Acquire) != 0).await;
    assert_eq!(crate::gse::automatic_generation(&state), old_generation);
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .is_none(),
        "no durable new floor before actual counted work retires"
    );
    shutdown.send(()).unwrap();
    pause.release();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pause.calls(),
        1,
        "no fresh sequence tick may explain this observation"
    );
    let terminal = fixture.terminal().await;
    println!(
        "NORMAL_CAUSAL {}",
        json!({"terminal":terminal,"current":facts(&state,"counted_tick_drained")})
    );
    assert_eq!(terminal["success"], true, "{terminal}");
    assert!(crate::gse::automatic_generation(&state) > old_generation);
    drop(pause);
    healthy_current_service(&state).await;
    assert_eq!(
        state.local_flight_state_snapshot(),
        FlightState::Idle,
        "legitimate Normal reset/policy progression preserved"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n5_original_ticket_before_claim_is_rejected_after_actual_completed_reset() {
    crate::mode_suite!(Normal, Testing);
    if separate("n5_original_ticket_before_claim_is_rejected_after_actual_completed_reset") {
        return;
    }
    let fixture = normal().await;
    let state = fixture.state();
    let generation = crate::gse::automatic_generation(&state);
    let pause = crate::normal_publication_probe::TicketPause::install(&state);
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let worker = tokio::spawn(crate::safety_task::safety_task(
        state.clone(),
        state.topology_router.get().unwrap().clone(),
        rx,
    ));
    let original = pause.entered().await;
    assert_eq!(original, (generation, 0, 0));
    assert_eq!(
        state.operator_dispatch_inflight.load(Ordering::Acquire),
        0,
        "actual BEFOREclaim stage; never drop an acquired lease"
    );
    let ticket = fixture.prepare("reset_startup", None).await;
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    let terminal = fixture.terminal().await;
    assert_eq!(terminal["success"], true, "{terminal}");
    assert!(crate::gse::automatic_generation(&state) > generation);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
    // Preserve the actual completed reset's postimage BEFORE old work resumes.
    // GenerationChanged below is the authority proof; no Startup/Idle is fabricated.
    let post_reset_state = state.local_flight_state_snapshot();
    assert!(matches!(post_reset_state, FlightState::Startup | FlightState::Idle), "actual completed ResetStartup baseline cannot accept another phase");
    assert_eq!(crate::network_variables::flight_state(), 0);
    if post_reset_state == FlightState::Idle {
        current_idle_identity(&state, crate::gse::automatic_generation(&state));
    }
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let packet_before_old_release = state
        .topology_router
        .get()
        .unwrap()
        .get_cached_network_variable(ty)
        .unwrap()
        .unwrap();
    let id = packet_before_old_release.packet_id();
    shutdown.send(()).unwrap();
    pause.release();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pause.refusal(),Some(crate::normal_publication_probe::ClaimRefusal::GenerationChanged),"actual original claim rejection is epoch mismatch, not a fabricated dropped owner");
    assert_eq!(state.local_flight_state_snapshot(), post_reset_state, "old rejected original cannot change the actual completed-reset postimage");
    assert_eq!(
        state
            .topology_router
            .get()
            .unwrap()
            .get_cached_network_variable(ty)
            .unwrap()
            .unwrap()
            .packet_id(),
        id
    );
    let packet_after_old_release = state.topology_router.get().unwrap()
        .get_cached_network_variable(ty).unwrap().unwrap();
    assert_eq!(packet_after_old_release.timestamp(), packet_before_old_release.timestamp());
    assert_eq!(packet_after_old_release.payload(), packet_before_old_release.payload());
    assert_eq!(packet_after_old_release.sender(), packet_before_old_release.sender());
    assert_eq!(packet_after_old_release.endpoints(), packet_before_old_release.endpoints());
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    println!(
        "NORMAL_SUCCESSOR {}",
        json!({"captured_before_claim":original,"completed_reset":terminal,"after_old_ticket_release":facts(&state,"original_ticket_retired")})
    );
    drop(pause);
    healthy_current_service(&state).await;
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    assert_eq!(
        state.local_flight_state_snapshot(),
        FlightState::Idle,
        "current acknowledged Normal progression is preserved; no fabricated StartupABA"
    );
    assert_eq!(crate::network_variables::flight_state(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n6_healthy_actual_safety_worker_keeps_live_idle_and_canonical_startup() {
    crate::mode_suite!(Normal, Testing);
    if separate("n6_healthy_actual_safety_worker_keeps_live_idle_and_canonical_startup") {
        return;
    }
    let fixture = normal().await;
    let state = fixture.state();
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let n6_outcomes = crate::normal_publication_probe::N6Outcomes::install(&state);
    let pause = CausalPause::install(&state, Site::SafetyReady);
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let mut worker = tokio::spawn(crate::safety_task::safety_task(
        state.clone(),
        state.topology_router.get().unwrap().clone(),
        rx,
    ));
    let _worker_scope = AbortWorkerOnDrop(worker.abort_handle());
    pause.entered().await;
    // Same original 2s post-release phase covers BOTH actual periodic progress
    // and shutdown/join. Later ticks capture new owners; no old packet retry.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    pause.release();
    let progress = tokio::time::timeout_at(deadline, async {
        while state.local_flight_state_snapshot() != FlightState::Idle {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        shutdown.send(()).unwrap();
        (&mut worker).await
    }).await;
    match progress {
        Ok(result) => result.unwrap(),
        Err(error) => {
            // Cleanup grants no extra successful work deadline. Always FAIL,
            // even if a late commit races with abort/reaping after expiration.
            let _ = shutdown.send(());
            worker.abort();
            let cleanup = tokio::time::timeout(Duration::from_secs(1), &mut worker).await;
            assert!(cleanup.is_ok(), "expired N6 worker not reaped in cleanup-only 1s");
            panic!("actual healthy periodic service exceeded original total 2s phase: {error}");
        }
    }
    eprintln!("NORMAL_N6_EXISTING_BRANCH_OUTCOMES {}", n6_outcomes.snapshot());
    assert!(pause.calls() >= 1, "credit only actual observed Safety-ready entries");
    let _identity = current_idle_identity(&state, crate::gse::automatic_generation(&state));
    println!(
        "NORMAL_CAUSAL {}",
        facts(&state, "healthy_actual_safety_worker")
    );
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    assert_eq!(crate::network_variables::flight_state(), 0);
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
    assert_eq!(
        state
            .topology_router
            .get()
            .unwrap()
            .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap()
            .unwrap()
            .payload(),
        &[1]
    );
}

// Own a REAL GSE mutex on a separate bounded thread; no async-held guard or
// synthetic Busy flag. Release/join is explicit and also runs on test failure.
struct N6BusyGseOwner {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl N6BusyGseOwner {
    fn start(state: Arc<AppState>) -> Self {
        let (release, released) = std::sync::mpsc::channel();
        let (held, observed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _guard = state.gse.lock().unwrap();
            held.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(3));
        });
        let mut owner = Self { release: Some(release), thread: Some(thread) };
        if let Err(error) = observed.recv_timeout(Duration::from_secs(2)) {
            owner.finish();
            panic!("real GSE owner did not establish Busy premise: {error}");
        }
        owner
    }
    fn finish(&mut self) {
        if let Some(release) = self.release.take() { let _ = release.send(()); }
        if let Some(thread) = self.thread.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(thread.is_finished(), "real GSE owner not reaped in cleanup-only 1s");
            thread.join().unwrap();
        }
    }
}
impl Drop for N6BusyGseOwner {
    fn drop(&mut self) { self.finish(); }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n6b_one_actual_safety_tick_refuses_busy_owner_without_partial_idle() {
    crate::mode_suite!(Normal, Testing);
    if separate("n6b_one_actual_safety_tick_refuses_busy_owner_without_partial_idle") { return; }
    let fixture = normal().await;
    let state = fixture.state();
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let generation = crate::gse::automatic_generation(&state);
    let epochs = (state.manual_abort_intent.epoch(), state.deferred_abort.manual_epoch());
    let cached = || state.topology_router.get().unwrap()
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap().map(|p| (p.packet_id(), p.timestamp(), p.sender().to_owned(), p.payload().to_vec()));
    let cached_before = cached();
    let outcomes = crate::normal_publication_probe::N6Outcomes::install(&state);
    let pause = CausalPause::install(&state, Site::SafetyReady);
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let mut worker = tokio::spawn(crate::safety_task::safety_task(
        state.clone(), state.topology_router.get().unwrap().clone(), rx,
    ));
    let _worker_scope = AbortWorkerOnDrop(worker.abort_handle());
    pause.entered().await;
    // The real original observation precedes this actual held owner. A missing
    // capture is a failed fixture premise, never fabricated publication proof.
    assert_eq!(outcomes.snapshot()["counts"]["observation_captured"], 1);
    let mut busy = N6BusyGseOwner::start(state.clone());
    shutdown.send(()).unwrap();
    pause.release();
    let joined = tokio::time::timeout(Duration::from_secs(2), &mut worker).await;
    if joined.is_err() {
        worker.abort();
        let cleanup = tokio::time::timeout(Duration::from_secs(1), &mut worker).await;
        busy.finish();
        assert!(cleanup.is_ok(), "Busy negative worker not reaped in cleanup-only 1s");
        panic!("Busy negative exceeded original single-tick 2s worker deadline");
    }
    joined.unwrap().unwrap();
    let counters = outcomes.snapshot();
    busy.finish();
    assert_eq!(pause.calls(), 1);
    assert_eq!(counters["counts"]["publish_entered"], 1);
    assert_eq!(counters["counts"]["commit_Busy"], 1);
    assert_eq!(counters["counts"]["commit_OK"], 0);
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(crate::network_variables::flight_state(), 0);
    assert_eq!(crate::network_variables::automatic_idle_test_snapshot(), before);
    assert_eq!(cached(), cached_before, "refusal must not replace/restamp cached FSTATE");
    assert_eq!(crate::gse::automatic_generation(&state), generation);
    assert_eq!((state.manual_abort_intent.epoch(), state.deferred_abort.manual_epoch()), epochs);
    eprintln!("NORMAL_N6_ACTUAL_BUSY_REFUSAL {counters}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n7_counted_old_tick_must_not_publish_after_actual_http_stop() {
    crate::mode_suite!(Normal, Testing);
    if separate("n7_counted_old_tick_must_not_publish_after_actual_http_stop") {
        return;
    }
    let fixture = normal().await;
    let state = fixture.state();
    let generation = crate::gse::automatic_generation(&state);
    let pause = CausalPause::install(&state, Site::CountedTick);
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let worker = crate::sequences::start_sequence_task(state.clone(), rx);
    pause.entered().await;
    assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
    actual_stop(state.clone()).await;
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let old = facts(&state, "Stop_with_counted_old_tick");
    shutdown.send(()).unwrap();
    pause.release();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pause.calls(), 1);
    let after = facts(&state, "retired_counted_tick_after_Stop");
    println!("NORMAL_CAUSAL {}", json!({"before":old,"after":after}));
    assert_eq!(
        crate::gse::automatic_generation(&state),
        generation,
        "no assumed reset epoch advance while inflight"
    );
    assert_eq!(
        state.local_flight_state_snapshot(),
        FlightState::Startup,
        "retained Stop must defeat stale local publication, not only board outputs"
    );
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
}

// Actual decoded SDK raw ingress and real correlated QUERY controls. These
// model peers certify only their native protocol state, not MCU storage.
async fn actual_raw_board_abort(fixture: &Fixture, board: Board) {
    let state=fixture.state();
    let router=state.topology_router.get().unwrap();
    let packet=Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],board.sender_id(),
        router.network_time_ms().unwrap(),Arc::<[u8]>::from([15])).unwrap();
    fixture.inject_state(packet).await;
    assert_ne!(state.board_abort.masks().0,0,"actual source-owned raw Abort must enter real register");
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn board_revision_changed_then_actual_healthy_query_cannot_revive_old_publication() {
    crate::mode_suite!(Normal, Testing);
    if separate("board_revision_changed_then_actual_healthy_query_cannot_revive_old_publication") {return;}
    let fixture=normal().await;let state=fixture.state();
    let original=crate::automatic_flight_state::Observation::begin(&state).unwrap();
    let captured=original.captured().unwrap();
    let old_revision=state.board_abort.abort_revision();
    let proposal=captured.proposal(FlightState::Idle,crate::automatic_flight_state::Cause::LegacySequence {next_policy:captured.policy}).unwrap();
    actual_raw_board_abort(&fixture,Board::ActuatorBoard).await;
    let changed=state.board_abort.abort_revision();assert!(changed>old_revision);
    fixture.query(Board::ActuatorBoard).await;
    // QUERY value publication precedes the actual board-observation callback
    // tail. Await THAT real register owner, never mint a new query/proof.
    until(||state.board_abort.masks().0==0).await;
    assert_eq!(state.board_abort.masks().0,0,"actual fresh correlated strong healthy status clears only board observation");
    assert_eq!(state.board_abort.abort_revision(),changed,"clear does not rewind revision");
    let before=crate::network_variables::automatic_idle_test_snapshot();
    let ty=crate::telemetry_schema::data_type("FLIGHT_STATE");
    let old_cache=state.topology_router.get().unwrap().get_cached_network_variable(ty).unwrap().map(|p|p.packet_id());
    assert!(!crate::automatic_flight_state::publish(&state,proposal),"old original revision must not be reclassified after clear");
    assert_eq!(state.local_flight_state_snapshot(),FlightState::Startup);
    assert_eq!(state.topology_router.get().unwrap().get_cached_network_variable(ty).unwrap().map(|p|p.packet_id()),old_cache);
    assert_eq!(crate::network_variables::automatic_idle_test_snapshot(),before);
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn active_actual_board_abort_cannot_capture_or_publish_automatic_prelaunch() {
    crate::mode_suite!(Normal, Testing);
    if separate("active_actual_board_abort_cannot_capture_or_publish_automatic_prelaunch") {return;}
    let fixture=normal().await;let state=fixture.state();
    actual_raw_board_abort(&fixture,Board::ActuatorBoard).await;
    let before=crate::network_variables::automatic_idle_test_snapshot();
    let observation=crate::automatic_flight_state::Observation::begin(&state).unwrap();
    assert!(observation.captured().is_none(),"known active Abort is not an eligible automatic origin");
    crate::sequences::refresh_action_policy_now(&state);
    assert_eq!(state.local_flight_state_snapshot(),FlightState::Startup);
    assert_eq!(crate::network_variables::automatic_idle_test_snapshot(),before);
    assert_ne!(state.board_abort.masks().0,0,"automatic projection never clears actual board Abort");
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn actual_healthy_query_allows_only_fresh_original_live_phase_after_clear() {
    crate::mode_suite!(Normal, Testing);
    if separate("actual_healthy_query_allows_only_fresh_original_live_phase_after_clear") {return;}
    let fixture=normal().await;let state=fixture.state();
    actual_raw_board_abort(&fixture,Board::ActuatorBoard).await;
    assert!(crate::automatic_flight_state::Observation::begin(&state).unwrap().captured().is_none());
    let revision=state.board_abort.abort_revision();
    fixture.query(Board::ActuatorBoard).await;
    // QUERY value publication precedes the actual board-observation callback
    // tail. Await THAT real register owner, never mint a new query/proof.
    until(||state.board_abort.masks().0==0).await;
    assert_eq!(state.board_abort.masks().0,0);
    assert_eq!(state.board_abort.abort_revision(),revision);
    let before=crate::network_variables::automatic_idle_test_snapshot();
    let baseline_state = state.local_flight_state_snapshot();
    let sdk_identity = || state.topology_router.get().unwrap()
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap().map(|packet| (packet.packet_id(), packet.timestamp(), packet.data_type(),
            packet.sender().to_string(), packet.endpoints().to_vec(), packet.payload().to_vec()));
    let baseline_sdk = sdk_identity();
    let first_admitted = {
        let observation=crate::automatic_flight_state::Observation::begin(&state).unwrap();
        let captured=observation.captured().expect("fresh post-clear original is eligible");
        let proposal=captured.proposal(FlightState::Idle,crate::automatic_flight_state::Cause::LegacySequence {next_policy:captured.policy}).unwrap();
        let admitted = crate::automatic_flight_state::publish(&state,proposal);
        if !admitted {
            // A nonwaiting attempt is not guaranteed an immediate SDK owner.
            // It must leave the ENTIRE actual baseline untouched on refusal.
            assert_eq!(state.local_flight_state_snapshot(), baseline_state);
            assert_eq!(crate::network_variables::automatic_idle_test_snapshot(), before);
            assert_eq!(sdk_identity(), baseline_sdk);
            assert_eq!(state.board_abort.masks().0, 0);
            assert_eq!(state.board_abort.abort_revision(), revision);
        }
        drop(observation); // retire the old original before any current cycle
        admitted
    };
    eprintln!("NORMAL_HEALTHY_CLEAR_FIRST_ATTEMPT accepted={first_admitted} revision={revision} refusal_cause=UNKNOWN_IF_REFUSED");
    healthy_current_service(&state).await;
    assert_eq!(state.board_abort.masks().0, 0);
    assert_eq!(state.board_abort.abort_revision(), revision);
    assert_eq!(state.local_flight_state_snapshot(),FlightState::Idle);
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
    assert_eq!(crate::network_variables::flight_state(),0);
    assert_eq!(state.topology_router.get().unwrap().get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap().unwrap().payload(),&[1]);
}
