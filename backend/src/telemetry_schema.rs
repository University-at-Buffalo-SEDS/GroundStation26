use std::sync::OnceLock;

use sedsnet::config::{DataEndpoint, DataType, register_schema_json_bytes};

const SCHEMA_JSON: &[u8] = include_bytes!("../config/telemetry_config.json");
static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

/// Stable application IDs assigned by the checked-in runtime schema.
pub mod types {
    use sedsnet::config::DataType;

    pub const GPS_DATA: DataType = DataType(101);
    pub const BAROMETER_DATA: DataType = DataType(106);
    pub const FUEL_FLOW: DataType = DataType(108);
    pub const FUEL_TANK_PRESSURE: DataType = DataType(111);
    pub const BATTERY_VOLTAGE: DataType = DataType(104);
    pub const BATTERY_CURRENT: DataType = DataType(105);
    pub const KG1000: DataType = DataType(118);
    pub const GPS_SATELLITE_NUMBER: DataType = DataType(120);
    pub const ASCENT_STATE: DataType = DataType(123);
    pub const DESCENT_STATE: DataType = DataType(124);
    pub const IMU_DATA: DataType = DataType(128);
}

pub mod endpoints {
    use sedsnet::config::DataEndpoint;

    pub const SD_CARD: DataEndpoint = DataEndpoint(203);
    pub const GROUND_STATION: DataEndpoint = DataEndpoint(204);
    pub const FLIGHT_CONTROLLER: DataEndpoint = DataEndpoint(205);
    pub const VALVE_BOARD: DataEndpoint = DataEndpoint(206);
    pub const ABORT: DataEndpoint = DataEndpoint(207);
    pub const FLIGHT_STATE: DataEndpoint = DataEndpoint(208);
    pub const ACTUATOR_BOARD: DataEndpoint = DataEndpoint(210);
}

/// Registers the application telemetry schema with SEDSnet's v4 runtime registry.
pub fn initialize() -> anyhow::Result<()> {
    INITIALIZED
        .get_or_init(|| {
            register_schema_json_bytes(SCHEMA_JSON)
                .map_err(|err| format!("failed to register telemetry schema: {err}"))?;
            for (name, expected) in [
                ("GPS_DATA", types::GPS_DATA),
                ("BAROMETER_DATA", types::BAROMETER_DATA),
                ("FUEL_FLOW", types::FUEL_FLOW),
                ("FUEL_TANK_PRESSURE", types::FUEL_TANK_PRESSURE),
                ("BATTERY_VOLTAGE", types::BATTERY_VOLTAGE),
                ("BATTERY_CURRENT", types::BATTERY_CURRENT),
                ("KG1000", types::KG1000),
                ("GPS_SATELLITE_NUMBER", types::GPS_SATELLITE_NUMBER),
                ("ASCENT_STATE", types::ASCENT_STATE),
                ("DESCENT_STATE", types::DESCENT_STATE),
                ("IMU_DATA", types::IMU_DATA),
            ] {
                if DataType::named(name) != expected {
                    return Err(format!("runtime schema ID mismatch for {name}"));
                }
            }
            for (name, expected) in [
                ("SD_CARD", endpoints::SD_CARD),
                ("GROUND_STATION", endpoints::GROUND_STATION),
                ("FLIGHT_CONTROLLER", endpoints::FLIGHT_CONTROLLER),
                ("VALVE_BOARD", endpoints::VALVE_BOARD),
                ("ABORT", endpoints::ABORT),
                ("FLIGHT_STATE", endpoints::FLIGHT_STATE),
                ("ACTUATOR_BOARD", endpoints::ACTUATOR_BOARD),
            ] {
                if DataEndpoint::named(name) != expected {
                    return Err(format!("runtime schema ID mismatch for {name}"));
                }
            }
            Ok(())
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

pub fn endpoint(name: &str) -> DataEndpoint {
    initialize().expect("telemetry schema must be valid");
    DataEndpoint::named(name)
}

pub fn data_type(name: &str) -> DataType {
    initialize().expect("telemetry schema must be valid");
    DataType::named(name)
}
