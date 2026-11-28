#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum FlightComputerCommands {
    Postinit = 0,
    Launch = 1,
    // Bytes 2..11 are retired: the FC's runtime options and their revokes
    // (MonitorAltitude 2/3, ConsecutiveSamples 4/5, ResetFailures 6/7,
    // ValidateMeasms 8/9, velocity checks 10/11, which the GS never sent).
    // The FC keeps its compiled defaults and refuses them. Never reuse them.
    /// Deploy drogue (FC `Deploy_Parachute`).
    DeployParachute = 12,
    /// Deploy main / reef release (FC `Expand_Parachute`).
    ExpandParachute = 13,
    // Bytes 14 and 15 (EvaluationRelax, EvaluationFocus) are retired: the
    // FC's estimator guard made them meaningless and the FC refuses them.
    // Never reuse them for another command.
    // Byte 16 (EvaluationAbort) is retired: Reset all boards (the FC's reset
    // to Startup) cancels the FC's evaluation. The FC refuses it. Never reuse.
    ReinitSensors = 17,
    ReinitBarometer = 18,
    /// Re-initialize the IMU (FC `Reinit_IMU`; formerly named EnableIMU).
    ReinitIMU = 19,
    DisableIMU = 20,
    AdvanceFlightState = 21,
    RewindFlightState = 22,
    // Bytes 23..28 are retired: the FC's failure thresholds (AbortAfter40,
    // AbortAfter100, AbortAfter250, ReinitAfter15, ReinitAfter30,
    // ReinitAfter50). The FC keeps its compiled thresholds and refuses them.
    // Never reuse them.
    // Integrated ground test (FC fc_msg Actionable_Decrees + 17..20).
    IgtEnter = 29,
    IgtLivePyro = 30,
    IgtDryRun = 31,
    IgtExit = 32,
    /// Re-attempt the SD mount and log file (FC `SD_Retry`, both images).
    FcSdRetry = 33,
    /// Kalman: stop (FC `Kalman_Stop`): stop an evaluation no station Launch
    /// started (a false launch detection) without changing the global state.
    KalmanStop = 34,
    /// Kalman: reset (FC `Kalman_Reset`): clear the stopped estimator's memory.
    KalmanReset = 35,
    /// Kalman: start (FC `Kalman_Start`), HITL only: start flight evaluation.
    KalmanStart = 36,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum ValveBoardCommands {
    PilotOpen = 0,
    NormallyOpenOpen = 1,
    DumpOpen = 2,
    PilotClose = 3,
    NormallyOpenClose = 4,
    DumpClose = 5,
    #[allow(unused)]
    Sequence = 6,
}
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum ActuatorBoardCommands {
    IgniterOn = 7,
    RetractPlumbing = 8,
    NitrogenOpen = 9,
    NitrousOpen = 10,
    IgniterOff = 11,
    NitrogenClose = 12,
    NitrousClose = 13,
    #[allow(unused)]
    IgniterSequence = 14,
    /// Automatic only (always sent with the 0x80 origin tag): tank
    /// overpressure before Launch. The Actuator Board enters its Abort safe
    /// state; it ignores the command after Launch. Sent by the station and by
    /// the Valve Board at its own relief.
    OverpressureAbort = 16,
}

#[cfg(test)]
mod flight_command_wire_tests {
    use super::FlightComputerCommands::*;

    #[test]
    fn existing_command_bytes_match_the_captured_fc_compatibility_table() {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "test-fixtures/fc-global-state-contract.json"
        )).unwrap();
        let mut cases = [
            (Postinit, "Compat_Postinit_Signal"),
            (Launch, "Compat_Launch_Signal"),
            (DeployParachute, "Compat_Deploy_Parachute"),
            (ExpandParachute, "Compat_Expand_Parachute"),
            (ReinitSensors, "Compat_Reinit_Sensors"),
            (ReinitBarometer, "Compat_Reinit_Barometer"),
            (ReinitIMU, "Compat_Reinit_IMU"),
            (DisableIMU, "Compat_Disable_IMU"),
        ]
        .to_vec();
        if crate::runtime_mode::is_hitl() {
            cases.extend([
                (AdvanceFlightState, "Compat_Advance_State"),
                (RewindFlightState, "Compat_Rewind_State"),
            ]);
        }
        cases.extend([
            (IgtEnter, "Compat_IGT_Enter"),
            (IgtLivePyro, "Compat_IGT_Live_Pyro"),
            (IgtDryRun, "Compat_IGT_Dry_Run"),
            (IgtExit, "Compat_IGT_Exit"),
            (FcSdRetry, "Compat_SD_Retry"),
            (KalmanStop, "Compat_Kalman_Stop"),
            (KalmanReset, "Compat_Kalman_Reset"),
            (KalmanStart, "Compat_Kalman_Start"),
        ]);
        for (command, expected) in cases {
            assert_eq!(contract["remote_commands_compat"][command as usize], expected);
        }
        // Retired bytes stay reserved on the FC and unused by the GS.
        let retired = [
            (2, "Compat_Retired_Vigilant_Mode"),
            (3, "Compat_Retired_Revoke_Vigilant_Mode"),
            (4, "Compat_Retired_Eval_Successive"),
            (5, "Compat_Retired_Revoke_Eval_Successive"),
            (6, "Compat_Retired_Reset_Failures"),
            (7, "Compat_Retired_Revoke_Reset_Failures"),
            (8, "Compat_Retired_Measm_Reports"),
            (9, "Compat_Retired_Revoke_Measm_Reports"),
            (10, "Compat_Retired_Velocity_Checks"),
            (11, "Compat_Retired_Revoke_Velocity_Checks"),
            (14, "Compat_Retired_Evaluation_Relax"),
            (15, "Compat_Retired_Evaluation_Focus"),
            (16, "Compat_Retired_Evaluation_Abort"),
            (23, "Compat_Retired_Abort_After_40"),
            (24, "Compat_Retired_Abort_After_100"),
            (25, "Compat_Retired_Abort_After_250"),
            (26, "Compat_Retired_Reinit_After_15"),
            (27, "Compat_Retired_Reinit_After_30"),
            (28, "Compat_Retired_Reinit_After_50"),
        ];
        for (byte, expected) in retired {
            assert_eq!(contract["remote_commands_compat"][byte], expected);
        }
        assert_eq!(contract["remote_commands_compat"].as_array().unwrap().len(), 37);
    }
}
