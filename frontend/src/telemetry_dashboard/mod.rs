use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
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

use data_tab::DataTab;
use errors_tab::ErrorsTab;
use serde::Deserialize;
use warnings_tab::WarningsTab;

pub const HISTORY_MS: i64 = 60_000 * 20; // 20 minutes
const WARNING_ACK_STORAGE_KEY: &str = "gs_last_warning_ack_ts";
const ERROR_ACK_STORAGE_KEY: &str = "gs_last_error_ack_ts";

// ------------------------------------------------------------------------------------------------
// WebSocket handle (thread_local because WASM is single-threaded)
// ------------------------------------------------------------------------------------------------
thread_local! {
    pub static WS_HANDLE: RefCell<Option<WebSocket>> = RefCell::new(None);
}

// ------------------------------------------------------------------------------------------------
// Incoming WebSocket messages from backend
// ------------------------------------------------------------------------------------------------
#[derive(Deserialize)]
#[serde(tag = "ty", content = "data")]
enum WsInMsg {
    Telemetry(TelemetryRow),
    Warning(WarningMsg),
    Error(ErrorMsg),
}

#[derive(Clone, Deserialize)]
struct WarningMsg {
    pub timestamp_ms: i64,
    pub message: String,
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
        if let Some(ws) = cell.borrow().as_ref() {
            if let Err(err) = ws.send_with_str(&msg) {
                web_sys::console::error_1(&err);
            }
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
    Data,
    Warnings,
    Errors,
}

// ------------------------------------------------------------------------------------------------
// MAIN UI COMPONENT
// ------------------------------------------------------------------------------------------------
#[component]
pub fn TelemetryDashboard() -> impl IntoView {
    // --- telemetry data ---
    let (rows, set_rows) = signal(Vec::<TelemetryRow>::new());

    // --- tab selection inside the DataTab ---
    let (active_tab, set_active_tab) = signal("GYRO_DATA".to_string());

    // --- MAIN TABS (Data / Warnings / Errors) ---
    let (active_main_tab, set_active_main_tab) = signal(MainTab::Data);

    // --- ALL warnings + errors (newest first) ---
    let (warnings, set_warnings) = signal(Vec::<WarningRow>::new());
    let (errors, set_errors) = signal(Vec::<ErrorRow>::new());

    // --- Last acknowledged timestamps (client-side) ---
    let (ack_warning_ts, set_ack_warning_ts) = signal(0_i64);
    let (ack_error_ts, set_ack_error_ts) = signal(0_i64);

    // Load acknowledged timestamps from localStorage on mount
    Effect::new({
        let set_ack_warning_ts = set_ack_warning_ts.clone();
        let set_ack_error_ts = set_ack_error_ts.clone();
        move |_| {
            if let Some(window) = web_sys::window() {
                if let Ok(Some(storage)) = window.local_storage() {
                    if let Ok(Some(val)) = storage.get_item(WARNING_ACK_STORAGE_KEY) {
                        if let Ok(parsed) = val.parse::<i64>() {
                            set_ack_warning_ts.set(parsed);
                        }
                    }
                    if let Ok(Some(val)) = storage.get_item(ERROR_ACK_STORAGE_KEY) {
                        if let Ok(parsed) = val.parse::<i64>() {
                            set_ack_error_ts.set(parsed);
                        }
                    }
                }
            }
        }
    });

    // --------------------------------------------------------------------------------------------
    // INITIAL `/api/recent` TELEMETRY LOAD
    // --------------------------------------------------------------------------------------------
    Effect::new({
        let set_rows = set_rows.clone();
        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(resp) = Request::get("/api/recent").send().await {
                    if let Ok(mut list) = resp.json::<Vec<TelemetryRow>>().await {
                        list.sort_by_key(|r| r.timestamp_ms);

                        if let Some(last) = list.last() {
                            let cutoff = last.timestamp_ms - HISTORY_MS;
                            let start = list.partition_point(|r| r.timestamp_ms < cutoff);
                            if start > 0 {
                                list.drain(0..start);
                            }
                        }

                        const MAX_INIT_POINTS: usize = 5000;
                        let n = list.len();
                        if n > MAX_INIT_POINTS {
                            let stride = (n as f32 / MAX_INIT_POINTS as f32).ceil() as usize;
                            list = list
                                .into_iter()
                                .enumerate()
                                .filter_map(
                                    |(i, row)| if i % stride == 0 { Some(row) } else { None },
                                )
                                .collect();
                        }

                        set_rows.set(list);
                    }
                }
            });
        }
    });

    // --------------------------------------------------------------------------------------------
    // INITIAL `/api/alerts` LOAD (warnings + errors from DB)
    // --------------------------------------------------------------------------------------------
    Effect::new({
        let set_warnings = set_warnings.clone();
        let set_errors = set_errors.clone();
        let ack_warning_ts = ack_warning_ts.clone();
        let ack_error_ts = ack_error_ts.clone();
        let set_ack_warning_ts = set_ack_warning_ts.clone();
        let set_ack_error_ts = set_ack_error_ts.clone();

        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(resp) = Request::get("/api/alerts?minutes=20").send().await {
                    if let Ok(mut alerts) = resp.json::<Vec<AlertDto>>().await {
                        // Detect backend reset using timestamps only:
                        let max_ts = alerts.iter().map(|a| a.timestamp_ms).max().unwrap_or(0);

                        let prev_warn = ack_warning_ts.get_untracked();
                        let prev_err = ack_error_ts.get_untracked();
                        let prev_ack = prev_warn.max(prev_err);

                        if prev_ack > 0 && max_ts > 0 && max_ts < prev_ack - HISTORY_MS {
                            set_ack_warning_ts.set(0);
                            set_ack_error_ts.set(0);
                            if let Some(window) = web_sys::window() {
                                if let Ok(Some(storage)) = window.local_storage() {
                                    let _ = storage.remove_item(WARNING_ACK_STORAGE_KEY);
                                    let _ = storage.remove_item(ERROR_ACK_STORAGE_KEY);
                                }
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
                    }
                }
            });
        }
    });

    // --------------------------------------------------------------------------------------------
    // WEBSOCKET LIVE UPDATES (Telemetry + Warning + Error)
    // --------------------------------------------------------------------------------------------
    Effect::new({
        let set_rows = set_rows.clone();
        let set_warnings = set_warnings.clone();
        let set_errors = set_errors.clone();

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
            let handler = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Some(text) = event.data().as_string() {
                    match serde_json::from_str::<WsInMsg>(&text) {
                        Ok(WsInMsg::Telemetry(row)) => {
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
                        Err(e) => {
                            web_sys::console::error_1(&format!("WS parse error: {e}").into());
                        }
                    }
                }
            });

            ws.set_onmessage(Some(handler.as_ref().unchecked_ref()));
            handler.forget();

            // tick loop for telemetry
            let pending_for_tick = pending.clone();
            let set_rows_for_tick = set_rows.clone();

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
    let warn_count = Signal::derive({
        let warnings = warnings.clone();
        move || warnings.get().len()
    });
    let err_count = Signal::derive({
        let errors = errors.clone();
        move || errors.get().len()
    });
    let has_warnings = Signal::derive({
        let warn_count = warn_count.clone();
        move || warn_count.get() > 0
    });
    // latest timestamps, for unacknowledged logic
    let latest_warning_ts = Signal::derive({
        let warnings = warnings.clone();
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
        let errors = errors.clone();
        move || {
            errors
                .get()
                .iter()
                .map(|e| e.timestamp_ms)
                .max()
                .unwrap_or(0)
        }
    });

    let has_errors = Signal::derive({
        let err_count = err_count.clone();
        move || err_count.get() > 0
    });

    // unacknowledged warnings: there is at least one warning newer than the ack timestamp
    let has_unacked_warnings = Signal::derive({
        let latest_warning_ts = latest_warning_ts.clone();
        let ack_warning_ts = ack_warning_ts.clone();
        move || {
            let latest = latest_warning_ts.get();
            let ack = ack_warning_ts.get();
            latest > 0 && latest > ack
        }
    });

    // unacknowledged errors: same idea
    let has_unacked_errors = Signal::derive({
        let latest_error_ts = latest_error_ts.clone();
        let ack_error_ts = ack_error_ts.clone();
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
        let has_errors = has_errors.clone();
        let has_unacked_errors = has_unacked_errors.clone();
        let flash_on = flash_on.clone();
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
                                                    "color:#9ca3af; opacity:0.6;"
                                                }
                                            }>
                                                "⚠"
                                            </span>
                                        }
                                    }}
                                </Show>
                            </button>

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
                                                    "color:#9ca3af; opacity:0.6;"
                                                }
                                            }>
                                                "⛔"
                                            </span>
                                        }
                                    }}
                                </Show>
                            </button>
                        </nav>
                    </div>
                </div>

                <div style="flex:0; min-width:200px;"></div>
            </div>

            <div style="margin-bottom:0.75rem; display:flex; gap:0.75rem; align-items:center;">
                <Show
                    when=move || warn_count.get() == 0 && err_count.get() == 0
                    fallback=move || {
                        // Status pill + tab-specific acknowledge buttons
                        view! {
                            <div style="
                                display:flex; align-items:center; gap:0.75rem;
                                padding:0.35rem 0.7rem; border-radius:999px;
                                background:#111827; border:1px solid #4b5563;">
                                <span style="color:#9ca3af;">"Status:"</span>

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

                                // Acknowledge warnings button – only in Warnings tab
                                <Show
                                    when=move || active_main_tab.get() == MainTab::Warnings
                                        && has_warnings.get()
                                >
                                    {move || {
                                        let latest_warning_ts = latest_warning_ts.clone();
                                        let set_ack_warning_ts = set_ack_warning_ts.clone();
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
                                                    if let Some(window) = web_sys::window() {
                                                        if let Ok(Some(storage)) = window.local_storage() {
                                                            let _ = storage.set_item(
                                                                WARNING_ACK_STORAGE_KEY,
                                                                &ts.to_string(),
                                                            );
                                                        }
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
                                        let latest_error_ts = latest_error_ts.clone();
                                        let set_ack_error_ts = set_ack_error_ts.clone();
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
                                                    if let Some(window) = web_sys::window() {
                                                        if let Ok(Some(storage)) = window.local_storage() {
                                                            let _ = storage.set_item(
                                                                ERROR_ACK_STORAGE_KEY,
                                                                &ts.to_string(),
                                                            );
                                                        }
                                                    }
                                                }
                                            >
                                                "Acknowledge errors"
                                            </button>
                                        }
                                    }}
                                </Show>
                            </div>
                        }
                    }
                >
                    <span style="color:#9ca3af;">"Status: All systems nominal."</span>
                </Show>
            </div>

            <ActionsPanel />

            {
                let rows_sig = Signal::from(rows);
                let warnings_sig = Signal::from(warnings);
                let errors_sig = Signal::from(errors);

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
                }
            }
        </div>
    }
}
