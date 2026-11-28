use super::*;
use crate::auth::{AuthPrincipal, CalibrationAccess, CommandAccess, Permissions, StreamRole};

fn principal(admin: bool) -> AuthPrincipal {
    AuthPrincipal {
        roles: if admin { vec![StreamRole::Admin] } else { Vec::new() },
        username: Some(if admin { "admin" } else { "operator" }.into()),
        permissions: Permissions { view_data: true, send_commands: true, ..Default::default() },
        expires_at_ms: None,
        anonymous: false,
        session_type: Some("session".into()),
        command_access: CommandAccess { allowed_commands: vec!["Launch".into()] },
        calibration_access: CalibrationAccess::default(),
    }
}

/// An FC BOARD_HEALTH with this uptime (1 kHz ticks).
fn health(uptime_ms: u32) -> [u32; 24] {
    let mut words = [0u32; 24];
    words[2] = uptime_ms;
    words[4] = 1000;
    words
}

fn echo(site: Site) -> Vec<Option<f32>> {
    vec![Some(site.lat_deg as f32), Some(site.lon_deg as f32), Some(site.alt_msl_m as f32)]
}

const SITE: Site = Site { lat_deg: 43.0012345, lon_deg: -78.7865432, alt_msl_m: 182.4 };

#[test]
fn launch_site_is_the_fc_schema_type() {
    crate::telemetry_schema::initialize().unwrap();
    let kind = crate::telemetry_schema::data_type("LAUNCH_SITE");
    assert_eq!(kind.as_u32(), 229_881_795);
    let packet = sedsnet::packet::Packet::new(
        kind,
        &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
        "GS",
        1,
        Arc::from(SITE.payload()),
    )
    .unwrap();
    let values = packet.data_as_f32().unwrap();
    assert_eq!(values, vec![SITE.lat_deg as f32, SITE.lon_deg as f32, SITE.alt_msl_m as f32]);
}

#[test]
fn echo_matches_within_float32_tolerance() {
    assert!(SITE.matches(&Site::from_values(&echo(SITE)).unwrap()));
    assert!(!SITE.matches(&Site { alt_msl_m: 182.5, ..SITE }));
    assert!(!SITE.matches(&Site { lat_deg: 43.0013, ..SITE }));
    assert!(Site::from_values(&[Some(1.0), None, Some(2.0)]).is_none());
    assert!(Site::from_values(&[Some(1.0), Some(2.0)]).is_none());
}

#[tokio::test]
async fn setting_is_admin_only_validated_and_persisted() {
    let state = crate::state::tests::test_app_state().await;
    crate::igt::load_persisted(&state);
    let current = view(&state);
    assert!(current["configured"].is_null() && current["fc_reported"].is_null());
    assert_eq!(current["compiled_default"], json!({"lat_deg": 31.758, "lon_deg": -102.046, "alt_msl_m": 890.016}));
    assert_eq!((current["confirmed"].as_bool(), current["pending"].as_bool()), (Some(false), Some(false)));
    let body = serde_json::to_vec(&SITE).unwrap();
    assert_eq!(update(&state, &principal(false), &body).unwrap_err().0, StatusCode::FORBIDDEN);
    for bad in [
        json!({"lat_deg": 91.0, "lon_deg": 0.0, "alt_msl_m": 0.0}),
        json!({"lat_deg": 0.0, "lon_deg": 181.0, "alt_msl_m": 0.0}),
        json!({"lat_deg": 0.0, "lon_deg": 0.0, "alt_msl_m": 9001.0}),
        json!({"lat_deg": 0.0, "lon_deg": 0.0}),
        json!({"reset": false}),
    ] {
        let bytes = serde_json::to_vec(&bad).unwrap();
        assert_eq!(update(&state, &principal(true), &bytes).unwrap_err().0, StatusCode::BAD_REQUEST, "{bad}");
    }
    let current = update(&state, &principal(true), &body).unwrap();
    assert_eq!(current["configured"], serde_json::to_value(SITE).unwrap());
    assert_eq!((current["confirmed"].as_bool(), current["pending"].as_bool()), (Some(false), Some(true)));
    // A restart reads it back.
    tracker(&state).inner.lock().unwrap().configured = None;
    crate::igt::load_persisted(&state);
    assert_eq!(tracker(&state).inner.lock().unwrap().configured, Some(SITE));
    // Reset: no setting, nothing pending.
    let current = update(&state, &principal(true), br#"{"reset": true}"#).unwrap();
    assert!(current["configured"].is_null());
    assert_eq!(current["pending"], false);
    crate::igt::load_persisted(&state);
    assert_eq!(tracker(&state).inner.lock().unwrap().configured, None);
}

#[tokio::test]
async fn sends_on_change_first_sight_reboot_and_every_10_s_until_the_echo_matches() {
    let state = crate::state::tests::test_app_state().await;
    let now = Instant::now();
    // No setting: never sent, even when the FC appears.
    observe_fc_health(&state, &health(1_000));
    assert!(due_for_test(&state, now, false).is_none());
    tracker(&state).inner.lock().unwrap().fc_uptime_ms = None;
    // A setting while the FC is unseen waits for it.
    {
        let mut inner = tracker(&state).inner.lock().unwrap();
        inner.configured = Some(SITE);
        inner.due = true;
    }
    assert!(due_for_test(&state, now, false).is_none(), "nothing sent before the FC is seen");
    observe_fc_health(&state, &health(1_000));
    assert_eq!(due_for_test(&state, now, false), Some(SITE), "first sight");
    assert!(due_for_test(&state, now + Duration::from_secs(9), false).is_none());
    assert_eq!(due_for_test(&state, now + Duration::from_secs(10), false), Some(SITE), "10 s resend");
    // The FC echoes its compiled site (refused): still pending, resent.
    observe_echo(&state, &echo(COMPILED_DEFAULT));
    let current = view(&state);
    assert_eq!((current["confirmed"].as_bool(), current["pending"].as_bool()), (Some(false), Some(true)));
    assert_eq!(due_for_test(&state, now + Duration::from_secs(20), false), Some(SITE));
    // The echo matches: confirmed, nothing more sent.
    observe_echo(&state, &echo(SITE));
    let current = view(&state);
    assert_eq!((current["confirmed"].as_bool(), current["pending"].as_bool()), (Some(true), Some(false)));
    assert_eq!(current["fc_reported"]["alt_msl_m"].as_f64().map(|v| (v - 182.4).abs() < 1e-4), Some(true));
    assert!(due_for_test(&state, now + Duration::from_secs(60), false).is_none());
    // A later report with a larger uptime changes nothing.
    observe_fc_health(&state, &health(9_000));
    assert!(due_for_test(&state, now + Duration::from_secs(61), false).is_none());
    // The FC reboots (uptime goes backwards): its site is the compiled one again.
    observe_fc_health(&state, &health(500));
    assert!(view(&state)["fc_reported"].is_null());
    assert_eq!(due_for_test(&state, now + Duration::from_secs(62), false), Some(SITE), "reboot resync");
    // Never after Launch.
    assert!(due_for_test(&state, now + Duration::from_secs(100), true).is_none());
    // A changed setting is due at once.
    tracker(&state).inner.lock().unwrap().due = true;
    assert_eq!(due_for_test(&state, now + Duration::from_secs(63), false), Some(SITE));
    assert!(last_sent_for_test(&state).is_some());
}
