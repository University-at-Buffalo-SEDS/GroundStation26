//! The absolute nitrous mass window: Launch and retraction admission, saved
//! nitrous targets and Start fill.
use super::*;
use crate::station_mode::Mode;

const MODES: [Mode; 2] = [Mode::TestFire, Mode::IntegratedGroundTest];

fn set_target(state: &AppState, target: f32) {
    let mut targets = state.fill_targets.lock().unwrap().clone();
    targets.nitrous.target_mass_kg = target;
    state.set_fill_targets(targets);
}

/// The real gate and the commands it admits, with every other input fresh.
async fn armed(target: f32) -> (Arc<AppState>, std::path::PathBuf) {
    let state = crate::state::tests::test_app_state().await;
    set_target(&state, target);
    let router = Arc::new(sedsnet::router::Router::new(
        sedsnet::router::RouterConfig::new([]),
    ));
    let side = router.add_side_packed("simulated_fill", |_| Ok(()));
    let announce = sedsnet::discovery::build_discovery_announce(
        "GB",
        123,
        &[crate::telemetry_schema::endpoint("VALVE_BOARD")],
    )
    .unwrap();
    router
        .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), side)
        .unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    let path = armed_fixture_for_test(&state, &router);
    let mut policy = state.action_policy_snapshot();
    policy.key_enabled = true;
    policy.software_buttons_enabled = true;
    state.set_action_policy(policy);
    // Held sequence clock: no reading ages while a slow host runs the test.
    {
        let mut rt = state.gse.lock().unwrap();
        rt.test_now_ms = Some(rt.now());
    }
    (state, path)
}

fn gate(state: &AppState, mass: Option<Sample>) -> bool {
    state.gse.lock().unwrap().mass = mass;
    let allowed = operational_launch_allowed(state, &state.gse.lock().unwrap());
    for cmd in [TelemetryCommand::Launch, TelemetryCommand::RetractPlumbing] {
        assert_eq!(command_allowed(state, &cmd), Some(allowed), "{cmd:?}");
    }
    allowed
}

fn at(state: &AppState, value: f32, age_ms: u64) -> Option<Sample> {
    let now = state.gse.lock().unwrap().now();
    Some(Sample { value, at_ms: now - age_ms })
}

#[tokio::test]
async fn launch_and_retraction_accept_the_absolute_window_for_any_target() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        for target in [5.0, 8.0, 8.5, -8.0] {
            let sign = f32::signum(target);
            let (state, path) = armed(target).await;
            for mass in [4.0, 4.01, 5.08, 7.5, 8.5, 8.99, 9.0] {
                let sample = at(&state, mass * sign, 0);
                assert!(gate(&state, sample), "{mode:?} {mass} at {target}");
            }
            for mass in [3.99, 9.01, 0.0, -5.0, f32::NAN, f32::INFINITY] {
                let sample = at(&state, mass * sign, 0);
                assert!(!gate(&state, sample), "{mode:?} {mass} at {target}");
            }
            // Fresh mass is still required.
            let (fresh, stale) = (at(&state, target, 2_000), at(&state, target, 2_001));
            assert!(gate(&state, fresh));
            assert!(!gate(&state, stale));
            assert!(!gate(&state, None));
            // So is a completed fill to the current target.
            let sample = at(&state, target, 0);
            assert!(gate(&state, sample));
            state.gse.lock().unwrap().operational.saved.fill_target_kg = Some(target + 0.1);
            assert!(!gate(&state, sample));
            {
                let mut rt = state.gse.lock().unwrap();
                rt.operational.saved.fill_target_kg = Some(target);
                rt.operational.saved.qualification.invalidate_fill().unwrap();
            }
            assert!(!gate(&state, sample), "the FillTarget milestone is required");
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tokio::test]
async fn launch_uses_the_configured_window_and_an_invalid_one_refuses() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::TestFire);
    let (state, path) = armed(8.0).await;
    let window = |min, max| {
        let mut rt = state.gse.lock().unwrap();
        rt.engine.config.launch_mass_min_kg = min;
        rt.engine.config.launch_mass_max_kg = max;
    };
    let sample = at(&state, 9.4, 0);
    assert!(!gate(&state, sample));
    window(6.0, 9.5);
    assert!(gate(&state, sample));
    let low = at(&state, 5.9, 0);
    assert!(!gate(&state, low));
    let inside = at(&state, 8.0, 0);
    for (min, max) in [(9.0, 4.0), (0.0, 9.0), (f32::NAN, 9.0), (4.0, f32::NAN)] {
        window(min, max);
        assert!(!gate(&state, inside), "{min}..{max}");
    }
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn a_launch_refused_for_mass_names_the_window_and_the_mass() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        for (target, mass, shown) in [(8.0, 9.2, "9.20"), (5.0, 3.5, "3.50"), (-8.0, 9.2, "9.20")] {
            let (state, path) = armed(target).await;
            let sample = at(&state, mass * f32::signum(target), 0);
            assert!(!gate(&state, sample));
            let reason = format!("Nitrous mass {shown} kg is outside the 4 to 9 kg Launch window");
            let mut policy = state.action_policy_snapshot();
            decorate_policy(&state, &mut policy);
            for cmd in [TelemetryCommand::Launch, TelemetryCommand::RetractPlumbing] {
                assert_eq!(command_refusal_reason(&state, &cmd).as_deref(), Some(reason.as_str()), "{mode:?} {cmd:?}");
                let name = crate::sequences::command_name(&cmd);
                let control = policy.controls.iter().find(|c| c.cmd == name).unwrap();
                assert!(!control.enabled);
                assert_eq!(control.disabled_reason.as_deref(), Some(reason.as_str()), "{mode:?} {name}");
            }
            // Inside the window the mass is not named (nothing else blocks here).
            let sample = at(&state, target, 0);
            assert!(gate(&state, sample));
            let mut policy = state.action_policy_snapshot();
            decorate_policy(&state, &mut policy);
            assert!(policy.controls.iter().filter(|c| c.cmd == "Launch" || c.cmd == "RetractPlumbing").all(|c| c.disabled_reason.is_none()));
            // A stale mass refuses without claiming a mass value.
            let sample = at(&state, mass * f32::signum(target), 2_001);
            assert!(!gate(&state, sample));
            assert_eq!(launch_mass_refusal(&state, &state.gse.lock().unwrap()), None);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// HITL has no operational Launch gate (qualification, checklists, fresh mass,
/// pressure, closed valves): the window is skipped with the rest of it.
#[tokio::test]
async fn hitl_skips_the_window_with_the_rest_of_the_operational_launch_gate() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Hitl);
    let (state, path) = armed(8.0).await;
    // Whatever HITL's own Launch rules answer here, the mass never changes it.
    let view = |state: &AppState| {
        let mut policy = state.action_policy_snapshot();
        decorate_policy(state, &mut policy);
        [TelemetryCommand::Launch, TelemetryCommand::RetractPlumbing].map(|cmd| {
            let name = crate::sequences::command_name(&cmd);
            let control = policy.controls.iter().find(|c| c.cmd == name).map(|c| (c.enabled, c.disabled_reason.clone()));
            (command_allowed(state, &cmd), state.is_command_allowed(&cmd), control)
        })
    };
    let sample = at(&state, 8.0, 0);
    state.gse.lock().unwrap().mass = sample;
    let inside = view(&state);
    for mass in [12.0, 3.0, f32::NAN] {
        let sample = at(&state, mass, 0);
        state.gse.lock().unwrap().mass = sample;
        assert_eq!(view(&state), inside, "{mass}");
        assert_eq!(launch_mass_refusal(&state, &state.gse.lock().unwrap()), None);
    }
    let _ = std::fs::remove_file(path);
    drop(_mode);
    // The same fixture outside HITL: the mass changes the answer.
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::TestFire);
    let (state, path) = armed(8.0).await;
    let sample = at(&state, 8.0, 0);
    assert!(gate(&state, sample));
    let sample = at(&state, 12.0, 0);
    assert!(!gate(&state, sample));
    let _ = std::fs::remove_file(path);
}

/// The Testing simulation fills to the configured target. The calibration's
/// default full mass (10 kg) is outside the window, so a target taken from it
/// could never Launch.
#[tokio::test]
async fn the_testing_simulation_fills_to_the_configured_target() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Testing);
    let _sim = crate::flight_sim::test_support::thread_sim(true);
    let state = crate::state::tests::test_app_state().await;
    set_target(&state, 8.0);
    state.loadcell_calibration.lock().unwrap().full_mass_kg =
        Some(crate::loadcell::DEFAULT_FULL_MASS_KG);
    let config = state.gse.lock().unwrap().engine.config.clone();
    assert!(!config.launch_mass_ready(10.0, 10.0));
    for phase in [FlightState::NitrousFill, FlightState::Armed] {
        *state.state.lock().unwrap() = phase;
        let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
        assert_eq!(target, 8.0, "{phase:?}");
        assert!(config.launch_mass_ready(target + config.fill_tolerance_kg, target));
    }
}
