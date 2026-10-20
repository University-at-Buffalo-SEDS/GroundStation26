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
async fn administrator(
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
        return (StatusCode::BAD_REQUEST, "Invalid username or command list").into_response();
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
        if let Some(index) = existing {
            config.users[index] = user;
        } else {
            if config.users.len() >= 128 {
                return Err("Account limit reached".into());
            }
            config.users.push(user);
        }
        if !config.users.iter().any(|u| {
            !u.disabled
                && u.permissions.for_roles(&u.roles).view_data
                && u.roles.contains(&StreamRole::Admin)
        }) {
            return Err("Keep at least one enabled application administrator".into());
        }
        Ok(result)
    });
    match result {
        Ok(profile) => Json(profile).into_response(),
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
}
