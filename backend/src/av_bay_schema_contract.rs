//! Every AV-bay board's compiled schema must be a subset of the station's.
//!
//! This is why the station keeps discovery schema exchange off the radio
//! (`keep_schema_exchange_off_the_radio`): the SDK keeps a compiled definition
//! over any received one, so a schema received over the radio can only add a
//! type the station lacks, and this test makes that impossible. The snapshots
//! in `backend/tests/board-schemas/` are each board's `config/sedsnet.json`
//! from its dev branch; refresh one whenever that board's schema changes:
//!
//!     git -C <BoardRepo> show origin/dev:config/sedsnet.json > backend/tests/board-schemas/<BoardRepo>.json
//!
//! Priority is not compared: it is local transport scheduling, and a compiled
//! definition's priority is never replaced by a received one.
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

const AV_BAY_BOARDS: [&str; 3] = ["RFBoard26", "FlightComputer26", "PowerBoard26"];

fn by_name(list: &Value) -> BTreeMap<String, Value> {
    list.as_array()
        .expect("schema list")
        .iter()
        .map(|entry| (entry["name"].as_str().expect("named entry").to_string(), entry.clone()))
        .collect()
}

/// Differences that would make a board's schema teach the station something.
fn schema_gaps(station: &Value, board: &Value) -> Vec<String> {
    let station_endpoints = by_name(&station["endpoints"]);
    let station_types = by_name(&station["types"]);
    // Boards name endpoints by their Rust alias inside type entries.
    let alias: BTreeMap<String, String> = board["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let name = e["name"].as_str().unwrap().to_string();
            (e.get("rust").and_then(Value::as_str).unwrap_or(&name).to_string(), name)
        })
        .collect();
    let mut gaps = Vec::new();
    for (name, endpoint) in by_name(&board["endpoints"]) {
        match station_endpoints.get(&name) {
            None => gaps.push(format!("endpoint {name} is missing from the station schema")),
            Some(ours) if ours.get("id") != endpoint.get("id") => gaps.push(format!("endpoint {name} id differs")),
            Some(_) => {}
        }
    }
    for (name, ty) in by_name(&board["types"]) {
        let Some(ours) = station_types.get(&name) else {
            gaps.push(format!("type {name} is missing from the station schema"));
            continue;
        };
        for field in ["class", "element", "reliable", "id"] {
            if ours.get(field) != ty.get(field) {
                gaps.push(format!("type {name} {field} differs: board {:?}, station {:?}", ty.get(field), ours.get(field)));
            }
        }
        let mut theirs: Vec<String> = ty["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let e = e.as_str().unwrap();
                alias.get(e).cloned().unwrap_or_else(|| e.to_string())
            })
            .collect();
        let mut mine: Vec<String> = ours["endpoints"].as_array().unwrap().iter().map(|e| e.as_str().unwrap().to_string()).collect();
        theirs.sort();
        mine.sort();
        if theirs != mine {
            gaps.push(format!("type {name} endpoints differ: board {theirs:?}, station {mine:?}"));
        }
    }
    gaps
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).unwrap()
}

#[test]
fn av_bay_board_schemas_are_a_subset_of_the_station_schema() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let station = read(&root.join("config/telemetry_config.json"));
    let mut all = Vec::new();
    for board in AV_BAY_BOARDS {
        let gaps = schema_gaps(&station, &read(&root.join("tests/board-schemas").join(format!("{board}.json"))));
        all.extend(gaps.into_iter().map(|gap| format!("{board}: {gap}")));
    }
    assert!(
        all.is_empty(),
        "an AV-bay board schema has what the station's lacks; the radio carries no schema exchange, so update \
         backend/config/telemetry_config.json:\n  {}",
        all.join("\n  ")
    );
}

#[test]
fn a_board_only_type_or_changed_definition_is_a_gap() {
    let station = serde_json::json!({
        "endpoints": [{"name": "GROUND_STATION"}],
        "types": [{"name": "A", "class": "Data", "element": {"kind": "Static", "data_type": "Float32", "count": 1},
                   "reliable": "Off", "priority": 0, "endpoints": ["GROUND_STATION"]}]
    });
    let mut board = station.clone();
    board["endpoints"][0]["rust"] = "BoardGroundStation".into();
    board["types"][0]["endpoints"] = serde_json::json!(["BoardGroundStation"]);
    board["types"][0]["priority"] = 200.into();
    assert!(schema_gaps(&station, &board).is_empty(), "aliases and priority alone are not gaps");
    board["types"][0]["reliable"] = "Ordered".into();
    board["types"].as_array_mut().unwrap().push(serde_json::json!({"name": "B", "class": "Data",
        "element": {"kind": "Static", "data_type": "NoData", "count": 0}, "reliable": "Off", "priority": 0, "endpoints": []}));
    let gaps = schema_gaps(&station, &board);
    assert!(gaps.iter().any(|g| g.contains("type B is missing")), "{gaps:?}");
    assert!(gaps.iter().any(|g| g.contains("type A reliable differs")), "{gaps:?}");
}
