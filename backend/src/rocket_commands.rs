#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum FlightCommands {
    Launch,
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