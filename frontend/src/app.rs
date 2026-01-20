// frontend/src/app.rs
//
// Replaces your existing file.
// Adds a route-aware TEST button that probes the actual backend routes you listed,
// while still doing a unicast TCP “connect poke” (native) to help trigger iOS Local Network permission.
//
// Notes:
// - Native uses reqwest (short timeout) for route probes.
// - Web uses gloo_net for route probes (same-origin or user-provided base URL in localStorage).
// - /ws is checked as “reachable” if it returns 400/426/101 because a plain GET won’t upgrade.
// - /tiles/ is treated as reachable even if it returns 404/403 (tile may not exist).

use dioxus::prelude::*;
use dioxus_router::{Routable, Router};

#[cfg(not(target_arch = "wasm32"))]
use dioxus_router::use_navigator;

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

const _BASE_URL_KEY: &str = "gs26_base_url";
const _CONNECT_SHOWN_KEY: &str = "gs26_connect_shown";

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
// Persistence helpers
// -------------------------

#[cfg(target_arch = "wasm32")]
mod persist {
    use super::{_BASE_URL_KEY, _CONNECT_SHOWN_KEY};

    fn read_key(key: &str) -> Option<String> {
        use web_sys::window;
        let w = window()?;
        let ls = w.local_storage().ok()??;
        ls.get_item(key).ok().flatten()
    }

    fn write_key(key: &str, v: &str) {
        use web_sys::window;
        if let Some(w) = window() {
            if let Ok(Some(ls)) = w.local_storage() {
                let _ = ls.set_item(key, v);
            }
        }
    }

    pub fn read_base_url() -> Option<String> {
        read_key(_BASE_URL_KEY)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn write_base_url(v: &str) {
        write_key(_BASE_URL_KEY, v);
    }

    pub fn read_connect_shown() -> bool {
        read_key(_CONNECT_SHOWN_KEY)
            .map(|s| s.trim().eq_ignore_ascii_case("true") || s.trim() == "1")
            .unwrap_or(false)
    }

    pub fn write_connect_shown(v: bool) {
        write_key(_CONNECT_SHOWN_KEY, if v { "true" } else { "false" });
    }
}

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

    pub fn write_base_url(v: &str) -> Result<(), io::Error> {
        write_key(_BASE_URL_KEY, v)
    }

    pub fn write_connect_shown(v: bool) -> Result<(), io::Error> {
        write_key(_CONNECT_SHOWN_KEY, if v { "true" } else { "false" })
    }
}

// -------------------------
// URL parsing + "connect poke" (native only)
// -------------------------

#[cfg(not(target_arch = "wasm32"))]
fn parse_host_port_from_url(url: &str) -> Option<(String, u16)> {
    let hp = url
        .trim()
        .strip_prefix("http://")
        .or_else(|| url.trim().strip_prefix("https://"))?;

    let hp = hp.split('/').next().unwrap_or(hp);

    let mut parts = hp.split(':');
    let host = parts.next()?.trim();
    if host.is_empty() {
        return None;
    }

    let port = parts.next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(80);
    Some((host.to_string(), port))
}

/// Unicast TCP attempt to the user-entered host/port.
/// - tends to map to "connect to devices" more than "discover"
/// - can also trigger the Local Network prompt if host resolves to LAN
#[cfg(not(target_arch = "wasm32"))]
fn tcp_connect_poke(host: String, port: u16) -> std::io::Result<()> {
    use std::{net::TcpStream, time::Duration};

    if let Ok(sa) = format!("{host}:{port}").parse::<std::net::SocketAddr>() {
        let _s = TcpStream::connect_timeout(&sa, Duration::from_millis(900))?;
        return Ok(());
    }

    let _s = TcpStream::connect((host.as_str(), port))?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn parse_host_port_from_url(_: &str) -> Option<(String, u16)> {
    None
}

// -------------------------
// Route probing (actual backend routes)
// -------------------------

#[derive(Clone)]
struct RouteCheck {
    path: &'static str,
    ok: bool,
    status: Option<u16>,
    note: String,
}

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

fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path
    } else {
        // should never happen here
        "/"
    };
    format!("{base}{path}")
}

#[cfg(not(target_arch = "wasm32"))]
async fn http_probe(url: String) -> Result<u16, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1400))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    Ok(resp.status().as_u16())
}

#[cfg(target_arch = "wasm32")]
async fn http_probe(url: String) -> Result<u16, String> {
    use gloo_net::http::Request;
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    Ok(resp.status())
}

fn status_ok_for_path(path: &str, status: u16) -> (bool, &'static str) {
    match path {
        // JSON-ish endpoints that should succeed with 200
        "/api/recent" | "/api/history" | "/api/alerts?minutes=1" | "/flightstate" | "/api/gps" => {
            (status == 200, "expected 200")
        }

        // Static assets that should succeed with 200
        "/ground_map.js" | "/vendor/leaflet/leaflet.js" | "/vendor/leaflet/leaflet.css" => {
            (status == 200, "expected 200")
        }

        // WebSocket route: GET won't upgrade; accept common “upgrade required/failed” statuses.
        "/ws" => match status {
            101 | 400 | 426 => (true, "reachable (ws upgrade required)"),
            _ => (false, "unexpected status for ws route"),
        },

        // Tiles: a directory request may 404/403 even if service is mounted.
        "/tiles/" => match status {
            200 | 403 | 404 => (true, "reachable (tile may not exist)"),
            _ => (false, "unexpected status for tiles"),
        },

        _ => (status >= 200 && status < 400, "ok"),
    }
}

async fn test_routes(base: &str) -> Vec<RouteCheck> {
    // Probes based on your axum Router
    let probes: &[&str] = &[
        "/api/recent",
        "/api/history",
        "/api/alerts?minutes=1",
        "/flightstate",
        "/api/gps",
        "/ground_map.js",
        "/vendor/leaflet/leaflet.js",
        "/vendor/leaflet/leaflet.css",
        "/tiles/",
        "/ws",
    ];

    let mut out = Vec::new();

    for path in probes {
        let url = join_url(base, path);
        match http_probe(url).await {
            Ok(status) => {
                let (ok, note) = status_ok_for_path(path, status);
                out.push(RouteCheck {
                    path,
                    ok,
                    status: Some(status),
                    note: note.to_string(),
                });
            }
            Err(e) => {
                out.push(RouteCheck {
                    path,
                    ok: false,
                    status: None,
                    note: e,
                });
            }
        }
    }

    out
}

fn format_route_report(base: &str, checks: &[RouteCheck]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Testing routes on: {base}\n\n"));

    for c in checks {
        let status_str = c.status.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
        let icon = if c.ok { "✅" } else { "❌" };
        s.push_str(&format!(
            "{icon} {:30} status {:>3}  {}\n",
            c.path, status_str, c.note
        ));
    }

    let ok_all = checks.iter().all(|c| c.ok);
    if ok_all {
        s.push_str("\nAll required routes look reachable.");
    } else {
        s.push_str("\nSome routes failed.");
        s.push_str("\nTip: /ws may show 400/426 and still be OK (it needs a WebSocket upgrade).");
    }

    s
}

// -------------------------
// App
// -------------------------

#[component]
pub fn App() -> Element {
    rsx! {
        document::Style { "{GLOBAL_CSS}" }

        // Leaflet CSS / JS are fine to include globally.
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
        // Web: always go dashboard (same-origin)
        return rsx! { Dashboard {} };
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let nav = use_navigator();

        use_effect(move || {
            // If base URL is missing, force Connect.
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
        .unwrap_or_else(|| "http://192.168.1.50:3000".to_string());

    let mut url_edit = use_signal(|| initial);

    // UI status for the Test button
    let mut test_status = use_signal(|| "".to_string());
    let mut testing = use_signal(|| false);

    // small helper
    let mut set_report = move |s: String| {
        test_status.set(s);
    };

    rsx! {
        div {
            style: "height:100vh; display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
            div {
                style: "width:min(720px, 94vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220; box-shadow:0 12px 30px rgba(0,0,0,0.5);",

                h1 { style: "margin:0 0 12px 0; font-size:20px;", "GroundStation 26" }

                p { style: "margin:0 0 16px 0; color:#94a3b8;",
                    "Enter the backend URL (including http:// or https://). Example: ",
                    code { "http://10.0.0.42:3000" }
                }

                input {
                    style: "width:100%; padding:12px; border-radius:12px; border:1px solid #334155; background:#020617; color:#e5e7eb; outline:none;",
                    value: "{url_edit()}",
                    oninput: move |evt| {
                        url_edit.set(evt.value());
                        test_status.set("".to_string());
                    },
                }

                // Report box (monospace, scroll)
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
                            max-height:240px;
                            overflow:auto;
                            white-space:pre;
                        ",
                        "{test_status()}"
                    }
                }

                div { style: "display:flex; gap:12px; margin-top:16px; justify-content:flex-end; flex-wrap:wrap;",

                    // TEST ROUTES button
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
                            let u = normalize_base_url(url_edit().trim().to_string());
                            if u.is_empty() {
                                set_report("Enter a URL first.".to_string());
                                return;
                            }
                            if !(u.starts_with("http://") || u.starts_with("https://")) {
                                set_report("URL must start with http:// or https://".to_string());
                                return;
                            }

                            testing.set(true);
                            set_report("Testing routes...".to_string());

                            // Optional: unicast “poke” to trigger Local Network permission (iOS)
                            if let Some((host, port)) = parse_host_port_from_url(&u) {
                                let _ = std::thread::spawn(move || {
                                    let _ = tcp_connect_poke(host, port);
                                });
                            }

                            // Run async HTTP probes
                            spawn(async move {
                                let checks = test_routes(&u).await;
                                let report = format_route_report(&u, &checks);
                                testing.set(false);
                                set_report(report);
                            });
                        },
                        if testing() { "Testing..." } else { "Test routes" }
                    }

                    // CONNECT button
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
                            let u = normalize_base_url(url_edit().trim().to_string());
                            if u.is_empty() {
                                test_status.set("Enter a URL first.".to_string());
                                return;
                            }
                            if !(u.starts_with("http://") || u.starts_with("https://")) {
                                test_status.set("URL must start with http:// or https://".to_string());
                                return;
                            }

                            let _ = persist::write_base_url(&u);
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
    // On native, refuse to mount the dashboard if base URL is missing.
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
