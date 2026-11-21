use super::*;
use crate::auth::{CalibrationAccess, CommandAccess, Permissions, StreamRole};
use serde_json::{Value, json};

fn principal(admin: bool, user: &str) -> AuthPrincipal {
    AuthPrincipal {
        roles: if admin { vec![StreamRole::Admin] } else { Vec::new() },
        username: Some(user.into()),
        permissions: Permissions { view_data: true, send_commands: true, ..Default::default() },
        expires_at_ms: None,
        anonymous: false,
        session_type: Some("session".into()),
        command_access: CommandAccess::default(),
        calibration_access: CalibrationAccess::default(),
    }
}

fn admin() -> AuthPrincipal {
    principal(true, "admin")
}

fn enable(state: &AppState, extra: Value) {
    let mut body = json!({"enabled": true});
    body.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
    update_settings(state, &admin(), body.to_string().as_bytes()).expect("enable");
}

fn report(id: &str, name: &str, participating: bool, in_range: Option<bool>) -> Value {
    json!({
        "v": 1, "device_id": id, "device_name": name, "platform": "ios",
        "participating": participating, "in_range": in_range, "accuracy": "high",
        "reference": "rocket", "background": true, "seq": 1, "sent_ms": 1_700_000_000_000i64,
    })
}

fn send(state: &AppState, user: &str, body: &Value, now: Instant) -> Result<Status, (StatusCode, String)> {
    accept_report(state, &principal(false, user), body.to_string().as_bytes(), now)
}

fn cmd(name: &str) -> TelemetryCommand {
    serde_json::from_value(json!(name)).unwrap()
}

/// Every client command name that the lock never refuses, whatever state.
const SAFING: &[&str] = &[
    "ValveSelfTest",
    "Abort", "Dump", "NormallyOpen", "NitrogenClose", "NitrousClose", "PauseFill", "CancelFill",
    "RetractPlumbing", "ResetSim", "ResetTClock", "MonitorAltitude", "RevokeMonitorAltitude", "ConsecutiveSamples",
    "RevokeConsecutiveSamples", "ResetFailures", "RevokeResetFailures", "ValidateMeasms",
    "RevokeValidateMeasms", "ToggleAvBayUnderglow", "ToggleFlightBuzzer", "FcSdRetry", "RewindFlightState",
    "ResetFlightState", "ResetLaunchLatch", "EvaluationAbort", "EvaluationRelax", "EvaluationFocus",
    "AbortAfter40", "AbortAfter100", "AbortAfter250", "DeployParachute", "ExpandParachute", "IgtExit",
    "IgtDryRun", "IgtEnter", "ReinitSensors", "ReinitBarometer", "EnableIMU", "DisableIMU",
    "ToggleButtonInterlock", "ToggleLaunchInterlock", "ToggleSupplyInterlock",
];

#[test]
fn classes_cover_arming_fill_and_launch_and_never_safing() {
    let opening = |_: &str| true;
    let closing = |_: &str| false;
    for name in ["Postinit", "AdvanceFlightState", "IgtLivePyro"] {
        assert_eq!(classify(name, opening), Some(ActionClass::Arming), "{name}");
    }
    for name in ["StartFill", "ContinueFillSequence", "NitrogenTest", "Turbocharge"] {
        assert_eq!(classify(name, opening), Some(ActionClass::Fill), "{name}");
    }
    for name in ["Launch", "GroundStationLaunch", "IgniterSequence", "Igniter"] {
        assert_eq!(classify(name, opening), Some(ActionClass::Launch), "{name}");
    }
    for name in ["Nitrogen", "Nitrous", "Pilot"] {
        assert_eq!(classify(name, opening), Some(ActionClass::ManualValves), "{name}");
    }
    // Valve self-test runs on the pad with crew next to the vehicle.
    assert_eq!(classify("ValveSelfTest", opening), None);
    // A toggle that would close (supply, PILOT) or turn off (igniter) is safing.
    for name in ["Nitrogen", "Nitrous", "Pilot", "Igniter"] {
        assert_eq!(classify(name, closing), None, "{name} closing");
    }
    for name in SAFING {
        assert_eq!(classify(name, opening), None, "{name} must never be refused");
        assert!(!CLASSIFIED.contains(name));
    }
    // Every client command of every mode is either listed as classified or
    // never refused: a new command needs an explicit decision here.
    for mode in [
        crate::station_mode::Mode::Normal,
        crate::station_mode::Mode::Hitl,
        crate::station_mode::Mode::TestFire,
        crate::station_mode::Mode::Testing,
        crate::station_mode::Mode::IntegratedGroundTest,
    ] {
        for name in crate::types::client_command_names(mode) {
            let classified = classify(name, opening).is_some();
            assert_eq!(classified, CLASSIFIED.contains(name), "{name}");
            assert!(classified || SAFING.contains(name) || name.starts_with("Toggle") || name.starts_with("Reinit"),
                "{name} has no decision");
        }
    }
}

#[tokio::test]
async fn off_by_default_and_nothing_is_kept() {
    let state = crate::state::tests::test_app_state().await;
    let status = status(&state);
    assert!(!status.settings.enabled && !status.active);
    assert_eq!(status.summary, "Proximity lock off");
    assert!(status.blocked_commands.is_empty());
    let now = Instant::now();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now).unwrap();
    assert!(state.proximity.inner.lock().unwrap().devices.is_empty());
    assert_eq!(refusal_at(&state, &TelemetryCommand::Launch, now), None);
}

#[tokio::test]
async fn in_range_refuses_each_enabled_class_and_never_safing() {
    // HITL deserializes every client command.
    let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    let now = Instant::now();
    let status = send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now).unwrap();
    assert!(status.active);
    assert_eq!(status.reason.as_deref(), Some("Proximity lock: 1 person within 150 m of the rocket (Alex's iPhone)"));
    assert_eq!(status.counts, Counts { reporting: 1, in_range: 1, unknown: 0, opted_out: 0 });
    for name in ["Postinit", "StartFill", "NitrogenTest", "Turbocharge", "Launch", "IgniterSequence"] {
        let reason = refusal_at(&state, &cmd(name), now).unwrap_or_else(|| panic!("{name} admitted"));
        assert!(reason.contains("Alex's iPhone"), "{reason}");
    }
    // By default the pad crew may run the valve self-test and work single
    // valves by hand; only the fill and launch sequences (and arming) lock.
    assert_eq!(refusal_at(&state, &TelemetryCommand::ValveSelfTest, now), None);
    assert_eq!(refusal_at(&state, &TelemetryCommand::Nitrogen, now), None);
    // Opted into: supply toggles open from an unknown/closed position: refused.
    enable(&state, json!({"blocks": {"arming": true, "fill": true, "launch": true, "manual_valves": true}}));
    assert!(refusal_at(&state, &TelemetryCommand::Nitrogen, now).is_some());
    assert_eq!(refusal_at(&state, &TelemetryCommand::ValveSelfTest, now), None);
    for name in SAFING.iter().filter(|n| crate::types::client_command_names(crate::station_mode::Mode::Hitl).contains(n)) {
        assert_eq!(refusal_at(&state, &cmd(name), now), None, "{name}");
    }
    // Each class can be turned off by itself.
    enable(&state, json!({"blocks": {"arming": false, "fill": true, "launch": false}}));
    assert_eq!(refusal_at(&state, &TelemetryCommand::Postinit, now), None);
    assert_eq!(refusal_at(&state, &TelemetryCommand::Launch, now), None);
    assert!(refusal_at(&state, &TelemetryCommand::StartFill, now).is_some());
    enable(&state, json!({"blocks": {"arming": true, "fill": false, "launch": false}}));
    assert!(refusal_at(&state, &TelemetryCommand::Postinit, now).is_some());
    assert_eq!(refusal_at(&state, &TelemetryCommand::StartFill, now), None);
    enable(&state, json!({"blocks": {"arming": false, "fill": false, "launch": true}}));
    assert!(refusal_at(&state, &TelemetryCommand::GroundStationLaunch, now).is_some());
    assert_eq!(refusal_at(&state, &TelemetryCommand::Nitrous, now), None);
    // At least one class while enabled.
    let none = update_settings(&state, &admin(), br#"{"blocks":{"arming":false,"fill":false,"launch":false,"manual_valves":false}}"#);
    assert_eq!(none.unwrap_err().0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_open_supply_may_still_be_closed_by_its_toggle() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({"blocks": {"arming": true, "fill": true, "launch": true, "manual_valves": true}}));
    let now = Instant::now();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now).unwrap();
    let key = crate::rocket_commands::ActuatorBoardCommands::NitrogenOpen as u8;
    assert!(primary_opens(&state, "Nitrogen"));
    state.set_pending_umbilical_valve_state(key, true);
    assert!(!primary_opens(&state, "Nitrogen"));
    assert_eq!(refusal_at(&state, &TelemetryCommand::Nitrogen, now), None, "the toggle now closes");
    assert!(refusal_at(&state, &TelemetryCommand::Nitrous, now).is_some());
}

#[tokio::test]
async fn out_of_range_and_opted_out_devices_do_not_block() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    let now = Instant::now();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now).unwrap();
    send(&state, "bo", &report("pixel-b", "Bo's Pixel", true, Some(false)), now).unwrap();
    assert!(refusal_at(&state, &TelemetryCommand::StartFill, now).is_some());
    // Alex opts out: that device stops affecting the lock at once.
    let mut out = report("phone-a", "Alex's iPhone", false, Some(true));
    out["opt_out"] = json!("user");
    let status = send(&state, "alex", &out, now).unwrap();
    assert!(!status.active, "{status:?}");
    assert_eq!(refusal_at(&state, &TelemetryCommand::StartFill, now), None);
    assert_eq!(status.counts, Counts { reporting: 1, in_range: 0, unknown: 0, opted_out: 1 });
    assert_eq!(status.summary, "Proximity lock: 1 device reporting, 0 opted out".replace("0 opted out", "1 opted out"));
    let alex = status.devices.iter().find(|d| d.name == "Alex's iPhone").unwrap();
    assert_eq!(alex.state, DeviceState::OptedOut);
    assert_eq!(alex.opt_out, Some(OptOut::User));
    assert_eq!(alex.accuracy, None, "nothing kept from a non-participating device");
    // An opted-out device is never unknown, however long it is silent.
    let later = now + Duration::from_secs(3600);
    let status = state.proximity.status_at(later, None);
    assert_eq!(status.counts.opted_out, 1);
    assert_eq!(status.counts.unknown, 1, "only Bo, who was participating");
    // Permission denied counts as opting out and is shown as such.
    let mut denied = report("phone-a", "Alex's iPhone", false, None);
    denied["opt_out"] = json!("permission_denied");
    let status = send(&state, "alex", &denied, now + Duration::from_secs(1)).unwrap();
    let alex = status.devices.iter().find(|d| d.name == "Alex's iPhone").unwrap();
    assert_eq!(alex.opt_out, Some(OptOut::PermissionDenied));
}

#[tokio::test]
async fn stale_devices_warn_by_default_and_block_when_configured() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    let now = Instant::now();
    send(&state, "bo", &report("pixel-b", "Bo's Pixel", true, Some(false)), now).unwrap();
    let fresh = now + Duration::from_secs(59);
    assert_eq!(state.proximity.status_at(fresh, None).counts.unknown, 0);
    let stale = now + Duration::from_secs(61);
    let status = state.proximity.status_at(stale, None);
    assert_eq!(status.counts.unknown, 1);
    assert!(!status.active);
    assert!(status.warning.as_deref().unwrap().contains("Bo's Pixel"), "{status:?}");
    assert!(status.warning.as_deref().unwrap().contains("only warn"));
    assert_eq!(refusal_at(&state, &TelemetryCommand::StartFill, stale), None);
    // "cannot tell" (in_range null) is unknown too.
    send(&state, "cy", &report("cy-1", "Cy's phone", true, None), now).unwrap();
    assert_eq!(state.proximity.status_at(now, None).counts.unknown, 1);
    enable(&state, json!({"stale_policy": "block"}));
    let status = state.proximity.status_at(stale, None);
    assert!(status.active);
    assert!(status.reason.as_deref().unwrap().contains("unknown devices block"), "{status:?}");
    assert!(refusal_at(&state, &TelemetryCommand::StartFill, stale).is_some());
    // Reporting again clears it.
    send(&state, "bo", &report("pixel-b", "Bo's Pixel", true, Some(false)), stale).unwrap();
    send(&state, "cy", &report("cy-1", "Cy's phone", true, Some(false)), stale).unwrap();
    assert_eq!(refusal_at(&state, &TelemetryCommand::StartFill, stale), None);
    // An administrator may remove a device that left.
    let status = state.proximity.status_at(stale + Duration::from_secs(120), None);
    assert!(status.active);
    for id in status.devices.iter().map(|d| d.id.clone()).collect::<Vec<_>>() {
        assert_eq!(dismiss(&state, &principal(false, "op"), json!({"id": id}).to_string().as_bytes()).unwrap_err().0, StatusCode::FORBIDDEN);
        dismiss(&state, &admin(), json!({"id": id}).to_string().as_bytes()).unwrap();
    }
    assert!(!state.proximity.status_at(stale + Duration::from_secs(120), None).active);
    assert!(state.proximity.audit().iter().any(|e| e.kind == AuditKind::Dismissed && e.text.contains("Bo's Pixel")));
}

#[tokio::test]
async fn the_override_is_admin_only_confirmed_and_expires() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    let now = Instant::now();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now).unwrap();
    let body = json!({"minutes": 10, "reason": "pad crew filling", "confirm": true}).to_string();
    assert_eq!(start_override(&state, &principal(false, "op"), body.as_bytes(), now).unwrap_err().0, StatusCode::FORBIDDEN);
    let unconfirmed = json!({"minutes": 10, "reason": "pad crew filling", "confirm": false}).to_string();
    assert_eq!(start_override(&state, &admin(), unconfirmed.as_bytes(), now).unwrap_err().0, StatusCode::BAD_REQUEST);
    for minutes in [0, 31] {
        let b = json!({"minutes": minutes, "reason": "pad crew", "confirm": true}).to_string();
        assert_eq!(start_override(&state, &admin(), b.as_bytes(), now).unwrap_err().0, StatusCode::BAD_REQUEST);
    }
    let status = start_override(&state, &admin(), body.as_bytes(), now).unwrap();
    assert!(!status.active && status.condition, "{status:?}");
    let o = status.override_view.as_ref().unwrap();
    assert_eq!((o.by.as_str(), o.reason.as_str(), o.remaining_s), ("admin", "pad crew filling", 600));
    assert!(status.warning.as_deref().unwrap().contains("Overridden by admin"));
    assert_eq!(refusal_at(&state, &TelemetryCommand::StartFill, now), None);
    // Still in force a moment before it expires; gone after.
    assert_eq!(refusal_at(&state, &TelemetryCommand::StartFill, now + Duration::from_secs(599)), None);
    let after = now + Duration::from_secs(601);
    // Alex kept reporting in range; once the override expires the lock is back.
    assert!(state.proximity.status_at(after - Duration::from_secs(2), None).override_view.is_some());
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), after).unwrap();
    assert!(refusal_at(&state, &TelemetryCommand::StartFill, after).is_some());
    assert!(state.proximity.inner.lock().unwrap().override_.is_none(), "expired override is removed");
    assert!(state.proximity.audit().iter().any(|e| e.kind == AuditKind::OverrideEnded && e.text.contains("expired")));
    // Ended early by an operator.
    start_override(&state, &admin(), body.as_bytes(), after).unwrap();
    end_override(&state, &principal(false, "op")).unwrap();
    assert!(state.proximity.audit().iter().any(|e| e.kind == AuditKind::OverrideEnded && e.text.contains("ended by op")));
    // Turning the lock off forgets the override and devices.
    update_settings(&state, &admin(), br#"{"enabled":false}"#).unwrap();
    assert!(state.proximity.inner.lock().unwrap().devices.is_empty());
}

#[tokio::test]
async fn audit_log_records_changes_with_names_and_never_places() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({"radius_m": 200}));
    let now = Instant::now();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(false)), now).unwrap();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now + Duration::from_secs(1)).unwrap();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(false)), now + Duration::from_secs(2)).unwrap();
    let mut out = report("phone-a", "Alex's iPhone", false, None);
    out["opt_out"] = json!("user");
    send(&state, "alex", &out, now + Duration::from_secs(3)).unwrap();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(false)), now + Duration::from_secs(4)).unwrap();
    refresh_at(&state, now + Duration::from_secs(100));
    assert!(refusal_at(&state, &TelemetryCommand::Launch, now).is_none());
    let audit = state.proximity.audit();
    let kinds: Vec<AuditKind> = audit.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            AuditKind::Settings,
            AuditKind::LeftRange,
            AuditKind::EnteredRange,
            AuditKind::LeftRange,
            AuditKind::OptedOut,
            AuditKind::OptedIn,
            AuditKind::Unknown,
        ],
        "{audit:#?}"
    );
    assert!(audit[0].text.contains("changed by admin") && audit[0].text.contains("lock on") && audit[0].text.contains("radius 150 m -> 200 m"));
    assert!(audit[2].text.contains("Alex's iPhone (alex) entered the 200 m lockout radius"));
    assert!(audit.iter().all(|e| e.at_ms > 0));
    // A refusal is audited with the command and the actor.
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now + Duration::from_secs(101)).unwrap();
    let message = admission_refusal(&state, &TelemetryCommand::StartFill, Some("op")).unwrap();
    assert_eq!(message, "StartFill refused. Proximity lock: 1 person within 200 m of the rocket (Alex's iPhone).");
    assert!(state.proximity.audit().iter().any(|e| e.kind == AuditKind::Refused && e.text.contains("StartFill from op")));
    let text = serde_json::to_string(&state.proximity.audit()).unwrap();
    for word in ["lat", "lon", "distance", "bearing"] {
        assert!(!text.contains(word), "audit mentions {word}: {text}");
    }
}

#[tokio::test]
async fn reports_with_any_location_field_are_refused_and_nothing_is_stored() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    let now = Instant::now();
    for (field, value) in [
        ("lat", json!(42.0)),
        ("lon", json!(-78.0)),
        ("latitude", json!(42.0)),
        ("longitude", json!(-78.0)),
        ("distance_m", json!(12.5)),
        ("bearing_deg", json!(90)),
        ("location", json!({"lat": 1, "lon": 2})),
        ("accuracy_m", json!(5.0)),
    ] {
        let mut body = report("phone-a", "Alex's iPhone", true, Some(true));
        body[field] = value;
        let error = send(&state, "alex", &body, now).unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST, "{field}");
        assert!(error.1.contains("never coordinates"), "{}", error.1);
    }
    assert!(state.proximity.inner.lock().unwrap().devices.is_empty(), "a refused report stores nothing");
    // The accuracy class is a word, not metres.
    let mut body = report("phone-a", "Alex's iPhone", true, Some(true));
    body["accuracy"] = json!(5.0);
    assert_eq!(send(&state, "alex", &body, now).unwrap_err().0, StatusCode::BAD_REQUEST);
    // A valid report: the status has no place for anyone.
    let status = send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now).unwrap();
    let value = serde_json::to_value(&status).unwrap();
    assert!(value.get("pad").is_none(), "no configured launch site, no pad");
    let device = &value["devices"][0];
    let keys: std::collections::BTreeSet<&str> = device.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["accuracy", "background", "id", "last_report_ms", "name", "platform", "reference", "state", "user"].into()
    );
    // Other bad input.
    for (mutate, expect) in [
        (json!({"v": 2}), StatusCode::BAD_REQUEST),
        (json!({"device_id": "has space"}), StatusCode::BAD_REQUEST),
        (json!({"device_name": ""}), StatusCode::BAD_REQUEST),
        (json!({"device_name": "x".repeat(49)}), StatusCode::BAD_REQUEST),
        (json!({"platform": "toaster"}), StatusCode::BAD_REQUEST),
    ] {
        let mut body = report("phone-a", "Alex's iPhone", true, Some(true));
        body.as_object_mut().unwrap().extend(mutate.as_object().cloned().unwrap());
        assert_eq!(send(&state, "alex", &body, now).unwrap_err().0, expect, "{mutate}");
    }
    let mut anonymous = principal(false, "");
    anonymous.anonymous = true;
    let body = report("phone-z", "Anon", true, Some(true)).to_string();
    assert_eq!(accept_report(&state, &anonymous, body.as_bytes(), now).unwrap_err().0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn reports_are_rate_limited_per_device() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    let now = Instant::now();
    let body = report("phone-a", "Alex's iPhone", true, Some(false));
    for i in 0..RATE_MAX {
        send(&state, "alex", &body, now + Duration::from_millis(i as u64)).unwrap();
    }
    assert_eq!(send(&state, "alex", &body, now + Duration::from_secs(1)).unwrap_err().0, StatusCode::TOO_MANY_REQUESTS);
    // Another device is unaffected; the window moves on.
    send(&state, "alex", &report("phone-b", "Alex's iPad", true, Some(false)), now).unwrap();
    send(&state, "alex", &body, now + RATE_WINDOW + Duration::from_secs(1)).unwrap();
    // Devices per account are bounded.
    for i in 2..MAX_DEVICES_PER_USER {
        send(&state, "alex", &report(&format!("dev-{i}"), "Spare", true, Some(false)), now).unwrap();
    }
    let error = send(&state, "alex", &report("dev-x", "One too many", true, Some(false)), now).unwrap_err();
    assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn settings_are_admin_only_validated_audited_and_persisted() {
    let state = crate::state::tests::test_app_state().await;
    let dir = std::env::temp_dir().join(format!("gs-proximity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    load(&state, dir.clone());
    assert_eq!(update_settings(&state, &principal(false, "op"), br#"{"enabled":true}"#).unwrap_err().0, StatusCode::FORBIDDEN);
    for body in [
        r#"{"radius_m":24}"#,
        r#"{"radius_m":2001}"#,
        r#"{"exit_margin_m":4}"#,
        r#"{"heartbeat_timeout_s":10}"#,
        r#"{"stale_policy":"ignore"}"#,
        r#"{"surprise":true}"#,
    ] {
        assert_eq!(update_settings(&state, &admin(), body.as_bytes()).unwrap_err().0, StatusCode::BAD_REQUEST, "{body}");
    }
    let status = update_settings(
        &state,
        &admin(),
        br#"{"enabled":true,"radius_m":300,"exit_margin_m":40,"stale_policy":"block","heartbeat_timeout_s":90}"#,
    )
    .unwrap();
    assert_eq!(status.settings.radius_m, 300);
    assert!(status.blocked_commands.contains(&"StartFill") && status.blocked_commands.contains(&"Launch"));
    let saved: Settings = serde_json::from_slice(&std::fs::read(dir.join(FILE)).unwrap()).unwrap();
    assert_eq!(saved, status.settings);
    // A fresh station reads it back.
    let other = crate::state::tests::test_app_state().await;
    load(&other, dir.clone());
    assert_eq!(crate::proximity_lock::status(&other).settings, saved);
    let audit = state.proximity.audit();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].text.contains("unknown devices block") && audit[0].text.contains("heartbeat timeout 60 s -> 90 s"), "{}", audit[0].text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn controls_are_disabled_with_the_reason_and_safing_stays_enabled() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({"blocks": {"arming": true, "fill": true, "launch": true, "manual_valves": true}}));
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), Instant::now()).unwrap();
    let control = |cmd: &str, enabled: bool, actuated: Option<bool>| crate::sequences::ActionControl {
        disabled_reason: None,
        cmd: cmd.into(),
        enabled,
        blink: crate::sequences::BlinkMode::None,
        actuated,
        requested: None,
        network_note: None,
        progress: None,
    };
    let mut policy = crate::sequences::ActionPolicyMsg {
        local_phase_controls: false,
        local_phase_controls_reason: None,
        launch_zero: None,
        station_recovery: None,
        key_enabled: true,
        software_buttons_enabled: true,
        controls: vec![
            control("StartFill", true, None),
            control("Nitrogen", true, Some(true)),
            control("Nitrous", true, Some(false)),
            control("Abort", true, None),
            control("Dump", true, Some(false)),
            control("NitrousClose", true, None),
            control("Launch", false, None),
        ],
    };
    policy.controls[6].disabled_reason = Some("Not Armed.".into());
    project_policy(&state, &mut policy);
    let get = |cmd: &str| policy.controls.iter().find(|c| c.cmd == cmd).unwrap().clone();
    let fill = get("StartFill");
    assert!(!fill.enabled);
    assert_eq!(fill.disabled_reason.as_deref(), Some("Proximity lock: 1 person within 150 m of the rocket (Alex's iPhone)."));
    assert!(get("Nitrogen").enabled, "an open supply's toggle closes it");
    assert!(!get("Nitrous").enabled);
    for safe in ["Abort", "Dump", "NitrousClose"] {
        assert!(get(safe).enabled, "{safe}");
    }
    assert_eq!(get("Launch").disabled_reason.as_deref(), Some("Not Armed. Also: Proximity lock: 1 person within 150 m of the rocket (Alex's iPhone)."));
}

#[tokio::test]
async fn the_command_path_answers_409_with_the_names() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), Instant::now()).unwrap();
    let (status, message) = crate::web::dispatch_authorized_command(
        state.clone(),
        principal(false, "op"),
        TelemetryCommand::StartFill,
        0,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(message, "StartFill refused. Proximity lock: 1 person within 150 m of the rocket (Alex's iPhone).");
    // Abort is never refused.
    let (status, _) =
        crate::web::dispatch_authorized_command(state.clone(), principal(false, "op"), TelemetryCommand::Abort, 0, None, None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn automatic_arming_waits_while_active() {
    let state = crate::state::tests::test_app_state().await;
    assert!(!holds_automatic_arming(&state));
    enable(&state, json!({}));
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), Instant::now()).unwrap();
    assert!(holds_automatic_arming(&state));
    assert!(holds_automatic_arming(&state), "announced once, held every tick");
    enable(&state, json!({"blocks": {"arming": false, "fill": true, "launch": true}}));
    assert!(!holds_automatic_arming(&state));
}

#[tokio::test]
async fn status_and_recorded_row_shapes() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({}));
    let now = Instant::now();
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), now).unwrap();
    let mut out = report("bo-1", "Bo's Pixel", false, None);
    out["opt_out"] = json!("user");
    send(&state, "bo", &out, now).unwrap();
    send(&state, "cy", &report("cy-1", "Cy's phone", true, Some(false)), now).unwrap();
    let status = state.proximity.status_at(now, Some(Pad { lat_deg: 1.0, lon_deg: 2.0 }));
    assert_eq!(status.summary, "Proximity lock: 2 devices reporting, 1 in range (Alex's iPhone), 1 opted out");
    let json = serde_json::to_value(&status).unwrap();
    let keys: std::collections::BTreeSet<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "active", "blocked_commands", "condition", "counts", "devices", "heartbeat_interval_s", "pad", "reason",
            "settings", "summary",
        ]
        .into()
    );
    assert_eq!(json["pad"], json!({"lat_deg": 1.0, "lon_deg": 2.0}));
    assert_eq!(json["settings"]["stale_policy"], "warn");
    let row = status_row(&status);
    let crate::telemetry_db::DbWrite::Telemetry { data_type, payload_json, .. } = row else { panic!() };
    assert_eq!(data_type, "GS_PROXIMITY_LOCK");
    assert!(!payload_json.contains("Alex"), "the recording logs counts, not names: {payload_json}");
    let payload: Value = serde_json::from_str(&payload_json).unwrap();
    assert_eq!((payload["in_range"].as_u64(), payload["opted_out"].as_u64(), payload["active"].as_bool()), (Some(1), Some(1), Some(true)));
}

#[tokio::test]
async fn the_safety_catalog_shows_the_live_lock() {
    let state = crate::state::tests::test_app_state().await;
    let rule = |state: &AppState| {
        crate::safety_catalog::catalog(state)
            .sections
            .into_iter()
            .flat_map(|s| s.rules)
            .find(|r| r.id == "proximity-lock")
            .unwrap()
            .summary
    };
    assert!(rule(&state).contains("Now: Off"), "{}", rule(&state));
    enable(&state, json!({}));
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), Instant::now()).unwrap();
    let summary = rule(&state);
    assert!(summary.contains("within 150 m") && summary.contains("Now: ACTIVE. Proximity lock: 1 person within 150 m of the rocket (Alex's iPhone)."), "{summary}");
    assert!(summary.contains("arming, fill/pressurizing, launch/ignition"), "{summary}");
}

#[tokio::test]
async fn an_rrls_judges_forwarded_names_by_the_mirrored_controls() {
    let state = crate::state::tests::test_app_state().await;
    enable(&state, json!({"blocks": {"arming": true, "fill": true, "launch": true, "manual_valves": true}}));
    send(&state, "alex", &report("phone-a", "Alex's iPhone", true, Some(true)), Instant::now()).unwrap();
    let mut policy = crate::sequences::default_action_policy();
    if let Some(control) = policy.controls.iter_mut().find(|c| c.cmd == "Nitrogen") {
        control.actuated = Some(true);
    }
    state.set_pending_umbilical_valve_state(crate::rocket_commands::ActuatorBoardCommands::NitrogenOpen as u8, true);
    state.set_action_policy(policy);
    assert_eq!(rrls_forward_refusal(&state, "Nitrogen", Some("op")), None, "closing an open supply");
    assert_eq!(rrls_forward_refusal(&state, "NitrogenClose", Some("op")), None);
    assert_eq!(rrls_forward_refusal(&state, "Abort", Some("op")), None);
    assert!(rrls_forward_refusal(&state, "Nitrous", Some("op")).unwrap().contains("Alex's iPhone"));
    assert!(rrls_forward_refusal(&state, "StartFill", Some("op")).is_some());
    assert!(rrls_forward_refusal(&state, "Launch", Some("op")).is_some());
}
