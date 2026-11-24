use super::coverage::tests::{rect, walk};
use super::*;
use crate::auth::{CalibrationAccess, CommandAccess, Permissions, StreamRole};
use serde_json::{Value, json};

const T0: i64 = 1_767_225_600_000;

fn principal(user: &str, admin: bool, commands: bool) -> AuthPrincipal {
    AuthPrincipal {
        roles: if admin { vec![StreamRole::Admin] } else { Vec::new() },
        username: Some(user.into()),
        permissions: Permissions { view_data: true, send_commands: commands, ..Default::default() },
        expires_at_ms: None,
        anonymous: false,
        session_type: Some("session".into()),
        command_access: CommandAccess::default(),
        calibration_access: CalibrationAccess::default(),
    }
}

fn operator() -> AuthPrincipal {
    principal("operator", false, true)
}
fn viewer(name: &str) -> AuthPrincipal {
    principal(name, false, false)
}
fn admin() -> AuthPrincipal {
    principal("administrator", true, true)
}

fn body(value: Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

fn session_id(state: &AppState) -> String {
    state.recovery_search.inner.lock().unwrap().session.session_id.clone().unwrap()
}

fn opt(state: &AppState, user: &str, device: &str, sharing: bool, now_ms: i64) -> Result<Value, Refusal> {
    opt_in(
        state,
        &viewer(user),
        &body(json!({"device_id": device, "device_name": format!("{user} phone"), "platform": "ios", "sharing": sharing})),
        now_ms,
    )
}

/// Fixes `(seq, t_ms, x_m, y_m)` in the local frame of the coverage tests.
fn batch(state: &AppState, opt_in_id: &str, device: &str, fixes: &[(u64, i64, f64, f64)]) -> Value {
    let frame = coverage::Frame::new(31.758, -102.046);
    let points: Vec<Value> = fixes
        .iter()
        .map(|&(seq, t, x, y)| {
            let (lat, lon) = frame.to_geo(x, y);
            json!({"seq": seq, "t_ms": t, "lat": lat, "lon": lon, "acc_m": 5.0})
        })
        .collect();
    json!({"v": 1, "session_id": session_id(state), "opt_in_id": opt_in_id, "device_id": device, "points": points})
}

fn send(state: &AppState, user: &str, value: &Value, now_ms: i64) -> Result<Value, Refusal> {
    upload(state, &viewer(user), &body(value.clone()), Instant::now(), now_ms)
}

fn code(result: Result<Value, Refusal>) -> (StatusCode, &'static str) {
    let (status, code, _) = result.expect_err("refused");
    (status, code)
}

fn status_now(state: &AppState, now_ms: i64) -> Value {
    state.recovery_search.status_body(now_ms)
}

async fn started() -> Arc<AppState> {
    let state = crate::state::tests::test_app_state().await;
    start(&state, &operator(), b"", T0).expect("start");
    state
}

/// A northward walk at x through y = -50..250 m: one fix per 5 m, 4 s apart.
fn lane(x: f64, seq0: u64, t0: i64) -> Vec<(u64, i64, f64, f64)> {
    (0..=60).map(|i| (seq0 + i as u64, t0 + i as i64 * 4_000, x, -50.0 + i as f64 * 5.0)).collect()
}

#[tokio::test]
async fn nothing_is_accepted_without_a_session_or_an_opt_in() {
    let state = crate::state::tests::test_app_state().await;
    // No session: opting in and uploading are refused.
    assert_eq!(code(opt(&state, "alice", "dev-a", true, T0)), (StatusCode::CONFLICT, "search_off"));
    let fake = json!({"v": 1, "session_id": "rs-x", "opt_in_id": "abc", "device_id": "dev-a", "points": []});
    assert_eq!(code(send(&state, "alice", &fake, T0)), (StatusCode::CONFLICT, "search_off"));

    start(&state, &operator(), b"", T0).unwrap();
    // A session, but no opt-in.
    let b = batch(&state, "guess", "dev-a", &lane(200.0, 1, T0 + 1_000));
    assert_eq!(code(send(&state, "alice", &b, T0 + 300_000)), (StatusCode::FORBIDDEN, "not_opted_in"));
    // Opted in, but with a wrong id, as another user, or for another session.
    let id = opt(&state, "alice", "dev-a", true, T0 + 1_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    assert_eq!(code(send(&state, "alice", &b, T0 + 300_000)), (StatusCode::FORBIDDEN, "not_opted_in"));
    let good = batch(&state, &id, "dev-a", &lane(200.0, 1, T0 + 1_000));
    assert_eq!(code(send(&state, "mallory", &good, T0 + 300_000)), (StatusCode::FORBIDDEN, "not_opted_in"));
    let mut other = good.clone();
    other["session_id"] = "rs-other".into();
    assert_eq!(code(send(&state, "alice", &other, T0 + 300_000)), (StatusCode::CONFLICT, "session_mismatch"));
    // Unknown fields (anything but the fix) are rejected.
    let mut extra = good.clone();
    extra["points"][0]["heading"] = 1.into();
    assert_eq!(code(send(&state, "alice", &extra, T0 + 300_000)), (StatusCode::BAD_REQUEST, "invalid"));
    // The right one is accepted.
    assert_eq!(send(&state, "alice", &good, T0 + 300_000).unwrap()["accepted"], 61);

    // Opting out voids the id at once.
    opt(&state, "alice", "dev-a", false, T0 + 400_000).unwrap();
    let more = batch(&state, &id, "dev-a", &lane(210.0, 100, T0 + 300_000));
    assert_eq!(code(send(&state, "alice", &more, T0 + 400_001)), (StatusCode::FORBIDDEN, "not_opted_in"));
    // Anonymous callers cannot share at all.
    let mut anon = viewer("x");
    anon.anonymous = true;
    assert_eq!(upload(&state, &anon, &body(good), Instant::now(), T0).unwrap_err().1, "sign_in");
}

#[tokio::test]
async fn fixes_from_before_the_opt_in_or_after_the_stop_are_refused() {
    let state = started().await;
    let opted = T0 + 600_000;
    let id = opt(&state, "alice", "dev-a", true, opted).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    // Recorded 10 minutes before the opt-in: not part of the sharing.
    let early = batch(&state, &id, "dev-a", &[(1, opted - 600_000, 0.0, 0.0), (2, opted + 1_000, 5.0, 0.0)]);
    let reply = send(&state, "alice", &early, opted + 2_000).unwrap();
    assert_eq!((reply["accepted"].as_u64(), reply["rejected"].as_u64()), (Some(1), Some(1)));

    let stopped = opted + 60_000;
    stop(&state, &operator(), stopped).unwrap();
    // Buffered out of signal before the stop, uploaded after it: merged.
    let late = batch(&state, &id, "dev-a", &[(3, stopped - 5_000, 10.0, 0.0), (4, stopped + 600_000, 15.0, 0.0)]);
    let reply = send(&state, "alice", &late, stopped + 900_000).unwrap();
    assert_eq!((reply["accepted"].as_u64(), reply["rejected"].as_u64()), (Some(1), Some(1)), "{reply}");
    // Opting in again while off is refused.
    assert_eq!(code(opt(&state, "bob", "dev-b", true, stopped + 1_000)), (StatusCode::CONFLICT, "search_off"));
    // After the grace nothing more is accepted.
    let later = batch(&state, &id, "dev-a", &[(5, stopped - 4_000, 12.0, 0.0)]);
    let after = stopped + LATE_UPLOAD_GRACE_S * 1000 + 1;
    assert_eq!(code(send(&state, "alice", &later, after)), (StatusCode::CONFLICT, "search_off"));
    assert_eq!(status_now(&state, after)["accepting_uploads"], false);
}

#[tokio::test]
async fn retried_and_out_of_order_batches_merge_once_in_time_order() {
    let target = rect(0.0, 0.0, 400.0, 200.0);
    let in_order = started().await;
    let shuffled = started().await;
    for state in [&in_order, &shuffled] {
        update_settings(state, &operator(), &body(json!({"target": {"polygon": target, "source": "los_prediction"}})), T0).unwrap();
    }
    let fixes = lane(200.0, 1, T0 + 10_000);
    let id = opt(&in_order, "alice", "dev-a", true, T0 + 1_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    send(&in_order, "alice", &batch(&in_order, &id, "dev-a", &fixes), T0 + 400_000).unwrap();

    let id = opt(&shuffled, "alice", "dev-a", true, T0 + 1_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    // The second half arrives first, then the first half (buffered offline),
    // then the whole lot again (a retry after a lost reply).
    let (first, second) = fixes.split_at(30);
    let r = send(&shuffled, "alice", &batch(&shuffled, &id, "dev-a", second), T0 + 300_000).unwrap();
    assert_eq!(r["accepted"], 31);
    let r = send(&shuffled, "alice", &batch(&shuffled, &id, "dev-a", first), T0 + 350_000).unwrap();
    assert_eq!((r["accepted"].as_u64(), r["max_seq"].as_u64()), (Some(30), Some(61)));
    let r = send(&shuffled, "alice", &batch(&shuffled, &id, "dev-a", &fixes), T0 + 400_000).unwrap();
    assert_eq!((r["accepted"].as_u64(), r["duplicates"].as_u64(), r["stored_points"].as_u64()), (Some(0), Some(61), Some(61)));

    let a = status_now(&in_order, T0 + 400_000);
    let b = status_now(&shuffled, T0 + 400_000);
    assert_eq!(a["stats"]["percent_covered"], b["stats"]["percent_covered"]);
    assert_eq!(a["overlay"]["tracks"][0]["points"], b["overlay"]["tracks"][0]["points"]);
    let pct = a["stats"]["percent_covered"].as_f64().unwrap();
    assert!((pct - 7.5).abs() < 0.6, "{pct}");
    assert!((a["stats"]["distance_m"].as_f64().unwrap() - 300.0).abs() < 1.0);
}

#[tokio::test]
async fn coverage_overlay_has_tracks_swaths_and_uncovered_areas() {
    let state = started().await;
    let target = rect(0.0, 0.0, 400.0, 200.0);
    update_settings(&state, &operator(), &body(json!({"swath_width_m": 30.0, "target": {"polygon": target, "source": "flight_plan", "label": "95% landing area"}})), T0).unwrap();
    for (user, dev, x) in [("alice", "dev-a", 100.0), ("bob", "dev-b", 300.0), ("carol", "dev-c", 300.0)] {
        let id = opt(&state, user, dev, true, T0 + 1_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
        send(&state, user, &batch(&state, &id, dev, &lane(x, 1, T0 + 10_000)), T0 + 400_000).unwrap();
    }
    let s = status_now(&state, T0 + 400_000);
    assert_eq!(s["overlay"]["kind"], "search_coverage");
    assert_eq!(s["overlay"]["tracks"].as_array().unwrap().len(), 3);
    assert!(s["overlay"]["tracks"].as_array().unwrap().iter().all(|t| t["style"] == "search"));
    let kinds: Vec<&str> = s["overlay"]["areas"].as_array().unwrap().iter().map(|a| a["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "search_swath").count(), 2, "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "uncovered").count(), 3, "{kinds:?}");
    // bob and carol walked the same lane: two lanes of 30 m, not three.
    let pct = s["stats"]["percent_covered"].as_f64().unwrap();
    assert!((pct - 15.0).abs() < 1.0, "{pct}");
    assert_eq!(s["stats"]["sharing"], 3);
    assert_eq!(s["searchers"][0]["color"], COLORS[0]);
    // The status never carries a device id or an opt-in id.
    let text = s.to_string();
    assert!(!text.contains("dev-a") && !text.contains("opt_in_id"), "{text}");
}

#[tokio::test]
async fn permissions_follow_the_station() {
    let state = crate::state::tests::test_app_state().await;
    assert_eq!(start(&state, &viewer("v"), b"", T0).unwrap_err().0, StatusCode::FORBIDDEN);
    start(&state, &operator(), br#"{"swath_width_m": 40}"#, T0).unwrap();
    assert_eq!(start(&state, &operator(), b"", T0).unwrap_err().1, "already_active");
    assert_eq!(update_settings(&state, &viewer("v"), br#"{"swath_width_m": 20}"#, T0).unwrap_err().0, StatusCode::FORBIDDEN);
    assert_eq!(update_settings(&state, &operator(), br#"{"swath_width_m": 1}"#, T0).unwrap_err().0, StatusCode::BAD_REQUEST);
    assert_eq!(stop(&state, &viewer("v"), T0).unwrap_err().0, StatusCode::FORBIDDEN);
    assert_eq!(clear_tracks(&state, &operator(), T0).unwrap_err().0, StatusCode::FORBIDDEN);
    clear_tracks(&state, &admin(), T0).unwrap();
    assert_eq!(status_now(&state, T0)["settings"]["swath_width_m"], 40.0);
}

#[tokio::test]
async fn tracks_live_only_for_the_session_and_can_be_cleared() {
    let state = started().await;
    let id = opt(&state, "alice", "dev-a", true, T0 + 1_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    send(&state, "alice", &batch(&state, &id, "dev-a", &lane(0.0, 1, T0 + 10_000)), T0 + 400_000).unwrap();
    let bid = opt(&state, "bob", "dev-b", true, T0 + 1_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    send(&state, "bob", &batch(&state, &bid, "dev-b", &lane(50.0, 1, T0 + 10_000)), T0 + 400_000).unwrap();
    // A searcher deletes their own track when opting out.
    opt_in(&state, &viewer("bob"), &body(json!({"device_id": "dev-b", "device_name": "b", "sharing": false, "delete_my_track": true})), T0 + 500_000).unwrap();
    assert_eq!(status_now(&state, T0 + 500_000)["stats"]["points"], 61);
    // An administrator clears everything; sharing devices stay opted in.
    clear_tracks(&state, &admin(), T0 + 600_000).unwrap();
    let s = status_now(&state, T0 + 600_000);
    assert_eq!(s["stats"]["points"], 0);
    assert_eq!(s["stats"]["sharing"], 1);
    send(&state, "alice", &batch(&state, &id, "dev-a", &lane(0.0, 100, T0 + 610_000)), T0 + 900_000).unwrap();
    // A new session never inherits tracks or opt-ins.
    stop(&state, &operator(), T0 + 1_000_000).unwrap();
    start(&state, &operator(), b"", T0 + 2_000_000).unwrap();
    let s = status_now(&state, T0 + 2_000_000);
    assert_eq!((s["stats"]["points"].as_u64(), s["stats"]["searchers"].as_u64()), (Some(0), Some(0)));
    let stale = batch(&state, &id, "dev-a", &lane(0.0, 200, T0 + 2_000_000));
    assert_eq!(code(send(&state, "alice", &stale, T0 + 2_300_000)), (StatusCode::FORBIDDEN, "not_opted_in"));
}

#[tokio::test]
async fn automatic_target_never_replaces_a_manual_one() {
    let state = crate::state::tests::test_app_state().await;
    let los = rect(0.0, 0.0, 300.0, 100.0);
    assert_eq!(set_target_area_at(&state, &los, "los_prediction", T0), Ok(true));
    assert_eq!(status_now(&state, T0)["target"]["source"], "los_prediction");
    let manual = rect(0.0, 0.0, 50.0, 50.0);
    update_settings(&state, &operator(), &body(json!({"target": {"polygon": manual}})), T0).unwrap();
    assert_eq!(set_target_area_at(&state, &los, "los_prediction", T0), Ok(false));
    assert_eq!(status_now(&state, T0)["target"]["source"], "manual");
    assert!(set_target_area_at(&state, &los[..2], "flight_plan", T0).is_err());
    assert!(set_target_area_at(&state, &los, "manual", T0).is_err());
    // Removing the area (null) lets the prediction set it again.
    update_settings(&state, &operator(), br#"{"target": null}"#, T0).unwrap();
    assert_eq!(set_target_area_at(&state, &los, "flight_plan", T0), Ok(true));
}

#[tokio::test]
async fn export_trims_to_the_recording_window_and_persistence_round_trips() {
    let dir = crate::recording_export::tests::TestDirectory::new();
    let state = crate::state::tests::test_app_state().await;
    load(&state, dir.0.clone());
    assert!(export(&state, None, None).is_none());
    start(&state, &operator(), b"", T0).unwrap();
    let id = opt(&state, "alice", "dev-a", true, T0 + 1_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    send(&state, "alice", &batch(&state, &id, "dev-a", &lane(0.0, 1, T0 + 10_000)), T0 + 400_000).unwrap();
    let all = export(&state, None, None).unwrap();
    assert_eq!(all["stats"]["points"], 61);
    let part = export(&state, Some(T0 + 10_000), Some(T0 + 50_000)).unwrap();
    assert_eq!(part["stats"]["points"], 11);
    assert!(export(&state, Some(T0 + 900_000), None).is_none());
    assert!(!all.to_string().contains("dev-a"));

    save_if_dirty(&state);
    let again = crate::state::tests::test_app_state().await;
    load(&again, dir.0.clone());
    assert_eq!(status_now(&again, T0 + 400_000)["stats"]["points"], 61);
    // The id survives a restart, so a device can keep uploading.
    let more = batch(&again, &id, "dev-a", &lane(5.0, 100, T0 + 300_000));
    assert_eq!(send(&again, "alice", &more, T0 + 600_000).unwrap()["accepted"], 61);
    // Clearing removes the stored tracks from disk too.
    clear_tracks(&again, &admin(), T0 + 700_000).unwrap();
    let stored = std::fs::read_to_string(dir.0.join(FILE)).unwrap();
    assert!(!stored.contains("\"t_ms\""), "{stored}");
}

#[tokio::test]
async fn walk_helper_matches_lane_density() {
    // Guards the fixtures above: the coverage-test walk and the lane agree.
    assert_eq!(walk((0.0, -50.0), (0.0, 250.0)).len(), lane(0.0, 1, 0).len());
}

#[tokio::test]
async fn a_replay_bundle_carries_the_search_tracks_only_when_there_are_any() {
    let root = crate::recording_export::tests::TestDirectory::new();
    let state = crate::recording_export::tests::export_state(&root.0, true).await;
    let id = "groundstation_recording_2026-01-01_00-00-00_000.db";
    let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id)).await.unwrap();
    for i in 0..60i64 {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(?,NULL,'FC','GPS_DATA',?,'[]')")
            .bind(T0 + i * 1000)
            .bind(format!("[31.758,{},900.0]", -102.046 + i as f64 * 1e-5))
            .execute(&db)
            .await
            .unwrap();
    }
    db.close().await;
    let headers = axum::http::HeaderMap::new();
    let options = crate::recording_bundle::Options::default();
    let entry = b"derived/recovery_search.json";
    let has = |bundle: &[u8]| bundle.windows(entry.len()).any(|w| w == entry);
    let plain = crate::recording_bundle::build(state.clone(), &headers, id, &options, T0).await.unwrap();
    assert!(!has(&plain), "no search: the bundle is unchanged");

    start(&state, &operator(), b"", T0 + 60_000).unwrap();
    let opt_in_id = opt(&state, "alice", "dev-a", true, T0 + 61_000).unwrap()["opt_in_id"].as_str().unwrap().to_owned();
    send(&state, "alice", &batch(&state, &opt_in_id, "dev-a", &lane(0.0, 1, T0 + 70_000)), T0 + 400_000).unwrap();
    let bundle = crate::recording_bundle::build(state.clone(), &headers, id, &options, T0).await.unwrap();
    assert!(has(&bundle), "the search tracks ride along");
    // A recording from long before the search does not get it.
    assert!(export(&state, Some(T0 - 10 * BUNDLE_AFTER_END_MS), Some(T0 - BUNDLE_AFTER_END_MS)).is_none());
}
