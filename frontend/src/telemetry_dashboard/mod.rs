// frontend/src/telemetry_dashboard/mod.rs

mod actions_tab;
mod connection_status_tab;
pub mod data_chart;
pub mod data_tab;
pub mod errors_tab;
mod gps;
mod gps_android;
mod latency_chart;
pub mod layout;
mod notifications_tab;
pub mod types;

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
use layout::LayoutConfig;
use map_tab::MapTab;
use notifications_tab::NotificationsTab;
use serde::{Deserialize, Serialize};
use state_tab::StateTab;
use types::{BoardStatusEntry, BoardStatusMsg, FlightState, TelemetryRow};
use warnings_tab::WarningsTab;

use std::collections::{HashMap, HashSet, VecDeque};
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
static RESEED_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

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

#[derive(Deserialize, Debug)]
#[serde(tag = "ty", content = "data")]
enum WsInMsg {
    Telemetry(TelemetryRow),
    TelemetryBatch(Vec<TelemetryRow>),
    FlightState(FlightStateMsg),
    Warning(AlertMsg),
    Error(AlertMsg),
    BoardStatus(BoardStatusMsg),
    Notifications(Vec<PersistentNotification>),
    ActionPolicy(ActionPolicyMsg),
    NetworkTime(NetworkTimeMsg),
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

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlinkMode {
    None,
    Slow,
    Fast,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActionControl {
    pub cmd: String,
    pub enabled: bool,
    pub blink: BlinkMode,
    pub actuated: Option<bool>,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActionPolicyMsg {
    pub key_enabled: bool,
    pub controls: Vec<ActionControl>,
}

impl ActionPolicyMsg {
    fn default_locked() -> Self {
        Self {
            key_enabled: false,
            controls: Vec::new(),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PersistentNotification {
    pub id: u64,
    pub timestamp_ms: i64,
    pub message: String,
    #[serde(default = "default_notification_persistent")]
    pub persistent: bool,
}

fn default_notification_persistent() -> bool {
    true
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DismissedNotification {
    id: u64,
    timestamp_ms: i64,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NetworkTimeMsg {
    pub timestamp_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NetworkTimeSync {
    network_ms: i64,
    received_mono_ms: f64,
}

#[cfg(target_arch = "wasm32")]
fn monotonic_now_ms() -> f64 {
    js_sys::Date::now()
}

#[cfg(not(target_arch = "wasm32"))]
fn monotonic_now_ms() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;

    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

#[inline]
fn compensated_network_time_ms(sync: NetworkTimeSync) -> i64 {
    let elapsed_ms = (monotonic_now_ms() - sync.received_mono_ms)
        .max(0.0)
        .round() as i64;
    sync.network_ms.saturating_add(elapsed_ms)
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn format_timestamp_ms_clock(ms_epoch: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms_epoch as f64));
    let h24 = d.get_hours();
    let (h, am_pm) = match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
    };
    let m = d.get_minutes();
    let s = d.get_seconds();
    let cs = (d.get_milliseconds() / 10).clamp(0, 99);
    format!("{h:02}:{m:02}:{s:02}:{cs:02} {am_pm}")
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn format_timestamp_ms_clock(ms_epoch: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(dt) = Local.timestamp_millis_opt(ms_epoch).single() else {
        return "--:--:--:--".to_string();
    };
    let cs = dt.timestamp_subsec_millis() / 10;
    format!("{}:{cs:02} {}", dt.format("%I:%M:%S"), dt.format("%p"))
}

fn format_network_time(ms_epoch: i64) -> String {
    format_timestamp_ms_clock(ms_epoch)
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
    Notifications,
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
const STARTUP_SEED_DELAY_MS: u64 = 1_200;

// unified storage keys
const WARNING_ACK_STORAGE_KEY: &str = "gs_last_warning_ack_ts";
const ERROR_ACK_STORAGE_KEY: &str = "gs_last_error_ack_ts";
const MAIN_TAB_STORAGE_KEY: &str = "gs_main_tab";
const DATA_TAB_STORAGE_KEY: &str = "gs_data_tab";
const BASE_URL_STORAGE_KEY: &str = "gs_base_url";
const LAYOUT_CACHE_KEY: &str = "gs_layout_cache_v4";
const NOTIFICATION_DISMISSED_STORAGE_KEY: &str = "gs_notification_dismissed_ids_v1";
const _SKIP_TLS_VERIFY_KEY_PREFIX: &str = "gs_skip_tls_verify_";
const NOTIFICATION_AUTO_DISMISS_MS: u32 = 5_000;
const MAX_ACTIVE_NOTIFICATIONS: usize = 2;
const MAX_NOTIFICATION_HISTORY: usize = 500;

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

pub fn map_tiles_url() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if UrlConfig::_skip_tls_verify() {
            return "gs26://local/tiles/{z}/{x}/{y}.jpg".to_string();
        }
    }

    abs_http("/tiles/{z}/{x}/{y}.jpg")
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn persisted_base_http_for_native_io() -> String {
    persist::get_string(BASE_URL_STORAGE_KEY)
        .map(normalize_base_url)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:3000".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn persisted_skip_tls_for_base_for_native_io(base: &str) -> bool {
    persist::get_string(&_tls_skip_key(base))
        .map(|v| v == "true")
        .unwrap_or(false)
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
        MainTab::Notifications => "notifications",
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
        "notifications" => MainTab::Notifications,
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

    pub fn _set_skip_tls_verify_for_base(base: &str, value: bool) {
        let clean = normalize_base_url(base.to_string());
        if clean.is_empty() {
            return;
        }
        let key = _tls_skip_key(&clean);
        persist::set_string(&key, if value { "true" } else { "false" });
    }

    pub fn _skip_tls_verify_for_base(base: &str) -> bool {
        let clean = normalize_base_url(base.to_string());
        if clean.is_empty() {
            return false;
        }
        let key = _tls_skip_key(&clean);
        persist::get_string(&key)
            .map(|v| v == "true")
            .unwrap_or(false)
    }

    pub fn _set_skip_tls_verify(value: bool) {
        let base = UrlConfig::base_http();
        UrlConfig::_set_skip_tls_verify_for_base(&base, value);
    }

    pub fn _skip_tls_verify() -> bool {
        let base = UrlConfig::base_http();
        UrlConfig::_skip_tls_verify_for_base(&base)
    }
}

fn _tls_skip_key(base: &str) -> String {
    let mut cleaned = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() {
            cleaned.push(ch.to_ascii_lowercase());
        } else {
            cleaned.push('_');
        }
    }
    format!("{_SKIP_TLS_VERIFY_KEY_PREFIX}{cleaned}")
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

    // Native: keep current UI mounted so charts/history remain visible while reseed runs.
}

fn clear_telemetry_runtime_buffers() {
    if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
        q.clear();
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
    fn send_cmd(&self, cmd: &str) -> Result<(), String> {
        let msg = format!(r#"{{"cmd":"{}"}}"#, cmd);

        #[cfg(target_arch = "wasm32")]
        {
            self.ws
                .send_with_str(&msg)
                .map_err(|_| "ws send failed".to_string())?;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            self.tx
                .send(msg)
                .map_err(|_| "ws channel closed".to_string())?;
        }

        Ok(())
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

    let layout_config = use_signal(|| None::<LayoutConfig>);
    let layout_loading = use_signal(|| true);
    let layout_error = use_signal(|| None::<String>);
    let did_request_layout = use_signal(|| false);
    let startup_seed_ready = use_signal(|| false);

    let parse_i64 = |s: &str| s.parse::<i64>().unwrap_or(0);

    // ----------------------------
    // Live app state
    // ----------------------------
    let rows = use_signal(Vec::<TelemetryRow>::new);

    let active_data_tab = use_signal(|| st_data_tab.read().clone());
    let warnings = use_signal(Vec::<AlertMsg>::new);
    let errors = use_signal(Vec::<AlertMsg>::new);
    let notifications = use_signal(Vec::<PersistentNotification>::new);
    let notification_history = use_signal(Vec::<PersistentNotification>::new);
    let dismissed_notifications = use_signal(load_dismissed_notifications);
    let unread_notification_ids = use_signal(Vec::<u64>::new);
    let action_policy = use_signal(ActionPolicyMsg::default_locked);
    let network_time = use_signal(|| None::<NetworkTimeSync>);
    let flight_state = use_signal(|| FlightState::Startup);
    let board_status = use_signal(Vec::<BoardStatusEntry>::new);

    let active_main_tab = use_signal(|| _main_tab_from_str(st_main_tab.read().as_str()));

    {
        let mut active_data_tab = active_data_tab;
        let layout_config = layout_config;
        use_effect(move || {
            let Some(layout) = layout_config.read().clone() else {
                return;
            };
            if layout.data_tab.tabs.is_empty() {
                return;
            }
            let current = active_data_tab.read().clone();
            if !layout.data_tab.tabs.iter().any(|t| t.id == current) {
                active_data_tab.set(layout.data_tab.tabs[0].id.clone());
            }
        });
    }

    let ack_warning_ts = use_signal(|| parse_i64(st_warn_ack.read().as_str()));
    let ack_error_ts = use_signal(|| parse_i64(st_err_ack.read().as_str()));
    let warning_event_counter = use_signal(|| 0u64);
    let error_event_counter = use_signal(|| 0u64);
    let ack_warning_count = use_signal(|| 0u64);
    let ack_error_count = use_signal(|| 0u64);

    let flash_on = use_signal(|| false);
    let clock_tick = use_signal(|| 0u64);

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

    // ---------------------------------------------------------
    // Layout config fetch + cache
    // ---------------------------------------------------------
    {
        let mut layout_config = layout_config;
        let mut layout_loading = layout_loading;
        let mut layout_error = layout_error;
        let mut did_request_layout = did_request_layout;

        use_effect(move || {
            if *did_request_layout.read() {
                return;
            }
            did_request_layout.set(true);

            if let Some(cached) = persist::get_string(LAYOUT_CACHE_KEY) {
                if let Ok(layout) = serde_json::from_str::<LayoutConfig>(&cached) {
                    layout_config.set(Some(layout));
                    layout_loading.set(false);
                }
            }

            spawn(async move {
                match http_get_json::<LayoutConfig>("/api/layout").await {
                    Ok(layout) => {
                        layout_config.set(Some(layout.clone()));
                        layout_loading.set(false);
                        layout_error.set(None);
                        if let Ok(raw) = serde_json::to_string(&layout) {
                            persist::set_string(LAYOUT_CACHE_KEY, &raw);
                        }
                    }
                    Err(err) => {
                        layout_error.set(Some(format!("Layout failed to load: {err}")));
                        if layout_config.read().is_none() {
                            layout_loading.set(false);
                        }
                    }
                }
            });
        });
    }

    // Delay the first DB seed until initial UI/layout load has settled.
    // Subsequent reseeds (button/reconnect) remain immediate.
    {
        let mut startup_seed_ready = startup_seed_ready;
        let layout_loading = layout_loading;
        let alive = alive.clone();

        use_effect(move || {
            if *startup_seed_ready.read() || *layout_loading.read() {
                return;
            }

            let alive = alive.clone();
            spawn(async move {
                let delay_ms: u64 = std::env::var("GS_UI_STARTUP_SEED_DELAY_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(STARTUP_SEED_DELAY_MS)
                    .clamp(0, 15_000);

                #[cfg(target_arch = "wasm32")]
                gloo_timers::future::TimeoutFuture::new(delay_ms as u32).await;

                #[cfg(not(target_arch = "wasm32"))]
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                startup_seed_ready.set(true);
            });
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
        let mut notifications_s = notifications;
        let mut notification_history_s = notification_history;
        let mut dismissed_notifications_s = dismissed_notifications;
        let mut unread_notification_ids_s = unread_notification_ids;
        let mut action_policy_s = action_policy;
        let mut network_time_s = network_time;

        let alive = alive.clone();
        let startup_seed_ready = startup_seed_ready;

        use_effect(move || {
            let current_seed = *SEED_EPOCH.read();
            if last_seed_epoch.read().as_ref() == Some(&current_seed) {
                return;
            }

            // Startup seed waits until layout has loaded and a short settle delay completes.
            // Explicit reseeds (seed epoch > 0) bypass this gate.
            if current_seed == 0 && !*startup_seed_ready.read() {
                return;
            }
            last_seed_epoch.set(Some(current_seed));

            // Keep current in-memory rows visible until reseed data arrives.
            // This avoids visible graph "blanking" during reconnect/reseed.

            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();
            spawn(async move {
                if !alive.load(Ordering::Relaxed) || *WS_EPOCH.read() != epoch {
                    return;
                }

                let mut last_err: Option<String> = None;
                const RESEED_ATTEMPTS: usize = 3;
                for attempt in 1..=RESEED_ATTEMPTS {
                    let res = seed_from_db(
                        &mut rows_s,
                        &mut warnings_s,
                        &mut errors_s,
                        &mut notifications_s,
                        &mut notification_history_s,
                        &mut dismissed_notifications_s,
                        &mut unread_notification_ids_s,
                        &mut action_policy_s,
                        &mut network_time_s,
                        &mut board_status_s,
                        &mut rocket_gps_s,
                        &mut user_gps_s,
                        &mut ack_warning_ts_s,
                        &mut ack_error_ts_s,
                        alive.clone(),
                    )
                        .await;

                    match res {
                        Ok(()) => {
                            last_err = None;
                            break;
                        }
                        Err(e) => {
                            last_err = Some(e);
                            if attempt < RESEED_ATTEMPTS
                                && alive.load(Ordering::Relaxed)
                                && *WS_EPOCH.read() == epoch
                            {
                                #[cfg(target_arch = "wasm32")]
                                gloo_timers::future::TimeoutFuture::new(400 * attempt as u32).await;

                                #[cfg(not(target_arch = "wasm32"))]
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    400 * attempt as u64,
                                ))
                                    .await;
                            }
                        }
                    }
                }

                if let Some(e) = last_err
                    && alive.load(Ordering::Relaxed)
                    && *WS_EPOCH.read() == epoch
                {
                    log!("seed_from_db failed after retries: {e}");
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

    // Rocket clock loop: keep header time moving even when no other UI state changes.
    {
        let mut clock_tick = clock_tick;
        let alive = alive.clone();

        use_effect(move || {
            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();
            spawn(async move {
                while alive.load(Ordering::Relaxed) && *WS_EPOCH.read() == epoch {
                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(100).await;

                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                    if !alive.load(Ordering::Relaxed) || *WS_EPOCH.read() != epoch {
                        break;
                    }

                    let next = clock_tick.read().saturating_add(1);
                    clock_tick.set(next);
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
    let has_unread_notifications = !unread_notification_ids.read().is_empty();

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

    // Checking the Notifications tab dismisses currently active notifications
    // and clears the unread indicator.
    {
        let notifications = notifications;
        let dismissed_notifications = dismissed_notifications;
        let unread_notification_ids = unread_notification_ids;
        use_effect(move || {
            if *active_main_tab.read() == MainTab::Notifications {
                dismiss_all_active_notifications_local_and_remote(
                    notifications,
                    dismissed_notifications,
                    unread_notification_ids,
                );
            }
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
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
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

    let layout_config = layout_config;
    let mut layout_loading = layout_loading;
    let mut layout_error = layout_error;
    let refresh_layout = move || {
        layout_loading.set(true);
        layout_error.set(None);
        persist::_remove(LAYOUT_CACHE_KEY);
        let mut layout_config = layout_config;
        let mut layout_loading = layout_loading;
        let mut layout_error = layout_error;
        spawn(async move {
            match http_get_json::<LayoutConfig>("/api/layout").await {
                Ok(layout) => {
                    layout_config.set(Some(layout.clone()));
                    layout_loading.set(false);
                    layout_error.set(None);
                    if let Ok(raw) = serde_json::to_string(&layout) {
                        persist::set_string(LAYOUT_CACHE_KEY, &raw);
                    }
                }
                Err(err) => {
                    layout_error.set(Some(format!("Layout failed to load: {err}")));
                    if layout_config.read().is_none() {
                        layout_loading.set(false);
                    }
                }
            }
        });
    };

    // Reload button (web: full reload, native: remount inner UI)
    let mut rows = rows;
    let mut _refresh_layout = refresh_layout;
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
                // Clear transient telemetry buffers first.
                clear_telemetry_runtime_buffers();
                charts_cache_request_refit();

                // Native reload should visibly clear graph history immediately.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    rows.set(Vec::new());
                    charts_cache_reset_and_ingest(&[]);
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    _refresh_layout();
                }
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

    let layout_snapshot = layout_config.read().clone();
    let layout_error_snapshot = layout_error.read().clone();
    let layout_loading_snapshot = *layout_loading.read();
    let _clock_tick_snapshot = *clock_tick.read();
    let network_time_snapshot = network_time
        .read()
        .as_ref()
        .copied()
        .map(compensated_network_time_ms)
        .map(format_network_time);

    // MAIN UI
    rsx! {
    gps::GpsDriver {
        user_gps: user_gps,
        // Only needed if you want to gate geolocation until the JS is ready on wasm:
        js_ready: Some(start_gps_js()),
    }
        style {
            "@keyframes gs26-blink-slow-off {{ 0%, 100% {{ opacity: 0.2; }} 18% {{ opacity: 1.0; }} }}
             @keyframes gs26-blink-slow-on  {{ 0%, 100% {{ opacity: 1.0; }} 82% {{ opacity: 0.25; }} }}
             @keyframes gs26-blink-fast-off {{ 0%, 100% {{ opacity: 0.15; }} 45% {{ opacity: 1.0; }} }}
             @keyframes gs26-blink-fast-on  {{ 0%, 100% {{ opacity: 1.0; }} 55% {{ opacity: 0.2; }} }}"
        }
        if layout_loading_snapshot && layout_snapshot.is_none() {
            div {
                style: "
                    height:100vh;
                    padding:24px;
                    color:#e5e7eb;
                    font-family:system-ui, -apple-system, BlinkMacSystemFont;
                    background:#020617;
                    display:flex;
                    align-items:center;
                    justify-content:center;
                    border:{border_style};
                    box-sizing:border-box;
                ",
                div { style: "text-align:center; display:flex; flex-direction:column; gap:10px;",
                    div { style: "font-size:22px; font-weight:800; color:#f97316;", "Loading layout..." }
                    div { style: "font-size:14px; color:#94a3b8;", "Waiting for layout from backend" }
                }
            }
        } else if layout_snapshot.is_none() {
            div {
                style: "
                    height:100vh;
                    padding:24px;
                    color:#e5e7eb;
                    font-family:system-ui, -apple-system, BlinkMacSystemFont;
                    background:#020617;
                    display:flex;
                    align-items:center;
                    justify-content:center;
                    border:{border_style};
                    box-sizing:border-box;
                ",
                div { style: "text-align:center; display:flex; flex-direction:column; gap:12px; align-items:center;",
                    div { style: "font-size:20px; font-weight:800; color:#ef4444;", "Layout failed to load" }
                    if let Some(msg) = layout_error_snapshot.clone() {
                        div { style: "font-size:13px; color:#94a3b8;", "{msg}" }
                    }
                    div { style: "display:flex; gap:10px; flex-wrap:wrap; justify-content:center;",
                        {reload_button}
                        {connect_button}
                    }
                }
            }
        } else if let Some(layout) = layout_snapshot {
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

            if let Some(msg) = layout_error_snapshot.clone() {
                div { style: "margin-bottom:12px; padding:10px 12px; border-radius:10px; border:1px solid #ef4444; background:#450a0a; color:#fecaca; font-size:12px;",
                    "{msg}"
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
                            style: if *active_main_tab.read() == MainTab::Notifications { tab_style_active("#3b82f6") } else { tab_style_inactive.to_string() },
                            onclick: {
                                let mut t = active_main_tab;
                                let notifications = notifications;
                                let dismissed_notifications = dismissed_notifications;
                                let unread_notification_ids = unread_notification_ids;
                                move |_| {
                                    t.set(MainTab::Notifications);
                                    dismiss_all_active_notifications_local_and_remote(
                                        notifications,
                                        dismissed_notifications,
                                        unread_notification_ids,
                                    );
                                }
                            },
                            span { "Notifications" }
                            if has_unread_notifications {
                                span { style: "margin-left:6px; color:#93c5fd;", "●" }
                            }
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
                    if let Some(ts) = network_time_snapshot {
                        span { style: "color:#cbd5e1; margin-left:0.5rem;", "(Rocket Time: {ts})" }
                    }

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
            if !notifications.read().is_empty() {
                div {
                    style: "display:flex; flex-direction:column; gap:8px; margin-bottom:10px;",
                    for n in notifications.read().iter() {
                        div {
                            style: "display:flex; align-items:center; gap:10px; padding:10px 12px; border:1px solid #2563eb; border-radius:10px; background:#0b1f4d; color:#bfdbfe;",
                            span { style: "flex:1;", "{n.message}" }
                            button {
                                style: "padding:0.2rem 0.55rem; border-radius:999px; border:1px solid #1d4ed8; background:#111827; color:#bfdbfe; font-size:0.75rem; cursor:pointer;",
                                onclick: {
                                    let id = n.id;
                                    let ts = n.timestamp_ms;
                                    let mut notifications = notifications;
                                    let mut dismissed_notifications = dismissed_notifications;
                                    let mut unread_notification_ids = unread_notification_ids;
                                    move |_| {
                                        let mut v = notifications.read().clone();
                                        v.retain(|x| x.id != id);
                                        notifications.set(v);
                                        let mut unread = unread_notification_ids.read().clone();
                                        unread.retain(|x| *x != id);
                                        unread_notification_ids.set(unread);
                                        let mut ids = dismissed_notifications.read().clone();
                                        let item = DismissedNotification {
                                            id,
                                            timestamp_ms: ts,
                                        };
                                        if !ids.contains(&item) {
                                            ids.push(item);
                                            ids.sort_by_key(|x| (x.id, x.timestamp_ms));
                                            dismissed_notifications.set(ids.clone());
                                            persist_dismissed_notifications(&ids);
                                        }
                                        spawn_detached(async move {
                                            let _ = dismiss_notification_remote(id).await;
                                        });
                                    }
                                },
                                "Dismiss"
                            }
                        }
                    }
                }
            }

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
                                    layout: layout.state_tab.clone(),
                                    actions: layout.actions_tab.clone(),
                                    action_policy: action_policy,
                                    default_valve_labels: layout
                                        .data_tab
                                        .tabs
                                        .iter()
                                        .find(|t| t.id == "VALVE_STATE")
                                        .and_then(|t| t.boolean_labels.clone()),
                                }
                            }
                    },
                    MainTab::ConnectionStatus => rsx! {
                        ConnectionStatusTab {
                            boards: board_status,
                            layout: layout.connection_tab.clone(),
                        }
                    },
                    MainTab::Map => rsx! { MapTab { rocket_gps: rocket_gps, user_gps: user_gps } },
                    MainTab::Actions => rsx! {
                        div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                            ActionsTab { layout: layout.actions_tab.clone(), action_policy: action_policy }
                        }
                    },
                    MainTab::Notifications => rsx! {
                        div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                            NotificationsTab { history: notification_history }
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
                    MainTab::Data => rsx! {
                        DataTab {
                            rows: rows,
                            active_tab: active_data_tab,
                            layout: layout.data_tab.clone(),
                        }
                    },
                }
            }
        }
        }
    }
}

fn send_cmd(cmd: &str) {
    if let Some(sender) = WS_SENDER.read().clone()
        && let Err(e) = sender.send_cmd(cmd)
    {
        log!("[CMD] ws send failed for '{cmd}': {e}");
    }
}

fn row_to_gps(row: &TelemetryRow) -> Option<(f64, f64)> {
    let is_gps_type = matches!(row.data_type.as_str(), "GPS" | "GPS_DATA" | "ROCKET_GPS");
    if !is_gps_type {
        return None;
    }
    Some((
        row.values.get(0).copied().flatten()? as f64,
        row.values.get(1).copied().flatten()? as f64,
    ))
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

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(UrlConfig::_skip_tls_verify())
        .build()
        .map_err(|e| e.to_string())?;

    client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<T>()
        .await
        .map_err(|e| e.to_string())
}

#[cfg(target_arch = "wasm32")]
async fn http_post_empty(path: &str) -> Result<(), String> {
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

    Request::post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
async fn http_post_empty(path: &str) -> Result<(), String> {
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

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(UrlConfig::_skip_tls_verify())
        .build()
        .map_err(|e| e.to_string())?;

    client.post(url).send().await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn dismiss_notification_remote(id: u64) -> Result<(), String> {
    http_post_empty(&format!("/api/notifications/{id}/dismiss")).await
}

#[cfg(target_arch = "wasm32")]
fn spawn_detached<F>(fut: F)
where
    F: std::future::Future<Output=()> + 'static,
{
    wasm_bindgen_futures::spawn_local(fut);
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_detached<F>(fut: F)
where
    F: std::future::Future<Output=()> + 'static,
{
    spawn(fut);
}

fn load_dismissed_notifications() -> Vec<DismissedNotification> {
    persist::get_string(NOTIFICATION_DISMISSED_STORAGE_KEY)
        .and_then(|raw| serde_json::from_str::<Vec<DismissedNotification>>(&raw).ok())
        .unwrap_or_default()
}

fn persist_dismissed_notifications(items: &[DismissedNotification]) {
    if let Ok(raw) = serde_json::to_string(items) {
        persist::set_string(NOTIFICATION_DISMISSED_STORAGE_KEY, &raw);
    }
}

async fn cooperative_yield() {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(0).await;

    #[cfg(not(target_arch = "wasm32"))]
    tokio::task::yield_now().await;
}

fn dismiss_all_active_notifications_local_and_remote(
    notifications: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
) {
    let mut notifications = notifications;
    let mut dismissed_notifications = dismissed_notifications;
    let mut unread_notification_ids = unread_notification_ids;

    let active = { notifications.read().clone() };
    if active.is_empty() {
        unread_notification_ids.set(Vec::new());
        return;
    }

    notifications.set(Vec::new());
    unread_notification_ids.set(Vec::new());

    let mut ids = { dismissed_notifications.read().clone() };
    let mut changed = false;
    for n in &active {
        let item = DismissedNotification {
            id: n.id,
            timestamp_ms: n.timestamp_ms,
        };
        if !ids.contains(&item) {
            ids.push(item);
            changed = true;
        }
    }
    if changed {
        ids.sort_by_key(|x| (x.id, x.timestamp_ms));
        dismissed_notifications.set(ids.clone());
        persist_dismissed_notifications(&ids);
    }

    for n in active {
        let id = n.id;
        spawn_detached(async move {
            let _ = dismiss_notification_remote(id).await;
        });
    }
}

fn merge_notification_history(
    history: &mut Vec<PersistentNotification>,
    incoming: &[PersistentNotification],
) {
    let mut seen: HashSet<(u64, i64)> = history.iter().map(|n| (n.id, n.timestamp_ms)).collect();
    for n in incoming {
        if seen.insert((n.id, n.timestamp_ms)) {
            history.push(n.clone());
        }
    }
    history.sort_by_key(|n| -n.timestamp_ms);
    if history.len() > MAX_NOTIFICATION_HISTORY {
        history.truncate(MAX_NOTIFICATION_HISTORY);
    }
}

fn apply_notifications_snapshot(
    incoming: Vec<PersistentNotification>,
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
) {
    let mut notification_history = notification_history;
    let mut notifications = notifications;
    let mut dismissed_notifications = dismissed_notifications;
    let mut unread_notification_ids = unread_notification_ids;

    // Always keep local history of all notifications (active + dismissed).
    let mut history = { notification_history.read().clone() };
    merge_notification_history(&mut history, &incoming);
    notification_history.set(history);

    // Active notifications come directly from backend snapshot.
    // Backend dismiss endpoint is source of truth; local cache is only for local bookkeeping.
    let mut active: Vec<PersistentNotification> = incoming;
    active.sort_by_key(|n| n.timestamp_ms);

    // Keep only latest N active notifications and auto-dismiss oldest overflow.
    if active.len() > MAX_ACTIVE_NOTIFICATIONS {
        let overflow = active.len() - MAX_ACTIVE_NOTIFICATIONS;
        let overflow_items: Vec<DismissedNotification> = active
            .iter()
            .take(overflow)
            .map(|n| DismissedNotification {
                id: n.id,
                timestamp_ms: n.timestamp_ms,
            })
            .collect();
        for item in overflow_items {
            let mut ids = dismissed_notifications.read().clone();
            if !ids.contains(&item) {
                ids.push(item);
                ids.sort_by_key(|x| (x.id, x.timestamp_ms));
                dismissed_notifications.set(ids.clone());
                persist_dismissed_notifications(&ids);
            }
            let id = item.id;
            spawn_detached(async move {
                let _ = dismiss_notification_remote(id).await;
            });
        }
        active = active.split_off(overflow);
    }

    let prev_ids: HashSet<u64> = { notifications.read().iter().map(|n| n.id).collect() };
    notifications.set(active.clone());

    let mut unread: HashSet<u64> = unread_notification_ids.read().iter().copied().collect();
    for n in &active {
        if !prev_ids.contains(&n.id) {
            unread.insert(n.id);
        }
    }
    let mut unread_vec: Vec<u64> = unread.into_iter().collect();
    unread_vec.sort_unstable();
    if *unread_notification_ids.read() != unread_vec {
        unread_notification_ids.set(unread_vec);
    }

    // Auto-dismiss new visible notifications after timeout.
    for n in active {
        if prev_ids.contains(&n.id) {
            continue;
        }
        if n.persistent {
            continue;
        }
        let id = n.id;
        let ts = n.timestamp_ms;
        let mut notifications = notifications;
        let mut dismissed_notifications = dismissed_notifications;
        spawn_detached(async move {
            #[cfg(target_arch = "wasm32")]
            gloo_timers::future::TimeoutFuture::new(NOTIFICATION_AUTO_DISMISS_MS).await;

            #[cfg(not(target_arch = "wasm32"))]
            tokio::time::sleep(std::time::Duration::from_millis(
                NOTIFICATION_AUTO_DISMISS_MS as u64,
            ))
                .await;

            let still_visible = { notifications.read().iter().any(|x| x.id == id) };
            if !still_visible {
                return;
            }

            let mut v = { notifications.read().clone() };
            v.retain(|x| x.id != id);
            notifications.set(v);

            let mut ids = { dismissed_notifications.read().clone() };
            let item = DismissedNotification {
                id,
                timestamp_ms: ts,
            };
            if !ids.contains(&item) {
                ids.push(item);
                ids.sort_by_key(|x| (x.id, x.timestamp_ms));
                dismissed_notifications.set(ids.clone());
                persist_dismissed_notifications(&ids);
            }

            let _ = dismiss_notification_remote(id).await;
        });
    }
}

// ------------------------------
// Seed telemetry/alerts/gps
// ------------------------------
#[allow(clippy::too_many_arguments)]
async fn seed_from_db(
    rows: &mut Signal<Vec<TelemetryRow>>,
    warnings: &mut Signal<Vec<AlertMsg>>,
    errors: &mut Signal<Vec<AlertMsg>>,
    notifications: &mut Signal<Vec<PersistentNotification>>,
    notification_history: &mut Signal<Vec<PersistentNotification>>,
    dismissed_notifications: &mut Signal<Vec<DismissedNotification>>,
    unread_notification_ids: &mut Signal<Vec<u64>>,
    action_policy: &mut Signal<ActionPolicyMsg>,
    network_time: &mut Signal<Option<NetworkTimeSync>>,
    board_status: &mut Signal<Vec<BoardStatusEntry>>,
    rocket_gps: &mut Signal<Option<(f64, f64)>>,
    user_gps: &mut Signal<Option<(f64, f64)>>,
    ack_warning_ts: &mut Signal<i64>,
    ack_error_ts: &mut Signal<i64>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    struct ReseedGuard;
    impl Drop for ReseedGuard {
        fn drop(&mut self) {
            RESEED_IN_PROGRESS.store(false, Ordering::Relaxed);
        }
    }
    RESEED_IN_PROGRESS.store(true, Ordering::Relaxed);
    let _reseed_guard = ReseedGuard;

    fn sort_rows(rows: &mut [TelemetryRow]) {
        rows.sort_by(|a, b| {
            a.timestamp_ms
                .cmp(&b.timestamp_ms)
                .then_with(|| a.data_type.cmp(&b.data_type))
        });
    }

    fn prune_history(rows: &mut Vec<TelemetryRow>) {
        if let Some(last) = rows.last() {
            let cutoff = last.timestamp_ms - HISTORY_MS;
            let start = rows.partition_point(|r| r.timestamp_ms < cutoff);
            if start > 0 {
                rows.drain(0..start);
            }
        }
    }

    fn dedupe_rows_exact(rows: Vec<TelemetryRow>) -> Vec<TelemetryRow> {
        let mut by_key: HashMap<(String, i64), TelemetryRow> = HashMap::new();
        for row in rows {
            let key = (row.data_type.clone(), row.timestamp_ms);
            match by_key.get_mut(&key) {
                Some(existing) => {
                    *existing = row;
                }
                None => {
                    by_key.insert(key, row);
                }
            }
        }
        let mut out: Vec<TelemetryRow> = by_key.into_values().collect();
        sort_rows(&mut out);
        out
    }

    fn merge_db_and_live(
        mut db_rows: Vec<TelemetryRow>,
        live_rows: Vec<TelemetryRow>,
    ) -> Vec<TelemetryRow> {
        // Keep full overlap and only dedupe exact duplicates to avoid losing sparse history.
        db_rows.extend(live_rows);
        let mut merged = dedupe_rows_exact(db_rows);
        prune_history(&mut merged);
        merged
    }

    let queue_snapshot = || -> Vec<TelemetryRow> {
        if let Ok(q) = TELEMETRY_QUEUE.lock() {
            q.iter().cloned().collect()
        } else {
            Vec::new()
        }
    };

    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }

    // ---- Telemetry history (/api/recent) ----
    let existing_rows_before_seed = rows.read().clone();
    match http_get_json::<Vec<TelemetryRow>>("/api/recent").await {
        Ok(mut list) => {
            if !alive.load(Ordering::Relaxed) {
                return Ok(());
            }

            sort_rows(&mut list);
            prune_history(&mut list);
            list = dedupe_rows_exact(list);

            // Capture rows that arrived while reseed was running and keep them.
            let mut live_rows = rows.read().clone();
            live_rows.extend(queue_snapshot());
            live_rows.extend(rows.read().clone());
            live_rows.extend(queue_snapshot());
            if !live_rows.is_empty() {
                sort_rows(&mut live_rows);
                prune_history(&mut live_rows);
                live_rows = dedupe_rows_exact(live_rows);
                list = merge_db_and_live(list, live_rows);
            }

            if let Some(gps) = list.iter().rev().find_map(row_to_gps) {
                rocket_gps.set(Some(gps));
            }

            if list.is_empty() && !existing_rows_before_seed.is_empty() {
                // Treat empty reseed as transient and keep already-visible history.
                list = existing_rows_before_seed;
            } else {
                // Rebuild chart cache from the merged list (DB + live + queued snapshots).
                // Chunk + yield so WS/UI processing remains responsive during large reseeds.
                charts_cache_reset_and_ingest(&[]);
                const RESEED_INGEST_CHUNK: usize = 1024;
                for (i, row) in list.iter().enumerate() {
                    charts_cache_ingest_row(row);
                    if i % RESEED_INGEST_CHUNK == 0 {
                        cooperative_yield().await;
                    }
                }

                // Replay whatever is currently queued right after reset so points that arrive
                // around reseed commit are not visually lost in the chart cache.
                let post_reset_queued_rows = queue_snapshot();
                for row in &post_reset_queued_rows {
                    charts_cache_ingest_row(row);
                }
                if !post_reset_queued_rows.is_empty() {
                    list.extend(post_reset_queued_rows);
                    list = dedupe_rows_exact(list);
                    prune_history(&mut list);
                }
            }
            rows.set(list);
        }
        Err(err) => {
            if existing_rows_before_seed.is_empty() {
                return Err(format!("telemetry reseed failed: {err}"));
            }
            log!("telemetry reseed failed (keeping existing history): {err}");
        }
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

    if let Ok(list) = http_get_json::<Vec<PersistentNotification>>("/api/notifications").await
        && alive.load(Ordering::Relaxed)
    {
        apply_notifications_snapshot(
            list,
            *notifications,
            *notification_history,
            *dismissed_notifications,
            *unread_notification_ids,
        );
    }

    if let Ok(policy) = http_get_json::<ActionPolicyMsg>("/api/action_policy").await
        && alive.load(Ordering::Relaxed)
    {
        action_policy.set(policy);
    }

    if let Ok(nt) = http_get_json::<NetworkTimeMsg>("/api/network_time").await
        && alive.load(Ordering::Relaxed)
    {
        network_time.set(Some(NetworkTimeSync {
            network_ms: nt.timestamp_ms,
            received_mono_ms: monotonic_now_ms(),
        }));
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
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
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
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
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
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
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
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
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
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
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
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
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

    let ws_stream = if UrlConfig::_skip_tls_verify() && ws_url.starts_with("wss://") {
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|e| format!("[WS] tls build failed: {e}"))?;
        tokio_tungstenite::connect_async_tls_with_config(
            ws_url.as_str(),
            None,
            false,
            Some(tokio_tungstenite::Connector::NativeTls(tls)),
        )
            .await
            .map_err(|e| format!("[WS] connect failed: {e}"))?
            .0
    } else {
        tokio_tungstenite::connect_async(ws_url.as_str())
            .await
            .map_err(|e| format!("[WS] connect failed: {e}"))?
            .0
    };

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
                notifications,
                notification_history,
                dismissed_notifications,
                unread_notification_ids,
                action_policy,
                network_time,
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
    // Only clear sender if this task still owns the active epoch.
    // Prevents old-epoch teardown from clobbering a freshly reconnected sender.
    if *WS_EPOCH.read() == epoch {
        *WS_SENDER.write() = None;
    }

    Err("websocket closed".to_string())
}

#[allow(clippy::too_many_arguments)]
fn handle_ws_message(
    s: &str,
    rows: Signal<Vec<TelemetryRow>>,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
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
    let notifications = notifications;
    let notification_history = notification_history;
    let dismissed_notifications = dismissed_notifications;
    let unread_notification_ids = unread_notification_ids;
    let mut action_policy = action_policy;
    let mut network_time = network_time;
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
            if !RESEED_IN_PROGRESS.load(Ordering::Relaxed) {
                // Keep chart cache hot when not reseeding.
                charts_cache_ingest_row(&row);
            }

            if let Some((lat, lon)) = row_to_gps(&row) {
                rocket_gps.set(Some((lat, lon)));
            }

            // Queue telemetry for UI batch flush
            if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
                q.push_back(row);

                // Safety cap if UI stalls
                const MAX_QUEUE: usize = 120_000;
                while q.len() > MAX_QUEUE {
                    q.pop_front();
                }
            }
        }

        WsInMsg::TelemetryBatch(batch) => {
            if batch.is_empty() {
                return;
            }
            if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
                for row in batch {
                    if !RESEED_IN_PROGRESS.load(Ordering::Relaxed) {
                        charts_cache_ingest_row(&row);
                    }
                    if let Some((lat, lon)) = row_to_gps(&row) {
                        rocket_gps.set(Some((lat, lon)));
                    }
                    q.push_back(row);
                }

                const MAX_QUEUE: usize = 120_000;
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

        WsInMsg::Notifications(list) => {
            apply_notifications_snapshot(
                list,
                notifications,
                notification_history,
                dismissed_notifications,
                unread_notification_ids,
            );
        }

        WsInMsg::ActionPolicy(policy) => {
            action_policy.set(policy);
        }

        WsInMsg::NetworkTime(t) => {
            network_time.set(Some(NetworkTimeSync {
                network_ms: t.timestamp_ms,
                received_mono_ms: monotonic_now_ms(),
            }));
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
