#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum FlightComputerCommands {
    LaunchSignal = 1,
    VigilantMode = 2,
    RevokeVigilantMode = 3,
    EvalSuccessive = 4,
    RevokeEvalSuccessive = 5,
    ResetFailures = 6,
    RevokeResetFailures = 7,
    MeasmReports = 8,
    RevokeMeasmReports = 9,
    VelocityChecks = 10,
    RevokeVelocityChecks = 11,
    #[cfg(feature = "hitl_mode")]
    DeployParachute = 12,
    #[cfg(feature = "hitl_mode")]
    ExpandParachute = 13,
    #[cfg(feature = "hitl_mode")]
    EvaluationRelax = 14,
    #[cfg(feature = "hitl_mode")]
    EvaluationFocus = 15,
    #[cfg(feature = "hitl_mode")]
    EvaluationAbort = 16,
    #[cfg(feature = "hitl_mode")]
    ReinitSensors = 17,
    #[cfg(feature = "hitl_mode")]
    ReinitBarometer = 18,
    #[cfg(feature = "hitl_mode")]
    ReinitIMU = 19,
    #[cfg(feature = "hitl_mode")]
    DisableIMU = 20,
    #[cfg(feature = "hitl_mode")]
    AdvanceFlightState = 21,
    #[cfg(feature = "hitl_mode")]
    RewindFlightState = 22,
    #[cfg(feature = "hitl_mode")]
    AbortAfter40 = 23,
    #[cfg(feature = "hitl_mode")]
    AbortAfter100 = 24,
    #[cfg(feature = "hitl_mode")]
    AbortAfter250 = 25,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter15 = 26,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter30 = 27,
    #[cfg(feature = "hitl_mode")]
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
