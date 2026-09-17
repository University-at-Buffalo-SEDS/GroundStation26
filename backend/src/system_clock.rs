//! Explicit, permission-gated Linux wall-clock updates. Never run a shell.
use crate::{auth::Permission, state::AppState};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
pub struct ClockStatus {
    pub utc_ms: u64,
    pub network_utc_ms: Option<i64>,
    pub can_set_system_time: bool,
}
#[derive(Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRequest {
    Client { utc_ms: i64 },
    Network,
}

fn snapshot(can_set: bool) -> ClockStatus {
    ClockStatus {
        utc_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        network_utc_ms: crate::telemetry_task::recording_network_utc_ms(),
        can_set_system_time: can_set,
    }
}
fn time_argument(utc: i64) -> Result<String, &'static str> {
    if !(1_577_836_800_000..4_102_444_800_000).contains(&utc) {
        return Err("Date must be between 2020 and 2100.");
    }
    // systemd's epoch format is independent of the machine's local timezone.
    Ok(format!("@{}.{:03}", utc / 1000, utc % 1000))
}
pub async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let p = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    Json(snapshot(!p.anonymous && p.permissions.set_system_time)).into_response()
}
pub async fn sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<SyncRequest>,
) -> Response {
    let p = match crate::web::authorize_headers(&state, &headers, Permission::SetSystemTime).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if p.anonymous {
        return (
            StatusCode::UNAUTHORIZED,
            "Sign in with permission to set system time.",
        )
            .into_response();
    }
    let utc = match request {
        SyncRequest::Client { utc_ms } => utc_ms,
        SyncRequest::Network => match crate::telemetry_task::recording_network_utc_ms() {
            Some(utc) => utc,
            None => return (StatusCode::CONFLICT, "No complete network UTC is available. Wait for RF GPS time or use your device clock.").into_response(),
        },
    };
    let argument = match time_argument(utc) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    static UPDATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Ok(_guard) = UPDATE.try_lock() else {
        return (
            StatusCode::CONFLICT,
            "A clock update is already in progress.",
        )
            .into_response();
    };
    let before = snapshot(true).utc_ms;
    match set_os_time(&argument).await {
        Ok(()) => {
            log::warn!(
                "System clock changed by {:?}: previous UTC ms={}, requested UTC ms={}",
                p.username,
                before,
                utc
            );
            Json(snapshot(true)).into_response()
        }
        Err(error) => {
            log::error!("System clock update rejected: {error}");
            (StatusCode::SERVICE_UNAVAILABLE, error).into_response()
        }
    }
}
async fn set_os_time(argument: &str) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("Setting the machine clock is supported on Linux/systemd hosts only.".into());
    }
    let mut cmd = tokio::process::Command::new("/usr/bin/timedatectl");
    cmd.args(["--no-ask-password", "set-time", argument])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| "Clock update timed out; check systemd-timedated and polkit.".to_string())?
        .map_err(|e| format!("Could not invoke timedatectl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "System clock was not set. Check the service user's polkit set-time permission and automatic NTP synchronization. NTP was not disabled automatically. {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn clock_permission_requires_a_current_enabled_user_and_is_revocable() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        let config = |allowed: bool, disabled: bool| {
            serde_json::json!({
                "version":1, "anonymous":{"view_data":true,"set_system_time":true},
                "users":[{"username":"clock-user", "password":{"iterations":120000,"salt_b64":"","hash_b64":""},
                    "permissions":{"view_data":true,"send_commands":true,"set_system_time":allowed},"disabled":disabled}]
            })
        };
        let save = |allowed, disabled| {
            std::fs::write(
                root.0.join("auth.json"),
                config(allowed, disabled).to_string(),
            )
            .unwrap()
        };
        save(true, false);
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, None, Permission::SetSystemTime)
                .await
                .is_err()
        );
        let now = snapshot(false).utc_ms as i64;
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('clock-test','clock-user','session',1,1,?,?)")
            .bind(now).bind(now+60000).execute(&state.auth_db).await.unwrap();
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("clock-test"),
                    Permission::SetSystemTime
                )
                .await
                .is_ok()
        );
        save(false, false);
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("clock-test"),
                    Permission::SetSystemTime
                )
                .await
                .is_err()
        );
        save(true, true);
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("clock-test"),
                    Permission::SetSystemTime
                )
                .await
                .is_err()
        );
        save(true, false);
        sqlx::query("UPDATE auth_sessions SET expires_at_ms=0")
            .execute(&state.auth_db)
            .await
            .unwrap();
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("clock-test"),
                    Permission::SetSystemTime
                )
                .await
                .is_err()
        );
        // All rejected HTTP requests return before reaching the OS helper.
        let reply = sync(
            State(state),
            HeaderMap::new(),
            Json(SyncRequest::Client {
                utc_ms: 1_800_000_000_000,
            }),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::UNAUTHORIZED);
    }
    #[test]
    fn timestamp_is_validated_and_not_shell_input() {
        assert_eq!(time_argument(1_800_000_000_123).unwrap(), "@1800000000.123");
        for invalid in [-1, 0, i64::MAX, 4_102_444_800_000] {
            assert!(time_argument(invalid).is_err());
        }
        assert!(
            serde_json::from_str::<SyncRequest>(r#"{"source":"client","utc_ms":"; reboot"}"#)
                .is_err()
        );
    }
    #[test]
    fn clock_authority_is_separate_and_defaults_off() {
        let p: crate::auth::Permissions =
            serde_json::from_str(r#"{"view_data":true,"send_commands":true}"#).unwrap();
        assert!(!p.allows(Permission::SetSystemTime));
        let p: crate::auth::Permissions =
            serde_json::from_str(r#"{"set_system_time":true}"#).unwrap();
        assert!(p.allows(Permission::SetSystemTime));
        assert!(!p.allows(Permission::SendCommands));
    }
}
