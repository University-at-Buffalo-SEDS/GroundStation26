//! Proposed exact-model counterexamples; compile only after source/base approval.
//! These are scalar/application-model checks, not SDK/hardware qualification.
use super::*;
#[test]
fn nitrogen_topup_cannot_erase_already_loaded_nitrous_mass() {
    crate::mode_suite!(Testing);
    crate::telemetry_schema::initialize().unwrap();
    let mut model = FlightSimState::new();
    let nitrogen_target = sim_nitrogen_target_mass_kg();
    let loaded_mass = nitrogen_target + 8.0;
    model.loadcell_mass_kg = loaded_mass;
    model.fuel_tank_pressure_psi = 745.0;
    model.last_physics_ms = 1000;
    model
        .valves
        .insert(ValveBoardCommands::DumpOpen as u8, false);
    model
        .valves
        .insert(ValveBoardCommands::NormallyOpenOpen as u8, false);
    model
        .valves
        .insert(ActuatorBoardCommands::NitrogenOpen as u8, true);
    model
        .valves
        .insert(ActuatorBoardCommands::NitrousOpen as u8, false);
    model.update_physics(1100);
    assert!(
        model.loadcell_mass_kg >= loaded_mass,
        "opening nitrogen erased existing mass: before={loaded_mass} after={} N2target={nitrogen_target}",
        model.loadcell_mass_kg
    );
}
#[test]
fn actual_admitted_raw_igniter_sequence_models_original_compensated_timeline() {
    crate::mode_suite!(Testing);
    crate::telemetry_schema::initialize().unwrap();
    let mut model = FlightSimState::new();
    // Deliberately invokes the same application-effect function after admission;
    // actual SDK/fresh-board/admission gates require separate end-to-end proof.
    model.apply_modeled_packet(
        Board::ActuatorBoard,
        crate::testing_board_lifecycle::Kind::Actuator,
        14,
        10_000,
    );
    model.update_physics(14_249);
    assert!(!model.valve_on(ActuatorBoardCommands::IgniterOn as u8));
    model.update_physics(14_250);
    assert!(
        model.valve_on(ActuatorBoardCommands::IgniterOn as u8),
        "accepted raw14 has no model effect at originalUTC+4250"
    );
    model.update_physics(24_249);
    assert!(model.valve_on(ActuatorBoardCommands::IgniterOn as u8));
    model.update_physics(24_250);
    assert!(!model.valve_on(ActuatorBoardCommands::IgniterOn as u8));
    model.update_physics(14250);
    assert!(
        !model.valve_on(ActuatorBoardCommands::IgniterOn as u8),
        "completed pulse cannot restart after modelclock rollback"
    );
}
#[test]
fn equal_clock_multisensor_polls_cannot_advance_tank_physics() {
    crate::mode_suite!(Testing);
    crate::telemetry_schema::initialize().unwrap();
    let mut model = FlightSimState::new();
    model.loadcell_mass_kg = 0.25;
    model.fuel_tank_pressure_psi = 0.0;
    model.last_physics_ms = 1000;
    model
        .valves
        .insert(ValveBoardCommands::DumpOpen as u8, false);
    model
        .valves
        .insert(ValveBoardCommands::NormallyOpenOpen as u8, false);
    model
        .valves
        .insert(ActuatorBoardCommands::NitrogenOpen as u8, true);
    model
        .valves
        .insert(ActuatorBoardCommands::NitrousOpen as u8, false);
    let before = (model.loadcell_mass_kg, model.fuel_tank_pressure_psi);
    for _ in 0..20 {
        model.update_physics(1000);
    }
    assert_eq!(
        (model.loadcell_mass_kg, model.fuel_tank_pressure_psi),
        before,
        "zero elapsed clock still changed physics due to sensor polling"
    );
}

#[test]
fn a_delayed_command_cannot_backfill_flow_before_model_receipt() {

    crate::mode_suite!(Testing);
    crate::telemetry_schema::initialize().unwrap();
    let mut m = FlightSimState::new();
    m.valves.insert(1, false);
    m.valves.insert(2, false);
    m.update_ground_plant(1000);
    // SourceUTC1001 is old but admitted within the existing5s freshness bound;
    // physical effect occurs at model receipt2000, not during the preceding1s.
    m.apply_modeled_packet_at(
        Board::ActuatorBoard,
        crate::testing_board_lifecycle::Kind::Actuator,
        9,
        1001,
        2000,
        2000,
    );
    assert_eq!(m.loadcell_mass_kg, 0.0);
    assert!(m.fuel_tank_pressure_psi < 0.001);
    m.update_ground_plant(2500);
    let pressure = m.fuel_tank_pressure_psi;
    assert!(pressure > 9.9 && pressure < 10.1);
    m.stop_modeled_outputs_at(Board::ActuatorBoard, 2500, 2500);
    m.update_ground_plant(3000);
    assert!((m.fuel_tank_pressure_psi - pressure).abs() < 0.001);
}
