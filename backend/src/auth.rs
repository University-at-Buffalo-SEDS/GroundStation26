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
        SessionStatus {
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
        .map(|user| (user.command_access.clone(), user.calibration_access))
        .unwrap_or_default()
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

            let config = self.load_users_file().map_err(|e| {
                AuthFailure::Internal(format!("failed to load users.json: {e}"))
            })?;
            let username = row.get::<String, _>("username");
            let (command_access, calibration_access) = configured_user_access(&config, &username);

            return Ok(AuthPrincipal {
                username: Some(username),
                permissions,
                expires_at_ms: Some(expires_at_ms),
                anonymous: false,
                session_type: Some(row.get::<String, _>("session_type")),
                command_access,
                calibration_access,
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


async fn create_session_for_user(
    db: &SqlitePool,
    config: &UsersFile,
    user: &UserRecord,
    remember_me: bool,
) -> Result<LoginResponse, AuthFailure> {
    let now_ms = now_ms();
    let expires_at_ms =
        now_ms + (config.session_ttl_seconds.min(i64::MAX as u64 / 1000) as i64 * 1000);
    let token = generate_token();
    let permissions = user.permissions.normalized();
    let session_type = if remember_me {
        "remembered"
    } else {
        "session"
    };
    let allowed_commands_json = serde_json::to_string(&user.command_access.allowed_commands)
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
