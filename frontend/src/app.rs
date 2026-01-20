// frontend/src/app.rs
//
// COMPLETE REPLACEMENT FILE
//
// Native improvements:
// - Resolve hostname (std::net::ToSocketAddrs) before testing
// - Poke each resolved IP via Objective-C shim to trigger Local Network prompt (if LAN)
// - Probe routes on:
//    1) original hostname base URL
//    2) first-resolved-IP base URL (with Host header set to original hostname)
// - Report resolution results + which base URL was tested
//
// WASM:
// - Same behavior as before (no hostname resolution in browser)

// --- imports ---
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
// Native-only Objective-C poke shims
// -------------------------
#[cfg(not(target_arch = "wasm32"))]
mod objc_poke {
    use std::ffi::CString;
    use std::os::raw::c_char;

    unsafe extern "C" {
        fn gs26_localnet_poke_url(url: *const c_char);
        fn gs26_localnet_poke_host_port(host: *const c_char, port: u16);
    }

    pub fn poke_url(url: &str) {
        if let Ok(c) = CString::new(url) {
            unsafe { gs26_localnet_poke_url(c.as_ptr()) };
        }
    }

    pub fn poke_host_port(host: &str, port: u16) {
        if let Ok(c) = CString::new(host) {
            unsafe { gs26_localnet_poke_host_port(c.as_ptr(), port) };
        }
    }
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
        std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
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
// URL parsing / normalization
// -------------------------

#[derive(Clone, Debug)]
struct ParsedBaseUrl {
    scheme: String, // "http" or "https"
    host: String,
    port: u16,
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
    let path = if path.starts_with('/') { path } else { "/" };
    format!("{base}{path}")
}

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
    let host = parts
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

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
fn rewrite_base_to_ip(parsed: &ParsedBaseUrl, ip: &std::net::IpAddr) -> String {
    // For IPv6 in URLs: https://[::1]:3000
    match ip {
        std::net::IpAddr::V4(v4) => format!("{}://{}:{}", parsed.scheme, v4, parsed.port),
        std::net::IpAddr::V6(v6) => format!("{}://[{}]:{}", parsed.scheme, v6, parsed.port),
    }
}

// -------------------------
// Native-only: resolve host -> IPs and poke them
// -------------------------

#[cfg(not(target_arch = "wasm32"))]
fn resolve_host_ips(host: &str, port: u16) -> Result<Vec<std::net::IpAddr>, String> {
    use std::net::ToSocketAddrs;

    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolve failed: {e}"))?;

    let mut ips: Vec<std::net::IpAddr> = addrs.map(|a| a.ip()).collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

#[cfg(not(target_arch = "wasm32"))]
fn is_private_lan_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            // 10.0.0.0/8
            if o[0] == 10 {
                return true;
            }
            // 172.16.0.0/12
            if o[0] == 172 && (16..=31).contains(&o[1]) {
                return true;
            }
            // 192.168.0.0/16
            if o[0] == 192 && o[1] == 168 {
                return true;
            }
            false
        }
        // For completeness: treat IPv6 ULA (fc00::/7) as LAN-ish
        std::net::IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            (seg0 & 0xfe00) == 0xfc00
        }
    }
}

// -------------------------
// Route probing (actual backend routes)
// -------------------------

#[derive(Clone)]
struct RouteCheck {
    label: String,       // "host" or "ip"
    path: &'static str,
    url: String,
    ok: bool,
    status: Option<u16>,
    body_snip: String,
    note: String,
    err: Option<String>,
}

fn status_ok_for_path(path: &str, status: u16) -> (bool, &'static str) {
    match path {
        "/api/recent" | "/api/history" | "/api/alerts" | "/flightstate" | "/api/gps" => {
            (status == 200, "expected 200")
        }
        "/ground_map.js" | "/vendor/leaflet/leaflet.js" | "/vendor/leaflet/leaflet.css" => {
            (status == 200, "expected 200")
        }
        "/ws" => match status {
            101 | 400 | 426 => (true, "reachable (ws upgrade required)"),
            _ => (false, "unexpected status for ws route"),
        },
        "/tiles/" => match status {
            200 | 403 | 404 => (true, "reachable (tile may not exist)"),
            _ => (false, "unexpected status for tiles"),
        },
        _ => ((200..400).contains(&status), "ok"),
    }
}

fn snip(mut s: String, max: usize) -> String {
    s = s.replace('\r', "");
    if s.len() > max {
        s.truncate(max);
        s.push('…');
    }
    s
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
async fn http_probe(url: String, host_header: Option<String>) -> Result<(u16, String), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1800))
        .build()
        .map_err(|e| format!("build client failed: {e}"))?;

    let mut req = client.get(&url);
    if let Some(h) = host_header {
        // Helps when we connect by IP but the server uses vhosts / routing by Host.
        req = req.header(reqwest::header::HOST, h);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("send failed: {} | kind={}", e, classify_reqwest_error(&e)))?;

    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| format!("read body failed: {e}"))?;

    Ok((status, snip(body, 300)))
}

#[cfg(target_arch = "wasm32")]
async fn http_probe(url: String, _host_header: Option<String>) -> Result<(u16, String), String> {
    use gloo_net::http::Request;

    let resp = Request::get(&url)
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|_| "".to_string());
    Ok((status, snip(body, 300)))
}

async fn test_routes(label: &str, base: &str, host_header: Option<String>) -> Vec<RouteCheck> {
    let probes: &[&str] = &[
        "/api/recent",
        "/api/history",
        "/api/alerts",
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

        match http_probe(url.clone(), host_header.clone()).await {
            Ok((status, body_snip)) => {
                let (ok, note) = status_ok_for_path(path, status);
                out.push(RouteCheck {
                    label: label.to_string(),
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
                    label: label.to_string(),
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

fn format_route_report(original_base: &str, parsed: &ParsedBaseUrl, resolved_ips: &[String], checks: &[RouteCheck]) -> String {
    let mut s = String::new();

    s.push_str(&format!("Original base: {original_base}\n"));
    s.push_str(&format!("Parsed host: {}  port: {}  scheme: {}\n", parsed.host, parsed.port, parsed.scheme));

    if resolved_ips.is_empty() {
        s.push_str("Resolved IPs: (none / resolve failed)\n\n");
    } else {
        s.push_str("Resolved IPs:\n");
        for ip in resolved_ips {
            s.push_str(&format!("  - {ip}\n"));
        }
        s.push('\n');
    }

    let mut last_label = "";
    for c in checks {
        if c.label != last_label {
            s.push_str(&format!("=== Probing via {} ===\n\n", c.label));
            last_label = &c.label;
        }

        let icon = if c.ok { "✅" } else { "❌" };
        let status_str = c.status.map(|v| v.to_string()).unwrap_or_else(|| "—".into());

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

    s.push_str("Notes:\n");
    s.push_str("- If HOST probes fail but IP probes succeed: DNS, vhost, SNI, or split-horizon issues.\n");
    s.push_str("- If both fail with connect/TLS errors but Safari works: likely certificate trust/chain issues for native clients.\n");
    s.push_str("- Local Network prompt only triggers for LAN destinations (e.g. 192.168.x.x / 10.x.x.x / 172.16-31.x.x).\n");

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
        .unwrap_or_else(|| "http://192.168.1.50:3000".to_string());

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

                    // TEST ROUTES
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
                            test_status.set("Resolving host...".to_string());

                            // Keep your original URL poke (hostname path)
                            objc_poke::poke_url(&u_norm);

                            spawn(async move {
                                // Resolve on a blocking thread
                                let host = parsed.host.clone();
                                let port = parsed.port;

                                let res = std::thread::spawn(move || resolve_host_ips(&host, port))
                                    .join()
                                    .unwrap_or_else(|_| Err("resolver panicked".to_string()));

                                let mut resolved_ips: Vec<std::net::IpAddr> = Vec::new();
                                let mut resolved_lines: Vec<String> = Vec::new();

                                match res {
                                    Ok(ips) => {
                                        resolved_ips = ips;
                                        for ip in &resolved_ips {
                                            let lan = if is_private_lan_ip(ip) { " (LAN)" } else { "\"\"" };
                                            resolved_lines.push(format!("{}{}", ip, lan));
                                        }
                                    }
                                    Err(e) => {
                                        resolved_lines.push(format!("(resolve failed) {e}"));
                                    }
                                }

                                // Poke every resolved IP (this is the "LAN trigger" path)
                                for ip in &resolved_ips {
                                    let ip_s = ip.to_string();
                                    // Only poke if it looks LAN-ish (still safe to poke all, but this keeps noise down)
                                    if is_private_lan_ip(ip) {
                                        objc_poke::poke_host_port(&ip_s, parsed.port);
                                    }
                                }

                                // Probe routes:
                                // 1) by hostname base
                                let mut all_checks = Vec::new();
                                let host_checks = test_routes("host", &u_norm, None).await;
                                all_checks.extend(host_checks);

                                // 2) by first resolved IP base (if we have one)
                                if let Some(first_ip) = resolved_ips.first() {
                                    let ip_base = rewrite_base_to_ip(&parsed, first_ip);
                                    // Force Host header to original hostname (helps vhost routing)
                                    let ip_checks = test_routes("ip", &ip_base, Some(parsed.host.clone())).await;
                                    all_checks.extend(ip_checks);
                                }

                                let report = format_route_report(&u_norm, &parsed, &resolved_lines, &all_checks);

                                testing.set(false);
                                test_status.set(report);
                            });
                        },
                        if testing() { "Testing..." } else { "Test routes" }
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

                            // Poke hostname + save
                            objc_poke::poke_url(&u_norm);

                            let _ = persist::write_base_url(&u_norm);
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
