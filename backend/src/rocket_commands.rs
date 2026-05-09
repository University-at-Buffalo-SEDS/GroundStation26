#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum FlightComputerCommands {
    LaunchSignal = 1,
    MonitorAltitude = 2,
    RevokeMonitorAltitude = 3,
    ConsecutiveSamples = 4,
    RevokeConsecutiveSamples = 5,
    ResetFailures = 6,
    RevokeResetFailures = 7,
    ValidateMeasms = 8,
    RevokeValidateMeasms = 9,
    #[cfg(feature = "hitl_mode")]
    DeployParachute = 10,
    #[cfg(feature = "hitl_mode")]
    ExpandParachute = 11,
    #[cfg(feature = "hitl_mode")]
    EvaluationRelax = 12,
    #[cfg(feature = "hitl_mode")]
    EvaluationFocus = 13,
    #[cfg(feature = "hitl_mode")]
    EvaluationAbort = 14,
    #[cfg(feature = "hitl_mode")]
    ReinitSensors = 15,
    #[cfg(feature = "hitl_mode")]
    ReinitBarometer = 16,
    #[cfg(feature = "hitl_mode")]
    ReinitIMU = 17,
    #[cfg(feature = "hitl_mode")]
    DisableIMU = 18,
    #[cfg(feature = "hitl_mode")]
    AdvanceFlightState = 19,
    #[cfg(feature = "hitl_mode")]
    RewindFlightState = 20,
    #[cfg(feature = "hitl_mode")]
    AbortAfter40 = 21,
    #[cfg(feature = "hitl_mode")]
    AbortAfter100 = 22,
    #[cfg(feature = "hitl_mode")]
    AbortAfter250 = 23,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter15 = 24,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter30 = 25,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter50 = 26,
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
