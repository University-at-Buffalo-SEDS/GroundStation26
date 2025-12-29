// frontend/src/telemetry_dashboard/mod.rs

mod actions_tab;
mod chart;
pub mod data_tab;
pub mod errors_tab;
mod gps;
pub mod map_tab;
pub mod state_tab;
pub mod warnings_tab;

use crate::telemetry_dashboard::actions_tab::ActionsTab;
use data_tab::DataTab;
use dioxus::prelude::*;
use dioxus_signals::Signal;
use errors_tab::ErrorsTab;
use groundstation_shared::{FlightState, TelemetryRow};
use map_tab::MapTab;
use serde::Deserialize;
use state_tab::StateTab;
use warnings_tab::WarningsTab;

// Matches your existing schema. (ty + data)
#[derive(Deserialize, Debug)]
#[serde(tag = "ty", content = "data")]
enum WsInMsg {
    Telemetry(TelemetryRow),
    FlightState(FlightStateMsg),
    Warning(AlertMsg),
    Error(AlertMsg),
}

#[derive(Deserialize, Debug, Clone)]
struct FlightStateMsg {
    state: FlightState,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AlertMsg {
    pub timestamp_ms: i64,
    pub message: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainTab {
    State,
    Map,
    Actions,
    Warnings,
    Errors,
    Data,
}

macro_rules! log {
    ($($t:tt)*) => {{
        let s = format!($($t)*);
        crate::telemetry_dashboard::log(&s);
    }}
}

pub const HISTORY_MS: i64 = 60_000 * 20; // 20 minutes
const _WARNING_ACK_STORAGE_KEY: &str = "gs_last_warning_ack_ts";
const _ERROR_ACK_STORAGE_KEY: &str = "gs_last_error_ack_ts";
const _MAIN_TAB_STORAGE_KEY: &str = "gs_main_tab";
const _DATA_TAB_STORAGE_KEY: &str = "gs_data_tab";

// --------------------------
// localStorage helpers (web)
// --------------------------
#[cfg(target_arch = "wasm32")]
fn storage_get_i64(key: &str) -> Option<i64> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok().flatten()?;
    let s = storage.get_item(key).ok().flatten()?;
    s.parse().ok()
}

#[cfg(target_arch = "wasm32")]
fn storage_set_i64(key: &str, val: i64) {
    if let Some(window) = web_sys::window()
        && let Ok(Some(storage)) = window.local_storage()
    {
        let _ = storage.set_item(key, &val.to_string());
    }
}

#[cfg(target_arch = "wasm32")]
fn storage_get_string(key: &str) -> Option<String> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok().flatten()?;
    storage.get_item(key).ok().flatten()
}

#[cfg(target_arch = "wasm32")]
fn storage_set_string(key: &str, val: &str) {
    if let Some(window) = web_sys::window()
        && let Ok(Some(storage)) = window.local_storage()
    {
        let _ = storage.set_item(key, val);
    }
}

fn _main_tab_to_str(tab: MainTab) -> &'static str {
    match tab {
        MainTab::State => "state",
        MainTab::Map => "map",
        MainTab::Actions => "actions",
        MainTab::Warnings => "warnings",
        MainTab::Errors => "errors",
        MainTab::Data => "data",
    }
}

fn _main_tab_from_str(s: &str) -> MainTab {
    match s {
        "state" => MainTab::State,
        "map" => MainTab::Map,
        "actions" => MainTab::Actions,
        "warnings" => MainTab::Warnings,
        "errors" => MainTab::Errors,
        "data" => MainTab::Data,
        _ => MainTab::State,
    }
}

// ---------- Base URL config (global, simple) ----------
pub struct UrlConfig;

impl UrlConfig {
    pub fn set_base_url(url: String) {
        *BASE_URL.write() = url;
    }

    pub fn _get_base_url() -> Option<String> {
        let v = BASE_URL.read().clone();
        if v.is_empty() { None } else { Some(v) }
    }

    pub fn base_http() -> String {
        // "" means same-origin for web; native should store full url
        BASE_URL.read().clone()
    }

    pub fn base_ws() -> String {
        let base = BASE_URL.read().clone();

        // Web: compute from window.location if base is empty
        #[cfg(target_arch = "wasm32")]
        if base.is_empty() {
            if let Some(window) = web_sys::window() {
                let loc = window.location();
                let protocol = loc.protocol().unwrap_or_else(|_| "http:".to_string());
                let host = loc.host().unwrap_or_else(|_| "localhost:3000".to_string());
                let ws_scheme = if protocol == "https:" { "wss" } else { "ws" };
                return format!("{ws_scheme}://{host}");
            }
        }

        // Native (or web with explicit URL):
        // accept http(s)://host:port and convert to ws(s)://host:port
        if base.starts_with("https://") {
            base.replacen("https://", "wss://", 1)
        } else if base.starts_with("http://") {
            base.replacen("http://", "ws://", 1)
        } else {
            // fallback: assume user typed host:port
            format!("ws://{base}")
        }
    }
}

static BASE_URL: GlobalSignal<String> = Signal::global(String::new);

// ---------- Cross-platform WS handle ----------
#[derive(Clone)]
struct WsSender {
    #[cfg(target_arch = "wasm32")]
    ws: web_sys::WebSocket,

    #[cfg(not(target_arch = "wasm32"))]
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl WsSender {
    fn send_cmd(&self, cmd: &str) {
        let msg = format!(r#"{{"cmd":"{}"}}"#, cmd);

        #[cfg(target_arch = "wasm32")]
        {
            let _ = self.ws.send_with_str(&msg);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = self.tx.send(msg);
        }
    }
}

static WS_SENDER: GlobalSignal<Option<WsSender>> = Signal::global(|| None::<WsSender>);

// ---------- Public root component ----------
#[component]
pub fn TelemetryDashboard() -> Element {
    // data
    let rows = use_signal(Vec::<TelemetryRow>::new);
    let active_data_tab = use_signal(|| "GYRO_DATA".to_string());

    let warnings = use_signal(Vec::<AlertMsg>::new);
    let errors = use_signal(Vec::<AlertMsg>::new);

    let flight_state = use_signal(|| FlightState::Startup);

    // main tabs
    let active_main_tab = use_signal(|| MainTab::State);

    // ack timestamps
    let ack_warning_ts = use_signal(|| 0_i64);
    let ack_error_ts = use_signal(|| 0_i64);

    // flashing indicator
    let flash_on = use_signal(|| false);

    // gps extracted from telemetry rows
    let rocket_gps = use_signal(|| None::<(f64, f64)>);
    let user_gps = use_signal(|| None::<(f64, f64)>);
    use_effect({
        let user_gps = user_gps.clone();
        move || {
            gps::start_gps_updates(user_gps);
        }
    });

    // ----------------------------------------
    // Web-only: restore persisted UI state
    // ----------------------------------------
    #[cfg(target_arch = "wasm32")]
    {
        // restore ack timestamps
        {
            let mut ack_warning_ts = ack_warning_ts;
            let mut ack_error_ts = ack_error_ts;
            use_effect(move || {
                if let Some(v) = storage_get_i64(_WARNING_ACK_STORAGE_KEY) {
                    ack_warning_ts.set(v);
                }
                if let Some(v) = storage_get_i64(_ERROR_ACK_STORAGE_KEY) {
                    ack_error_ts.set(v);
                }
            });
        }

        // restore active main tab
        {
            let mut active_main_tab = active_main_tab;
            use_effect(move || {
                if let Some(s) = storage_get_string(_MAIN_TAB_STORAGE_KEY) {
                    active_main_tab.set(_main_tab_from_str(&s));
                }
            });
        }

        // persist active main tab when it changes
        {
            let active_main_tab = active_main_tab;
            use_effect(move || {
                let s = _main_tab_to_str(*active_main_tab.read());
                storage_set_string(_MAIN_TAB_STORAGE_KEY, s);
            });
        }

        // restore inner data tab
        {
            let mut active_data_tab = active_data_tab;
            use_effect(move || {
                if let Some(s) = storage_get_string(_DATA_TAB_STORAGE_KEY)
                    && !s.is_empty()
                {
                    active_data_tab.set(s);
                }
            });
        }

        // persist inner data tab
        {
            let active_data_tab = active_data_tab;
            use_effect(move || {
                storage_set_string(_DATA_TAB_STORAGE_KEY, &active_data_tab.read());
            });
        }
    }

    // ----------------------------------------
    // Flash loop (both web + native)
    // ----------------------------------------
    {
        let mut flash_on = flash_on;
        use_effect(move || {
            spawn(async move {
                loop {
                    // dioxus::prelude::sleep is available in modern dioxus; if you don't have it,
                    // replace with tokio::time::sleep on native and gloo_timers on wasm.
                    #[cfg(target_arch = "wasm32")]
                    use gloo_timers::future::TimeoutFuture;

                    #[cfg(target_arch = "wasm32")]
                    TimeoutFuture::new(500).await;
                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let next = !*flash_on.read();
                    flash_on.set(next);
                }
            });
        });
    }

    // ----------------------------------------
    // Derived state: counts + unacked + border
    // ----------------------------------------
    let warn_count = warnings.read().len();
    let err_count = errors.read().len();

    let latest_warning_ts = warnings
        .read()
        .iter()
        .map(|w| w.timestamp_ms)
        .max()
        .unwrap_or(0);

    let latest_error_ts = errors
        .read()
        .iter()
        .map(|e| e.timestamp_ms)
        .max()
        .unwrap_or(0);

    let has_warnings = warn_count > 0;
    let has_errors = err_count > 0;

    let has_unacked_warnings = latest_warning_ts > 0 && latest_warning_ts > *ack_warning_ts.read();
    let has_unacked_errors = latest_error_ts > 0 && latest_error_ts > *ack_error_ts.read();

    let border_style = if has_unacked_errors && *flash_on.read() {
        "2px solid #ef4444"
    } else if has_unacked_errors && has_errors {
        "1px solid #ef4444"
    } else {
        "1px solid transparent"
    };

    // ----------------------------------------
    // Initial flightstate (HTTP)
    // ----------------------------------------
    {
        let mut flight_state = flight_state;
        use_effect(move || {
            spawn(async move {
                if let Ok(state) = http_get_json::<FlightState>("/flightstate").await {
                    flight_state.set(state);
                }
            });
        });
    }

    // ----------------------------------------
    // WebSocket connect once
    // ----------------------------------------
    {
        use_effect(move || {
            spawn(async move {
                if let Err(e) =
                    connect_ws_loop(rows, warnings, errors, flight_state, rocket_gps, user_gps)
                        .await
                {
                    log!("ws loop ended: {e:?}");
                }
            });
        });
    }

    // ----------------------------------------
    // Top nav button styles (old vibe)
    // ----------------------------------------
    let tab_style_active = |color: &str| {
        format!(
            "padding:0.4rem 0.8rem; border-radius:0.5rem;\
             border:1px solid {color}; background:#111827;\
             color:{color}; cursor:pointer;"
        )
    };
    let tab_style_inactive = "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                             border:1px solid #4b5563; background:#020617;\
                             color:#e5e7eb; cursor:pointer;";

    // ----------------------------------------
    // MAIN UI (Leptos-like shell)
    // ----------------------------------------
    rsx! {
        div {
            style: "
                min-height:100vh;
                padding:24px;
                color:#e5e7eb;
                font-family:system-ui, -apple-system, BlinkMacSystemFont;
                background:#020617;
                display:flex;
                flex-direction:column;
                border:{border_style};
                box-sizing:border-box;
            ",

            // Header row: title (left) + centered nav card
            div { style: "display:flex; align-items:center; width:100%; margin-bottom:12px;",
                div { style: "flex:0; min-width:200px; display:flex; align-items:center; gap:10px;",
                h1 { style: "color:#f97316; margin:0; font-size:22px; font-weight:800;", "Rocket Dashboard" }

                // Always-available ABORT
                button {
                    style: "
                            padding:0.45rem 0.85rem;
                            border-radius:0.75rem;
                            border:1px solid #ef4444;
                            background:#450a0a;
                            color:#fecaca;
                            font-weight:900;
                            cursor:pointer;
                        ",
                    onclick: move |_| send_cmd("Abort"),
                    "ABORT"
                }
            }

                // centered nav card
                div { style: "flex:1; display:flex; justify-content:center;",
                    div { style: "
                        display:flex; align-items:center; gap:0.5rem;
                        padding:0.85rem; border-radius:0.75rem;
                        background:#020617ee; border:1px solid #4b5563;
                        box-shadow:0 10px 25px rgba(0,0,0,0.45);
                        min-width:12rem;
                    ",
                        nav { style: "display:flex; gap:0.5rem; flex-wrap:wrap;",

                            button {
                                style: if *active_main_tab.read() == MainTab::State { tab_style_active("#38bdf8") } else { tab_style_inactive.to_string() },
                                onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::State) },
                                "Flight"
                            }
                            button {
                                style: if *active_main_tab.read() == MainTab::Map { tab_style_active("#22c55e") } else { tab_style_inactive.to_string() },
                                onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::Map) },
                                "Map"
                            }

                            button {
                                style: if *active_main_tab.read() == MainTab::Actions { tab_style_active("#a78bfa") } else { tab_style_inactive.to_string() },
                                onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::Actions) },
                                "Actions"
                            }

                            button {
                                style: if *active_main_tab.read() == MainTab::Warnings {
                                    // yellow when active
                                    tab_style_active("#facc15")
                                } else {
                                    // inactive, but we still show icon if warnings exist
                                    tab_style_inactive.to_string()
                                },
                                onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::Warnings) },
                                span { "Warnings" }
                                if has_warnings {
                                    span {
                                        style: {
                                            if has_unacked_warnings && *flash_on.read() {
                                                "margin-left:6px; color:#facc15; opacity:1;".to_string()
                                            } else if has_unacked_warnings {
                                                "margin-left:6px; color:#facc15; opacity:0.4;".to_string()
                                            } else {
                                                "margin-left:6px; color:#9ca3af; opacity:1;".to_string()
                                            }
                                        },
                                        "⚠"
                                    }
                                }
                            }

                            button {
                                style: if *active_main_tab.read() == MainTab::Errors {
                                    tab_style_active("#ef4444")
                                } else {
                                    tab_style_inactive.to_string()
                                },
                                onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::Errors) },
                                span { "Errors" }
                                if has_errors {
                                    span {
                                        style: {
                                            if has_unacked_errors && *flash_on.read() {
                                                "margin-left:6px; color:#fecaca; opacity:1;".to_string()
                                            } else if has_unacked_errors {
                                                "margin-left:6px; color:#fecaca; opacity:0.4;".to_string()
                                            } else {
                                                "margin-left:6px; color:#9ca3af; opacity:1;".to_string()
                                            }
                                        },
                                        "⛔"
                                    }
                                }
                            }

                            button {
                                style: if *active_main_tab.read() == MainTab::Data { tab_style_active("#f97316") } else { tab_style_inactive.to_string() },
                                onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::Data) },
                                "Data"
                            }
                        }
                    }
                }

                // right spacer (keeps nav centered)
                div { style: "flex:0; min-width:200px;" }
            }

            // Status pill row
            div { style: "margin-bottom:12px; display:flex; gap:12px; align-items:center;",
                div { style: "
                    display:flex; align-items:center; gap:0.75rem;
                    padding:0.35rem 0.7rem; border-radius:999px;
                    background:#111827; border:1px solid #4b5563;
                ",
                    span { style: "color:#9ca3af;", "Status:" }

                    if !has_warnings && !has_errors {
                        span { style: "color:#22c55e; font-weight:600;", "Nominal" }
                        span { style: "color:#93c5fd; margin-left:0.75rem;",
                            "(Flight state: ",
                            "{flight_state.read().to_string()}",
                            ")"
                        }
                    } else {
                        if has_errors {
                            span { style: "color:#fecaca;", {format!("{err_count} error(s)")} }
                        }
                        if has_warnings {
                            span { style: "color:#fecaca;", {format!("{warn_count} warnings(s)")} }
                        }

                        span { style: "color:#93c5fd; margin-left:0.75rem;",
                            "(Flight state: ",
                            "{flight_state.read().to_string()}",
                            ")"
                        }

                        // Ack buttons only when you're on that tab (old behavior)
                        if *active_main_tab.read() == MainTab::Warnings && has_warnings {
                            button {
                                style: "
                                    margin-left:auto;
                                    padding:0.25rem 0.7rem;
                                    border-radius:999px;
                                    border:1px solid #4b5563;
                                    background:#020617;
                                    color:#e5e7eb;
                                    font-size:0.75rem;
                                    cursor:pointer;
                                ",
                                onclick: {
                                    let mut ack_warning_ts = ack_warning_ts;
                                    move |_| {
                                        let ts = latest_warning_ts;
                                        ack_warning_ts.set(ts);
                                        #[cfg(target_arch = "wasm32")]
                                        storage_set_i64(_WARNING_ACK_STORAGE_KEY, ts);
                                    }
                                },
                                "Acknowledge warnings"
                            }
                        }

                        if *active_main_tab.read() == MainTab::Errors && has_errors {
                            button {
                                style: "
                                    margin-left:auto;
                                    padding:0.25rem 0.7rem;
                                    border-radius:999px;
                                    border:1px solid #4b5563;
                                    background:#020617;
                                    color:#e5e7eb;
                                    font-size:0.75rem;
                                    cursor:pointer;
                                ",
                                onclick: {
                                    let mut ack_error_ts = ack_error_ts;
                                    move |_| {
                                        let ts = latest_error_ts;
                                        ack_error_ts.set(ts);
                                        #[cfg(target_arch = "wasm32")]
                                        storage_set_i64(_ERROR_ACK_STORAGE_KEY, ts);
                                    }
                                },
                                "Acknowledge errors"
                            }
                        }
                    }
                }
            }

            // Main body
            div { style: "flex:1; min-height:0;",
                match *active_main_tab.read() {
                    MainTab::State => rsx! {
                        StateTab { flight_state: flight_state }
                    },
                    MainTab::Map => rsx! {
                        MapTab { rocket_gps: rocket_gps, user_gps: user_gps }
                    },
                    MainTab::Actions => rsx! {
                        ActionsTab {}
                    },
                    MainTab::Warnings => rsx! {
                        WarningsTab { warnings: warnings }
                    },
                    MainTab::Errors => rsx! {
                        ErrorsTab { errors: errors }
                    },
                    MainTab::Data => rsx! {
                        DataTab {
                            rows: rows,
                            active_tab: active_data_tab
                        }
                    },
                }
            }
        }
    }
}

fn send_cmd(cmd: &str) {
    if let Some(sender) = WS_SENDER.read().clone() {
        sender.send_cmd(cmd);
    }
}

// --------- Extract GPS points ----------
fn row_to_gps(row: &TelemetryRow) -> Option<(f64, f64)> {
    let is_gps_type = matches!(row.data_type.as_str(), "GPS" | "GPS_DATA" | "ROCKET_GPS");
    if !is_gps_type {
        return None;
    }
    Some((row.v0? as f64, row.v1? as f64))
}

// ---------- Web vs Native logging ----------
fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());

    #[cfg(not(target_arch = "wasm32"))]
    println!("{msg}");
}

// ---------- HTTP helpers ----------
#[cfg(target_arch = "wasm32")]
async fn http_get_json<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
    use gloo_net::http::Request;
    let base = UrlConfig::base_http();
    let url = if base.is_empty() {
        path.to_string()
    } else {
        format!("{base}{path}")
    };
    Request::get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<T>()
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
async fn http_get_json<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
    let base = UrlConfig::base_http();
    let url = if base.is_empty() {
        format!("http://localhost:3000{path}")
    } else {
        format!("{base}{path}")
    };
    reqwest::get(url)
        .await
        .map_err(|e| e.to_string())?
        .json::<T>()
        .await
        .map_err(|e| e.to_string())
}

// ---------- WebSocket loop ----------
async fn connect_ws_loop(
    rows: Signal<Vec<TelemetryRow>>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    flight_state: Signal<FlightState>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;
        use web_sys::{MessageEvent, WebSocket};

        let base_ws = UrlConfig::base_ws();
        let ws_url = format!("{base_ws}/ws");

        let ws = WebSocket::new(&ws_url).map_err(|_| "failed to create websocket".to_string())?;
        *WS_SENDER.write() = Some(WsSender { ws: ws.clone() });

        let onmessage = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
            if let Some(s) = e.data().as_string() {
                handle_ws_message(
                    &s,
                    rows,
                    warnings,
                    errors,
                    flight_state,
                    rocket_gps,
                    user_gps,
                );
            }
        });

        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use futures_util::{SinkExt, StreamExt};

        let base_ws = UrlConfig::base_ws();
        let ws_url = format!("{base_ws}/ws");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        *WS_SENDER.write() = Some(WsSender { tx });

        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url.as_str())
            .await
            .map_err(|e| e.to_string())?;

        let (mut write, mut read) = ws_stream.split();

        // writer task
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let _ = write
                    .send(tokio_tungstenite::tungstenite::Message::Text(msg.into()))
                    .await;
            }
        });

        // reader loop
        while let Some(item) = read.next().await {
            let msg = item.map_err(|e| e.to_string())?;
            if let tokio_tungstenite::tungstenite::Message::Text(s) = msg {
                handle_ws_message(
                    &s,
                    rows,
                    warnings,
                    errors,
                    flight_state,
                    rocket_gps,
                    user_gps,
                );
            }
        }

        let _ = writer.await;
        Ok(())
    }
}

fn handle_ws_message(
    s: &str,
    rows: Signal<Vec<TelemetryRow>>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    flight_state: Signal<FlightState>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) {
    let mut rows = rows;
    let mut warnings = warnings;
    let mut errors = errors;
    let mut flight_state = flight_state;
    let mut rocket_gps = rocket_gps;
    let _user_gps = user_gps;

    let Ok(msg) = serde_json::from_str::<WsInMsg>(s) else {
        return;
    };

    match msg {
        WsInMsg::Telemetry(row) => {
            if let Some((lat, lon)) = row_to_gps(&row) {
                rocket_gps.set(Some((lat, lon)));
            }

            let mut v = rows.read().clone();
            v.push(row);

            // Time-window trim (prefer timestamp-based)
            if let Some(last) = v.last() {
                let cutoff = last.timestamp_ms - HISTORY_MS;
                let split = v.partition_point(|r| r.timestamp_ms < cutoff);
                if split > 0 {
                    v.drain(0..split);
                }
            }

            // cheap cap as safety
            const MAX_SAMPLES: usize = 10_000;
            if v.len() > MAX_SAMPLES {
                let n = v.len();
                let stride = (n as f32 / MAX_SAMPLES as f32).ceil() as usize;
                v = v
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, row)| (i % stride == 0).then_some(row))
                    .collect();
            }

            rows.set(v);
        }

        WsInMsg::FlightState(st) => {
            flight_state.set(st.state);
        }

        WsInMsg::Warning(w) => {
            let mut v = warnings.read().clone();
            v.insert(0, w);
            if v.len() > 500 {
                v.truncate(500);
            }
            warnings.set(v);
        }

        WsInMsg::Error(e) => {
            let mut v = errors.read().clone();
            v.insert(0, e);
            if v.len() > 500 {
                v.truncate(500);
            }
            errors.set(v);
        }
    }
}
