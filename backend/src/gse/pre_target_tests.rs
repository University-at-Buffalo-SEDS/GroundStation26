use super::*;
use sedsnet::packet::Packet;

async fn exercise(cached: bool, test_name: &str) {
    const CHILD: &str = "GS_PRE_TARGET_CONTROLLER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let path = std::env::temp_dir().join(format!(
            "gs-pre-target-{}-{}.clock",
            std::process::id(),
            cached
        ));
        std::fs::write(&path, "1000").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("GS_SIM_CLOCK_FILE", &path)
            .output()
            .unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let (state, sent, peer) = tests::button_test_state().await;
    state
        .gse
        .lock()
        .unwrap()
        .engine
        .config
        .dry_self_test_confirmed = true;
    let packet = |source, nonce, value| {
        Packet::new_with_nonce(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "AB",
            source,
            nonce,
            Arc::from([9, value]),
        )
        .unwrap()
    };
    let mut old = Some(
        crate::telemetry_task::capture_umbilical_status_at_ingress(&state, &packet(10, 1, 0))
            .unwrap(),
    );
    if cached {
        crate::telemetry_task::apply_captured_umbilical_status(&state, old.take().unwrap())
            .unwrap();
        assert_eq!(
            state.gse.lock().unwrap().valves[3].map(|v| (v.open, v.at_ms)),
            Some((false, 1000))
        );
    }
    assert!(handle_command(
        &state,
        &if cached {
            TelemetryCommand::NitrogenTest
        } else {
            TelemetryCommand::ValveSelfTest
        }
    ));
    tests::pump_test_link(&state, &peer);
    let first = sent.lock().unwrap().clone();
    assert_eq!(first, vec![("ACTUATOR_COMMAND".into(), vec![12 | 0x80])]);
    if !cached {
        crate::telemetry_task::apply_captured_umbilical_status(&state, old.take().unwrap())
            .unwrap();
    }
    let evidence = {
        let rt = state.gse.lock().unwrap();
        (
            rt.valves[3].map(|v| (v.open, v.at_ms)),
            rt.last_valve_captures[3],
        )
    };
    if !cached {
        assert_eq!(evidence.1.source_ms, 10);
        assert_eq!(
            evidence.1.seen_values, 3,
            "pre-target tick retained only as a non-actionable replay floor"
        );
    }
    tick(&state, true);
    tests::pump_test_link(&state, &peer);
    eprintln!(
        "ACTUAL_PRE_TARGET_CONTROLLER cached={cached}: evidence={evidence:?}, commands={:?}",
        *sent.lock().unwrap()
    );
    assert_eq!(
        *sent.lock().unwrap(),
        first,
        "pre-target evidence must not advance the actual controller"
    );
    assert_eq!(
        evidence.0,
        if cached { Some((false, 1000)) } else { None },
        "retained safety observations must not be overwritten by rejected evidence"
    );
    assert_eq!(state.get_pending_umbilical_valve_state(9), Some(false));
    let clock = std::env::var_os("GS_SIM_CLOCK_FILE").unwrap();
    std::fs::write(&clock, "1001").unwrap();
    for (nonce, value) in [(2, 0), (3, 1)] {
        let capture = crate::telemetry_task::capture_umbilical_status_at_ingress(
            &state,
            &packet(10, nonce, value),
        )
        .unwrap();
        crate::telemetry_task::apply_captured_umbilical_status(&state, capture).unwrap();
    }
    tick(&state, true);
    tests::pump_test_link(&state, &peer);
    assert_eq!(
        *sent.lock().unwrap(),
        first,
        "re-nonced/opposite rejected source tick must remain non-actionable"
    );
    let capture =
        crate::telemetry_task::capture_umbilical_status_at_ingress(&state, &packet(11, 4, 0))
            .unwrap();
    crate::telemetry_task::apply_captured_umbilical_status(&state, capture).unwrap();
    assert_eq!(state.get_pending_umbilical_valve_state(9), None);
    tick(&state, true);
    tests::pump_test_link(&state, &peer);
    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            ("ACTUATOR_COMMAND".into(), vec![12 | 0x80]),
            ("ACTUATOR_COMMAND".into(), vec![13 | 0x80])
        ],
        "genuinely fresh post-target source evidence advances once"
    );
}

#[tokio::test]
async fn cached_same_tick_capture_cannot_advance_new_controller_command() {
    crate::mode_suite!(Hitl);
    exercise(
        true,
        "gse::pre_target_tests::cached_same_tick_capture_cannot_advance_new_controller_command",
    )
    .await;
}

#[tokio::test]
async fn delayed_same_tick_capture_cannot_advance_new_controller_command() {
    crate::mode_suite!(Hitl);
    exercise(
        false,
        "gse::pre_target_tests::delayed_same_tick_capture_cannot_advance_new_controller_command",
    )
    .await;
}
