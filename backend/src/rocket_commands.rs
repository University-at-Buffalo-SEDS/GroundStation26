#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum FlightComputerCommands {
    Postinit = 0,
    Launch = 1,
    MonitorAltitude = 2,
    RevokeMonitorAltitude = 3,
    ConsecutiveSamples = 4,
    RevokeConsecutiveSamples = 5,
    ResetFailures = 6,
    RevokeResetFailures = 7,
    ValidateMeasms = 8,
    RevokeValidateMeasms = 9,
    DeployParachute = 12,
    ExpandParachute = 13,
    EvaluationRelax = 14,
    EvaluationFocus = 15,
    EvaluationAbort = 16,
    ReinitSensors = 17,
    ReinitBarometer = 18,
    // Historical API name: FC byte 19 reinitializes the IMU.
    EnableIMU = 19,
    DisableIMU = 20,
    #[cfg(feature = "hitl_mode")]
    AdvanceFlightState = 21,
    #[cfg(feature = "hitl_mode")]
    RewindFlightState = 22,
    AbortAfter40 = 23,
    AbortAfter100 = 24,
    AbortAfter250 = 25,
    ReinitAfter15 = 26,
    ReinitAfter30 = 27,
    ReinitAfter50 = 28,
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
}

#[cfg(test)]
mod flight_command_wire_tests {
    use super::FlightComputerCommands::*;

    #[test]
    fn existing_command_bytes_match_the_captured_fc_compatibility_table() {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "test-fixtures/fc-global-state-contract.json"
        )).unwrap();
        let cases = [
            (Postinit, "Compat_Postinit_Signal"),
            (Launch, "Compat_Launch_Signal"),
            (MonitorAltitude, "Compat_Vigilant_Mode"),
            (RevokeMonitorAltitude, "Revoke_Vigilant_Mode"),
            (ConsecutiveSamples, "Compat_Eval_Successive"),
            (RevokeConsecutiveSamples, "Revoke_Eval_Successive"),
            (ResetFailures, "Compat_Reset_Failures"),
            (RevokeResetFailures, "Revoke_Reset_Failures"),
            (ValidateMeasms, "Compat_Measm_Reports"),
            (RevokeValidateMeasms, "Revoke_Measm_Reports"),
            (DeployParachute, "Compat_Deploy_Parachute"),
            (ExpandParachute, "Compat_Expand_Parachute"),
            (EvaluationRelax, "Compat_Evaluation_Relax"),
            (EvaluationFocus, "Compat_Evaluation_Focus"),
            (EvaluationAbort, "Compat_Evaluation_Abort"),
            (ReinitSensors, "Compat_Reinit_Sensors"),
            (ReinitBarometer, "Compat_Reinit_Barometer"),
            (EnableIMU, "Compat_Reinit_IMU"),
            (DisableIMU, "Compat_Disable_IMU"),
            #[cfg(feature = "hitl_mode")]
            (AdvanceFlightState, "Compat_Advance_State"),
            #[cfg(feature = "hitl_mode")]
            (RewindFlightState, "Compat_Rewind_State"),
            (AbortAfter40, "Compat_Abort_After_40"),
            (AbortAfter100, "Compat_Abort_After_100"),
            (AbortAfter250, "Compat_Abort_After_250"),
            (ReinitAfter15, "Compat_Reinit_After_15"),
            (ReinitAfter30, "Compat_Reinit_After_30"),
            (ReinitAfter50, "Compat_Reinit_After_50"),
        ];
        for (command, expected) in cases {
            assert_eq!(contract["remote_commands_compat"][command as usize], expected);
        }
        // Bytes 10 and 11 are intentionally not reused by another GS command.
        assert_eq!(contract["remote_commands_compat"][10], "Compat_Velocity_Checks");
        assert_eq!(contract["remote_commands_compat"][11], "Revoke_Velocity_Checks");
    }
}
