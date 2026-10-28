fn main() {
    println!("cargo:rerun-if-changed=config/telemetry_config.json");
    let schema = std::fs::read("config/telemetry_config.json").expect("read GS schema");
    let snapshot = sedsnet::config::schema_snapshot_from_json_bytes(&schema)
        .expect("GS schema IDs and metadata must be valid");
    let mut output = String::from("// Generated from config/telemetry_config.json; do not edit.\n");
    for (module, wrapper, entries) in [
        (
            "types",
            "DataType",
            snapshot
                .types
                .iter()
                .map(|t| (t.name.as_str(), t.id.0))
                .collect::<Vec<_>>(),
        ),
        (
            "endpoints",
            "DataEndpoint",
            snapshot
                .endpoints
                .iter()
                .map(|e| (e.name.as_str(), e.id.0))
                .collect::<Vec<_>>(),
        ),
    ] {
        output.push_str(&format!(
            "pub mod {module} {{\nuse sedsnet::config::{wrapper};\n"
        ));
        // These names define the existing GS const API used by flight_sim.
        // The schema remains the sole authority for their numeric identities.
        let required: &[&str] = match module {
            "types" => &[
                "GPS_DATA",
                "BAROMETER_DATA",
                "FUEL_FLOW",
                "FUEL_TANK_PRESSURE",
                "BATTERY_VOLTAGE",
                "BATTERY_CURRENT",
                "KG1000",
                "KG50",
                "GPS_SATELLITE_NUMBER",
                "ASCENT_STATE",
                "DESCENT_STATE",
                "IMU_DATA",
            ],
            "endpoints" => &[
                "SD_CARD",
                "GROUND_STATION",
                "FLIGHT_CONTROLLER",
                "VALVE_BOARD",
                "ABORT",
                "FLIGHT_STATE",
                "ACTUATOR_BOARD",
            ],
            _ => unreachable!(),
        };
        for &name in required {
            let id = entries
                .iter()
                .find_map(|(candidate, id)| (*candidate == name).then_some(*id))
                .unwrap_or_else(|| {
                    panic!("GS schema is missing required {module} identity {name}")
                });
            assert!(
                !name.is_empty()
                    && name != "_"
                    && name.bytes().enumerate().all(|(i, b)| b == b'_'
                        || b.is_ascii_uppercase()
                        || (i > 0 && b.is_ascii_digit())),
                "GS constant name must be an uppercase Rust identifier: {name}"
            );
            output.push_str(&format!("pub const {name}: {wrapper} = {wrapper}({id});\n"));
        }
        output.push_str("}\n");
    }
    std::fs::write(
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("telemetry_schema_ids.rs"),
        output,
    )
    .unwrap();
}
