#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum FlightComputerCommands {
    PostinitSignal = 0,
    LaunchSignal = 1,
    RollbackSignal = 2,
    MonitorAltitude = 3,
    RevokeMonitorAltitude = 4,
    ConsecutiveSamples = 5,
    RevokeConsecutiveSamples = 6,
    ResetFailures = 7,
    RevokeResetFailures = 8,
    ValidateMeasms = 9,
    RevokeValidateMeasms = 10,
    #[cfg(feature = "hitl_mode")]
    DeployParachute = 11,
    #[cfg(feature = "hitl_mode")]
    ExpandParachute = 12,
    #[cfg(feature = "hitl_mode")]
    EvaluationRelax = 13,
    #[cfg(feature = "hitl_mode")]
    EvaluationFocus = 14,
    #[cfg(feature = "hitl_mode")]
    EvaluationAbort = 15,
    #[cfg(feature = "hitl_mode")]
    ReinitSensors = 16,
    #[cfg(feature = "hitl_mode")]
    ReinitBarometer = 17,
    #[cfg(feature = "hitl_mode")]
    ReinitIMU = 18,
    #[cfg(feature = "hitl_mode")]
    DisableIMU = 19,
    #[cfg(feature = "hitl_mode")]
    AdvanceFlightState = 20,
    #[cfg(feature = "hitl_mode")]
    RewindFlightState = 21,
    #[cfg(feature = "hitl_mode")]
    AbortAfter40 = 22,
    #[cfg(feature = "hitl_mode")]
    AbortAfter100 = 23,
    #[cfg(feature = "hitl_mode")]
    AbortAfter250 = 24,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter15 = 25,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter30 = 26,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter50 = 27,
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
