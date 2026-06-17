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
    EnableIMU = 19,
    DisableIMU = 20,
    #[cfg(feature = "hitl_mode")]
    AdvanceFlightState = 21,
    #[cfg(feature = "hitl_mode")]
    RewindFlightState = 22,
    AbortAfter40 = 23,
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
