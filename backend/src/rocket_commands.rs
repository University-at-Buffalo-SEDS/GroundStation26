#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum FlightCommands {
    Launch = 3,
}

#[cfg(feature = "hitl_mode")]
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum FlightComputerCommands {
    DeployParachute = 0,
    ExpandParachute = 1,
    ReinitSensors = 2,
    LaunchSignal = 3,
    EvaluationRelax = 4,
    EvaluationFocus = 5,
    EvaluationAbort = 6,
    ReinitBarometer = 7,
    EnableIMU = 8,
    DisableIMU = 9,
    MonitorAltitude = 10,
    RevokeMonitorAltitude = 11,
    ConsecutiveSamples = 12,
    RevokeConsecutiveSamples = 13,
    ResetFailures = 14,
    RevokeResetFailures = 15,
    ValidateMeasms = 16,
    RevokeValidateMeasms = 17,
    AbortAfter15 = 18,
    AbortAfter40 = 19,
    AbortAfter70 = 20,
    ReinitAfter12 = 21,
    ReinitAfter26 = 22,
    ReinitAfter44 = 23,
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
