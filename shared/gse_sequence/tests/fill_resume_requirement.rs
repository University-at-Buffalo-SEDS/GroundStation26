use gse_sequence::{Baseline, CLOSED, Engine, Inputs, Phase, Sample, ValveState};

fn paused() -> (Engine, Inputs) {
    let mut engine = Engine::default();
    engine.config.nitrogen_target_psi = 120.0; // Preserve this low-pressure test fixture.
    engine.config.pressure_ceiling_psi = Some(200.0);
    engine.config.maximum_zero_offset_psi = Some(8.0);
    engine.nitrous_target_psi = 140.0;
    engine.status.phase = Phase::Paused;
    engine.status.nitrogen_passed = true;
    engine.status.baseline = Some(Baseline {
        average_psi: 5.0,
        min_psi: 5.0,
        max_psi: 5.0,
        noise_psi: 1.0,
        samples: 10,
    });
    let input = Inputs {
        now_ms: 100,
        pressure: Some(Sample {
            value: 5.0,
            at_ms: 100,
        }),
        valves: [Some(ValveState {
            open: false,
            at_ms: 100,
        }); 5],
        interlock: true,
        prelaunch: true,
    };
    (engine, input)
}

#[test]
fn near_at_and_above_target_explicit_fill_is_inert() {
    for mass in [9.9_f32, 10.0, 10.1] {
        let (mut engine, input) = paused();
        // The mass-aware entrypoint receives the selected calibrated kg sample.
        let result = engine
            .request_fill(
                input,
                Some(Sample {
                    value: mass,
                    at_ms: 100,
                }),
                10.0,
            )
            .unwrap();
        assert!(
            matches!(result, gse_sequence::fill_resume::FillStart::Noop),
            "mass={mass}, configured target=10kg"
        );
        assert_eq!(engine.status.phase, Phase::Paused);
        assert_eq!(engine.status.valves, [None; 5]);
    }
}

#[test]
fn existing_explicit_fill_positive_has_real_valve_effects() {
    let (mut engine, input) = paused();
    let result = engine
        .request_fill(
            input,
            Some(Sample {
                value: 9.0,
                at_ms: 100,
            }),
            10.0,
        )
        .unwrap();
    let gse_sequence::fill_resume::FillStart::Started(effects) = result else {
        panic!("expected explicit start")
    };
    assert!(!effects.valves.is_empty());
    assert_eq!(engine.status.phase, Phase::FillSetup);
    assert_eq!(CLOSED, [false; 5]);
}

#[test]
fn stale_pressure_remains_a_refusal() {
    let (mut engine, mut input) = paused();
    input.now_ms = 2101;
    assert!(
        engine
            .request_fill(
                input,
                Some(Sample {
                    value: 10.0,
                    at_ms: 2101
                }),
                10.0
            )
            .is_err()
    );
    assert_eq!(engine.status.phase, Phase::Paused);
}

#[test]
fn exact_boundary_resumes_and_next_mass_above_is_inert_in_both_directions() {
    for sign in [1.0_f32, -1.0] {
        let target = 10.0 * sign;
        let boundary = 10.0 - gse_sequence::fill_resume::RESUME_DEFICIT_KG;
        for (magnitude, starts) in [
            (boundary.next_down(), true),
            (boundary, true),
            (boundary.next_up(), false),
        ] {
            let (mut engine, input) = paused();
            let result = engine
                .request_fill(
                    input,
                    Some(Sample {
                        value: magnitude * sign,
                        at_ms: 100,
                    }),
                    target,
                )
                .unwrap();
            assert_eq!(
                matches!(result, gse_sequence::fill_resume::FillStart::Started(_)),
                starts,
                "target={target}, mass={}",
                magnitude * sign
            );
            assert_eq!(
                engine.status.phase,
                if starts {
                    Phase::FillSetup
                } else {
                    Phase::Paused
                }
            );
        }
    }
}

#[test]
fn missing_invalid_stale_future_mass_or_target_never_becomes_a_noop() {
    for mass in [
        None,
        Some(Sample {
            value: f32::NAN,
            at_ms: 100,
        }),
        Some(Sample {
            value: f32::INFINITY,
            at_ms: 100,
        }),
        Some(Sample {
            value: 10.0,
            at_ms: 101,
        }),
    ] {
        let (mut engine, input) = paused();
        assert!(engine.request_fill(input, mass, 10.0).is_err());
        assert_eq!(engine.status.phase, Phase::Paused);
    }
    let (mut engine, mut input) = paused();
    input.now_ms = 2101;
    input.pressure.as_mut().unwrap().at_ms = 2101;
    assert!(
        engine
            .request_fill(
                input,
                Some(Sample {
                    value: 10.0,
                    at_ms: 100
                }),
                10.0
            )
            .is_err()
    );
    for target in [f32::NAN, f32::INFINITY, 0.0] {
        let (mut engine, input) = paused();
        assert!(
            engine
                .request_fill(
                    input,
                    Some(Sample {
                        value: 10.0,
                        at_ms: 100
                    }),
                    target
                )
                .is_err()
        );
    }
}

#[test]
fn no_op_cannot_bypass_pressure_state_or_key_and_ticks_never_resume() {
    for blocked in 0..3 {
        let (mut engine, mut input) = paused();
        match blocked {
            0 => input.pressure.as_mut().unwrap().value = 190.0,
            1 => input.prelaunch = false,
            _ => input.interlock = false,
        }
        assert!(
            engine
                .request_fill(
                    input,
                    Some(Sample {
                        value: 10.0,
                        at_ms: 100
                    }),
                    10.0
                )
                .is_err()
        );
        assert_eq!(engine.status.phase, Phase::Paused);
    }
    let (mut engine, mut input) = paused();
    assert!(matches!(
        engine
            .request_fill(
                input,
                Some(Sample {
                    value: 10.0,
                    at_ms: 100
                }),
                10.0
            )
            .unwrap(),
        gse_sequence::fill_resume::FillStart::Noop
    ));
    for now in [200, 400, 1000] {
        input.now_ms = now;
        input.pressure.as_mut().unwrap().at_ms = now;
        for valve in input.valves.iter_mut().flatten() {
            valve.at_ms = now;
        }
        let effects = engine.tick(input);
        assert!(effects.valves.is_empty());
        assert_eq!(engine.status.phase, Phase::Paused);
    }
}

#[test]
fn little_endian_wire_f32_boundary_uses_no_epsilon_or_target_replacement() {
    for sign in [1.0_f32, -1.0] {
        let target = f32::from_le_bytes((10.0_f32 * sign).to_le_bytes());
        let boundary = target.abs() - gse_sequence::fill_resume::RESUME_DEFICIT_KG;
        for (wire_mass, starts) in [(boundary, true), (boundary.next_up(), false), (10.1, false)] {
            let mass = f32::from_le_bytes((wire_mass * sign).to_le_bytes());
            let (mut engine, input) = paused();
            let original = target.to_bits();
            let result = engine
                .request_fill(
                    input,
                    Some(Sample {
                        value: mass,
                        at_ms: 100,
                    }),
                    target,
                )
                .unwrap();
            assert_eq!(
                matches!(result, gse_sequence::fill_resume::FillStart::Started(_)),
                starts
            );
            assert_eq!(target.to_bits(), original);
        }
    }
}
