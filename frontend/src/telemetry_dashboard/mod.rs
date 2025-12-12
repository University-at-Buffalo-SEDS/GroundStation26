use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use groundstation_shared::FlightState;
use groundstation_shared::TelemetryRow;
use leptos::__reexports::wasm_bindgen_futures;
use leptos::prelude::*;
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

mod data_tab;
mod errors_tab;
mod warnings_tab;

mod map_tab;
mod state_tab;

use map_tab::MapTab;
use state_tab::StateTab;

use data_tab::DataTab;
use errors_tab::ErrorsTab;
use serde::Deserialize;
use warnings_tab::WarningsTab;

pub const HISTORY_MS: i64 = 60_000 * 20; // 20 minutes
const WARNING_ACK_STORAGE_KEY: &str = "gs_last_warning_ack_ts";
const ERROR_ACK_STORAGE_KEY: &str = "gs_last_error_ack_ts";
const MAIN_TAB_STORAGE_KEY: &str = "gs_main_tab";
const DATA_TAB_STORAGE_KEY: &str = "gs_data_tab";

// ------------------------------------------------------------------------------------------------
// WebSocket handle (thread_local because WASM is single-threaded)
// ------------------------------------------------------------------------------------------------
thread_local! {
    pub static WS_HANDLE: RefCell<Option<WebSocket>> = const { RefCell::new(None) };
}

// ------------------------------------------------------------------------------------------------
// Incoming WebSocket messages from backend
// ------------------------------------------------------------------------------------------------
#[derive(Deserialize)]
#[serde(tag = "ty", content = "data")]
enum WsInMsg {
    Telemetry(TelemetryRow),
    FlightState(FlightStateMsg),
    Warning(WarningMsg),
    Error(ErrorMsg),
}

#[derive(Clone, Deserialize)]
struct WarningMsg {
    pub timestamp_ms: i64,
    pub message: String,
}

#[derive(Clone, Deserialize)]
struct FlightStateMsg {
    pub state: FlightState,
}

#[derive(Clone, Deserialize)]
struct ErrorMsg {
    pub timestamp_ms: i64,
    pub message: String,
}

// ------------------------------------------------------------------------------------------------
// Alerts from DB (/api/alerts)
// ------------------------------------------------------------------------------------------------
#[derive(Deserialize)]
struct AlertDto {
    pub timestamp_ms: i64,
    pub severity: String, // "warning" or "error"
    pub message: String,
}

// ------------------------------------------------------------------------------------------------
// GPS API response (/api/gps) – adjust to match your backend
// ------------------------------------------------------------------------------------------------
#[derive(Clone, Deserialize)]
struct GpsResponse {
    pub rocket_lat: f64,
    pub rocket_lon: f64,
    pub user_lat: f64,
    pub user_lon: f64,
}

#[derive(Clone, Copy)]
struct GpsPoint {
    pub lat: f64,
    pub lon: f64,
}

/// Extract rocket GPS from a TelemetryRow, if this row represents GPS data.
///
/// Assumes:
/// - `data_type` is something like "GPS" or "GPS_DATA"
/// - `v0 = latitude (deg)`, `v1 = longitude (deg)`
fn row_to_gps(row: &TelemetryRow) -> Option<GpsPoint> {
    // Adjust these matches to whatever you actually use in the backend
    let is_gps_type = matches!(row.data_type.as_str(), "GPS" | "GPS_DATA" | "ROCKET_GPS");

    if !is_gps_type {
        return None;
    }

    let lat = row.v0?;
    let lon = row.v1?;

    Some(GpsPoint {
        lat: lat as f64,
        lon: lon as f64,
    })
}

// ------------------------------------------------------------------------------------------------
// Action commands
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct CommandDef {
    pub label: &'static str,
    pub cmd: &'static str,
    pub style: &'static str,
}

pub const COMMANDS: &[CommandDef] = &[
    CommandDef {
        label: "Arm",
        cmd: "Arm",
        style: "border:1px solid #22c55e; background:#022c22; color:#bbf7d0;",
    },
    CommandDef {
        label: "Disarm",
        cmd: "Disarm",
        style: "border:1px solid #ef4444; background:#450a0a; color:#fecaca;",
    },
    CommandDef {
        label: "Abort",
        cmd: "Abort",
        style: "border:1px solid #ef4444; background:#450a0a; color:#fecaca;",
    },
];

// ------------------------------------------------------------------------------------------------
// Actions panel UI
// ------------------------------------------------------------------------------------------------
#[component]
pub fn ActionsPanel() -> impl IntoView {
    view! {
        <div
            style="
                position:fixed;
                right:1rem;
                top:11.25%;
                transform:translateY(-50%);
                display:flex;
                flex-direction:column;
                gap:0.5rem;
                background:#020617ee;
                padding:0.85rem;
                border-radius:0.75rem;
                border:1px solid #4b5563;
                box-shadow:0 10px 25px rgba(0,0,0,0.45);
                backdrop-filter:blur(6px);
                min-width: 9rem;
                z-index: 9999;
            "
        >
            <div
                style="
                    font-size:0.75rem;
                    text-transform:uppercase;
                    letter-spacing:0.08em;
                    color:#9ca3af;
                    margin-bottom:0.5rem;
                    border-bottom:1px solid #4b5563;
                    padding-bottom:0.25rem;
                "
            >
                "Actions"
            </div>

            {
                COMMANDS
                    .iter()
                    .map(|c| {
                        let label = c.label;
                        let cmd = c.cmd;
                        let style = c.style;
                        view! {
                            <button
                                style=format!(
                                    "padding:0.4rem 0.8rem; border-radius:0.5rem; \
                                     cursor:pointer; width:100%; text-align:center; {}",
                                    style
                                )
                                on:click=move |_| send_cmd(cmd)
                            >
                                {label}
                            </button>
                        }
                    })
                    .collect_view()
            }
        </div>
    }
}

// ------------------------------------------------------------------------------------------------
// Build WebSocket URL based on current page
// ------------------------------------------------------------------------------------------------
fn make_ws_url() -> String {
    let window = web_sys::window().expect("no global `window`");
    let location = window.location();

    let protocol = location.protocol().unwrap_or_else(|_| "http:".to_string());
    let host = location
        .host()
        .unwrap_or_else(|_| "localhost:3000".to_string());

    let ws_scheme = if protocol == "https:" { "wss" } else { "ws" };

    format!("{ws_scheme}://{host}/ws")
}

// ------------------------------------------------------------------------------------------------
// Send command through WS
// ------------------------------------------------------------------------------------------------
pub(super) fn send_cmd(cmd: &str) {
    let msg = format!(r#"{{"cmd":"{}"}}"#, cmd);
    WS_HANDLE.with(|cell| {
        if let Some(ws) = cell.borrow().as_ref()
            && let Err(err) = ws.send_with_str(&msg)
        {
            web_sys::console::error_1(&err);
        }
    });
}

// ------------------------------------------------------------------------------------------------
// Warning / Error row types used by UI
// ------------------------------------------------------------------------------------------------
#[derive(Clone)]
pub struct WarningRow {
    pub timestamp_ms: i64,
    pub message: String,
}

#[derive(Clone)]
pub struct ErrorRow {
    pub timestamp_ms: i64,
    pub message: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainTab {
    State,
    Map,
    Warnings,
    Errors,
    Data,
}

// helpers for localStorage <-> MainTab
fn main_tab_to_str(tab: MainTab) -> &'static str {
    match tab {
        MainTab::State => "state",
        MainTab::Map => "map",
        MainTab::Warnings => "warnings",
        MainTab::Errors => "errors",
        MainTab::Data => "data",
    }
}

fn main_tab_from_str(s: &str) -> MainTab {
    match s {
        "state" => MainTab::State,
        "map" => MainTab::Map,
        "warnings" => MainTab::Warnings,
        "errors" => MainTab::Errors,
        "data" => MainTab::Data,
        _ => MainTab::State,
    }
}

// ------------------------------------------------------------------------------------------------
// MAIN UI COMPONENT
// ------------------------------------------------------------------------------------------------
#[component]
pub fn TelemetryDashboard() -> impl IntoView + 'static {
    // --- telemetry data ---
    let (rows, set_rows) = signal(Vec::<TelemetryRow>::new());

    // --- tab selection inside the DataTab (GYRO_DATA, etc.) ---
    let (active_tab, set_active_tab) = signal("GYRO_DATA".to_string());

    // --- MAIN TABS (State / Map / Warnings / Errors / Data) ---
    let (active_main_tab, set_active_main_tab) = signal(MainTab::State);

    // --- ALL warnings + errors (newest first) ---
    let (warnings, set_warnings) = signal(Vec::<WarningRow>::new());
    let (errors, set_errors) = signal(Vec::<ErrorRow>::new());

    // --- Last acknowledged timestamps (client-side) ---
    let (ack_warning_ts, set_ack_warning_ts) = signal(0_i64);
    let (ack_error_ts, set_ack_error_ts) = signal(0_i64);

    // --- Current flight state ---
    let (flight_state, set_flight_state) = signal(FlightState::Startup);
    let flight_state_str = Signal::derive(move || flight_state.get().to_string());

    // --- GPS positions ---
    let (rocket_gps, set_rocket_gps) = signal(None::<GpsPoint>);
    let (user_gps, set_user_gps) = signal(None::<GpsPoint>);

    // --------------------------------------------------------------------------------------------
    // INITIAL `/flightstate` LOAD (current flight state from backend)
    // --------------------------------------------------------------------------------------------
    Effect::new({
        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                let Ok(resp) = Request::get("/flightstate").send().await else {
                    return;
                };

                if !resp.ok() {
                    return;
                }

                let Ok(state) = resp.json::<FlightState>().await else {
                    return;
                };

                set_flight_state.set(state);
            });
        }
    });

    // Load acknowledged timestamps from localStorage on mount
    Effect::new({
        move |_| {
            let Some(window) = web_sys::window() else {
                return;
            };
            let Ok(Some(storage)) = window.local_storage() else {
                return;
            };

            if let Ok(Some(val)) = storage.get_item(WARNING_ACK_STORAGE_KEY)
                && let Ok(parsed) = val.parse::<i64>()
            {
                set_ack_warning_ts.set(parsed);
            }

            if let Ok(Some(val)) = storage.get_item(ERROR_ACK_STORAGE_KEY)
                && let Ok(parsed) = val.parse::<i64>()
            {
                set_ack_error_ts.set(parsed);
            }
        }
    });

    // Restore active main tab from localStorage on mount
    Effect::new({
        move |_| {
            let Some(window) = web_sys::window() else {
                return;
            };
            let Ok(Some(storage)) = window.local_storage() else {
                return;
            };

            if let Ok(Some(tab_str)) = storage.get_item(MAIN_TAB_STORAGE_KEY) {
                let tab = main_tab_from_str(&tab_str);
                set_active_main_tab.set(tab);
            }
        }
    });

    // Persist active main tab to localStorage whenever it changes
    Effect::new({
        move |_| {
            let tab = active_main_tab.get();
            if let Some(window) = web_sys::window()
                && let Ok(Some(storage)) = window.local_storage()
            {
                let _ = storage.set_item(MAIN_TAB_STORAGE_KEY, main_tab_to_str(tab));
            }
        }
    });

    // Restore inner data tab (active_tab) from localStorage on mount
    Effect::new({
        move |_| {
            let Some(window) = web_sys::window() else {
                return;
            };
            let Ok(Some(storage)) = window.local_storage() else {
                return;
            };

            if let Ok(Some(tab_str)) = storage.get_item(DATA_TAB_STORAGE_KEY)
                && !tab_str.is_empty()
            {
                set_active_tab.set(tab_str);
            }
        }
    });

    // Persist inner data tab whenever it changes
    Effect::new({
        move |_| {
            let tab = active_tab.get();
            if let Some(window) = web_sys::window()
                && let Ok(Some(storage)) = window.local_storage()
            {
                let _ = storage.set_item(DATA_TAB_STORAGE_KEY, &tab);
            }
        }
    });

    // --------------------------------------------------------------------------------------------
    // INITIAL `/api/recent` TELEMETRY LOAD
    // --------------------------------------------------------------------------------------------
    Effect::new({
        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                let Ok(resp) = Request::get("/api/recent").send().await else {
                    return;
                };

                let Ok(mut list) = resp.json::<Vec<TelemetryRow>>().await else {
                    return;
                };

                list.sort_by_key(|r| r.timestamp_ms);

                if let Some(last) = list.last() {
                    let cutoff = last.timestamp_ms - HISTORY_MS;
                    let start = list.partition_point(|r| r.timestamp_ms < cutoff);
                    if start > 0 {
                        list.drain(0..start);
                    }
                }

                // downsample for plotting
                const MAX_INIT_POINTS: usize = 5000;
                let n = list.len();
                if n > MAX_INIT_POINTS {
                    let stride = (n as f32 / MAX_INIT_POINTS as f32).ceil() as usize;
                    list = list
                        .into_iter()
                        .enumerate()
                        .filter_map(|(i, row)| (i % stride == 0).then_some(row))
                        .collect();
                }

                // Seed rocket_gps from the most recent GPS row in the history
                if let Some(gps) = list.iter().rev().find_map(row_to_gps) {
                    set_rocket_gps.set(Some(gps));
                }

                set_rows.set(list);
            });
        }
    });

    // --------------------------------------------------------------------------------------------
    // INITIAL `/api/alerts` LOAD (warnings + errors from DB)
    // --------------------------------------------------------------------------------------------
    Effect::new({
        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                let Ok(resp) = Request::get("/api/alerts?minutes=20").send().await else {
                    return;
                };

                let Ok(mut alerts) = resp.json::<Vec<AlertDto>>().await else {
                    return;
                };

                // Detect backend reset using timestamps only:
                let max_ts = alerts.iter().map(|a| a.timestamp_ms).max().unwrap_or(0);

                let prev_warn = ack_warning_ts.get_untracked();
                let prev_err = ack_error_ts.get_untracked();
                let prev_ack = prev_warn.max(prev_err);

                if prev_ack > 0 && max_ts > 0 && max_ts < prev_ack - HISTORY_MS {
                    set_ack_warning_ts.set(0);
                    set_ack_error_ts.set(0);

                    if let Some(window) = web_sys::window()
                        && let Ok(Some(storage)) = window.local_storage()
                    {
                        let _ = storage.remove_item(WARNING_ACK_STORAGE_KEY);
                        let _ = storage.remove_item(ERROR_ACK_STORAGE_KEY);
                    }
                }

                // newest first
                alerts.sort_by_key(|a| -a.timestamp_ms);

                let mut warns = Vec::<WarningRow>::new();
                let mut errs = Vec::<ErrorRow>::new();

                for a in alerts {
                    match a.severity.as_str() {
                        "warning" => warns.push(WarningRow {
                            timestamp_ms: a.timestamp_ms,
                            message: a.message,
                        }),
                        "error" => errs.push(ErrorRow {
                            timestamp_ms: a.timestamp_ms,
                            message: a.message,
                        }),
                        _ => { /* ignore unknown severity */ }
                    }
                }

                set_warnings.set(warns);
                set_errors.set(errs);
            });
        }
    });

    // --------------------------------------------------------------------------------------------
    // GPS seed on reload (/api/gps) – one-shot, not a loop
    // --------------------------------------------------------------------------------------------
    Effect::new({
        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                let Ok(resp) = Request::get("/api/gps").send().await else {
                    return;
                };

                if !resp.ok() {
                    return;
                }

                let Ok(gps) = resp.json::<GpsResponse>().await else {
                    return;
                };

                set_rocket_gps.set(Some(GpsPoint {
                    lat: gps.rocket_lat,
                    lon: gps.rocket_lon,
                }));
                set_user_gps.set(Some(GpsPoint {
                    lat: gps.user_lat,
                    lon: gps.user_lon,
                }));
            });
        }
    });

    // --------------------------------------------------------------------------------------------
    // WEBSOCKET LIVE UPDATES (Telemetry + Warning + Error)
    // --------------------------------------------------------------------------------------------
    Effect::new({
        move |_| {
            let ws_url = make_ws_url();
            web_sys::console::log_1(&format!("Connecting WebSocket to {ws_url}").into());

            let ws = WebSocket::new(&ws_url).unwrap();

            WS_HANDLE.with(|cell| {
                *cell.borrow_mut() = Some(ws.clone());
            });

            // Buffer for telemetry only
            let pending: Rc<RefCell<Vec<TelemetryRow>>> = Rc::new(RefCell::new(Vec::new()));

            let pending_for_ws = pending.clone();
            let handler = {
                Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                    if let Some(text) = event.data().as_string() {
                        match serde_json::from_str::<WsInMsg>(&text) {
                            Ok(WsInMsg::Telemetry(row)) => {
                                // Update rocket_gps if this row is GPS
                                if let Some(gps) = row_to_gps(&row) {
                                    set_rocket_gps.set(Some(gps));
                                }

                                pending_for_ws.borrow_mut().push(row);
                            }
                            Ok(WsInMsg::Warning(w)) => {
                                set_warnings.update(|v| {
                                    v.insert(
                                        0,
                                        WarningRow {
                                            timestamp_ms: w.timestamp_ms,
                                            message: w.message,
                                        },
                                    );
                                });
                            }
                            Ok(WsInMsg::Error(e)) => {
                                set_errors.update(|v| {
                                    v.insert(
                                        0,
                                        ErrorRow {
                                            timestamp_ms: e.timestamp_ms,
                                            message: e.message,
                                        },
                                    );
                                });
                            }
                            Ok(WsInMsg::FlightState(fs)) => {
                                set_flight_state.set(fs.state);
                            }
                            Err(e) => {
                                web_sys::console::error_1(&format!("WS parse error: {e}").into());
                            }
                        }
                    }
                })
            };

            ws.set_onmessage(Some(handler.as_ref().unchecked_ref()));
            handler.forget();

            // tick loop for telemetry (unchanged)
            let pending_for_tick = pending.clone();
            let set_rows_for_tick = set_rows;

            wasm_bindgen_futures::spawn_local(async move {
                const FRAME_MS: u32 = 66;

                loop {
                    TimeoutFuture::new(FRAME_MS).await;

                    let mut buf = pending_for_tick.borrow_mut();
                    if buf.is_empty() {
                        continue;
                    }

                    let mut batch = Vec::new();
                    std::mem::swap(&mut *buf, &mut batch);

                    set_rows_for_tick.update(|v| {
                        v.extend(batch);

                        if let Some(last) = v.last() {
                            let cutoff = last.timestamp_ms - HISTORY_MS;
                            let split = v.partition_point(|r| r.timestamp_ms < cutoff);
                            if split > 0 {
                                v.drain(0..split);
                            }
                        }

                        const MAX_SAMPLES: usize = 10_000;
                        if v.len() > MAX_SAMPLES {
                            let n = v.len();
                            let stride = (n as f32 / MAX_SAMPLES as f32).ceil() as usize;
                            *v = v
                                .iter()
                                .cloned()
                                .enumerate()
                                .filter_map(
                                    |(i, row)| if i % stride == 0 { Some(row) } else { None },
                                )
                                .collect();
                        }
                    });
                }
            });
        }
    });

    // --------------------------------------------------------------------------------------------
    // Flashing border + warning/error counts
    // --------------------------------------------------------------------------------------------
    let warn_count = Signal::derive(move || warnings.get().len());
    let err_count = Signal::derive(move || errors.get().len());
    let has_warnings = Signal::derive(move || warn_count.get() > 0);
    // latest timestamps, for unacknowledged logic
    let latest_warning_ts = Signal::derive({
        move || {
            warnings
                .get()
                .iter()
                .map(|w| w.timestamp_ms)
                .max()
                .unwrap_or(0)
        }
    });

    let latest_error_ts = Signal::derive({
        move || {
            errors
                .get()
                .iter()
                .map(|e| e.timestamp_ms)
                .max()
                .unwrap_or(0)
        }
    });

    let has_errors = Signal::derive(move || err_count.get() > 0);

    // unacknowledged warnings: there is at least one warning newer than the ack timestamp
    let has_unacked_warnings = Signal::derive({
        move || {
            let latest = latest_warning_ts.get();
            let ack = ack_warning_ts.get();
            latest > 0 && latest > ack
        }
    });

    // unacknowledged errors: same idea
    let has_unacked_errors = Signal::derive({
        move || {
            let latest = latest_error_ts.get();
            let ack = ack_error_ts.get();
            latest > 0 && latest > ack
        }
    });

    let (flash_on, set_flash_on) = signal(false);
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                TimeoutFuture::new(500).await;
                set_flash_on.update(|v| *v = !*v);
            }
        });
    });

    // Border only flashes for *unacknowledged* errors
    let border_style = Signal::derive({
        move || {
            if has_unacked_errors.get() && flash_on.get() {
                "2px solid #ef4444"
            } else if has_errors.get() && has_unacked_errors.get() {
                "1px solid #ef4444"
            } else {
                "1px solid transparent"
            }
        }
    });

    // --------------------------------------------------------------------------------------------
    // MAIN UI
    // --------------------------------------------------------------------------------------------
    view! {
                <div
                    style=move || format!(
                        "min-height:100vh; padding:1.5rem; color:#e5e7eb;\
                         font-family:system-ui, -apple-system, BlinkMacSystemFont;\
                         background:#020617; display:flex; flex-direction:column;\
                         border:{}; box-sizing:border-box;",
                        border_style.get()
                    )
                >
                    <div style="display:flex; align-items:center; width:100%; margin-bottom:0.75rem;">
                        <div style="flex:0; min-width:200px;">
                            <h1 style="color:#f97316; margin:0;">"Rocket Dashboard"</h1>
                        </div>

                        <div style="flex:1; display:flex; justify-content:center;">
                            <div style="
                        display:flex; align-items:center; gap:0.5rem;
                        padding:0.85rem; border-radius:0.75rem;
                        background:#020617ee; border:1px solid #4b5563;
                        box-shadow:0 10px 25px rgba(0,0,0,0.45);
                        min-width:12rem;">
                                <nav style="display:flex; gap:0.5rem; flex-wrap:wrap;">

                                    // --- Flight / State tab ---
                                    <button
                                        style=move || {
                                            if active_main_tab.get() == MainTab::State {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #38bdf8; background:#111827;\
                                                 color:#38bdf8; cursor:pointer;"
                                            } else {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #4b5563; background:#020617;\
                                                 color:#e5e7eb; cursor:pointer;"
                                            }
                                        }
                                        on:click=move |_| set_active_main_tab.set(MainTab::State)
                                    >
                                        "Flight"
                                    </button>

                                    // --- Map tab (new) ---
                                    <button
                                        style=move || {
                                            if active_main_tab.get() == MainTab::Map {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #22c55e; background:#111827;\
                                                 color:#22c55e; cursor:pointer;"
                                            } else {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #4b5563; background:#020617;\
                                                 color:#e5e7eb; cursor:pointer;"
                                            }
                                        }
                                        on:click=move |_| set_active_main_tab.set(MainTab::Map)
                                    >
                                        "Map"
                                    </button>

                                    // --- Warnings tab ---
                                    <button
                                        style=move || {
                                            if active_main_tab.get() == MainTab::Warnings {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #facc15; background:#111827;\
                                                 color:#facc15; cursor:pointer; display:flex;\
                                                 align-items:center; gap:0.35rem;"
                                            } else {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #4b5563; background:#020617;\
                                                 color:#e5e7eb; cursor:pointer; display:flex;\
                                                 align-items:center; gap:0.35rem;"
                                            }
                                        }
                                        on:click=move |_| set_active_main_tab.set(MainTab::Warnings)
                                    >
                                        <span>"Warnings"</span>
                                        <Show when=move || has_warnings.get()>
                                            {move || {
                                                let flash = flash_on.get();
                                                let unacked = has_unacked_warnings.get();
                                                view! {
                                                    <span style=move || {
                                                        if unacked && flash {
                                                            "color:#facc15; opacity:1;"
                                                        } else if unacked {
                                                            "color:#facc15; opacity:0.4;"
                                                        } else {
                                                            // acknowledged warnings: no flashing, dimmer icon
                                                            "color:#9ca3af; opacity:1;"
                                                        }
                                                    }>
                                                        "⚠"
                                                    </span>
                                                }
                                            }}
                                        </Show>
                                    </button>

                                    // --- Errors tab ---
                                    <button
                                        style=move || {
                                            if active_main_tab.get() == MainTab::Errors {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #ef4444; background:#111827;\
                                                 color:#ef4444; cursor:pointer; display:flex;\
                                                 align-items:center; gap:0.35rem;"
                                            } else {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #4b5563; background:#020617;\
                                                 color:#e5e7eb; cursor:pointer; display:flex;\
                                                 align-items:center; gap:0.35rem;"
                                            }
                                        }
                                        on:click=move |_| set_active_main_tab.set(MainTab::Errors)
                                    >
                                        <span>"Errors"</span>
                                        <Show when=move || has_errors.get()>
                                            {move || {
                                                let flash = flash_on.get();
                                                let unacked = has_unacked_errors.get();
                                                view! {
                                                    <span style=move || {
                                                        if unacked && flash {
                                                            "color:#fecaca; opacity:1;"
                                                        } else if unacked {
                                                            "color:#fecaca; opacity:0.4;"
                                                        } else {
                                                            // acknowledged errors: no flashing, dimmer icon
                                                            "color:#9ca3af; opacity:1;"
                                                        }
                                                    }>
                                                        "⛔"
                                                    </span>
                                                }
                                            }}
                                        </Show>
                                    </button>

                                    // --- Data tab (last) ---
                                    <button
                                        style=move || {
                                            if active_main_tab.get() == MainTab::Data {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #f97316; background:#111827;\
                                                 color:#f97316; cursor:pointer;"
                                            } else {
                                                "padding:0.4rem 0.8rem; border-radius:0.5rem;\
                                                 border:1px solid #4b5563; background:#020617;\
                                                 color:#e5e7eb; cursor:pointer;"
                                            }
                                        }
                                        on:click=move |_| set_active_main_tab.set(MainTab::Data)
                                    >
                                        "Data"
                                    </button>
                                </nav>
                            </div>
                        </div>

                        <div style="flex:0; min-width:200px;"></div>
                    </div>

                    <div style="margin-bottom:0.75rem; display:flex; gap:0.75rem; align-items:center;">
                        <div style="
                    display:flex; align-items:center; gap:0.75rem;
                    padding:0.35rem 0.7rem; border-radius:999px;
                    background:#111827; border:1px solid #4b5563;">
                            <span style="color:#9ca3af;">"Status:"</span>

                            // --- Nominal case ---
                            <Show
                                when=move || warn_count.get() == 0 && err_count.get() == 0
                                fallback=move || {
                                    // --- Non-nominal: show counts + buttons ---
                                    view! {
                                        <>
                                            <Show when=move || has_errors.get()>
                                                {move || view! {
                                                    <span style="color:#fecaca;">
                                                        {format!("{} error(s)", err_count.get())}
                                                    </span>
                                                }}
                                            </Show>

                                            <Show when=move || has_warnings.get()>
                                                {move || view! {
                                                    <span style="color:#facc15;">
                                                        {format!("{} warning(s)", warn_count.get())}
                                                    </span>
                                                }}
                                            </Show>

                                            <span style="color:#93c5fd; margin-left:0.75rem;">
                                                {move || format!("(Flight state: {})", flight_state_str.get())}
                                            </span>

                                            // Acknowledge warnings button – only in Warnings tab
                                            <Show
                                                when=move || active_main_tab.get() == MainTab::Warnings
                                                    && has_warnings.get()
                                            >
                                                {move || {
                                                    view! {
                                                        <button
                                                            style="
                                                        margin-left:auto;
                                                        padding:0.25rem 0.7rem;
                                                        border-radius:999px;
                                                        border:1px solid #4b5563;
                                                        background:#020617;
                                                        color:#e5e7eb;
                                                        font-size:0.75rem;
                                                        cursor:pointer;
                                                    "
                                                            on:click=move |_| {
            let ts = latest_warning_ts.get_untracked();
            set_ack_warning_ts.set(ts);
            if let Some(window) = web_sys::window()
                && let Ok(Some(storage)) = window.local_storage()
            {
                let _ = storage.set_item(WARNING_ACK_STORAGE_KEY, &ts.to_string());
            }
        }

                                                        >
                                                            "Acknowledge warnings"
                                                        </button>
                                                    }
                                                }}
                                            </Show>

                                            // Acknowledge errors button – only in Errors tab
                                            <Show
                                                when=move || active_main_tab.get() == MainTab::Errors
                                                    && err_count.get() != 0
                                            >
                                                {move || {
                                                    view! {
                                                        <button
                                                            style="
                                                        margin-left:auto;
                                                        padding:0.25rem 0.7rem;
                                                        border-radius:999px;
                                                        border:1px solid #4b5563;
                                                        background:#020617;
                                                        color:#e5e7eb;
                                                        font-size:0.75rem;
                                                        cursor:pointer;
                                                    "
                                                            on:click=move |_| {
        let ts = latest_error_ts.get_untracked();
        set_ack_error_ts.set(ts);
        if let Some(window) = web_sys::window()
            && let Ok(Some(storage)) = window.local_storage()
        {
            let _ = storage.set_item(ERROR_ACK_STORAGE_KEY, &ts.to_string());
        }
    }

                                                        >
                                                            "Acknowledge errors"
                                                        </button>
                                                    }
                                                }}
                                            </Show>
                                        </>
                                    }
                                }
                            >
                                // Nominal content (no errors/warnings)
                                {move || view! {
                                <>
                                    <span style="color:#22c55e; font-weight:600;">
                                        "Nominal"
                                    </span>
                                    <span style="color:#93c5fd; margin-left:0.75rem;">
                                        {format!("(Flight state: {})", flight_state_str.get())}
                                    </span>
                                </>
                            }}
                            </Show>
                        </div>
                    </div>

                    <ActionsPanel />

                    {
                        let rows_sig = Signal::from(rows);
                        let warnings_sig = Signal::from(warnings);
                        let errors_sig = Signal::from(errors);
                        let flight_state_sig = flight_state;
                        let rocket_gps_sig = rocket_gps;
                        let user_gps_sig = user_gps;

                        move || match active_main_tab.get() {
                            MainTab::Data => view! {
                                <DataTab
                                    rows=rows_sig
                                    active_tab=Signal::from(active_tab)
                                    set_active_tab=set_active_tab
                                />
                            }.into_any(),

                            MainTab::Warnings => view! {
                                <WarningsTab rows=warnings_sig />
                            }.into_any(),

                            MainTab::Errors => view! {
                                <ErrorsTab rows=errors_sig />
                            }.into_any(),
                            MainTab::State => view! {
                                <StateTab flight_state=flight_state_sig />
                            }.into_any(),

                            MainTab::Map => view! {
                                <MapTab
                                    rocket_gps=Signal::derive({
                                        move || rocket_gps_sig.get()
                                    })
                                    user_gps=Signal::derive({
                                        move || user_gps_sig.get()
                                    })
                                />
                            }.into_any(),
                        }
                    }
                </div>
            }
}
