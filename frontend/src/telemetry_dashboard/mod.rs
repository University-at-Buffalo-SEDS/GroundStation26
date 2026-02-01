// frontend/src/telemetry_dashboard/mod.rs

mod actions_tab;
mod connection_status_tab;
pub mod data_chart;
pub mod data_tab;
pub mod errors_tab;
mod gps;
mod gps_android;
mod latency_chart;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod gps_apple;

pub mod map_tab;
pub mod state_tab;
pub mod warnings_tab;

#[cfg(not(target_arch = "wasm32"))]
use crate::app::Route;
use data_chart::{
    charts_cache_ingest_row, charts_cache_request_refit, charts_cache_reset_and_ingest,
};

use crate::telemetry_dashboard::actions_tab::ActionsTab;
use connection_status_tab::ConnectionStatusTab;
use data_tab::DataTab;
use dioxus::prelude::*;
use dioxus_signals::Signal;
use errors_tab::ErrorsTab;
use groundstation_shared::{BoardStatusEntry, BoardStatusMsg, FlightState, TelemetryRow};
use map_tab::MapTab;
use serde::Deserialize;
use state_tab::StateTab;
use warnings_tab::WarningsTab;

use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering}, Arc,
    Mutex,
};

use once_cell::sync::Lazy;

// ============================================================================
// Telemetry queue: decouple high-rate telemetry ingest from UI re-render cadence.
// - WS ingest becomes O(1) and never does large Vec rebuilds.
// - UI flush loop drains at ~120Hz (or as fast as runtime allows).
// ============================================================================
static TELEMETRY_QUEUE: Lazy<Mutex<VecDeque<TelemetryRow>>> =
    Lazy::new(|| Mutex::new(VecDeque::new()));

// ============================================================================
// Dashboard lifetime: STATIC + ALWAYS PRESENT (never Option)
// - Solves: Inner reads before Outer writes -> false Arc -> tasks early-exit
//
// CHANGE: we make "unmount" idempotent (swap) and we also let the CONNECT button
//         explicitly flip alive=false *before* bumping WS_EPOCH, so the WS
//         supervisor won't spawn a new epoch while we're leaving the dashboard.
// ============================================================================
#[derive(Clone)]
struct DashboardLife {
    alive: Arc<AtomicBool>,
    // bumps on every REAL mount of outer dashboard
    r#gen: u64,
}

impl DashboardLife {
    fn _new_dead() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(false)),
            r#gen: 0,
        }
    }
    fn new_alive() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            r#gen: 0,
        }
    }
}

static DASHBOARD_LIFE: GlobalSignal<DashboardLife> = Signal::global(DashboardLife::new_alive);

#[inline]
fn dashboard_alive() -> Arc<AtomicBool> {
    DASHBOARD_LIFE.read().alive.clone()
}

#[inline]
fn _set_dashboard_alive(alive: bool) {
    let alive = Arc::new(AtomicBool::new(alive));
    *DASHBOARD_LIFE.write() = DashboardLife {
        alive,
        r#gen: dashboard_gen() + 1,
    };
}

#[inline]
fn dashboard_gen() -> u64 {
    DASHBOARD_LIFE.read().r#gen
}

// ----------------------------
// Cross-platform persistence
//  - wasm32: localStorage
//  - native: JSON file in app data dir
// ----------------------------
mod persist {
    pub fn get_string(key: &str) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        {
            use web_sys::window;
            let w = window()?;
            let ls = w.local_storage().ok()??;
            return ls.get_item(key).ok().flatten();
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            native::get_string(key).ok().flatten()
        }
    }

    pub fn set_string(key: &str, value: &str) {
        #[cfg(target_arch = "wasm32")]
        {
            use web_sys::window;
            if let Some(w) = window() {
                if let Ok(Some(ls)) = w.local_storage() {
                    let _ = ls.set_item(key, value);
                }
            }
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = native::set_string(key, value);
        }
    }

    pub fn _remove(key: &str) {
        #[cfg(target_arch = "wasm32")]
        {
            use web_sys::window;
            if let Some(w) = window() {
                if let Ok(Some(ls)) = w.local_storage() {
                    let _ = ls.remove_item(key);
                }
            }
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = native::_remove(key);
        }
    }

    pub fn get_or(key: &str, default: &str) -> String {
        get_string(key).unwrap_or_else(|| default.to_string())
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod native {
        use std::collections::HashMap;
        use std::io;

        fn storage_path() -> std::path::PathBuf {
            let mut base = dirs::data_local_dir()
                .or_else(dirs::data_dir)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
            base.push("gs26");
            base.push("storage.json");
            base
        }

        fn load_map() -> Result<HashMap<String, String>, io::Error> {
            let path = storage_path();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
                Err(e) => return Err(e),
            };

            let map = serde_json::from_slice::<HashMap<String, String>>(&bytes).unwrap_or_default();
            Ok(map)
        }

        fn save_map(map: &HashMap<String, String>) -> Result<(), io::Error> {
            let path = storage_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = serde_json::to_vec_pretty(map).unwrap_or_else(|_| b"{}".to_vec());
            std::fs::write(path, bytes)?;
            Ok(())
        }

        pub fn get_string(key: &str) -> Result<Option<String>, io::Error> {
            let map = load_map()?;
            Ok(map.get(key).cloned())
        }

        pub fn set_string(key: &str, value: &str) -> Result<(), io::Error> {
            let mut map = load_map()?;
            map.insert(key.to_string(), value.to_string());
            save_map(&map)?;
            Ok(())
        }

        pub fn _remove(key: &str) -> Result<(), io::Error> {
            let mut map = load_map()?;
            map.remove(key);
            save_map(&map)?;
            Ok(())
        }
    }
}

// Matches your existing schema. (ty + data)
#[derive(Deserialize, Debug)]
#[serde(tag = "ty", content = "data")]
enum WsInMsg {
    Telemetry(TelemetryRow),
    FlightState(FlightStateMsg),
    Warning(AlertMsg),
    Error(AlertMsg),
    BoardStatus(BoardStatusMsg),
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

// --------------------------
// DB alert DTO (/api/alerts)
// --------------------------
#[derive(Deserialize, Debug, Clone)]
struct AlertDto {
    pub timestamp_ms: i64,
    pub severity: String, // "warning" | "error"
    pub message: String,
}

// --------------------------
// GPS DTO (/api/gps)
// --------------------------
#[derive(Deserialize, Debug, Clone)]
struct GpsResponse {
    pub rocket_lat: f64,
    pub rocket_lon: f64,
    pub user_lat: f64,
    pub user_lon: f64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainTab {
    State,
    ConnectionStatus,
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

// unified storage keys
const WARNING_ACK_STORAGE_KEY: &str = "gs_last_warning_ack_ts";
const ERROR_ACK_STORAGE_KEY: &str = "gs_last_error_ack_ts";
const MAIN_TAB_STORAGE_KEY: &str = "gs_main_tab";
const DATA_TAB_STORAGE_KEY: &str = "gs_data_tab";
const BASE_URL_STORAGE_KEY: &str = "gs_base_url";

// When this number changes, we tear down and rebuild the websocket connection.
static WS_EPOCH: GlobalSignal<u64> = Signal::global(|| 0);

#[cfg(target_arch = "wasm32")]
static WS_RAW: GlobalSignal<Option<web_sys::WebSocket>> = Signal::global(|| None);

// Native “reload UI” remount key.
// IMPORTANT: this key is applied ONLY to the INNER component, so it does NOT
// trigger TelemetryDashboard’s unmount guard.
static UI_EPOCH: GlobalSignal<u64> = Signal::global(|| 0);
// Force re-seed of graphs/history from backend.
static SEED_EPOCH: GlobalSignal<u64> = Signal::global(|| 0);

fn normalize_base_url(mut url: String) -> String {
    if let Some(idx) = url.find('#') {
        url.truncate(idx);
    }
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if let Some(slash) = rest.find('/') {
            url.truncate(scheme_end + 3 + slash);
        }
    }
    url.trim_end_matches('/').to_string()
}

pub fn abs_http(path: &str) -> String {
    let base = UrlConfig::base_http();
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    if base.is_empty() {
        path
    } else {
        format!("{base}{path}")
    }
}

fn bump_ws_epoch() {
    *WS_SENDER.write() = None;

    #[cfg(target_arch = "wasm32")]
    {
        if let Some(ws) = WS_RAW.write().take() {
            let _ = ws.close();
        }
    }

    *WS_EPOCH.write() += 1;
}

fn _bump_ui_epoch() {
    *UI_EPOCH.write() += 1;
}

fn bump_seed_epoch() {
    *SEED_EPOCH.write() += 1;
}
// tab <-> string
fn _main_tab_to_str(tab: MainTab) -> &'static str {
    match tab {
        MainTab::State => "state",
        MainTab::ConnectionStatus => "connection-status",
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
        "connection-status" => MainTab::ConnectionStatus,
        "map" => MainTab::Map,
        "actions" => MainTab::Actions,
        "warnings" => MainTab::Warnings,
        "errors" => MainTab::Errors,
        "data" => MainTab::Data,
        _ => MainTab::State,
    }
}

// ---------- Base URL config ----------
pub struct UrlConfig;

impl UrlConfig {
    pub fn set_base_url_and_persist(url: String) {
        let clean = normalize_base_url(url);
        *BASE_URL.write() = clean.clone();
        persist::set_string(BASE_URL_STORAGE_KEY, &clean);
    }

    pub fn _stored_base_url() -> Option<String> {
        persist::get_string(BASE_URL_STORAGE_KEY)
            .map(normalize_base_url)
            .filter(|s| !s.trim().is_empty())
    }

    pub fn base_http() -> String {
        // load from storage key if present
        let base = persist::get_string(BASE_URL_STORAGE_KEY)
            .map(normalize_base_url)
            .unwrap_or_else(|| BASE_URL.read().clone());

        #[cfg(not(target_arch = "wasm32"))]
        if base.is_empty() {
            return "http://localhost:3000".to_string();
        }

        base
    }

    /// Returns ws/wss scheme + host[:port] (no path).
    pub fn base_ws() -> String {
        #[cfg(target_arch = "wasm32")]
        {
            let base_http = BASE_URL.read().clone();
            if base_http.is_empty() {
                if let Some(window) = web_sys::window() {
                    let loc = window.location();
                    let protocol = loc.protocol().unwrap_or_else(|_| "http:".to_string());
                    let host = loc.host().unwrap_or_else(|_| "localhost:3000".to_string());
                    let ws_scheme = if protocol == "https:" { "wss" } else { "ws" };
                    return format!("{ws_scheme}://{host}");
                }
                return "ws://localhost:3000".to_string();
            }
        }

        let base_http = UrlConfig::base_http().trim_end_matches('/').to_string();

        if base_http.starts_with("https://") {
            base_http.replacen("https://", "wss://", 1)
        } else if base_http.starts_with("http://") {
            base_http.replacen("http://", "ws://", 1)
        } else if base_http.starts_with("wss://") || base_http.starts_with("ws://") {
            base_http
        } else {
            format!("ws://{base_http}")
        }
    }
}

static BASE_URL: GlobalSignal<String> = Signal::global(String::new);

#[cfg(target_arch = "wasm32")]
fn hard_reload_app_web() {
    if let Some(w) = web_sys::window() {
        let _ = w.location().reload();
    }
}

fn reconnect_and_reload_ui() {
    // Always restart websockets/tasks
    bump_ws_epoch();
    bump_seed_epoch();

    // Web: real reload
    #[cfg(target_arch = "wasm32")]
    {
        hard_reload_app_web();
    }

    // Native: soft “reload” by remounting ONLY the inner dashboard subtree
    #[cfg(not(target_arch = "wasm32"))]
    {
        _bump_ui_epoch();
    }
}

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

// ============================================================================
// OUTER component: owns “real mount” lifetime & publishes it into DASHBOARD_LIFE
// INNER component is keyed for native “reload UI” without tripping outer Drop.
// ============================================================================
#[component]
pub fn TelemetryDashboard() -> Element {
    // Create once per real mount
    *DASHBOARD_LIFE.write() = DashboardLife::new_alive();

    log!(
        "[UI] TelemetryDashboard mounted (alive=true, gen={})",
        dashboard_gen()
    );

    rsx! {
        TelemetryDashboardInner { key: "{*UI_EPOCH.read()}" }
    }
}

// ---------- INNER dashboard (this is what we remount on native reload) ----------
#[component]
fn TelemetryDashboardInner() -> Element {
    // Always valid; becomes “real” once outer publishes it.
    let alive = dashboard_alive();

    // ----------------------------
    // Persistent values (strings)
    // ----------------------------
    let st_warn_ack = use_signal(|| persist::get_or(WARNING_ACK_STORAGE_KEY, "0"));
    let st_err_ack = use_signal(|| persist::get_or(ERROR_ACK_STORAGE_KEY, "0"));
    let st_main_tab = use_signal(|| persist::get_or(MAIN_TAB_STORAGE_KEY, "state"));
    let st_data_tab = use_signal(|| persist::get_or(DATA_TAB_STORAGE_KEY, "GYRO_DATA"));
    let st_base_url = use_signal(|| persist::get_or(BASE_URL_STORAGE_KEY, ""));

    let parse_i64 = |s: &str| s.parse::<i64>().unwrap_or(0);

    // ----------------------------
    // Live app state
    // ----------------------------
    let rows = use_signal(Vec::<TelemetryRow>::new);

    let active_data_tab = use_signal(|| st_data_tab.read().clone());
    let warnings = use_signal(Vec::<AlertMsg>::new);
    let errors = use_signal(Vec::<AlertMsg>::new);
    let flight_state = use_signal(|| FlightState::Startup);
    let board_status = use_signal(Vec::<BoardStatusEntry>::new);

    let active_main_tab = use_signal(|| _main_tab_from_str(st_main_tab.read().as_str()));

    let ack_warning_ts = use_signal(|| parse_i64(st_warn_ack.read().as_str()));
    let ack_error_ts = use_signal(|| parse_i64(st_err_ack.read().as_str()));
    let warning_event_counter = use_signal(|| 0u64);
    let error_event_counter = use_signal(|| 0u64);
    let ack_warning_count = use_signal(|| 0u64);
    let ack_error_count = use_signal(|| 0u64);

    let flash_on = use_signal(|| false);

    let rocket_gps = use_signal(|| None::<(f64, f64)>);
    let user_gps = use_signal(|| None::<(f64, f64)>);

    // ---------------------------------------------------------
    // Base URL sync
    // ---------------------------------------------------------
    {
        let mut last_applied_base = use_signal(String::new);

        use_effect(move || {
            let base = st_base_url.read().clone();
            if *last_applied_base.read() == base {
                return;
            }

            last_applied_base.set(base.clone());

            UrlConfig::set_base_url_and_persist(base);
            log!("[GS26] Base URL changed; bumping ws epoch.");
            bump_ws_epoch();
        });
    }

    // Persist UI state changes
    {
        let mut st_main_tab = st_main_tab;
        use_effect(move || {
            let s = _main_tab_to_str(*active_main_tab.read()).to_string();
            st_main_tab.set(s.clone());
            persist::set_string(MAIN_TAB_STORAGE_KEY, &s);
        });
    }
    {
        use_effect(move || {
            if *active_main_tab.read() == MainTab::Map {
                js_eval(
                    r#"
                    (function() {
                      try {
                        if (typeof window.__gs26_map_size_hook_update === "function") {
                          window.__gs26_map_size_hook_update();
                        }
                      } catch (e) {}
                    })();
                    "#,
                );
            }
        });
    }
    {
        let mut st_data_tab = st_data_tab;
        use_effect(move || {
            let v = active_data_tab.read().clone();
            st_data_tab.set(v.clone());
            persist::set_string(DATA_TAB_STORAGE_KEY, &v);
        });
    }
    {
        let mut st_warn_ack = st_warn_ack;
        use_effect(move || {
            let v = ack_warning_ts.read().to_string();
            st_warn_ack.set(v.clone());
            persist::set_string(WARNING_ACK_STORAGE_KEY, &v);
        });
    }
    {
        let mut st_err_ack = st_err_ack;
        use_effect(move || {
            let v = ack_error_ts.read().to_string();
            st_err_ack.set(v.clone());
            persist::set_string(ERROR_ACK_STORAGE_KEY, &v);
        });
    }
    {
        use_effect(move || {
            let v = st_base_url.read().clone();
            persist::set_string(BASE_URL_STORAGE_KEY, &v);
        });
    }

    // ------------------------------------------------------------------------
    // UI flush loop: drain telemetry queue into `rows` at a fixed cadence
    // ------------------------------------------------------------------------
    {
        let alive = alive.clone();
        let mut rows_s = rows;

        use_effect(move || {
            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();

            spawn(async move {
                // Target ~120 FPS. In browsers this often ends up ~60 FPS depending on clamps.
                let tick_ms: u32 = std::env::var("GS_UI_TICK_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(4)
                    .clamp(1, 50);

                while alive.load(Ordering::Relaxed) && *WS_EPOCH.read() == epoch {
                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(tick_ms).await;

                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(tick_ms as u64)).await;

                    if !alive.load(Ordering::Relaxed) || *WS_EPOCH.read() != epoch {
                        break;
                    }

                    // Drain queued telemetry
                    let mut drained: Vec<TelemetryRow> = Vec::new();
                    if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
                        while let Some(r) = q.pop_front() {
                            drained.push(r);
                        }
                    }

                    if drained.is_empty() {
                        continue;
                    }

                    // Append + prune in one write
                    {
                        let mut v = rows_s.write();
                        v.extend(drained);

                        // Time prune to HISTORY_MS using newest timestamp
                        if let Some(last) = v.last() {
                            let cutoff = last.timestamp_ms - HISTORY_MS;
                            let split = v.partition_point(|r| r.timestamp_ms < cutoff);
                            if split > 0 {
                                v.drain(0..split);
                            }
                        }

                        // Hard cap to keep UI/state light (avoid pathological growth)
                        const MAX_KEEP: usize = 12_000;
                        if v.len() > MAX_KEEP {
                            let drop_n = v.len() - MAX_KEEP;
                            v.drain(0..drop_n);
                        }
                    }
                }
            });
        });
    }

    // Seed from DB (HTTP) on mount
    {
        let mut last_seed_epoch = use_signal(|| None::<u64>);

        let mut rows_s = rows;
        let mut warnings_s = warnings;
        let mut errors_s = errors;
        let mut board_status_s = board_status;
        let mut rocket_gps_s = rocket_gps;
        let mut user_gps_s = user_gps;
        let mut ack_warning_ts_s = ack_warning_ts;
        let mut ack_error_ts_s = ack_error_ts;

        let alive = alive.clone();

        use_effect(move || {
            let current_seed = *SEED_EPOCH.read();
            if last_seed_epoch.read().as_ref() == Some(&current_seed) {
                return;
            }
            last_seed_epoch.set(Some(current_seed));

            // Clear graphs immediately before re-seeding.
            rows_s.set(Vec::new());

            // Tell charts to refit their time window to whatever history we load next
            // (smooth shrink on demand; does nothing during normal live updates)
            charts_cache_request_refit();

            // Reset chart cache too (so DataTab doesn't show stale lines)
            charts_cache_reset_and_ingest(&[]);

            // Also clear any queued telemetry to avoid mixing old/new epochs
            if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
                q.clear();
            }

            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();
            spawn(async move {
                if !alive.load(Ordering::Relaxed) || *WS_EPOCH.read() != epoch {
                    return;
                }

                if let Err(e) = seed_from_db(
                    &mut rows_s,
                    &mut warnings_s,
                    &mut errors_s,
                    &mut board_status_s,
                    &mut rocket_gps_s,
                    &mut user_gps_s,
                    &mut ack_warning_ts_s,
                    &mut ack_error_ts_s,
                    alive.clone(),
                )
                .await
                    && alive.load(Ordering::Relaxed)
                    && *WS_EPOCH.read() == epoch
                {
                    log!("seed_from_db failed: {e}");
                }
            });
        });
    }

    // Flash loop
    {
        let mut flash_on = flash_on;
        let alive = alive.clone();

        use_effect(move || {
            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();
            spawn(async move {
                while alive.load(Ordering::Relaxed) && *WS_EPOCH.read() == epoch {
                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(500).await;

                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                    if !alive.load(Ordering::Relaxed) || *WS_EPOCH.read() != epoch {
                        break;
                    }

                    let next = !*flash_on.read();
                    flash_on.set(next);
                }
            });
        });
    }

    // Derived state
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

    let has_unacked_warnings = latest_warning_ts > 0
        && (latest_warning_ts > *ack_warning_ts.read()
            || *warning_event_counter.read() > *ack_warning_count.read());
    let has_unacked_errors = latest_error_ts > 0
        && (latest_error_ts > *ack_error_ts.read()
            || *error_event_counter.read() > *ack_error_count.read());

    let border_style = if has_unacked_errors && *flash_on.read() {
        "2px solid #ef4444"
    } else if has_unacked_errors && has_errors {
        "1px solid #ef4444"
    } else if has_unacked_warnings && *flash_on.read() {
        "2px solid #facc15"
    } else if has_unacked_warnings && has_warnings {
        "1px solid #facc15"
    } else {
        "1px solid transparent"
    };

    // Initial flightstate (HTTP)
    {
        let mut flight_state = flight_state;
        let alive = alive.clone();

        use_effect(move || {
            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();
            spawn(async move {
                if !alive.load(Ordering::Relaxed) || *WS_EPOCH.read() != epoch {
                    return;
                }

                if let Ok(state) = http_get_json::<FlightState>("/flightstate").await
                    && alive.load(Ordering::Relaxed)
                    && *WS_EPOCH.read() == epoch
                {
                    flight_state.set(state);
                }
            });
        });
    }

    // WebSocket supervisor (spawn ONCE per epoch)
    {
        let alive = alive.clone();
        let mut last_started_epoch = use_signal(|| None::<u64>);

        use_effect(move || {
            let epoch = *WS_EPOCH.read();

            // IMPORTANT: if dashboard has been "logically" disabled (CONNECT pressed),
            // do not spawn a supervisor for the new epoch.
            if !alive.load(Ordering::Relaxed) {
                return;
            }

            if last_started_epoch.read().as_ref() == Some(&epoch) {
                return;
            }
            last_started_epoch.set(Some(epoch));

            log!("[WS] supervisor spawn (epoch={epoch})");
            let alive = alive.clone();
            spawn(async move {
                if !alive.load(Ordering::Relaxed) {
                    log!("[WS] early exit (alive=false) epoch={epoch}");
                    return;
                }

                if let Err(e) = connect_ws_supervisor(
                    epoch,
                    rows,
                    warnings,
                    errors,
                    warning_event_counter,
                    error_event_counter,
                    flight_state,
                    board_status,
                    rocket_gps,
                    user_gps,
                    alive.clone(),
                )
                .await
                    && alive.load(Ordering::Relaxed)
                {
                    log!("[WS] supervisor ended: {e}");
                }
            });
        });
    }

    // Button styles
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

    // Native-only CONNECT button
    let connect_button: Element = {
        #[cfg(not(target_arch = "wasm32"))]
        use dioxus_router::use_navigator;
        #[cfg(not(target_arch = "wasm32"))]
        let nav = use_navigator();

        #[cfg(target_arch = "wasm32")]
        {
            rsx! { div {} }
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let alive_for_click = alive.clone();

            rsx! {

                button {
                    style: "
                        padding:0.45rem 0.85rem;
                        border-radius:0.75rem;
                        border:1px solid #334155;
                        background:#111827;
                        color:#e5e7eb;
                        font-weight:800;
                        cursor:pointer;
                    ",
                    onclick: move |_| {
                        // KEY CHANGE:
                        // Mark dashboard "not alive" *before* bumping WS_EPOCH.
                        // That prevents the dashboard's WS supervisor effect from spawning
                        // a new epoch while we're navigating away.
                        let was_alive = alive_for_click.swap(false, Ordering::Relaxed);
                        #[cfg(any(target_os = "macos", target_os = "ios"))]
                        gps::stop_gps_updates();
                        _set_dashboard_alive(false);
                        if was_alive {
                            bump_ws_epoch();
                            log!("[UI] CONNECT pressed -> alive=false + bump epoch");
                        }

                        let _ = nav.push(Route::Connect {});
                    },
                    "CONNECT"
                }
            }
        }
    };

    // Reload button (web: full reload, native: remount inner UI)
    let reload_button: Element = rsx! {
        button {
            style: "
                padding:0.45rem 0.85rem;
                border-radius:0.75rem;
                border:1px solid #334155;
                background:#111827;
                color:#e5e7eb;
                font-weight:800;
                cursor:pointer;
            ",
            onclick: move |_| {
                    charts_cache_request_refit();

                reconnect_and_reload_ui();
            },
            "RELOAD"
        }
    };

    fn start_gps_js() -> bool {
        // Only needed if you want to gate geolocation until the JS is ready on wasm:
        #[cfg(target_arch = "wasm32")]
        return js_is_ground_map_ready();

        #[cfg(not(target_arch = "wasm32"))]
        true
    }

    // MAIN UI
    rsx! {
    gps::GpsDriver {
        user_gps: user_gps,
        // Only needed if you want to gate geolocation until the JS is ready on wasm:
        js_ready: Some(start_gps_js()),
    }
        div {

            style: "
                height:100vh;
                padding:24px;
                color:#e5e7eb;
                font-family:system-ui, -apple-system, BlinkMacSystemFont;
                background:#020617;
                display:flex;
                flex-direction:column;
                border:{border_style};
                box-sizing:border-box;
                overflow:hidden;
            ",

            // Header row 1
            div {
                style: "
                    display:flex;
                    align-items:center;
                    justify-content:space-between;
                    gap:16px;
                    width:100%;
                    margin-bottom:12px;
                    flex-wrap:wrap;
                ",
                h1 { style: "color:#f97316; margin:0; font-size:22px; font-weight:800;", "Rocket Dashboard" }

                div { style: "display:flex; align-items:center; gap:10px; flex-wrap:wrap;",
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

                    {reload_button}
                    {connect_button}
                }
            }

            // Header row 2
            div {
                style: "
                    display:flex;
                    align-items:center;
                    gap:12px;
                    width:100%;
                    margin-bottom:12px;
                    flex-wrap:wrap;
                ",

                div {
                    style: "
                        flex:1 1 520px;
                        display:flex;
                        align-items:center;
                        padding:0.85rem;
                        border-radius:0.75rem;
                        background:#020617ee;
                        border:1px solid #4b5563;
                        box-shadow:0 10e0px 25px rgba(0,0,0,0.45);
                        min-width:260px;
                    ",
                    nav { style: "display:flex; gap:0.5rem; flex-wrap:wrap;",
                        button {
                            style: if *active_main_tab.read() == MainTab::State { tab_style_active("#38bdf8") } else { tab_style_inactive.to_string() },
                            onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::State) },
                            "Flight"
                        }
                        button {
                            style: if *active_main_tab.read() == MainTab::ConnectionStatus { tab_style_active("#06b6d4") } else { tab_style_inactive.to_string() },
                            onclick: { let mut t = active_main_tab; move |_| t.set(MainTab::ConnectionStatus) },
                            "Connection Status"
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
                            style: if *active_main_tab.read() == MainTab::Warnings { tab_style_active("#facc15") } else { tab_style_inactive.to_string() },
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
                            style: if *active_main_tab.read() == MainTab::Errors { tab_style_active("#ef4444") } else { tab_style_inactive.to_string() },
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

                div {
                    style: "
                        flex:1 1 320px;
                        display:flex;
                        align-items:center;
                        flex-wrap:wrap;
                        gap:0.5rem;
                        padding:0.35rem 0.7rem;
                        border-radius:1rem;
                        background:#111827;
                        border:1px solid #4b5563;
                        min-width:260px;
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
                                    let mut ack_warning_count = ack_warning_count;
                                    move |_| {
                                        ack_warning_ts.set(latest_warning_ts);
                                        ack_warning_count.set(*warning_event_counter.read());
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
                                    let mut ack_error_count = ack_error_count;
                                    move |_| {
                                        ack_error_ts.set(latest_error_ts);
                                        ack_error_count.set(*error_event_counter.read());
                                    }
                                },
                                "Acknowledge errors"
                            }
                        }
                    }
                }
            }

            // Main body
            div { style: "flex:1; min-height:0; overflow:hidden;",
                match *active_main_tab.read() {
                    MainTab::State => rsx! {
                        div { style: "height:100%; overflow-y:auto; overflow-x:hidden; -webkit-overflow-scrolling:auto;",
                            StateTab {
                                flight_state: flight_state,
                                rows: rows,
                                board_status: board_status,
                                rocket_gps: rocket_gps,
                                user_gps: user_gps,
                            }
                        }
                    },
                    MainTab::ConnectionStatus => rsx! { ConnectionStatusTab { boards: board_status } },
                    MainTab::Map => rsx! { MapTab { rocket_gps: rocket_gps, user_gps: user_gps } },
                    MainTab::Actions => rsx! {
                        div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                            ActionsTab {}
                        }
                    },
                    MainTab::Warnings => rsx! {
                        div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                            WarningsTab { warnings: warnings }
                        }
                    },
                    MainTab::Errors => rsx! {
                        div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                            ErrorsTab { errors: errors }
                        }
                    },
                    MainTab::Data => rsx! { DataTab { rows: rows, active_tab: active_data_tab } },
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

    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    let base = UrlConfig::base_http();

    let url = if base.is_empty() {
        let w = web_sys::window().ok_or("no window".to_string())?;
        let origin = w
            .location()
            .origin()
            .map_err(|_| "failed to read window.location.origin".to_string())?;
        format!("{origin}{path}")
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
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

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

// ------------------------------
// Seed telemetry/alerts/gps
// ------------------------------
#[allow(clippy::too_many_arguments)]
async fn seed_from_db(
    rows: &mut Signal<Vec<TelemetryRow>>,
    warnings: &mut Signal<Vec<AlertMsg>>,
    errors: &mut Signal<Vec<AlertMsg>>,
    board_status: &mut Signal<Vec<BoardStatusEntry>>,
    rocket_gps: &mut Signal<Option<(f64, f64)>>,
    user_gps: &mut Signal<Option<(f64, f64)>>,
    ack_warning_ts: &mut Signal<i64>,
    ack_error_ts: &mut Signal<i64>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }

    // ---- Telemetry history (/api/recent) ----
    if let Ok(mut list) = http_get_json::<Vec<TelemetryRow>>("/api/recent").await {
        if !alive.load(Ordering::Relaxed) {
            return Ok(());
        }

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
                .filter_map(|(i, row)| (i % stride == 0).then_some(row))
                .collect();
        }

        if let Some(gps) = list.iter().rev().find_map(row_to_gps) {
            rocket_gps.set(Some(gps));
        }

        // IMPORTANT: keep chart cache in sync with seeded history
        charts_cache_reset_and_ingest(&list);
        rows.set(list);
    }

    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }

    // ---- Alerts history (/api/alerts) ----
    if let Ok(mut alerts) = http_get_json::<Vec<AlertDto>>("/api/alerts?minutes=20").await {
        if !alive.load(Ordering::Relaxed) {
            return Ok(());
        }

        let max_ts = alerts.iter().map(|a| a.timestamp_ms).max().unwrap_or(0);
        let prev_ack = (*ack_warning_ts.read()).max(*ack_error_ts.read());
        if prev_ack > 0 && max_ts > 0 && max_ts < prev_ack - HISTORY_MS {
            ack_warning_ts.set(0);
            ack_error_ts.set(0);
        }

        alerts.sort_by_key(|a| -a.timestamp_ms);

        let mut w = Vec::<AlertMsg>::new();
        let mut e = Vec::<AlertMsg>::new();
        for a in alerts {
            match a.severity.as_str() {
                "warning" => w.push(AlertMsg {
                    timestamp_ms: a.timestamp_ms,
                    message: a.message,
                }),
                "error" => e.push(AlertMsg {
                    timestamp_ms: a.timestamp_ms,
                    message: a.message,
                }),
                _ => {}
            }
        }

        warnings.set(w);
        errors.set(e);
    }

    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }

    // ---- Board status (/api/boards) ----
    if let Ok(status) = http_get_json::<BoardStatusMsg>("/api/boards").await
        && alive.load(Ordering::Relaxed)
    {
        board_status.set(status.boards);
    }

    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }

    // ---- Optional GPS seed (/api/gps) ----
    if let Ok(gps) = http_get_json::<GpsResponse>("/api/gps").await
        && alive.load(Ordering::Relaxed)
    {
        rocket_gps.set(Some((gps.rocket_lat, gps.rocket_lon)));
        user_gps.set(Some((gps.user_lat, gps.user_lon)));
    }

    Ok(())
}

// ---------------------------------------------------------
// WebSocket supervisor (reconnect loop) — both platforms
// ---------------------------------------------------------
#[allow(clippy::too_many_arguments)]
async fn connect_ws_supervisor(
    epoch: u64,
    rows: Signal<Vec<TelemetryRow>>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    warning_event_counter: Signal<u64>,
    error_event_counter: Signal<u64>,
    flight_state: Signal<FlightState>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    if *WS_EPOCH.read() != epoch {
        return Ok(());
    }

    log!("[WS] supervisor starting connection (epoch={epoch})");

    loop {
        if !alive.load(Ordering::Relaxed) {
            break;
        }
        if *WS_EPOCH.read() != epoch {
            break;
        }

        let res = {
            #[cfg(target_arch = "wasm32")]
            {
                connect_ws_once_wasm(
                    epoch,
                    rows,
                    warnings,
                    errors,
                    warning_event_counter,
                    error_event_counter,
                    flight_state,
                    board_status,
                    rocket_gps,
                    user_gps,
                    alive.clone(),
                )
                .await
            }

            #[cfg(not(target_arch = "wasm32"))]
            {
                connect_ws_once_native(
                    epoch,
                    rows,
                    warnings,
                    errors,
                    warning_event_counter,
                    error_event_counter,
                    flight_state,
                    board_status,
                    rocket_gps,
                    user_gps,
                    alive.clone(),
                )
                .await
            }
        };

        if !alive.load(Ordering::Relaxed) {
            break;
        }
        if *WS_EPOCH.read() != epoch {
            break;
        }

        if let Err(e) = res
            && alive.load(Ordering::Relaxed)
        {
            log!("[WS] connect error: {e}");
        }

        #[cfg(target_arch = "wasm32")]
        gloo_timers::future::TimeoutFuture::new(800).await;

        #[cfg(not(target_arch = "wasm32"))]
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }

    Ok(())
}

#[cfg(target_arch = "wasm32")]
async fn connect_ws_once_wasm(
    epoch: u64,
    rows: Signal<Vec<TelemetryRow>>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    warning_event_counter: Signal<u64>,
    error_event_counter: Signal<u64>,
    flight_state: Signal<FlightState>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    use futures_channel::oneshot;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};

    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }

    let base_ws = UrlConfig::base_ws();
    let ws_url = format!("{base_ws}/ws");

    log!("[WS] connecting to {ws_url} (epoch={epoch})");

    let ws = WebSocket::new(&ws_url).map_err(|_| "failed to create websocket".to_string())?;

    *WS_RAW.write() = Some(ws.clone());
    *WS_SENDER.write() = Some(WsSender { ws: ws.clone() });

    let (closed_tx, closed_rx) = oneshot::channel::<()>();
    let closed_tx = std::rc::Rc::new(std::cell::RefCell::new(Some(closed_tx)));

    {
        let onopen: Closure<dyn FnMut(Event)> = Closure::new(move |_e: Event| {
            log!("[WS] open");
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();
    }

    {
        let onmessage: Closure<dyn FnMut(MessageEvent)> = Closure::new(move |e: MessageEvent| {
            if let Some(s) = e.data().as_string() {
                handle_ws_message(
                    &s,
                    rows,
                    warnings,
                    errors,
                    warning_event_counter,
                    error_event_counter,
                    flight_state,
                    board_status,
                    rocket_gps,
                    user_gps,
                );
            }
        });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();
    }

    {
        let closed_tx = closed_tx.clone();
        let onerror: Closure<dyn FnMut(ErrorEvent)> = Closure::new(move |e: ErrorEvent| {
            log!("[WS] error: {}", e.message());
            if let Some(tx) = closed_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
        });
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();
    }

    {
        let closed_tx = closed_tx.clone();
        let onclose: Closure<dyn FnMut(CloseEvent)> = Closure::new(move |e: CloseEvent| {
            log!("[WS] close code={} reason='{}'", e.code(), e.reason());
            if let Some(tx) = closed_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
        });
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();
    }

    futures_util::pin_mut!(closed_rx);

    loop {
        if !alive.load(Ordering::Relaxed) {
            let _ = ws.close();
            break;
        }
        if *WS_EPOCH.read() != epoch {
            let _ = ws.close();
            break;
        }

        let done = futures_util::future::select(
            &mut closed_rx,
            gloo_timers::future::TimeoutFuture::new(150),
        )
        .await;

        match done {
            futures_util::future::Either::Left((_closed, _timeout)) => break,
            futures_util::future::Either::Right((_timeout, _closed)) => {}
        }
    }

    if *WS_EPOCH.read() == epoch {
        *WS_SENDER.write() = None;
        *WS_RAW.write() = None;
    }

    Err("websocket closed".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn connect_ws_once_native(
    epoch: u64,
    rows: Signal<Vec<TelemetryRow>>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    warning_event_counter: Signal<u64>,
    error_event_counter: Signal<u64>,
    flight_state: Signal<FlightState>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    use futures_util::{SinkExt, StreamExt};

    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }
    if *WS_EPOCH.read() != epoch {
        return Ok(());
    }

    let base_ws = UrlConfig::base_ws();
    let ws_url = format!("{base_ws}/ws");

    log!("[WS] connecting to {ws_url} (epoch={epoch})");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    *WS_SENDER.write() = Some(WsSender { tx });

    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url.as_str())
        .await
        .map_err(|e| format!("[WS] connect failed: {e}"))?;

    let (mut write, mut read) = ws_stream.split();

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let _ = write
                .send(tokio_tungstenite::tungstenite::Message::Text(msg.into()))
                .await;
        }
    });

    while alive.load(Ordering::Relaxed) && *WS_EPOCH.read() == epoch {
        let Some(item) = read.next().await else { break };

        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                log!("[WS] read error: {e}");
                break;
            }
        };

        if let tokio_tungstenite::tungstenite::Message::Text(s) = msg {
            handle_ws_message(
                &s,
                rows,
                warnings,
                errors,
                warning_event_counter,
                error_event_counter,
                flight_state,
                board_status,
                rocket_gps,
                user_gps,
            );
        }
    }

    writer.abort();
    *WS_SENDER.write() = None;

    Err("websocket closed".to_string())
}

#[allow(clippy::too_many_arguments)]
fn handle_ws_message(
    s: &str,
    rows: Signal<Vec<TelemetryRow>>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    warning_event_counter: Signal<u64>,
    error_event_counter: Signal<u64>,
    flight_state: Signal<FlightState>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
) {
    let mut warnings = warnings;
    let mut errors = errors;
    let mut warning_event_counter = warning_event_counter;
    let mut error_event_counter = error_event_counter;
    let mut flight_state = flight_state;
    let mut board_status = board_status;
    let mut rocket_gps = rocket_gps;
    let _user_gps = user_gps;

    // NOTE: `rows` is no longer written here; we batch-update it in the UI flush loop.
    let _rows = rows;

    let Ok(msg) = serde_json::from_str::<WsInMsg>(s) else {
        return;
    };

    match msg {
        WsInMsg::Telemetry(row) => {
            // Always keep chart cache hot (cheap, incremental)
            charts_cache_ingest_row(&row);

            if let Some((lat, lon)) = row_to_gps(&row) {
                rocket_gps.set(Some((lat, lon)));
            }

            // Queue telemetry for UI batch flush
            if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
                q.push_back(row);

                // Safety cap if UI stalls
                const MAX_QUEUE: usize = 30_000;
                while q.len() > MAX_QUEUE {
                    q.pop_front();
                }
            }
        }

        WsInMsg::FlightState(st) => {
            flight_state.set(st.state);
        }

        WsInMsg::Warning(w) => {
            let mut v = warnings.read().clone();
            v.insert(0, w.clone());
            if v.len() > 500 {
                v.truncate(500);
            }
            warnings.set(v);
            let next = warning_event_counter.read().saturating_add(1);
            warning_event_counter.set(next);
        }

        WsInMsg::Error(e) => {
            let mut v = errors.read().clone();
            v.insert(0, e.clone());
            if v.len() > 500 {
                v.truncate(500);
            }
            errors.set(v);
            let next = error_event_counter.read().saturating_add(1);
            error_event_counter.set(next);
        }

        WsInMsg::BoardStatus(status) => {
            board_status.set(status.boards);
        }
    }
}

// --------------------------------------------------------------------------------------------
// JS helpers
// --------------------------------------------------------------------------------------------
fn js_read_window_string(key: &str) -> Option<String> {
    js_eval(&format!(
        r#"
        (function() {{
          try {{
            const v = window[{key:?}];
            window.__gs26_tmp_str = (typeof v === "string") ? v : "";
          }} catch (e) {{
            window.__gs26_tmp_str = "";
          }}
        }})();
        "#
    ));

    js_get_tmp_str()
}

#[cfg(target_arch = "wasm32")]
fn js_eval(js: &str) {
    let _ = js_sys::eval(js);
}

#[cfg(not(target_arch = "wasm32"))]
fn js_eval(js: &str) {
    dioxus::document::eval(js);
}

#[cfg(target_arch = "wasm32")]
fn js_get_tmp_str() -> Option<String> {
    let win = web_sys::window()?;
    let v = js_sys::Reflect::get(&win, &wasm_bindgen::JsValue::from_str("__gs26_tmp_str")).ok()?;
    v.as_string()
}

#[cfg(not(target_arch = "wasm32"))]
fn js_get_tmp_str() -> Option<String> {
    None
}

fn js_is_ground_map_ready() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    return true;

    #[cfg(target_arch = "wasm32")]
    {
        js_eval(
            r#"
        (function() {
          try {
            const ok =
              (window.__gs26_ground_station_loaded === true) &&
              (typeof window.updateGroundMapMarkers === "function") &&
              (typeof window.initGroundMap === "function");

            window.__gs26_tmp_ready = ok ? "true" : "false";
          } catch (e) {
            window.__gs26_tmp_ready = "false";
          }
        })();
        "#,
        );

        js_read_window_string("__gs26_tmp_ready")
            .unwrap_or_else(|| "false".to_string())
            .eq_ignore_ascii_case("true")
    }
}
