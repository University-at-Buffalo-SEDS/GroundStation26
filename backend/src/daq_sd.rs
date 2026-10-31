//! DAQ storage diagnostics and explicit, non-retained retry requests.
use crate::{auth::Permission, state::AppState, types::BoardDiagnostics};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const COMMAND_NAME: &str = "DaqSdRetry";
const COOLDOWN: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SdStatus {
    pub flags: u32,
    last_write_age_ms: Option<u32>,
    last_flush_age_ms: Option<u32>,
    init_stage: u32,
    retry_count: u32,
    requested_token: u32,
    completed_token: u32,
    init_failures: u32,
}
impl SdStatus {
    pub fn decode(words: &[u32]) -> Option<Self> {
        if words.len() != 8 || words[0] & !0xf != 0 {
            return None;
        }
        let age = |value| (value != u32::MAX).then_some(value);
        Some(Self {
            flags: words[0],
            last_write_age_ms: age(words[1]),
            last_flush_age_ms: age(words[2]),
            init_stage: words[3],
            retry_count: words[4],
            requested_token: words[5],
            completed_token: words[6],
            init_failures: words[7],
        })
    }
    pub fn overlay(&self, report: &mut BoardDiagnostics, received_age_ms: u64) {
        report.sd_status_received_age_ms = Some(received_age_ms);
        report.sd_flags = Some(self.flags);
        report.sd_last_write_age_ms = self.last_write_age_ms;
        report.sd_last_flush_age_ms = self.last_flush_age_ms;
        report.sd_init_stage = Some(self.init_stage);
        report.sd_retry_count = Some(self.retry_count);
        report.sd_retry_requested_token = Some(self.requested_token);
        report.sd_retry_completed_token = Some(self.completed_token);
        report.sd_init_failures = Some(self.init_failures);
    }
}

#[derive(Default)]
pub(crate) struct RetryGate {
    last_request: Option<(u32, Instant)>,
    awaiting: bool,
}
impl RetryGate {
    fn reserve(
        &mut self,
        status: &SdStatus,
        received_age_ms: u64,
        now: Instant,
        token: u32,
    ) -> Result<(), (StatusCode, &'static str)> {
        if received_age_ms > crate::network_health::TIMEOUT_MS || status.flags & 1 == 0 {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Fresh initialized DAQ SD diagnostics are required.",
            ));
        }
        if status.flags & 8 != 0 {
            return Err((
                StatusCode::CONFLICT,
                "DAQ SD retry is already pending or running.",
            ));
        }
        if let Some((previous_token, submitted)) = self.last_request {
            let age = now.saturating_duration_since(submitted);
            let completed = status.completed_token == previous_token;
            if self.awaiting && !completed && age < REQUEST_TIMEOUT {
                return Err((
                    StatusCode::CONFLICT,
                    "Waiting for DAQ SD retry completion diagnostics.",
                ));
            }
            if age < COOLDOWN {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "Wait five seconds between DAQ SD retry requests.",
                ));
            }
        }
        self.last_request = Some((token, now));
        self.awaiting = true;
        Ok(())
    }
    fn rejected(&mut self, token: u32) {
        if self
            .last_request
            .is_some_and(|(previous, _)| previous == token)
        {
            self.awaiting = false;
        }
    }
}

fn retry_packet(token: u32) -> sedsnet::TelemetryResult<sedsnet::packet::Packet> {
    if token == 0 {
        return Err(sedsnet::TelemetryError::BadArg);
    }
    sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("DAQ_SD_RETRY"),
        &[crate::telemetry_schema::endpoint("SD_CARD")],
        "GS",
        crate::telemetry_task::get_current_timestamp_ms(),
        Arc::from(token.to_le_bytes()),
    )
}

#[derive(Serialize)]
struct RetryAccepted {
    accepted: bool,
    token: u32,
    status: &'static str,
    message: &'static str,
}

fn local_authorization(
    anonymous: bool,
    allowed: bool,
    rrls: bool,
) -> Result<(), (StatusCode, &'static str)> {
    if anonymous {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Sign in to request a DAQ SD retry.",
        ));
    }
    if !allowed {
        return Err((
            StatusCode::FORBIDDEN,
            "DaqSdRetry command permission is required.",
        ));
    }
    if rrls {
        return Err((
            StatusCode::FORBIDDEN,
            "RRLS storage controls are read-only. Use the primary GroundStation.",
        ));
    }
    Ok(())
}

pub async fn retry(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(principal) => principal,
            Err(response) => return response,
        };
    if let Err(error) = local_authorization(
        principal.anonymous,
        principal.allows_command_name(COMMAND_NAME),
        crate::rrls::is_rrls(),
    ) {
        return error.into_response();
    }
    let Some(router) = state.topology_router.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "DAQ network router is unavailable.",
        )
            .into_response();
    };
    let transport_pending =
        router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("DAQ_SD_RETRY"));
    let now = Instant::now();
    let token = {
        let mut tracker = state.network_health.lock().unwrap();
        let Some((status, age)) = tracker.daq_sd_status(now) else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "DAQ SD status is unavailable; update DAQ firmware and wait for diagnostics.",
            )
                .into_response();
        };
        if age > crate::network_health::TIMEOUT_MS || status.flags & 1 == 0 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Fresh initialized DAQ SD diagnostics are required.",
            )
                .into_response();
        }
        if transport_pending {
            return (
                StatusCode::CONFLICT,
                "A DAQ SD retry command is still in transport.",
            )
                .into_response();
        }
        let mut token = rand::random::<u32>();
        while token == 0 || token == status.requested_token || token == status.completed_token {
            token = rand::random();
        }
        if let Err(error) = tracker.sd_retry_gate.reserve(&status, age, now, token) {
            return error.into_response();
        }
        token
    };
    let result = retry_packet(token).and_then(|packet| router.tx_queue(packet));
    if let Err(error) = result {
        state
            .network_health
            .lock()
            .unwrap()
            .sd_retry_gate
            .rejected(token);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("DAQ SD retry could not be queued: {error}"),
        )
            .into_response();
    }
    (StatusCode::ACCEPTED, Json(RetryAccepted { accepted: true, token, status: "queued",
        message: "Retry command queued; waiting for DAQ diagnostics. This does not confirm an SD write." })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn companion_rejects_wrong_lengths_flags_and_preserves_never_ages() {
        assert!(SdStatus::decode(&[]).is_none());
        assert!(SdStatus::decode(&[0; 7]).is_none());
        assert!(SdStatus::decode(&[0; 9]).is_none());
        assert!(SdStatus::decode(&[16, 0, 0, 0, 0, 0, 0, 0]).is_none());
        let status = SdStatus::decode(&[3, u32::MAX, 0, 5, 10, 42, 41, 7]).unwrap();
        let mut report = BoardDiagnostics::default();
        status.overlay(&mut report, 1234);
        assert_eq!(report.sd_flags, Some(3));
        assert_eq!(report.sd_last_write_age_ms, None);
        assert_eq!(report.sd_last_flush_age_ms, Some(0));
        assert_eq!(report.sd_status_received_age_ms, Some(1234));
        assert_eq!(report.sd_retry_requested_token, Some(42));
        assert_eq!(report.sd_retry_completed_token, Some(41));
    }
    #[test]
    fn retry_gate_bounds_inflight_cooldown_and_stale_status() {
        let mut gate = RetryGate::default();
        let now = Instant::now();
        let mut status = SdStatus::decode(&[1, u32::MAX, u32::MAX, 1, 0, 0, 0, 0]).unwrap();
        assert_eq!(
            gate.reserve(&status, 12_001, now, 1).unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(gate.reserve(&status, 0, now, 1).is_ok());
        assert_eq!(
            gate.reserve(&status, 0, now + Duration::from_secs(6), 2)
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        status.completed_token = 1;
        assert_eq!(
            gate.reserve(&status, 0, now + Duration::from_secs(1), 2)
                .unwrap_err()
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(
            gate.reserve(&status, 0, now + Duration::from_secs(5), 2)
                .is_ok()
        );
        status.flags |= 8;
        assert_eq!(
            gate.reserve(&status, 0, now + Duration::from_secs(30), 3)
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        status.flags &= !8;
        assert!(
            gate.reserve(&status, 0, now + Duration::from_secs(30), 3)
                .is_ok()
        );
    }
    #[test]
    fn authorization_denies_anonymous_allowlist_and_rrls() {
        assert_eq!(
            local_authorization(true, true, false).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            local_authorization(false, false, false).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            local_authorization(false, true, true).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        assert!(local_authorization(false, true, false).is_ok());
    }
    #[test]
    fn retry_packet_is_nonzero_uint32_direct_and_not_managed() {
        crate::telemetry_schema::initialize().unwrap();
        assert!(retry_packet(0).is_err());
        let packet = retry_packet(0x12345678).unwrap();
        assert_eq!(packet.data_type().as_u32(), 24116261);
        assert_eq!(
            packet.endpoints(),
            &[crate::telemetry_schema::endpoint("SD_CARD")]
        );
        assert_eq!(packet.data_as_u32().unwrap().as_slice(), &[0x12345678]);
        let router =
            sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        router.tx_queue(packet).unwrap();
        assert_eq!(router.export_runtime_stats().queues.tx_len, 1);
        assert!(
            router
                .get_cached_network_variable(crate::telemetry_schema::data_type("DAQ_SD_RETRY"))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn queued_retry_reaches_the_discovered_sd_side_without_managed_replay() {
        use std::sync::Mutex;
        crate::telemetry_schema::initialize().unwrap();
        let router =
            sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        let seen = Arc::new(Mutex::new(Vec::<u32>::new()));
        let captured = seen.clone();
        let side = router.add_side_packet("test-can", move |packet| {
            if packet.data_type() == crate::telemetry_schema::data_type("DAQ_SD_RETRY") {
                assert_eq!(
                    packet.endpoints(),
                    &[crate::telemetry_schema::endpoint("SD_CARD")]
                );
                captured
                    .lock()
                    .unwrap()
                    .extend(packet.data_as_u32().unwrap());
            }
            Ok(())
        });
        router
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "DAQ",
                    1,
                    &[crate::telemetry_schema::endpoint("SD_CARD")],
                )
                .unwrap(),
                side,
            )
            .unwrap();
        router.tx_queue(retry_packet(42).unwrap()).unwrap();
        router.process_all_queues_with_timeout(20).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![42]);
        assert!(
            router
                .get_cached_network_variable(crate::telemetry_schema::data_type("DAQ_SD_RETRY"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn api_requires_current_auth_fresh_daq_status_and_truthfully_queues_once() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        let save = |allowed: bool, command: &str| {
            std::fs::write(root.0.join("auth.json"), serde_json::json!({
                "version":1,"anonymous":{"view_data":true,"send_commands":false},
                "users":[{"username":"sd-operator","password":{"iterations":120000,"salt_b64":"","hash_b64":""},
                    "permissions":{"view_data":true,"send_commands":allowed},
                    "command_access":{"allowed_commands":[command]}}]
            }).to_string()).unwrap();
        };
        save(true, COMMAND_NAME);
        let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('sd-test','sd-operator','session',1,1,?,?)")
            .bind(now).bind(now+60000).execute(&state.auth_db).await.unwrap();
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        ));
        assert!(state.topology_router.set(router.clone()).is_ok());
        let denied = retry(State(state.clone()), HeaderMap::new()).await.status();
        assert!(matches!(
            denied,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer sd-test".parse().unwrap(),
        );
        save(true, "OtherCommand");
        assert_eq!(
            retry(State(state.clone()), headers.clone()).await.status(),
            StatusCode::FORBIDDEN
        );
        save(true, COMMAND_NAME);
        assert_eq!(
            retry(State(state.clone()), headers.clone()).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let words = [1, u32::MAX, u32::MAX, 1, 0, 0, 0, 0];
        state.network_health.lock().unwrap().observe_sd_status(
            "DAQ",
            &words,
            Instant::now(),
            now as u64,
        );
        // Storage maintenance is allowed in primary normal/test-fire/HITL builds,
        // including an observed launch state; it never triggers a physical output.
        *state.state.lock().unwrap() = crate::types::FlightState::Launch;
        let response = retry(State(state.clone()), headers.clone()).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["accepted"], true);
        assert_eq!(body["status"], "queued");
        assert!(body["token"].as_u64().unwrap() > 0);
        assert!(body.get("ready").is_none());
        assert_eq!(router.export_runtime_stats().queues.tx_len, 1);
        assert_eq!(
            retry(State(state.clone()), headers.clone()).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(router.export_runtime_stats().queues.tx_len, 1);
        save(false, COMMAND_NAME);
        assert_eq!(
            retry(State(state.clone()), headers.clone()).await.status(),
            StatusCode::FORBIDDEN
        );
        save(true, COMMAND_NAME);
        state.network_health.lock().unwrap().observe_sd_status(
            "DAQ",
            &words,
            Instant::now() - Duration::from_secs(13),
            now as u64,
        );
        assert_eq!(
            retry(State(state), headers).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
