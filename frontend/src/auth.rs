use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[cfg(not(target_arch = "wasm32"))]
const AUTH_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

static CURRENT_SESSION: Lazy<Mutex<Option<StoredAuthSession>>> = Lazy::new(|| Mutex::new(None));
static CURRENT_STATUS: Lazy<Mutex<SessionStatus>> =
    Lazy::new(|| Mutex::new(SessionStatus::default()));
static CURRENT_HOST_SCOPE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const AUTH_STORAGE_KEY: &str = "auth_session_v1";
const AUTH_STORAGE_LIMIT: usize = 20;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAuthSessionEntry {
    host_scope: String,
    session: StoredAuthSession,
    updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoredAuthSessionStore {
    #[serde(default)]
    entries: Vec<StoredAuthSessionEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct LoginRequest<'a> {
    username: &'a str,
    password: &'a str,
    remember_me: bool,
}

pub fn init_from_storage(base: &str) {
    let host_scope = host_scope_for_base(base);
    let restored = read_storage_session(&host_scope);
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = restored.clone();
    }
    if let Ok(mut status) = CURRENT_STATUS.lock() {
        *status = restored.map(|session| session.session).unwrap_or_default();
    }
    if let Ok(mut current_host) = CURRENT_HOST_SCOPE.lock() {
        *current_host = Some(host_scope);
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
    let host_scope = current_host_scope();
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = Some(session.clone());
    }
    if let Ok(mut status) = CURRENT_STATUS.lock() {
        *status = session.session.clone();
    }
    if session.remember_me && !host_scope.is_empty() {
        write_storage_session(&host_scope, Some(&session));
    } else {
        clear_storage_session_for_host(&host_scope);
    }
}

pub fn set_logged_out_status(status: SessionStatus) {
    let host_scope = current_host_scope();
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = None;
    }
    if let Ok(mut current) = CURRENT_STATUS.lock() {
        *current = status;
    }
    clear_storage_session_for_host(&host_scope);
}

pub fn clear_current_session() {
    let host_scope = current_host_scope();
    if let Ok(mut slot) = CURRENT_SESSION.lock() {
        *slot = None;
    }
    if let Ok(mut status) = CURRENT_STATUS.lock() {
        *status = SessionStatus::default();
    }
    clear_storage_session_for_host(&host_scope);
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
            let host_scope = current_host_scope();
            if !host_scope.is_empty() {
                write_storage_session(&host_scope, Some(session));
            }
        }
    }
    Ok(status)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn build_native_http_client(
    skip_tls_verify: bool,
    connect_timeout: std::time::Duration,
    timeout: std::time::Duration,
) -> Result<reqwest::Client, String> {
    let make_builder = || {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(skip_tls_verify)
            .connect_timeout(connect_timeout)
            .timeout(timeout)
            // Keep native HTTPS aligned with the successful WebSocket/upgrade path.
            // Some proxies behave differently for reqwest+rustrls over HTTP/2.
            .http1_only()
    };

    #[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
    if !skip_tls_verify {
        use rustls_platform_verifier::ConfigVerifierExt;
        if let Ok(tls_config) = rustls::ClientConfig::with_platform_verifier() {
            match make_builder()
                .use_preconfigured_tls(std::sync::Arc::new(tls_config))
                .build()
            {
                Ok(client) => return Ok(client),
                Err(err) => {
                    eprintln!(
                        "Platform-verifier TLS client build failed, falling back to default rustls: {err}"
                    );
                }
            }
        }
    }

    make_builder()
        .build()
        .map_err(|e| format_native_auth_error(&format!("{e:?}"), skip_tls_verify))
}

#[cfg(not(target_arch = "wasm32"))]
fn build_native_auth_client(skip_tls_verify: bool) -> Result<reqwest::Client, String> {
    build_native_http_client(skip_tls_verify, AUTH_HTTP_TIMEOUT, AUTH_HTTP_TIMEOUT)
}

#[cfg(not(target_arch = "wasm32"))]
fn format_native_auth_error(raw: &str, skip_tls_verify: bool) -> String {
    let lower = raw.to_ascii_lowercase();
    let tls_like = lower.contains("certificate")
        || lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("unknown issuer")
        || lower.contains("self signed")
        || lower.contains("invalid peer certificate");

    if tls_like {
        if skip_tls_verify {
            format!("SSL/TLS connection failed even with certificate verification disabled.\n{raw}")
        } else {
            format!(
                "SSL/TLS certificate verification failed.\nEnable 'Skip SSL verification' for this backend if you trust the certificate.\n{raw}"
            )
        }
    } else if lower.contains("timed out") {
        format!("Backend session check timed out.\n{raw}")
    } else {
        raw.to_string()
    }
}

fn body_looks_like_html(body: &str) -> bool {
    let trimmed = body.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("<!doctype html")
        || lower.starts_with("<html")
        || lower.contains("<body")
        || lower.contains("<script")
}

fn compact_error_body(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() || body_looks_like_html(trimmed) {
        return None;
    }

    let single_line = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.is_empty() {
        return None;
    }

    const MAX_LEN: usize = 180;
    if single_line.len() > MAX_LEN {
        Some(format!("{}...", &single_line[..MAX_LEN]))
    } else {
        Some(single_line)
    }
}

#[cfg(target_arch = "wasm32")]
fn format_http_error(status: u16, body: &str) -> String {
    let label = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    };
    let headline = if label.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status} {label}")
    };

    if let Some(details) = compact_error_body(body) {
        format!("{headline}\n{details}")
    } else {
        format!(
            "{headline}\nThe backend returned an error page instead of the expected API response."
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn format_http_error(status: reqwest::StatusCode, body: &str) -> String {
    let headline = match status.canonical_reason() {
        Some(reason) => format!("HTTP {} {reason}", status.as_u16()),
        None => format!("HTTP {}", status.as_u16()),
    };

    if let Some(details) = compact_error_body(body) {
        format!("{headline}\n{details}")
    } else {
        format!(
            "{headline}\nThe backend returned an error page instead of the expected API response."
        )
    }
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
    if let Ok(mut current_host) = CURRENT_HOST_SCOPE.lock() {
        *current_host = Some(host_scope_for_base(base));
    }
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
        return Err(format_http_error(status, &text));
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
    let client = build_native_auth_client(skip_tls_verify)?;
    let mut request = client.get(url.to_string());
    if include_token && let Some(token) = current_token() {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format_native_auth_error(&e.to_string(), skip_tls_verify))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if clear_on_unauthorized && status == reqwest::StatusCode::UNAUTHORIZED {
        clear_current_session();
    }
    if !status.is_success() {
        return Err(format_http_error(status, &text));
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
        return Err(format_http_error(status, &text));
    }
    Ok(text)
}

#[cfg(not(target_arch = "wasm32"))]
async fn auth_request_post_json(
    url: &str,
    body: &str,
    skip_tls_verify: bool,
) -> Result<String, String> {
    let client = build_native_auth_client(skip_tls_verify)?;
    let mut request = client
        .post(url.to_string())
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(token) = current_token() {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format_native_auth_error(&e.to_string(), skip_tls_verify))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        clear_current_session();
    }
    if !status.is_success() {
        return Err(format_http_error(status, &text));
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
        return Err(format_http_error(status, &text));
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
async fn auth_request_post_empty(url: &str, skip_tls_verify: bool) -> Result<(), String> {
    let client = build_native_auth_client(skip_tls_verify)?;
    let mut request = client.post(url.to_string());
    if let Some(token) = current_token() {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format_native_auth_error(&e.to_string(), skip_tls_verify))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        clear_current_session();
    }
    if !status.is_success() {
        return Err(format_http_error(status, &text));
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
fn read_storage_store() -> StoredAuthSessionStore {
    let raw = web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(AUTH_STORAGE_KEY).ok().flatten());
    parse_storage_store(raw.as_deref())
}

#[cfg(not(target_arch = "wasm32"))]
fn read_storage_store() -> StoredAuthSessionStore {
    let path = auth_storage_path();
    let raw = std::fs::read_to_string(path).ok();
    parse_storage_store(raw.as_deref())
}

#[cfg(target_arch = "wasm32")]
fn write_storage_store(store: &StoredAuthSessionStore) {
    if let Some(window) = web_sys::window()
        && let Ok(Some(storage)) = window.local_storage()
    {
        if store.entries.is_empty() {
            let _ = storage.remove_item(AUTH_STORAGE_KEY);
        } else if let Ok(raw) = serde_json::to_string(store) {
            let _ = storage.set_item(AUTH_STORAGE_KEY, &raw);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn write_storage_store(store: &StoredAuthSessionStore) {
    let path = auth_storage_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if store.entries.is_empty() {
        let _ = std::fs::remove_file(path);
    } else if let Ok(raw) = serde_json::to_string_pretty(store) {
        let _ = std::fs::write(path, raw);
    }
}

fn read_storage_session(host_scope: &str) -> Option<StoredAuthSession> {
    if host_scope.is_empty() {
        return None;
    }
    let mut store = read_storage_store();
    if let Some(entry) = store
        .entries
        .iter_mut()
        .find(|entry| entry.host_scope == host_scope)
    {
        entry.updated_at_ms = now_ms();
        let session = entry.session.clone();
        prune_store(&mut store);
        write_storage_store(&store);
        return Some(session);
    }
    if let Some(entry) = store
        .entries
        .iter_mut()
        .find(|entry| entry.host_scope.is_empty())
    {
        entry.host_scope = host_scope.to_string();
        entry.updated_at_ms = now_ms();
        let session = entry.session.clone();
        prune_store(&mut store);
        write_storage_store(&store);
        return Some(session);
    }
    None
}

fn write_storage_session(host_scope: &str, session: Option<&StoredAuthSession>) {
    if host_scope.is_empty() {
        return;
    }
    let mut store = read_storage_store();
    store.entries.retain(|entry| entry.host_scope != host_scope);
    if let Some(session) = session {
        store.entries.push(StoredAuthSessionEntry {
            host_scope: host_scope.to_string(),
            session: session.clone(),
            updated_at_ms: now_ms(),
        });
    }
    prune_store(&mut store);
    write_storage_store(&store);
}

fn clear_storage_session_for_host(host_scope: &str) {
    if host_scope.is_empty() {
        return;
    }
    let mut store = read_storage_store();
    let original_len = store.entries.len();
    store.entries.retain(|entry| entry.host_scope != host_scope);
    if store.entries.len() != original_len {
        write_storage_store(&store);
    }
}

fn prune_store(store: &mut StoredAuthSessionStore) {
    store.entries.sort_by_key(|entry| entry.updated_at_ms);
    if store.entries.len() > AUTH_STORAGE_LIMIT {
        let drop_count = store.entries.len() - AUTH_STORAGE_LIMIT;
        store.entries.drain(0..drop_count);
    }
}

fn parse_storage_store(raw: Option<&str>) -> StoredAuthSessionStore {
    let Some(raw) = raw else {
        return StoredAuthSessionStore::default();
    };
    serde_json::from_str::<StoredAuthSessionStore>(raw)
        .or_else(|_| {
            serde_json::from_str::<StoredAuthSession>(raw).map(|session| StoredAuthSessionStore {
                entries: vec![StoredAuthSessionEntry {
                    host_scope: String::new(),
                    session,
                    updated_at_ms: now_ms(),
                }],
            })
        })
        .unwrap_or_default()
}

fn current_host_scope() -> String {
    CURRENT_HOST_SCOPE
        .lock()
        .ok()
        .and_then(|scope| scope.clone())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
fn host_scope_for_base(base: &str) -> String {
    let mut scope = base.trim().trim_end_matches('/').to_ascii_lowercase();
    if scope.is_empty() {
        if let Some(window) = web_sys::window()
            && let Ok(origin) = window.location().origin()
        {
            scope = origin.trim().trim_end_matches('/').to_ascii_lowercase();
        }
    }
    scope
}

#[cfg(not(target_arch = "wasm32"))]
fn host_scope_for_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now().max(0.0) as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
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
