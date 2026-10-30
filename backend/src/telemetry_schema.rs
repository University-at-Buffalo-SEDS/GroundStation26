use std::sync::OnceLock;

use sedsnet::config::{DataEndpoint, DataType};

const SCHEMA_JSON: &[u8] = include_bytes!("../config/telemetry_config.json");
static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

// Build-time constants and runtime registration use the same checked-in schema
// and the SDK's identity validation; configured wire IDs override default hashes.
include!(concat!(env!("OUT_DIR"), "/telemetry_schema_ids.rs"));

fn register_firmware_compatible_schema() -> Result<(), String> {
    sedsnet::config::register_schema_json_bytes(SCHEMA_JSON)
        .map_err(|err| format!("failed to register telemetry schema: {err}"))
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
                ("DAQ_LOADCELL_CHAMBER", types::DAQ_LOADCELL_CHAMBER),
                ("KG50", types::KG50),
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
    fn chamber_wire_roundtrip_is_additive_float_triplet() {
        initialize().unwrap();
        assert_eq!(data_type("DAQ_LOADCELL_CHAMBER"), DataType(259346466));
        let payload: Vec<u8> = [4.25_f32, 500.0, 2.5]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let packet = sedsnet::packet::Packet::new(
            data_type("DAQ_LOADCELL_CHAMBER"),
            &[endpoint("GROUND_STATION")],
            "DAQ",
            123456,
            std::sync::Arc::from(payload),
        )
        .unwrap();
        let bytes = sedsnet::wire_format::pack_packet(&packet);
        let decoded = sedsnet::wire_format::unpack_packet(&bytes).unwrap();
        assert_eq!(decoded.data_as_f32().unwrap(), vec![4.25, 500.0, 2.5]);
        assert_eq!(decoded.timestamp(), 123456);
    }

    #[test]
    fn runtime_schema_uses_embedded_firmware_wire_ids() {
        initialize().expect("schema registration");
        assert_eq!(endpoint("SD_CARD"), DataEndpoint(100));
        assert_eq!(endpoint("GROUND_STATION"), DataEndpoint(191579769));
        assert_eq!(endpoint("FLIGHT_CONTROLLER"), DataEndpoint(266702084));
        assert_eq!(endpoint("VALVE_BOARD"), DataEndpoint(61303946));
        assert_eq!(endpoint("HEART_BEAT"), DataEndpoint(179378936));
        assert_eq!(endpoint("ACTUATOR_BOARD"), DataEndpoint(49912881));
        assert_eq!(data_type("GENERIC_ERROR"), DataType(30636108));
        assert_eq!(data_type("VALVE_COMMAND"), DataType(245583120));
        assert_eq!(data_type("AV_BAY_UNDERGLOW"), DataType(68266860));
        assert_eq!(data_type("FLIGHT_BUZZER"), DataType(169036741));
    }

    #[test]
    fn umbilical_command_ack_is_reliable_and_ordered() {
        let schema: serde_json::Value =
            serde_json::from_slice(SCHEMA_JSON).expect("embedded telemetry schema");
        let status = schema["types"]
            .as_array()
            .expect("schema types")
            .iter()
            .find(|item| item["name"] == "UMBILICAL_STATUS")
            .expect("UMBILICAL_STATUS type");
        assert_eq!(status["reliable"], "Ordered");
    }

    #[test]
    fn commands_and_confirmations_use_protected_can_priority() {
        let schema: serde_json::Value = serde_json::from_slice(SCHEMA_JSON).unwrap();
        for name in [
            "ACTUATOR_COMMAND",
            "VALVE_COMMAND",
            "FLIGHT_COMMAND",
            "UMBILICAL_STATUS",
        ] {
            let ty = schema["types"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == name)
                .expect("control type");
            assert!(ty["priority"].as_u64().unwrap() >= 200, "{name}");
            assert_eq!(ty["reliable"], "Ordered", "{name}");
        }
    }

    #[test]
    fn heartbeat_matches_firmware_priority() {
        let schema: serde_json::Value =
            serde_json::from_slice(SCHEMA_JSON).expect("embedded telemetry schema");
        let heartbeat = schema["types"]
            .as_array()
            .expect("schema types")
            .iter()
            .find(|item| item["name"] == "HEARTBEAT")
            .expect("HEARTBEAT type");
        assert_eq!(heartbeat["priority"], 255);
        assert_eq!(heartbeat["reliable"], "Off");
    }
    #[test]
    fn daq_analog_schema_and_layouts_share_the_wire_contract() {
        initialize().unwrap();
        let schema: serde_json::Value = serde_json::from_slice(SCHEMA_JSON).unwrap();
        let entries = [
            ("DAQ_SAR_VOLTAGES", 227135715, 8),
            ("DAQ_SDADC_VOLTAGES", 250592328, 6),
            ("DAQ_CURRENT_SENSE", 69721613, 4),
            ("DAQ_POWER_MONITOR", 87662373, 4),
        ];
        for (name, id, count) in entries {
            assert_eq!(data_type(name), DataType(id));
            let ty = schema["types"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap();
            assert_eq!(ty["element"]["count"], count);
            assert_eq!(ty["element"]["data_type"], "Float32");
            assert_eq!(ty["endpoints"], serde_json::json!(["GROUND_STATION"]));
        }
        for text in [
            include_str!("../layout/layout.json"),
            include_str!("../layout/layout_test_fire.json"),
            include_str!("../layout/layout_hitl.json"),
        ] {
            let layout: serde_json::Value = serde_json::from_str(text).unwrap();
            let tab = layout["data_tab"]["tabs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == "DAQ_ANALOG")
                .unwrap();
            for ((name, _, count), sub) in entries.iter().zip(tab["subtabs"].as_array().unwrap()) {
                assert_eq!(sub["data_type"], *name);
                assert_eq!(
                    sub["summary_items"].as_array().unwrap().len(),
                    *count as usize
                );
                let mut indices = Vec::new();
                for group in sub["chart_groups"].as_array().unwrap() {
                    for series in group["chart_series"].as_array().unwrap() {
                        assert_eq!(series["data_type"], *name);
                        assert_eq!(series["sender_id"], "DAQ");
                        indices.push(series["index"].as_u64().unwrap());
                    }
                }
                indices.sort_unstable();
                assert_eq!(indices, (0..*count as u64).collect::<Vec<_>>());
            }
        }
    }
    #[test]
    fn daq_analog_loadcells_preserve_raw_calibrated_and_temperature_bindings() {
        for text in [
            include_str!("../layout/layout.json"),
            include_str!("../layout/layout_test_fire.json"),
            include_str!("../layout/layout_hitl.json"),
        ] {
            let layout: serde_json::Value = serde_json::from_str(text).unwrap();
            let tabs = layout["data_tab"]["tabs"].as_array().unwrap();
            let original = tabs.iter().find(|t| t["id"] == "LOADCELL").unwrap();
            let analog = tabs.iter().find(|t| t["id"] == "DAQ_ANALOG").unwrap();
            for (source, target) in [
                ("loadcell_1000kg", "daq_mcp_ch0"),
                ("loadcell_50kg", "daq_mcp_ch1"),
            ] {
                let expected = original["subtabs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["id"] == source)
                    .unwrap();
                let actual = analog["subtabs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["id"] == target)
                    .unwrap();
                for field in [
                    "data_type",
                    "channels",
                    "chart_groups",
                    "summary_items",
                    "chart",
                ] {
                    assert_eq!(actual[field], expected[field], "{target}: {field}");
                }
            }
        }
    }
}

#[cfg(test)]
mod explicit_id_tests {
    use super::*;
    use sedsnet::config::DataType;
    #[test]
    fn configured_constants_registration_and_flight_specs_align() {
        initialize().unwrap();
        for (name, id) in [
            ("GPS_DATA", types::GPS_DATA),
            ("IMU_DATA", types::IMU_DATA),
            ("BAROMETER_DATA", types::BAROMETER_DATA),
        ] {
            assert_eq!(data_type(name), id);
            assert_ne!(DataType::identity(name), id);
        }
        assert_eq!(types::GPS_DATA.0, 100);
        assert_eq!(types::IMU_DATA.0, 101);
        assert_eq!(types::BAROMETER_DATA.0, 104);
        assert_eq!(endpoint("SD_CARD"), endpoints::SD_CARD);
        assert_eq!(endpoints::SD_CARD.0, 100);
        let snapshot = sedsnet::config::schema_snapshot_from_json_bytes(include_bytes!(
            "../config/telemetry_config.json"
        ))
        .unwrap();
        for t in snapshot.types {
            assert_eq!(data_type(&t.name), t.id);
        }
        for e in snapshot.endpoints {
            assert_eq!(endpoint(&e.name), e.id);
        }
        // Same const-expression shape as flight_sim static RateSpec arrays.
        const FLIGHT_TYPES: [DataType; 3] =
            [types::IMU_DATA, types::BAROMETER_DATA, types::GPS_DATA];
        assert_eq!(FLIGHT_TYPES.map(|t| t.0), [101, 104, 100]);
    }
    #[test]
    fn malformed_or_conflicting_explicit_ids_fail_without_registry_changes() {
        const CHILD: &str = "GS_SCHEMA_ID_REJECTION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Other backend tests legitimately add runtime schema definitions.
            // Measure rollback in a process that owns its registry.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "telemetry_schema::explicit_id_tests::malformed_or_conflicting_explicit_ids_fail_without_registry_changes",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        initialize().unwrap();
        let before = sedsnet::config::schema_fingerprint();
        for id in [
            serde_json::json!(-1),
            serde_json::json!(4294967296u64),
            serde_json::json!("100"),
            serde_json::json!(null),
            serde_json::json!(0),
        ] {
            let mut value: serde_json::Value =
                serde_json::from_slice(include_bytes!("../config/telemetry_config.json")).unwrap();
            value["types"][0]["id"] = id.clone();
            // null is serde optional absence; it is legal and hashes normally.
            if id.is_null() {
                continue;
            }
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(
                sedsnet::config::schema_snapshot_from_json_bytes(&bytes).is_err(),
                "{id}"
            );
            assert!(
                sedsnet::config::register_schema_json_bytes(&bytes).is_err(),
                "{id}"
            );
            assert_eq!(sedsnet::config::schema_fingerprint(), before);
        }
        let mut v: serde_json::Value =
            serde_json::from_slice(include_bytes!("../config/telemetry_config.json")).unwrap();
        v["types"][0]["id"] = serde_json::json!(100);
        let bytes = serde_json::to_vec(&v).unwrap();
        assert!(sedsnet::config::schema_snapshot_from_json_bytes(&bytes).is_err());
        assert!(sedsnet::config::register_schema_json_bytes(&bytes).is_err());
        assert_eq!(sedsnet::config::schema_fingerprint(), before);
    }
}
