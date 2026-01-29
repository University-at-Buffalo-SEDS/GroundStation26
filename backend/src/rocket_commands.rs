#[repr(u8)]
pub enum FlightCommands {
    Launch,
}

#[repr(u8)]
pub enum ValveBoardCommands {
    PilotOpen,
    TanksOpen,
    DumpOpen,
    PilotClose,
    TanksClose,
    DumpClose,
}
#[repr(u8)]
pub enum ActuatorBoardCommands {
    IgniterOn,
    RetractPlumbing,
    NitrogenValveOpen,
    NitrousOpen,
    IgniterOff,
    NitrogenValveOff,
    NitrousOff,

}