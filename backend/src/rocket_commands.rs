#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum FlightComputerCommands {
    DeployParachute = 0,
    ExpandParachute = 1,
    ReinitSensors = 2,
    LaunchSignal = 3,
    #[cfg(feature = "hitl_mode")]
    EvaluationRelax = 4,
    #[cfg(feature = "hitl_mode")]
    EvaluationFocus = 5,
    #[cfg(feature = "hitl_mode")]
    EvaluationAbort = 6,
    ReinitBarometer = 7,
    ReinitIMU = 8,
    #[cfg(feature = "hitl_mode")]
    DisableIMU = 9,
    #[cfg(feature = "hitl_mode")]
    AdvanceFlightState = 10,
    #[cfg(feature = "hitl_mode")]
    RewindFlightState = 11,
    MonitorAltitude = 12,
    RevokeMonitorAltitude = 13,
    #[cfg(feature = "hitl_mode")]
    ConsecutiveSamples = 14,
    #[cfg(feature = "hitl_mode")]
    RevokeConsecutiveSamples = 15,
    #[cfg(feature = "hitl_mode")]
    ResetFailures = 16,
    #[cfg(feature = "hitl_mode")]
    RevokeResetFailures = 17,
    ValidateMeasms = 18,
    RevokeValidateMeasms = 19,
    #[cfg(feature = "hitl_mode")]
    AbortAfter40 = 20,
    #[cfg(feature = "hitl_mode")]
    AbortAfter100 = 21,
    AbortAfter250 = 22,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter15 = 23,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter30 = 24,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter50 = 25,
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
