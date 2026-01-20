// frontend/src/app.rs
//
// COMPLETE REPLACEMENT FILE
//
// Changes:
// - Removed IP-based resolve/poke/tests (hostname only)
// - Added REAL WebSocket connect probe (ws:// or wss://) and prints why it fails
// - Kept native ObjC poke for Local Network prompt (hostname only)
// - WASM behavior unchanged (no Connect screen)

use dioxus::prelude::*;
use dioxus_router::{Routable, Router};

#[cfg(not(target_arch = "wasm32"))]
use dioxus_router::use_navigator;
#[allow(unused_imports)]
use crate::telemetry_dashboard::UrlConfig;

// --- global css ---
const GLOBAL_CSS: &str = r#"
html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    background: #020617;
    overflow: hidden;
}

#main {
    width: 100%;
    height: 100%;
    background: #020617;
}

* { box-sizing: border-box; }
"#;

const _BASE_URL_KEY: &str = "gs_base_url";
const _CONNECT_SHOWN_KEY: &str = "gs_connect_shown";

#[derive(Clone, Routable, PartialEq)]
pub enum Route {
    #[route("/")]
    Root {},

    #[route("/dashboard")]
    Dashboard {},

    #[cfg(not(target_arch = "wasm32"))]
    #[route("/connect")]
    Connect {},
}

// -------------------------
// Native-only Objective-C poke shims
// -------------------------
#[cfg(not(target_arch = "wasm32"))]
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

// -------------------------
// Persistence helpers
// -------------------------

#[cfg(not(target_arch = "wasm32"))]
mod persist {
    use super::{_BASE_URL_KEY, _CONNECT_SHOWN_KEY};
    use std::io;

    fn storage_dir() -> std::path::PathBuf {
        dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
            .join("gs26")
    }

    fn path_for(key: &str) -> std::path::PathBuf {
        storage_dir().join(format!("{key}.txt"))
    }

    fn read_key(key: &str) -> Option<String> {
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

    pub fn read_base_url() -> Option<String> {
        read_key(_BASE_URL_KEY).filter(|s| !s.trim().is_empty())
    }

    pub fn _write_base_url(v: &str) -> Result<(), io::Error> {
        write_key(_BASE_URL_KEY, v)
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
    // Use explicit port if non-default, keep it always (simpler + matches your base)
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
        "/api/recent" | "/api/history" | "/api/alerts" | "/flightstate" | "/api/gps" => {
            (status == 200, "expected 200")
        }
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
async fn http_probe(url: String) -> Result<(u16, String), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(18000))
        .build()
        .map_err(|e| format!("build client failed: {e}"))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("send failed: {} | kind={}", e, classify_reqwest_error(&e)))?;

    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;

    Ok((status, snip(body, 300)))
}
#[cfg(not(target_arch = "wasm32"))]
async fn test_routes_host_only(base: &str) -> Vec<RouteCheck> {
    let probes: &[&str] = &[
        "/api/recent",
        "/api/history",
        "/api/alerts",
        "/flightstate",
        "/api/gps",
        "/tiles",
        "/ws",
    ];

    let mut out = Vec::new();

    for path in probes {
        let url = join_url(base, path);

        match http_probe(url.clone()).await {
            Ok((status, body_snip)) => {
                let (ok, note) = status_ok_for_path(path, status);
                out.push(RouteCheck {
                    path,
                    url,
                    ok,
                    status: Some(status),
                    body_snip,
                    note: note.to_string(),
                    err: None,
                });
            }
            Err(e) => {
                out.push(RouteCheck {
                    path,
                    url,
                    ok: false,
                    status: None,
                    body_snip: "".to_string(),
                    note: "request failed".to_string(),
                    err: Some(e),
                });
            }
        }
    }

    out
}

#[cfg(not(target_arch = "wasm32"))]
async fn ws_connect_probe(parsed: &ParsedBaseUrl) -> Result<String, String> {
    use tokio_tungstenite::connect_async;

    let ws_origin = ws_origin_for_base(parsed);
    let ws_url = format!("{ws_origin}/ws");

    // This is a REAL websocket handshake. If wss fails due to cert/trust/SNI, you’ll see it here.
    let res = connect_async(ws_url.clone()).await;

    match res {
        Ok((_stream, resp)) => Ok(format!(
            "✅ WebSocket handshake OK\n    URL: {}\n    HTTP: {}",
            ws_url,
            resp.status()
        )),
        Err(e) => Err(format!(
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

    s.push_str(&format!("Original base: {original_base}\n"));
    s.push_str(&format!(
        "Parsed host: {}  port: {}  scheme: {}\n\n",
        parsed.host, parsed.port, parsed.scheme
    ));

    s.push_str("=== Probing via host ===\n\n");

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
    rsx! {
        document::Style { "{GLOBAL_CSS}" }

        document::Link {
            rel: "stylesheet",
            href: asset!("static/vendor/leaflet/leaflet.css"),
        }
        document::Script { src: asset!("static/vendor/leaflet/leaflet.js") }
        document::Script { src: asset!("static/ground_map.js") }

        div {
            style: "min-height: 100vh; width: 100%; background: #020617; color: #e5e7eb;",
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
            let u = persist::read_base_url().unwrap_or_default();
            if u.trim().is_empty() {
                let _ = nav.replace(Route::Connect {});
            } else {
                let _ = nav.replace(Route::Dashboard {});
            }
        });

        rsx! { div {} }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[component]
pub fn Connect() -> Element {
    let nav = use_navigator();

    let initial = persist::read_base_url()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:3000".to_string());

    let mut url_edit = use_signal(|| initial);

    let mut test_status = use_signal(|| "".to_string());
    let mut testing = use_signal(|| false);

    rsx! {
        div {
            style: "height:100vh; display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
            div {
                style: "width:min(900px, 94vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220; box-shadow:0 12px 30px rgba(0,0,0,0.5);",

                h1 { style: "margin:0 0 12px 0; font-size:20px;", "GroundStation 26" }

                p { style: "margin:0 0 16px 0; color:#94a3b8;",
                    "Enter the backend URL (including http:// or https://). Example: ",
                    code { "http://localhost:3000" }
                }

                input {
                    style: "width:100%; padding:12px; border-radius:12px; border:1px solid #334155; background:#020617; color:#e5e7eb; outline:none;",
                    value: "{url_edit()}",
                    oninput: move |evt| {
                        url_edit.set(evt.value());
                        test_status.set("".to_string());
                    },
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

                    // TEST ROUTES (HOSTNAME ONLY)
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
                            test_status.set("Testing connection...".to_string());

                            // Hostname-only poke (keeps your original behavior)
                            objc_poke::poke_url(&u_norm);

                            spawn(async move {
                                // 1) HTTP probes
                                let checks = test_routes_host_only(&u_norm).await;

                                // 2) REAL websocket probe (ws/wss)
                                let ws_probe = Some(ws_connect_probe(&parsed).await);

                                let report = format_route_report_host_only(&u_norm, &parsed, &checks, ws_probe);

                                testing.set(false);
                                test_status.set(report);
                            });
                        },
                        if testing() { "Testing..." } else { "Test Connection" }
                    }

                    // CONNECT
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

#[component]
pub fn Dashboard() -> Element {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let u = persist::read_base_url().unwrap_or_default();
        if u.trim().is_empty() {
            return rsx! {
                div {
                    style: "height:100vh; display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
                    div {
                        style: "width:min(560px, 92vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220;",
                        h1 { style: "margin:0 0 12px 0; font-size:18px;", "Not connected" }
                        p { style: "margin:0; color:#94a3b8;", "Please configure the backend URL on the Connect screen." }
                    }
                }
            };
        }
    }

    rsx! { crate::telemetry_dashboard::TelemetryDashboard {} }
}
