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
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
pub struct ClockStatus {
    pub utc_ms: u64,
    pub network_utc_ms: Option<i64>,
    pub can_set_system_time: bool,
    pub set_system_time_permission: bool,
    pub system_time_unavailable_reason: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRequest {
    Client { utc_ms: i64 },
    Network,
}

// SEDSnet already anchors a supplied full UTC reading to its monotonic clock
// and slews corrections. Remember which router has an operator-selected source
// so the one-second host-clock fallback cannot overwrite that selection.
static CLIENT_TIME_ROUTERS: std::sync::Mutex<Vec<std::sync::Weak<sedsnet::router::Router>>> =
    std::sync::Mutex::new(Vec::new());

fn retain_live_client_sources(sources: &mut Vec<std::sync::Weak<sedsnet::router::Router>>) {
    sources.retain(|source| source.strong_count() != 0);
}

pub(crate) fn refresh_network_source(router: &sedsnet::router::Router, host_utc_ms: u64) {
    let mut sources = CLIENT_TIME_ROUTERS.lock().unwrap();
    retain_live_client_sources(&mut sources);
    if sources
        .iter()
        .any(|source| std::ptr::eq(source.as_ptr(), router))
    {
        // Keep the SDK's monotonic anchor and normal GPS/remote election.
        // A wall-clock refresh here would silently undo Use client time.
        return;
    }
    crate::telemetry_task::refresh_host_network_time(router, host_utc_ms);
}

fn select_client_network_source(
    router: &Arc<sedsnet::router::Router>,
    utc_ms: i64,
) -> Result<(), String> {
    let mut sources = CLIENT_TIME_ROUTERS.lock().unwrap();
    retain_live_client_sources(&mut sources);
    // Hold policy selection through SDK admission so a concurrent periodic
    // host refresh cannot win between the accepted update and source commit.
    set_network_utc(router, utc_ms)?;
    if !sources
        .iter()
        .any(|source| source.ptr_eq(&Arc::downgrade(router)))
    {
        sources.push(Arc::downgrade(router));
    }
    Ok(())
}

fn capability_from_linux_status(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
        .is_some_and(|caps| caps & (1u64 << 25) != 0) // CAP_SYS_TIME
}

fn os_clock_unavailable_reason() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("OS clock updates require a Linux GroundStation or RRLS host.".into());
    }
    match std::fs::read_to_string("/proc/self/status") {
        Ok(status) if capability_from_linux_status(&status) => None,
        _ => Some("The service needs CAP_SYS_TIME. Run the native service as root and include CAP_SYS_TIME if systemd restricts capabilities.".into()),
    }
}

fn snapshot(permission: bool) -> ClockStatus {
    let unavailable = os_clock_unavailable_reason();
    ClockStatus {
        utc_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        network_utc_ms: crate::telemetry_task::recording_network_utc_ms(),
        can_set_system_time: permission && unavailable.is_none(),
        set_system_time_permission: permission,
        system_time_unavailable_reason: unavailable,
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
    match time_argument(utc) {
        Ok(_) => {}
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    }
    let Some(router) = state.topology_router.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Network router is not ready; the OS clock was not changed.",
        )
            .into_response();
    };
    if let Some(reason) = os_clock_unavailable_reason() {
        return (StatusCode::SERVICE_UNAVAILABLE, reason).into_response();
    }
    static UPDATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Ok(_guard) = UPDATE.try_lock() else {
        return (
            StatusCode::CONFLICT,
            "A clock update is already in progress.",
        )
            .into_response();
    };
    let before = snapshot(true).utc_ms;
    match update_os_and_network_time(utc, set_os_time, |utc| {
        select_client_network_source(router, utc)
    })
    .await
    {
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

/// Set the primary's SEDSnet UTC source without changing the host wall clock.
/// Authorization is independent of flight state and physical button interlocks.
pub async fn sync_network(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<SyncRequest>,
) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::SetSystemTime)
        .await
    {
        Ok(principal) if !principal.anonymous => principal,
        Ok(_) => return (StatusCode::UNAUTHORIZED, "Sign in to set network UTC.").into_response(),
        Err(error) => return error,
    };
    let SyncRequest::Client { utc_ms } = request else {
        return (
            StatusCode::BAD_REQUEST,
            "Supply a complete UTC date and time.",
        )
            .into_response();
    };
    if crate::rrls::is_rrls() {
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        return crate::command_proxy::submit(
            &state,
            &principal,
            token,
            crate::command_proxy::Operation::NetworkUtc(utc_ms),
        )
        .await;
    }
    apply_authorized_network_utc(state, principal, utc_ms).await
}

/// The primary owns UTC policy for both local and authenticated proxy calls.
pub(crate) async fn apply_authorized_network_utc(
    state: Arc<AppState>,
    principal: crate::auth::AuthPrincipal,
    utc_ms: i64,
) -> Response {
    if crate::rrls::is_rrls() || principal.anonymous || !principal.permissions.set_system_time {
        return (
            StatusCode::FORBIDDEN,
            "Primary network UTC authority required.",
        )
            .into_response();
    }
    let Some(router) = state.topology_router.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Network router is not ready.",
        )
            .into_response();
    };
    if let Err(error) = time_argument(utc_ms) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if let Err(error) = select_client_network_source(router, utc_ms) {
        return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
    }
    log::info!(
        "Network UTC source set by {:?} to {utc_ms} ms",
        principal.username
    );
    Json(snapshot(true)).into_response()
}

fn set_network_utc(router: &sedsnet::router::Router, utc_ms: i64) -> Result<(), String> {
    time_argument(utc_ms).map_err(str::to_string)?;
    router
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            utc_ms as u64,
        ))
        .map_err(|error| format!("Network UTC source was not admitted: {error}"))
}

#[cfg(test)]
mod network_tests {
    #[test]
    fn client_source_survives_periodic_host_refresh_and_advances_monotonically() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        let clock = Arc::new(AtomicU64::new(0));
        let input = clock.clone();
        let router = Arc::new(sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]),
            Box::new(move || input.load(Ordering::Relaxed)),
        ));
        let client = 1_800_000_000_000;
        super::select_client_network_source(&router, client).unwrap();
        for elapsed in 1..=5 {
            clock.store(elapsed * 1000, Ordering::Relaxed);
            super::refresh_network_source(&router, (client + 3_600_000) as u64);
            assert_eq!(
                router.network_time_ms(),
                Some(client as u64 + elapsed * 1000)
            );
        }
        // Failed input cannot replace the accepted source or its anchor.
        assert!(super::select_client_network_source(&router, -1).is_err());
        assert_eq!(router.network_time_ms(), Some(client as u64 + 5000));
    }

    #[test]
    fn host_fallback_remains_available_on_other_routers() {
        use std::sync::Arc;
        let selected = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        super::select_client_network_source(&selected, 1_800_000_000_000).unwrap();
        let other = sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]));
        super::refresh_network_source(&other, 1_900_000_000_000);
        let actual = other.network_time_ms().unwrap();
        assert!((1_900_000_000_000..1_900_000_001_000).contains(&actual));
    }

    #[test]
    fn utc_corrections_steer_the_existing_network_clock_without_a_hard_jump() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        let clock = Arc::new(AtomicU64::new(0));
        let input = clock.clone();
        let router = sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]),
            Box::new(move || input.load(Ordering::Relaxed)),
        );
        let base = 1_800_000_000_000;
        super::set_network_utc(&router, base).unwrap();
        clock.store(1000, Ordering::Relaxed);
        super::set_network_utc(&router, base + 11_000).unwrap();
        assert_eq!(router.network_time_ms(), Some(base as u64 + 1000));
        clock.store(2000, Ordering::Relaxed);
        assert_eq!(router.network_time_ms(), Some(base as u64 + 2050));
    }

    #[test]
    fn network_utc_changes_the_router_without_invoking_a_system_clock_command() {
        let router = sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]));
        let desired = 1_800_000_000_000;
        super::set_network_utc(&router, desired).unwrap();
        let actual = router.network_time().unwrap().unix_time_ms.unwrap();
        assert!((desired as u64..desired as u64 + 1000).contains(&actual));
        assert!(super::set_network_utc(&router, -1).is_err());
        assert!(super::set_network_utc(&router, i64::MAX).is_err());
    }
}
/// Separately test the irreversible OS update and the fallible SDK admission.
/// No clock mutation occurs until validation and the explicit caller authorize it.
async fn update_os_and_network_time<F, Fut, N>(
    utc_ms: i64,
    set_os: F,
    set_network: N,
) -> Result<(), String>
where
    F: FnOnce(i64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
    N: FnOnce(i64) -> Result<(), String>,
{
    time_argument(utc_ms).map_err(str::to_string)?;
    set_os(utc_ms).await?;
    set_network(utc_ms).map_err(|error| format!(
        "OS clock was updated, but the network UTC source was not accepted: {error}. Network corrections normally apply gradually."
    ))
}

async fn set_os_time(utc_ms: i64) -> Result<(), String> {
    time_argument(utc_ms).map_err(str::to_string)?;
    if let Some(reason) = os_clock_unavailable_reason() {
        return Err(reason);
    }
    #[cfg(target_os = "linux")]
    {
        let time = libc::timespec {
            tv_sec: (utc_ms / 1000).try_into().map_err(|_| {
                "Requested UTC is outside this platform's OS clock range.".to_string()
            })?,
            tv_nsec: ((utc_ms % 1000) * 1_000_000) as libc::c_long,
        };
        // CLOCK_REALTIME only: CLOCK_MONOTONIC and all process deadlines remain
        // unchanged. This syscall neither disables NTP nor invokes a shell.
        if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &time) } != 0 {
            return Err(format!(
                "OS clock was not changed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err("OS clock updates require a Linux host.".into())
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
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        assert!(state.topology_router.set(router.clone()).is_ok());
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer clock-test".parse().unwrap(),
        );
        let mut previous = 0;
        for flight_state in 0..=15 {
            *state.state.lock().unwrap() = crate::types::u8_to_flight_state(flight_state).unwrap();
            let utc_ms = 1_800_000_000_000 + i64::from(flight_state) * 60_000;
            let reply = sync_network(
                State(state.clone()),
                headers.clone(),
                Json(SyncRequest::Client { utc_ms }),
            )
            .await;
            assert_eq!(
                reply.status(),
                StatusCode::OK,
                "UTC setter in state {flight_state}"
            );
            let reading = router.network_time().unwrap().unix_time_ms.unwrap();
            // SEDSnet slews subsequent corrections instead of jumping running
            // network clocks. Flight state must not prevent accepting a source.
            assert!(reading >= previous);
            previous = reading;
        }
        save(false, false);
        assert!(
            !sync_network(
                State(state.clone()),
                headers.clone(),
                Json(SyncRequest::Client {
                    utc_ms: 1_900_000_000_000
                })
            )
            .await
            .status()
            .is_success()
        );
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
    #[test]
    fn kernel_capability_is_distinct_from_application_permission() {
        assert!(capability_from_linux_status(
            "Name:\tstation\nCapEff:\t0000000002000000\n"
        ));
        assert!(!capability_from_linux_status("CapEff:\t0000000000000000\n"));
        assert!(!capability_from_linux_status("CapEff: not-hex"));
        assert!(!capability_from_linux_status(""));
        let denied = snapshot(false);
        assert!(!denied.can_set_system_time && !denied.set_system_time_permission);
        #[cfg(not(target_os = "linux"))]
        assert!(
            snapshot(true)
                .system_time_unavailable_reason
                .unwrap()
                .contains("Linux")
        );
    }

    #[tokio::test]
    async fn explicit_os_update_then_checked_network_source_uses_only_injected_setter() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let observed = AtomicI64::new(0);
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        let desired = 1_800_000_000_123;
        update_os_and_network_time(
            desired,
            |utc| {
                observed.store(utc, Ordering::Relaxed);
                async { Ok(()) }
            },
            |utc| select_client_network_source(&router, utc),
        )
        .await
        .unwrap();
        assert_eq!(observed.load(Ordering::Relaxed), desired);
        let reading = router.network_time_ms().unwrap();
        assert!((desired as u64..desired as u64 + 1000).contains(&reading));
        refresh_network_source(&router, 1_900_000_000_000);
        assert!(router.network_time_ms().unwrap() < desired as u64 + 1000);
    }

    #[tokio::test]
    async fn clock_update_failures_report_which_side_changed() {
        use std::cell::Cell;
        let os_calls = Cell::new(0);
        let network_calls = Cell::new(0);
        let invalid = update_os_and_network_time(
            -1,
            |_| {
                os_calls.set(os_calls.get() + 1);
                async { Ok(()) }
            },
            |_| {
                network_calls.set(network_calls.get() + 1);
                Ok(())
            },
        )
        .await;
        assert!(invalid.is_err());
        assert_eq!((os_calls.get(), network_calls.get()), (0, 0));
        let failed = update_os_and_network_time(
            1_800_000_000_000,
            |_| {
                os_calls.set(os_calls.get() + 1);
                async { Err("mock kernel denied clock update".into()) }
            },
            |_| {
                network_calls.set(network_calls.get() + 1);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(failed.contains("kernel denied"));
        assert_eq!((os_calls.get(), network_calls.get()), (1, 0));
        let partial = update_os_and_network_time(
            1_800_000_000_000,
            |_| {
                os_calls.set(os_calls.get() + 1);
                async { Ok(()) }
            },
            |_| {
                network_calls.set(network_calls.get() + 1);
                Err("mock SDK admission failure".into())
            },
        )
        .await
        .unwrap_err();
        assert!(
            partial.contains("OS clock was updated")
                && partial.contains("network UTC source was not accepted")
        );
        assert_eq!((os_calls.get(), network_calls.get()), (2, 1));
    }
}
