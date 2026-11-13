//! One-shot pressure relief during retained recovery. This capability is never
//! deserialized, queued as an ordinary command, or renewed after an await.
use super::Token;
use crate::{state::AppState, types::TelemetryCommand};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

pub(crate) struct Intent {
    token: Token,
    router: Arc<sedsnet::router::Router>,
    captured_at: Instant,
    source_ms: u64,
    command: &'static str,
    kind: sedsnet::config::DataType,
    byte: u8,
}
impl Intent {
    pub(crate) fn capture(state: &AppState, cmd: &TelemetryCommand) -> Option<Self> {
        if crate::rrls::is_rrls() {
            return None;
        }
        let owner = state
            .operator_admission_barrier
            .load(std::sync::atomic::Ordering::Acquire);
        if owner == 0 {
            return None;
        }
        let (kind, byte) = match cmd {
            TelemetryCommand::NitrogenClose => ("ACTUATOR_COMMAND", 12),
            TelemetryCommand::NitrousClose => ("ACTUATOR_COMMAND", 13),
            TelemetryCommand::NormallyOpen if state.get_umbilical_valve_state(1) == Some(false) => {
                ("VALVE_COMMAND", 1)
            }
            TelemetryCommand::Dump if state.get_umbilical_valve_state(2) == Some(false) => {
                ("VALVE_COMMAND", 2)
            }
            TelemetryCommand::Pilot if state.get_umbilical_valve_state(0) == Some(true) => {
                ("VALVE_COMMAND", 3)
            }
            TelemetryCommand::Igniter if state.get_umbilical_valve_state(7) == Some(true) => {
                ("ACTUATOR_COMMAND", 11)
            }
            TelemetryCommand::Nitrogen if state.get_umbilical_valve_state(9) == Some(true) => {
                ("ACTUATOR_COMMAND", 12)
            }
            TelemetryCommand::Nitrous if state.get_umbilical_valve_state(10) == Some(true) => {
                ("ACTUATOR_COMMAND", 13)
            }
            _ => return None,
        };
        let captured_at = Instant::now();
        let token = Token::capture(state, owner);
        let router = state.topology_router.get()?.clone();
        let source_ms = crate::system_clock::required_control_network_utc_ms(&router).ok()?;
        Some(Self {
            token,
            router,
            captured_at,
            source_ms,
            command: crate::sequences::command_name(cmd),
            kind: crate::telemetry_schema::data_type(kind),
            byte,
        })
    }
    pub(crate) fn current(&self, state: &AppState) -> bool {
        use std::sync::atomic::Ordering;
        !state.shutdown_requested.load(Ordering::Acquire)
            && self.captured_at.elapsed() <= Duration::from_secs(5)
            && self.token.owner != 0
            && self.token.generation != 0
            && self.token.generation != u64::MAX
            && state.operator_admission_barrier.load(Ordering::Acquire) == self.token.owner
            && crate::gse::automatic_generation(state) == self.token.generation
            && state.operator_queue.relief_epoch() == self.token.relief_epoch
            && state
                .topology_router
                .get()
                .is_some_and(|r| Arc::ptr_eq(r, &self.router))
    }
    pub(crate) fn token(&self) -> Token {
        self.token
    }
}

pub(crate) async fn dispatch(
    state: Arc<AppState>,
    principal: &crate::auth::AuthPrincipal,
    intent: Intent,
    source_current: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> (axum::http::StatusCode, String) {
    use axum::http::StatusCode;
    if !principal.permissions.send_commands || !principal.allows_command_name(intent.command) {
        return (StatusCode::FORBIDDEN, "permission denied".into());
    }
    if state.operator_queue.fill_launch_active() {
        return (
            StatusCode::CONFLICT,
            "Fill-system controls are owned by the active launch sequence; Abort remains available"
                .into(),
        );
    }
    if !state.action_policy.lock().unwrap().software_buttons_enabled {
        return (
            StatusCode::CONFLICT,
            "Software controls are disabled".into(),
        );
    }
    static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let Ok(permit) = WORKERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(8)))
        .clone()
        .try_acquire_owned()
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Safe manual command worker busy; no command sent".into(),
        );
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let now = crate::system_clock::required_control_network_utc_ms(&intent.router)
            .map_err(|_| "Control clock unavailable")?;
        if !now
            .checked_sub(intent.source_ms)
            .is_some_and(|age| age <= 5000)
        {
            return Err("Safe manual command expired");
        }
        if source_current.as_ref().is_some_and(|current| !current()) {
            return Err("Command source disconnected");
        }
        let owner = if intent.kind == crate::telemetry_schema::data_type("VALVE_COMMAND") {
            "VALVE_BOARD"
        } else {
            "ACTUATOR_BOARD"
        };
        let packet = sedsnet::packet::Packet::new(
            intent.kind,
            &[
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                crate::telemetry_schema::endpoint(owner),
            ],
            &intent.router.sender(),
            intent.source_ms,
            Arc::from(&[intent.byte][..]),
        )
        .map_err(|_| "Safe manual packet invalid")?;
        // A refusal only because the SDK core was held admitted nothing; the
        // same safe direction is offered again within the shared window.
        const BUSY: &str = "SDK core stayed busy; safe manual command was not sent";
        crate::core_busy::retry_blocking(
            || {
                let packet = packet.clone();
                let (accepted, _claim) = crate::gse::admit_safe_manual_unless_unadmitted(
                    &state,
                    &intent,
                    || intent.router.try_tx_queue(packet),
                    |result| result.as_ref().is_err_and(crate::core_busy::is_core_busy),
                )
                .ok_or("Safe manual command cancelled by newer control or recovery")?;
                accepted.map_err(|error| if crate::core_busy::is_core_busy(&error) { BUSY }
                    else { "SDK did not accept safe manual command; no application retry" })
            },
            |error| *error == BUSY,
        )?;
        crate::telemetry_task::flush_command_tx(&intent.router, "safe manual relief");
        Ok::<_, &'static str>(())
    })
    .await;
    match result {
        Ok(Ok(())) => (StatusCode::ACCEPTED, "Safe direction accepted by SDK; board confirmation remains separate. Recovery is still retained.".into()),
        Ok(Err(error)) => (StatusCode::CONFLICT, error.into()),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "Safe manual worker failed; inspect board confirmation before retrying".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn safe_manual_expired_and_superseded_intents_never_renew_recovery_authority() {
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([
                sedsnet::router::EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
                    |_| Ok(()),
                ),
            ])
            .with_sender("GS"),
        ));
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                1_793_563_200_000,
            ))
            .unwrap();
        state.topology_router.set(router).unwrap();
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        state.action_policy.lock().unwrap().software_buttons_enabled = true;
        let mut intent = Intent::capture(&state, &TelemetryCommand::NitrogenClose).unwrap();
        intent.captured_at = Instant::now() - Duration::from_secs(6);
        assert!(
            crate::gse::admit_safe_manual(&state, &intent, || panic!("expired command sent"))
                .is_none()
        );
        let intent = Intent::capture(&state, &TelemetryCommand::NitrogenClose).unwrap();
        assert!(crate::gse::admit_safe_manual(&state, &intent, || ()).is_some());
        assert!(
            crate::gse::admit_safe_manual(&state, &intent, || panic!("replayed command sent"))
                .is_none()
        );
        assert_eq!(
            state
                .operator_admission_barrier
                .load(std::sync::atomic::Ordering::Acquire),
            77
        );
        assert_eq!(
            state
                .operator_dispatch_inflight
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }
    #[tokio::test]
    async fn safe_manual_static_direction_can_follow_retained_stop_but_not_a_newer_stop() {
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([
                sedsnet::router::EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
                    |_| Ok(()),
                ),
            ])
            .with_sender("GS"),
        ));
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                1_793_563_200_000,
            ))
            .unwrap();
        state.topology_router.set(router).unwrap();
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        state.action_policy.lock().unwrap().software_buttons_enabled = true;
        state.manual_abort_intent.request();
        let intent = Intent::capture(&state, &TelemetryCommand::NitrogenClose).unwrap();
        assert!(crate::gse::admit_safe_manual(&state, &intent, || ()).is_some());
        let intent = Intent::capture(&state, &TelemetryCommand::NitrogenClose).unwrap();
        state.manual_abort_intent.request();
        assert!(
            crate::gse::admit_safe_manual(&state, &intent, || panic!("old intent crossed Stop"))
                .is_none()
        );
        let intent = Intent::capture(&state, &TelemetryCommand::NitrogenClose).unwrap();
        let packet = sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("ABORT"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            100,
            Arc::from(&b"manual"[..]),
        )
        .unwrap();
        state
            .deferred_abort
            .observe(&packet, crate::gse::automatic_generation(&state));
        assert!(
            crate::gse::admit_safe_manual(&state, &intent, || panic!(
                "old intent crossed received Stop"
            ))
            .is_none()
        );
    }
}
