//! Explicit application administrators manage accounts; stream roles never grant it.
use crate::{
    auth::{self, CalibrationAccess, CommandAccess, Permission, Permissions, StreamRole},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/admin/users", get(users).post(save_user))
        .route("/api/admin/users/create", axum::routing::post(create_user))
        .route("/api/admin/users/delete", axum::routing::post(delete_user))
}
const LAST_ADMIN: &str = "Keep at least one enabled application administrator. Make another account an administrator first.";
const MISSING_ACCOUNT: &str = "That account no longer exists";
fn enabled_admin_count(config: &auth::UsersFile) -> usize {
    config
        .users
        .iter()
        .filter(|u| !u.disabled && u.roles.contains(&StreamRole::Admin))
        .count()
}
fn same_user(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}
/// A short, secret-free description of what an account edit changed.
fn describe_change(before: Option<&auth::UserRecord>, after: &auth::UserRecord, password_reset: bool) -> String {
    let Some(before) = before else {
        return format!(
            "created with roles {:?}{}",
            after.roles,
            if after.disabled { ", disabled" } else { "" }
        );
    };
    let mut changes = Vec::new();
    if before.roles != after.roles {
        changes.push(format!("roles {:?} -> {:?}", before.roles, after.roles));
    }
    for permission in Permission::ALL {
        let (old, new) = (before.permissions.flag(*permission), after.permissions.flag(*permission));
        if old != new {
            changes.push(format!("{} {}", permission.field_name(), if new { "granted" } else { "revoked" }));
        }
    }
    if before.command_access != after.command_access {
        changes.push("command limits changed".into());
    }
    if before.calibration_access != after.calibration_access {
        changes.push("calibration access changed".into());
    }
    if before.disabled != after.disabled {
        changes.push(if after.disabled { "disabled" } else { "enabled" }.into());
    }
    if password_reset {
        changes.push("password reset".into());
    }
    if changes.is_empty() {
        "no changes".into()
    } else {
        changes.join(", ")
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    username: String,
    roles: Vec<StreamRole>,
    permissions: Permissions,
    command_access: CommandAccess,
    calibration_access: CalibrationAccess,
    disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}
impl From<auth::UserRecord> for Account {
    fn from(user: auth::UserRecord) -> Self {
        Self {
            username: user.username,
            roles: user.roles,
            permissions: user.permissions,
            command_access: user.command_access,
            calibration_access: user.calibration_access,
            disabled: user.disabled,
            password: None,
        }
    }
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
pub(crate) async fn administrator(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<auth::AuthPrincipal, Response> {
    let principal = crate::web::authorize_headers(state, headers, Permission::ViewData).await?;
    if principal.anonymous || !principal.roles.contains(&StreamRole::Admin) {
        return Err((StatusCode::FORBIDDEN, "Application administrator required").into_response());
    }
    Ok(principal)
}
async fn users(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = administrator(&state, &headers).await {
        return error;
    }
    match state.auth.load_users_file() {
        Ok(config) => Json(
            config
                .users
                .into_iter()
                .map(Account::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Cannot read accounts").into_response(),
    }
}
async fn save_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(account): Json<Account>,
) -> Response {
    save_user_inner(state, headers, account, false).await
}
async fn create_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(account): Json<Account>,
) -> Response {
    save_user_inner(state, headers, account, true).await
}
async fn save_user_inner(
    state: Arc<AppState>,
    headers: HeaderMap,
    mut account: Account,
    create_only: bool,
) -> Response {
    let principal = match administrator(&state, &headers).await {
        Ok(p) => p,
        Err(error) => return error,
    };
    account.username = account.username.trim().to_string();
    if account.username.is_empty()
        || account.username.len() > 64
        || account.username.chars().any(char::is_control)
        || account.command_access.allowed_commands.len() > 128
    {
        return (
            StatusCode::BAD_REQUEST,
            "Usernames must be 1–64 characters without control characters, with at most 128 allowed commands",
        )
            .into_response();
    }
    // Reject self-demotion before hashing passwords or invalidating sessions.
    if principal
        .username
        .as_deref()
        .is_some_and(|actor| actor.eq_ignore_ascii_case(&account.username))
        && (account.disabled || !account.roles.contains(&StreamRole::Admin))
    {
        return (
            StatusCode::CONFLICT,
            "You cannot remove your own administrator role or disable your own account. Ask another administrator to make that change.",
        )
            .into_response();
    }
    let password = account
        .password
        .take()
        .filter(|password| !password.is_empty());
    if password
        .as_ref()
        .is_some_and(|password| !(12..=128).contains(&password.len()))
    {
        return (
            StatusCode::BAD_REQUEST,
            "Use a password of 12–128 characters",
        )
            .into_response();
    }
    let hash = if let Some(password) = password {
        match tokio::task::spawn_blocking(move || auth::create_password_hash(&password)).await {
            Ok(Ok(hash)) => Some(hash),
            _ => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "Cannot prepare password")
                    .into_response();
            }
        }
    } else {
        None
    };
    // Resetting a password invalidates old sessions before the new credential
    // becomes valid. Never leave an old session usable with a new password.
    if hash.is_some()
        && !create_only
        && sqlx::query("DELETE FROM auth_sessions WHERE username = ? COLLATE NOCASE")
            .bind(&account.username)
            .execute(&state.auth_db)
            .await
            .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot invalidate old sessions",
        )
            .into_response();
    }
    let actor = principal.username.unwrap_or_default();
    let password_reset = hash.is_some();
    let result = state.auth.update_users(|config| {
        if !config.users.iter().any(|u| {
            u.username.eq_ignore_ascii_case(&actor)
                && !u.disabled
                && u.roles.contains(&StreamRole::Admin)
        }) {
            return Err("Administrator permission changed".into());
        }
        account.permissions = account.permissions.normalized();
        account.command_access = account.command_access.normalized();
        account.calibration_access = account.calibration_access.normalized();
        if account.roles.iter().any(|r| {
            matches!(
                r,
                StreamRole::StreamMaster | StreamRole::StreamAdmin | StreamRole::Admin
            )
        }) {
            account.roles.retain(|r| *r != StreamRole::StreamViewer);
        }
        let existing = config
            .users
            .iter()
            .position(|user| user.username.eq_ignore_ascii_case(&account.username));
        if create_only && existing.is_some() {
            return Err("Account already exists".into());
        }
        let password = hash
            .or_else(|| existing.map(|i| config.users[i].password.clone()))
            .ok_or("New accounts require a password")?;
        let user = auth::UserRecord {
            username: account.username,
            roles: account.roles,
            permissions: account.permissions,
            command_access: account.command_access,
            calibration_access: account.calibration_access,
            disabled: account.disabled,
            password,
        };
        let result = Account::from(user.clone());
        let change = describe_change(existing.map(|i| &config.users[i]), &user, password_reset && existing.is_some());
        if let Some(index) = existing {
            config.users[index] = user;
        } else {
            if config.users.len() >= 128 {
                return Err("Account limit reached (128 accounts)".into());
            }
            config.users.push(user);
        }
        if enabled_admin_count(config) == 0 {
            return Err(LAST_ADMIN.into());
        }
        Ok((result, change))
    });
    match result {
        Ok((profile, change)) => {
            log::info!(
                "Account {} {} by administrator {}: {}",
                profile.username,
                if create_only { "created" } else { "updated" },
                actor,
                change
            );
            Json(profile).into_response()
        }
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRequest {
    username: String,
}
#[derive(Serialize)]
struct Deleted {
    deleted: String,
}
async fn delete_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<DeleteRequest>,
) -> Response {
    let principal = match administrator(&state, &headers).await {
        Ok(p) => p,
        Err(error) => return error,
    };
    let target = request.username.trim().to_string();
    if target.is_empty() || target.len() > 64 || target.chars().any(char::is_control) {
        return (StatusCode::BAD_REQUEST, "Choose an account to delete").into_response();
    }
    let actor = principal.username.unwrap_or_default();
    if same_user(&actor, &target) {
        return (
            StatusCode::CONFLICT,
            "You cannot delete your own account. Ask another administrator to remove it.",
        )
            .into_response();
    }
    let result = state.auth.update_users(|config| {
        if !config.users.iter().any(|u| {
            same_user(&u.username, &actor) && !u.disabled && u.roles.contains(&StreamRole::Admin)
        }) {
            return Err("Administrator permission changed".to_string());
        }
        let index = config
            .users
            .iter()
            .position(|u| same_user(&u.username, &target))
            .ok_or_else(|| MISSING_ACCOUNT.to_string())?;
        let removed = config.users.remove(index);
        if enabled_admin_count(config) == 0 {
            return Err(LAST_ADMIN.to_string());
        }
        Ok(removed.username)
    });
    match result {
        Ok(username) => {
            // Authorization already rejects sessions of removed accounts; also
            // drop their rows so nothing lingers if the name is reused.
            let _ = sqlx::query("DELETE FROM auth_sessions WHERE username = ? COLLATE NOCASE AND session_type <> 'revoked'")
                .bind(&username)
                .execute(&state.auth_db)
                .await;
            log::info!("Account {username} deleted by administrator {actor}");
            Json(Deleted { deleted: username }).into_response()
        }
        Err(error) if error == MISSING_ACCOUNT => (StatusCode::NOT_FOUND, error).into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_export::tests::{TestDirectory, export_state};
    async fn fixture(role: &str) -> (TestDirectory, Arc<AppState>, HeaderMap) {
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"),serde_json::json!({"users":[
            {"username":"administrator","roles":[role],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true,"voice_transmit":true}}
        ]}).to_string()).unwrap();
        for (token, name) in [
            ("admin-test", "administrator"),
            ("operator-test", "operator"),
        ] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
                .bind(token).bind(name).execute(&state.auth_db).await.unwrap();
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer admin-test".parse().unwrap(),
        );
        (root, state, headers)
    }
    #[tokio::test]
    async fn stream_admin_cannot_administer_users() {
        let (_root, state, headers) = fixture("stream_admin").await;
        assert_eq!(
            users(State(state), headers).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    #[tokio::test]
    async fn admin_listing_hides_passwords_and_admin_inherits_full_access() {
        let (_root, state, headers) = fixture("admin").await;
        let response = users(State(state.clone()), headers).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("password"));
        assert!(!text.contains("hash_b64"));
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, Some("admin-test"), Permission::SendCommands)
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn admin_inheritance_updates_legacy_sessions_and_role_revocation_is_immediate() {
        let (root, state, _) = fixture("admin").await;
        sqlx::query("UPDATE auth_sessions SET can_view_data=0, can_send_commands=0 WHERE token='admin-test'")
            .execute(&state.auth_db).await.unwrap();
        let principal = state
            .auth
            .authorize_token(&state.auth_db, Some("admin-test"), Permission::SendCommands)
            .await
            .unwrap();
        assert!(principal.permissions.voice_transmit && principal.permissions.set_system_time);
        assert!(principal.can_manage_stream());
        assert!(principal.allows_command_name("StreamControl"));
        assert!(principal.calibration_access.view && principal.calibration_access.edit);
        let mut config = state.auth.load_users_file().unwrap();
        config.users[0].roles.clear();
        std::fs::write(
            root.0.join("auth.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, Some("admin-test"), Permission::SendCommands)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn creating_a_viewer_stores_a_hash_without_hardware_authority() {
        let (_root, state, headers) = fixture("admin").await;
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(1));
        account.username = "new-viewer".into();
        account.permissions.send_commands = false;
        account.permissions.voice_transmit = false;
        account.password = Some("viewer temporary passphrase".into());
        assert_eq!(
            create_user(State(state.clone()), headers, Json(account))
                .await
                .status(),
            StatusCode::OK
        );
        let users = state.auth.load_users_file().unwrap().users;
        let created = users
            .iter()
            .find(|user| user.username == "new-viewer")
            .unwrap();
        assert!(created.permissions.view_data);
        assert!(!created.permissions.send_commands);
        assert!(!created.permissions.voice_transmit);
        assert!(!created.password.hash_b64.is_empty());
    }
    #[tokio::test]
    async fn duplicate_create_cannot_overwrite_an_account_or_revoke_its_sessions() {
        let (_root, state, headers) = fixture("admin").await;
        let before = state.auth.load_users_file().unwrap().users.remove(1);
        let mut account = Account::from(before.clone());
        account.username = "OPERATOR".into();
        account.disabled = true;
        assert_eq!(
            create_user(State(state.clone()), headers, Json(account))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        let after = state.auth.load_users_file().unwrap().users.remove(1);
        assert_eq!(after.disabled, before.disabled);
        assert_eq!(after.password.hash_b64, before.password.hash_b64);
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("operator-test"),
                    Permission::SendCommands
                )
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn password_reset_invalidates_existing_sessions_and_omits_credentials() {
        let (_root, state, headers) = fixture("admin").await;
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(1));
        account.password = Some("a new test password".into());
        let response = save_user(State(state.clone()), headers, Json(account)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(!text.contains("password"));
        assert!(!text.contains("a new test password"));
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, Some("operator-test"), Permission::ViewData)
                .await
                .is_err()
        );
        let hash = state
            .auth
            .load_users_file()
            .unwrap()
            .users
            .remove(1)
            .password;
        assert!(!hash.hash_b64.is_empty());
    }
    #[tokio::test]
    async fn last_administrator_cannot_be_disabled() {
        let (_root, state, headers) = fixture("admin").await;
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(0));
        account.disabled = true;
        assert_eq!(
            save_user(State(state.clone()), headers, Json(account))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert!(!state.auth.load_users_file().unwrap().users[0].disabled);
    }
    #[tokio::test]
    async fn admin_cannot_demote_self_even_with_another_admin() {
        let (_root, state, headers) = fixture("admin").await;
        state
            .auth
            .update_users(|config| {
                config.users[1].roles = vec![StreamRole::Admin];
                Ok(())
            })
            .unwrap();
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(0));
        account.username = "ADMINISTRATOR".into();
        account.roles.clear();
        account.password = Some("must not revoke this session".into());
        assert_eq!(
            save_user(State(state.clone()), headers, Json(account))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert!(
            state.auth.load_users_file().unwrap().users[0]
                .roles
                .contains(&StreamRole::Admin)
        );
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, Some("admin-test"), Permission::ViewData)
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn admin_can_grant_and_revoke_another_users_admin_access() {
        let (_root, state, headers) = fixture("admin").await;
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(1));
        account.roles = vec![StreamRole::Admin];
        assert_eq!(
            save_user(State(state.clone()), headers.clone(), Json(account))
                .await
                .status(),
            StatusCode::OK
        );
        assert!(
            state.auth.load_users_file().unwrap().users[1]
                .roles
                .contains(&StreamRole::Admin)
        );
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(1));
        account.roles.clear();
        assert_eq!(
            save_user(State(state.clone()), headers, Json(account))
                .await
                .status(),
            StatusCode::OK
        );
        assert!(
            !state.auth.load_users_file().unwrap().users[1]
                .roles
                .contains(&StreamRole::Admin)
        );
    }
    #[tokio::test]
    async fn permission_revocation_applies_to_existing_operator_sessions() {
        let (_root, state, headers) = fixture("admin").await;
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("operator-test"),
                    Permission::SendCommands
                )
                .await
                .is_ok()
        );
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(1));
        account.permissions.send_commands = false;
        account.permissions.voice_transmit = false;
        assert_eq!(
            save_user(State(state.clone()), headers, Json(account))
                .await
                .status(),
            StatusCode::OK
        );
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("operator-test"),
                    Permission::SendCommands
                )
                .await
                .is_err()
        );
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("operator-test"),
                    Permission::VoiceTransmit
                )
                .await
                .is_err()
        );
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, Some("operator-test"), Permission::ViewData)
                .await
                .is_ok()
        );
    }
    async fn delete(state: &Arc<AppState>, headers: HeaderMap, username: &str) -> StatusCode {
        delete_user(
            State(state.clone()),
            headers,
            Json(DeleteRequest { username: username.into() }),
        )
        .await
        .status()
    }
    #[tokio::test]
    async fn admin_session_holds_every_permission_with_any_stored_flags_or_stale_row() {
        let (_root, state, _) = fixture("admin").await;
        state
            .auth
            .update_users(|config| {
                config.users[0].roles = vec![StreamRole::StreamViewer, StreamRole::Admin];
                config.users[0].permissions = Permissions::default();
                config.users[0].command_access.allowed_commands = vec!["Pilot".into()];
                config.users[0].calibration_access = CalibrationAccess::default();
                Ok(())
            })
            .unwrap();
        sqlx::query("UPDATE auth_sessions SET can_view_data=0, can_send_commands=0, allowed_commands_json='[\"Pilot\"]' WHERE token='admin-test'")
            .execute(&state.auth_db).await.unwrap();
        for permission in Permission::ALL {
            let principal = state
                .auth
                .authorize_token(&state.auth_db, Some("admin-test"), *permission)
                .await
                .unwrap_or_else(|_| panic!("admin lacks {permission:?}"));
            assert_eq!(principal.permissions, Permissions::all());
            for command in ["FirmwareUpdate", "RetractPlumbing", "StreamControl", "Abort"] {
                assert!(principal.allows_command_name(command), "{command}");
            }
            assert!(principal.can_manage_stream() && principal.can_publish_camera());
            assert!(principal.calibration_access.edit);
        }
        let status = state
            .auth
            .session_status(&state.auth_db, Some("admin-test"))
            .await
            .unwrap();
        assert_eq!(status.permissions, Permissions::all());
        assert!(status.allowed_commands.is_empty());
    }
    #[tokio::test]
    async fn admin_can_delete_another_account_and_its_sessions_end() {
        let (_root, state, headers) = fixture("admin").await;
        assert_eq!(delete(&state, headers.clone(), "OPERATOR").await, StatusCode::OK);
        assert!(state.auth.load_users_file().unwrap().users.iter().all(|u| u.username != "operator"));
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, Some("operator-test"), Permission::ViewData)
                .await
                .is_err()
        );
        assert_eq!(delete(&state, headers, "operator").await, StatusCode::NOT_FOUND);
    }
    #[tokio::test]
    async fn admin_cannot_delete_self_or_the_last_admin() {
        let (_root, state, headers) = fixture("admin").await;
        assert_eq!(delete(&state, headers.clone(), "Administrator").await, StatusCode::CONFLICT);
        assert_eq!(state.auth.load_users_file().unwrap().users.len(), 2);
        // With a second admin, one admin may delete the other.
        state
            .auth
            .update_users(|config| {
                config.users[1].roles = vec![StreamRole::Admin];
                Ok(())
            })
            .unwrap();
        let mut operator = HeaderMap::new();
        operator.insert(axum::http::header::AUTHORIZATION, "Bearer operator-test".parse().unwrap());
        state
            .auth
            .update_users(|config| {
                config.users.push(auth::UserRecord {
                    username: "third".into(),
                    roles: vec![],
                    ..config.users[1].clone()
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(delete(&state, operator.clone(), "administrator").await, StatusCode::OK);
        // The remaining (last) admin can neither be demoted nor deleted.
        assert_eq!(delete(&state, operator.clone(), "operator").await, StatusCode::CONFLICT);
        let mut last = Account::from(state.auth.load_users_file().unwrap().users.remove(0));
        last.roles.clear();
        assert_eq!(
            save_user(State(state.clone()), operator, Json(last)).await.status(),
            StatusCode::CONFLICT
        );
    }
    #[tokio::test]
    async fn non_admins_cannot_delete_accounts() {
        let (_root, state, _) = fixture("stream_admin").await;
        let mut operator = HeaderMap::new();
        operator.insert(axum::http::header::AUTHORIZATION, "Bearer operator-test".parse().unwrap());
        assert_eq!(delete(&state, operator, "administrator").await, StatusCode::FORBIDDEN);
        assert_eq!(state.auth.load_users_file().unwrap().users.len(), 2);
    }
    #[tokio::test]
    async fn last_admin_demotion_by_another_path_is_rejected() {
        let (_root, state, headers) = fixture("admin").await;
        let mut account = Account::from(state.auth.load_users_file().unwrap().users.remove(0));
        account.roles = vec![StreamRole::StreamAdmin];
        let response = save_user(State(state.clone()), headers, Json(account)).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(state.auth.load_users_file().unwrap().users[0].roles.contains(&StreamRole::Admin));
    }
}
