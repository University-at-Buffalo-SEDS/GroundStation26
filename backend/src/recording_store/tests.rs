use super::*;
use crate::recording_export::tests::{TestDirectory, export_state};
use crate::telemetry_db::{DbQueueItem, DbWrite, RecordingCommand};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const T0: i64 = 1_800_000_000_000;

async fn write_segment(dir: &Path, start_ms: i64, rows: &[(i64, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{SEGMENT_PREFIX}{}.db", stamp(start_ms)));
    let (db, _) = crate::telemetry_db::open_telemetry_db(&path).await.unwrap();
    sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type,sender_id,payload_json) VALUES(?,'GS_STATION_MODE','GS','{\"mode\":\"normal\",\"igt_active\":false}')")
        .bind(start_ms)
        .execute(&db)
        .await
        .unwrap();
    for (ts, ty) in rows {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(?,?,'DAQ','[1.5]','[0]')")
            .bind(ts)
            .bind(ty)
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO flight_state(timestamp_ms,f_state) VALUES(?,1)").bind(start_ms).execute(&db).await.unwrap();
    crate::telemetry_db::close_and_finalize_sqlite(db, &path.to_string_lossy()).await;
    path
}

fn segment(name_ms: i64, bytes: u64) -> SegmentFile {
    SegmentFile {
        name: format!("{SEGMENT_PREFIX}{}.db", stamp(name_ms)),
        path: PathBuf::from(format!("/ring/{name_ms}.db")),
        start_ms: name_ms,
        bytes,
    }
}

fn capture(kind: EventKind, start: i64, pre_s: i64, end: i64, post_s: i64) -> Capture {
    Capture {
        serial: 1,
        kind,
        key: "k".into(),
        label: "Test capture".into(),
        event_start_ms: start,
        window_start_ms: start - pre_s * 1000,
        event_end_ms: Some(end),
        post_roll_ms: post_s * 1000,
        max_end_ms: end + 3_600_000,
        result: Some("Passed: done".into()),
        operator: Some("operator".into()),
        station_mode: "normal".into(),
        provenance: Some("hardware".into()),
        details: serde_json::Value::Null,
        materializing: false,
    }
}

#[test]
fn captures_record_their_provenance_and_older_ones_read_as_unknown() {
    let store = Store::default();
    store.enable_rolling();
    let request = BeginRequest {
        kind: EventKind::Manual,
        key: "m",
        label: "Manual",
        operator: None,
        details: serde_json::Value::Null,
    };
    let _ = store.begin(request, 1_000, "hitl");
    let captures = store.captures();
    // this process is neither the firmware nor the internal simulation
    assert!(captures.iter().all(|c| c.provenance.as_deref() == Some("hardware")));
    let mut old = serde_json::to_value(&captures[0]).unwrap();
    old.as_object_mut().unwrap().remove("provenance");
    let old: Capture = serde_json::from_value(old).unwrap();
    assert_eq!(old.provenance, None);
}

async fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn admin_state(root: &Path) -> Arc<AppState> {
    let state = export_state(root, true).await;
    std::fs::write(root.join("auth.json"), serde_json::json!({"anonymous":{"view_data":true},"users":[{"username":"rec-admin","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}]}).to_string()).unwrap();
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms)VALUES('rec-token','rec-admin','session',1,1,0,9999999999999)").execute(&state.auth_db).await.unwrap();
    state
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    assert!(status.is_success(), "{status}: {}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// Identifiers and settings
// ---------------------------------------------------------------------------

#[test]
fn ids_cover_legacy_segments_captures_and_attachments_and_reject_paths() {
    assert_eq!(
        RecordingId::parse("groundstation_recording_2026-11-09_06-26-25_821.db"),
        Some(RecordingId::Legacy("groundstation_recording_2026-11-09_06-26-25_821.db".into()))
    );
    let segment = RecordingId::parse("rolling.segment_2026-11-20_12-00-00_000.db").unwrap();
    assert_eq!(segment.relative_path(), Path::new("recordings/rolling/segment_2026-11-20_12-00-00_000.db"));
    let event = RecordingId::parse("event.nitrogen_test.2026-11-20_12-00-00_000_n2.db").unwrap();
    assert_eq!(
        event.relative_path(),
        Path::new("recordings/events/nitrogen_test/2026-11-20_12-00-00_000_n2/recording.db")
    );
    let attachment = RecordingId::parse("event.launch.f1.groundstation_recording_123.db").unwrap();
    assert_eq!(attachment.relative_path(), Path::new("recordings/events/launch/f1/groundstation_recording_123.db"));
    assert_eq!(event.sibling("groundstation_recording_9.db").unwrap(), "event.nitrogen_test.2026-11-20_12-00-00_000_n2.groundstation_recording_9.db");
    assert_eq!(segment.sibling("x.db"), None, "no SD derivative beside a ring segment");
    for bad in [
        "../auth.db",
        "event.launch..db",
        "event.launch.a/b.db",
        "event.unknown.f.db",
        "event.launch.f.recording.db",
        "rolling.other_1.db",
        "rolling.segment_../x.db",
        "event.launch.f.g.h.db",
        "groundstation_recording_.db",
    ] {
        assert!(RecordingId::parse(bad).is_none(), "{bad}");
    }
    assert_eq!(segment_stamp_ms(&format!("{SEGMENT_PREFIX}{}.db", stamp(T0 + 123))), Some(T0 + 123));
}

#[test]
fn settings_normalize_into_safe_ranges_and_keep_every_event_type() {
    let mut settings = Settings { ring_budget_mb: 1, segment_mb: 100_000, critical_free_mb: 1, warn_free_mb: 0, ..Default::default() };
    settings.events.clear();
    let settings = settings.normalized();
    assert_eq!(settings.ring_budget_mb, 256);
    assert_eq!(settings.segment_mb, 64, "a segment is at most a quarter of the ring");
    assert!(settings.warn_free_mb >= settings.critical_free_mb);
    assert_eq!(settings.events.len(), EventKind::ALL.len());
    let defaults = Settings::default();
    assert_eq!(defaults.ring_budget_mb, 50 * 1024, "50 GB unless the disk fills first");
    assert!(defaults.reserve_free_mb >= defaults.warn_free_mb);
    assert_eq!(defaults.policy(EventKind::Launch).pre_roll_s, 600);
    assert!(EventKind::ALL.iter().all(|k| defaults.policy(*k).auto));
    let text = serde_json::to_string(&defaults).unwrap();
    assert!(text.contains("\"nitrogen_test\""));
    assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), defaults);
}

// ---------------------------------------------------------------------------
// Ring budget, rollover and the low-disk policy
// ---------------------------------------------------------------------------

#[test]
fn ring_rollover_deletes_the_oldest_segments_down_to_the_budget() {
    let segments: Vec<_> = (0..5).map(|i| segment(T0 + i * 60_000, 100)).collect();
    let active = segments[4].path.clone();
    // 500 bytes against a 250-byte budget: the three oldest go, oldest first.
    let deleted = plan_deletions(&segments, Some(&active), None, 250, None, 0);
    assert_eq!(deleted, vec![segments[0].path.clone(), segments[1].path.clone(), segments[2].path.clone()]);
    assert!(plan_deletions(&segments, Some(&active), None, 500, None, 0).is_empty(), "within budget");
    // Never the active segment, even with a zero budget.
    let all = plan_deletions(&segments, Some(&active), None, 0, None, 0);
    assert_eq!(all.len(), 4);
    assert!(!all.contains(&active));
}

#[test]
fn pinned_segments_survive_rollover_until_the_capture_is_copied() {
    let segments: Vec<_> = (0..5).map(|i| segment(T0 + i * 600_000, 100)).collect();
    // A capture still needs data from inside segment 2.
    let pin = segments[2].start_ms + 120_000;
    let deleted = plan_deletions(&segments, Some(&segments[4].path), Some(pin), 0, None, 0);
    assert_eq!(deleted, vec![segments[0].path.clone(), segments[1].path.clone()], "no hole and no pinned data");
}

#[test]
fn low_disk_shrinks_the_ring_even_under_budget_but_never_past_pins_or_active() {
    let segments: Vec<_> = (0..4).map(|i| segment(T0 + i * 600_000, 1000)).collect();
    // Within the budget, but only 500 bytes free against a 2500-byte floor.
    let deleted = plan_deletions(&segments, Some(&segments[3].path), None, 1_000_000, Some(500), 2_500);
    assert_eq!(deleted.len(), 2);
    let pinned = plan_deletions(&segments, Some(&segments[3].path), Some(segments[0].start_ms), 1_000_000, Some(0), 1_000_000);
    assert!(pinned.is_empty(), "a pending capture's pre-roll is never removed");
}

#[tokio::test]
async fn maintenance_applies_the_disk_policy_and_never_touches_captures() {
    let root = TestDirectory::new();
    let state = crate::state::tests::test_app_state().await;
    state.recording_store.enable_rolling();
    let rolling = rolling_dir(&root.0);
    let mut paths = Vec::new();
    for i in 0..3 {
        paths.push(write_segment(&rolling, T0 + i * 600_000, &[(T0 + i * 600_000 + 1, "RAW")]).await);
    }
    let saved = materialize(&root.0, &capture(EventKind::Fill, T0 + 10, 0, T0 + 20, 0), T0 + 30).await.unwrap();
    let captures_before = dir_bytes(&events_dir(&root.0));
    // Critically low disk: everything but the active segment goes.
    let deleted = maintain_ring(&state, &root.0, Some(&paths[2]), Some(0));
    assert_eq!(deleted, paths[..2].to_vec());
    let status = state.recording_store.ring_status();
    assert_eq!(status.disk, DiskState::Critical);
    assert!(status.warning.unwrap().contains("never deleted automatically"));
    assert_eq!(status.segments, 1);
    assert!(saved.folder.join(CAPTURE_FILE).exists() && saved.folder.join(MANIFEST_FILE).exists());
    assert_eq!(dir_bytes(&events_dir(&root.0)), captures_before, "captures are outside the ring");
    // Low but above the critical floor: warn only.
    maintain_ring(&state, &root.0, Some(&paths[2]), Some(1024 * MIB));
    let status = state.recording_store.ring_status();
    assert_eq!(status.disk, DiskState::Low);
    assert_eq!(status.segments, 1);
}

// ---------------------------------------------------------------------------
// Capture copies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capture_pre_roll_comes_from_older_ring_segments_with_context() {
    let root = TestDirectory::new();
    let rolling = rolling_dir(&root.0);
    // Segment A holds the pre-roll; the event starts in segment B.
    write_segment(&rolling, T0, &[(T0 + 1_000, "BEFORE_WINDOW"), (T0 + 50_000, "PRE_ROLL"), (T0 + 59_000, "PRE_ROLL")]).await;
    write_segment(&rolling, T0 + 60_000, &[(T0 + 61_000, "EVENT"), (T0 + 90_000, "POST_ROLL"), (T0 + 200_000, "AFTER")]).await;
    let mut c = capture(EventKind::Fill, T0 + 60_000, 20, T0 + 80_000, 15);
    c.label = "Fill / top-off #1".into();
    let saved = materialize(&root.0, &c, T0 + 300_000).await.unwrap();
    assert!(saved.folder.starts_with(events_dir(&root.0).join("fill")));
    assert!(saved.folder.file_name().unwrap().to_string_lossy().ends_with("_fill-top-off-1"));
    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", saved.folder.join(CAPTURE_FILE).display())).await.unwrap();
    let types: Vec<String> = sqlx::query_scalar("SELECT data_type FROM telemetry ORDER BY id").fetch_all(&db).await.unwrap();
    assert_eq!(types, ["GS_STATION_MODE", "PRE_ROLL", "PRE_ROLL", "GS_STATION_MODE", "EVENT", "POST_ROLL"]);
    let first_state: i64 = sqlx::query_scalar("SELECT f_state FROM flight_state ORDER BY id LIMIT 1").fetch_one(&db).await.unwrap();
    assert_eq!(first_state, 1, "flight state in force at the window start");
    db.close().await;
    let manifest = read_manifest(&saved.folder).unwrap();
    assert_eq!(manifest.event_type, EventKind::Fill);
    assert_eq!(manifest.pre_roll_s, 20);
    assert_eq!(manifest.post_roll_s, 15);
    assert_eq!(manifest.result.as_deref(), Some("Passed: done"));
    assert_eq!(manifest.operator.as_deref(), Some("operator"));
    assert_eq!(manifest.source_segments.len(), 2);
    assert!(!manifest.pre_roll_truncated);
    assert_eq!(manifest.rows["telemetry"], 6);
    assert!(manifest.versions["groundstation_backend"].is_string());
    assert!(crate::telemetry_db::sqlite_sidecars_present(&saved.folder.join(CAPTURE_FILE).to_string_lossy()).is_empty());
}

#[tokio::test]
async fn capture_reports_a_pre_roll_the_ring_no_longer_holds() {
    let root = TestDirectory::new();
    write_segment(&rolling_dir(&root.0), T0 + 100_000, &[(T0 + 101_000, "EVENT")]).await;
    let saved = materialize(&root.0, &capture(EventKind::Abort, T0 + 100_500, 600, T0 + 102_000, 0), T0 + 200_000).await.unwrap();
    let manifest = read_manifest(&saved.folder).unwrap();
    assert!(manifest.pre_roll_truncated);
    assert_eq!(manifest.data_from_ms, Some(T0 + 100_000));
}

#[tokio::test]
async fn capture_copies_safely_while_the_active_segment_keeps_writing() {
    let root = TestDirectory::new();
    let rolling = rolling_dir(&root.0);
    std::fs::create_dir_all(&rolling).unwrap();
    let path = rolling.join(format!("{SEGMENT_PREFIX}{}.db", stamp(T0)));
    let (writer, _) = crate::telemetry_db::open_telemetry_db(&path).await.unwrap();
    for i in 0..50 {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(?,'LIVE')").bind(T0 + i).execute(&writer).await.unwrap();
    }
    let keep_writing = {
        let writer = writer.clone();
        tokio::spawn(async move {
            for i in 50..400 {
                sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(?,'LIVE')").bind(T0 + i).execute(&writer).await.unwrap();
            }
        })
    };
    let saved = materialize(&root.0, &capture(EventKind::Manual, T0, 0, T0 + 49, 0), T0 + 10_000).await.unwrap();
    keep_writing.await.unwrap();
    assert_eq!(read_manifest(&saved.folder).unwrap().rows["telemetry"], 50);
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM telemetry").fetch_one(&writer).await.unwrap();
    assert_eq!(live, 400, "the writer was never blocked or rolled back");
    writer.close().await;
}

// ---------------------------------------------------------------------------
// Triggers
// ---------------------------------------------------------------------------

fn snapshot(kind: crate::sequence_progress::Kind, run: &str, outcome: crate::sequence_progress::Outcome) -> crate::sequence_progress::Snapshot {
    crate::sequence_progress::Snapshot {
        version: 1,
        key: crate::sequence_progress::RunKey {
            backend_instance: "0".repeat(32),
            reset_epoch: "0".repeat(16),
            run_id: run.into(),
        },
        revision: 1,
        kind,
        outcome,
        origin_client: Some("operator-1".into()),
        title: format!("{kind:?}"),
        message: "fixture".into(),
        current_step: None,
        next_step: None,
        steps: vec![],
        observed_age_ms: 0,
        valid_for_ms: 2000,
        cancel_allowed: false,
        cold_flow_release_allowed: false,
        notification_id: None,
        notification_timestamp_ms: None,
        reporting_error: None,
        watch_history_truncated: false,
        details_complete: true,
    }
}

#[tokio::test]
async fn captures_are_inert_until_the_ring_is_enabled() {
    let state = crate::state::tests::test_app_state().await;
    recording_store_observe_all(&state);
    assert!(state.recording_store.captures().is_empty());
}

fn recording_store_observe_all(state: &AppState) {
    use crate::sequence_progress::{Kind, Outcome};
    observe_flight_state(state, crate::types::FlightState::Launch as i64);
    observe_flight_state(state, crate::types::FlightState::Aborted as i64);
    observe_sequence(state, &snapshot(Kind::Fill, "fill-run", Outcome::Running));
}

#[tokio::test]
async fn each_station_sequence_run_gets_its_own_capture_with_result() {
    use crate::sequence_progress::{Kind, Outcome};
    let state = crate::state::tests::test_app_state().await;
    state.recording_store.enable_rolling();
    for (kind, run, expected) in [
        (Kind::Fill, "a1", EventKind::Fill),
        (Kind::NitrogenTest, "a2", EventKind::NitrogenTest),
        (Kind::ValveSelfTest, "a3", EventKind::ValveSelfTest),
        (Kind::Turbocharge, "a4", EventKind::Turbocharge),
        (Kind::ColdFlow, "a5", EventKind::ColdFlow),
        (Kind::IgniterSequence, "a6", EventKind::Igniter),
        (Kind::ContinueFillSequence, "a7", EventKind::Fill),
    ] {
        observe_sequence(&state, &snapshot(kind, run, Outcome::Running));
        observe_sequence(&state, &snapshot(kind, run, Outcome::Running));
        assert!(state.recording_store.is_open(expected, run), "{kind:?}");
        observe_sequence(&state, &snapshot(kind, run, Outcome::Passed));
        assert!(!state.recording_store.is_open(expected, run));
        let c = state.recording_store.captures().into_iter().find(|c| c.key == run).unwrap();
        assert_eq!(c.kind, expected);
        assert_eq!(c.result.as_deref(), Some("Passed: fixture"));
        assert_eq!(c.operator.as_deref(), Some("operator-1"));
        let policy = Settings::default().policy(expected);
        assert_eq!(c.event_start_ms - c.window_start_ms, i64::from(policy.pre_roll_s) * 1000);
        assert_eq!(c.post_roll_ms, i64::from(policy.post_roll_s) * 1000);
    }
    assert_eq!(state.recording_store.captures().len(), 7, "one capture per run, never duplicated");
    // A run seen only at its end still gets a capture; a cancelled run says so.
    observe_sequence(&state, &snapshot(Kind::NitrogenTest, "late", Outcome::Cancelled));
    let late = state.recording_store.captures().into_iter().find(|c| c.key == "late").unwrap();
    assert_eq!(late.result.as_deref(), Some("Cancelled: fixture"));
    assert_eq!(state.recording_status_snapshot().captures.len(), 8, "clients see captures in progress");
}

#[tokio::test]
async fn launch_and_abort_triggers_follow_the_flight_state() {
    use crate::types::FlightState as F;
    let state = crate::state::tests::test_app_state().await;
    state.recording_store.enable_rolling();
    observe_flight_state(&state, F::Armed as i64);
    assert!(state.recording_store.captures().is_empty(), "arming alone is not a launch");
    observe_flight_state(&state, F::Launch as i64);
    observe_flight_state(&state, F::InFlight as i64);
    assert!(state.recording_store.is_open(EventKind::Launch, "flight"));
    let launch = &state.recording_store.captures()[0];
    assert_eq!(launch.event_start_ms - launch.window_start_ms, 600_000, "ten-minute pre-roll covers arming");
    observe_flight_state(&state, F::Landed as i64);
    assert!(!state.recording_store.is_open(EventKind::Launch, "flight"));
    assert_eq!(state.recording_store.captures()[0].result.as_deref(), Some("Landed"));
    // Abort: always captured, open while aborted.
    observe_flight_state(&state, F::Aborted as i64);
    assert!(state.recording_store.is_open(EventKind::Abort, "abort"));
    observe_flight_state(&state, F::Idle as i64);
    assert!(!state.recording_store.is_open(EventKind::Abort, "abort"));
    // An abort during flight ends the launch capture too.
    observe_flight_state(&state, F::Launch as i64);
    observe_flight_state(&state, F::Aborted as i64);
    let kinds: Vec<_> = state.recording_store.captures().iter().map(|c| (c.kind, c.result.clone())).collect();
    assert!(kinds.contains(&(EventKind::Launch, Some("Aborted".into()))), "{kinds:?}");
    assert!(state.recording_store.is_open(EventKind::Abort, "abort"));
}

#[tokio::test]
async fn disabled_auto_capture_and_max_duration_are_honoured() {
    use crate::types::FlightState as F;
    let state = crate::state::tests::test_app_state().await;
    state.recording_store.enable_rolling();
    let mut settings = Settings::default();
    settings.events.get_mut(&EventKind::Abort).unwrap().auto = false;
    state.recording_store.set_settings(settings);
    observe_flight_state(&state, F::Aborted as i64);
    assert!(state.recording_store.captures().is_empty());
    let store = &state.recording_store;
    store.begin(BeginRequest { kind: EventKind::GroundTest, key: "igt", label: "IGT", operator: None, details: serde_json::Value::Null }, T0, "integrated_ground_test");
    assert!(store.due(T0 + 1000).is_empty());
    let due = store.due(T0 + 8 * 3_600_000 + FLUSH_GRACE_MS);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].result.as_deref(), Some("Maximum capture duration reached"));
    assert!(store.due(T0 + 9 * 3_600_000).is_empty(), "handed out once");
}

#[tokio::test]
async fn manual_snapshot_api_saves_a_labelled_protected_capture() {
    let root = TestDirectory::new();
    let state = admin_state(&root.0).await;
    let app = crate::recording_export::routes().with_state(state.clone());
    let request = || {
        Request::post("/api/recordings/snapshot")
            .header("content-type", "application/json")
            .header("authorization", "Bearer rec-token")
            .body(Body::from(r#"{"label":"Leak check","before_s":30}"#))
            .unwrap()
    };
    assert_eq!(app.clone().oneshot(request()).await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE, "no ring, no snapshot");
    state.recording_store.enable_rolling();
    let now = now_ms();
    write_segment(&rolling_dir(&root.0), now - 60_000, &[(now - 20_000, "SNAP"), (now - 50_000, "TOO_OLD")]).await;
    let response = body_json(app.clone().oneshot(request()).await.unwrap()).await;
    assert_eq!(response["label"], "Leak check");
    let c = &state.recording_store.captures()[0];
    assert_eq!((c.kind, c.event_start_ms - c.window_start_ms), (EventKind::Manual, 30_000));
    assert_eq!(c.operator.as_deref(), Some("rec-admin"));
    tokio::time::sleep(Duration::from_millis(FLUSH_GRACE_MS as u64 + 100)).await;
    let results = run_due(&state, &root.0).await;
    assert_eq!(results.len(), 1);
    let saved = results.into_iter().next().unwrap().unwrap();
    let manifest = read_manifest(&saved.folder).unwrap();
    assert_eq!(manifest.label, "Leak check");
    assert_eq!(manifest.rows["telemetry"], 2, "context row + the one row in the window");
    assert!(state.recording_store.captures().is_empty());
    assert!(state.notifications_snapshot().iter().any(|n| n.message.contains("Saved Manual snapshot capture")));
}

// ---------------------------------------------------------------------------
// Export, replay, listing, deletion, legacy migration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capture_round_trips_through_csv_replay_report_listing_and_admin_delete() {
    let root = TestDirectory::new();
    let state = admin_state(&root.0).await;
    state.recording_store.enable_rolling();
    write_segment(&rolling_dir(&root.0), T0, &[(T0 + 1_000, "ATTITUDE"), (T0 + 2_000, "ATTITUDE")]).await;
    let saved = materialize(&root.0, &capture(EventKind::Launch, T0 + 1_500, 1, T0 + 2_500, 0), T0 + 5_000).await.unwrap();
    // A legacy recording made before the ring existed.
    let legacy = "groundstation_recording_2026-11-09_06-26-25_821.db";
    let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(legacy)).await.unwrap();
    sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type) VALUES(5,'LEGACY_ROW')").execute(&db).await.unwrap();
    db.close().await;
    let app = crate::recording_export::routes().with_state(state.clone());
    let get = |uri: String| Request::get(uri).header("authorization", "Bearer rec-token").body(Body::empty()).unwrap();

    // CSV export of the capture.
    let response = app.clone().oneshot(get(format!("/api/recordings/{}/csv", saved.id))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let csv = String::from_utf8(to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    assert_eq!(csv.matches("ATTITUDE").count(), 2, "{csv}");
    // Replay of the capture.
    let replay = body_json(app.clone().oneshot(get(format!("/api/recordings/{}/replay", saved.id))).await.unwrap()).await;
    assert!(replay.is_object());
    // Raw file and manifest downloads.
    let response = app.clone().oneshot(get(format!("/api/recordings/{}/file", saved.id))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 24).await.unwrap();
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    let manifest = body_json(app.clone().oneshot(get(format!("/api/recordings/{}/manifest", saved.id))).await.unwrap()).await;
    assert_eq!(manifest["event_type"], "launch");

    // The legacy recording is still listed, resolvable and exportable.
    let list = body_json(app.clone().oneshot(get("/api/recordings".into())).await.unwrap()).await;
    let kinds: Vec<(String, String)> = list.as_array().unwrap().iter().map(|r| (r["id"].as_str().unwrap().to_owned(), r["kind"].as_str().unwrap().to_owned())).collect();
    assert!(kinds.contains(&(legacy.to_owned(), "legacy".into())), "{kinds:?}");
    assert!(kinds.contains(&(saved.id.clone(), "capture".into())));
    assert!(kinds.iter().any(|(id, kind)| id.starts_with("rolling.segment_") && kind == "rolling"));
    let response = app.clone().oneshot(get(format!("/api/recordings/{legacy}/csv"))).await.unwrap();
    let csv = String::from_utf8(to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    assert!(csv.contains("LEGACY_ROW"));

    // The recordings page listing.
    let store = body_json(app.clone().oneshot(get("/api/recordings/store".into())).await.unwrap()).await;
    assert_eq!(store["always_on"], true);
    assert_eq!(store["is_admin"], true);
    assert_eq!(store["groups"][0]["event_type"], "launch");
    assert_eq!(store["groups"][0]["captures"][0]["id"], saved.id.as_str());
    assert_eq!(store["groups"][0]["captures"][0]["manifest"]["label"], "Test capture");
    assert_eq!(store["legacy"][0]["id"], legacy);
    assert!(store["settings"]["events"]["fill"]["pre_roll_s"].is_number());

    // Date-range CSV reads the ring and legacy files, never duplicate captures.
    let response = app.clone().oneshot(get(format!("/api/recordings/csv?start_ms={}&end_ms={}", T0, T0 + 10_000))).await.unwrap();
    let csv = String::from_utf8(to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    assert_eq!(csv.matches("ATTITUDE").count(), 2, "{csv}");

    // Segments are never deleted by hand; captures only by an administrator.
    let segment_id = kinds.iter().find(|(_, k)| k == "rolling").unwrap().0.clone();
    let del = |uri: String, auth: bool| {
        let mut b = Request::delete(uri);
        if auth {
            b = b.header("authorization", "Bearer rec-token");
        }
        b.body(Body::empty()).unwrap()
    };
    assert_eq!(app.clone().oneshot(del(format!("/api/recordings/{segment_id}"), true)).await.unwrap().status(), StatusCode::CONFLICT);
    assert_eq!(app.clone().oneshot(del(format!("/api/recordings/{}", saved.id), false)).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(app.clone().oneshot(del(format!("/api/recordings/{}", saved.id), true)).await.unwrap().status(), StatusCode::OK);
    assert!(!saved.folder.exists());
    assert!(state.notifications_snapshot().iter().any(|n| n.message.contains("deleted by rec-admin")));
}

#[tokio::test]
async fn settings_api_is_admin_only_and_persists_normalized_values() {
    let root = TestDirectory::new();
    let state = admin_state(&root.0).await;
    let app = crate::recording_export::routes().with_state(state.clone());
    let mut settings = Settings::default();
    settings.ring_budget_mb = 8192;
    settings.events.get_mut(&EventKind::Fill).unwrap().pre_roll_s = 300;
    let put = |auth: bool| {
        let mut b = Request::put("/api/recordings/settings").header("content-type", "application/json");
        if auth {
            b = b.header("authorization", "Bearer rec-token");
        }
        b.body(Body::from(serde_json::to_string(&settings).unwrap())).unwrap()
    };
    assert_eq!(app.clone().oneshot(put(false)).await.unwrap().status(), StatusCode::FORBIDDEN);
    body_json(app.clone().oneshot(put(true)).await.unwrap()).await;
    assert_eq!(load_settings(&root.0).ring_budget_mb, 8192);
    assert_eq!(state.recording_store.settings().policy(EventKind::Fill).pre_roll_s, 300);
}

// ---------------------------------------------------------------------------
// The DB actor: always on, rotation, triggers, restart recovery
// ---------------------------------------------------------------------------

fn telemetry(ts: i64, ty: &str) -> DbQueueItem {
    DbQueueItem::Write(DbWrite::Telemetry {
        timestamp_ms: ts,
        source_timestamp_ms: None,
        data_type: ty.into(),
        sender_id: "DAQ".into(),
        values_json: Some("[1]".into()),
        payload_json: "[1]".into(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_records_from_start_rotates_never_pauses_and_captures_a_launch() {
    use crate::types::FlightState as F;
    let root = TestDirectory::new();
    let mut state = crate::state::tests::test_app_state().await;
    state.recording_store.enable_rolling();
    // Launch with a short post-roll so the copy happens inside the test.
    let mut settings = Settings::default();
    settings.events.get_mut(&EventKind::Launch).unwrap().post_roll_s = 0;
    save_settings(&root.0, &settings).unwrap();
    let owner = crate::telemetry_task::recording_owner::TestOwner::start(&mut state, &root.0);
    wait_for(|| state.recording_status_snapshot().db_path.is_some(), "recording to start without any command").await;
    let status = state.recording_status_snapshot();
    assert_eq!(status.mode, crate::telemetry_db::RecordingModeWire::Recording);
    assert!(status.always_on);
    let first = status.db_path.clone().unwrap();
    assert!(first.contains("/recordings/rolling/segment_"));
    assert!(state.fill_recording.ready_instance() != 0, "fill sees a ready recording at once");

    let now = now_ms();
    state.db_queue_tx.send(telemetry(now - 5_000, "PRE_LAUNCH")).await.unwrap();
    // Pause/Stop cannot stop the safety record.
    state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::Pause)).await.unwrap();
    state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::Stop)).await.unwrap();
    state.db_queue_tx.send(telemetry(now - 4_000, "AFTER_PAUSE")).await.unwrap();
    state.db_queue_tx.send(DbQueueItem::Write(DbWrite::FlightState { timestamp_ms: now - 3_000, state_code: F::Launch as i64 })).await.unwrap();
    wait_for(|| state.recording_store.is_open(EventKind::Launch, "flight"), "launch capture").await;
    state.db_queue_tx.send(telemetry(now - 2_000, "IN_FLIGHT")).await.unwrap();
    // Rotation closes the segment cleanly and opens the next one.
    state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::RotateSegment)).await.unwrap();
    wait_for(|| state.recording_status_snapshot().db_path.as_deref() != Some(first.as_str()), "rotation").await;
    assert!(crate::telemetry_db::sqlite_sidecars_present(&first).is_empty(), "closed segment finalized");
    state.db_queue_tx.send(telemetry(now - 1_000, "LANDING")).await.unwrap();
    state.db_queue_tx.send(DbQueueItem::Write(DbWrite::FlightState { timestamp_ms: now, state_code: F::Landed as i64 })).await.unwrap();
    let events = events_dir(&root.0).join("launch");
    wait_for(
        || std::fs::read_dir(&events).into_iter().flatten().flatten().any(|e| e.path().join(MANIFEST_FILE).exists()),
        "launch capture saved",
    )
    .await;
    assert_eq!(state.recording_status_snapshot().mode, crate::telemetry_db::RecordingModeWire::Recording);
    let folder = std::fs::read_dir(&events).unwrap().flatten().next().unwrap().path();
    let manifest = read_manifest(&folder).unwrap();
    assert_eq!(manifest.result.as_deref(), Some("Landed"));
    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", folder.join(CAPTURE_FILE).display())).await.unwrap();
    let types: Vec<String> = sqlx::query_scalar("SELECT data_type FROM telemetry WHERE sender_id='DAQ' ORDER BY id").fetch_all(&db).await.unwrap();
    db.close().await;
    assert_eq!(types, ["PRE_LAUNCH", "AFTER_PAUSE", "IN_FLIGHT", "LANDING"], "pre-roll from the earlier segment, nothing lost");
    assert_eq!(manifest.source_segments.len(), 2);
    drop(owner);
    assert_eq!(scan_segments(&rolling_dir(&root.0)).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_resumes_the_ring_and_finishes_interrupted_captures() {
    let root = TestDirectory::new();
    // A previous run: two segments and two captures in progress at the crash.
    let rolling = rolling_dir(&root.0);
    let now = now_ms();
    write_segment(&rolling, now - 120_000, &[(now - 110_000, "OLD")]).await;
    let crashed = write_segment(&rolling, now - 60_000, &[(now - 30_000, "FILLING")]).await;
    // Leave a stale WAL as a crash would.
    let (db, _) = crate::telemetry_db::open_telemetry_db(&crashed).await.unwrap();
    sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type,sender_id) VALUES(?,'LAST_BEFORE_CRASH','DAQ')").bind(now - 20_000).execute(&db).await.unwrap();
    std::mem::forget(db);
    let mut fill = capture(EventKind::Fill, now - 40_000, 10, now, 0);
    fill.event_end_ms = None;
    fill.key = "run".into();
    let mut launch = capture(EventKind::Launch, now - 35_000, 5, now, 0);
    launch.serial = 2;
    launch.event_end_ms = None;
    launch.key = "flight".into();
    std::fs::create_dir_all(events_dir(&root.0)).unwrap();
    std::fs::write(events_dir(&root.0).join(PENDING_FILE), serde_json::to_vec(&vec![fill, launch]).unwrap()).unwrap();
    let mut settings = Settings::default();
    settings.events.get_mut(&EventKind::Launch).unwrap().post_roll_s = 0;
    save_settings(&root.0, &settings).unwrap();

    let mut state = crate::state::tests::test_app_state().await;
    state.recording_store.enable_rolling();
    let owner = crate::telemetry_task::recording_owner::TestOwner::start(&mut state, &root.0);
    wait_for(|| state.recording_status_snapshot().db_path.is_some(), "ring resumed").await;
    assert_eq!(scan_segments(&rolling).len(), 3, "old segments kept, a new one opened");
    for (kind, result) in [(EventKind::Fill, "Interrupted"), (EventKind::Launch, "Station returned to idle")] {
        let dir = events_dir(&root.0).join(kind.id());
        wait_for(
            || std::fs::read_dir(&dir).into_iter().flatten().flatten().any(|e| e.path().join(MANIFEST_FILE).exists()),
            kind.id(),
        )
        .await;
        let folder = std::fs::read_dir(&dir).unwrap().flatten().next().unwrap().path();
        let manifest = read_manifest(&folder).unwrap();
        assert!(manifest.result.unwrap().starts_with(result), "{kind:?}");
    }
    let fill_dir = std::fs::read_dir(events_dir(&root.0).join("fill")).unwrap().flatten().next().unwrap().path();
    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", fill_dir.join(CAPTURE_FILE).display())).await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar("SELECT data_type FROM telemetry WHERE sender_id='DAQ' ORDER BY id").fetch_all(&db).await.unwrap();
    db.close().await;
    assert_eq!(rows, ["FILLING", "LAST_BEFORE_CRASH"], "the crashed segment's journal was recovered");
    drop(owner);
    assert!(load_pending(&root.0).is_empty());
}

#[tokio::test]
async fn incomplete_capture_copies_are_removed_at_startup() {
    let root = TestDirectory::new();
    let folder = events_dir(&root.0).join("fill").join("2026_half");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(PARTIAL_FILE), b"x").unwrap();
    let kept = events_dir(&root.0).join("fill").join("2026_done");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join(CAPTURE_FILE), b"x").unwrap();
    std::fs::write(kept.join(MANIFEST_FILE), b"{}").unwrap();
    remove_incomplete_captures(&root.0);
    assert!(!folder.exists());
    assert!(kept.exists());
}

// ---------------------------------------------------------------------------
// Disk-aware budget
// ---------------------------------------------------------------------------

#[test]
fn ring_budget_is_50_gb_or_the_disk_above_the_capture_reserve_whichever_is_smaller() {
    let settings = Settings::default().normalized();
    let gib = 1024 * MIB;
    // Plenty of disk: the 50 GB ceiling applies.
    assert_eq!(effective_budget(&settings, 10 * gib, Some(500 * gib)), 50 * gib);
    // A 32 GB card with 12 GB of ring and 10 GB free: the ring may grow by only
    // 2 GB more, keeping the 8 GB reserve for captures and the OS.
    assert_eq!(effective_budget(&settings, 12 * gib, Some(10 * gib)), 14 * gib);
    // Captures ate into the reserve: the budget falls below the ring, so the
    // oldest segments go until the reserve is free again.
    assert_eq!(effective_budget(&settings, 12 * gib, Some(4 * gib)), 8 * gib);
    assert_eq!(effective_budget(&settings, 1 * gib, Some(1 * gib)), 0);
    assert_eq!(effective_budget(&settings, 1 * gib, None), 50 * gib, "unknown free space: ceiling only");
    // The reserve never sits below the warning threshold.
    let tight = Settings { reserve_free_mb: 1, ..Default::default() }.normalized();
    assert_eq!(tight.reserve_free_mb, tight.warn_free_mb);
}

#[tokio::test]
async fn the_ring_stops_at_the_reserve_and_reports_its_effective_budget() {
    let root = TestDirectory::new();
    let state = crate::state::tests::test_app_state().await;
    state.recording_store.enable_rolling();
    let rolling = rolling_dir(&root.0);
    let mut paths = Vec::new();
    for i in 0..3 {
        paths.push(write_segment(&rolling, T0 + i * 600_000, &[(T0 + i * 600_000 + 1, "RAW")]).await);
    }
    let ring: u64 = scan_segments(&rolling).iter().map(|s| s.bytes).sum();
    let mut settings = Settings::default();
    settings.reserve_free_mb = 4096;
    state.recording_store.set_settings(settings);
    // Free space exactly at the reserve plus one segment: the ring is over its
    // effective budget by the other two... minus what fits.
    let one = scan_segments(&rolling)[0].bytes;
    let free = 4096 * MIB - ring + one;
    let deleted = maintain_ring(&state, &root.0, Some(&paths[2]), Some(free));
    assert_eq!(deleted, paths[..2].to_vec(), "oldest go first, the active one stays");
    let status = state.recording_store.ring_status();
    assert_eq!(status.budget_bytes, 50 * 1024 * MIB);
    assert_eq!(status.effective_budget_bytes, one);
    assert_eq!(status.disk, DiskState::Ok, "the reserve keeps the disk above the warning");
}

// ---------------------------------------------------------------------------
// Board SD launch files follow the launch capture
// ---------------------------------------------------------------------------

const BOARD_HEADER: &str = "timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence";

fn board_csv(path: &Path, rows: &[(i64, i64, &str)]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = format!("{BOARD_HEADER}\n# calibration,ch0,1,0\n");
    for (i, (ts, mono, source)) in rows.iter().enumerate() {
        // FC rows quote a space-separated vector in `value`; commas inside
        // quotes must not shift `time_source`.
        text.push_str(&format!("{ts},{mono},accelerometer_sample,\"1,5 2 3\",,,,{source},,,{mono},1000,{i}\n"));
    }
    std::fs::write(path, text).unwrap();
}

async fn launch_capture(root: &Path, kind: EventKind, start: i64, end: i64) -> CaptureSaved {
    write_segment(&rolling_dir(root), start - 120_000, &[(start, "EVENT")]).await;
    let mut c = capture(kind, start, 60, end, 30);
    c.label = kind.title().into();
    materialize(root, &c, end + 60_000).await.unwrap()
}

#[test]
fn board_file_names_and_quoted_rows_are_understood() {
    use board_sd::classify_name;
    assert_eq!(classify_name("irec_LAUNCH-1800000000-0.csv"), Some(("FC", true)));
    assert_eq!(classify_name("irec-1800000000-0.csv"), Some(("FC", false)));
    assert_eq!(classify_name("DAQ_LAUNCH_0001.CSV"), Some(("DAQ", true)));
    assert_eq!(classify_name("DAQ_0001.CSV"), Some(("DAQ", false)));
    assert_eq!(classify_name("notes.txt"), None);
    assert_eq!(classify_name("other.csv"), None);
    let dir = TestDirectory::new();
    let path = dir.0.join("irec_LAUNCH-1-0.csv");
    board_csv(&path, &[(5, 1, "local"), (T0 + 1_000, 2, "network"), (T0 + 9_000, 3, "network")]);
    let file = board_sd::scan(&path, "irec_LAUNCH-1-0.csv").unwrap();
    assert_eq!((file.board, file.launch, file.first_utc, file.last_utc, file.rows), ("FC", true, T0 + 1_000, T0 + 9_000, 2));
    let local_only = dir.0.join("DAQ_LAUNCH_2.CSV");
    board_csv(&local_only, &[(5, 1, "local")]);
    assert!(board_sd::scan(&local_only, "DAQ_LAUNCH_2.CSV").is_none(), "no network UTC: cannot be dated");
}

#[tokio::test]
async fn board_launch_files_attach_to_the_matching_launch_capture_with_a_lead_up() {
    use board_sd::Outcome;
    let root = TestDirectory::new();
    // An earlier fill and two launch captures; the board file belongs to the second.
    let first = launch_capture(&root.0, EventKind::Launch, T0, T0 + 120_000).await;
    let second = launch_capture(&root.0, EventKind::Launch, T0 + 3_600_000, T0 + 3_720_000).await;
    let igt = launch_capture(&root.0, EventKind::GroundTest, T0 + 3_000_000, T0 + 4_000_000).await;
    let incoming = root.0.join("incoming");
    // The DAQ launch file (T0..T+20 s of the second launch).
    let launch = incoming.join("DAQ_LAUNCH_0007.CSV");
    board_csv(&launch, &[(T0 + 3_600_000, 50_000, "network"), (T0 + 3_620_000, 70_000, "network")]);
    let out = board_sd::offer_file(&root.0, &launch, "DAQ_LAUNCH_0007.CSV", "network_download").unwrap();
    assert_eq!(out, vec![Outcome::Attached(second.folder.clone())], "launch beats the overlapping ground test");
    let attached = second.folder.join("board_sd/DAQ/DAQ_LAUNCH_0007.CSV");
    assert_eq!(std::fs::read(&attached).unwrap(), std::fs::read(&launch).unwrap(), "copied unchanged");
    let manifest = read_manifest(&second.folder).unwrap();
    assert_eq!(manifest.board_sd.len(), 1);
    let entry = &manifest.board_sd[0];
    assert_eq!((entry.board.as_str(), entry.rows, entry.matched_by.as_str(), entry.source.as_str()), ("DAQ", 2, "utc_window", "network_download"));
    assert_eq!(entry.sha256.len(), 64);
    assert!(read_manifest(&first.folder).unwrap().board_sd.is_empty());
    assert!(read_manifest(&igt.folder).unwrap().board_sd.is_empty());
    // Offering it again (download and import of the same file) adds nothing.
    board_sd::offer_file(&root.0, &launch, "DAQ_LAUNCH_0007.CSV", "sd_import").unwrap();
    assert_eq!(read_manifest(&second.folder).unwrap().board_sd.len(), 1);

    // The ordinary file of the same boot supplies the minute before launch.
    let ordinary = incoming.join("DAQ_0006.CSV");
    board_csv(&ordinary, &[
        (T0 + 3_500_000, 0, "network"),       // too early
        (T0 + 3_545_000, 45_000, "network"),  // 55 s before
        (T0 + 3_599_000, 49_000, "network"),  // 1 s before
    ]);
    let out = board_sd::offer_file(&root.0, &ordinary, "DAQ_0006.CSV", "sd_import").unwrap();
    assert_eq!(out, vec![Outcome::LeadUp(second.folder.clone())]);
    let entry = read_manifest(&second.folder).unwrap().board_sd[0].clone();
    assert_eq!(entry.lead_up_file.as_deref(), Some("DAQ_LAUNCH_0007.leadup.csv"));
    assert_eq!(entry.lead_up_from_ms, Some(T0 + 3_600_000 - board_sd::LEAD_UP_MS));
    assert_eq!(entry.lead_up_rows, 2);
    let lead = std::fs::read_to_string(second.folder.join("board_sd/DAQ/DAQ_LAUNCH_0007.leadup.csv")).unwrap();
    assert!(lead.starts_with(BOARD_HEADER) && lead.contains("# calibration"));
    assert_eq!(lead.lines().filter(|l| l.contains("accelerometer_sample")).count(), 2);

    // A launch file outside every capture stays where it is.
    let stray = incoming.join("irec_LAUNCH-9-0.csv");
    board_csv(&stray, &[(T0 + 90_000_000, 1, "network")]);
    assert_eq!(board_sd::offer_file(&root.0, &stray, "irec_LAUNCH-9-0.csv", "sd_import").unwrap(), vec![Outcome::NoCapture]);
    // Not a board log.
    assert_eq!(board_sd::offer_file(&root.0, &stray, "../x.csv", "sd_import").unwrap(), vec![Outcome::Ignored]);
}

#[tokio::test]
async fn downloads_made_before_the_capture_was_saved_attach_when_it_is_and_are_served() {
    let root = TestDirectory::new();
    // The DAQ files were downloaded first (ordinary one included).
    let downloads = root.0.join("sd-downloads/DAQ");
    board_csv(&downloads.join("LAUNCH/DAQ_LAUNCH_0001.CSV"), &[(T0 + 1_000, 61_000, "network"), (T0 + 21_000, 81_000, "network")]);
    board_csv(&downloads.join("UTC/DAQ_0001.CSV"), &[(T0 - 30_000, 30_000, "network"), (T0 + 500, 60_500, "network")]);
    let saved = launch_capture(&root.0, EventKind::Launch, T0, T0 + 60_000).await;
    board_sd::rescan_downloads(&root.0);
    let manifest = read_manifest(&saved.folder).unwrap();
    assert_eq!(manifest.board_sd.len(), 1);
    assert_eq!(manifest.board_sd[0].lead_up_rows, 2, "lead-up cut from the downloaded ordinary file");
    // Served by the recordings API.
    let state = admin_state(&root.0).await;
    let app = crate::recording_export::routes().with_state(state);
    let get = |uri: String| Request::get(uri).header("authorization", "Bearer rec-token").body(Body::empty()).unwrap();
    let response = app.clone().oneshot(get(format!("/api/recordings/{}/board_sd/DAQ/DAQ_LAUNCH_0001.CSV", saved.id))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert!(String::from_utf8_lossy(&body).starts_with(BOARD_HEADER));
    for bad in ["DAQ/..%2Fmanifest.json", "XX/DAQ_LAUNCH_0001.CSV", "DAQ/missing.CSV"] {
        let status = app.clone().oneshot(get(format!("/api/recordings/{}/board_sd/{bad}", saved.id))).await.unwrap().status();
        assert!(status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST, "{bad}: {status}");
    }
    let store = body_json(app.oneshot(get("/api/recordings/store".into())).await.unwrap()).await;
    assert_eq!(store["groups"][0]["captures"][0]["manifest"]["board_sd"][0]["board"], "DAQ");
}

#[test]
fn a_saved_launch_capture_requests_the_daq_launch_file_fetch() {
    crate::sd_download::schedule_launch_fetch();
    assert!(crate::sd_download::take_launch_fetch());
    assert!(!crate::sd_download::take_launch_fetch(), "taken once");
}
