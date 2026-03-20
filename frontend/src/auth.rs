use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

static CURRENT_SESSION: Lazy<Mutex<Option<StoredAuthSession>>> = Lazy::new(|| Mutex::new(None));
static CURRENT_STATUS: Lazy<Mutex<SessionStatus>> =
    Lazy::new(|| Mutex::new(SessionStatus::default()));
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const AUTH_STORAGE_KEY: &str = "auth_session_v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Permissions {
    #[serde(default)]
    pub view_data: bool,
    #[serde(default)]
    pub send_commands: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionStatus {
    #[serde(default)]
    pub authenticated: bool,
    pub username: Option<String>,
    #[serde(default)]
    pub permissions: Permissions,
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub anonymous: bool,
    pub session_type: Option<String>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub session: SessionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAuthSession {
    pub token: String,
    pub session: SessionStatus,
    #[serde(default)]
    pub remember_me: bool,
}

#[derive(Debug, Clone, Serialize)]
struct LoginRequest<'a> {
    username: &'a str,
    password: &'a str,
    remember_me: bool,
}

pub fn init_from_storage() {
    let restored = read_storage_session();
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = restored.clone();
    }
    if let Ok(mut status) = CURRENT_STATUS.lock() {
        *status = restored.map(|session| session.session).unwrap_or_default();
    }
}

pub fn current_session() -> Option<StoredAuthSession> {
    CURRENT_SESSION.lock().ok().and_then(|slot| slot.clone())
}

pub fn current_token() -> Option<String> {
    current_session().map(|session| session.token)
}

pub fn current_status() -> SessionStatus {
    CURRENT_STATUS
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_default()
}

pub fn can_send_command(cmd: &str) -> bool {
    let status = current_status();
    if !status.permissions.send_commands {
        return false;
    }
    status.allowed_commands.is_empty() || status.allowed_commands.iter().any(|item| item == cmd)
}

pub fn set_current_session(session: StoredAuthSession) {
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = Some(session.clone());
    }
    if let Ok(mut status) = CURRENT_STATUS.lock() {
        *status = session.session.clone();
    }
    if session.remember_me {
        write_storage_session(Some(&session));
    } else {
        write_storage_session(None);
    }
}

pub fn set_logged_out_status(status: SessionStatus) {
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = None;
    }
    if let Ok(mut current) = CURRENT_STATUS.lock() {
        *current = status;
    }
    write_storage_session(None);
}

pub fn clear_current_session() {
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = None;
    }
    if let Ok(mut status) = CURRENT_STATUS.lock() {
        *status = SessionStatus::default();
    }
    write_storage_session(None);
}

pub async fn fetch_session_status(
    base: &str,
    skip_tls_verify: bool,
) -> Result<SessionStatus, String> {
    let url = build_url(base, "/api/auth/session")?;
    let text = auth_request_get(&url, skip_tls_verify, true, true).await?;
    let status = serde_json::from_str::<SessionStatus>(&text)
        .map_err(|e| format!("invalid auth JSON: {e}"))?;
    if let Ok(mut slot) = CURRENT_STATUS.lock() {
        *slot = status.clone();
    }
    if let Ok(mut session_slot) = CURRENT_SESSION.lock()
        && let Some(session) = session_slot.as_mut()
    {
        session.session = status.clone();
        if session.remember_me {
            write_storage_session(Some(session));
        }
    }
    Ok(status)
}

pub async fn fetch_logged_out_session_status(
    base: &str,
    skip_tls_verify: bool,
) -> Result<SessionStatus, String> {
    let url = build_url(base, "/api/auth/session")?;
    let text = auth_request_get(&url, skip_tls_verify, false, false).await?;
    serde_json::from_str::<SessionStatus>(&text).map_err(|e| format!("invalid auth JSON: {e}"))
}

pub async fn login(
    base: &str,
    skip_tls_verify: bool,
    username: &str,
    password: &str,
    remember_me: bool,
) -> Result<StoredAuthSession, String> {
    let url = build_url(base, "/api/auth/login")?;
    let body = serde_json::to_string(&LoginRequest {
        username,
        password,
        remember_me,
    })
    .map_err(|e| e.to_string())?;
    let text = auth_request_post_json(&url, &body, skip_tls_verify).await?;
    let response = serde_json::from_str::<LoginResponse>(&text)
        .map_err(|e| format!("invalid auth JSON: {e}"))?;
    let stored = StoredAuthSession {
        token: response.token,
        session: response.session,
        remember_me,
    };
    set_current_session(stored.clone());
    Ok(stored)
}

pub async fn logout(base: &str, skip_tls_verify: bool) -> Result<(), String> {
    let url = build_url(base, "/api/auth/logout")?;
    let _ = auth_request_post_empty(&url, skip_tls_verify).await;
    clear_current_session();
    Ok(())
}

#[cfg(target_arch = "wasm32")]
async fn auth_request_get(
    url: &str,
    _skip_tls_verify: bool,
    include_token: bool,
    clear_on_unauthorized: bool,
) -> Result<String, String> {
    use gloo_net::http::Request;

    let mut request = Request::get(url);
    if include_token && let Some(token) = current_token() {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if clear_on_unauthorized && status == 401 {
        clear_current_session();
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", text.trim()));
    }
    Ok(text)
}

#[cfg(not(target_arch = "wasm32"))]
async fn auth_request_get(
    url: &str,
    skip_tls_verify: bool,
    include_token: bool,
    clear_on_unauthorized: bool,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client.get(url.to_string());
    if include_token && let Some(token) = current_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if clear_on_unauthorized && status == reqwest::StatusCode::UNAUTHORIZED {
        clear_current_session();
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", text.trim()));
    }
    Ok(text)
}

#[cfg(target_arch = "wasm32")]
async fn auth_request_post_json(
    url: &str,
    body: &str,
    _skip_tls_verify: bool,
) -> Result<String, String> {
    use gloo_net::http::Request;

    let mut request = Request::post(url).header("Content-Type", "application/json");
    if let Some(token) = current_token() {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let response = request
        .body(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if status == 401 {
        clear_current_session();
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", text.trim()));
    }
    Ok(text)
}

#[cfg(not(target_arch = "wasm32"))]
async fn auth_request_post_json(
    url: &str,
    body: &str,
    skip_tls_verify: bool,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client
        .post(url.to_string())
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(token) = current_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        clear_current_session();
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", text.trim()));
    }
    Ok(text)
}

#[cfg(target_arch = "wasm32")]
async fn auth_request_post_empty(url: &str, _skip_tls_verify: bool) -> Result<(), String> {
    use gloo_net::http::Request;

    let mut request = Request::post(url);
    if let Some(token) = current_token() {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status == 401 {
        clear_current_session();
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", text.trim()));
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
async fn auth_request_post_empty(url: &str, skip_tls_verify: bool) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client.post(url.to_string());
    if let Some(token) = current_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        clear_current_session();
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", text.trim()));
    }
    Ok(())
}

fn build_url(base: &str, path: &str) -> Result<String, String> {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let base = base.trim().trim_end_matches('/').to_string();
    if !base.is_empty() {
        return Ok(format!("{base}{path}"));
    }

    #[cfg(target_arch = "wasm32")]
    {
        let window = web_sys::window().ok_or("no window".to_string())?;
        let origin = window
            .location()
            .origin()
            .map_err(|_| "failed to read window origin".to_string())?;
        Ok(format!("{origin}{path}"))
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        Ok(format!("http://localhost:3000{path}"))
    }
}

#[cfg(target_arch = "wasm32")]
fn read_storage_session() -> Option<StoredAuthSession> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    storage
        .get_item(AUTH_STORAGE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<StoredAuthSession>(&raw).ok())
}

#[cfg(not(target_arch = "wasm32"))]
fn read_storage_session() -> Option<StoredAuthSession> {
    let path = auth_storage_path();
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<StoredAuthSession>(&raw).ok())
}

#[cfg(target_arch = "wasm32")]
fn write_storage_session(session: Option<&StoredAuthSession>) {
    if let Some(window) = web_sys::window()
        && let Ok(Some(storage)) = window.local_storage()
    {
        match session {
            Some(session) => {
                if let Ok(raw) = serde_json::to_string(session) {
                    let _ = storage.set_item(AUTH_STORAGE_KEY, &raw);
                }
            }
            None => {
                let _ = storage.remove_item(AUTH_STORAGE_KEY);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn write_storage_session(session: Option<&StoredAuthSession>) {
    let path = auth_storage_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match session {
        Some(session) => {
            if let Ok(raw) = serde_json::to_string_pretty(session) {
                let _ = std::fs::write(path, raw);
            }
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn auth_storage_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
        .join("gs26")
        .join("auth_session.json")
}
