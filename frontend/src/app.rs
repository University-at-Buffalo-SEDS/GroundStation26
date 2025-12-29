use dioxus::prelude::*;
#[allow(unused_imports)]
use dioxus_router::{use_navigator, Routable, Router};

use crate::telemetry_dashboard::UrlConfig;


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

* {
    box-sizing: border-box;
}
"#;

#[derive(Clone, Routable, PartialEq)]
pub enum Route {
    #[route("/")]
    Root {},

    // A real dashboard route for native navigation.
    // (Web will not navigate to it, so the URL stays unchanged.)
    #[route("/dashboard")]
    Dashboard {},

    // Native only (desktop/mobile): choose backend URL
    #[cfg(not(target_arch = "wasm32"))]
    #[route("/connect")]
    Connect {},
}

#[component]
pub fn App() -> Element {


    rsx! {
        document::Style { "{GLOBAL_CSS}" }

        // Your JS that defines window.initGroundMap / window.updateGroundMapMarkers
        document::Script {
            src: asset!("static/ground_map.js"),
            // defer: true,
        }

        document::Link {
            rel: "stylesheet",
            href: asset!("static/vendor/leaflet/leaflet.css"),
        }

        // Leaflet JS (must come before ground_map.js)
        document::Script {
            src: asset!("static/vendor/leaflet/leaflet.js"),
            // defer: true,
        }


        // document::
        div {
            style: "min-height: 100vh; width: 100%; background: #020617; color: #e5e7eb;",
            Router::<Route> {}
        }
    }
}

#[component]
pub fn Root() -> Element {
    // Web builds: do NOT navigate anywhere (keeps the browser URL unchanged).
    #[cfg(target_arch = "wasm32")]
    {
        UrlConfig::set_base_url("".to_string()); // same-origin
        return rsx! { Dashboard {} };
    }

    // Native builds:
    // - If a base URL is already saved, go straight to dashboard.
    // - Otherwise go to /connect.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let nav = use_navigator();
        use_effect(move || match UrlConfig::_get_base_url() {
            Some(u) if !u.trim().is_empty() => {
                let _ = nav.replace(Route::Dashboard {});
            }
            _ => {
                let _ = nav.replace(Route::Connect {});
            }
        });

        rsx! { div {} }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[component]
pub fn Connect() -> Element {
    let mut url = use_signal(|| {
        UrlConfig::_get_base_url().unwrap_or_else(|| "http://localhost:3000".to_string())
    });
    let nav = use_navigator();

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
                    value: "{url()}",
                    oninput: move |evt| url.set(evt.value()),
                }

                div { style: "display:flex; gap:12px; margin-top:16px; justify-content:flex-end;",
                    button {
                        style: "padding:10px 14px; border-radius:12px; border:1px solid #334155; background:#111827; color:#e5e7eb; cursor:pointer;",
                        onclick: move |_| {
                            UrlConfig::set_base_url(url());
                            nav.replace(Route::Dashboard {});
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
