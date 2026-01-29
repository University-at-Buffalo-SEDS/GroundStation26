#[repr(u8)]
pub enum FlightCommands {
    Launch,
}

#[repr(u8)]
pub enum ValveBoardCommands {
    PilotOpen = 0,
    TanksOpen = 1,
    DumpOpen = 2,
    PilotClose = 3,
    TanksClose = 4,
    DumpClose = 5,
}
#[repr(u8)]
pub enum ActuatorBoardCommands {
    IgniterOn = 6,
    RetractPlumbing = 7,
    NitrogenOpen = 8,
    NitrousOpen = 9,
    IgniterOff = 10,
    NitrogenClose = 11,
    NitrousClose = 12,

}