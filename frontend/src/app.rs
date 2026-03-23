// frontend/src/app.rs
//
const _CONNECTION_TIMEOUT_MS: u64 = 8000;
const _BODY_TRANSFER_TIMEOUT_MS: u64 = 10000;
const _WS_TIMEOUT_MS: u64 = 4500;
#[allow(dead_code)]
const APP_DISPLAY_NAME: &str = "Telemetry Client";

use crate::auth::{self, SessionStatus as AuthSessionStatus};
use dioxus::prelude::*;
use dioxus_router::{use_navigator, Routable, Router};

#[allow(unused_imports)]
use crate::telemetry_dashboard::{self, UrlConfig};

// -------------------------
// Native-only keep-awake shims (mobile)
// -------------------------
#[cfg(not(target_arch = "wasm32"))]
mod keep_awake {
    #[cfg(target_os = "ios")]
    mod ios {
        use std::os::raw::c_int;

        unsafe extern "C" {
            fn gs26_set_idle_timer_disabled(disabled: c_int);
        }

        pub fn set_enabled(enabled: bool) {
            // iOS API is "idle timer disabled", so enabled=true -> disabled=1
            unsafe { gs26_set_idle_timer_disabled(if enabled { 1 } else { 0 }) };
        }
    }

    #[cfg(target_os = "android")]
    mod android {
        pub fn set_enabled(enabled: bool) {
            crate::telemetry_dashboard::gps_android::set_keep_screen_on(enabled);
        }
    }

    pub fn set_enabled(enabled: bool) {
        #[cfg(target_os = "ios")]
        ios::set_enabled(enabled);

        #[cfg(target_os = "android")]
        android::set_enabled(enabled);

        // Other native targets: no-op
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        {
            let _ = enabled;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn all_tests_passed(checks: &[RouteCheck], ws_probe: &Option<Result<String, String>>) -> bool {
    let routes_ok = checks.iter().all(|c| c.ok);
    let ws_ok = matches!(ws_probe, Some(Ok(_)));
    routes_ok && ws_ok
}

// --- global css ---
const GLOBAL_CSS: &str = r#"
:root {
    --gs26-app-height: 100dvh;
}

@supports not (height: 100dvh) {
    :root {
        --gs26-app-height: 100vh;
    }
}

html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    min-height: var(--gs26-app-height);
    height: var(--gs26-app-height);
    background: #020617;
    overflow: hidden;
}

:root, html {
    color-scheme: dark;
}

#main {
    width: 100%;
    min-height: var(--gs26-app-height);
    height: var(--gs26-app-height);
    background: #020617;
}

* { box-sizing: border-box; }
"#;

const _CONNECT_SHOWN_KEY: &str = "gs_connect_shown";

#[derive(Clone, Routable, PartialEq)]
pub enum Route {
    #[route("/")]
    Root {},

    #[route("/dashboard")]
    Dashboard {},

    #[route("/login")]
    Login {},

    #[cfg(not(target_arch = "wasm32"))]
    #[route("/connect")]
    Connect {},

    #[cfg(not(target_arch = "wasm32"))]
    #[route("/version")]
    Version {},
}

#[cfg(target_arch = "wasm32")]
fn connect_route() -> Route {
    Route::Root {}
}

#[cfg(not(target_arch = "wasm32"))]
fn connect_route() -> Route {
    Route::Connect {}
}

// -------------------------
// Native-only Objective-C poke shims
// -------------------------
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod objc_poke {
    use std::ffi::CString;
    use std::os::raw::c_char;

    unsafe extern "C" {
        fn gs26_localnet_poke_url(url: *const c_char);
    }

    pub fn poke_url(url: &str) {
        if let Ok(c) = CString::new(url) {
            unsafe { gs26_localnet_poke_url(c.as_ptr()) };
        }
    }
}

#[cfg(all(
    not(target_arch = "wasm32"),
    not(any(target_os = "macos", target_os = "ios"))
))]
mod objc_poke {
    pub fn poke_url(_url: &str) {}
}

// -------------------------
// Persistence helpers
// -------------------------
#[cfg(not(target_arch = "wasm32"))]
mod persist {
    use super::_CONNECT_SHOWN_KEY;
    use std::io;

    fn fallback_storage_dir() -> std::path::PathBuf {
        dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
            .join("gs26")
    }

    #[cfg(target_os = "android")]
    fn android_storage_dir() -> Option<std::path::PathBuf> {
        use jni::objects::{JObject, JString};
        use jni::{jni_sig, jni_str, JavaVM};
        use ndk_context::android_context;

        let ctx = android_context();
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
        vm.attach_current_thread(|env| -> jni::errors::Result<std::path::PathBuf> {
            let context = unsafe { JObject::from_raw(env, ctx.context().cast()) };

            let files_dir = env
                .call_method(
                    &context,
                    jni_str!("getFilesDir"),
                    jni_sig!("()Ljava/io/File;"),
                    &[],
                )?
                .l()?;
            let path_obj = env
                .call_method(
                    &files_dir,
                    jni_str!("getAbsolutePath"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()?;
            let path = env.as_cast::<JString>(&path_obj)?.try_to_string(env)?;

            let _ = context.into_raw();
            Ok(std::path::PathBuf::from(path).join("gs26"))
        })
        .ok()
    }

    fn storage_dir() -> std::path::PathBuf {
        #[cfg(target_os = "android")]
        {
            if let Some(path) = android_storage_dir() {
                return path;
            }
        }

        fallback_storage_dir()
    }

    fn path_for(key: &str) -> std::path::PathBuf {
        storage_dir().join(format!("{key}.txt"))
    }

    fn _read_key(key: &str) -> Option<String> {
        let path = path_for(key);
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn write_key(key: &str, v: &str) -> Result<(), io::Error> {
        let dir = storage_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(path_for(key), v.as_bytes())
    }

    pub fn write_connect_shown(v: bool) -> Result<(), io::Error> {
        write_key(_CONNECT_SHOWN_KEY, if v { "true" } else { "false" })
    }
}

// -------------------------
// URL parsing / normalization
// -------------------------
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
struct ParsedBaseUrl {
    scheme: String, // "http" or "https"
    host: String,
    port: u16,
}

#[cfg(not(target_arch = "wasm32"))]
fn normalize_base_url(mut base: String) -> String {
    // strip fragment
    if let Some(i) = base.find('#') {
        base.truncate(i);
    }

    // strip path but keep scheme://host[:port]
    if let Some(scheme_end) = base.find("://") {
        let rest = &base[scheme_end + 3..];
        if let Some(slash) = rest.find('/') {
            base.truncate(scheme_end + 3 + slash);
        }
    }

    base.trim_end_matches('/').trim().to_string()
}

#[cfg(not(target_arch = "wasm32"))]
fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = if path.starts_with('/') { path } else { "/" };
    format!("{base}{path}")
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_base_url(url: &str) -> Result<ParsedBaseUrl, String> {
    let u = url.trim();
    let (scheme, rest) = if let Some(x) = u.strip_prefix("http://") {
        ("http".to_string(), x)
    } else if let Some(x) = u.strip_prefix("https://") {
        ("https".to_string(), x)
    } else {
        return Err("URL must start with http:// or https://".to_string());
    };

    let hostport = rest.split('/').next().unwrap_or(rest);
    let mut parts = hostport.split(':');
    let host = parts.next().unwrap_or("").trim().to_string();

    if host.is_empty() {
        return Err("Missing host in URL".to_string());
    }

    let port = parts
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or_else(|| if scheme == "https" { 443 } else { 80 });

    Ok(ParsedBaseUrl { scheme, host, port })
}

#[cfg(not(target_arch = "wasm32"))]
fn ws_origin_for_base(parsed: &ParsedBaseUrl) -> String {
    let ws_scheme = if parsed.scheme == "https" {
        "wss"
    } else {
        "ws"
    };
    format!("{ws_scheme}://{}:{}", parsed.host, parsed.port)
}

#[cfg(not(target_arch = "wasm32"))]
fn snip(mut s: String, max: usize) -> String {
    s = s.replace('\r', "");
    if s.len() > max {
        s.truncate(max);
        s.push('…');
    }
    s
}

// -------------------------
// Route probing (actual backend routes)
// -------------------------
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct RouteCheck {
    path: &'static str,
    url: String,
    ok: bool,
    status: Option<u16>,
    body_snip: String,
    note: String,
    err: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
fn status_ok_for_path(path: &str, status: u16) -> (bool, &'static str) {
    match path {
        "/api/recent" | "/api/alerts" | "/api/layout" | "/api/map_config" | "/flightstate"
        | "/api/gps" => (status == 200, "expected 200"),
        "/ws" => match status {
            101 | 400 | 426 => (true, "reachable (ws upgrade required)"),
            _ => (false, "unexpected status for ws route"),
        },
        "/tiles" => match status {
            200 | 403 | 404 => (true, "reachable (tile may not exist)"),
            _ => (false, "unexpected status for tiles"),
        },
        _ => ((200..400).contains(&status), "ok"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn classify_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timeout".into();
    }
    if e.is_connect() {
        return "connect failed (refused/unreachable/DNS/TLS)".into();
    }
    if e.is_request() {
        return "request build/dispatch error".into();
    }
    if e.is_body() {
        return "body read error".into();
    }
    if e.is_decode() {
        return "decode error".into();
    }

    let mut chain = String::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        chain.push_str(&format!(" -> {err}"));
        cur = err.source();
    }
    format!("unknown ({chain})")
}

#[cfg(not(target_arch = "wasm32"))]
fn build_probe_client(skip_tls_verify: bool) -> Result<reqwest::Client, String> {
    // Fast but still reliable:
    // - connect_timeout: how long we wait for TCP/TLS connect
    // - timeout: total request time budget (includes body)
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(_CONNECTION_TIMEOUT_MS))
        .timeout(std::time::Duration::from_millis(_BODY_TRANSFER_TIMEOUT_MS))
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()
        .map_err(|e| format!("build client failed: {e}"))
}

#[cfg(not(target_arch = "wasm32"))]
async fn http_probe_with_client(
    client: &reqwest::Client,
    url: String,
) -> Result<(u16, String), String> {
    const MAX_BODY_BYTES: usize = 4096;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("send failed: {} | kind={}", e, classify_reqwest_error(&e)))?;

    let status = resp.status().as_u16();

    // Cap body download so routes like /tiles don't slow the "test connection" UI.
    // We only need enough to help debugging / confirm "responding".
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;

    let mut slice = bytes.as_ref();
    if slice.len() > MAX_BODY_BYTES {
        slice = &slice[..MAX_BODY_BYTES];
    }

    let body = String::from_utf8_lossy(slice).to_string();
    Ok((status, snip(body, 300)))
}

#[cfg(not(target_arch = "wasm32"))]
async fn test_routes_host_only(base: &str, skip_tls_verify: bool) -> Vec<RouteCheck> {
    use futures_util::future::join_all;

    let probes: &[&str] = &[
        "/api/recent",
        "/api/alerts",
        "/api/layout",
        "/api/map_config",
        "/flightstate",
        "/api/gps",
        "/tiles",
        "/ws",
    ];

    let client = match build_probe_client(skip_tls_verify) {
        Ok(c) => c,
        Err(e) => {
            // If client build failed, mark everything failed quickly.
            return probes
                .iter()
                .map(|path| RouteCheck {
                    path,
                    url: join_url(base, path),
                    ok: false,
                    status: None,
                    body_snip: "".to_string(),
                    note: "client build failed".to_string(),
                    err: Some(e.clone()),
                })
                .collect();
        }
    };

    // Run all probes concurrently.
    let futs = probes.iter().map(|path| {
        let url = join_url(base, path);
        let path = *path;
        let client = &client;

        async move {
            match http_probe_with_client(client, url.clone()).await {
                Ok((status, body_snip)) => {
                    let (ok, note) = status_ok_for_path(path, status);
                    RouteCheck {
                        path,
                        url,
                        ok,
                        status: Some(status),
                        body_snip,
                        note: note.to_string(),
                        err: None,
                    }
                }
                Err(e) => RouteCheck {
                    path,
                    url,
                    ok: false,
                    status: None,
                    body_snip: "".to_string(),
                    note: "request failed".to_string(),
                    err: Some(e),
                },
            }
        }
    });

    join_all(futs).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn ws_connect_probe(parsed: &ParsedBaseUrl, skip_tls_verify: bool) -> Result<String, String> {
    use tokio::time::timeout;

    let ws_origin = ws_origin_for_base(parsed);
    let ws_url = format!("{ws_origin}/ws");

    // Real websocket handshake, but time-bounded so it can't hang forever.
    let res = timeout(std::time::Duration::from_millis(_WS_TIMEOUT_MS), async {
        if skip_tls_verify && ws_url.starts_with("wss://") {
            let tls = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .map_err(|e| format!("tls build failed: {e}"))?;
            tokio_tungstenite::connect_async_tls_with_config(
                ws_url.clone(),
                None,
                false,
                Some(tokio_tungstenite::Connector::NativeTls(tls)),
            )
            .await
            .map_err(|e| format!("{e}"))
        } else {
            tokio_tungstenite::connect_async(ws_url.clone())
                .await
                .map_err(|e| format!("{e}"))
        }
    })
    .await;

    match res {
        Err(_) => Err(format!(
            "❌ WebSocket connect FAILED (timeout)\n    URL: {}",
            ws_url
        )),
        Ok(Ok((_stream, resp))) => Ok(format!(
            "✅ WebSocket handshake OK\n    URL: {}\n    HTTP: {}",
            ws_url,
            resp.status()
        )),
        Ok(Err(e)) => Err(format!(
            "❌ WebSocket connect FAILED\n    URL: {}\n    ERROR: {}",
            ws_url, e
        )),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn format_route_report_host_only(
    original_base: &str,
    parsed: &ParsedBaseUrl,
    checks: &[RouteCheck],
    ws_probe: Option<Result<String, String>>,
) -> String {
    let mut s = String::new();

    // ⭐ All-tests-passed banner
    if all_tests_passed(checks, &ws_probe) {
        s.push_str("🎉 ALL CONNECTION TESTS PASSED\n");
        s.push_str("    Backend is reachable, HTTP routes OK, WebSocket OK.\n");
        s.push_str("--------------------------------------------------------\n\n");
    }

    s.push_str(&format!("Original base: {original_base}\n"));
    s.push_str(&format!(
        "Parsed host: {}  port: {}  scheme: {}\n\n",
        parsed.host, parsed.port, parsed.scheme
    ));

    s.push_str("=== Probing via host (concurrent, short timeouts) ===\n\n");

    for c in checks {
        let icon = if c.ok { "✅" } else { "❌" };
        let status_str = c
            .status
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into());

        s.push_str(&format!(
            "{icon} {:30} status {:>3}  {}\n    URL: {}\n",
            c.path, status_str, c.note, c.url
        ));

        if let Some(e) = &c.err {
            s.push_str(&format!("    ERROR: {e}\n"));
        }
        if !c.body_snip.trim().is_empty() {
            s.push_str(&format!("    BODY: {}\n", c.body_snip.trim()));
        }
        s.push('\n');
    }

    s.push_str("=== WebSocket probe ===\n\n");
    match ws_probe {
        Some(Ok(msg)) => {
            s.push_str(&msg);
            s.push('\n');
        }
        Some(Err(e)) => {
            s.push_str(&e);
            s.push('\n');
        }
        None => {
            s.push_str("(not run)\n");
        }
    }

    s.push_str("\nNotes:\n");
    s.push_str("- Tests are concurrent; worst-case time ~= the slowest single probe, not sum of all probes.\n");
    s.push_str("- If HTTP routes are OK but WebSocket fails: likely TLS/cert/SNI issue for wss, or WS is blocked by server/proxy.\n");
    s.push_str("- If /ws HTTP probe shows 400/426 but WS probe fails: server is reachable, handshake is failing.\n");
    s.push_str("- Local Network prompt on iOS triggers only for LAN access; this test uses hostname only.\n");

    s
}

// -------------------------
// App
// -------------------------
#[component]
pub fn App() -> Element {
    #[cfg(not(target_arch = "wasm32"))]
    {
        keep_awake::set_enabled(true);
    }
    rsx! {
        document::Style { "{GLOBAL_CSS}" }
        Meta { name: "viewport", content: "width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no" }

        document::Link {
            rel: "stylesheet",
            href: asset!("static/vendor/leaflet/leaflet.css"),
        }
        document::Script { src: asset!("static/vendor/leaflet/leaflet.js") }
        document::Script { src: asset!("static/ground_map.js") }

        div {
            style: "min-height: var(--gs26-app-height); width: 100%; background: #020617; color: #e5e7eb;",
            Router::<Route> {}
        }
    }
}

#[component]
pub fn Root() -> Element {
    #[cfg(target_arch = "wasm32")]
    {
        return rsx! { Dashboard {} };
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let nav = use_navigator();

        use_effect(move || {
            if UrlConfig::_stored_base_url().is_some() {
                let _ = nav.replace(Route::Dashboard {});
            } else {
                let _ = nav.replace(connect_route());
            }
        });

        rsx! { div {} }
    }
}

#[component]
fn LoginCard(
    title: String,
    subtitle: String,
    allow_back_to_connect: bool,
    on_success_route: Route,
    #[props(default = false)] overlay_mode: bool,
) -> Element {
    let nav = use_navigator();
    let base = UrlConfig::base_http();
    auth::init_from_storage(&base);
    let effect_base = base.clone();
    let continue_logged_out_base = base.clone();
    let skip_tls = UrlConfig::_skip_tls_verify();
    let mut logged_out_status = use_signal(|| None::<Result<AuthSessionStatus, String>>);
    let mut logged_out_probe_base = use_signal(String::new);
    let continue_logged_out_route = on_success_route.clone();
    let sign_in_route = on_success_route.clone();
    let stored_username = auth::current_session()
        .and_then(|session| session.session.username)
        .unwrap_or_default();
    let mut username = use_signal(|| stored_username);
    let mut password = use_signal(String::new);
    let mut remember_me = use_signal(|| true);
    let mut status = use_signal(String::new);
    let mut busy = use_signal(|| false);

    use_effect({
        let effect_base = effect_base.clone();
        move || {
            auth::init_from_storage(&effect_base);
        }
    });

    use_effect(move || {
        if effect_base.trim().is_empty() {
            return;
        }
        if *logged_out_probe_base.read() == effect_base {
            return;
        }
        logged_out_probe_base.set(effect_base.clone());
        logged_out_status.set(None);
        let base = effect_base.clone();
        spawn(async move {
            logged_out_status.set(Some(
                auth::fetch_logged_out_session_status(&base, skip_tls).await,
            ));
        });
    });

    rsx! {
        div {
            style: if overlay_mode {
                "width:min(560px, 92vw); color:#e5e7eb; font-family:system-ui;"
            } else {
                "min-height:var(--gs26-app-height); height:var(--gs26-app-height); overflow-y:auto; overflow-x:hidden; display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;"
            },
            div {
                style: "width:min(560px, 92vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220; box-shadow:0 12px 30px rgba(0,0,0,0.5);",
                h1 { style: "margin:0 0 10px 0; font-size:22px;", "{title}" }
                p { style: "margin:0 0 16px 0; color:#94a3b8;", "{subtitle}" }

                if base.trim().is_empty() {
                    div {
                        style: "margin-bottom:14px; padding:12px; border-radius:12px; border:1px solid #7c2d12; background:#451a03; color:#fed7aa;",
                        "Configure the backend URL before logging in."
                    }
                }

                input {
                    style: "width:100%; padding:12px; border-radius:12px; border:1px solid #334155; background:#020617; color:#e5e7eb; outline:none; margin-bottom:12px;",
                    placeholder: "Username",
                    value: "{username()}",
                    oninput: move |evt| username.set(evt.value()),
                }

                input {
                    style: "width:100%; padding:12px; border-radius:12px; border:1px solid #334155; background:#020617; color:#e5e7eb; outline:none;",
                    r#type: "password",
                    placeholder: "Password",
                    value: "{password()}",
                    oninput: move |evt| password.set(evt.value()),
                }

                div { style: "margin-top:12px; display:flex; align-items:center; gap:10px;",
                    input {
                        r#type: "checkbox",
                        checked: *remember_me.read(),
                        onclick: move |_| {
                            let next = !*remember_me.read();
                            remember_me.set(next);
                        },
                    }
                    div { style: "font-size:13px; color:#94a3b8;", "Remember this device until the backend session expires" }
                }

                if !status().is_empty() {
                    div {
                        style: "margin-top:14px; padding:12px; border-radius:12px; border:1px solid #334155; background:#020617; color:#cbd5e1; white-space:pre-wrap;",
                        "{status()}"
                    }
                }

                div { style: "display:flex; gap:12px; margin-top:16px; justify-content:flex-end; flex-wrap:wrap;",
                    if matches!(logged_out_status.read().as_ref(), Some(Ok(status)) if status.permissions.view_data) {
                        button {
                            style: "padding:10px 14px; border-radius:12px; border:1px solid #334155; background:#0f172a; color:#e5e7eb; cursor:pointer;",
                            disabled: busy() || base.trim().is_empty(),
                            onclick: move |_| {
                                let success_route = continue_logged_out_route.clone();
                                let base = continue_logged_out_base.clone();
                                busy.set(true);
                                status.set("Continuing logged out...".to_string());
                                spawn(async move {
                                    match auth::fetch_logged_out_session_status(&base, skip_tls).await {
                                        Ok(session) => {
                                            auth::set_logged_out_status(session);
                                            busy.set(false);
                                            status.set(String::new());
                                            let _ = nav.replace(success_route);
                                        }
                                        Err(err) => {
                                            busy.set(false);
                                            status.set(format!("Logged-out access failed: {err}"));
                                        }
                                    }
                                });
                            },
                            "Use Logged Out"
                        }
                    }

                    if allow_back_to_connect {
                        button {
                            style: "padding:10px 14px; border-radius:12px; border:1px solid #334155; background:#111827; color:#e5e7eb; cursor:pointer;",
                            onclick: move |_| {
                                let _ = nav.replace(connect_route());
                            },
                            "Back"
                        }
                    }

                    button {
                        style: "padding:10px 14px; border-radius:12px; border:1px solid #334155; background:#111827; color:#e5e7eb; cursor:pointer;",
                        disabled: busy() || base.trim().is_empty(),
                        onclick: move |_| {
                            let base = UrlConfig::base_http();
                            if base.trim().is_empty() {
                                status.set("Configure the backend URL first.".to_string());
                                return;
                            }
                            let username_value = username();
                            let password_value = password();
                            if username_value.trim().is_empty() || password_value.is_empty() {
                                status.set("Enter both username and password.".to_string());
                                return;
                            }
                            let remember = *remember_me.read();
                            let success_route = sign_in_route.clone();
                            busy.set(true);
                            status.set("Signing in...".to_string());
                            spawn(async move {
                                match auth::login(&base, skip_tls, username_value.trim(), &password_value, remember).await {
                                    Ok(_) => {
                                        telemetry_dashboard::reconnect_and_reseed_after_auth_change();
                                        busy.set(false);
                                        status.set(String::new());
                                        let _ = nav.replace(success_route);
                                    }
                                    Err(err) => {
                                        busy.set(false);
                                        status.set(err);
                                    }
                                }
                            });
                        },
                        if busy() { "Signing In..." } else { "Sign In" }
                    }
                }
            }
        }
    }
}

#[component]
fn ConnectionFailedCard(message: String, on_retry: EventHandler<()>) -> Element {
    let nav = use_navigator();
    rsx! {
        div {
            style: "min-height:var(--gs26-app-height); height:var(--gs26-app-height); overflow-y:auto; overflow-x:hidden; display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
            div {
                style: "width:min(560px, 92vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220; box-shadow:0 12px 30px rgba(0,0,0,0.5);",
                h1 { style: "margin:0 0 10px 0; font-size:22px;", "Failed to Connect" }
                p { style: "margin:0 0 16px 0; color:#94a3b8; white-space:pre-wrap;", "{message}" }
                div { style: "display:flex; gap:12px; justify-content:flex-end; flex-wrap:wrap;",
                    button {
                        style: "padding:10px 14px; border-radius:12px; border:1px solid #334155; background:#111827; color:#e5e7eb; cursor:pointer;",
                        onclick: move |_| {
                            let _ = nav.replace(connect_route());
                        },
                        "Back to Connect"
                    }
                    button {
                        style: "padding:10px 14px; border-radius:12px; border:1px solid #334155; background:#0f172a; color:#e5e7eb; cursor:pointer;",
                        onclick: move |_| {
                            on_retry.call(());
                        },
                        "Retry"
                    }
                }
            }
        }
    }
}

#[component]
fn LoginOverlay(
    title: String,
    subtitle: String,
    allow_back_to_connect: bool,
    on_success_route: Route,
) -> Element {
    rsx! {
        div {
            style: "position:relative; width:100%; min-height:var(--gs26-app-height);",
            crate::telemetry_dashboard::TelemetryDashboard {}
            div {
                style: "position:fixed; inset:0; display:flex; align-items:center; justify-content:center; padding:24px; background:rgba(2, 6, 23, 0.78); backdrop-filter:blur(8px); z-index:1000;",
                LoginCard {
                    title: title.clone(),
                    subtitle: subtitle.clone(),
                    allow_back_to_connect,
                    on_success_route: on_success_route.clone(),
                    overlay_mode: true,
                }
            }
        }
    }
}

#[component]
pub fn Login() -> Element {
    let show_live_dashboard = telemetry_dashboard::dashboard_has_prior_backend_connection();
    if show_live_dashboard {
        rsx! {
            LoginOverlay {
                title: "Sign In".to_string(),
                subtitle: "Authenticate with the backend to view protected data or send commands.".to_string(),
                allow_back_to_connect: true,
                on_success_route: Route::Dashboard {},
            }
        }
    } else {
        rsx! {
            LoginCard {
                title: "Sign In".to_string(),
                subtitle: "Authenticate with the backend to view protected data or send commands.".to_string(),
                allow_back_to_connect: true,
                on_success_route: Route::Dashboard {},
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[component]
pub fn Connect() -> Element {
    let nav = use_navigator();

    let initial =
        UrlConfig::_stored_base_url().unwrap_or_else(|| "https://your-backend-url.com".to_string());
    let initial_skip_tls = UrlConfig::_skip_tls_verify_for_base(&initial);

    let mut url_edit = use_signal(|| initial);
    let mut skip_tls = use_signal(|| initial_skip_tls);

    let mut test_status = use_signal(|| "".to_string());
    let mut testing = use_signal(|| false);

    rsx! {
        div {
            style: "min-height:var(--gs26-app-height); height:var(--gs26-app-height); overflow-y:auto; overflow-x:hidden; display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
            div {
                style: "width:min(900px, 94vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220; box-shadow:0 12px 30px rgba(0,0,0,0.5);",

                div {
                    style: "display:flex; align-items:flex-start; justify-content:space-between; gap:12px; margin-bottom:12px;",
                    h1 { style: "margin:0; font-size:20px;", "{APP_DISPLAY_NAME}" }
                    button {
                        style: "
                            padding:10px 14px;
                            border-radius:12px;
                            border:1px solid #334155;
                            background:#111827;
                            color:#e5e7eb;
                            cursor:pointer;
                        ",
                        onclick: move |_| {
                            let _ = nav.push(Route::Version {});
                        },
                        "Version"
                    }
                }
                p { style: "margin:0 0 16px 0; color:#94a3b8;",
                    "Enter the backend URL (including http:// or https://). Example: ",
                    code { "https://your-backend-url.com" }
                }

                input {
                    style: "width:100%; padding:12px; border-radius:12px; border:1px solid #334155; background:#020617; color:#e5e7eb; outline:none;",
                    value: "{url_edit()}",
                    oninput: move |evt| {
                        url_edit.set(evt.value());
                        test_status.set("".to_string());
                    },
                }

                div { style: "margin-top:12px; display:flex; align-items:center; gap:10px;",
                    input {
                        r#type: "checkbox",
                        checked: *skip_tls.read(),
                        onclick: move |_| {
                            let next = !*skip_tls.read();
                            skip_tls.set(next);
                            let base = normalize_base_url(url_edit().trim().to_string());
                            if !base.is_empty() {
                                UrlConfig::_set_skip_tls_verify_for_base(&base, next);
                            }
                        }
                    }
                    div { style: "font-size:13px; color:#94a3b8;",
                        "Disable TLS certificate verification for this host (self-signed certs)"
                    }
                }

                if !test_status().is_empty() {
                    pre {
                        style: "
                            margin:14px 0 0 0;
                            padding:12px;
                            border-radius:12px;
                            border:1px solid #334155;
                            background:#020617;
                            color:#cbd5e1;
                            font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, 'Liberation Mono', 'Courier New', monospace;
                            font-size:12px;
                            line-height:1.35;
                            max-height:420px;
                            overflow:auto;
                            white-space:pre;
                        ",
                        "{test_status()}"
                    }
                }

                div { style: "display:flex; gap:12px; margin-top:16px; justify-content:flex-end; flex-wrap:wrap;",
                    button {
                        style: "
                            padding:10px 14px;
                            border-radius:12px;
                            border:1px solid #334155;
                            background:#111827;
                            color:#e5e7eb;
                            cursor:pointer;
                        ",
                        onclick: move |_| {
                            let u_norm = normalize_base_url(url_edit().trim().to_string());
                            if u_norm.is_empty() {
                                test_status.set("Enter a URL first.".to_string());
                                return;
                            }
                            if !(u_norm.starts_with("http://") || u_norm.starts_with("https://")) {
                                test_status.set("URL must start with http:// or https://".to_string());
                                return;
                            }

                            objc_poke::poke_url(&u_norm);
                            UrlConfig::set_base_url_and_persist(u_norm.to_string());
                            UrlConfig::_set_skip_tls_verify_for_base(&u_norm, *skip_tls.read());
                            let _ = persist::write_connect_shown(true);
                            let _ = nav.replace(Route::Login {});
                        },
                        "Sign In"
                    }

                    button {
                        style: "
                            padding:10px 14px;
                            border-radius:12px;
                            border:1px solid #334155;
                            background:#0f172a;
                            color:#e5e7eb;
                            cursor:pointer;
                        ",
                        disabled: testing(),
                        onclick: move |_| {
                            let u_norm = normalize_base_url(url_edit().trim().to_string());
                            if u_norm.is_empty() {
                                test_status.set("Enter a URL first.".to_string());
                                return;
                            }

                            let parsed = match parse_base_url(&u_norm) {
                                Ok(p) => p,
                                Err(e) => {
                                    test_status.set(e);
                                    return;
                                }
                            };

                            testing.set(true);
                            test_status.set("Testing connection (fast probes)...".to_string());

                            objc_poke::poke_url(&u_norm);

                            let skip_tls_verify = *skip_tls.read();
                            spawn(async move {
                                let checks = test_routes_host_only(&u_norm, skip_tls_verify).await;

                                let ws_probe = Some(ws_connect_probe(&parsed, skip_tls_verify).await);

                                let report =
                                    format_route_report_host_only(&u_norm, &parsed, &checks, ws_probe);

                                testing.set(false);
                                test_status.set(report);
                            });
                        },
                        if testing() { "Testing..." } else { "Test Connection" }
                    }

                    button {
                        style: "
                            padding:10px 14px;
                            border-radius:12px;
                            border:1px solid #334155;
                            background:#111827;
                            color:#e5e7eb;
                            cursor:pointer;
                        ",
                        onclick: move |_| {
                            let u_norm = normalize_base_url(url_edit().trim().to_string());
                            if u_norm.is_empty() {
                                test_status.set("Enter a URL first.".to_string());
                                return;
                            }
                            if !(u_norm.starts_with("http://") || u_norm.starts_with("https://")) {
                                test_status.set("URL must start with http:// or https://".to_string());
                                return;
                            }

                            objc_poke::poke_url(&u_norm);

                            UrlConfig::set_base_url_and_persist(u_norm.to_string());
                            UrlConfig::_set_skip_tls_verify_for_base(&u_norm, *skip_tls.read());
                            if UrlConfig::_stored_base_url().as_deref() != Some(u_norm.as_str()) {
                                test_status.set(
                                    "Failed to save the backend URL on this device. The app stayed disconnected."
                                        .to_string(),
                                );
                                return;
                            }
                            let _ = persist::write_connect_shown(true);
                            let _ = nav.replace(Route::Dashboard {});
                        },
                        "Connect"
                    }
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[component]
pub fn Version() -> Element {
    let nav = use_navigator();
    let can_go_back = nav.can_go_back();
    let back_action = move |_| {
        if can_go_back {
            nav.go_back();
        } else if UrlConfig::_stored_base_url().is_some() {
            let _ = nav.replace(Route::Dashboard {});
        } else {
            let _ = nav.replace(connect_route());
        }
    };

    rsx! {
        div {
            style: "position:fixed; inset:0; overflow-y:auto; overflow-x:hidden; display:flex; align-items:flex-start; justify-content:center; padding:24px 16px; background:#020617; color:#e5e7eb; font-family:system-ui; overscroll-behavior:contain; -webkit-overflow-scrolling:touch;",
            div {
                style: "width:min(900px, 100%); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220; box-shadow:0 12px 30px rgba(0,0,0,0.5);",
                div {
                    style: "display:flex; align-items:flex-start; justify-content:space-between; gap:12px; margin-bottom:12px; flex-wrap:wrap;",
                    h1 { style: "margin:0; font-size:20px;", "{APP_DISPLAY_NAME}" }
                    button {
                        style: "
                            padding:10px 14px;
                            border-radius:12px;
                            border:1px solid #334155;
                            background:#111827;
                            color:#e5e7eb;
                            font-weight:700;
                            cursor:pointer;
                        ",
                        onclick: back_action,
                        "Back"
                    }
                }
                crate::telemetry_dashboard::version_page::VersionTab {}
            }
        }
    }
}

#[component]
pub fn Dashboard() -> Element {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let nav = use_navigator();
        if UrlConfig::_stored_base_url().is_none() {
            return rsx! {
                div {
                    style: "height:var(--gs26-app-height); display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
                    div {
                        style: "width:min(560px, 92vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220;",
                        h1 { style: "margin:0 0 12px 0; font-size:18px;", "Not connected" }
                        p { style: "margin:0 0 16px 0; color:#94a3b8;", "Please configure the backend URL on the Connect screen." }
                        button {
                            style: "
                                padding:10px 14px;
                                border-radius:10px;
                                border:1px solid #334155;
                                background:#111827;
                                color:#e5e7eb;
                                font-weight:700;
                                cursor:pointer;
                            ",
                            onclick: move |_| {
                                let _ = nav.replace(connect_route());
                            },
                            "Back to Connect"
                        }
                    }
                }
            };
        }
    }

    let base = UrlConfig::base_http();
    auth::init_from_storage(&base);
    let mut auth_state = use_signal(|| None::<Result<AuthSessionStatus, String>>);
    let mut auth_state_base = use_signal(String::new);
    use_effect(move || {
        let base = UrlConfig::base_http();
        if *auth_state_base.read() != base {
            auth_state_base.set(base.clone());
            auth_state.set(None);
        }
        let skip_tls = UrlConfig::_skip_tls_verify();
        if auth_state.read().is_some() {
            return;
        }
        spawn(async move {
            auth_state.set(Some(auth::fetch_session_status(&base, skip_tls).await));
        });
    });

    match auth_state.read().as_ref() {
        None => rsx! {
            div { style: "height:var(--gs26-app-height); display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
                div { style: "padding:20px; border:1px solid #334155; border-radius:16px; background:#0b1220;", "Checking session..." }
            }
        },
        Some(Ok(status)) if status.permissions.view_data => {
            rsx! { crate::telemetry_dashboard::TelemetryDashboard {} }
        }
        Some(Ok(_)) => {
            if telemetry_dashboard::dashboard_has_prior_backend_connection() {
                rsx! {
                    LoginOverlay {
                        title: "Sign In Required".to_string(),
                        subtitle: "This backend does not allow anonymous view access. Sign in to continue.".to_string(),
                        allow_back_to_connect: true,
                        on_success_route: Route::Dashboard {},
                    }
                }
            } else {
                rsx! {
                    LoginCard {
                        title: "Sign In Required".to_string(),
                        subtitle: "This backend does not allow anonymous view access. Sign in to continue.".to_string(),
                        allow_back_to_connect: true,
                        on_success_route: Route::Dashboard {},
                    }
                }
            }
        }
        Some(Err(err)) => rsx! {
            ConnectionFailedCard {
                message: format!(
                    "The frontend could not reach the backend session endpoint.\n\n{}",
                    err
                ),
                on_retry: move |_| {
                    let base = UrlConfig::base_http();
                    let skip_tls = UrlConfig::_skip_tls_verify();
                    auth_state.set(None);
                    spawn(async move {
                        auth_state.set(Some(auth::fetch_session_status(&base, skip_tls).await));
                    });
                },
            }
        },
    }
}
