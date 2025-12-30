use dioxus::prelude::*;
use dioxus_router::{Routable, Router};

#[cfg(not(target_arch = "wasm32"))]
use dioxus_router::use_navigator;

// --- your existing global css ---
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

// NEW: show connect screen once on native targets
const _CONNECT_SHOWN_KEY: &str = "gs26_connect_shown";

#[derive(Clone, Routable, PartialEq)]
pub enum Route {
    #[route("/")]
    Root {},

    #[route("/dashboard")]
    Dashboard {},

    // native only
    #[cfg(not(target_arch = "wasm32"))]
    #[route("/connect")]
    Connect {},
}

// -------------------------
// Persistence helpers
// -------------------------

#[cfg(target_arch = "wasm32")]
mod persist {
    #[allow(unused_imports)]
    use super::{_BASE_URL_KEY, _CONNECT_SHOWN_KEY};

    fn _read_key(key: &str) -> Option<String> {
        use web_sys::window;
        let w = window()?;
        let ls = w.local_storage().ok()??;
        ls.get_item(key).ok().flatten()
    }

    fn _write_key(key: &str, v: &str) {
        use web_sys::window;
        if let Some(w) = window() {
            if let Ok(Some(ls)) = w.local_storage() {
                let _ = ls.set_item(key, v);
            }
        }
    }

    pub fn _read_base_url() -> Option<String> {
        _read_key(_BASE_URL_KEY).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn _write_base_url(v: &str) {
        _write_key(_BASE_URL_KEY, v);
    }

    pub fn _read_connect_shown() -> bool {
        _read_key(_CONNECT_SHOWN_KEY)
            .map(|s| s.trim().eq_ignore_ascii_case("true") || s.trim() == "1")
            .unwrap_or(false)
    }

    pub fn _write_connect_shown(v: bool) {
        _write_key(_CONNECT_SHOWN_KEY, if v { "true" } else { "false" });
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

    pub fn read_connect_shown() -> bool {
        read_key(_CONNECT_SHOWN_KEY)
            .map(|s| s.eq_ignore_ascii_case("true") || s == "1")
            .unwrap_or(false)
    }

    pub fn write_connect_shown(v: bool) -> Result<(), io::Error> {
        write_key(_CONNECT_SHOWN_KEY, if v { "true" } else { "false" })
    }
}

// -------------------------
// App
// -------------------------

#[component]
pub fn App() -> Element {
    rsx! {
        document::Style { "{GLOBAL_CSS}" }

        // Leaflet CSS
        document::Link {
            rel: "stylesheet",
            href: asset!("static/vendor/leaflet/leaflet.css"),
        }

        // Leaflet JS (must come before ground_map.js)
        document::Script { src: asset!("static/vendor/leaflet/leaflet.js") }

        // Your JS that defines window.initGroundMap / window.updateGroundMapMarkers
        document::Script { src: asset!("static/ground_map.js") }

        div {
            style: "min-height: 100vh; width: 100%; background: #020617; color: #e5e7eb;",
            Router::<Route> {}
        }
    }
}

#[component]
pub fn Root() -> Element {
    // Web: keep URL unchanged, just render dashboard (same-origin)
    #[cfg(target_arch = "wasm32")]
    {
        return rsx! { Dashboard {} };
    }

    // Native:
    // - If connect has never been shown: go to connect (once)
    // - Else: connect only if base URL missing
    #[cfg(not(target_arch = "wasm32"))]
    {
        let nav = use_navigator();

        use_effect(move || {
            let shown = persist::read_connect_shown();
            if !shown {
                let _ = nav.replace(Route::Connect {});
                return;
            }

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

    // Initial value from native persistence (file)
    let initial = persist::read_base_url()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:3000".to_string());

    // Editable field
    let mut url_edit = use_signal(|| initial);

    rsx! {
        div {
            style: "height:100vh; display:flex; align-items:center; justify-content:center; background:#020617; color:#e5e7eb; font-family:system-ui;",
            div {
                style: "width:min(560px, 92vw); padding:24px; border:1px solid #334155; border-radius:16px; background:#0b1220; box-shadow:0 12px 30px rgba(0,0,0,0.5);",
                h1 { style: "margin:0 0 12px 0; font-size:20px;", "GroundStation 26" }
                p { style: "margin:0 0 16px 0; color:#94a3b8;",
                    "Enter the backend URL (including http:// or https://). Example: ",
                    code { "http://10.0.0.42:3000" }
                }

                input {
                    style: "width:100%; padding:12px; border-radius:12px; border:1px solid #334155; background:#020617; color:#e5e7eb; outline:none;",
                    value: "{url_edit()}",
                    oninput: move |evt| url_edit.set(evt.value()),
                }

                div { style: "display:flex; gap:12px; margin-top:16px; justify-content:flex-end;",
                    button {
                        style: "padding:10px 14px; border-radius:12px; border:1px solid #334155; background:#111827; color:#e5e7eb; cursor:pointer;",
                        onclick: move |_| {
                            let u = url_edit().trim().to_string();
                            if !u.is_empty() {
                                // Persist base url + mark connect as shown
                                let _ = persist::write_base_url(&u);
                                let _ = persist::write_connect_shown(true);

                                let _ = nav.replace(Route::Dashboard {});
                            }
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
    rsx! { crate::telemetry_dashboard::TelemetryDashboard {} }
}
