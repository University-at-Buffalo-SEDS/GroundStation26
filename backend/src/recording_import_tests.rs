use super::*;
use crate::recording_export::tests::{TestDirectory, export_state};
use axum::{body::Body, http::Request};
use tower::ServiceExt;

const ID: &str = "groundstation_recording_2026-01-01_00-00-00_000.db";
const T0: i64 = 1_767_225_600_000;

type Row = (i64, Option<i64>, Option<&'static str>, &'static str, Option<&'static str>, Option<&'static str>, Option<i64>);

/// Rows that exercise every CSV rule: source times, a legacy type name,
/// low and high over-range markers, a formula-like sender, quotes, commas
/// and a line break in a payload, and NULL sender, values and payload.
fn rows() -> Vec<Row> {
    vec![
        (T0, Some(T0 - 5), Some("FC"), "FLIGHT_STATE", Some("[6]"), Some("[]"), None),
        (T0 + 10, None, Some("DAQ"), "LOADCELL_50_WEIGHT_KG", Some("[null,2.5]"), Some("[]"), crate::over_range::pack(1, 1)),
        (T0 + 20, None, Some("VB"), "FUEL_TANK_PRESSURE", Some("[745.0]"), Some("[]"), None),
        (T0 + 30, Some(12), Some("=cmd"), "GS_NOTE", None, Some("{\"text\":\"a, \\\"b\\\"\"}\r\nnext line"), None),
        (T0 + 40, None, None, "BATTERY_VOLTAGE", Some("[7.9]"), None, None),
        (T0 + 50, None, Some("DAQ"), "LOADCELL_1000_WEIGHT_KG", Some("[1.25,null,3]"), Some("[]"), crate::over_range::pack(2, 0)),
        (T0 + 60_000, None, Some("FC"), "FLIGHT_STATE", Some("[13]"), Some("[]"), None),
    ]
}

/// An integrated ground test recording: its CSV has the provenance column.
fn igt_rows() -> Vec<Row> {
    let mut rows = vec![
        (T0 - 1, None, Some("GS"), "GS_STATION_MODE", None, Some("{\"mode\":\"integrated_ground_test\"}"), None),
        (T0 - 1, None, Some("GS"), "GS_IGT", None, Some("{\"fc_mode\":\"sim\"}"), None),
        (T0 + 5, None, Some("FC"), "SIM_SAMPLE", Some("[1,2]"), Some("[]"), None),
    ];
    rows.extend(self::rows());
    rows
}

async fn recording(root: &Path, rows: Vec<Row>) {
    let (db, path) = crate::telemetry_db::open_telemetry_db(&root.join(ID)).await.unwrap();
    for (t, source, sender, data_type, values, payload, over_range) in rows {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json,over_range) VALUES(?,?,?,?,?,?,?)")
            .bind(t).bind(source).bind(sender).bind(data_type).bind(values).bind(payload).bind(over_range)
            .execute(&db).await.unwrap();
    }
    sqlx::query("INSERT INTO alerts(timestamp_ms,severity,message) VALUES(?,?,?)").bind(T0 + 15).bind("warning").bind("Tank pressure high")
        .execute(&db).await.unwrap();
    crate::telemetry_db::close_and_finalize_sqlite(db, &path).await;
}

async fn get(state: &Arc<AppState>, path: &str) -> (StatusCode, Vec<u8>) {
    let router = crate::recording_export::routes().with_state(state.clone());
    let response = router.oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    (status, axum::body::to_bytes(response.into_body(), 256 << 20).await.unwrap().to_vec())
}

async fn admin_state(root: &Path) -> Arc<AppState> {
    let state = export_state(root, true).await;
    std::fs::write(root.join("auth.json"), json!({"version": 1, "anonymous": {"view_data": true}, "users": [{"username": "import-admin", "roles": ["admin"], "password": {"salt_b64": "", "hash_b64": ""}, "permissions": {"view_data": true}}]}).to_string()).unwrap();
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('import-token','import-admin','session',1,0,0,9999999999999)")
        .execute(&state.auth_db).await.unwrap();
    state
}

/// The bundle of a recording made from `rows`, and that recording's CSV and replay export.
async fn source_bundle(rows: Vec<Row>) -> (Vec<u8>, Vec<u8>, Value) {
    let root = TestDirectory::new();
    let state = export_state(&root.0, true).await;
    recording(&root.0, rows).await;
    let bundle = crate::recording_bundle::build(state.clone(), &HeaderMap::new(), ID, &Default::default(), T0).await.unwrap();
    let (status, csv) = get(&state, &format!("/api/recordings/{ID}/csv")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, replay) = get(&state, &format!("/api/recordings/{ID}/replay/export")).await;
    assert_eq!(status, StatusCode::OK);
    (bundle, csv, serde_json::from_slice(&replay).unwrap())
}

async fn round_trip(rows: Vec<Row>) -> (Vec<u8>, Vec<u8>) {
    let (bundle, csv, replay) = source_bundle(rows).await;
    let root = TestDirectory::new();
    let state = export_state(&root.0, true).await;
    let imported = import_bundle(&root.0, &bundle, Some("import-admin"), T0 + 1).await.unwrap();
    let id = imported["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("event.imported.") && id.ends_with("_groundstation-recording-2026-01-01-00-00-00-000.db"), "{id}");
    assert_eq!(imported["source"]["recording"], ID);

    let (status, listing) = get(&state, "/api/recordings").await;
    assert_eq!(status, StatusCode::OK);
    let listing: Value = serde_json::from_slice(&listing).unwrap();
    let listed = listing.as_array().unwrap().iter().find(|r| r["id"] == id.as_str()).expect("the import is listed");
    assert_eq!(listed["event_type"], "imported");

    let (status, exported) = get(&state, &format!("/api/recordings/{id}/csv")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, imported_replay) = get(&state, &format!("/api/recordings/{id}/replay/export")).await;
    assert_eq!(status, StatusCode::OK);
    let imported_replay: Value = serde_json::from_slice(&imported_replay).unwrap();
    assert_eq!(imported_replay["samples"], replay["samples"], "the imported recording replays the same rows");
    assert_eq!(imported_replay["snapshot"], replay["snapshot"]);
    let (_, alerts) = get(&state, &format!("/api/recordings/{id}/replay/alerts")).await;
    assert_eq!(serde_json::from_slice::<Value>(&alerts).unwrap()["alerts"][0]["message"], "Tank pressure high");

    let again = import_bundle(&root.0, &bundle, None, T0 + 2).await.unwrap_err();
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.existing.as_deref(), Some(id.as_str()));
    (csv, exported)
}

#[tokio::test]
async fn an_imported_bundle_exports_the_same_csv_byte_for_byte() {
    let (csv, exported) = round_trip(rows()).await;
    let text = String::from_utf8(csv.clone()).unwrap();
    assert!(text.contains("\"[\"\"-OVR\"\",2.5]\"") && text.contains("\"OX_TANK_PRESSURE\"") && text.contains("\"'=cmd\""), "{text}");
    assert_eq!(String::from_utf8(exported).unwrap(), text);
}

#[tokio::test]
async fn an_integrated_ground_test_import_keeps_the_provenance_column() {
    let (csv, exported) = round_trip(igt_rows()).await;
    assert!(String::from_utf8_lossy(&csv).starts_with(PROVENANCE_HEADER));
    assert_eq!(exported, csv);
}

#[tokio::test]
async fn only_administrators_import() {
    let (bundle, _, _) = source_bundle(rows()).await;
    let root = TestDirectory::new();
    let state = admin_state(&root.0).await;
    let post = |token: Option<&str>| {
        let mut request = Request::post("/api/recordings/import");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let router = routes().with_state(state.clone());
        let request = request.body(Body::from(bundle.clone())).unwrap();
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            (status, serde_json::from_slice::<Value>(&axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap_or_default())
        }
    };
    assert_eq!(post(None).await.0, StatusCode::FORBIDDEN, "a viewer cannot import");
    let (status, created) = post(Some("import-token")).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let folder = store::events_dir(&root.0).join("imported").join(created["id"].as_str().unwrap().split('.').nth(2).unwrap());
    let manifest = store::read_manifest(&folder).unwrap();
    assert_eq!(manifest.operator.as_deref(), Some("import-admin"));
    assert_eq!(manifest.details["import"]["source"]["recording"], ID);
    let (status, conflict) = post(Some("import-token")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["id"], created["id"]);
}

/// A bundle of `files` whose manifest lists them; `edit` changes the manifest.
fn bundle_of(files: &[(&str, &[u8])], edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut manifest = json!({"format": "gsreplay", "version": 1, "source": {"recording": ID},
        "entries": files.iter().map(|(path, bytes)| json!({"path": path, "bytes": bytes.len(), "sha256": sha256_hex(bytes)})).collect::<Vec<_>>()});
    edit(&mut manifest);
    let mut zip = crate::flight_review::zip::Zip::new(0);
    zip.add("manifest.json", manifest.to_string().as_bytes());
    for (path, bytes) in files {
        zip.add(path, bytes);
    }
    zip.finish()
}

#[tokio::test]
async fn unsafe_or_inconsistent_bundles_are_refused() {
    let root = TestDirectory::new();
    let csv = format!("{PLAIN_HEADER}\r\n1,{T0},,\"FC\",\"FLIGHT_STATE\",\"[6]\",\"[]\"\r\n");
    let status = |bytes: Vec<u8>| {
        let root = root.0.clone();
        async move { import_bundle(&root, &bytes, None, T0).await.map(|_| StatusCode::CREATED).unwrap_or_else(|r| r.status) }
    };
    let message = |bytes: Vec<u8>| {
        let root = root.0.clone();
        async move { import_bundle(&root, &bytes, None, T0).await.unwrap_err().message }
    };
    // Size and hash: a listed entry that differs, or is absent.
    let tampered = bundle_of(&[(CSV_PATH, csv.as_bytes())], |m| m["entries"][0]["sha256"] = "00".into());
    assert!(message(tampered).await.contains("SHA-256"));
    let absent = bundle_of(&[], |m| m["entries"] = json!([{"path": CSV_PATH, "bytes": 1, "sha256": "00"}]));
    assert!(message(absent).await.contains("missing"));
    // Format and version.
    assert_eq!(status(bundle_of(&[(CSV_PATH, csv.as_bytes())], |m| m["format"] = "zip".into())).await, StatusCode::BAD_REQUEST);
    assert_eq!(status(bundle_of(&[(CSV_PATH, csv.as_bytes())], |m| m["version"] = 2.into())).await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(status(bundle_of(&[(CSV_PATH, csv.as_bytes())], |m| m["version"] = "1".into())).await, StatusCode::BAD_REQUEST);
    // An SD-resolved recording's CSV.
    let resolved = "id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json,canonical_utc_ms,provenance_json\r\n";
    assert_eq!(status(bundle_of(&[(CSV_PATH, resolved.as_bytes())], |_| {})).await, StatusCode::UNPROCESSABLE_ENTITY);
    // Unsafe names.
    for name in ["../evil", "/abs", "a//b", "a\\b", "c:x", "./a"] {
        let bytes = bundle_of(&[(name, b"x")], |_| {});
        assert!(message(bytes).await.contains("safe relative path"), "{name}");
    }
    // Container: other methods, ZIP64, and the unpacked limit.
    let good = bundle_of(&[(CSV_PATH, csv.as_bytes())], |_| {});
    assert_eq!(status(good.clone()).await, StatusCode::CREATED);
    let central = (0..good.len() - 4).rev().find(|&i| good[i..i + 4] == [0x50, 0x4b, 0x01, 0x02]).unwrap();
    let mut method = good.clone();
    method[central + 10] = 12;
    assert!(message(method).await.contains("method 12"));
    let mut zip64 = good.clone();
    let end = good.len() - 22;
    zip64[end + 10] = 0xFF;
    zip64[end + 11] = 0xFF;
    assert!(message(zip64).await.contains("ZIP64"));
    let mut huge = good.clone();
    huge[central + 24..central + 28].copy_from_slice(&(193u32 << 20).to_le_bytes());
    assert!(message(huge).await.contains("192 MiB"));
    let mut encrypted = good.clone();
    encrypted[central + 8] |= 1;
    assert!(message(encrypted).await.contains("encrypted"));
    assert_eq!(status(b"not a zip".to_vec()).await, StatusCode::BAD_REQUEST);
    // A malformed row leaves nothing behind.
    let broken = format!("{PLAIN_HEADER}\r\n1,{T0},,\"FC\",\"FLIGHT_STATE\",\"[6]\"\r\n");
    let other = bundle_of(&[(CSV_PATH, broken.as_bytes())], |m| m["source"]["recording"] = "other.db".into());
    assert!(message(other).await.contains("cells"));
    let imported: Vec<_> = std::fs::read_dir(store::events_dir(&root.0).join("imported")).unwrap().collect();
    assert_eq!(imported.len(), 1, "only the good bundle was stored");
}

#[test]
fn csv_cells_round_trip_through_the_export_quoting() {
    for value in ["plain", "=SUM(A1)", "  -1", "@x", "+", "'=already", "with \"quotes\", commas\r\nand lines", ""] {
        let cell = crate::recording_export::text_cell(value);
        let mut records = Records { bytes: format!("{cell},{cell}\r\n").as_bytes(), at: 0 }.next_record_owned();
        let parsed = stored_text(records.remove(0));
        assert_eq!(crate::recording_export::text_cell(&parsed), cell, "{value}");
    }
    assert_eq!(stored_values("[\"OVR\",1.5,\"-OVR\"]"), (Some("[null,1.5,null]".into()), crate::over_range::pack(0b101, 0b100)));
    assert_eq!(stored_values("[1,2]"), (Some("[1,2]".into()), None));
    assert_eq!(stored_values(""), (None, None));
}

impl Records<'_> {
    fn next_record_owned(&mut self) -> Vec<String> {
        self.next_record().unwrap().unwrap()
    }
}
