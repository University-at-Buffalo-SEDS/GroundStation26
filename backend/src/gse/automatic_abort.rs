//! Serialize automatic-abort ownership with durable Fire using the per-GSE lock.
//! A dispatch permit owns no lock while the router invokes transport callbacks.
use super::*;

pub(crate) struct AutomaticAbortClaim {
    runtime: Arc<std::sync::Mutex<Runtime>>,
    _operator_claim: Option<OperatorDispatchClaim>,
}
impl Drop for AutomaticAbortClaim {
    fn drop(&mut self) {
        self.runtime.lock().unwrap().automatic_abort_in_flight = false;
    }
}

// Safety catalog (About safety): no-automatic-abort-after-launch. Keep backend/src/safety_catalog_rules.rs in step.
pub(super) fn stage_allows_automatic_abort(rt: &Runtime, stage: FlightState) -> bool {
    rt.operational.saved.qualification.stage() != gse_sequence::qualification::MissionStage::Fire
        && matches!(
            stage,
            FlightState::Startup
                | FlightState::Idle
                | FlightState::PreFill
                | FlightState::FillTest
                | FlightState::NitrogenFill
                | FlightState::NitrousFill
                | FlightState::Armed
        )
}

pub(crate) enum LivenessAbortDecision {
    Claimed(AutomaticAbortClaim),
    Startup(FlightState),
    Unavailable,
}

pub(crate) fn claim_automatic_abort(state: &AppState) -> Option<AutomaticAbortClaim> {
    claim_automatic_abort_for_generation(state, automatic_generation(state))
}

pub(crate) fn claim_automatic_abort_for_generation(
    state: &AppState,
    generation: u64,
) -> Option<AutomaticAbortClaim> {
    if crate::runtime_mode::is_hitl() {
        let operator_claim = claim_operator_dispatch(state, generation)?;
        let mut runtime = state.gse.lock().unwrap();
        if runtime.automatic_abort_in_flight
            || state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
            || !matches!(
                state.local_flight_state_snapshot(),
                FlightState::Startup
                    | FlightState::Idle
                    | FlightState::PreFill
                    | FlightState::FillTest
                    | FlightState::NitrogenFill
                    | FlightState::NitrousFill
                    | FlightState::Armed
            )
        {
            return None;
        }
        runtime.automatic_abort_in_flight = true;
        Some(AutomaticAbortClaim {
            runtime: state.gse.clone(),
            _operator_claim: Some(operator_claim),
        })
    } else {
        match claim_from_source(state, false, generation) {
            LivenessAbortDecision::Claimed(claim) => Some(claim),
            LivenessAbortDecision::Startup(stage) => {
                debug_assert_eq!(stage, FlightState::Startup);
                None
            }
            LivenessAbortDecision::Unavailable => None,
        }
    }
}

pub(crate) fn claim_liveness_abort(state: &AppState) -> LivenessAbortDecision {
    claim_from_source(state, true, automatic_generation(state))
}

fn claim_from_source(state: &AppState, liveness: bool, generation: u64) -> LivenessAbortDecision {
    {
        let Some(operator_claim) = claim_operator_dispatch(state, generation) else {
            return LivenessAbortDecision::Unavailable;
        };
        let mut rt = state.gse.lock().unwrap();
        // Preserve the original general-claim short circuit: a held/latched
        // claim must not acquire the flight-state mutex unnecessarily.
        if rt.automatic_abort_in_flight
            || rt.operational.saved.automatic_abort_claimed
            || state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        {
            return LivenessAbortDecision::Unavailable;
        }
        let stage = state.local_flight_state_snapshot();
        // Source-specific eligibility is checked at the same serialized claim
        // decision as the durable latch. Disconnect aborts arm from the Idle
        // mission phase onward; only Startup is exempt.
        if liveness
            && crate::mission_phase::safety_phase(stage) == crate::mission_phase::MissionPhase::Startup
        {
            return LivenessAbortDecision::Startup(stage);
        }
        if !stage_allows_automatic_abort(&rt, stage) {
            return LivenessAbortDecision::Unavailable;
        }
        rt.automatic_abort_in_flight = true;
        rt.operational.saved.automatic_abort_claimed = true;
        rt.operational.running = false;
        rt.engine.claimed = true;
        rt.engine.status.phase = Phase::Fault;
        rt.engine.status.message =
            "Automatic abort requested; clear GSE abort before another operation".into();
        // Storage failure cannot suppress the critical abort. The RAM latch and
        // persist_error still block Fire; durability is not claimed on I/O error.
        if let Err(error) = rt.operational.persist() {
            log::error!(
                "Automatic abort could not be persisted; dispatching abort and blocking Fire: {error}"
            );
        }
        drop(rt);
        LivenessAbortDecision::Claimed(AutomaticAbortClaim {
            runtime: state.gse.clone(),
            _operator_claim: Some(operator_claim),
        })
    }
}

pub(super) fn reserve_abort_clear(state: &AppState, rt: &mut Runtime) -> AutomaticAbortClaim {
    debug_assert!(!rt.automatic_abort_in_flight);
    rt.automatic_abort_in_flight = true;
    AutomaticAbortClaim {
        runtime: state.gse.clone(),
        // Existing explicit clear already owns its outer dispatch reservation.
        _operator_claim: None,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    async fn fixture() -> (
        Arc<AppState>,
        Arc<sedsnet::router::Router>,
        Arc<AtomicUsize>,
        std::path::PathBuf,
    ) {
        fixture_with_state(crate::state::tests::test_app_state().await).await
    }

    pub(in crate::gse) async fn fixture_with_state(
        state: Arc<AppState>,
    ) -> (
        Arc<AppState>,
        Arc<sedsnet::router::Router>,
        Arc<AtomicUsize>,
        std::path::PathBuf,
    ) {
        let weak = Arc::downgrade(&state);
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([
                sedsnet::router::EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("ABORT"),
                    move |packet| {
                        assert!(
                            weak.upgrade().unwrap().gse.try_lock().is_ok(),
                            "router callback must not run under GSE lock"
                        );
                        if packet.data_type() == crate::telemetry_schema::data_type("ABORT") {
                            count.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(())
                    },
                ),
            ]),
        ));
        let path = armed_fixture_for_test(&state, &router);
        (state, router, seen, path)
    }
    // The direct safety producer is deliberately absent from test-fire builds.
    // Exercise shared claim/TX/Clear policy there; the separate actual command
    // worker regression covers the applicable automatic producer end to end.
    fn dispatch_abort_for_shared_policy_test(
        state: &Arc<AppState>,
        router: &sedsnet::router::Router,
    ) -> bool {
        if !crate::runtime_mode::is_test_fire() {
            // The HITL suite skips every caller (HITL builds had no safety task).
            crate::safety_task::dispatch_safety_abort(state, router)
        } else {
            let Some(_claim) = claim_automatic_abort(state) else {
                return false;
            };
            crate::telemetry_task::queue_automatic_abort_packet(
                router,
                "Safety Task Abort Command Issued",
            )
            .unwrap();
            router.process_all_queues_with_timeout(3).unwrap();
            true
        }
    }
    fn acknowledge_abort_owners(router: &sedsnet::router::Router) {
        for row in router.export_reliable_pending_stats() {
            if row.data_type != crate::telemetry_schema::data_type("ABORT") {
                continue;
            }
            assert_eq!(row.pending_destinations.len(), 1);
            // The prerequisite fixture advertises GB. Named ACKs follow the
            // same production identity validation as that discovered owner.
            let ack = sedsnet::packet::Packet::new(
                sedsnet::config::DataType::ReliableAck,
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "E2EACK:GB",
                1,
                Arc::<[u8]>::from(row.packet_id.to_le_bytes()),
            )
            .unwrap();
            router.rx_queue_from_side(ack, 0).unwrap();
        }
        router.process_all_queues_with_timeout(0).unwrap();
    }
    #[tokio::test]
    async fn liveness_claim_rechecks_startup_without_weakening_general_fault_claims() {
        crate::mode_suite!(Normal, Testing);
        let (state, _router, _seen, path) = fixture().await;
        state.set_preexisting_flight_state_for_test(FlightState::Startup);
        assert!(matches!(
            claim_liveness_abort(&state),
            LivenessAbortDecision::Startup(FlightState::Startup)
        ));
        assert!(
            !state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        // GENERIC_ERROR and automatic ADC failure use this unchanged general path.
        let claim = claim_automatic_abort(&state).expect("general fault remains eligible in Startup");
        assert!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        drop(claim);
        assert!(
            claim_automatic_abort(&state).is_none(),
            "durable claim remains latched"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn liveness_claim_is_armed_from_the_idle_phase() {
        crate::mode_suite!(Normal, Testing);
        let (state, _router, _seen, path) = fixture().await;
        state.set_local_flight_state(FlightState::Idle);
        assert!(matches!(
            claim_liveness_abort(&state),
            LivenessAbortDecision::Claimed(_)
        ));
        assert!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }

    /// Every GS automatic-abort producer, enumerated: none may act from Launch
    /// admission onward (Launch, In flight and Landed mission phases).
    /// Operator manual Abort/Stop is a separate path and is not gated here.
    #[tokio::test]
    async fn no_automatic_abort_path_acts_after_launch() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        let post_launch = [
            FlightState::Launch,
            FlightState::Ascent,
            FlightState::Coast,
            FlightState::Apogee,
            FlightState::Descent,
            FlightState::Reefing,
            FlightState::Landed,
            FlightState::Recovery,
        ];
        for stage in post_launch {
            state.set_preexisting_flight_state_for_test(stage);
            let phase = crate::mission_phase::safety_phase(stage);
            assert!(matches!(
                phase,
                crate::mission_phase::MissionPhase::Launch
                    | crate::mission_phase::MissionPhase::InFlight
                    | crate::mission_phase::MissionPhase::Landed
            ));
            assert!(!phase.automatic_aborts_allowed(), "{stage:?}");
            assert!(!phase.disconnect_aborts_armed(), "{stage:?}");
            let generation = automatic_generation(&state);
            // 1. Shared stage boundary used by every producer.
            assert!(!automatic_abort_allowed(&state), "{stage:?}");
            // 2. General claim: safety-task faults, ADC/tare failure, overpressure.
            assert!(claim_automatic_abort(&state).is_none(), "{stage:?}");
            assert!(claim_automatic_abort_for_generation(&state, generation).is_none());
            // 3. Disconnect (board-offline) claim.
            if !crate::runtime_mode::is_test_fire() {
                assert!(!matches!(
                    claim_liveness_abort(&state),
                    LivenessAbortDecision::Claimed(_)
                ));
            }
            // 4. Queued automatic abort and automatic supply closes.
            assert!(!queue_automatic_abort_for_generation(&state, &router, generation, "test").unwrap());
            assert!(!queue_automatic_supply_close(
                &state,
                &router,
                crate::rocket_commands::ActuatorBoardCommands::NitrousClose as u8,
                generation
            )
            .unwrap());
            // 5. Final dispatch filter for automatic command intents.
            for command in [
                TelemetryCommand::AutomaticAbort { generation },
                TelemetryCommand::AutomaticNitrogenClose { generation },
                TelemetryCommand::AutomaticNitrousClose { generation },
            ] {
                assert!(crate::telemetry_task::command_at_dispatch(&state, command).is_none());
            }
            // 6. Observed automatic abort acceptance. This is where the GS sees
            // board-side automatic aborts: AB heartbeat/initial-heartbeat/ADC
            // and VB heartbeat/continuity/overpressure aborts arrive as ABORT
            // packets tagged SEDS_AUTO_ABORT_V1 (About safety: Board behaviour).
            // The boards gate them before Launch themselves; the GS must not
            // adopt one after Launch either.
            assert!(!accept_observed_abort(&state, true, generation));
        }
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 0, "no ABORT left the station");
        assert!(!state.abort_indicator_latched());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn latched_general_claim_returns_without_waiting_for_flight_state_mutex() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, _router, _seen, path) = fixture().await;
        drop(claim_automatic_abort(&state).unwrap());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let held_state = state.state.lock().unwrap();
        let worker = std::thread::spawn(move || {
            let _ = started_tx.send(());
            let unavailable = matches!(
                claim_from_source(&worker_state, false, automatic_generation(&worker_state)),
                LivenessAbortDecision::Unavailable
            );
            let _ = result_tx.send(unavailable);
        });
        let started = started_rx.recv_timeout(std::time::Duration::from_secs(1));
        let outcome = if started.is_ok() {
            result_rx.recv_timeout(std::time::Duration::from_secs(1))
        } else {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        };
        // Always release the lock and join before asserting, including the
        // old-order negative path, so the regression cannot strand a worker.
        drop(held_state);
        let joined = worker.join();
        let _ = std::fs::remove_file(path);
        assert!(started.is_ok(), "claim worker started");
        assert!(joined.is_ok(), "claim worker completed after cleanup");
        assert!(
            matches!(outcome, Ok(true)),
            "latched claim returns Unavailable without reading state"
        );
    }

    #[tokio::test]
    async fn abort_claim_first_blocks_fire_and_clear_until_dispatch_finishes() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        let ready = Arc::new(Barrier::new(2));
        let go = Arc::new(Barrier::new(2));
        let worker_state = state.clone();
        let worker_router = router.clone();
        let worker_ready = ready.clone();
        let worker_go = go.clone();
        let worker = std::thread::spawn(move || {
            let claim = claim_automatic_abort(&worker_state).unwrap();
            worker_ready.wait();
            worker_go.wait();
            crate::telemetry_task::queue_automatic_abort_packet(
                &worker_router,
                "claimed automatic abort",
            )
            .unwrap();
            worker_router.process_all_queues_with_timeout(0).unwrap();
            drop(claim);
        });
        ready.wait();
        assert!(
            commit_fire_intent(&state)
                .unwrap_err()
                .contains("Automatic abort")
        );
        assert!(
            clear_ground_abort(&state)
                .unwrap_err()
                .1
                .contains("in progress")
        );
        go.wait();
        worker.join().unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        // Requalification cannot erase the separate persisted abort latch.
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational.saved.qualification.reset().unwrap();
            rt.operational.persist().unwrap();
        }
        assert!(commit_fire_intent(&state).is_err());
        acknowledge_abort_owners(&router);
        router
            .log_queue(crate::telemetry_schema::data_type("FLIGHT_BUZZER"), &[1u8])
            .unwrap();
        assert!(
            router.has_queued_work(),
            "unrelated telemetry need not globally drain"
        );
        clear_ground_abort(&state).unwrap();
        assert!(
            !operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn fire_first_denies_automatic_claim_but_manual_abort_remains_available() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        commit_fire_intent(&state).unwrap();
        assert!(claim_automatic_abort(&state).is_none());
        assert!(!dispatch_abort_for_shared_policy_test(&state, &router));
        assert!(state.is_command_allowed(&TelemetryCommand::Abort));
        crate::telemetry_task::queue_abort_packet(&router, "manual operator abort").unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn persistence_failure_still_dispatches_abort_and_keeps_fire_blocked() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, router, seen, path) = fixture().await;
        state.gse.lock().unwrap().operational.path = Some(path.join("cannot-create-below-file"));
        assert!(
            dispatch_abort_for_shared_policy_test(&state, &router),
            "storage failure must not suppress critical abort"
        );
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let rt = state.gse.lock().unwrap();
        assert!(rt.operational.saved.automatic_abort_claimed);
        assert!(rt.operational.persist_error.is_some());
        drop(rt);
        assert!(commit_fire_intent(&state).is_err());
        assert!(clear_ground_abort(&state).is_err());
        assert!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn failed_final_clear_save_retains_claim_and_queue_failure_does_not_rearm() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let (state, _router, _seen, path) = fixture().await;
        let claim = claim_automatic_abort(&state).unwrap();
        // A genuine router queue-admission failure, not a mocked callback.
        let memory = sedsnet::config::RuntimeMemoryConfig {
            max_queue_budget: 64,
            starting_queue_size: 64,
            max_recent_rx_ids: 1,
            queue_grow_step: 1.0,
        };
        let bounded = sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([])
                .with_memory_config(memory)
                .unwrap(),
        );
        assert!(
            crate::telemetry_task::queue_automatic_abort_packet(
                &bounded,
                "automatic abort cannot fit in tiny queue"
            )
            .is_err()
        );
        drop(claim);
        assert!(commit_fire_intent(&state).is_err());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational.path = Some(path.join("invalid-final-save"));
            assert!(rt.operational.clear_automatic_abort_claim().is_err());
            assert!(rt.operational.saved.automatic_abort_claimed);
        }
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn persisted_claim_restores_abort_and_explicit_clear_recovers_after_restart() {
        crate::mode_suite!(Normal, TestFire, Testing);
        const CHILD: &str = "GS_ABORT_CLAIM_RESTART_TEST";
        const NAME: &str = "gse::automatic_abort::tests::persisted_claim_restores_abort_and_explicit_clear_recovers_after_restart";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (state, _router, _seen, path) = fixture().await;
        let claim = claim_automatic_abort(&state).unwrap();
        drop(claim);
        let restored = crate::state::tests::test_app_state().await;
        unsafe {
            std::env::set_var("GS_GSE_QUALIFICATION", &path);
        }
        initialize(&restored);
        unsafe {
            std::env::remove_var("GS_GSE_QUALIFICATION");
        }
        assert_eq!(restored.local_flight_state_snapshot(), FlightState::Aborted);
        assert!(
            restored
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        assert!(commit_fire_intent(&restored).is_err());
        // A late FC Idle cannot bypass the latch; explicit recovery remains reachable.
        restored.set_local_flight_state(FlightState::Idle);
        assert!(commit_fire_intent(&restored).is_err());
        assert!(
            clear_ground_abort(&restored).is_err(),
            "missing router must not clear the persisted latch"
        );
        assert!(
            restored
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .automatic_abort_claimed
        );
        restored
            .topology_router
            .set(Arc::new(sedsnet::router::Router::new(
                sedsnet::router::RouterConfig::new([]),
            )))
            .unwrap();
        crate::telemetry_task::refresh_host_network_time(
            restored.topology_router.get().unwrap(),
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        clear_ground_abort(&restored).unwrap();
        assert!(
            !operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn clear_waits_for_busy_sdk_retry_then_becomes_available() {
        crate::mode_suite!(Normal, TestFire, Testing);
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let state = crate::state::tests::test_app_state().await;
        let now = Arc::new(AtomicU64::new(0));
        let clock = now.clone();
        let router = Arc::new(sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]),
            Box::new(move || clock.load(Ordering::SeqCst)),
        ));
        let path = armed_fixture_for_test(&state, &router);
        router.remove_side(0).unwrap(); // Remove prerequisite-only advertised peer.
        let busy = Arc::new(AtomicBool::new(true));
        let blocked = busy.clone();
        router.add_side_packed("no_advertised_peer", move |wire| {
            if sedsnet::wire_format::peek_frame_info(wire)
                .is_ok_and(|f| f.envelope.ty == crate::telemetry_schema::data_type("ABORT"))
                && blocked.load(Ordering::SeqCst)
            {
                return Err(sedsnet::TelemetryError::Io("side tx busy"));
            }
            Ok(())
        });
        assert!(dispatch_abort_for_shared_policy_test(&state, &router));
        assert!(
            clear_ground_abort(&state)
                .unwrap_err()
                .1
                .contains("delivery is still pending")
        );
        assert!(commit_fire_intent(&state).is_err());
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        busy.store(false, Ordering::SeqCst);
        now.store(1000, Ordering::SeqCst);
        router.process_all_queues_with_timeout(0).unwrap();
        assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT")));
        clear_ground_abort(&state).unwrap();
        assert!(
            !operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn clear_retires_an_automatic_abort_the_radio_cannot_deliver() {
        crate::mode_suite!(Normal, TestFire, Testing);
        use std::sync::atomic::AtomicU64;
        let state = crate::state::tests::test_app_state().await;
        let clock = Arc::new(AtomicU64::new(0));
        let model = clock.clone();
        let router = Arc::new(sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]),
            Box::new(move || model.load(Ordering::SeqCst)),
        ));
        let path = armed_fixture_for_test(&state, &router);
        router.remove_side(0).unwrap();
        // The AV bay sits behind a saturated radio that refuses every ABORT.
        let radio = router.add_side_packed("saturated_radio", |wire| {
            if sedsnet::wire_format::peek_frame_info(wire)
                .is_ok_and(|f| f.envelope.ty == crate::telemetry_schema::data_type("ABORT"))
            {
                return Err(sedsnet::TelemetryError::Io("side tx busy"));
            }
            Ok(())
        });
        let announce = sedsnet::discovery::build_discovery_announce(
            "FC",
            123,
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
        )
        .unwrap();
        router.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), radio).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert!(dispatch_abort_for_shared_policy_test(&state, &router));
        let abort = crate::telemetry_schema::data_type("ABORT");
        let id = router.export_reliable_pending_stats().into_iter()
            .find(|row| row.data_type == abort)
            .expect("owed to the AV bay").packet_id;
        assert!(operator_reset_block_reason(&state).is_some_and(|r| r.contains("ABORT")));
        // The normal delivery window still holds Clear (unchanged rule).
        assert!(clear_ground_abort(&state).unwrap_err().1.contains("delivery is still pending"));
        crate::gse::age_delivery_tracking_for_test(crate::sim_freshness::wall(UNDELIVERABLE_AFTER) * 2);
        clear_ground_abort(&state).unwrap();
        assert_eq!(
            crate::abort_retirement::retired_reason(&router, id),
            Some(crate::abort_retirement::RetireReason::OperatorCleared)
        );
        assert!(router.has_pending_reliable_tx_type(abort), "the SDK still holds it");
        assert!(
            !operator_reset_block_reason(&state).is_some_and(|r| r.contains("ABORT")),
            "a retired abort no longer holds Reset"
        );
        let notes = state.notifications.lock().unwrap().iter().map(|n| n.message.clone()).collect::<Vec<_>>();
        assert!(notes.iter().any(|n| n.contains("Stale automatic Abort retired") && n.contains("Flight Computer")), "{notes:?}");
        assert!(!notes.iter().any(|n| n == UNDELIVERED_ABORT_AFTER_CLEAR), "{notes:?}");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn clear_waits_for_no_peer_abort_already_handed_to_software_transport() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        let path = armed_fixture_for_test(&state, &router);
        router.remove_side(0).unwrap(); // This test deliberately has no ACK owner.
        let activity = state.transport_tx_activity.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<(u8, Vec<u8>)>();
        router.add_side_packed("asynchronous_transport", move |wire| {
            if sedsnet::wire_format::peek_frame_info(wire)
                .is_ok_and(|f| f.envelope.ty == crate::telemetry_schema::data_type("ABORT"))
            {
                activity.try_enqueue(|| {
                    sender
                        .send((255, wire.to_vec()))
                        .map_err(|_| sedsnet::TelemetryError::Io("software transport closed"))
                })?;
            }
            Ok(())
        });
        assert!(dispatch_abort_for_shared_policy_test(&state, &router));
        assert!(
            !router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT")),
            "no advertised peer: SDK has handed off this publication"
        );
        assert!(state.has_pending_transport_tx());
        let (_, wire) = receiver
            .try_recv()
            .expect("actual Comms channel owns the accepted abort");
        let packet = sedsnet::wire_format::unpack_packet(&wire).unwrap();
        assert_eq!(
            packet.data_type(),
            crate::telemetry_schema::data_type("ABORT")
        );
        assert_eq!(
            packet.payload(),
            b"SEDS_AUTO_ABORT_V1:Safety Task Abort Command Issued"
        );
        // Popping the software channel does not release ownership before the
        // worker completes its write; active-write retries remain pending.
        assert!(state.has_pending_transport_tx());
        assert!(
            clear_ground_abort(&state)
                .unwrap_err()
                .1
                .contains("delivery is still pending")
        );
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .automatic_abort_claimed
        );
        assert!(commit_fire_intent(&state).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn old_saved_record_defaults_to_no_automatic_abort_claim() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let mut old = serde_json::to_value(operational::Saved::default()).unwrap();
        old.as_object_mut()
            .unwrap()
            .remove("automatic_abort_claimed");
        let restored: operational::Saved = serde_json::from_value(old).unwrap();
        assert!(!restored.automatic_abort_claimed);
    }
}

#[cfg(test)]
mod launch_inhibition_tests {
    use super::*;

    #[tokio::test]
    async fn launch_inhibits_automatic_abort_despite_delayed_state_and_clock_reset() {
        let state = crate::state::tests::test_app_state().await;
        crate::gse::qualified_launch_proofs_for_test(&state);
        state.set_local_flight_state(FlightState::Armed);
        super::super::inhibit_automatic_actions_for_launch(&state).unwrap();
        for stage in [
            FlightState::Armed,
            FlightState::Idle,
            FlightState::Startup,
            FlightState::Launch,
        ] {
            // A still-active fill with missing pressure would otherwise fault and
            // automatically close/open valves on the next real controller tick.
            state.gse.lock().unwrap().engine.status.phase = Phase::Filling;
            state.set_preexisting_flight_state_for_test(stage);
            state.reset_launch_clock();
            assert!(!super::super::automatic_abort_allowed(&state));
            assert!(claim_automatic_abort(&state).is_none());
            for command in [
                TelemetryCommand::AutomaticNitrogenClose {
                    generation: crate::gse::automatic_generation(&state),
                },
                TelemetryCommand::AutomaticNitrousClose {
                    generation: crate::gse::automatic_generation(&state),
                },
            ] {
                assert!(crate::telemetry_task::command_at_dispatch(&state, command).is_none());
            }
            assert!(
                crate::telemetry_task::command_at_dispatch(&state, TelemetryCommand::Abort)
                    .is_some()
            );
            assert!(
                super::super::inhibit_automatic_actions_for_launch(&state).is_err(),
                "T-clock reset cannot admit launch twice"
            );
            super::super::tick(&state, false);
            super::super::pause_fill_for_recoverable_fault(&state, "late board error");
            super::super::start_fill_after_automatic_zero(
                &state,
                crate::gse::automatic_generation(&state),
            );
            super::super::report_safety(&state);
            assert_eq!(
                state.gse.lock().unwrap().engine.status.phase,
                Phase::Filling,
                "post-launch automatic tick/cancel must not change valve sequencing"
            );
        }
    }

    #[tokio::test]
    async fn launch_cannot_overtake_an_admitted_automatic_abort() {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Armed);
        let claim = claim_automatic_abort(&state).expect("prelaunch automatic abort admission");
        assert!(super::super::inhibit_automatic_actions_for_launch(&state).is_err());
        drop(claim);
    }
}
