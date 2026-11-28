//! Station records a replay bundle carries (`docs/backend/replay-bundle.md`,
//! "Station records"): the checklist document and its exports, and the
//! launch recovery history exports, as the station answered them at bundle
//! time, so the Data Export panel's checklist and recovery buttons and the
//! Checklists tab work offline.
//!
//! These are station records, not recording data. Each body is stored byte
//! for byte as its route answered (no JSON re-serialization), and is not
//! filtered to the recording: the checklist document is the station's one
//! current document (its sign-offs are an audit trail with no recording
//! link), and recovery events name a launch session, which the station does
//! not map to recordings. Both are small.
//!
//! The checklist routes refuse anonymous callers (403), so a bundle made
//! without signing in lists them under `missing`. Leave them out with
//! `records=false`.
use axum::{Router, http::HeaderMap};
use serde_json::{Value, json};

/// (entry path, route, media type).
pub(crate) const RECORDS: [(&str, &str, &str); 5] = [
    ("records/checklists.json", "/api/checklists", "application/json"),
    ("records/checklists_export.json", "/api/checklists/export", "application/json"),
    ("records/checklist_signoffs.csv", "/api/checklists/export.csv", "text/csv"),
    ("records/recovery_history.json", "/api/recovery/export.json", "application/json"),
    ("records/recovery_history.csv", "/api/recovery/export.csv", "text/csv"),
];

/// The record entries the station serves (path, bytes, media type, route) and the missing ones.
pub(crate) async fn entries(router: &Router, headers: &HeaderMap) -> (Vec<(String, Vec<u8>, &'static str, String)>, Vec<Value>) {
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for (path, route, media_type) in RECORDS {
        match crate::recording_bundle::fetch(router, headers, route).await {
            Ok(bytes) => found.push((path.to_owned(), bytes, media_type, route.to_owned())),
            Err(status) => missing.push(json!({"path": path, "route": route, "status": status})),
        }
    }
    (found, missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_export::tests::{TestDirectory, export_state};
    use tower::ServiceExt;

    async fn get(app: &Router, route: &str, token: Option<&str>) -> (u16, Vec<u8>) {
        let mut request = axum::http::Request::get(route);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = app.clone().oneshot(request.body(axum::body::Body::empty()).unwrap()).await.unwrap();
        let status = response.status().as_u16();
        (status, axum::body::to_bytes(response.into_body(), 1 << 24).await.unwrap().to_vec())
    }

    /// The bundle's record entries are the station's answers byte for byte,
    /// under their routes; an anonymous bundle lists the checklist ones as missing.
    #[tokio::test]
    async fn records_are_the_station_answers_at_bundle_time() {
        // The checklist store is one per process: another test changing it
        // between the bundle and the GET below made them differ. A process
        // of its own, with private configuration, compares them alone.
        if crate::operator_queue::isolated_case(module_path!(), "records_are_the_station_answers_at_bundle_time") {
            return;
        }
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"), json!({"anonymous":{"view_data":true},"users":[{"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}]}).to_string()).unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('records-viewer','viewer','session',1,0,0,9999999999999)")
            .execute(&state.auth_db)
            .await
            .unwrap();
        let id = "groundstation_recording_2026-03-01_00-00-00_000.db";
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id)).await.unwrap();
        sqlx::query("INSERT INTO telemetry(timestamp_ms,sender_id,data_type,values_json) VALUES(1772323200000,'FC','BAROMETER_DATA','[1.0,2.0,3.0]')").execute(&db).await.unwrap();
        db.close().await;
        let app = crate::web::router(state.clone(), String::new());
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer records-viewer".parse().unwrap());
        let options = crate::recording_bundle::Options { records: Some(true), ..Default::default() };
        let bundle = crate::recording_bundle::build(state.clone(), &headers, id, &options, 0).await.unwrap();
        let files = crate::recording_import::unzip(&bundle).unwrap();
        let manifest: Value = serde_json::from_slice(&files["manifest.json"]).unwrap();
        for (path, route, _) in RECORDS {
            let (status, body) = get(&app, route, Some("records-viewer")).await;
            assert_eq!(status, 200, "{route}");
            assert_eq!(files[path], body, "{path} is the station's {route} answer");
            let listed = manifest["entries"].as_array().unwrap().iter().find(|e| e["path"] == path).unwrap();
            assert_eq!(listed["route"], route);
        }
        // Anonymous: the recovery history is bundled, the checklists are not.
        let bundle = crate::recording_bundle::build(state.clone(), &HeaderMap::new(), id, &options, 0).await.unwrap();
        let files = crate::recording_import::unzip(&bundle).unwrap();
        let manifest: Value = serde_json::from_slice(&files["manifest.json"]).unwrap();
        assert!(files.contains_key("records/recovery_history.csv"));
        assert!(!files.contains_key("records/checklists.json"));
        assert!(manifest["missing"].as_array().unwrap().iter().any(|m| m["route"] == "/api/checklists" && m["status"] == 403));
        // records=false leaves them out.
        let none = crate::recording_bundle::Options { records: Some(false), ..Default::default() };
        let files = crate::recording_import::unzip(&crate::recording_bundle::build(state, &headers, id, &none, 0).await.unwrap()).unwrap();
        assert!(files.keys().all(|path| !path.starts_with("records/")));
    }
}
