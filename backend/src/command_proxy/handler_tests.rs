use super::*;
use crate::{
    auth::{AuthManager, Permission},
    types::TelemetryCommand,
};
fn child(name: &str, role: &str) -> bool {
    if std::env::var_os("GS_PROXY_HANDLER_CHILD").is_some() {
        return true;
    }
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env("GS_PROXY_HANDLER_CHILD", "1")
        .env("GS_RRLS_ROLE", role)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    false
}
fn configure() -> crate::rrls::Shared {
    crate::rrls::configure(&crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "fixture-no-serial-open".into(),
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115200,
            protocol: crate::comms_config::SerialProtocol::RawUart,
        },
    })
    .unwrap();
    let mesh = crate::rrls::settings_transport().unwrap();
    {
        let mut m = mesh.lock().unwrap();
        let now = m.now();
        m.tick(now);
    }
    mesh
}
fn user(auth: &AuthManager, send: bool, time: bool, disabled: bool, commands: &[&str]) {
    std::fs::write(auth.path(),serde_json::to_vec(&serde_json::json!({"version":1,"users":[{"username":"operator","password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":send,"set_system_time":time},"command_access":{"allowed_commands":commands},"disabled":disabled}]})).unwrap()).unwrap();
}
fn clients(p: &Pairing, r: &Pairing, view: Transport) -> (Runtime, Runtime, Transport) {
    let rv = Transport {
        sequence: view.sequence.wrapping_sub(1),
        ..view
    };
    let mut primary = Runtime::new(Role::Primary);
    let mut rr = Runtime::new(Role::Rrls);
    primary.connect(Some(view));
    rr.connect(Some(rv));
    let query = rr.query(r, rv).unwrap();
    let Received::Reply(reply) = primary
        .receive(p, view, crate::station_mode::local_mode(), &query)
        .unwrap()
    else {
        panic!("reply")
    };
    rr.receive(r, rv, Mode::Normal, &reply).unwrap();
    (primary, rr, rv)
}
fn request(
    p: &Pairing,
    r: &Pairing,
    primary: &mut Runtime,
    rr: &Runtime,
    pv: Transport,
    rv: Transport,
    id: u8,
    operation: Operation,
) -> VerifiedRequest {
    let bytes = rr
        .request(
            r,
            rv,
            crate::station_mode::local_mode(),
            [id; 16],
            "operator".into(),
            operation,
        )
        .unwrap();
    let Received::Execute(request) = primary
        .receive(p, pv, crate::station_mode::local_mode(), &bytes)
        .unwrap()
    else {
        panic!("request")
    };
    request
}
#[tokio::test]
async fn primary_actual_dispatch_rechecks_actor_policy_and_nonblocking_admission() {
    if !child(
        "command_proxy::handler_tests::primary_actual_dispatch_rechecks_actor_policy_and_nonblocking_admission",
        "primary",
    ) {
        return;
    }
    let (root, p, r) = super::tests::fixtures();
    let mesh = configure();
    let pv = {
        let m = mesh.lock().unwrap();
        m.command_proxy_transport(m.now()).unwrap()
    };
    let (mut primary, rr, rv) = clients(&p, &r, pv);
    let mut state = crate::recording_export::tests::export_state(&root.0, false).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
    state.action_policy.lock().unwrap().software_buttons_enabled = true;
    user(&state.auth, true, false, false, &["ToggleAvBayUnderglow"]);
    let work = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        1,
        Operation::Command("ToggleAvBayUnderglow".into()),
    );
    assert_eq!(execute_primary(state.clone(), &work).await, Outcome::Queued);
    assert!(matches!(
        rx.try_recv().unwrap(),
        TelemetryCommand::ToggleAvBayUnderglow
    ));
    assert!(rx.try_recv().is_err());
    let reply = primary.finish(&p, work.clone(), Outcome::Queued).unwrap();
    assert!(!reply.is_empty());
    user(&state.auth, true, false, true, &["ToggleFlightBuzzer"]);
    let denied = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        2,
        Operation::Command("ToggleFlightBuzzer".into()),
    );
    assert_eq!(
        execute_primary(state.clone(), &denied).await,
        Outcome::Denied
    );
    assert!(rx.try_recv().is_err());
    user(&state.auth, true, false, false, &["ToggleFlightBuzzer"]);
    let disallowed = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        3,
        Operation::Command("ToggleAvBayUnderglow".into()),
    );
    assert_eq!(
        execute_primary(state.clone(), &disallowed).await,
        Outcome::Denied
    );
    state.cmd_tx.try_send(TelemetryCommand::Postinit).unwrap();
    let full = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        4,
        Operation::Command("ToggleFlightBuzzer".into()),
    );
    assert_eq!(
        execute_primary(state.clone(), &full).await,
        Outcome::Unavailable
    );
    assert!(matches!(rx.try_recv().unwrap(), TelemetryCommand::Postinit));
    let after = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        5,
        Operation::Command("ToggleFlightBuzzer".into()),
    );
    assert_eq!(
        execute_primary(state.clone(), &after).await,
        Outcome::Queued
    );
    assert!(
        matches!(rx.try_recv().unwrap(), TelemetryCommand::ToggleFlightBuzzer),
        "a refused queue slot must not be recorded as an accepted duplicate"
    );
    state.action_policy.lock().unwrap().software_buttons_enabled = false;
    let interlock = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        6,
        Operation::Command("ToggleFlightBuzzer".into()),
    );
    assert_eq!(
        execute_primary(state.clone(), &interlock).await,
        Outcome::Denied
    );
    assert!(rx.try_recv().is_err());
    user(&state.auth, false, false, false, &[]);
    assert!(
        state
            .auth
            .paired_proxy_actor("operator", Permission::SendCommands)
            .is_err()
    );
    state.request_shutdown();
}
#[tokio::test]
async fn primary_network_utc_uses_its_current_time_permission_and_router() {
    if !child(
        "command_proxy::handler_tests::primary_network_utc_uses_its_current_time_permission_and_router",
        "primary",
    ) {
        return;
    }
    let (root, p, r) = super::tests::fixtures();
    let mesh = configure();
    let pv = {
        let m = mesh.lock().unwrap();
        m.command_proxy_transport(m.now()).unwrap()
    };
    let (mut primary, rr, rv) = clients(&p, &r, pv);
    let state = crate::recording_export::tests::export_state(&root.0, false).await;
    let router = Arc::new(sedsnet::router::Router::new_with_clock(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        Box::new(|| 0),
    ));
    state.topology_router.set(router.clone()).ok().unwrap();
    user(&state.auth, true, false, false, &[]);
    let denied = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        1,
        Operation::NetworkUtc(1_800_000_000_000),
    );
    assert_eq!(
        execute_primary(state.clone(), &denied).await,
        Outcome::Denied
    );
    assert_eq!(router.network_time_ms(), None);
    user(&state.auth, false, true, false, &[]);
    let allowed = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        2,
        Operation::NetworkUtc(1_800_000_000_000),
    );
    assert_eq!(
        execute_primary(state.clone(), &allowed).await,
        Outcome::Queued
    );
    assert_eq!(router.network_time_ms(), Some(1_800_000_000_000));
    let invalid = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        3,
        Operation::NetworkUtc(0),
    );
    assert_eq!(
        execute_primary(state.clone(), &invalid).await,
        Outcome::Invalid
    );
    state.request_shutdown();
}
#[tokio::test]
async fn recovery_cannot_execute_proxied_hardware_locally_even_with_granted_actor() {
    if !child(
        "command_proxy::handler_tests::recovery_cannot_execute_proxied_hardware_locally_even_with_granted_actor",
        "rrls",
    ) {
        return;
    }
    let (root, p, r) = super::tests::fixtures();
    let mut state = crate::recording_export::tests::export_state(&root.0, false).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
    user(&state.auth, true, true, false, &[]);
    let principal = state
        .auth
        .paired_proxy_actor("operator", Permission::SendCommands)
        .unwrap();
    assert!(!state.is_command_allowed(&TelemetryCommand::ToggleAvBayUnderglow));
    assert_eq!(
        crate::web::dispatch_proxied_command(
            state.clone(),
            principal,
            TelemetryCommand::ToggleAvBayUnderglow
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let pv = Transport {
        session: 7,
        sequence: 21,
    };
    let (mut primary, rr, rv) = clients(&p, &r, pv);
    let work = request(
        &p,
        &r,
        &mut primary,
        &rr,
        pv,
        rv,
        1,
        Operation::Command("ToggleAvBayUnderglow".into()),
    );
    assert_eq!(execute_primary(state.clone(), &work).await, Outcome::Denied);
    assert!(rx.try_recv().is_err());
    state.request_shutdown();
}

#[tokio::test]
async fn recovery_submission_rechecks_session_is_bounded_and_never_claims_device_confirmation() {
    if !child(
        "command_proxy::handler_tests::recovery_submission_rechecks_session_is_bounded_and_never_claims_device_confirmation",
        "rrls",
    ) {
        return;
    }
    let (root, _, _) = super::tests::fixtures();
    let mesh = configure();
    // Complete real mesh HEALTH decoding instead of inventing a connected flag.
    let mut health = b"RR\x01\x00".to_vec();
    health.extend(7u32.to_le_bytes());
    health.extend(20u32.to_le_bytes());
    health.push(1);
    let mut crc = u32::MAX;
    for b in &health {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    health.extend((!crc).to_le_bytes());
    {
        let mut mesh = mesh.lock().unwrap();
        let now = mesh.now();
        mesh.incoming(true, &health, now);
    }
    let state = crate::recording_export::tests::export_state(&root.0, false).await;
    user(
        &state.auth,
        true,
        false,
        false,
        &["ToggleAvBayUnderglow", "ToggleFlightBuzzer"],
    );
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,allowed_commands_json,created_at_ms,expires_at_ms) VALUES ('fixture-token','operator','password',1,1,'[]',0,9223372036854775807)")
        .execute(&state.auth_db).await.unwrap();
    let principal = state
        .auth
        .authorize_token(
            &state.auth_db,
            Some("fixture-token"),
            Permission::SendCommands,
        )
        .await
        .unwrap();
    {
        let mut broker = broker().lock().unwrap();
        broker.status.available = true;
        broker.status.supported_commands =
            vec!["ToggleAvBayUnderglow".into(), "ToggleFlightBuzzer".into()];
    }
    let operation = Operation::Command("ToggleAvBayUnderglow".into());
    assert_eq!(
        submit(&state, &principal, Some("fixture-token"), operation.clone())
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    let first = status();
    assert_eq!(first.pending.as_ref().unwrap().phase, "pending_primary");
    assert!(!first.pending.as_ref().unwrap().primary_accepted);
    assert!(!first.pending.as_ref().unwrap().device_confirmed);
    assert_eq!(
        submit(&state, &principal, Some("fixture-token"), operation)
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        status().pending.unwrap().request_id,
        first.pending.unwrap().request_id
    );
    assert_eq!(
        submit(
            &state,
            &principal,
            Some("fixture-token"),
            Operation::Command("ToggleFlightBuzzer".into())
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    user(&state.auth, true, false, true, &[]);
    assert_eq!(
        submit(
            &state,
            &principal,
            Some("fixture-token"),
            Operation::Command("ToggleFlightBuzzer".into())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let id = broker().lock().unwrap().pending.as_ref().unwrap().id;
    finish_pending([0; 16], "accepted_by_primary", true, None);
    assert!(
        status().pending.is_some(),
        "unmatched results cannot confirm anything"
    );
    finish_pending(id, "accepted_by_primary", true, None);
    let last = status().last.unwrap();
    assert!(last.primary_accepted);
    assert!(!last.device_confirmed);
    let encoded = serde_json::to_string(&status()).unwrap();
    assert!(!encoded.contains("fixture-token"));
    // Revoking a credential cannot prove a request already handed to radio
    // never executed. Only a request never admitted to Mesh is definitively denied.
    user(&state.auth, true, false, false, &["ToggleFlightBuzzer"]);
    for attempts in [0, 1] {
        assert_eq!(
            submit(
                &state,
                &principal,
                Some("fixture-token"),
                Operation::Command("ToggleFlightBuzzer".into())
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        let mut pending = broker().lock().unwrap().pending.clone().unwrap();
        pending.attempts = attempts;
        finish_revoked(&pending);
        let last = status().last.unwrap();
        assert_eq!(last.phase, if attempts == 0 { "denied" } else { "unknown" });
        assert!(!last.primary_accepted && !last.device_confirmed);
    }
    state.request_shutdown();
}

#[tokio::test]
async fn recovery_websocket_policy_uses_primary_projection_and_new_signed_observations_publish_immediately()
 {
    if !child(
        "command_proxy::handler_tests::recovery_websocket_policy_uses_primary_projection_and_new_signed_observations_publish_immediately",
        "rrls",
    ) {
        return;
    }
    let (root, _, _) = super::tests::fixtures();
    let state = crate::recording_export::tests::export_state(&root.0, false).await;
    crate::primary_mirror::invalidate_control_observation();
    let mut published = state.action_policy_tx.subscribe();
    state.set_action_policy(crate::sequences::ActionPolicyMsg {
        key_enabled: true,
        software_buttons_enabled: true,
        controls: vec![crate::sequences::ActionControl {
            cmd: "ToggleAvBayUnderglow".into(),
            enabled: true,
            blink: crate::sequences::BlinkMode::None,
            actuated: Some(true),
        }],
    });
    let unknown = published.try_recv().unwrap();
    assert!(!unknown.key_enabled && !unknown.software_buttons_enabled);
    assert_eq!(unknown, state.action_policy_snapshot());
    assert!(
        !unknown
            .controls
            .iter()
            .any(|c| c.cmd == "ToggleAvBayUnderglow" && c.enabled)
    );
    // Valid optional presentation bytes represent a verified nonce/result at
    // this private boundary; tests do not create any hardware authority.
    let mut observation = vec![0; 57];
    observation[0] = 1;
    observation[1] = 3;
    let bit = command_catalog()
        .iter()
        .position(|c| *c == "ToggleAvBayUnderglow")
        .unwrap();
    observation[2..10].copy_from_slice(&(1u64 << bit).to_le_bytes());
    assert!(accept_presentation(&state, &observation, 0));
    let current = published.try_recv().unwrap();
    assert!(current.key_enabled && current.software_buttons_enabled);
    assert!(
        current
            .controls
            .iter()
            .any(|c| c.cmd == "ToggleAvBayUnderglow" && c.enabled && c.actuated.is_none())
    );
    assert!(!accept_presentation(&state, &[255], 0));
    assert!(published.try_recv().is_err());
    invalidate_presentation(&state);
    let expired = published.try_recv().unwrap();
    assert!(!expired.key_enabled && !expired.software_buttons_enabled);
    state.request_shutdown();
}
