use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD};
use ring::hmac;
use ring::pbkdf2;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const USERS_FILE_VERSION: u32 = 1;
const DEFAULT_SESSION_TTL_SECONDS: u64 = 60 * 60 * 24 * 14;
const LOGIN_CHALLENGE_TTL_MS: i64 = 60_000;
const LOGIN_CHALLENGE_BYTES: usize = 32;
#[allow(dead_code)]
const DEFAULT_PBKDF2_ITERATIONS: u32 = 120_000;
#[allow(dead_code)]
const PBKDF2_OUTPUT_LEN: usize = 32;

static PENDING_LOGIN_CHALLENGES: LazyLock<Mutex<HashMap<String, PendingLoginChallenge>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Declares every grantable permission exactly once. The flag struct, the
/// `Permission` enum, `Permission::ALL` and `Permissions::all()` are all
/// generated from this one list, so an application administrator (who always
/// receives `Permissions::all()`) cannot be missing a permission added later.
macro_rules! permission_set {
    ($($field:ident => $variant:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
        pub struct Permissions {
            $(#[serde(default)] pub $field: bool,)+
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Permission {
            $($variant,)+
        }

        impl Permission {
            /// Every permission, in declaration order.
            #[allow(dead_code)]
            pub const ALL: &'static [Permission] = &[$(Permission::$variant,)+];

            /// The stored/serialized flag name for this permission.
            #[allow(dead_code)]
            pub const fn field_name(self) -> &'static str {
                match self {
                    $(Permission::$variant => stringify!($field),)+
                }
            }
        }

        impl Permissions {
            /// Every permission granted; the only grant an administrator holds.
            pub const fn all() -> Self {
                Self { $($field: true,)+ }
            }

            /// The raw stored flag, without dependency normalization.
            #[allow(dead_code)]
            pub const fn flag(self, permission: Permission) -> bool {
                match permission {
                    $(Permission::$variant => self.$field,)+
                }
            }

            /// Grants held by both sides (paired sessions, revocation).
            pub const fn intersect(self, other: Self) -> Self {
                Self { $($field: self.$field && other.$field,)+ }
            }
        }
    };
}

permission_set! {
    view_data => ViewData,
    send_commands => SendCommands,
    set_system_time => SetSystemTime,
    voice_transmit => VoiceTransmit,
    camera_publish => CameraPublish,
    sign_checklists => SignChecklists,
}

impl Permissions {
    /// Effective permissions for an account. Administrators always hold every
    /// permission regardless of the stored flags; other roles use their flags.
    pub fn for_roles(self, roles: &[StreamRole]) -> Self {
        if roles.contains(&StreamRole::Admin) {
            Self::all()
        } else {
            self.normalized()
        }
    }
    pub fn normalized(mut self) -> Self {
        if self.send_commands {
            self.view_data = true;
        }
        self
    }

    pub fn allows(self, required: Permission) -> bool {
        let normalized = self.normalized();
        match required {
            Permission::ViewData => normalized.view_data,
            Permission::SendCommands => normalized.send_commands,
            Permission::SetSystemTime => normalized.set_system_time,
            Permission::VoiceTransmit => normalized.view_data && normalized.voice_transmit,
            Permission::CameraPublish => normalized.view_data && normalized.camera_publish,
            Permission::SignChecklists => normalized.view_data && normalized.sign_checklists,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CommandAccess {
    #[serde(default)]
    pub allowed_commands: Vec<String>,
}

impl CommandAccess {
    pub fn normalized(mut self) -> Self {
        self.allowed_commands = self
            .allowed_commands
            .into_iter()
            .map(|cmd| cmd.trim().to_string())
            .filter(|cmd| !cmd.is_empty())
            .collect();
        self.allowed_commands.sort();
        self.allowed_commands.dedup();
        self
    }

    pub fn allows(&self, cmd: &str) -> bool {
        self.allowed_commands.is_empty() || self.allowed_commands.iter().any(|item| item == cmd)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CalibrationAccess {
    #[serde(default)]
    pub view: bool,
    #[serde(default)]
    pub edit: bool,
}

impl CalibrationAccess {
    pub fn normalized(mut self) -> Self {
        if self.edit {
            self.view = true;
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasswordHashRecord {
    #[serde(default = "default_password_algorithm")]
    pub algorithm: String,
    #[serde(default)]
    pub iterations: u32,
    pub salt_b64: String,
    pub hash_b64: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamRole {
    Admin,
    StreamMaster,
    StreamAdmin,
    StreamViewer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    #[serde(default)]
    pub roles: Vec<StreamRole>,
    pub username: String,
    pub password: PasswordHashRecord,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub command_access: CommandAccess,
    #[serde(default)]
    pub calibration_access: CalibrationAccess,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsersFile {
    #[serde(default = "default_users_file_version")]
    pub version: u32,
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
    #[serde(default)]
    pub anonymous: Permissions,
    #[serde(default)]
    pub anonymous_command_access: CommandAccess,
    #[serde(default)]
    pub anonymous_calibration_access: CalibrationAccess,
    #[serde(default)]
    pub users: Vec<UserRecord>,
}

impl Default for UsersFile {
    fn default() -> Self {
        Self {
            version: USERS_FILE_VERSION,
            session_ttl_seconds: DEFAULT_SESSION_TTL_SECONDS,
            anonymous: Permissions::default(),
            anonymous_command_access: CommandAccess::default(),
            anonymous_calibration_access: CalibrationAccess::default(),
            users: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub station_name: &'static str,
    pub station_role: &'static str,
    pub station_pairing: Option<crate::station_pairing::Public>,
    pub operating_mode: crate::station_mode::Snapshot,
    pub command_proxy: crate::command_proxy::Status,
    pub roles: Vec<StreamRole>,
    pub authenticated: bool,
    pub username: Option<String>,
    pub permissions: Permissions,
    pub expires_at_ms: Option<i64>,
    pub anonymous: bool,
    pub session_type: Option<String>,
    pub allowed_commands: Vec<String>,
    pub can_view_calibration: bool,
    pub can_edit_calibration: bool,
}

#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub roles: Vec<StreamRole>,
    pub username: Option<String>,
    pub permissions: Permissions,
    pub expires_at_ms: Option<i64>,
    pub anonymous: bool,
    pub session_type: Option<String>,
    pub command_access: CommandAccess,
    pub calibration_access: CalibrationAccess,
}

impl AuthPrincipal {
    /// A signed-in application administrator.
    pub fn is_admin(&self) -> bool {
        !self.anonymous && self.roles.contains(&StreamRole::Admin)
    }

    /// Every authenticated principal is finished here. An administrator always
    /// holds every permission, unrestricted commands and full calibration
    /// access; no stored flag, stale session row or paired claim can narrow it.
    /// Station/mode/proxy admission rules are enforced separately and still apply.
    fn with_role_authority(mut self) -> Self {
        if self.is_admin() {
            self.permissions = Permissions::all();
            self.command_access = CommandAccess::default();
            self.calibration_access = CalibrationAccess {
                view: true,
                edit: true,
            };
            self.roles.retain(|role| *role != StreamRole::StreamViewer);
        }
        self
    }

    pub fn can_publish_camera(&self) -> bool {
        !self.anonymous
            && (self.roles.contains(&StreamRole::Admin)
                || self.permissions.allows(Permission::CameraPublish))
    }

    pub fn can_manage_stream(&self) -> bool {
        if self.roles.contains(&StreamRole::StreamViewer)
            && !self.roles.contains(&StreamRole::StreamAdmin)
            && !self.roles.contains(&StreamRole::Admin)
        {
            return false;
        }
        !self.anonymous
            && (self.roles.contains(&StreamRole::StreamMaster)
                || self.roles.contains(&StreamRole::StreamAdmin)
                || self.roles.contains(&StreamRole::Admin)
                || self
                    .command_access
                    .allowed_commands
                    .iter()
                    .any(|c| c == "StreamControl"))
    }
    pub fn session_status(&self) -> SessionStatus {
        SessionStatus {
            station_name: if crate::rrls::is_rrls() {
                "RRLS"
            } else {
                "GroundStation"
            },
            station_role: if crate::rrls::is_rrls() {
                "rrls"
            } else {
                "primary"
            },
            station_pairing: crate::station_pairing::public(),
            operating_mode: crate::station_mode::snapshot(),
            command_proxy: crate::command_proxy::status(),
            roles: self.roles.clone(),
            authenticated: !self.anonymous,
            username: self.username.clone(),
            permissions: self.permissions,
            expires_at_ms: self.expires_at_ms,
            anonymous: self.anonymous,
            session_type: self.session_type.clone(),
            allowed_commands: self.command_access.allowed_commands.clone(),
            can_view_calibration: self.calibration_access.view,
            can_edit_calibration: self.calibration_access.edit,
        }
    }

    pub fn allows_command_name(&self, cmd: &str) -> bool {
        self.permissions.send_commands && self.command_access.allows(cmd)
    }
}

fn configured_user_access(
    config: &UsersFile,
    username: &str,
) -> (CommandAccess, CalibrationAccess) {
    config
        .users
        .iter()
        .find(|user| user.username.eq_ignore_ascii_case(username))
        .map(|user| {
            if user.roles.contains(&StreamRole::Admin) {
                (
                    CommandAccess::default(),
                    CalibrationAccess {
                        view: true,
                        edit: true,
                    },
                )
            } else {
                (user.command_access.clone(), user.calibration_access)
            }
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod stream_role_tests {
    use super::*;
    fn principal(roles: Vec<StreamRole>, anonymous: bool) -> AuthPrincipal {
        AuthPrincipal {
            roles,
            username: Some("producer".into()),
            permissions: Permissions {
                view_data: true,
                send_commands: false,
                set_system_time: false,
                voice_transmit: false,
                camera_publish: false,
                sign_checklists: false,
            },
            expires_at_ms: None,
            anonymous,
            session_type: Some("session".into()),
            command_access: CommandAccess::default(),
            calibration_access: CalibrationAccess::default(),
        }
    }
    #[test]
    fn stream_master_is_independent_of_hardware_permissions() {
        let p = principal(vec![StreamRole::StreamMaster], false);
        assert!(p.can_manage_stream());
        assert!(!p.allows_command_name("StartFill"));
        assert_eq!(p.session_status().roles, vec![StreamRole::StreamMaster]);
    }
    #[test]
    fn anonymous_and_ordinary_viewers_cannot_direct() {
        assert!(!principal(vec![], false).can_manage_stream());
        assert!(!principal(vec![StreamRole::StreamMaster], true).can_manage_stream());
    }
    #[test]
    fn explicit_viewer_revokes_legacy_grant_without_broadening_commands() {
        let mut p = principal(vec![StreamRole::StreamViewer], false);
        p.command_access.allowed_commands = vec!["StreamControl".into()];
        assert!(!p.can_manage_stream());
        assert_eq!(p.command_access.allowed_commands, vec!["StreamControl"]);
    }
}

#[cfg(test)]
mod admin_authority_tests {
    use super::*;

    fn user(name: &str, roles: Vec<StreamRole>, permissions: Permissions) -> UserRecord {
        UserRecord {
            roles,
            username: name.into(),
            password: PasswordHashRecord {
                algorithm: default_password_algorithm(),
                iterations: 1,
                salt_b64: String::new(),
                hash_b64: String::new(),
            },
            permissions,
            command_access: CommandAccess {
                allowed_commands: vec!["Pilot".into()],
            },
            calibration_access: CalibrationAccess::default(),
            disabled: false,
        }
    }

    /// Every stored flag combination an admin record might carry, including
    /// none at all and each single flag on its own.
    fn stored_variants() -> Vec<Permissions> {
        let mut variants = vec![Permissions::default(), Permissions::all()];
        for permission in Permission::ALL {
            let mut only: serde_json::Value = serde_json::to_value(Permissions::default()).unwrap();
            only[permission.field_name()] = true.into();
            variants.push(serde_json::from_value(only).unwrap());
        }
        variants
    }

    fn assert_holds_everything(permissions: Permissions, context: &str) {
        assert_eq!(permissions, Permissions::all(), "{context}");
        for permission in Permission::ALL {
            assert!(permissions.flag(*permission), "{context}: {permission:?} flag");
            assert!(permissions.allows(*permission), "{context}: {permission:?} allowed");
        }
    }

    #[test]
    fn permission_list_covers_every_serialized_flag() {
        // Fails if a flag is ever added to `Permissions` without also being
        // part of `Permission::ALL` (and therefore `Permissions::all()`).
        let default = serde_json::to_value(Permissions::default()).unwrap();
        let mut stored: Vec<_> = default.as_object().unwrap().keys().cloned().collect();
        let mut listed: Vec<_> = Permission::ALL.iter().map(|p| p.field_name().to_string()).collect();
        stored.sort();
        listed.sort();
        assert_eq!(stored, listed);
        let all = serde_json::to_value(Permissions::all()).unwrap();
        assert!(all.as_object().unwrap().values().all(|v| v == &serde_json::Value::Bool(true)));
        assert_eq!(all.as_object().unwrap().len(), Permission::ALL.len());
    }

    #[test]
    fn admin_role_grants_every_permission_whatever_is_stored() {
        for stored in stored_variants() {
            for roles in [
                vec![StreamRole::Admin],
                vec![StreamRole::Admin, StreamRole::StreamViewer],
                vec![StreamRole::StreamViewer, StreamRole::Admin, StreamRole::StreamMaster],
            ] {
                assert_holds_everything(stored.for_roles(&roles), &format!("{stored:?} {roles:?}"));
            }
        }
    }

    #[test]
    fn non_admin_roles_keep_exactly_their_stored_flags() {
        for stored in stored_variants() {
            for roles in [
                vec![],
                vec![StreamRole::StreamAdmin],
                vec![StreamRole::StreamMaster],
                vec![StreamRole::StreamViewer],
            ] {
                assert_eq!(stored.for_roles(&roles), stored.normalized(), "{roles:?}");
            }
        }
        let viewer = Permissions { view_data: true, ..Default::default() };
        assert!(!viewer.for_roles(&[StreamRole::StreamAdmin]).allows(Permission::VoiceTransmit));
    }

    fn assert_admin_principal(principal: &AuthPrincipal, context: &str) {
        assert_holds_everything(principal.permissions, context);
        assert!(principal.command_access.allowed_commands.is_empty(), "{context}");
        for command in ["FirmwareUpdate", "StreamControl", "RetractPlumbing", "Abort", "ValveSelfTest"] {
            assert!(principal.allows_command_name(command), "{context}: {command}");
        }
        assert!(principal.calibration_access.view && principal.calibration_access.edit, "{context}");
        assert!(principal.can_manage_stream() && principal.can_publish_camera(), "{context}");
        assert!(!principal.roles.contains(&StreamRole::StreamViewer), "{context}");
        let status = principal.session_status();
        assert_holds_everything(status.permissions, context);
        assert!(status.can_view_calibration && status.can_edit_calibration, "{context}");
    }

    fn claims(roles: Vec<StreamRole>, permissions: Permissions, issuer: &str) -> crate::station_pairing::Claims {
        crate::station_pairing::Claims {
            installation_id: "test".into(),
            issuer: issuer.into(),
            username: "Chief".into(),
            issued_at_ms: now_ms(),
            expires_at_ms: now_ms() + 60_000,
            nonce: "n".into(),
            roles,
            permissions,
            command_access: CommandAccess { allowed_commands: vec!["Pilot".into()] },
            calibration_access: CalibrationAccess::default(),
            remembered: false,
        }
    }

    #[test]
    fn paired_admin_holds_every_permission_even_with_stale_claims() {
        for stored in stored_variants() {
            let config = UsersFile {
                users: vec![user("chief", vec![StreamRole::Admin, StreamRole::StreamViewer], stored)],
                ..Default::default()
            };
            for claimed in stored_variants() {
                for permission in Permission::ALL {
                    // Peer-issued portable session: both stations say admin.
                    let principal = paired_principal(
                        &config,
                        &claims(vec![StreamRole::Admin], claimed, "primary"),
                        "rrls",
                        *permission,
                        now_ms(),
                    )
                    .unwrap();
                    assert_admin_principal(&principal, "peer-issued");
                    // A token this station issued follows the current local role.
                    let principal = paired_principal(
                        &config,
                        &claims(vec![], claimed, "rrls"),
                        "rrls",
                        *permission,
                        now_ms(),
                    )
                    .unwrap();
                    assert_admin_principal(&principal, "self-issued");
                }
            }
        }
    }

    #[test]
    fn paired_non_admin_and_unconfirmed_admin_stay_intersected() {
        let operator = Permissions { view_data: true, send_commands: true, ..Default::default() };
        let config = UsersFile {
            users: vec![user("chief", vec![StreamRole::Admin], operator)],
            ..Default::default()
        };
        // The peer did not vouch for administrator: no admin authority here.
        let principal = paired_principal(
            &config,
            &claims(vec![], operator, "primary"),
            "rrls",
            Permission::ViewData,
            now_ms(),
        )
        .unwrap();
        assert!(!principal.is_admin());
        assert!(!principal.permissions.voice_transmit);
        assert!(paired_principal(&config, &claims(vec![], operator, "primary"), "rrls", Permission::VoiceTransmit, now_ms()).is_err());
        // Plain operators keep the claim/local intersection and command list.
        let config = UsersFile {
            users: vec![user("chief", vec![], operator)],
            ..Default::default()
        };
        let principal = paired_principal(
            &config,
            &claims(vec![StreamRole::Admin], Permissions::all(), "primary"),
            "rrls",
            Permission::SendCommands,
            now_ms(),
        )
        .unwrap();
        assert_eq!(principal.permissions, operator);
        assert!(principal.allows_command_name("Pilot"));
        assert!(!principal.allows_command_name("FirmwareUpdate"));
    }

    #[test]
    fn primary_proxy_actor_admin_holds_every_permission() {
        let root = crate::recording_export::tests::TestDirectory::new();
        let manager = AuthManager::new(root.0.join("auth.json"));
        for stored in stored_variants() {
            let config = UsersFile {
                users: vec![user("chief", vec![StreamRole::Admin, StreamRole::StreamViewer], stored)],
                ..Default::default()
            };
            std::fs::write(manager.path(), serde_json::to_vec(&config).unwrap()).unwrap();
            for permission in Permission::ALL {
                let principal = manager.paired_proxy_actor("CHIEF", *permission).unwrap();
                assert_admin_principal(&principal, "proxy actor");
            }
        }
    }
}

#[derive(Debug)]
pub enum AuthFailure {
    Unauthorized(String),
    Forbidden(String),
    Internal(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginChallengeRequest {
    pub username: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginChallengeResponse {
    pub challenge_id: String,
    pub algorithm: String,
    pub iterations: u32,
    pub salt_b64: String,
    pub server_nonce_b64: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub challenge_id: String,
    pub client_nonce_b64: String,
    pub proof_b64: String,
    #[serde(default)]
    pub remember_me: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub session: SessionStatus,
}

#[derive(Debug, Clone, Serialize)]
struct AuthProofPayload<'a> {
    username: &'a str,
    challenge_id: &'a str,
    server_nonce_b64: &'a str,
    client_nonce_b64: &'a str,
    remember_me: bool,
}

#[derive(Debug, Clone)]
struct PendingLoginChallenge {
    username_normalized: String,
    server_nonce_b64: String,
    issued_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct AuthManager {
    path: PathBuf,
}

impl AuthManager {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_file(&self) -> Result<(), String> {
        if self.path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let raw = serde_json::to_string_pretty(&UsersFile::default()).map_err(|e| e.to_string())?;
        fs::write(&self.path, raw).map_err(|e| e.to_string())
    }

    /// Serialize every in-process account edit and atomically replace the file.
    pub fn update_users<T>(
        &self,
        update: impl FnOnce(&mut UsersFile) -> Result<T, String>,
    ) -> Result<T, String> {
        static WRITE: Mutex<()> = Mutex::new(());
        let _guard = WRITE
            .lock()
            .map_err(|_| "Account update lock unavailable")?;
        let mut config = self.load_users_file()?;
        let result = update(&mut config)?;
        let parent = self.path.parent().ok_or("Missing accounts directory")?;
        let temporary = parent.join(format!(
            ".users-{}.upload",
            URL_SAFE_NO_PAD.encode(generate_random_bytes(12))
        ));
        let save = (|| -> Result<(), String> {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
            use std::io::Write;
            file.write_all(&serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            std::fs::rename(&temporary, &self.path).map_err(|e| e.to_string())?;
            std::fs::File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|e| e.to_string())
        })();
        if save.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        save?;
        Ok(result)
    }

    pub fn load_users_file(&self) -> Result<UsersFile, String> {
        self.ensure_file()?;
        let raw = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut config = serde_json::from_str::<UsersFile>(&raw).map_err(|e| e.to_string())?;
        config.anonymous = config.anonymous.normalized();
        config.anonymous_command_access = config.anonymous_command_access.normalized();
        config.anonymous_calibration_access = config.anonymous_calibration_access.normalized();
        for user in &mut config.users {
            user.permissions = user.permissions.normalized();
            user.command_access = user.command_access.clone().normalized();
            user.calibration_access = user.calibration_access.normalized();
        }
        Ok(config)
    }

    pub async fn create_login_challenge(
        &self,
        req: LoginChallengeRequest,
    ) -> Result<LoginChallengeResponse, AuthFailure> {
        let requested_username = req.username.trim();
        if requested_username.is_empty() {
            return Err(AuthFailure::Unauthorized(
                "invalid username or password".to_string(),
            ));
        }

        let config = self
            .load_users_file()
            .map_err(|e| AuthFailure::Internal(format!("failed to load users.json: {e}")))?;
        let user = config
            .users
            .iter()
            .find(|user| user.username.eq_ignore_ascii_case(requested_username));

        let salt_b64 = user
            .map(|user| user.password.salt_b64.clone())
            .unwrap_or_else(|| B64.encode(generate_random_bytes(16)));
        let iterations = user
            .map(|user| user.password.iterations.max(1))
            .unwrap_or(DEFAULT_PBKDF2_ITERATIONS);
        let algorithm = user
            .map(|user| user.password.algorithm.clone())
            .unwrap_or_else(default_password_algorithm);

        cleanup_expired_login_challenges();
        let challenge_id = URL_SAFE_NO_PAD.encode(generate_random_bytes(LOGIN_CHALLENGE_BYTES));
        let server_nonce_b64 = URL_SAFE_NO_PAD.encode(generate_random_bytes(LOGIN_CHALLENGE_BYTES));
        let pending = PendingLoginChallenge {
            username_normalized: normalize_username(requested_username),
            server_nonce_b64: server_nonce_b64.clone(),
            issued_at_ms: now_ms(),
        };
        if let Ok(mut challenges) = PENDING_LOGIN_CHALLENGES.lock() {
            challenges.insert(challenge_id.clone(), pending);
        }

        Ok(LoginChallengeResponse {
            challenge_id,
            algorithm,
            iterations,
            salt_b64,
            server_nonce_b64,
        })
    }

    pub async fn login(
        &self,
        db: &SqlitePool,
        req: LoginRequest,
    ) -> Result<LoginResponse, AuthFailure> {
        let requested_username = req.username.trim();
        let username_normalized = normalize_username(requested_username);
        if requested_username.is_empty() || req.challenge_id.trim().is_empty() {
            return Err(AuthFailure::Unauthorized(
                "invalid username or password".to_string(),
            ));
        }

        let challenge = consume_login_challenge(req.challenge_id.trim(), &username_normalized)?;
        let config = self
            .load_users_file()
            .map_err(|e| AuthFailure::Internal(format!("failed to load users.json: {e}")))?;
        let user = config
            .users
            .iter()
            .find(|user| user.username.eq_ignore_ascii_case(requested_username))
            .ok_or_else(|| AuthFailure::Unauthorized("invalid username or password".to_string()))?;

        if user.disabled {
            return Err(AuthFailure::Forbidden("user is disabled".to_string()));
        }
        verify_login_proof(
            &user.password,
            &username_normalized,
            req.challenge_id.trim(),
            &challenge.server_nonce_b64,
            req.client_nonce_b64.trim(),
            req.remember_me,
            req.proof_b64.trim(),
        )
        .map_err(|_| AuthFailure::Unauthorized("invalid username or password".to_string()))?;

        create_session_for_user(db, &config, user, req.remember_me).await
    }

    pub async fn logout(&self, db: &SqlitePool, token: &str) -> Result<(), AuthFailure> {
        let token = token.trim();
        if token.starts_with("paired1.") {
            let pairing = crate::station_pairing::Pairing::load()
                .ok()
                .flatten()
                .ok_or_else(|| AuthFailure::Unauthorized("unpaired session".into()))?;
            let claims = pairing
                .verify(token, now_ms())
                .map_err(|_| AuthFailure::Unauthorized("invalid paired session".into()))?;
            // A destination may validate a portable session without a local row.
            // Keep a tombstone until expiry so logout cannot resurrect it.
            sqlx::query("INSERT INTO auth_sessions (token,username,session_type,can_view_data,can_send_commands,allowed_commands_json,created_at_ms,expires_at_ms) VALUES (?,?,'revoked',0,0,'[]',?,?) ON CONFLICT(token) DO UPDATE SET session_type='revoked',can_view_data=0,can_send_commands=0")
                .bind(token).bind(claims.username).bind(now_ms()).bind(claims.expires_at_ms)
                .execute(db).await.map_err(|_| AuthFailure::Internal("failed to revoke paired session".into()))?;
            return Ok(());
        }
        sqlx::query("DELETE FROM auth_sessions WHERE token = ?")
            .bind(token)
            .execute(db)
            .await
            .map_err(|e| AuthFailure::Internal(format!("failed to remove session: {e}")))?;
        Ok(())
    }

    fn authorize_paired(
        &self,
        token: &str,
        row: Option<&sqlx::sqlite::SqliteRow>,
        required: Permission,
    ) -> Result<AuthPrincipal, AuthFailure> {
        let unauthorized = || AuthFailure::Unauthorized("invalid or revoked paired session".into());
        let pairing = crate::station_pairing::Pairing::load()
            .ok()
            .flatten()
            .ok_or_else(unauthorized)?;
        let claims = pairing
            .verify(token, now_ms())
            .map_err(|_| unauthorized())?;
        if row.is_some_and(|r| r.get::<String, _>("session_type") == "revoked")
            || claims.issuer == pairing.public.station_id && row.is_none()
        {
            return Err(unauthorized());
        }
        let config = self
            .load_users_file()
            .map_err(|_| AuthFailure::Internal("could not load accounts".into()))?;
        paired_principal(
            &config,
            &claims,
            &pairing.public.station_id,
            required,
            now_ms(),
        )
    }

    /// Resolve the primary's CURRENT local grant for one authenticated paired
    /// proxy assertion. The caller must already verify the narrowly signed
    /// actor/operation envelope; this method creates no login or session.
    pub(crate) fn paired_proxy_actor(
        &self,
        username: &str,
        required: Permission,
    ) -> Result<AuthPrincipal, AuthFailure> {
        let config = self
            .load_users_file()
            .map_err(|_| AuthFailure::Internal("could not load accounts".into()))?;
        let user = config
            .users
            .iter()
            .find(|user| !user.disabled && user.username.eq_ignore_ascii_case(username))
            .ok_or_else(|| AuthFailure::Forbidden("paired actor is unavailable".into()))?;
        let permissions = user.permissions.for_roles(&user.roles);
        if !permissions.allows(required) {
            return Err(AuthFailure::Forbidden(
                "paired actor lacks local permission".into(),
            ));
        }
        let (command_access, calibration_access) = configured_user_access(&config, &user.username);
        Ok(AuthPrincipal {
            roles: user.roles.clone(),
            username: Some(user.username.clone()),
            permissions,
            expires_at_ms: None,
            anonymous: false,
            session_type: Some("paired_command_assertion".into()),
            command_access,
            calibration_access,
        }
        .with_role_authority())
    }

    pub async fn authorize_token(
        &self,
        db: &SqlitePool,
        token: Option<&str>,
        required: Permission,
    ) -> Result<AuthPrincipal, AuthFailure> {
        self.cleanup_expired_sessions(db).await?;

        if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
            let token = token.trim();
            let row = sqlx::query(
                r#"
                SELECT username, session_type, can_view_data, can_send_commands, expires_at_ms
                     , allowed_commands_json
                FROM auth_sessions
                WHERE token = ?
                LIMIT 1
                "#,
            )
            .bind(token.trim())
            .fetch_optional(db)
            .await
            .map_err(|e| AuthFailure::Internal(format!("failed to query session: {e}")))?;

            if token.starts_with("paired1.") {
                return self.authorize_paired(token, row.as_ref(), required);
            }
            let Some(row) = row else {
                return Err(AuthFailure::Unauthorized(
                    "invalid session token".to_string(),
                ));
            };

            let expires_at_ms = row.get::<i64, _>("expires_at_ms");
            if expires_at_ms <= now_ms() {
                let _ = sqlx::query("DELETE FROM auth_sessions WHERE token = ?")
                    .bind(token.trim())
                    .execute(db)
                    .await;
                return Err(AuthFailure::Unauthorized(
                    "session token expired".to_string(),
                ));
            }

            let mut permissions = Permissions {
                view_data: row.get::<i64, _>("can_view_data") != 0,
                send_commands: row.get::<i64, _>("can_send_commands") != 0,
                set_system_time: false,
                voice_transmit: false,
                camera_publish: false,
                sign_checklists: false,
            }
            .normalized();

            let config = self
                .load_users_file()
                .map_err(|e| AuthFailure::Internal(format!("failed to load users.json: {e}")))?;
            let username = row.get::<String, _>("username");
            let user = config
                .users
                .iter()
                .find(|u| u.username.eq_ignore_ascii_case(&username) && !u.disabled)
                .ok_or_else(|| AuthFailure::Unauthorized("Account disabled or removed".into()))?;
            // Application admins inherit full application access through
            // `with_role_authority`. Removing that role or disabling the
            // account is checked against the current file.
            let current = user.permissions.for_roles(&user.roles);
            permissions.view_data &= current.view_data;
            permissions.send_commands &= current.send_commands;
            permissions.set_system_time = current.set_system_time;
            permissions.voice_transmit = current.voice_transmit && current.view_data;
            permissions.camera_publish = current.camera_publish && current.view_data;
            permissions.sign_checklists = current.sign_checklists && current.view_data;
            let (command_access, calibration_access) = configured_user_access(&config, &username);
            let principal = AuthPrincipal {
                roles: user.roles.clone(),
                username: Some(username),
                permissions,
                expires_at_ms: Some(expires_at_ms),
                anonymous: false,
                session_type: Some(row.get::<String, _>("session_type")),
                command_access,
                calibration_access,
            }
            .with_role_authority();
            if !principal.permissions.allows(required) {
                return Err(AuthFailure::Forbidden(
                    "session does not have the required permission".into(),
                ));
            }
            return Ok(principal);
        }

        let config = self
            .load_users_file()
            .map_err(|e| AuthFailure::Internal(format!("failed to load users.json: {e}")))?;
        let mut permissions = config.anonymous.normalized();
        permissions.set_system_time = false;
        permissions.voice_transmit = false;
        permissions.camera_publish = false;
        permissions.sign_checklists = false;
        if !permissions.allows(required) {
            return Err(AuthFailure::Unauthorized(
                "authentication required".to_string(),
            ));
        }

        Ok(AuthPrincipal {
            roles: Vec::new(),
            username: None,
            permissions,
            expires_at_ms: None,
            anonymous: true,
            session_type: None,
            command_access: config.anonymous_command_access,
            calibration_access: config.anonymous_calibration_access,
        })
    }

    pub async fn session_status(
        &self,
        db: &SqlitePool,
        token: Option<&str>,
    ) -> Result<SessionStatus, AuthFailure> {
        // A token that was sent but no longer resolves must be reported as 401,
        // exactly as every data endpoint does, so clients drop it and sign in
        // again instead of looking "anonymous" here while all other requests fail.
        let presented = token.is_some_and(|token| !token.trim().is_empty());
        match self.authorize_token(db, token, Permission::ViewData).await {
            Ok(principal) => Ok(principal.session_status()),
            Err(AuthFailure::Unauthorized(message)) if presented => {
                Err(AuthFailure::Unauthorized(message))
            }
            Err(AuthFailure::Unauthorized(_)) => {
                let config = self.load_users_file().map_err(|e| {
                    AuthFailure::Internal(format!("failed to load users.json: {e}"))
                })?;
                Ok(AuthPrincipal {
                    roles: Vec::new(),
                    username: None,
                    permissions: Permissions {
                        voice_transmit: false,
                        camera_publish: false,
                        sign_checklists: false,
                        ..config.anonymous.normalized()
                    },
                    expires_at_ms: None,
                    anonymous: true,
                    session_type: None,
                    command_access: config.anonymous_command_access,
                    calibration_access: config.anonymous_calibration_access,
                }
                .session_status())
            }
            Err(err) => Err(err),
        }
    }

    pub async fn cleanup_expired_sessions(&self, db: &SqlitePool) -> Result<(), AuthFailure> {
        sqlx::query("DELETE FROM auth_sessions WHERE expires_at_ms <= ?")
            .bind(now_ms())
            .execute(db)
            .await
            .map_err(|e| AuthFailure::Internal(format!("failed to cleanup sessions: {e}")))?;
        Ok(())
    }
}

/// Resolve a verified paired (portable) session against the CURRENT local
/// accounts. Non-admin grants are the intersection of the signed claims and
/// the local account. An administrator holds every permission here; command
/// admission through the primary's proxy is still enforced separately.
fn paired_principal(
    config: &UsersFile,
    claims: &crate::station_pairing::Claims,
    own_station_id: &str,
    required: Permission,
    now: i64,
) -> Result<AuthPrincipal, AuthFailure> {
    let unauthorized = || AuthFailure::Unauthorized("invalid or revoked paired session".into());
    let user = config
        .users
        .iter()
        .find(|u| u.username.eq_ignore_ascii_case(&claims.username) && !u.disabled)
        .ok_or_else(unauthorized)?;
    let local = user.permissions.for_roles(&user.roles);
    let mut roles: Vec<_> = user
        .roles
        .iter()
        .copied()
        .filter(|r| claims.roles.contains(r))
        .collect();
    // A token this station issued is backed by a local session row, so it
    // follows the CURRENT local account exactly like an unpaired session:
    // granting administrator takes effect without signing in again. A
    // peer-issued token still needs both stations to agree on the role.
    if claims.issuer == own_station_id
        && user.roles.contains(&StreamRole::Admin)
        && !roles.contains(&StreamRole::Admin)
    {
        roles.push(StreamRole::Admin);
    }
    let permissions = claims.permissions.intersect(local);
    let (local_commands, local_calibration) = configured_user_access(config, &user.username);
    let mut allowed_commands = intersect_commands(
        &claims.command_access.allowed_commands,
        &local_commands.allowed_commands,
    );
    // An empty list means unrestricted, so an empty intersection must use a
    // non-command sentinel rather than accidentally expanding permissions.
    if !claims.command_access.allowed_commands.is_empty()
        && !local_commands.allowed_commands.is_empty()
        && allowed_commands.is_empty()
    {
        allowed_commands.push("__no_paired_commands__".into());
    }
    let local_expiry = claims.issued_at_ms.saturating_add(
        (config.session_ttl_seconds.min(i64::MAX as u64 / 1000) as i64).saturating_mul(1000),
    );
    let expires = claims.expires_at_ms.min(local_expiry);
    if expires <= now {
        return Err(unauthorized());
    }
    let principal = AuthPrincipal {
        roles,
        username: Some(user.username.clone()),
        permissions,
        expires_at_ms: Some(expires),
        anonymous: false,
        session_type: Some(
            if claims.remembered {
                "remembered"
            } else {
                "session"
            }
            .into(),
        ),
        command_access: CommandAccess { allowed_commands },
        calibration_access: CalibrationAccess {
            view: claims.calibration_access.view && local_calibration.view,
            edit: claims.calibration_access.edit && local_calibration.edit,
        },
    }
    .with_role_authority();
    if !principal.permissions.allows(required) {
        return Err(AuthFailure::Forbidden(
            "paired account lacks permission".into(),
        ));
    }
    Ok(principal)
}

async fn create_session_for_user(
    db: &SqlitePool,
    config: &UsersFile,
    user: &UserRecord,
    remember_me: bool,
) -> Result<LoginResponse, AuthFailure> {
    let now_ms = now_ms();
    let expires_at_ms =
        now_ms + (config.session_ttl_seconds.min(i64::MAX as u64 / 1000) as i64 * 1000);
    let permissions = user.permissions.for_roles(&user.roles);
    let (command_access, calibration_access) = configured_user_access(config, &user.username);
    let session_type = if remember_me { "remembered" } else { "session" };
    // A broken pairing falls back to an ordinary local session; login must not fail.
    let token = match crate::station_pairing::Pairing::load_or_unpaired("login") {
        Some(pairing) => pairing
            .token(&crate::station_pairing::Claims {
                installation_id: pairing.public.installation_id.clone(),
                issuer: pairing.public.station_id.clone(),
                username: user.username.clone(),
                issued_at_ms: now_ms,
                expires_at_ms,
                nonce: generate_token(),
                roles: user.roles.clone(),
                permissions,
                command_access: command_access.clone(),
                calibration_access,
                remembered: remember_me,
            })
            .map_err(|_| AuthFailure::Internal("could not create paired session".into()))?,
        None => generate_token(),
    };
    let allowed_commands_json = serde_json::to_string(&command_access.allowed_commands)
        .map_err(|e| AuthFailure::Internal(format!("failed to serialize command access: {e}")))?;

    sqlx::query(
        r#"
        INSERT INTO auth_sessions (
            token, username, session_type, can_view_data, can_send_commands, allowed_commands_json, created_at_ms, expires_at_ms
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&token)
    .bind(&user.username)
    .bind(session_type)
    .bind(permissions.view_data as i64)
    .bind(permissions.send_commands as i64)
    .bind(allowed_commands_json)
    .bind(now_ms)
    .bind(expires_at_ms)
    .execute(db)
    .await
    .map_err(|e| AuthFailure::Internal(format!("failed to create session: {e}")))?;

    Ok(LoginResponse {
        token,
        session: AuthPrincipal {
            roles: user.roles.clone(),
            username: Some(user.username.clone()),
            permissions,
            expires_at_ms: Some(expires_at_ms),
            anonymous: false,
            session_type: Some(session_type.to_string()),
            command_access,
            calibration_access,
        }
        .with_role_authority()
        .session_status(),
    })
}

fn normalize_username(username: &str) -> String {
    username.trim().to_ascii_lowercase()
}

fn cleanup_expired_login_challenges() {
    let cutoff = now_ms() - LOGIN_CHALLENGE_TTL_MS;
    if let Ok(mut challenges) = PENDING_LOGIN_CHALLENGES.lock() {
        challenges.retain(|_, challenge| challenge.issued_at_ms >= cutoff);
    }
}

fn consume_login_challenge(
    challenge_id: &str,
    expected_username_normalized: &str,
) -> Result<PendingLoginChallenge, AuthFailure> {
    cleanup_expired_login_challenges();
    let challenge = PENDING_LOGIN_CHALLENGES
        .lock()
        .map_err(|_| AuthFailure::Internal("failed to access login challenges".to_string()))?
        .remove(challenge_id)
        .ok_or_else(|| AuthFailure::Unauthorized("invalid username or password".to_string()))?;

    if challenge.username_normalized != expected_username_normalized {
        return Err(AuthFailure::Unauthorized(
            "invalid username or password".to_string(),
        ));
    }
    if challenge.issued_at_ms + LOGIN_CHALLENGE_TTL_MS < now_ms() {
        return Err(AuthFailure::Unauthorized(
            "invalid username or password".to_string(),
        ));
    }
    Ok(challenge)
}

fn auth_proof_message(
    username_normalized: &str,
    challenge_id: &str,
    server_nonce_b64: &str,
    client_nonce_b64: &str,
    remember_me: bool,
) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&AuthProofPayload {
        username: username_normalized,
        challenge_id,
        server_nonce_b64,
        client_nonce_b64,
        remember_me,
    })
    .map_err(|e| e.to_string())
}

fn verify_login_proof(
    record: &PasswordHashRecord,
    username_normalized: &str,
    challenge_id: &str,
    server_nonce_b64: &str,
    client_nonce_b64: &str,
    remember_me: bool,
    proof_b64: &str,
) -> Result<(), String> {
    if record.algorithm != default_password_algorithm() {
        return Err("unsupported password hash algorithm".to_string());
    }
    let verifier = B64
        .decode(record.hash_b64.as_bytes())
        .map_err(|e| e.to_string())?;
    let message = auth_proof_message(
        username_normalized,
        challenge_id,
        server_nonce_b64,
        client_nonce_b64,
        remember_me,
    )?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, &verifier);
    let provided = URL_SAFE_NO_PAD
        .decode(proof_b64.as_bytes())
        .map_err(|e| e.to_string())?;
    hmac::verify(&key, &message, &provided).map_err(|_| "invalid login proof".to_string())
}

#[allow(dead_code)]
pub fn create_password_hash(password: &str) -> Result<PasswordHashRecord, String> {
    let salt = generate_random_bytes(16);
    let hash = derive_pbkdf2(password.as_bytes(), &salt, DEFAULT_PBKDF2_ITERATIONS)?;
    Ok(PasswordHashRecord {
        algorithm: default_password_algorithm(),
        iterations: DEFAULT_PBKDF2_ITERATIONS,
        salt_b64: B64.encode(salt),
        hash_b64: B64.encode(hash),
    })
}

#[allow(dead_code)]
pub fn verify_password(record: &PasswordHashRecord, password: &str) -> Result<(), String> {
    if record.algorithm != default_password_algorithm() {
        return Err("unsupported password hash algorithm".to_string());
    }
    let salt = B64
        .decode(record.salt_b64.as_bytes())
        .map_err(|e| e.to_string())?;
    let expected = B64
        .decode(record.hash_b64.as_bytes())
        .map_err(|e| e.to_string())?;
    let iterations = NonZeroU32::new(record.iterations.max(1))
        .ok_or_else(|| "invalid PBKDF2 iteration count".to_string())?;
    pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        &salt,
        password.as_bytes(),
        &expected,
    )
    .map_err(|_| "password verification failed".to_string())
}

#[allow(dead_code)]
fn derive_pbkdf2(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Result<[u8; PBKDF2_OUTPUT_LEN], String> {
    let mut out = [0u8; PBKDF2_OUTPUT_LEN];
    let iterations = NonZeroU32::new(iterations.max(1))
        .ok_or_else(|| "invalid PBKDF2 iteration count".to_string())?;
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        password,
        &mut out,
    );
    Ok(out)
}

fn intersect_commands(source: &[String], destination: &[String]) -> Vec<String> {
    if source.is_empty() {
        destination.to_vec()
    } else if destination.is_empty() {
        source.to_vec()
    } else {
        source
            .iter()
            .filter(|c| destination.contains(c))
            .cloned()
            .collect()
    }
}

fn generate_token() -> String {
    URL_SAFE_NO_PAD.encode(generate_random_bytes(32))
}

fn generate_random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    let rng = ring::rand::SystemRandom::new();
    ring::rand::SecureRandom::fill(&rng, &mut bytes).expect("secure random failure");
    bytes
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn default_users_file_version() -> u32 {
    USERS_FILE_VERSION
}

fn default_session_ttl_seconds() -> u64 {
    DEFAULT_SESSION_TTL_SECONDS
}

fn default_password_algorithm() -> String {
    "pbkdf2_sha256".to_string()
}
