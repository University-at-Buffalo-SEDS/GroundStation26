static POSTMISSION_NATIVE_STATES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
fn postmission_native_ingress_enabled(state:&Arc<AppState>) -> bool {
    POSTMISSION_NATIVE_STATES.lock().unwrap().contains(&(Arc::as_ptr(state) as usize))
}
struct PostmissionNativeIngress(usize);
impl PostmissionNativeIngress {
    fn install(state:&Arc<AppState>)->Self {
        let key=Arc::as_ptr(state) as usize;POSTMISSION_NATIVE_STATES.lock().unwrap().push(key);Self(key)
    }
}
impl Drop for PostmissionNativeIngress {
    fn drop(&mut self){POSTMISSION_NATIVE_STATES.lock().unwrap().retain(|key|*key!=self.0);}
}

// Private actual-host operator-flow diagnostic; peer application state is an explicit model.
#[tokio::test]
async fn local_stop_postmission_reset_actual_runtime_diagnostic() {
    // Own process: the test-fire receipt probe and the native peer services
    // are process-wide and another diagnostic cannot share them.
    if isolated("local_stop_postmission_reset_actual_runtime_diagnostic") { return; }
    use tower::ServiceExt;
    use crate::types::FlightState;
    let mut observed = Vec::new();
    for initial in [FlightState::Landed, FlightState::Recovery] {
        let _tf_probe = crate::runtime_mode::is_test_fire().then(tf_fresh_receipt_probe);
        let fixture = Fixture::new_with_managed_peers().await;
        let state = fixture.state();
        let _ingress=PostmissionNativeIngress::install(&state);
        state.set_local_flight_state(initial);
        // This is the initial postmission state, never an artificial post-Stop Aborted transition.
        for (board, model) in &fixture.models {
            if matches!(board, Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer) {
                model.lock().unwrap().state = initial as u8;
            }
        }
        let (deferred, drain) = crate::deferred_abort::spawn(state.clone());
        let stop_state = state.clone();
        let stop_router = state.topology_router.get().unwrap().clone();
        let publisher = tokio::spawn(crate::manual_abort_intent::service_with_claim(
            state.manual_abort_intent.clone(), state.shutdown_subscribe(), move |claim| {
                match crate::manual_abort_utc::publish_local_claim(&stop_state, &stop_router, claim) {
                    Ok(true) => { crate::telemetry_task::flush_command_tx(&stop_router,"private postmission Manual Stop"); true },
                    _ => false,
                }
            }
        ));
        let response = crate::web::router(state.clone(), "private-fixture-only".into()).oneshot(
            axum::http::Request::builder().method("POST").uri("/api/command")
                .header("content-type","application/json").header("authorization","Bearer operator")
                .body(axum::body::Body::from("\"Abort\"")).unwrap()
        ).await.unwrap();
        assert_eq!(response.status(),StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let all = {
                    let delivered = fixture.incoming_startup.lock().unwrap();
                    [Board::ActuatorBoard,Board::ValveBoard,Board::FlightComputer].iter().all(|board|
                        delivered.iter().any(|(b,p)|b==board && p.data_type()==crate::telemetry_schema::data_type("ABORT")
                            && p.payload().starts_with(b"SEDS_MANUAL_ABORT_V1:")))
                };
                if all && state.abort_indicator_latched() && state.manual_abort_intent.pending_epoch().is_none()
                    && !state.deferred_abort.manual_pending() { break; }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await.expect("actual authorized Stop must reach all three peer SDK callbacks and deferred host owner");
        // Explicit peer model boundary: only AFTER actual original Stop delivery do peers report Aborted.
        for (board,model) in &fixture.models {
            if matches!(board, Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer) {
                model.lock().unwrap().state = FlightState::Aborted as u8;
            }
        }
        for board in BOARDS { fixture.query(board).await; }
        for board in [Board::ActuatorBoard,Board::ValveBoard,Board::FlightComputer] {
            assert_eq!(state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()].as_ref().unwrap().value.state,Some(15));
        }
        let after = state.local_flight_state_snapshot();
        let response = super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),
            json!({"action":"reset_startup","boards":BOARDS.iter().map(|b|b.sender_id()).collect::<Vec<_>>(),"board":null,"mode":null,
                "backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
        let status = response.status(); let reply=body(response).await;
        eprintln!("POSTMISSION_FLOW {}",json!({"initial":initial as u8,"after_actual_stop":after as u8,
            "manual_epoch":state.manual_abort_intent.epoch(),"stop_indicator":state.abort_indicator_latched(),
            "fresh_peer_stop_states":[15,15,15],"prepare_status":status.as_u16(),"prepare_reply":reply,
            "scope":"actualHTTP/auth/SDK wire/manual publisher/deferred host/queries; peer application states are modeled, no physical proof"}));
        observed.push((initial as u8,after as u8,status.as_u16()));
        if status == StatusCode::OK {
            assert_eq!(after, FlightState::Aborted, "only admitted local Stop owns this host phase");
            assert_eq!(fixture.confirm(&reply).await.status(),StatusCode::OK);
            let terminal=fixture.terminal().await;
            assert_eq!(terminal["success"],true,"{terminal}");
            assert_eq!(terminal["reset_applied_state"],0);
            assert!(fixture.models.iter().all(|(_,m)|m.lock().unwrap().state==0));
            eprintln!("POSTMISSION_RESET_APPLIED {}",terminal);
        }

        publisher.abort(); let _=publisher.await;
        let _=drain.send(());
        // Drop the fixture first: it checks that its wire/receiver workers are
        // still alive, and the shutdown requested below ends the receiver.
        drop(fixture);
        state.request_shutdown(); let _=deferred.await;
    }
    assert!(observed.iter().all(|(_,after,status)|*after==15&&*status==200),"explicit Stop postmission reset remains unavailable: {observed:?}");
}

// Private actual-host operator-flow diagnostic; peer application state is an explicit model.
#[tokio::test]
async fn local_stop_active_startup_abort_reset_actual_runtime_diagnostic() {
    crate::mode_suite!(Normal, TestFire, Testing);
    // Own process, as above.
    if isolated("local_stop_active_startup_abort_reset_actual_runtime_diagnostic") { return; }
    use tower::ServiceExt;
    use crate::types::FlightState;
    let mut observed = Vec::new();
    for initial in [FlightState::Idle] {
        let _tf_probe = crate::runtime_mode::is_test_fire().then(tf_fresh_receipt_probe);
        let fixture = Fixture::new_with_active_startup_peer_services().await;
        let state = fixture.state();
        let _ingress=PostmissionNativeIngress::install(&state);
        state.set_local_flight_state(initial);
        // This is the initial postmission state, never an artificial post-Stop Aborted transition.
        for (board, model) in &fixture.models {
            if matches!(board, Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer) {
                model.lock().unwrap().state = initial as u8;
            }
        }
        for board in BOARDS { fixture.query(board).await; }
        let router=state.topology_router.get().unwrap();
        let gb_side=router.export_topology().routes.iter().find(|row|
            row.announcers.iter().any(|peer|peer.sender_id=="GB")).expect("actual GB discovery route").side_id;
        router.rx_from_side(&sedsnet::discovery::build_discovery_announce("GB",1,&[
            crate::telemetry_schema::endpoint(endpoint(Board::GatewayBoard)),
            crate::telemetry_schema::endpoint("FLIGHT_STATE"),
            crate::telemetry_schema::endpoint("VALVE_BOARD"),
            crate::telemetry_schema::endpoint("ACTUATOR_BOARD")]).unwrap(),gb_side).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        let active=crate::gse::native_owned_startup_prime(&state);
        eprintln!("ACTIVE_STARTUP_BEFORE {}",active);
        assert_eq!(active["operational_running"],true,"actual operational tick must own startup");
        assert_eq!(active["active"],true,"unconfirmed startup valves must remain active");
        // Restore this fixture's original direct-peer route before issuing Stop.
        // Startup ownership is real and retained; no operational flag or phase
        // is rewritten and no new GSE tick runs between this route event and Stop.
        router.rx_from_side(&sedsnet::discovery::build_discovery_announce("GB",1,&[
            crate::telemetry_schema::endpoint(endpoint(Board::GatewayBoard)),
            crate::telemetry_schema::endpoint("FLIGHT_STATE")]).unwrap(),gb_side).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        let (deferred, drain) = crate::deferred_abort::spawn(state.clone());
        let stop_state = state.clone();
        let stop_router = state.topology_router.get().unwrap().clone();
        let publisher = tokio::spawn(crate::manual_abort_intent::service_with_claim(
            state.manual_abort_intent.clone(), state.shutdown_subscribe(), move |claim| {
                match crate::manual_abort_utc::publish_local_claim(&stop_state, &stop_router, claim) {
                    Ok(true) => { crate::telemetry_task::flush_command_tx(&stop_router,"private postmission Manual Stop"); true },
                    _ => false,
                }
            }
        ));
        let response = crate::web::router(state.clone(), "private-fixture-only".into()).oneshot(
            axum::http::Request::builder().method("POST").uri("/api/command")
                .header("content-type","application/json").header("authorization","Bearer operator")
                .body(axum::body::Body::from("\"Abort\"")).unwrap()
        ).await.unwrap();
        assert_eq!(response.status(),StatusCode::ACCEPTED);
        let stop_delivery = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let all = {
                    let delivered = fixture.incoming_startup.lock().unwrap();
                    [Board::ActuatorBoard,Board::ValveBoard,Board::FlightComputer].iter().all(|board|
                        delivered.iter().any(|(b,p)|b==board && p.data_type()==crate::telemetry_schema::data_type("ABORT")
                            && p.payload().starts_with(b"SEDS_MANUAL_ABORT_V1:")))
                };
                if all && state.abort_indicator_latched() && state.manual_abort_intent.pending_epoch().is_none()
                    && !state.deferred_abort.manual_pending() { break; }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await;
        if stop_delivery.is_err() {
            let packets=fixture.incoming_startup.lock().unwrap();
            eprintln!("ACTIVE_STOP_DELIVERY_FAILURE {}",json!({"host":state.local_flight_state_snapshot() as u8,"running_snapshot":crate::gse::native_owned_startup_snapshot(&state),"manual_pending":state.manual_abort_intent.pending_epoch(),"deferred_pending":state.deferred_abort.manual_pending(),"actual_abort_peer_callbacks":packets.iter().filter(|(_,p)|p.data_type()==crate::telemetry_schema::data_type("ABORT")).map(|(b,p)|(b.sender_id(),p.packet_id())).collect::<Vec<_>>()}));
        }
        stop_delivery.expect("actual authorized Stop must reach all three peer SDK callbacks and deferred host owner");
        // Explicit peer model boundary: only AFTER actual original Stop delivery do peers report Aborted.
        for (board,model) in &fixture.models {
            if matches!(board, Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer) {
                model.lock().unwrap().state = FlightState::Aborted as u8;
            }
        }
        for board in BOARDS { fixture.query(board).await; }
        for board in [Board::ActuatorBoard,Board::ValveBoard,Board::FlightComputer] {
            assert_eq!(state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()].as_ref().unwrap().value.state,Some(15));
        }
        let after = state.local_flight_state_snapshot();
        eprintln!("ACTIVE_STARTUP_AFTER {}",crate::gse::native_owned_startup_snapshot(&state));
        let response = super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),
            json!({"action":"reset_startup","boards":BOARDS.iter().map(|b|b.sender_id()).collect::<Vec<_>>(),"board":null,"mode":null,
                "backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
        let status = response.status(); let reply=body(response).await;
        eprintln!("POSTMISSION_FLOW {}",json!({"initial":initial as u8,"after_actual_stop":after as u8,
            "manual_epoch":state.manual_abort_intent.epoch(),"stop_indicator":state.abort_indicator_latched(),
            "fresh_peer_stop_states":[15,15,15],"prepare_status":status.as_u16(),"prepare_reply":reply,
            "scope":"actualHTTP/auth/SDK wire/manual publisher/deferred host/queries; peer application states are modeled, no physical proof"}));
        observed.push((initial as u8,after as u8,status.as_u16()));
        if status == StatusCode::OK {
            assert_eq!(after, FlightState::Aborted, "only admitted local Stop owns this host phase");
            assert_eq!(fixture.confirm(&reply).await.status(),StatusCode::OK);
            let terminal=fixture.terminal().await;
            assert_eq!(terminal["success"],true,"{terminal}");
            assert_eq!(terminal["reset_applied_state"],0);
            assert!(fixture.models.iter().all(|(_,m)|m.lock().unwrap().state==0));
            eprintln!("POSTMISSION_RESET_APPLIED {}",terminal);
        }

        publisher.abort(); let _=publisher.await;
        let _=drain.send(());
        // Drop the fixture first: it checks that its wire/receiver workers are
        // still alive, and the shutdown requested below ends the receiver.
        drop(fixture);
        state.request_shutdown(); let _=deferred.await;
    }
    assert!(observed.iter().all(|(_,after,status)|*after==15&&*status==200),"explicit Stop postmission reset remains unavailable: {observed:?}");
}
