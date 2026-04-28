use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD};
use ring::pbkdf2;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const USERS_FILE_VERSION: u32 = 1;
const DEFAULT_SESSION_TTL_SECONDS: u64 = 60 * 60 * 24 * 14;
#[allow(dead_code)]
const DEFAULT_PBKDF2_ITERATIONS: u32 = 120_000;
#[allow(dead_code)]
const PBKDF2_OUTPUT_LEN: usize = 32;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Permissions {
    #[serde(default)]
    pub view_data: bool,
    #[serde(default)]
    pub send_commands: bool,
}

impl Permissions {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    ViewData,
    SendCommands,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
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
    pub username: Option<String>,
    pub permissions: Permissions,
    pub expires_at_ms: Option<i64>,
    pub anonymous: bool,
    pub session_type: Option<String>,
    pub command_access: CommandAccess,
    pub calibration_access: CalibrationAccess,
}

impl AuthPrincipal {
    pub fn session_status(&self) -> SessionStatus {
        #[cfg(feature = "hitl_mode")]
        let allowed_commands = Vec::new();
        #[cfg(not(feature = "hitl_mode"))]
        let allowed_commands = self.command_access.allowed_commands.clone();
        SessionStatus {
            authenticated: !self.anonymous,
            username: self.username.clone(),
            permissions: self.permissions,
            expires_at_ms: self.expires_at_ms,
            anonymous: self.anonymous,
            session_type: self.session_type.clone(),
            allowed_commands,
            can_view_calibration: self.calibration_access.view,
            can_edit_calibration: self.calibration_access.edit,
        }
    }

    pub fn allows_command_name(&self, cmd: &str) -> bool {
        #[cfg(feature = "hitl_mode")]
        {
            let _ = cmd;
            self.permissions.send_commands
        }
        #[cfg(not(feature = "hitl_mode"))]
        {
            self.permissions.send_commands && self.command_access.allows(cmd)
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
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub remember_me: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub session: SessionStatus,
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

    pub async fn login(
        &self,
        db: &SqlitePool,
        req: LoginRequest,
    ) -> Result<LoginResponse, AuthFailure> {
        let requested_username = req.username.trim();
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
        verify_password(&user.password, &req.password)
            .map_err(|_| AuthFailure::Unauthorized("invalid username or password".to_string()))?;

        let now_ms = now_ms();
        let expires_at_ms =
            now_ms + (config.session_ttl_seconds.min(i64::MAX as u64 / 1000) as i64 * 1000);
        let token = generate_token();
        let permissions = user.permissions.normalized();
        let session_type = if req.remember_me {
            "remembered"
        } else {
            "session"
        };
        let allowed_commands_json = serde_json::to_string(&user.command_access.allowed_commands)
            .map_err(|e| {
                AuthFailure::Internal(format!("failed to serialize command access: {e}"))
            })?;

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
                username: Some(user.username.clone()),
                permissions,
                expires_at_ms: Some(expires_at_ms),
                anonymous: false,
                session_type: Some(session_type.to_string()),
                command_access: user.command_access.clone(),
                calibration_access: user.calibration_access,
            }
            .session_status(),
        })
    }

    pub async fn logout(&self, db: &SqlitePool, token: &str) -> Result<(), AuthFailure> {
        sqlx::query("DELETE FROM auth_sessions WHERE token = ?")
            .bind(token)
            .execute(db)
            .await
            .map_err(|e| AuthFailure::Internal(format!("failed to remove session: {e}")))?;
        Ok(())
    }

    pub async fn authorize_token(
        &self,
        db: &SqlitePool,
        token: Option<&str>,
        required: Permission,
    ) -> Result<AuthPrincipal, AuthFailure> {
        self.cleanup_expired_sessions(db).await?;

        if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
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

            let permissions = Permissions {
                view_data: row.get::<i64, _>("can_view_data") != 0,
                send_commands: row.get::<i64, _>("can_send_commands") != 0,
            }
            .normalized();

            if !permissions.allows(required) {
                return Err(AuthFailure::Forbidden(
                    "session does not have the required permission".to_string(),
                ));
            }

            return Ok(AuthPrincipal {
                username: Some(row.get::<String, _>("username")),
                permissions,
                expires_at_ms: Some(expires_at_ms),
                anonymous: false,
                session_type: Some(row.get::<String, _>("session_type")),
                command_access: CommandAccess {
                    allowed_commands: serde_json::from_str(
                        row.get::<String, _>("allowed_commands_json").as_str(),
                    )
                    .unwrap_or_default(),
                }
                .normalized(),
                calibration_access: {
                    let config = self.load_users_file().map_err(|e| {
                        AuthFailure::Internal(format!("failed to load users.json: {e}"))
                    })?;
                    let username = row.get::<String, _>("username");
                    config
                        .users
                        .iter()
                        .find(|user| user.username.eq_ignore_ascii_case(username.as_str()))
                        .map(|user| user.calibration_access)
                        .unwrap_or_default()
                },
            });
        }

        let config = self
            .load_users_file()
            .map_err(|e| AuthFailure::Internal(format!("failed to load users.json: {e}")))?;
        let permissions = config.anonymous.normalized();
        if !permissions.allows(required) {
            return Err(AuthFailure::Unauthorized(
                "authentication required".to_string(),
            ));
        }

        Ok(AuthPrincipal {
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
        match self.authorize_token(db, token, Permission::ViewData).await {
            Ok(principal) => Ok(principal.session_status()),
            Err(AuthFailure::Unauthorized(_)) => {
                let config = self.load_users_file().map_err(|e| {
                    AuthFailure::Internal(format!("failed to load users.json: {e}"))
                })?;
                Ok(AuthPrincipal {
                    username: None,
                    permissions: config.anonymous.normalized(),
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
