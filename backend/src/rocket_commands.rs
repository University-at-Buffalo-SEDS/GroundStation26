#[repr(u8)]
pub enum FlightCommands {
    Launch,

}

#[repr(u8)]
pub enum ValveCommands {
    Igniter,
    Pilot,
    Tanks,
    Dump,
}