// Actual scalar-model implementation checks; no board/hardware authority.
use super::*;
#[test]
fn automatic_prelaunch_projection_cannot_create_fc_packets_or_board_epochs() {
    crate::mode_suite!(Testing);
    crate::telemetry_schema::initialize().unwrap();
    let local = Mutex::new(FlightSimState::new());
    {
        let mut model = local.lock().unwrap();
        model.queued.clear();
    }
    let before = (!crate::runtime_mode::is_hitl_or_test_fire())
        .then(|| local.lock().unwrap().recovery.publication_state());
    for target in [
        FlightState::Idle,
        FlightState::PreFill,
        FlightState::NitrogenFill,
        FlightState::FillTest,
        FlightState::NitrousFill,
        FlightState::Armed,
    ] {
        {
            let mut guard = AutomaticPhaseGuard {
                model: Some(local.lock().unwrap()),
                target,
            };
            guard.admitted();
        }
        let mut model = local.lock().unwrap();
        assert_eq!(model.flight_state, target);
        assert!(model.automatic_idle_volatile);
        assert!(model.queued.is_empty());
        model.queue_flight_state(1_793_000_000_000);
        assert!(
            model.queued.is_empty(),
            "GS-derived automatic phase must not remint FC authority on periodic source path"
        );
        if let Some(before) = before {
            assert_eq!(
                model.recovery.publication_state(),
                before,
                "GS projection cannot advance actual modeled FC owner"
            );
        }
    }
}
#[test]
fn actual_explicit_model_transition_retires_automatic_provenance() {
    crate::mode_suite!(Testing);
    crate::telemetry_schema::initialize().unwrap();
    let mut model = FlightSimState::new();
    if crate::runtime_mode::is_testing() {
        // This is a NEW local unit-model boot with no retained host holds.
        // Same initializer used by initialize_boot(0,0,0,0), not a flag clear,
        // selected-Dummy substitute or independent hardware/NVM proof.
        model.recovery.initialize(0, 0, 0);
        assert_eq!(model.recovery.publication_state(), Some(FlightState::Startup as u8));
        assert!(model.queued.is_empty(), "owner initialization cannot inject an FC report");
    }
    model.automatic_idle_volatile = true;
    // Existing explicit/model-owned transition function, not a new Launch
    // bypass: this unit control does not qualify a physical output/Launch gate.
    model.set_flight_state(FlightState::Ascent, 1_793_000_000_000);
    assert!(!model.automatic_idle_volatile);
    assert_eq!(model.flight_state, FlightState::Ascent);
    assert!(
        model
            .queued
            .iter()
            .any(|p| p.packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE"))
    );
}
