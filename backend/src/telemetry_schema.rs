use std::sync::OnceLock;

use std::collections::HashMap;

use sedsnet::config::{
    DataEndpoint, DataType, register_data_type_id_with_description_and_e2e_encryption,
    register_endpoint_id_with_description,
};
use sedsnet::{E2eEncryptionPolicy, MessageClass, MessageDataType, MessageElement, ReliableMode};
use serde_json::Value;

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

    pub const SD_CARD: DataEndpoint = DataEndpoint(100);
    pub const GROUND_STATION: DataEndpoint = DataEndpoint(101);
    pub const FLIGHT_CONTROLLER: DataEndpoint = DataEndpoint(102);
    pub const VALVE_BOARD: DataEndpoint = DataEndpoint(103);
    pub const ABORT: DataEndpoint = DataEndpoint(104);
    pub const FLIGHT_STATE: DataEndpoint = DataEndpoint(105);
    pub const ACTUATOR_BOARD: DataEndpoint = DataEndpoint(107);
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("schema field `{key}` must be a string"))
}

fn message_data_type(name: &str) -> Result<MessageDataType, String> {
    match name {
        "Float64" => Ok(MessageDataType::Float64),
        "Float32" => Ok(MessageDataType::Float32),
        "UInt8" => Ok(MessageDataType::UInt8),
        "UInt16" => Ok(MessageDataType::UInt16),
        "UInt32" => Ok(MessageDataType::UInt32),
        "UInt64" => Ok(MessageDataType::UInt64),
        "UInt128" => Ok(MessageDataType::UInt128),
        "Int8" => Ok(MessageDataType::Int8),
        "Int16" => Ok(MessageDataType::Int16),
        "Int32" => Ok(MessageDataType::Int32),
        "Int64" => Ok(MessageDataType::Int64),
        "Int128" => Ok(MessageDataType::Int128),
        "Bool" => Ok(MessageDataType::Bool),
        "String" => Ok(MessageDataType::String),
        "Binary" => Ok(MessageDataType::Binary),
        "NoData" => Ok(MessageDataType::NoData),
        other => Err(format!("unsupported schema data type `{other}`")),
    }
}

fn message_class(name: &str) -> Result<MessageClass, String> {
    match name {
        "Data" => Ok(MessageClass::Data),
        "Error" => Ok(MessageClass::Error),
        "Warning" => Ok(MessageClass::Warning),
        other => Err(format!("unsupported schema message class `{other}`")),
    }
}

fn register_firmware_compatible_schema() -> Result<(), String> {
    let schema: Value = serde_json::from_slice(SCHEMA_JSON)
        .map_err(|err| format!("failed to parse telemetry schema: {err}"))?;
    let endpoints = schema["endpoints"]
        .as_array()
        .ok_or_else(|| "schema endpoints must be an array".to_string())?;
    let mut endpoint_ids = HashMap::new();

    // Embedded SEDSNet assigns each endpoint and type namespace from 100. Use
    // those exact wire IDs here instead of the std registry's next free ID
    // (203 after its built-ins), otherwise firmware discovery cannot decode.
    for (index, endpoint) in endpoints.iter().enumerate() {
        let id = DataEndpoint(100 + u32::try_from(index).map_err(|err| err.to_string())?);
        let name = required_str(endpoint, "name")?;
        let alias = endpoint["rust"].as_str().unwrap_or(name);
        let description = endpoint["doc"]
            .as_str()
            .or_else(|| endpoint["description"].as_str())
            .unwrap_or("");
        let link_local = endpoint["link_local_only"].as_bool().unwrap_or(false)
            || endpoint["broadcast_mode"].as_str() == Some("Never");
        register_endpoint_id_with_description(id, name, description, link_local)
            .map_err(|err| format!("failed to register endpoint {name} at {}: {err}", id.0))?;
        endpoint_ids.insert(alias.to_string(), id);
        endpoint_ids.insert(name.to_string(), id);
    }

    let types = schema["types"]
        .as_array()
        .ok_or_else(|| "schema types must be an array".to_string())?;
    for (index, ty) in types.iter().enumerate() {
        let id = DataType(100 + u32::try_from(index).map_err(|err| err.to_string())?);
        let name = required_str(ty, "name")?;
        let description = ty["doc"]
            .as_str()
            .or_else(|| ty["description"].as_str())
            .unwrap_or("");
        let class = message_class(required_str(ty, "class")?)?;
        let element_config = &ty["element"];
        let data_type = message_data_type(required_str(element_config, "data_type")?)?;
        let element = match required_str(element_config, "kind")? {
            "Static" => MessageElement::Static(
                element_config["count"].as_u64().unwrap_or(1) as usize,
                data_type,
                class,
            ),
            "Dynamic" => MessageElement::Dynamic(data_type, class),
            other => return Err(format!("unsupported schema element kind `{other}`")),
        };
        let reliable = match ty["reliable_mode"].as_str() {
            Some("Ordered") => ReliableMode::Ordered,
            Some("Unordered") => ReliableMode::Unordered,
            Some("None") | None if ty["reliable"].as_bool().unwrap_or(false) => {
                ReliableMode::Ordered
            }
            Some("None") | None => ReliableMode::None,
            Some(other) => return Err(format!("unsupported reliable mode `{other}`")),
        };
        let e2e = match ty["e2e_encryption"].as_str().unwrap_or("PreferOff") {
            "PreferOff" | "prefer_off" | "off" | "false" => E2eEncryptionPolicy::PreferOff,
            "PreferOn" | "prefer_on" | "preferred" | "true" => E2eEncryptionPolicy::PreferOn,
            "RequireOn" | "require_on" | "required" => E2eEncryptionPolicy::RequireOn,
            other => return Err(format!("unsupported e2e encryption policy `{other}`")),
        };
        let endpoint_names = ty["endpoints"]
            .as_array()
            .ok_or_else(|| format!("type {name} endpoints must be an array"))?;
        let resolved_endpoints = endpoint_names
            .iter()
            .map(|endpoint| {
                let endpoint = endpoint
                    .as_str()
                    .ok_or_else(|| format!("type {name} endpoint must be a string"))?;
                endpoint_ids
                    .get(endpoint)
                    .copied()
                    .ok_or_else(|| format!("type {name} references unknown endpoint {endpoint}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        register_data_type_id_with_description_and_e2e_encryption(
            id,
            name,
            description,
            element,
            &resolved_endpoints,
            reliable,
            ty["priority"].as_u64().unwrap_or(0) as u8,
            e2e,
        )
        .map_err(|err| format!("failed to register type {name} at {}: {err}", id.0))?;
    }
    Ok(())
}

/// Registers the application telemetry schema with SEDSnet's v4 runtime registry.
pub fn initialize() -> anyhow::Result<()> {
    INITIALIZED
        .get_or_init(|| {
            register_firmware_compatible_schema()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_schema_uses_embedded_firmware_wire_ids() {
        initialize().expect("schema registration");
        assert_eq!(endpoint("SD_CARD"), DataEndpoint(100));
        assert_eq!(endpoint("GROUND_STATION"), DataEndpoint(101));
        assert_eq!(endpoint("FLIGHT_CONTROLLER"), DataEndpoint(102));
        assert_eq!(endpoint("VALVE_BOARD"), DataEndpoint(103));
        assert_eq!(endpoint("HEART_BEAT"), DataEndpoint(106));
        assert_eq!(endpoint("ACTUATOR_BOARD"), DataEndpoint(107));
        assert_eq!(data_type("GENERIC_ERROR"), DataType(100));
        assert_eq!(data_type("VALVE_COMMAND"), DataType(109));
        assert_eq!(data_type("AV_BAY_UNDERGLOW"), DataType(133));
        assert_eq!(data_type("FLIGHT_BUZZER"), DataType(134));
    }

}
