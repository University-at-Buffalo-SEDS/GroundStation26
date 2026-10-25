//! Serialize automatic-abort ownership with durable Fire using the per-GSE lock.
//! A dispatch permit owns no lock while the router invokes transport callbacks.
use super::*;

pub(crate) struct AutomaticAbortClaim {
    #[cfg(not(feature = "hitl_mode"))]
    runtime: Arc<std::sync::Mutex<Runtime>>,
}
impl Drop for AutomaticAbortClaim {
    fn drop(&mut self) {
        #[cfg(not(feature = "hitl_mode"))]
        {
            self.runtime.lock().unwrap().automatic_abort_in_flight = false;
        }
    }
}

#[cfg(not(feature = "hitl_mode"))]
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

pub(crate) fn claim_automatic_abort(state: &AppState) -> Option<AutomaticAbortClaim> {
    #[cfg(feature = "hitl_mode")]
    {
        let _ = state;
        Some(AutomaticAbortClaim {})
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        let mut rt = state.gse.lock().unwrap();
        if rt.automatic_abort_in_flight
            || rt.operational.saved.automatic_abort_claimed
            || !stage_allows_automatic_abort(&rt, state.local_flight_state_snapshot())
        {
            return None;
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
        Some(AutomaticAbortClaim {
            runtime: state.gse.clone(),
        })
    }
}

#[cfg(not(feature = "hitl_mode"))]
pub(super) fn reserve_abort_clear(state: &AppState, rt: &mut Runtime) -> AutomaticAbortClaim {
    debug_assert!(!rt.automatic_abort_in_flight);
    rt.automatic_abort_in_flight = true;
    AutomaticAbortClaim {
        runtime: state.gse.clone(),
    }
}

#[cfg(all(test, not(feature = "hitl_mode")))]
mod tests {
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
        let state = crate::state::tests::test_app_state().await;
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
        #[cfg(not(feature = "test_fire_mode"))]
        {
            crate::safety_task::dispatch_safety_abort(state, router)
        }
        #[cfg(feature = "test_fire_mode")]
        {
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
    async fn abort_claim_first_blocks_fire_and_clear_until_dispatch_finishes() {
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
    async fn clear_waits_for_no_peer_abort_already_handed_to_software_transport() {
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
        assert_eq!(packet.payload(), b"Safety Task Abort Command Issued");
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
        let mut old = serde_json::to_value(operational::Saved::default()).unwrap();
        old.as_object_mut()
            .unwrap()
            .remove("automatic_abort_claimed");
        let restored: operational::Saved = serde_json::from_value(old).unwrap();
        assert!(!restored.automatic_abort_claimed);
    }
}
