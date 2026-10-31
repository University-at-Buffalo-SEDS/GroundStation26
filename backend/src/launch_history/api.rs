//! Metadata-only operator observations. This module never queues telemetry commands.
use super::{ClockRecord, RecoveryEvent};
use crate::{auth::Permission, state::AppState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Serialize)]
struct Status {
    recovery_marker_supported: bool,
    clock_reset_request_supported: bool,
    clock: ClockRecord,
    synchronized: bool,
    clock_known: bool,
    persistence_error: Option<String>,
    station: String,
    events: Vec<RecoveryEvent>,
    reset: crate::launch_clock_sync::ResetStatus,
}
fn station() -> String {
    crate::station_pairing::public()
        .map(|p| p.station_id)
        .unwrap_or_else(|| {
            if crate::rrls::is_rrls() {
                "rrls".into()
            } else {
                "primary".into()
            }
        })
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return error;
    }
    let station = station();
    let history = state.launch_history.lock().unwrap();
    Json(Status {
        recovery_marker_supported: true,
        clock_reset_request_supported: true,
        clock: history.snapshot(),
        synchronized: history.synchronized && history.clock_known(),
        clock_known: history.clock_known(),
        persistence_error: history.persistence_error(),
        station,
        events: history.events(),
        reset: crate::launch_clock_sync::reset_status(),
    })
    .into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    launch_session: String,
}
async fn mark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mark): Json<Mark>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(e) => return e,
        };
    let Some(actor) = principal.username.filter(|_| !principal.anonymous) else {
        return (
            StatusCode::FORBIDDEN,
            "An authenticated operator is required",
        )
            .into_response();
    };
    // Audit only a fingerprint of the authenticated session, never the bearer secret.
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let fingerprint = ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
        .as_ref()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let station = station();
    let clock_now = crate::telemetry_task::procedure_clock::now();
    let result = (|| -> anyhow::Result<_> {
        let mut history = state.launch_history.lock().unwrap();
        anyhow::ensure!(
            mark.launch_session == history.snapshot().session,
            "Launch session changed; refresh before marking recovered"
        );
        let mono = history.monotonic_ms(clock_now.model_monotonic_ms());
        let event = history.mark_reached(
            &actor,
            &fingerprint,
            &station,
            clock_now.timestamp_ms(),
            mono,
        )?;
        Ok((event, history.waiter()))
    })();
    let (event, waiter) = match result {
        Ok(v) => v,
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };
    if let Err(error) = super::flush(waiter).await {
        state.add_notification(format!("Recovery observation has not been saved: {error}"));
        return (StatusCode::INSUFFICIENT_STORAGE, error.to_string()).into_response();
    }
    Json(event).into_response()
}
async fn json(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return error;
    }
    let history = state.launch_history.lock().unwrap();
    (
        [(
            header::CONTENT_DISPOSITION,
            "attachment; filename=launch-recovery-history.json",
        )],
        Json(history.data.clone()),
    )
        .into_response()
}
fn csv_cell(value: impl ToString) -> String {
    format!("\"{}\"", value.to_string().replace('"', "\"\""))
}
async fn csv(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return error;
    }
    let events = state.launch_history.lock().unwrap().events();
    let mut csv="event_id,launch_session,authority,clock_revision,t0_utc_ms,reached_utc_ms,t_relative_ms,actor,auth_session,station,primary_verified\r\n".to_string();
    for event in events {
        csv.push_str(
            &[
                event.id,
                event.launch_session,
                event.authority,
                event.clock_revision.to_string(),
                event.t0_utc_ms.to_string(),
                event.reached_utc_ms.to_string(),
                event.t_relative_ms.to_string(),
                event.actor,
                event.auth_session,
                event.station,
                event.primary_verified.to_string(),
            ]
            .iter()
            .map(csv_cell)
            .collect::<Vec<_>>()
            .join(","),
        );
        csv.push_str("\r\n");
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=launch-recovery-history.csv",
            ),
        ],
        csv,
    )
        .into_response()
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/recovery", get(status))
        .route("/api/recovery/reached", post(mark))
        .route("/api/recovery/export.json", get(json))
        .route("/api/recovery/export.csv", get(csv))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_export::tests::{TestDirectory, export_state};
    async fn fixture() -> (TestDirectory, Arc<AppState>, HeaderMap) {
        let root = TestDirectory::new();
        let mut state = export_state(&root.0, false).await;
        std::fs::write(root.0.join("auth.json"),serde_json::json!({"users":[{"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}]}).to_string()).unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('recovery-test','operator','session',1,1,0,9999999999999)").execute(&state.auth_db).await.unwrap();
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.launch_history = Arc::new(std::sync::Mutex::new(
            super::super::Store::open(root.0.join("clock.json"), false, None, 10000, 0).unwrap(),
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer recovery-test".parse().unwrap(),
        );
        (root, state, headers)
    }
    #[tokio::test]
    async fn mark_requires_operator_and_current_session_and_is_durable_without_commands() {
        let (root, state, headers) = fixture().await;
        let clock = {
            let mut history = state.launch_history.lock().unwrap();
            history
                .update_primary(
                    crate::telemetry_db::LaunchClockMsg {
                        kind: crate::telemetry_db::LaunchClockKind::TPlus,
                        anchor_timestamp_ms: Some(9000),
                        duration_ms: None,
                    },
                    10000,
                    0,
                )
                .unwrap()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let mut state = state;
        Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
        let unauthorized = mark(
            State(state.clone()),
            HeaderMap::new(),
            Json(Mark {
                launch_session: clock.session.clone(),
            }),
        )
        .await;
        assert!(!unauthorized.status().is_success());
        assert_eq!(
            mark(
                State(state.clone()),
                headers.clone(),
                Json(Mark {
                    launch_session: "stale".into()
                })
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        for _ in 0..2 {
            assert_eq!(
                mark(
                    State(state.clone()),
                    headers.clone(),
                    Json(Mark {
                        launch_session: clock.session.clone()
                    })
                )
                .await
                .status(),
                StatusCode::OK
            );
        }
        assert!(rx.try_recv().is_err());
        let events = state.launch_history.lock().unwrap().events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor, "operator");
        assert_eq!(events[0].auth_session.len(), 64);
        assert!(!events[0].auth_session.contains("recovery-test"));
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.0.join("clock.json")).unwrap()).unwrap();
        assert_eq!(disk["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            json(State(state.clone()), headers.clone()).await.status(),
            StatusCode::OK
        );
        assert_eq!(csv(State(state), headers).await.status(), StatusCode::OK);
    }
    #[test]
    fn csv_quotes_fields_without_losing_actor_data() {
        assert_eq!(csv_cell("a,\"b\""), "\"a,\"\"b\"\"\"");
    }
}
