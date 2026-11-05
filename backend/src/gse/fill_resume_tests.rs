//! Actual GSE handler and public SDK command-owner controls; no HTTP claim here.
use super::*;

async fn paused(
    mass: f32,
) -> (
    Arc<AppState>,
    tests::TestCommands,
    Arc<sedsnet::router::Router>,
) {
    let (state, sent, peer) = tests::button_test_state().await;
    let mut policy = state.action_policy_snapshot();
    policy.key_enabled = true;
    policy.software_buttons_enabled = true;
    state.set_action_policy(policy);
    let target = state.fill_targets_snapshot().nitrous;
    {
        let mut rt = state.gse.lock().unwrap();
        rt.engine.nitrous_target_psi = target.target_pressure_psi;
        rt.engine.status.phase = Phase::Paused;
        rt.engine.status.nitrogen_passed = true;
        rt.engine.status.baseline = Some(gse_sequence::Baseline {
            average_psi: 0.,
            min_psi: 0.,
            max_psi: 0.,
            noise_psi: 0.1,
            samples: 50,
        });
        rt.observe_pressure(Some(0.));
        rt.observe_mass(Some(mass));
        #[cfg(not(feature = "hitl_mode"))]
        {
            rt.operational.startup_configured = true;
            rt.operational.topology_stable = true;
            let generation = rt.operational.saved.qualification.generation();
            rt.operational
                .saved
                .qualification
                .complete(Milestone::SelfTest, generation)
                .unwrap();
            rt.operational
                .saved
                .qualification
                .complete(Milestone::NitrogenTest, generation)
                .unwrap();
            rt.operational
                .saved
                .qualification
                .complete(Milestone::FillTarget, generation)
                .unwrap();
        }
    }
    (state, sent, peer)
}

#[tokio::test]
async fn current_near_target_fill_is_inert_with_paused_policy_illumination() {
    let temporary = crate::state::tests::test_app_state().await;
    let target = temporary.fill_targets_snapshot().nitrous.target_mass_kg;
    drop(temporary);
    for mass in [
        target,
        target + target.signum() * 0.1,
        target - target.signum() * 0.1,
    ] {
        let (state, sent, peer) = paused(mass).await;
        let generation = automatic_generation(&state);
        // Snapshot every actual Summary field and its existing owner/revision marker.
        // The guard ends before command handling; no production serialization is needed.
        let summary_snapshot = || {
            let rt = state.gse.lock().unwrap();
            (
                rt.progress.current().map(|summary| {
                    (
                        summary.key.clone(),
                        summary.kind,
                        summary.outcome,
                        summary.cancel_allowed,
                    )
                }),
                rt.progress.marker(),
            )
        };
        let prior = summary_snapshot();
        assert!(consume_fill_noop(&state, &TelemetryCommand::StartFill));
        assert!(handle_command(&state, &TelemetryCommand::StartFill));
        tests::pump_test_link(&state, &peer);
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Paused);
        assert_eq!(summary_snapshot(), prior);
        assert_eq!(automatic_generation(&state), generation);
        let mut policy = state.action_policy_snapshot();
        decorate_policy(&state, &mut policy);
        assert_eq!(
            policy
                .controls
                .iter()
                .find(|c| c.cmd == "PauseFill")
                .unwrap()
                .actuated,
            Some(true)
        );
        assert_eq!(
            policy
                .controls
                .iter()
                .find(|c| c.cmd == "StartFill")
                .unwrap()
                .actuated,
            Some(false)
        );
    }
}

#[tokio::test]
async fn no_op_probe_refuses_missing_mass_and_pressure_limit_without_effects() {
    let (state, sent, peer) = paused(0.).await;
    state.gse.lock().unwrap().observe_mass(None);
    assert!(!consume_fill_noop(&state, &TelemetryCommand::StartFill));
    {
        let target = state.fill_targets_snapshot().nitrous;
        let mut rt = state.gse.lock().unwrap();
        rt.observe_mass(Some(target.target_mass_kg));
        rt.observe_pressure(Some(target.target_pressure_psi + 50.));
    }
    assert!(!consume_fill_noop(&state, &TelemetryCommand::StartFill));
    tests::pump_test_link(&state, &peer);
    assert!(sent.lock().unwrap().is_empty());
    assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Paused);
}

#[tokio::test]
async fn current_deficit_at_and_beyond_deadband_starts_actual_engine_and_sdk_command() {
    let temporary = crate::state::tests::test_app_state().await;
    let target = temporary.fill_targets_snapshot().nitrous.target_mass_kg;
    drop(temporary);
    assert!(target.is_finite() && target != 0.0);
    for deficit in [gse_sequence::fill_resume::RESUME_DEFICIT_KG, 0.3] {
        let mass = (target.abs() - deficit) * target.signum();
        let (state, sent, peer) = paused(mass).await;
        #[cfg(not(feature = "hitl_mode"))]
        let directory = crate::recording_export::tests::TestDirectory::new();
        #[cfg(not(feature = "hitl_mode"))]
        {
            // The generic button fixture intentionally has no initialized
            // qualification store. This positive exercises its real save path.
            let mut rt = state.gse.lock().unwrap();
            rt.operational.path = Some(directory.0.join("qualification.json"));
            rt.operational.saved.baseline = rt.engine.status.baseline.clone();
            rt.operational.saved.fill_target_kg = Some(target);
        }
        assert!(!consume_fill_noop(&state, &TelemetryCommand::StartFill));
        assert!(handle_command(&state, &TelemetryCommand::StartFill));
        tests::pump_test_link(&state, &peer);
        let status = state.gse.lock().unwrap().engine.status.clone();
        assert_eq!(status.phase, Phase::FillSetup, "{}", status.message);
        #[cfg(not(feature = "hitl_mode"))]
        assert!(operational::Saved::load(&directory.0.join("qualification.json")).is_ok());
        assert!(
            sent.lock().unwrap().contains(&(
                "ACTUATOR_COMMAND".into(),
                vec![12 | 0x80],
            )),
            "actual SDK must carry the first safe nitrogen-close configuration command before fill"
        );
    }
}

#[tokio::test]
async fn verified_paused_pressurized_inventory_resumes_through_actual_sdk_nitrous_open() {
    let (state,sent,peer)=paused(7.7).await;
    let mut targets=state.fill_targets_snapshot();targets.nitrous.target_mass_kg=8.;targets.nitrous.target_pressure_psi=750.;state.set_fill_targets(targets);
    #[cfg(not(feature="hitl_mode"))]
    let directory=crate::recording_export::tests::TestDirectory::new();
    {
        let mut rt=state.gse.lock().unwrap();rt.test_now_ms=Some(10_000);
        rt.engine.nitrous_target_psi=750.;rt.observe_pressure(Some(0.));
        let seed=rt.input(true,true);rt.engine.request(Action::StartFill,seed).unwrap();
        rt.engine.manual_override();rt.engine.status.phase=Phase::Paused;
        assert!(rt.engine.nitrous_session());rt.observe_mass(Some(7.7));rt.observe_pressure(Some(500.));
        rt.valve_pressure=Some(Sample{value:500.,at_ms:rt.now()});
        #[cfg(not(feature="hitl_mode"))]
        {rt.operational.path=Some(directory.0.join("qualification.json"));rt.operational.saved.baseline=rt.engine.status.baseline.clone();rt.operational.saved.fill_target_kg=Some(8.);}
    }
    // The small command fixture has real routes but deliberately no board
    // heartbeat observations. Supply them explicitly, and establish the real
    // two-second topology continuity before admitting this active fill.
    for board in [crate::types::Board::GatewayBoard,crate::types::Board::ValveBoard,crate::types::Board::ActuatorBoard,crate::types::Board::DaqBoard] {
        state.mark_board_seen(board.sender_id(),crate::telemetry_task::get_current_timestamp_ms());
    }
    #[cfg(not(feature="hitl_mode"))]
    {
        assert!(operational_tick(&state,true));
        {
            let mut rt=state.gse.lock().unwrap();rt.test_now_ms=Some(12_000);
            rt.observe_pressure(Some(500.));rt.observe_mass(Some(7.7));rt.valve_pressure=Some(Sample{value:500.,at_ms:rt.now()});
        }
        assert!(operational_tick(&state,true));
        let rt=state.gse.lock().unwrap();assert!(rt.operational.topology_healthy&&rt.operational.topology_stable);
    }
    #[cfg(feature="hitl_mode")]
    {let mut rt=state.gse.lock().unwrap();rt.test_now_ms=Some(12_000);rt.observe_pressure(Some(500.));rt.observe_mass(Some(7.7));}
    assert!(!consume_fill_noop(&state,&TelemetryCommand::StartFill));
    assert!(handle_command(&state,&TelemetryCommand::StartFill));
    assert_eq!(state.gse.lock().unwrap().engine.status.phase,Phase::FillSetup);
    let mut actual=[false;5];let mut consumed=0;let mut nitrous_seen=false;
    for step in 1..=20 {
        tests::pump_test_link(&state,&peer);
        {
            let commands=sent.lock().unwrap();
            for(kind,bytes)in &commands[consumed..] {
                assert_eq!(bytes.len(),1);assert_ne!(bytes[0]&0x80,0);let code=bytes[0]&0x7f;
                let i=(if kind=="VALVE_COMMAND"{0..3}else{assert_eq!(kind,"ACTUATOR_COMMAND");3..5}).find(|i|code==OPEN[*i]||code==CLOSE[*i]).unwrap();
                let open=code==OPEN[i];assert_ne!((i,open),(3,true),"fill resume must never open nitrogen");
                actual[i]=open;if(i,open)==(4,true){nitrous_seen=true;}
            }consumed=commands.len();
        }
        {
            let mut rt=state.gse.lock().unwrap();rt.test_now_ms=Some(12_000+step*100);
            for(key,open)in KEYS.into_iter().zip(actual){rt.observe_valve(key,open);}
            rt.observe_pressure(Some(500.));rt.observe_mass(Some(7.7));rt.valve_pressure=Some(Sample{value:500.,at_ms:rt.now()});
        }
        if nitrous_seen{break;}
        tick(&state,true);flush_effects(&state);
    }
    let failure_facts = {
        let rt=state.gse.lock().unwrap();
        #[cfg(not(feature="hitl_mode"))]
        let operational=serde_json::json!({"healthy":rt.operational.topology_healthy,"stable":rt.operational.topology_stable,"stage":format!("{:?}",rt.operational.saved.qualification.stage()),"fault":format!("{:?}",rt.operational.saved.qualification.fault()),"message":rt.operational.persist_error});
        #[cfg(feature="hitl_mode")]
        let operational=serde_json::json!({"mode":"hitl"});
        serde_json::json!({"status":rt.engine.status,"operational":operational,"sent":*sent.lock().unwrap()})
    };
    assert!(nitrous_seen,"actual SDK never emitted Nitrous open after safe pressure and valve confirmations: {failure_facts}");
    assert_eq!(state.gse.lock().unwrap().engine.status.phase,Phase::Filling);
    assert!(sent.lock().unwrap().contains(&("ACTUATOR_COMMAND".into(),vec![10|0x80])));
}
