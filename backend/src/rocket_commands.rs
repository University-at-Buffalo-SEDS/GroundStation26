#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum FlightCommands {
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
}
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum ActuatorBoardCommands {
    IgniterOn = 6,
    RetractPlumbing = 7,
    NitrogenOpen = 8,
    NitrousOpen = 9,
    IgniterOff = 10,
    NitrogenClose = 11,
    NitrousClose = 12,
    #[allow(unused)]
    IgniterSequence = 13,
}
