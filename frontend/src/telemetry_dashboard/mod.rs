#![allow(clippy::redundant_locals)]

// frontend/src/telemetry_dashboard/mod.rs

mod actions_tab;
mod calibration_tab;
mod connection_status_tab;
pub mod data_chart;
pub mod data_tab;
mod detailed_tab;
pub mod errors_tab;
mod gps;
pub(crate) mod gps_android;
mod gps_webview;
mod latency_chart;
pub mod layout;
mod layout_settings_tab;
mod network_topology_tab;
mod notifications_tab;
pub mod types;
#[cfg(not(target_arch = "wasm32"))]
pub mod version_page;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod gps_apple;

pub mod map_tab;
pub mod state_tab;
pub mod warnings_tab;

use crate::app::Route;
use crate::auth;
#[cfg(not(target_arch = "wasm32"))]
use data_chart::charts_cache_reset_and_ingest;
use data_chart::{
    charts_cache_begin_reseed_build, charts_cache_cancel_reseed_build,
    charts_cache_finish_reseed_build, charts_cache_ingest_row, charts_cache_request_refit,
    charts_cache_reseed_ingest_row,
};

use crate::telemetry_dashboard::actions_tab::ActionsTab;
use calibration_tab::CalibrationTab;
use connection_status_tab::ConnectionStatusTab;
use data_tab::DataTab;
use detailed_tab::DetailedTab;
use dioxus::prelude::*;
use dioxus_signals::Signal;
use errors_tab::ErrorsTab;
use layout::LayoutConfig;
use layout_settings_tab::SettingsPage;
use map_tab::MapTab;
use network_topology_tab::NetworkTopologyTab;
use notifications_tab::NotificationsTab;
use serde::{Deserialize, Serialize};
use state_tab::StateTab;
use types::{BoardStatusEntry, BoardStatusMsg, FlightState, NetworkTopologyMsg, TelemetryRow};
#[cfg(not(target_arch = "wasm32"))]
use version_page::VersionTab;
use warnings_tab::WarningsTab;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
static RESEED_LIVE_BUFFER: Lazy<Mutex<Vec<TelemetryRow>>> = Lazy::new(|| Mutex::new(Vec::new()));
static DASHBOARD_HAS_CONNECTED: AtomicBool = AtomicBool::new(false);
static LAST_WS_CONNECT_WARNING: Lazy<Mutex<Option<(String, i64)>>> = Lazy::new(|| Mutex::new(None));
static FRONTEND_NETWORK_METRICS_STATE: Lazy<Mutex<FrontendNetworkMetrics>> =
    Lazy::new(|| Mutex::new(FrontendNetworkMetrics::default()));
static TRANSLATION_MISS_QUEUE: Lazy<Mutex<HashSet<String>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));
static TRANSLATION_REQUEST_ACTIVE: AtomicBool = AtomicBool::new(false);

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
    /// Creates a dashboard lifetime marker that is already considered torn down.
    fn _new_dead() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(false)),
            r#gen: 0,
        }
    }
    /// Creates a dashboard lifetime marker for a freshly mounted dashboard.
    fn new_alive() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            r#gen: 0,
        }
    }
}

static DASHBOARD_LIFE: GlobalSignal<DashboardLife> = Signal::global(DashboardLife::new_alive);

#[inline]
/// Returns the current shared dashboard-alive flag.
fn dashboard_alive() -> Arc<AtomicBool> {
    DASHBOARD_LIFE.read().alive.clone()
}

#[inline]
/// Replaces the dashboard lifetime flag and bumps the mount generation.
fn _set_dashboard_alive(alive: bool) {
    let alive = Arc::new(AtomicBool::new(alive));
    *DASHBOARD_LIFE.write() = DashboardLife {
        alive,
        r#gen: dashboard_gen() + 1,
    };
}

#[inline]
/// Returns the current dashboard mount generation.
fn dashboard_gen() -> u64 {
    DASHBOARD_LIFE.read().r#gen
}

// ----------------------------
// Cross-platform persistence
//  - wasm32: localStorage
//  - native: JSON file in app data dir
// ----------------------------
mod persist {
    /// Reads a persisted string value from browser storage or the native JSON store.
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

    /// Persists a string value across app launches.
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

    /// Removes a persisted key when the current platform supports it.
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

    /// Reads a stored string value or falls back to the provided default.
    pub fn get_or(key: &str, default: &str) -> String {
        get_string(key).unwrap_or_else(|| default.to_string())
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod native {
        use std::collections::HashMap;
        use std::io;

        /// Resolves the default native storage root when no platform-specific path is available.
        fn fallback_storage_base_dir() -> std::path::PathBuf {
            dirs::data_local_dir()
                .or_else(dirs::data_dir)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
        }

        #[cfg(target_os = "android")]
        /// Resolves the Android app-private storage root through JNI.
        fn android_storage_base_dir() -> Option<std::path::PathBuf> {
            use ::jni::objects::{JObject, JString};
            use ::jni::{jni_sig, jni_str, JavaVM};
            use ndk_context::android_context;

            let ctx = android_context();
            let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
            vm.attach_current_thread(|env| -> ::jni::errors::Result<std::path::PathBuf> {
                let context = unsafe { JObject::from_raw(env, ctx.context().cast()) };

                let files_dir = env
                    .call_method(
                        &context,
                        jni_str!("getFilesDir"),
                        jni_sig!("()Ljava/io/File;"),
                        &[],
                    )?
                    .l()?;
                let path_obj = env
                    .call_method(
                        &files_dir,
                        jni_str!("getAbsolutePath"),
                        jni_sig!("()Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                let path = env.as_cast::<JString>(&path_obj)?.try_to_string(env)?;

                let _ = context.into_raw();
                Ok(std::path::PathBuf::from(path))
            })
            .ok()
        }

        /// Picks the best native storage root for the JSON persistence file.
        fn storage_base_dir() -> std::path::PathBuf {
            #[cfg(target_os = "android")]
            {
                if let Some(path) = android_storage_base_dir() {
                    return path;
                }
            }

            fallback_storage_base_dir()
        }

        /// Returns the full path to the native JSON persistence file.
        fn storage_path() -> std::path::PathBuf {
            let mut base = storage_base_dir();
            base.push("gs26");
            base.push("storage.json");
            base
        }

        /// Loads the native persistence map from disk.
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

        /// Saves the native persistence map back to disk.
        fn save_map(map: &HashMap<String, String>) -> Result<(), io::Error> {
            let path = storage_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = serde_json::to_vec_pretty(map).unwrap_or_else(|_| b"{}".to_vec());
            std::fs::write(path, bytes)?;
            Ok(())
        }

        /// Reads a string key from the native persistence file.
        pub fn get_string(key: &str) -> Result<Option<String>, io::Error> {
            let map = load_map()?;
            Ok(map.get(key).cloned())
        }

        /// Writes a string key into the native persistence file.
        pub fn set_string(key: &str, value: &str) -> Result<(), io::Error> {
            let mut map = load_map()?;
            map.insert(key.to_string(), value.to_string());
            save_map(&map)?;
            Ok(())
        }

        /// Removes a key from the native persistence file.
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
    NetworkTopology(NetworkTopologyMsg),
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
    #[serde(default = "default_software_buttons_enabled")]
    pub software_buttons_enabled: bool,
    pub controls: Vec<ActionControl>,
}

impl ActionPolicyMsg {
    /// Returns the startup action policy before the backend publishes a real one.
    fn default_locked() -> Self {
        Self {
            key_enabled: false,
            software_buttons_enabled: true,
            controls: Vec::new(),
        }
    }
}

/// Provides the serde default for software action buttons.
fn default_software_buttons_enabled() -> bool {
    true
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PersistentNotification {
    pub id: u64,
    pub timestamp_ms: i64,
    pub message: String,
    #[serde(default = "default_notification_persistent")]
    pub persistent: bool,
    #[serde(default)]
    pub action_label: Option<String>,
    #[serde(default)]
    pub action_cmd: Option<String>,
}

/// Provides the serde default for notification persistence.
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

#[derive(Deserialize, Debug, Clone, Default)]
struct TranslationCatalogResponse {
    lang: String,
    translations: HashMap<String, String>,
}

#[derive(Serialize, Debug, Clone, Default)]
struct TranslationRequest {
    target_lang: String,
    texts: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct TranslationResponse {
    lang: String,
    translations: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NetworkTimeSync {
    network_ms: i64,
    received_mono_ms: f64,
}

#[cfg(target_arch = "wasm32")]
/// Returns a monotonic-ish timestamp source for rate calculations in the browser.
fn monotonic_now_ms() -> f64 {
    js_sys::Date::now()
}

#[cfg(not(target_arch = "wasm32"))]
/// Returns a monotonic timestamp source for rate calculations on native builds.
fn monotonic_now_ms() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;

    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

#[inline]
/// Projects the last synced network time forward using monotonic elapsed time.
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

/// Formats the network-synchronized wall clock for dashboard display.
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
#[derive(Deserialize, Debug, Clone, Copy)]
struct GpsPoint {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Deserialize, Debug, Clone)]
struct GpsResponse {
    pub rocket: Option<GpsPoint>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainTab {
    State,
    ConnectionStatus,
    Detailed,
    NetworkTopology,
    Map,
    Actions,
    Calibration,
    Notifications,
    Warnings,
    Errors,
    Data,
}

#[derive(Clone, Debug, PartialEq)]
struct FrontendNetworkMetrics {
    ws_connected: bool,
    ws_url: String,
    base_http: String,
    ws_epoch: u64,
    ws_disconnects_total: u64,
    ws_messages_total: u64,
    ws_bytes_total: u64,
    telemetry_rows_total: u64,
    telemetry_batches_total: u64,
    bytes_per_sec: f64,
    msgs_per_sec: f64,
    rows_per_sec: f64,
    http_rtt_ms: Option<f64>,
    http_rtt_ema_ms: Option<f64>,
    last_connect_wall_ms: Option<i64>,
    last_disconnect_reason: Option<String>,
    last_ws_message_wall_ms: Option<i64>,
    last_rate_sample_mono_ms: f64,
    bytes_since_last_sample: u64,
    msgs_since_last_sample: u64,
    rows_since_last_sample: u64,
}

impl Default for FrontendNetworkMetrics {
    fn default() -> Self {
        Self {
            ws_connected: false,
            ws_url: String::new(),
            base_http: String::new(),
            ws_epoch: 0,
            ws_disconnects_total: 0,
            ws_messages_total: 0,
            ws_bytes_total: 0,
            telemetry_rows_total: 0,
            telemetry_batches_total: 0,
            bytes_per_sec: 0.0,
            msgs_per_sec: 0.0,
            rows_per_sec: 0.0,
            http_rtt_ms: None,
            http_rtt_ema_ms: None,
            last_connect_wall_ms: None,
            last_disconnect_reason: None,
            last_ws_message_wall_ms: None,
            last_rate_sample_mono_ms: 0.0,
            bytes_since_last_sample: 0,
            msgs_since_last_sample: 0,
            rows_since_last_sample: 0,
        }
    }
}

/// Resets the frontend-side WebSocket and HTTP metrics to a clean state.
fn reset_frontend_network_metrics_state() {
    if let Ok(mut metrics) = FRONTEND_NETWORK_METRICS_STATE.lock() {
        *metrics = FrontendNetworkMetrics {
            base_http: UrlConfig::base_http(),
            ..FrontendNetworkMetrics::default()
        };
    }
}

/// Returns a snapshot of the frontend network metrics without exposing the mutex guard.
fn frontend_network_metrics_snapshot() -> FrontendNetworkMetrics {
    FRONTEND_NETWORK_METRICS_STATE
        .lock()
        .map(|metrics| metrics.clone())
        .unwrap_or_default()
}

/// Redacts authentication tokens from a WebSocket URL before it is shown in the UI.
fn redact_ws_url_for_display(ws_url: &str) -> String {
    if let Some((prefix, query)) = ws_url.split_once('?') {
        let redacted_query = query
            .split('&')
            .map(|part| {
                if let Some((key, _)) = part.split_once('=') {
                    if key == "token" {
                        return format!("{key}=<redacted>");
                    }
                } else if part == "token" {
                    return "token=<redacted>".to_string();
                }
                part.to_string()
            })
            .collect::<Vec<_>>()
            .join("&");
        format!("{prefix}?{redacted_query}")
    } else {
        ws_url.to_string()
    }
}

/// Records a WebSocket connection or disconnection transition for the dashboard diagnostics.
fn note_ws_connection_state(connected: bool, ws_url: String, reason: Option<String>, epoch: u64) {
    if connected {
        DASHBOARD_HAS_CONNECTED.store(true, Ordering::Relaxed);
        if let Ok(mut slot) = LAST_WS_CONNECT_WARNING.lock() {
            *slot = None;
        }
    }
    if let Ok(mut next) = FRONTEND_NETWORK_METRICS_STATE.lock() {
        let was_connected = next.ws_connected;
        next.ws_connected = connected;
        next.ws_url = redact_ws_url_for_display(&ws_url);
        next.base_http = UrlConfig::base_http();
        next.ws_epoch = epoch;
        if connected {
            next.last_connect_wall_ms = Some(current_wallclock_ms());
        } else if was_connected {
            next.ws_disconnects_total = next.ws_disconnects_total.saturating_add(1);
        }
        if let Some(reason) = reason {
            next.last_disconnect_reason = Some(reason);
        }
    }
}

fn note_ws_connect_failure_warning(
    warnings: &mut Signal<Vec<AlertMsg>>,
    warning_event_counter: &mut Signal<u64>,
    ws_url: &str,
    reason: &str,
) {
    let now_ms = current_wallclock_ms();
    let fingerprint = format!("{}|{}", redact_ws_url_for_display(ws_url), reason.trim());
    if let Ok(mut slot) = LAST_WS_CONNECT_WARNING.lock() {
        if let Some((last_fingerprint, last_ts)) = slot.as_ref()
            && last_fingerprint == &fingerprint
            && now_ms.saturating_sub(*last_ts) < 15_000
        {
            return;
        }
        *slot = Some((fingerprint, now_ms));
    }

    let mut list = warnings.read().clone();
    list.insert(
        0,
        AlertMsg {
            timestamp_ms: now_ms,
            message: format!(
                "WebSocket connection failed.\nURL: {}\nReason: {}",
                redact_ws_url_for_display(ws_url),
                reason.trim()
            ),
        },
    );
    if list.len() > 500 {
        list.truncate(500);
    }
    warnings.set(list);
    let next = warning_event_counter.read().saturating_add(1);
    warning_event_counter.set(next);
}

/// Tracks incoming WebSocket message volume and updates rate calculations.
fn note_incoming_ws_message(raw_bytes: usize) {
    if let Ok(mut next) = FRONTEND_NETWORK_METRICS_STATE.lock() {
        let now_mono = monotonic_now_ms();
        let now_wall = current_wallclock_ms();
        if next.last_rate_sample_mono_ms <= 0.0 {
            next.last_rate_sample_mono_ms = now_mono;
        }
        next.ws_messages_total = next.ws_messages_total.saturating_add(1);
        next.ws_bytes_total = next.ws_bytes_total.saturating_add(raw_bytes as u64);
        next.bytes_since_last_sample = next
            .bytes_since_last_sample
            .saturating_add(raw_bytes as u64);
        next.msgs_since_last_sample = next.msgs_since_last_sample.saturating_add(1);
        next.last_ws_message_wall_ms = Some(now_wall);

        let dt_ms = (now_mono - next.last_rate_sample_mono_ms).max(0.0);
        if dt_ms >= 800.0 {
            let scale = 1000.0 / dt_ms;
            next.bytes_per_sec = next.bytes_since_last_sample as f64 * scale;
            next.msgs_per_sec = next.msgs_since_last_sample as f64 * scale;
            next.rows_per_sec = next.rows_since_last_sample as f64 * scale;
            next.bytes_since_last_sample = 0;
            next.msgs_since_last_sample = 0;
            next.rows_since_last_sample = 0;
            next.last_rate_sample_mono_ms = now_mono;
        }
    }
}

/// Tracks telemetry row throughput separately from raw WebSocket message volume.
fn note_incoming_telemetry_rows(telemetry_rows: usize, telemetry_batch_count: usize) {
    if let Ok(mut next) = FRONTEND_NETWORK_METRICS_STATE.lock() {
        next.telemetry_rows_total = next
            .telemetry_rows_total
            .saturating_add(telemetry_rows as u64);
        next.telemetry_batches_total = next
            .telemetry_batches_total
            .saturating_add(telemetry_batch_count as u64);
        next.rows_since_last_sample = next
            .rows_since_last_sample
            .saturating_add(telemetry_rows as u64);
    }
}

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
fn current_wallclock_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

macro_rules! log {
    ($($t:tt)*) => {{
        let s = format!($($t)*);
        crate::telemetry_dashboard::log(&s);
    }}
}

pub const HISTORY_MS: i64 = 60_000 * 20; // 20 minutes
const UI_ROW_BUCKET_MS: i64 = 20; // Match chart bucket width in data_chart.rs.
const STARTUP_SEED_DELAY_MS: u64 = 1_200;

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct UiRowKey {
    bucket: i64,
    data_type: String,
    sender_id: String,
}

#[derive(Clone, Eq, PartialEq, Hash)]
struct LatestTelemetryKey {
    data_type: String,
    sender_id: String,
}

impl LatestTelemetryKey {
    /// Builds the cache key used for latest-row tracking.
    fn new(data_type: &str, sender_id: &str) -> Self {
        Self {
            data_type: data_type.to_string(),
            sender_id: sender_id.to_string(),
        }
    }
}

#[derive(Default)]
struct UiTelemetryStore {
    rows: BTreeMap<UiRowKey, TelemetryRow>,
}

impl UiTelemetryStore {
    /// Clears all compacted UI telemetry rows.
    fn clear(&mut self) {
        self.rows.clear();
    }

    /// Replaces the compacted UI store with a fresh telemetry snapshot.
    fn replace_from_rows(&mut self, rows: &[TelemetryRow]) {
        self.rows.clear();
        self.apply_rows(rows.iter().cloned());
    }

    /// Inserts rows into the compacted UI store, keeping only the newest row per bucket.
    fn apply_rows<I>(&mut self, rows: I)
    where
        I: IntoIterator<Item = TelemetryRow>,
    {
        for row in rows {
            // The UI only needs one representative row per bucket/sender/type tuple.
            let key = UiRowKey {
                bucket: row.timestamp_ms.div_euclid(UI_ROW_BUCKET_MS),
                data_type: row.data_type.clone(),
                sender_id: row.sender_id.clone(),
            };
            self.rows.insert(key, row);
        }

        self.prune_history();
    }

    /// Drops buckets that are older than the retained history window.
    fn prune_history(&mut self) {
        let Some((&newest_bucket, _)) = self.rows.last_key_value().map(|(k, v)| (&k.bucket, v))
        else {
            return;
        };
        let min_bucket =
            (newest_bucket * UI_ROW_BUCKET_MS - HISTORY_MS).div_euclid(UI_ROW_BUCKET_MS);
        self.rows.retain(|key, _| key.bucket >= min_bucket);
    }

    /// Returns the compacted UI store as a sorted vector.
    fn snapshot(&self) -> Vec<TelemetryRow> {
        self.rows.values().cloned().collect()
    }
}

static UI_TELEMETRY_STORE: Lazy<Mutex<UiTelemetryStore>> =
    Lazy::new(|| Mutex::new(UiTelemetryStore::default()));
static LATEST_TELEMETRY: Lazy<Mutex<HashMap<LatestTelemetryKey, TelemetryRow>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static LATEST_TELEMETRY_BY_TYPE: Lazy<Mutex<HashMap<String, TelemetryRow>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Sorts telemetry rows into a stable UI presentation order.
fn sort_rows(rows: &mut [TelemetryRow]) {
    rows.sort_by(|a, b| {
        a.timestamp_ms
            .cmp(&b.timestamp_ms)
            .then_with(|| a.sender_id.cmp(&b.sender_id))
            .then_with(|| a.data_type.cmp(&b.data_type))
    });
}

/// Trims a telemetry vector down to the retained history window.
fn prune_history(rows: &mut Vec<TelemetryRow>) {
    if let Some(last) = rows.last() {
        let cutoff = last.timestamp_ms - HISTORY_MS;
        let start = rows.partition_point(|r| r.timestamp_ms < cutoff);
        if start > 0 {
            rows.drain(0..start);
        }
    }
}

/// Compacts raw telemetry rows down to the newest row per UI bucket.
fn compact_rows_for_ui(rows: Vec<TelemetryRow>) -> Vec<TelemetryRow> {
    let mut by_key: HashMap<(String, String, i64), TelemetryRow> = HashMap::new();
    for row in rows {
        let bucket = row.timestamp_ms.div_euclid(UI_ROW_BUCKET_MS);
        let key = (row.data_type.clone(), row.sender_id.clone(), bucket);
        by_key.insert(key, row);
    }
    let mut out: Vec<TelemetryRow> = by_key.into_values().collect();
    sort_rows(&mut out);
    prune_history(&mut out);
    out
}

/// Rebuilds the latest-row indexes from a full telemetry snapshot.
fn reset_latest_telemetry(rows: &[TelemetryRow]) {
    if let Ok(mut latest) = LATEST_TELEMETRY.lock()
        && let Ok(mut latest_by_type) = LATEST_TELEMETRY_BY_TYPE.lock()
    {
        latest.clear();
        latest_by_type.clear();
        for row in rows {
            update_latest_telemetry_locked(&mut latest, &mut latest_by_type, row);
        }
    }
}

/// Inserts a single row into the latest-row indexes.
fn update_latest_telemetry(row: &TelemetryRow) {
    if let Ok(mut latest) = LATEST_TELEMETRY.lock()
        && let Ok(mut latest_by_type) = LATEST_TELEMETRY_BY_TYPE.lock()
    {
        update_latest_telemetry_locked(&mut latest, &mut latest_by_type, row);
    }
}

/// Applies latest-row replacement rules while both latest-row maps are already locked.
fn update_latest_telemetry_locked(
    latest: &mut HashMap<LatestTelemetryKey, TelemetryRow>,
    latest_by_type: &mut HashMap<String, TelemetryRow>,
    row: &TelemetryRow,
) {
    let key = LatestTelemetryKey::new(&row.data_type, &row.sender_id);
    let should_replace = latest
        .get(&key)
        .is_none_or(|existing| existing.timestamp_ms <= row.timestamp_ms);
    if should_replace {
        latest.insert(key, row.clone());
    }

    let should_replace_type = latest_by_type
        .get(&row.data_type)
        .is_none_or(|existing| existing.timestamp_ms <= row.timestamp_ms);
    if should_replace_type {
        latest_by_type.insert(row.data_type.clone(), row.clone());
    }
}

/// Returns the latest telemetry row for a given data type and optional sender.
pub(crate) fn latest_telemetry_row(
    data_type: &str,
    sender_id: Option<&str>,
) -> Option<TelemetryRow> {
    match sender_id {
        Some(sender_id) => {
            if let Ok(latest) = LATEST_TELEMETRY.lock() {
                latest
                    .get(&LatestTelemetryKey::new(data_type, sender_id))
                    .cloned()
            } else {
                None
            }
        }
        None => {
            if let Ok(latest_by_type) = LATEST_TELEMETRY_BY_TYPE.lock() {
                latest_by_type.get(data_type).cloned()
            } else {
                None
            }
        }
    }
}

/// Returns a single channel from the latest telemetry row for the given key.
pub(crate) fn latest_telemetry_value(
    data_type: &str,
    sender_id: Option<&str>,
    index: usize,
) -> Option<f32> {
    latest_telemetry_row(data_type, sender_id)
        .and_then(|row| row.values.get(index).copied().flatten())
}

/// Clears all telemetry runtime buffers used by the dashboard.
fn clear_ui_telemetry_store() {
    if let Ok(mut store) = UI_TELEMETRY_STORE.lock() {
        store.clear();
    }
    if let Ok(mut latest) = LATEST_TELEMETRY.lock() {
        latest.clear();
    }
    if let Ok(mut latest_by_type) = LATEST_TELEMETRY_BY_TYPE.lock() {
        latest_by_type.clear();
    }
    let mut epoch = TELEMETRY_RENDER_EPOCH.write();
    *epoch = epoch.wrapping_add(1);
}

/// Returns the compacted UI telemetry store as a snapshot vector.
pub(crate) fn ui_telemetry_rows_snapshot() -> Vec<TelemetryRow> {
    if let Ok(store) = UI_TELEMETRY_STORE.lock() {
        store.snapshot()
    } else {
        Vec::new()
    }
}

// unified storage keys
const WARNING_ACK_STORAGE_KEY: &str = "gs_last_warning_ack_ts";
const ERROR_ACK_STORAGE_KEY: &str = "gs_last_error_ack_ts";
const MAIN_TAB_STORAGE_KEY: &str = "gs_main_tab";
const DATA_TAB_STORAGE_KEY: &str = "gs_data_tab";
const BASE_URL_STORAGE_KEY: &str = "gs_base_url";
const MAP_DISTANCE_UNITS_STORAGE_KEY: &str = "gs_map_distance_units";
const THEME_PRESET_STORAGE_KEY: &str = "gs_theme_preset";
const LANGUAGE_STORAGE_KEY: &str = "gs_language";
const NETWORK_FLOW_ANIMATION_STORAGE_KEY: &str = "gs_network_flow_animation";
const LAYOUT_CACHE_KEY: &str = "gs_layout_cache_v8";
const NOTIFICATION_DISMISSED_STORAGE_KEY: &str = "gs_notification_dismissed_ids_v1";
const _SKIP_TLS_VERIFY_KEY_PREFIX: &str = "gs_skip_tls_verify_";
const NOTIFICATION_AUTO_DISMISS_MS: u32 = 5_000;
const MAX_ACTIVE_NOTIFICATIONS: usize = 2;
const MAX_NOTIFICATION_HISTORY: usize = 500;

// When this number changes, we tear down and rebuild the websocket connection.
static WS_EPOCH: GlobalSignal<u64> = Signal::global(|| 0);
static TELEMETRY_RENDER_EPOCH: GlobalSignal<u64> = Signal::global(|| 0);
static PREFERRED_LANGUAGE: GlobalSignal<String> = Signal::global(|| "en".to_string());
static TRANSLATION_CATALOG: GlobalSignal<HashMap<String, String>> = Signal::global(HashMap::new);
pub(crate) static APP_THEME_CONFIG: GlobalSignal<layout::ThemeConfig> = Signal::global(|| {
    let stored = persist::get_or(THEME_PRESET_STORAGE_KEY, "default");
    let preset = if stored == "layout" {
        "backend"
    } else {
        &stored
    };
    localized_theme(&layout::ThemeConfig::default(), preset)
});

#[cfg(target_arch = "wasm32")]
static WS_RAW: GlobalSignal<Option<web_sys::WebSocket>> = Signal::global(|| None);
// Force re-seed of graphs/history from backend.
static SEED_EPOCH: GlobalSignal<u64> = Signal::global(|| 0);

/// Normalizes a stored base URL down to `scheme://host[:port]`.
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

#[cfg(target_arch = "wasm32")]
/// Builds an absolute HTTP path for the web build using the active backend base URL.
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

/// Returns the tile URL template appropriate for the current platform.
pub fn map_tiles_url() -> String {
    #[cfg(target_os = "windows")]
    {
        // WebView2 cannot always resolve custom subresource schemes directly.
        // WRY maps the custom `gs26://` protocol to this host form on Windows.
        "http://gs26.localhost/tiles/{z}/{x}/{y}.jpg".to_string()
    }

    #[cfg(target_os = "android")]
    {
        // On Android, WRY rewrites custom protocols into host-mapped HTTP(S) URLs
        // like `https://gs26.local/...` before handing them back to the request handler.
        // Use HTTPS here so WebView does not block tile fetches as mixed content from
        // the secure `https://dioxus.index.html` app origin.
        "https://gs26.local/tiles/{z}/{x}/{y}.jpg".to_string()
    }

    #[cfg(target_os = "ios")]
    {
        // iOS does not use the desktop custom-protocol registration path, so
        // `gs26://...` tile URLs never reach our proxy handler there.
        format!(
            "{}/tiles/{{z}}/{{x}}/{{y}}.jpg",
            UrlConfig::base_http().trim_end_matches('/')
        )
    }

    #[cfg(all(
        not(target_arch = "wasm32"),
        not(target_os = "windows"),
        not(target_os = "android"),
        not(target_os = "ios")
    ))]
    {
        // Native WebViews can block plain-http tile fetches; always proxy through
        // our native protocol handler, which performs the upstream HTTP(S) request.
        "gs26://local/tiles/{z}/{x}/{y}.jpg".to_string()
    }

    #[cfg(target_arch = "wasm32")]
    {
        abs_http("/tiles/{z}/{x}/{y}.jpg")
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Reads the persisted backend base URL for native blocking I/O paths.
pub(crate) fn persisted_base_http_for_native_io() -> String {
    persist::get_string(BASE_URL_STORAGE_KEY)
        .map(normalize_base_url)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:3000".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
/// Reads the persisted TLS-skip flag for the supplied native base URL.
pub(crate) fn persisted_skip_tls_for_base_for_native_io(base: &str) -> bool {
    persist::get_string(&_tls_skip_key(base))
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Forces all WebSocket-backed tasks to tear down and reconnect on the next render tick.
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

/// Requests a fresh telemetry reseed from the backend.
fn bump_seed_epoch() {
    let mut epoch = SEED_EPOCH.write();
    *epoch += 1;
    log!("[seed] bump_seed_epoch -> {}", *epoch);
}

pub(crate) fn localized_copy(lang: &str, en: &str, es: &str, fr: &str) -> String {
    match lang {
        "es" => es.to_string(),
        "fr" => fr.to_string(),
        _ => en.to_string(),
    }
}

pub(crate) fn current_language() -> String {
    PREFERRED_LANGUAGE.read().clone()
}

pub(crate) fn set_preferred_language(code: &str) {
    let value = code.to_string();
    *PREFERRED_LANGUAGE.write() = value.clone();
    persist::set_string(LANGUAGE_STORAGE_KEY, &value);
}

pub(crate) fn translate_text(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() {
        return input.to_string();
    }
    if let Some(value) = TRANSLATION_CATALOG.read().get(text) {
        return value.clone();
    }
    if let Ok(mut pending) = TRANSLATION_MISS_QUEUE.lock() {
        pending.insert(text.to_string());
    }
    input.to_string()
}

fn drain_translation_misses(limit: usize, catalog: &HashMap<String, String>) -> Vec<String> {
    let Ok(mut pending) = TRANSLATION_MISS_QUEUE.lock() else {
        return Vec::new();
    };
    let mut batch = Vec::new();
    let keys: Vec<String> = pending.iter().cloned().collect();
    for key in keys {
        if batch.len() >= limit {
            break;
        }
        if catalog.contains_key(&key) {
            pending.remove(&key);
            continue;
        }
        pending.remove(&key);
        batch.push(key);
    }
    batch
}

fn merge_translation_map(items: HashMap<String, String>) {
    if items.is_empty() {
        return;
    }
    let mut next = TRANSLATION_CATALOG.read().clone();
    for (key, value) in items {
        if !key.trim().is_empty() && !value.trim().is_empty() {
            next.insert(key, value);
        }
    }
    *TRANSLATION_CATALOG.write() = next;
}

fn localized_theme(base: &layout::ThemeConfig, preset: &str) -> layout::ThemeConfig {
    if preset == "backend" || preset == "layout" {
        return base.clone();
    }

    let mut out = if preset == "default" {
        layout::ThemeConfig::default()
    } else {
        base.clone()
    };
    match preset {
        "sunset" => {
            out.app_background = "#140d0c".to_string();
            out.panel_background = "#201514".to_string();
            out.panel_background_alt = "#2b1c1a".to_string();
            out.overlay_background = "#140d0cf0".to_string();
            out.border = "#6d4538".to_string();
            out.border_strong = "#8c5a48".to_string();
            out.border_soft = "#3b2622".to_string();
            out.text_primary = "#f5e7dd".to_string();
            out.text_secondary = "#ddc3b4".to_string();
            out.text_muted = "#b99582".to_string();
            out.text_soft = "#d8a15f".to_string();
            out.button_background = "#32211d".to_string();
            out.button_border = "#9d6a50".to_string();
            out.button_text = "#f8ece3".to_string();
            out.tab_shell_background = "#201514ee".to_string();
            out.tab_shell_border = "#9d6a50".to_string();
            out.info_accent = "#e59b57".to_string();
            out.info_background = "#37201a".to_string();
            out.info_text = "#f3c79d".to_string();
            out.success_text = "#9fd28e".to_string();
            out.warning_background = "#402616".to_string();
            out.warning_border = "#d7a15c".to_string();
            out.warning_text = "#f4ddaf".to_string();
            out.error_background = "#3a171d".to_string();
            out.error_border = "#d9888f".to_string();
            out.error_text = "#f4c7cb".to_string();
        }
        "forest" => {
            out.app_background = "#091311".to_string();
            out.panel_background = "#101c19".to_string();
            out.panel_background_alt = "#162723".to_string();
            out.overlay_background = "#091311f0".to_string();
            out.border = "#335248".to_string();
            out.border_strong = "#4b6f62".to_string();
            out.border_soft = "#213630".to_string();
            out.text_primary = "#e6f0eb".to_string();
            out.text_secondary = "#bfd2c8".to_string();
            out.text_muted = "#8eafa2".to_string();
            out.text_soft = "#75a58d".to_string();
            out.button_background = "#1b2d28".to_string();
            out.button_border = "#5d8677".to_string();
            out.button_text = "#edf5f1".to_string();
            out.tab_shell_background = "#101c19ee".to_string();
            out.tab_shell_border = "#5d8677".to_string();
            out.info_accent = "#7cc7a5".to_string();
            out.info_background = "#17312a".to_string();
            out.info_text = "#c2ecd8".to_string();
            out.success_text = "#93d39f".to_string();
            out.warning_background = "#3b3320".to_string();
            out.warning_border = "#c9ad63".to_string();
            out.warning_text = "#efe0b2".to_string();
            out.error_background = "#34181d".to_string();
            out.error_border = "#cf8790".to_string();
            out.error_text = "#f0c7cc".to_string();
        }
        "high_contrast" => {
            out.app_background = "#000000".to_string();
            out.panel_background = "#0b0b0b".to_string();
            out.panel_background_alt = "#171717".to_string();
            out.overlay_background = "#000000f2".to_string();
            out.border = "#ffffff".to_string();
            out.border_strong = "#ffffff".to_string();
            out.border_soft = "#6b7280".to_string();
            out.text_primary = "#ffffff".to_string();
            out.text_secondary = "#f3f4f6".to_string();
            out.text_muted = "#d1d5db".to_string();
            out.text_soft = "#e5e7eb".to_string();
            out.button_background = "#111111".to_string();
            out.button_border = "#ffffff".to_string();
            out.button_text = "#ffffff".to_string();
            out.tab_shell_background = "#050505f2".to_string();
            out.tab_shell_border = "#ffffff".to_string();
            out.info_accent = "#60a5fa".to_string();
            out.info_background = "#0f172a".to_string();
            out.info_text = "#dbeafe".to_string();
            out.success_text = "#4ade80".to_string();
            out.warning_background = "#2b1800".to_string();
            out.warning_border = "#facc15".to_string();
            out.warning_text = "#fef08a".to_string();
            out.error_background = "#2b0a0a".to_string();
            out.error_border = "#f87171".to_string();
            out.error_text = "#fee2e2".to_string();
        }
        _ => {}
    }
    out
}

/// Returns the app-shell theme derived from the persisted preset.
///
/// Outside the live dashboard we do not have a backend-provided theme available, so
/// the "backend" preset falls back to the default theme config for shell styling.
pub fn app_shell_theme() -> layout::ThemeConfig {
    APP_THEME_CONFIG.read().clone()
}

#[component]
fn NetworkTimeBadge(network_time: Signal<Option<NetworkTimeSync>>, language: String) -> Element {
    let tick = use_signal(|| 0u64);
    {
        let mut tick = tick;
        use_effect(move || {
            spawn(async move {
                loop {
                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(100).await;

                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                    let next_tick = {
                        let current_tick = *tick.read();
                        current_tick.wrapping_add(1)
                    };
                    tick.set(next_tick);
                }
            });
        });
    }
    let _tick_snapshot = *tick.read();
    let Some(ts) = network_time
        .read()
        .as_ref()
        .copied()
        .map(compensated_network_time_ms)
        .map(format_network_time)
    else {
        return rsx! { div {} };
    };

    let label = localized_copy(&language, "Network Time", "Hora de red", "Heure réseau");
    rsx! {
        div { style: "display:flex; align-items:center; flex:0 0 auto; min-width:0;",
            span { style: "color:#cbd5e1; display:inline-flex; align-items:baseline; white-space:nowrap;",
                "({label}:"
                span {
                    style: "display:inline-flex; align-items:baseline; width:16ch; padding-left:0.4ch; white-space:nowrap; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace; font-variant-numeric:tabular-nums;",
                    span { "{ts}" }
                    span { ")" }
                }
            }
        }
    }
}
// tab <-> string
/// Converts a dashboard tab enum into its persisted string id.
fn _main_tab_to_str(tab: MainTab) -> &'static str {
    match tab {
        MainTab::State => "state",
        MainTab::ConnectionStatus => "connection-status",
        MainTab::Detailed => "detailed",
        MainTab::NetworkTopology => "network-topology",
        MainTab::Map => "map",
        MainTab::Actions => "actions",
        MainTab::Calibration => "calibration",
        MainTab::Notifications => "notifications",
        MainTab::Warnings => "warnings",
        MainTab::Errors => "errors",
        MainTab::Data => "data",
    }
}

/// Returns the default label for a dashboard tab when the layout config does not override it.
fn _default_main_tab_label(tab: MainTab) -> String {
    let lang = current_language();
    match tab {
        MainTab::State => localized_copy(&lang, "Flight", "Vuelo", "Vol"),
        MainTab::ConnectionStatus => localized_copy(
            &lang,
            "Connection Status",
            "Estado de Conexion",
            "Etat Connexion",
        ),
        MainTab::Detailed => {
            localized_copy(&lang, "Detailed Info", "Info Detallada", "Infos Detaillees")
        }
        MainTab::NetworkTopology => localized_copy(
            &lang,
            "Network Topology",
            "Topologia Red",
            "Topologie Reseau",
        ),
        MainTab::Map => localized_copy(&lang, "Map", "Mapa", "Carte"),
        MainTab::Actions => localized_copy(&lang, "Actions", "Acciones", "Actions"),
        MainTab::Calibration => localized_copy(&lang, "Calibration", "Calibracion", "Calibration"),
        MainTab::Notifications => {
            localized_copy(&lang, "Notifications", "Notificaciones", "Notifications")
        }
        MainTab::Warnings => localized_copy(&lang, "Warnings", "Avisos", "Alertes"),
        MainTab::Errors => localized_copy(&lang, "Errors", "Errores", "Erreurs"),
        MainTab::Data => localized_copy(&lang, "Data", "Datos", "Donnees"),
    }
}

/// Resolves the visible label for a dashboard tab from the loaded layout config.
fn _main_tab_label(layout: &LayoutConfig, tab: MainTab) -> String {
    layout
        .branding
        .tab_labels
        .get(_main_tab_to_str(tab))
        .map(|label| translate_text(label))
        .unwrap_or_else(|| _default_main_tab_label(tab))
}

/// Resolves the title shown at the top of the dashboard.
fn _dashboard_title(layout: &LayoutConfig) -> String {
    layout
        .branding
        .dashboard_title
        .clone()
        .or_else(|| layout.branding.app_name.clone())
        .map(|title| translate_text(&title))
        .unwrap_or_else(|| {
            let lang = current_language();
            localized_copy(
                &lang,
                "Telemetry Dashboard",
                "Panel de Telemetria",
                "Tableau Telemetrie",
            )
        })
}
/// Converts a persisted tab id back into the corresponding enum.
fn _main_tab_from_str(s: &str) -> MainTab {
    match s {
        "state" => MainTab::State,
        "connection-status" => MainTab::ConnectionStatus,
        "detailed" => MainTab::Detailed,
        "network-topology" => MainTab::NetworkTopology,
        "map" => MainTab::Map,
        "actions" => MainTab::Actions,
        "calibration" => MainTab::Calibration,
        "notifications" => MainTab::Notifications,
        "warnings" => MainTab::Warnings,
        "errors" => MainTab::Errors,
        "data" => MainTab::Data,
        _ => MainTab::State,
    }
}

/// Returns whether a tab is enabled by the loaded layout config.
fn _layout_main_tab_enabled(layout: &LayoutConfig, tab: MainTab) -> bool {
    let listed = layout
        .main_tabs
        .iter()
        .any(|id| _main_tab_from_str(id) == tab);
    listed && (tab != MainTab::NetworkTopology || layout.network_tab.enabled)
}

/// Returns whether the actions tab has at least one command the current session may send.
fn _actions_tab_has_visible_actions(layout: &LayoutConfig, abort_only_mode: bool) -> bool {
    let _ = abort_only_mode;
    layout
        .actions_tab
        .actions
        .iter()
        .any(|action| auth::can_send_command(action.cmd.as_str()))
}

/// Computes the final visible tab list after applying layout and auth filtering.
fn _configured_main_tabs(layout: &LayoutConfig, abort_only_mode: bool) -> Vec<MainTab> {
    let mut tabs = Vec::new();
    for id in &layout.main_tabs {
        let tab = _main_tab_from_str(id);
        if !_layout_main_tab_enabled(layout, tab) || tabs.contains(&tab) {
            continue;
        }
        if tab == MainTab::Actions && !_actions_tab_has_visible_actions(layout, abort_only_mode) {
            continue;
        }
        tabs.push(tab);
    }
    if tabs.is_empty() {
        tabs.push(MainTab::State);
    }
    tabs
}

// ---------- Base URL config ----------
pub struct UrlConfig;

impl UrlConfig {
    /// Normalizes and persists the backend base URL selected by the operator.
    pub fn set_base_url_and_persist(url: String) {
        let clean = normalize_base_url(url);
        *BASE_URL.write() = clean.clone();
        persist::set_string(BASE_URL_STORAGE_KEY, &clean);
    }

    /// Returns the stored backend base URL when one exists.
    pub fn _stored_base_url() -> Option<String> {
        persist::get_string(BASE_URL_STORAGE_KEY)
            .map(normalize_base_url)
            .filter(|s| !s.trim().is_empty())
    }

    /// Returns the current HTTP base URL, including platform-specific defaults.
    pub fn base_http() -> String {
        // load from storage key if present
        let base = persist::get_string(BASE_URL_STORAGE_KEY)
            .map(normalize_base_url)
            .unwrap_or_else(|| BASE_URL.read().clone());

        #[cfg(target_arch = "wasm32")]
        if base.is_empty() {
            if let Some(window) = web_sys::window()
                && let Ok(origin) = window.location().origin()
            {
                return normalize_base_url(origin);
            }
        }

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

    /// Persists the TLS validation override for a specific backend base URL.
    pub fn _set_skip_tls_verify_for_base(base: &str, value: bool) {
        let clean = normalize_base_url(base.to_string());
        if clean.is_empty() {
            return;
        }
        let key = _tls_skip_key(&clean);
        persist::set_string(&key, if value { "true" } else { "false" });
    }

    /// Returns whether TLS validation is disabled for a specific backend base URL.
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

    /// Persists the TLS validation override for the currently selected backend base URL.
    pub fn _set_skip_tls_verify(value: bool) {
        let base = UrlConfig::base_http();
        UrlConfig::_set_skip_tls_verify_for_base(&base, value);
    }

    /// Returns whether TLS validation is disabled for the currently selected backend base URL.
    pub fn _skip_tls_verify() -> bool {
        let base = UrlConfig::base_http();
        UrlConfig::_skip_tls_verify_for_base(&base)
    }
}

/// Builds the persistence key used for the per-backend TLS validation override.
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

/// Restarts the WebSocket connection and triggers a fresh telemetry reseed.
fn reconnect_and_reload_ui() {
    // Always restart websockets/tasks
    bump_ws_epoch();
    bump_seed_epoch();

    // Native: keep current UI mounted so charts/history remain visible while reseed runs.
}

/// Mirrors the explicit reload button behavior before reconnecting to a backend.
pub fn clear_and_reconnect_after_connect() {
    clear_telemetry_runtime_buffers();
    charts_cache_request_refit();

    #[cfg(not(target_arch = "wasm32"))]
    {
        clear_ui_telemetry_store();
        charts_cache_reset_and_ingest(&[]);
    }

    reconnect_and_reload_ui();
}

#[cfg(not(target_arch = "wasm32"))]
/// Returns whether the dashboard has ever reached a live backend connection in this process.
pub fn dashboard_has_prior_backend_connection() -> bool {
    DASHBOARD_HAS_CONNECTED.load(Ordering::Relaxed)
}

/// Restarts backend-backed frontend state after login or logout changes.
pub fn reconnect_and_reseed_after_auth_change() {
    reconnect_and_reload_ui();
}

#[cfg(target_arch = "wasm32")]
/// Returns whether the browser should keep dashboard background tasks running on this route.
fn web_dashboard_runtime_allowed() -> bool {
    web_sys::window()
        .and_then(|window| window.location().pathname().ok())
        .map(|path| path != "/login")
        .unwrap_or(true)
}

#[cfg(not(target_arch = "wasm32"))]
/// Native builds always allow the dashboard runtime.
fn web_dashboard_runtime_allowed() -> bool {
    true
}

/// Clears runtime telemetry buffers before a reconnect or reseed.
fn clear_telemetry_runtime_buffers() {
    if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
        q.clear();
    }
    clear_ui_telemetry_store();
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
    /// Sends a command over the current WebSocket transport.
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
/// Outer dashboard component that owns the real mount lifetime.
pub fn TelemetryDashboard() -> Element {
    // Create once per real mount
    *DASHBOARD_LIFE.write() = DashboardLife::new_alive();

    log!(
        "[UI] TelemetryDashboard mounted (alive=true, gen={})",
        dashboard_gen()
    );

    rsx! {
        TelemetryDashboardInner {}
    }
}

// ---------- INNER dashboard (this is what we remount on native reload) ----------
#[component]
/// Inner dashboard component that owns the live UI state and background tasks.
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
    let distance_units_metric = use_signal(|| {
        persist::get_string(MAP_DISTANCE_UNITS_STORAGE_KEY)
            .map(|v| v == "metric")
            .unwrap_or(false)
    });
    let theme_preset = use_signal(|| {
        let stored = persist::get_or(THEME_PRESET_STORAGE_KEY, "default");
        if stored == "layout" {
            "backend".to_string()
        } else {
            stored
        }
    });
    let language_code = use_signal(|| persist::get_or(LANGUAGE_STORAGE_KEY, "en"));
    let network_flow_animation_enabled =
        use_signal(|| persist::get_or(NETWORK_FLOW_ANIMATION_STORAGE_KEY, "on") != "off");

    let layout_config = use_signal(|| None::<LayoutConfig>);
    let layout_loading = use_signal(|| true);
    let layout_error = use_signal(|| None::<String>);
    let did_request_layout = use_signal(|| false);
    let startup_seed_ready = use_signal(|| false);

    let parse_i64 = |s: &str| s.parse::<i64>().unwrap_or(0);

    // ----------------------------
    // Live app state
    // ----------------------------
    let active_data_tab = use_signal(|| st_data_tab.read().clone());
    let warnings = use_signal(Vec::<AlertMsg>::new);
    let errors = use_signal(Vec::<AlertMsg>::new);
    let notifications = use_signal(Vec::<PersistentNotification>::new);
    let notification_history = use_signal(Vec::<PersistentNotification>::new);
    let dismissed_notifications = use_signal(load_dismissed_notifications);
    let unread_notification_ids = use_signal(Vec::<u64>::new);
    let action_policy = use_signal(ActionPolicyMsg::default_locked);
    let network_time = use_signal(|| None::<NetworkTimeSync>);
    let flight_state = use_signal(|| "Startup".to_string());
    let board_status = use_signal(Vec::<BoardStatusEntry>::new);
    let network_topology = use_signal(NetworkTopologyMsg::default);
    let frontend_network_metrics = use_signal(FrontendNetworkMetrics::default);
    let abort_only_mode = use_signal(|| false);
    let tabs_expanded = use_signal(|| false);
    let header_actions_expanded = use_signal(|| false);
    let last_applied_disable_actions_default = use_signal(|| None::<bool>);
    let show_settings_overlay = use_signal(|| false);
    #[cfg(not(target_arch = "wasm32"))]
    let show_version_overlay = use_signal(|| false);

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

    {
        let mut frontend_network_metrics = frontend_network_metrics;
        let alive = alive.clone();
        let active_main_tab = active_main_tab;
        use_effect(move || {
            reset_frontend_network_metrics_state();
            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();
            spawn(async move {
                while alive.load(Ordering::Relaxed) && *WS_EPOCH.read() == epoch {
                    if *active_main_tab.read() == MainTab::Detailed {
                        frontend_network_metrics.set(frontend_network_metrics_snapshot());
                    }
                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(1_000).await;
                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });
        });
    }

    {
        let mut active_main_tab = active_main_tab;
        let layout_config = layout_config;
        let abort_only_mode = abort_only_mode;
        use_effect(move || {
            let Some(layout) = layout_config.read().clone() else {
                return;
            };
            let current = *active_main_tab.read();
            let configured = _configured_main_tabs(&layout, *abort_only_mode.read());
            if !configured.contains(&current) {
                let next = configured.into_iter().next().unwrap_or(MainTab::State);
                active_main_tab.set(next);
            }
        });
    }

    {
        let layout_config = layout_config;
        let mut abort_only_mode = abort_only_mode;
        let mut last_applied_disable_actions_default = last_applied_disable_actions_default;
        use_effect(move || {
            let Some(layout) = layout_config.read().clone() else {
                return;
            };
            let default_disabled = layout.actions_tab.disable_actions_by_default;
            if *last_applied_disable_actions_default.read() == Some(default_disabled) {
                return;
            }
            last_applied_disable_actions_default.set(Some(default_disabled));
            abort_only_mode.set(default_disabled);
        });
    }

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

            if let Some(cached) = persist::get_string(LAYOUT_CACHE_KEY)
                && let Ok(layout) = serde_json::from_str::<LayoutConfig>(&cached)
                && let Ok(()) = layout.validate()
            {
                layout_config.set(Some(layout));
                layout_loading.set(false);
            }

            spawn(async move {
                match http_get_json::<LayoutConfig>("/api/layout").await {
                    Ok(layout) => {
                        if let Err(err) = layout.validate() {
                            layout_error.set(Some(format!("Layout failed to load: {err}")));
                            if layout_config.read().is_none() {
                                layout_loading.set(false);
                            }
                            return;
                        }
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
                bump_seed_epoch();
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
        let distance_units_metric = distance_units_metric;
        use_effect(move || {
            let value = if *distance_units_metric.read() {
                "metric"
            } else {
                "imperial"
            };
            persist::set_string(MAP_DISTANCE_UNITS_STORAGE_KEY, value);
        });
    }
    {
        let theme_preset = theme_preset;
        use_effect(move || {
            let value = theme_preset.read().clone();
            persist::set_string(THEME_PRESET_STORAGE_KEY, &value);
        });
    }
    {
        let language_code = language_code;
        use_effect(move || {
            let value = language_code.read().clone();
            *PREFERRED_LANGUAGE.write() = value.clone();
            persist::set_string(LANGUAGE_STORAGE_KEY, &value);
        });
    }
    {
        let network_flow_animation_enabled = network_flow_animation_enabled;
        use_effect(move || {
            let value = if *network_flow_animation_enabled.read() {
                "on"
            } else {
                "off"
            };
            persist::set_string(NETWORK_FLOW_ANIMATION_STORAGE_KEY, value);
        });
    }
    {
        let language_code = language_code;
        let alive = alive.clone();
        use_effect(move || {
            let lang = language_code.read().clone();
            *TRANSLATION_CATALOG.write() = HashMap::new();
            if let Ok(mut pending) = TRANSLATION_MISS_QUEUE.lock() {
                pending.clear();
            }
            let alive = alive.clone();
            spawn(async move {
                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                let path = format!("/api/i18n/catalog?lang={lang}");
                if let Ok(response) = http_get_json::<TranslationCatalogResponse>(&path).await
                    && alive.load(Ordering::Relaxed)
                    && response.lang == lang
                {
                    *TRANSLATION_CATALOG.write() = response.translations;
                }
            });
        });
    }
    {
        let alive = alive.clone();
        use_effect(move || {
            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();
            spawn(async move {
                while alive.load(Ordering::Relaxed) && *WS_EPOCH.read() == epoch {
                    #[cfg(target_arch = "wasm32")]
                    gloo_timers::future::TimeoutFuture::new(300).await;

                    #[cfg(not(target_arch = "wasm32"))]
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

                    if !alive.load(Ordering::Relaxed) || *WS_EPOCH.read() != epoch {
                        break;
                    }

                    if TRANSLATION_REQUEST_ACTIVE
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                        .is_err()
                    {
                        continue;
                    }

                    let lang = current_language();
                    let catalog = TRANSLATION_CATALOG.read().clone();
                    let batch = drain_translation_misses(64, &catalog);
                    if batch.is_empty() {
                        TRANSLATION_REQUEST_ACTIVE.store(false, Ordering::Release);
                        continue;
                    }

                    let result = http_post_json::<TranslationRequest, TranslationResponse>(
                        "/api/i18n/translate",
                        &TranslationRequest {
                            target_lang: lang.clone(),
                            texts: batch,
                        },
                    )
                    .await;

                    if let Ok(response) = result
                        && alive.load(Ordering::Relaxed)
                        && response.lang == lang
                    {
                        merge_translation_map(response.translations);
                    }

                    TRANSLATION_REQUEST_ACTIVE.store(false, Ordering::Release);
                }
            });
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

        use_effect(move || {
            let alive = alive.clone();
            let epoch = *WS_EPOCH.read();

            spawn(async move {
                // Default to ~60 FPS; Linux WebKitGTK tends to degrade when we flush harder.
                let tick_ms: u32 = std::env::var("GS_UI_TICK_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(16)
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

                    if let Ok(mut store) = UI_TELEMETRY_STORE.lock() {
                        store.apply_rows(drained);
                    }
                    let mut render_epoch = TELEMETRY_RENDER_EPOCH.write();
                    *render_epoch = render_epoch.wrapping_add(1);
                }
            });
        });
    }

    // Seed from DB (HTTP) on mount
    {
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
        let mut network_topology_s = network_topology;

        let alive = alive.clone();
        let startup_seed_ready = startup_seed_ready;

        use_effect(move || {
            let alive = alive.clone();
            spawn(async move {
                let mut handled_seed_epoch: Option<u64> = None;
                while alive.load(Ordering::Relaxed) {
                    // Initial seed waits until layout has loaded and the startup delay completes.
                    if !*startup_seed_ready.read() {
                        #[cfg(target_arch = "wasm32")]
                        gloo_timers::future::TimeoutFuture::new(150).await;

                        #[cfg(not(target_arch = "wasm32"))]
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        continue;
                    }

                    let seed_epoch = *SEED_EPOCH.read();
                    if handled_seed_epoch == Some(seed_epoch) {
                        #[cfg(target_arch = "wasm32")]
                        gloo_timers::future::TimeoutFuture::new(150).await;

                        #[cfg(not(target_arch = "wasm32"))]
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        continue;
                    }
                    handled_seed_epoch = Some(seed_epoch);
                    log!("[seed] watcher picked up epoch={seed_epoch}");

                    // Keep current in-memory rows visible until reseed data arrives.
                    // This avoids visible graph "blanking" during reconnect/reseed.
                    let mut last_err: Option<String> = None;
                    const RESEED_ATTEMPTS: usize = 3;
                    for attempt in 1..=RESEED_ATTEMPTS {
                        log!("[seed] epoch={seed_epoch} attempt={attempt} starting seed_from_db");
                        let res = seed_from_db(
                            &mut warnings_s,
                            &mut errors_s,
                            &mut notifications_s,
                            &mut notification_history_s,
                            &mut dismissed_notifications_s,
                            &mut unread_notification_ids_s,
                            &mut action_policy_s,
                            &mut network_time_s,
                            &mut network_topology_s,
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
                                log!("[seed] epoch={seed_epoch} attempt={attempt} completed");
                                last_err = None;
                                break;
                            }
                            Err(e) => {
                                log!("[seed] epoch={seed_epoch} attempt={attempt} failed: {e}");
                                last_err = Some(e);
                                if attempt < RESEED_ATTEMPTS
                                    && alive.load(Ordering::Relaxed)
                                    && *SEED_EPOCH.read() == seed_epoch
                                {
                                    #[cfg(target_arch = "wasm32")]
                                    gloo_timers::future::TimeoutFuture::new(400 * attempt as u32)
                                        .await;

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
                        && *SEED_EPOCH.read() == seed_epoch
                    {
                        log!("seed_from_db failed after retries: {e}");
                    }
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
            if !web_dashboard_runtime_allowed() {
                log!("[WS] supervisor skipped on non-dashboard route");
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
                    warnings,
                    errors,
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
                    network_topology,
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

    let base_theme = layout_config
        .read()
        .as_ref()
        .map(|cfg| cfg.theme.clone())
        .unwrap_or_default();
    let language_snapshot = language_code.read().clone();
    let theme = localized_theme(&base_theme, theme_preset.read().as_str());
    {
        let layout_config = layout_config;
        let theme_preset = theme_preset;
        use_effect(move || {
            let base_theme = layout_config
                .read()
                .as_ref()
                .map(|cfg| cfg.theme.clone())
                .unwrap_or_default();
            let theme = localized_theme(&base_theme, theme_preset.read().as_str());
            *APP_THEME_CONFIG.write() = theme.clone();
            js_eval(&format!(
                r#"
                (function() {{
                  try {{
                    const bg = {bg:?};
                    const fg = {fg:?};
                    document.documentElement.style.setProperty('--gs26-app-background', bg);
                    document.documentElement.style.setProperty('--gs26-app-text', fg);
                    document.documentElement.style.backgroundColor = bg;
                    document.documentElement.style.color = fg;
                    if (document.body) {{
                      document.body.style.setProperty('--gs26-app-background', bg);
                      document.body.style.setProperty('--gs26-app-text', fg);
                      document.body.style.backgroundColor = bg;
                      document.body.style.color = fg;
                    }}
                    const main = document.getElementById("main");
                    if (main) {{
                      main.style.setProperty('--gs26-app-background', bg);
                      main.style.setProperty('--gs26-app-text', fg);
                      main.style.backgroundColor = bg;
                      main.style.color = fg;
                    }}
                  }} catch (_) {{}}
                }})();
                "#,
                bg = theme.app_background,
                fg = theme.text_primary,
            ));
        });
    }
    let main_tab_accent = |tab_id: &str, fallback: &str| {
        theme
            .main_tab_accents
            .get(tab_id)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    };
    // Button styles
    let tab_style_active = |color: &str| {
        format!(
            "padding:0.4rem 0.8rem; border-radius:0.5rem;\
             display:inline-flex; align-items:center; justify-content:center; gap:0.35rem;\
             font:inherit;\
             min-width:0; max-width:100%; text-align:center; line-height:1.2;\
             white-space:normal; overflow-wrap:anywhere; word-break:break-word;\
             border:1px solid {color}; background:{};\
             color:{color}; cursor:pointer;",
            theme.button_background
        )
    };
    let tab_style_inactive = format!(
        "padding:0.4rem 0.8rem; border-radius:0.5rem;\
         display:inline-flex; align-items:center; justify-content:center; gap:0.35rem;\
         font:inherit;\
         min-width:0; max-width:100%; text-align:center; line-height:1.2;\
         white-space:normal; overflow-wrap:anywhere; word-break:break-word;\
         border:1px solid {}; background:{};\
         color:{}; cursor:pointer;",
        theme.tab_shell_border, theme.app_background, theme.text_primary
    );
    let dashboard_font_stack = "system-ui, -apple-system, BlinkMacSystemFont";

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
            let connect_button_label =
                localized_copy(&current_language(), "CONNECT", "CONECTAR", "CONNECTER");

            rsx! {

                button {
                    style: format!("
                        padding:0.45rem 0.85rem;
                        border-radius:0.75rem;
                        border:1px solid {};
                        background:{};
                        color:{};
                        font-weight:800;
                        cursor:pointer;
                    ", theme.button_border, theme.button_background, theme.button_text),
                    onclick: move |_| {
                        // KEY CHANGE:
                        // Mark dashboard "not alive" *before* bumping WS_EPOCH.
                        // That prevents the dashboard's WS supervisor effect from spawning
                        // a new epoch while we're navigating away.
                        let was_alive = alive_for_click.swap(false, Ordering::Relaxed);
                        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
                        gps::stop_gps_updates();
                        _set_dashboard_alive(false);
                        if was_alive {
                            bump_ws_epoch();
                            log!("[UI] CONNECT pressed -> alive=false + bump epoch");
                        }

                        let _ = nav.push(Route::Connect {});
                    },
                    "{connect_button_label}"
                }
            }
        }
    };

    let version_button: Element = {
        #[cfg(target_arch = "wasm32")]
        {
            rsx! { div {} }
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let show_version_overlay = show_version_overlay;
            rsx! {
                button {
                    style: format!("
                        padding:0.45rem 0.85rem;
                        border-radius:0.75rem;
                        border:1px solid {};
                        background:{};
                        color:{};
                        font-weight:800;
                        cursor:pointer;
                    ", theme.button_border, theme.button_background, theme.button_text),
                    onclick: {
                        let mut show_version_overlay = show_version_overlay;
                        move |_| {
                            show_version_overlay.set(true);
                        }
                    },
                    ontouchend: {
                        let mut show_version_overlay = show_version_overlay;
                        move |_| {
                            show_version_overlay.set(true);
                        }
                    },
                    {localized_copy(&language_snapshot, "VERSION", "VERSION", "VERSION")}
                }
            }
        }
    };

    let settings_button: Element = {
        let show_settings_overlay = show_settings_overlay;
        rsx! {
            button {
                style: format!("
                    padding:0.45rem 0.85rem;
                    border-radius:0.75rem;
                    border:1px solid {};
                    background:{};
                    color:{};
                    font-weight:800;
                    cursor:pointer;
                ", theme.button_border, theme.button_background, theme.button_text),
                onclick: {
                    let mut show_settings_overlay = show_settings_overlay;
                    move |_| {
                        show_settings_overlay.set(true);
                    }
                },
                ontouchend: {
                    let mut show_settings_overlay = show_settings_overlay;
                    move |_| {
                        show_settings_overlay.set(true);
                    }
                },
                {localized_copy(&language_snapshot, "SETTINGS", "AJUSTES", "PARAMETRES")}
            }
        }
    };

    let reload_button_label = localized_copy(&language_snapshot, "RELOAD", "RECARGAR", "RECHARGER");
    let close_button_label = localized_copy(&language_snapshot, "Close", "Cerrar", "Fermer");
    let _version_title = localized_copy(&language_snapshot, "UBSEDS GS", "UBSEDS GS", "UBSEDS GS");
    let settings_title = localized_copy(&language_snapshot, "Settings", "Ajustes", "Parametres");
    let sign_in_label = localized_copy(
        &language_snapshot,
        "SIGN IN",
        "INICIAR SESION",
        "SE CONNECTER",
    );
    let sign_out_prefix = localized_copy(
        &language_snapshot,
        "SIGN OUT",
        "CERRAR SESION",
        "SE DECONNECTER",
    );
    let auth_label = auth::current_session()
        .and_then(|session| session.session.username)
        .map(|username| format!("{sign_out_prefix} {username}"))
        .unwrap_or(sign_in_label);
    let disable_actions_label = if *abort_only_mode.read() {
        localized_copy(
            &language_snapshot,
            "DISABLE ACTIONS ON",
            "DESACTIVAR ACCIONES ON",
            "DESACTIVER ACTIONS ON",
        )
    } else {
        localized_copy(
            &language_snapshot,
            "DISABLE ACTIONS OFF",
            "DESACTIVAR ACCIONES OFF",
            "DESACTIVER ACTIONS OFF",
        )
    };

    let auth_button: Element = {
        use dioxus_router::use_navigator;
        let nav = use_navigator();
        let base = UrlConfig::base_http();
        let skip_tls = UrlConfig::_skip_tls_verify();
        rsx! {
            button {
                style: format!("
                    padding:0.45rem 0.85rem;
                    border-radius:0.75rem;
                    border:1px solid {};
                    background:{};
                    color:{};
                    font-weight:800;
                    cursor:pointer;
                ", theme.button_border, theme.button_background, theme.button_text),
                onclick: move |_| {
                    if auth::current_session().is_some() {
                        let base = base.clone();
                        spawn(async move {
                            let _ = auth::logout(&base, skip_tls).await;
                            match auth::fetch_logged_out_session_status(&base, skip_tls).await {
                                Ok(status) if status.permissions.view_data => {
                                    auth::set_logged_out_status(status);
                                    reconnect_and_reseed_after_auth_change();
                                }
                                Ok(_) | Err(_) => {
                                    auth::clear_current_session();
                                    _set_dashboard_alive(false);
                                    bump_ws_epoch();
                                    reconnect_and_reseed_after_auth_change();
                                    let _ = nav.push(Route::Login {});
                                }
                            }
                        });
                    } else {
                        _set_dashboard_alive(false);
                        bump_ws_epoch();
                        let _ = nav.push(Route::Login {});
                    }
                },
                "{auth_label}"
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
                    if let Err(err) = layout.validate() {
                        layout_error.set(Some(format!("Layout failed to load: {err}")));
                        if layout_config.read().is_none() {
                            layout_loading.set(false);
                        }
                        return;
                    }
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
    let mut _refresh_layout = refresh_layout;
    let reload_button: Element = rsx! {
        button {
            style: format!("
                padding:0.45rem 0.85rem;
                border-radius:0.75rem;
                border:1px solid {};
                background:{};
                color:{};
                font-weight:800;
                cursor:pointer;
            ", theme.button_border, theme.button_background, theme.button_text),
            onclick: move |_| {
                clear_and_reconnect_after_connect();
            },
            "{reload_button_label}"
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
    #[cfg(not(target_arch = "wasm32"))]
    let version_overlay_open = *show_version_overlay.read();
    let version_overlay: Element = {
        #[cfg(target_arch = "wasm32")]
        {
            rsx! { div {} }
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            if version_overlay_open {
                rsx! {
                    div {
                        style: "
                            position:fixed;
                            inset:0;
                            z-index:3000;
                            display:flex;
                            align-items:flex-start;
                            justify-content:center;
                            padding:24px 16px;
                            overflow-y:auto;
                            overflow-x:hidden;
                            background:{theme.app_background};
                            font-family:{dashboard_font_stack};
                            backdrop-filter:blur(6px);
                            overscroll-behavior:contain;
                            -webkit-overflow-scrolling:touch;
                        ",
                        onclick: {
                            let mut show_version_overlay = show_version_overlay;
                            move |_| show_version_overlay.set(false)
                        },
                        div {
                            style: "
                                width:min(900px, 100%);
                                padding:24px;
                                color:{theme.text_primary};
                                border:1px solid {theme.tab_shell_border};
                                border-radius:16px;
                                background:{theme.tab_shell_background};
                                font-family:{dashboard_font_stack};
                                box-shadow:0 12px 30px rgba(0,0,0,0.5);
                            ",
                            onclick: move |evt| evt.stop_propagation(),
                            ontouchend: move |evt| evt.stop_propagation(),
                            div {
                                style: "display:flex; align-items:flex-start; justify-content:space-between; gap:12px; margin-bottom:12px; flex-wrap:wrap;",
                                h1 { style: "margin:0; font-size:20px;", "{_version_title}" }
                                button {
                                    style: "
                                        padding:10px 14px;
                                        border-radius:12px;
                                        border:1px solid {theme.button_border};
                                        background:{theme.button_background};
                                        color:{theme.button_text};
                                        font-family:{dashboard_font_stack};
                                        font-weight:700;
                                        cursor:pointer;
                                    ",
                                    onclick: {
                                        let mut show_version_overlay = show_version_overlay;
                                        move |_| show_version_overlay.set(false)
                                    },
                                    ontouchend: {
                                        let mut show_version_overlay = show_version_overlay;
                                        move |_| show_version_overlay.set(false)
                                    },
                                    "{close_button_label}"
                                }
                            }
                            VersionTab {}
                        }
                    }
                }
            } else {
                rsx! { div {} }
            }
        }
    };
    let settings_overlay_open = *show_settings_overlay.read();
    let settings_overlay: Element = {
        if settings_overlay_open {
            rsx! {
                div {
                    style: "
                        position:fixed;
                        inset:0;
                        z-index:3000;
                        display:flex;
                        align-items:flex-start;
                        justify-content:center;
                        padding:24px 16px;
                        overflow-y:auto;
                        overflow-x:hidden;
                        background:{theme.app_background};
                        font-family:{dashboard_font_stack};
                        backdrop-filter:blur(6px);
                        overscroll-behavior:contain;
                        -webkit-overflow-scrolling:touch;
                    ",
                    onclick: {
                        let mut show_settings_overlay = show_settings_overlay;
                        move |_| show_settings_overlay.set(false)
                    },
                    div {
                        style: "
                            width:min(980px, 100%);
                            padding:24px;
                            color:{theme.text_primary};
                            border:1px solid {theme.tab_shell_border};
                            border-radius:16px;
                            background:{theme.tab_shell_background};
                            font-family:{dashboard_font_stack};
                            box-shadow:0 12px 30px rgba(0,0,0,0.5);
                        ",
                        onclick: move |evt| evt.stop_propagation(),
                        ontouchend: move |evt| evt.stop_propagation(),
                        div {
                            style: "display:flex; align-items:flex-start; justify-content:space-between; gap:12px; margin-bottom:12px; flex-wrap:wrap;",
                            h1 { style: "margin:0; font-size:20px;", "{settings_title}" }
                            button {
                                style: "
                                    padding:10px 14px;
                                    border-radius:12px;
                                    border:1px solid {theme.button_border};
                                    background:{theme.button_background};
                                    color:{theme.button_text};
                                    font-family:{dashboard_font_stack};
                                    font-weight:700;
                                    cursor:pointer;
                                ",
                                onclick: {
                                    let mut show_settings_overlay = show_settings_overlay;
                                    move |_| show_settings_overlay.set(false)
                                },
                                ontouchend: {
                                    let mut show_settings_overlay = show_settings_overlay;
                                    move |_| show_settings_overlay.set(false)
                                },
                                "{close_button_label}"
                            }
                        }
                        SettingsPage {
                            distance_units_metric: distance_units_metric,
                            theme_preset: theme_preset,
                            language_code: language_code,
                            network_flow_animation_enabled: network_flow_animation_enabled,
                            theme: theme.clone(),
                            title: settings_title.clone(),
                        }
                    }
                }
            }
        } else {
            rsx! { div {} }
        }
    };

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
             @keyframes gs26-blink-fast-on  {{ 0%, 100% {{ opacity: 1.0; }} 55% {{ opacity: 0.2; }} }}
             .gs26-tab-shell {{ min-width:260px; }}
             .gs26-tab-toggle {{ display:none; }}
             .gs26-tab-nav {{ display:flex; gap:0.5rem; flex-wrap:wrap; }}
             .gs26-header-actions-shell {{ margin-left:auto; position:relative; z-index:2000; }}
             .gs26-header-actions-list {{ display:flex; align-items:center; gap:10px; flex-wrap:wrap; }}
             .gs26-header-menu-toggle {{ display:none; }}
             @media (max-width: 900px) {{
               .gs26-header-actions-shell {{
                 display:flex;
                 align-items:center;
                 justify-content:flex-end;
               }}
               .gs26-header-menu-toggle {{
                 display:inline-flex;
                 align-items:center;
                 justify-content:center;
                 padding:0.55rem 0.9rem;
                 border-radius:0.75rem;
                 border:1px solid var(--gs26-header-menu-border);
                 background:var(--gs26-header-menu-background);
                 color:var(--gs26-header-menu-text);
                 font:inherit;
                 font-weight:800;
                 cursor:pointer;
               }}
               .gs26-header-actions-list {{
                 display:none;
                 position:absolute;
                 top:calc(100% + 8px);
                 right:0;
                 z-index:60;
                 min-width:min(320px, calc(100vw - 32px));
                 max-width:calc(100vw - 32px);
                 padding:0.8rem;
                 border-radius:0.9rem;
                 border:1px solid var(--gs26-header-menu-border);
                 background:var(--gs26-header-menu-background);
                 box-shadow:0 18px 40px rgba(0,0,0,0.4);
                 flex-direction:column;
                 align-items:stretch;
                 gap:8px;
               }}
               .gs26-header-actions-shell[data-expanded=\"true\"] .gs26-header-actions-list {{
                 display:flex;
               }}
               .gs26-header-actions-list button {{
                 width:100%;
                 margin-left:0 !important;
               }}
             }}
             @media (max-width: 720px), (max-height: 780px) {{
               .gs26-tab-shell {{
                 flex:1 1 100%;
                 min-width:0;
                 display:grid !important;
                 width:100% !important;
                 justify-content:stretch !important;
                 align-items:center !important;
                 justify-items:center !important;
                 row-gap:0.95rem;
                 padding:0.7rem;
               }}
               .gs26-tab-shell[data-expanded=\"false\"] {{
                 grid-template-columns:auto;
                 justify-content:center;
               }}
               .gs26-tab-shell[data-expanded=\"true\"] {{
                 grid-template-columns:minmax(0, 1fr) minmax(0, 1fr);
                 column-gap:0.95rem;
                 justify-content:stretch;
               }}
               .gs26-tab-shell[data-expanded=\"true\"] .gs26-tab-toggle {{
                 grid-column:1;
               }}
               .gs26-tab-shell[data-expanded=\"true\"] .gs26-tab-nav {{
                 grid-column:2;
               }}
               .gs26-tab-toggle {{
                 display:inline-flex;
                 align-items:center;
                 justify-content:center;
                 font:inherit;
                 width:fit-content;
                 max-width:100%;
                 align-self:center;
                 justify-self:center;
                 text-align:center;
                 line-height:1.2;
                 white-space:normal;
                 overflow-wrap:anywhere;
                 word-break:break-word;
                 padding:0.7rem 0.9rem;
                 border-radius:0.75rem;
                 border:1px solid #334155;
                 background:#0f172a;
                 color:#e5e7eb;
                 font-weight:800;
                 cursor:pointer;
               }}
               .gs26-tab-nav {{
                 display:none;
                 width:auto;
               }}
               .gs26-tab-shell[data-expanded=\"true\"] .gs26-tab-nav {{
                 display:flex;
                 flex-direction:column;
                 align-items:center;
                 justify-self:stretch;
                 width:100%;
                  margin-top:0;
               }}
               .gs26-tab-shell[data-expanded=\"true\"] .gs26-tab-nav button {{
                 width:18ch;
                 max-width:100%;
                 min-width:0;
                 justify-content:center;
                 margin-left:auto;
                 margin-right:auto;
               }}
             }}"
                }
                if layout_loading_snapshot && layout_snapshot.is_none() {
                    div {
                        style: "
                    height:var(--gs26-app-height);
                    padding:24px;
                    color:var(--gs26-app-text);
                    font-family:system-ui, -apple-system, BlinkMacSystemFont;
                    background:var(--gs26-app-background);
                    display:flex;
                    align-items:center;
                    justify-content:center;
                    border:{border_style};
                    box-sizing:border-box;
                ",
                        div { style: "text-align:center; display:flex; flex-direction:column; gap:10px; align-items:center;",
                            div { style: "font-size:22px; font-weight:800; color:#f97316;", "Loading layout..." }
                            div { style: "font-size:14px; color:#94a3b8;", "Waiting for layout from backend" }
                            div { style: "display:flex; gap:10px; flex-wrap:wrap; justify-content:center; margin-top:4px;",
                                {version_button}
                                {connect_button}
                            }
                        }
                    }
                } else if layout_snapshot.is_none() {
                    div {
                        style: "
                    height:var(--gs26-app-height);
                    padding:24px;
                    color:var(--gs26-app-text);
                    font-family:system-ui, -apple-system, BlinkMacSystemFont;
                    background:var(--gs26-app-background);
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
                                {version_button}
                                {connect_button}
                            }
                        }
                    }
                } else if let Some(layout) = layout_snapshot {
                div {

                    style: "
                height:var(--gs26-app-height);
                padding:24px;
                color:var(--gs26-app-text);
                font-family:system-ui, -apple-system, BlinkMacSystemFont;
                background:var(--gs26-app-background);
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
                    position:relative;
                    z-index:2000;
                ",
                        h1 { style: "color:#f97316; margin:0; font-size:22px; font-weight:800;", "{_dashboard_title(&layout)}" }

                        {
                            let show_disable_actions = _actions_tab_has_visible_actions(&layout, *abort_only_mode.read());
                            rsx! {
                        div {
                            class: "gs26-header-actions-shell",
                            "data-expanded": if *header_actions_expanded.read() { "true" } else { "false" },
                            style: "
                                margin-left:auto;
                                --gs26-header-menu-background:{theme.tab_shell_background};
                                --gs26-header-menu-border:{theme.tab_shell_border};
                                --gs26-header-menu-text:{theme.button_text};
                            ",
                            button {
                                class: "gs26-header-menu-toggle",
                                onclick: {
                                    let mut header_actions_expanded = header_actions_expanded;
                                    move |_| {
                                        let next = !*header_actions_expanded.read();
                                        header_actions_expanded.set(next);
                                    }
                                },
                                if *header_actions_expanded.read() {
                                    "Close menu"
                                } else {
                                    "Menu"
                                }
                            }
                        div { class: "gs26-header-actions-list",
                            if show_disable_actions {
                            button {
                                style: if *abort_only_mode.read() {
                                    "
                                        padding:0.45rem 0.85rem;
                                        border-radius:0.75rem;
                                        border:1px solid #ef4444;
                                        background:#4c0519;
                                        color:#fecdd3;
                                        box-shadow:0 0 0 1px rgba(239,68,68,0.15), 0 8px 20px rgba(76,5,25,0.35);
                                        font-weight:800;
                                        cursor:pointer;
                                    "
                                } else {
                                    "
                                        padding:0.45rem 0.85rem;
                                        border-radius:0.75rem;
                                        border:1px solid #475569;
                                        background:#0f172a;
                                        color:#cbd5e1;
                                        font-weight:800;
                                        cursor:pointer;
                                    "
                                },
                                onclick: {
                                    let mut abort_only_mode = abort_only_mode;
                                    let mut header_actions_expanded = header_actions_expanded;
                                    move |_| {
                                        let next = !*abort_only_mode.read();
                                        abort_only_mode.set(next);
                                        header_actions_expanded.set(false);
                                    }
                                },
                                "{disable_actions_label}"
                            }
                            }

                            {reload_button}
                            {settings_button}
                            {auth_button}
                            {version_button}
                            {connect_button}

                            {
                                let software_buttons_enabled =
                                    action_policy.read().software_buttons_enabled;
                                let abort_visible = auth::can_send_command("Abort");
                                let abort_allowed = software_buttons_enabled && abort_visible;
                                let abort_style = if abort_allowed {
                                    "
                                margin-left:clamp(20px, 6vw, 96px);
                                padding:0.45rem 0.85rem;
                                border-radius:0.75rem;
                                border:1px solid #ef4444;
                                background:#450a0a;
                                color:#fecaca;
                                font-weight:900;
                                cursor:pointer;
                            "
                                } else {
                                    "
                                margin-left:clamp(20px, 6vw, 96px);
                                padding:0.45rem 0.85rem;
                                border-radius:0.75rem;
                                border:1px solid #7f1d1d;
                                background:#1f2937;
                                color:#fca5a5;
                                font-weight:900;
                                cursor:not-allowed;
                                opacity:0.55;
                                filter:grayscale(0.25) brightness(0.9);
                            "
                                };
                                rsx! {
                                    if abort_visible {
                                        button {
                                            style: "{abort_style}",
                                            disabled: !abort_allowed,
                                            onclick: {
                                                let mut header_actions_expanded = header_actions_expanded;
                                                move |_| {
                                                    header_actions_expanded.set(false);
                                                if abort_allowed {
                                                    send_cmd("Abort")
                                                }
                                                }
                                            },
                                            "ABORT"
                                        }
                                    }
                                }
                            }
                        }
                        }
                            }
                        }
                    }

                    if let Some(msg) = layout_error_snapshot.clone() {
                        div { style: "margin-bottom:12px; padding:10px 12px; border-radius:10px; border:1px solid #ef4444; background:#450a0a; color:#fecaca; font-size:12px;",
                            "{msg}"
                        }
                    }

                    if !action_policy.read().software_buttons_enabled {
                        div { style: "margin-bottom:12px; padding:10px 12px; border-radius:10px; border:1px solid {theme.warning_border}; background:{theme.warning_background}; color:{theme.warning_text}; font-size:12px;",
                            "Software command buttons are disabled by the hardware GPIO lockout."
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
                            class: "gs26-tab-shell",
                            "data-expanded": if *tabs_expanded.read() { "true" } else { "false" },
                            style: "
                        flex:1 1 520px;
                        display:flex;
                        align-items:center;
                        padding:0.85rem;
                        border-radius:0.75rem;
                        background:{theme.tab_shell_background};
                        border:1px solid {theme.tab_shell_border};
                        box-shadow:0 10e0px 25px rgba(0,0,0,0.45);
                        min-width:260px;
                    ",
                            button {
                                class: "gs26-tab-toggle",
                                onclick: {
                                    let mut tabs_expanded = tabs_expanded;
                                    move |_| {
                                        let next = !*tabs_expanded.read();
                                        tabs_expanded.set(next);
                                    }
                                },
                                {
                                if *tabs_expanded.read() {
                                    "Hide tabs".to_string()
                                } else {
                                    format!("Show tabs ({})", _main_tab_label(&layout, *active_main_tab.read()))
                                }
                                }
                            }
                            nav { class: "gs26-tab-nav",
                                for tab in _configured_main_tabs(&layout, *abort_only_mode.read()).into_iter() {
                                    match tab {
                                        MainTab::State => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::State { tab_style_active(&main_tab_accent("state", "#38bdf8")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::State);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::State)}"
                                            }
                                        },
                                        MainTab::ConnectionStatus => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::ConnectionStatus { tab_style_active(&main_tab_accent("connection-status", "#06b6d4")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::ConnectionStatus);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::ConnectionStatus)}"
                                            }
                                        },
                                        MainTab::Detailed => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Detailed { tab_style_active(&main_tab_accent("detailed", "#0ea5e9")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::Detailed);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::Detailed)}"
                                            }
                                        },
                                        MainTab::Map => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Map { tab_style_active(&main_tab_accent("map", "#22c55e")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::Map);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::Map)}"
                                            }
                                        },
                                        MainTab::Actions => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Actions { tab_style_active(&main_tab_accent("actions", "#a78bfa")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::Actions);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::Actions)}"
                                            }
                                        },
                                        MainTab::Calibration => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Calibration { tab_style_active(&main_tab_accent("calibration", "#14b8a6")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::Calibration);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::Calibration)}"
                                            }
                                        },
                                        MainTab::Notifications => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Notifications { tab_style_active(&main_tab_accent("notifications", "#3b82f6")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    let notifications = notifications;
                                                    let dismissed_notifications = dismissed_notifications;
                                                    let unread_notification_ids = unread_notification_ids;
                                                    move |_| {
                                                        t.set(MainTab::Notifications);
                                                        tabs_expanded.set(false);
                                                        dismiss_all_active_notifications_local_and_remote(
                                                            notifications,
                                                            dismissed_notifications,
                                                            unread_notification_ids,
                                                        );
                                                    }
                                                },
                                                span { "{_main_tab_label(&layout, MainTab::Notifications)}" }
                                                if has_unread_notifications {
                                                    span { style: "margin-left:6px; color:{theme.info_text};", "●" }
                                                }
                                            }
                                        },
                                        MainTab::Warnings => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Warnings { tab_style_active(&main_tab_accent("warnings", "#facc15")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::Warnings);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                span { "{_main_tab_label(&layout, MainTab::Warnings)}" }
                                                if has_warnings {
                                                    span {
                                                        style: {
                                                            if has_unacked_warnings && *flash_on.read() {
                                                                format!("margin-left:6px; color:{}; opacity:1;", main_tab_accent("warnings", "#facc15"))
                                                            } else if has_unacked_warnings {
                                                                format!("margin-left:6px; color:{}; opacity:0.4;", main_tab_accent("warnings", "#facc15"))
                                                            } else {
                                                                format!("margin-left:6px; color:{}; opacity:1;", theme.text_soft)
                                                            }
                                                        },
                                                        "⚠"
                                                    }
                                                }
                                            }
                                        },
                                        MainTab::Errors => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Errors { tab_style_active(&main_tab_accent("errors", "#ef4444")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::Errors);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                span { "{_main_tab_label(&layout, MainTab::Errors)}" }
                                                if has_errors {
                                                    span {
                                                        style: {
                                                            if has_unacked_errors && *flash_on.read() {
                                                                format!("margin-left:6px; color:{}; opacity:1;", theme.error_text)
                                                            } else if has_unacked_errors {
                                                                format!("margin-left:6px; color:{}; opacity:0.4;", theme.error_text)
                                                            } else {
                                                                format!("margin-left:6px; color:{}; opacity:1;", theme.text_soft)
                                                            }
                                                        },
                                                        "⛔"
                                                    }
                                                }
                                            }
                                        },
                                        MainTab::Data => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::Data { tab_style_active(&main_tab_accent("data", "#f97316")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::Data);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::Data)}"
                                            }
                                        },
                                        MainTab::NetworkTopology => rsx! {
                                            button {
                                                style: if *active_main_tab.read() == MainTab::NetworkTopology { tab_style_active(&main_tab_accent("network-topology", "#8b5cf6")) } else { tab_style_inactive.to_string() },
                                                onclick: {
                                                    let mut t = active_main_tab;
                                                    let mut tabs_expanded = tabs_expanded;
                                                    move |_| {
                                                        t.set(MainTab::NetworkTopology);
                                                        tabs_expanded.set(false);
                                                    }
                                                },
                                                "{_main_tab_label(&layout, MainTab::NetworkTopology)}"
                                            }
                                        },
                    }
                }
        }
    }

                        div {
                            style: "
                        flex:1 1 320px;
                        display:flex;
                        align-items:center;
                        justify-content:space-between;
                        flex-wrap:wrap;
                        gap:0.5rem;
                        padding:0.35rem 0.7rem;
                        border-radius:1rem;
                        background:{theme.button_background};
                        border:1px solid {theme.tab_shell_border};
                        min-width:260px;
                    ",
                            div { style: "display:flex; align-items:center; flex-wrap:wrap; gap:0.5rem; min-width:0;",
                                span { style: "color:{theme.text_soft};", {localized_copy(&language_snapshot, "Status:", "Estado:", "Statut:")} }

                                if !has_warnings && !has_errors {
                                    span { style: "color:{theme.success_text}; font-weight:600; flex:0 0 auto;", {translate_text("Nominal")} }
                                    span { style: "color:{theme.info_text}; display:inline-flex; flex:0 0 auto; align-items:baseline; white-space:nowrap;",
                                        "({localized_copy(&language_snapshot, \"Flight state\", \"Estado de vuelo\", \"Etat de vol\")}:"
                                        span {
                                            style: "display:inline-flex; align-items:baseline; width:15.5ch; padding-left:0.4ch; white-space:nowrap;",
                                            span { {translate_text(&flight_state.read().to_string())} }
                                            span { ")" }
                                        }
                                    }
                                } else {
                                    if has_errors {
                                        span { style: "color:{theme.error_text}; flex:0 0 auto;", {format!("{}: {err_count}", translate_text("Errors"))} }
                                    }
                                    if has_warnings {
                                        span { style: "color:{theme.warning_text}; flex:0 0 auto;", {format!("{}: {warn_count}", translate_text("Warnings"))} }
                                    }
                                    span { style: "color:{theme.info_text}; display:inline-flex; flex:0 0 auto; align-items:baseline; white-space:nowrap;",
                                        "({localized_copy(&language_snapshot, \"Flight state\", \"Estado de vuelo\", \"Etat de vol\")}:"
                                        span {
                                            style: "display:inline-flex; align-items:baseline; width:15.5ch; padding-left:0.4ch; white-space:nowrap;",
                                            span { {translate_text(&flight_state.read().to_string())} }
                                            span { ")" }
                                        }
                                    }

                                    if *active_main_tab.read() == MainTab::Warnings && has_warnings {
                                        button {
                                            style: "
                                        margin-left:auto;
                                        padding:0.25rem 0.7rem;
                                        border-radius:999px;
                                        border:1px solid {theme.tab_shell_border};
                                        background:{theme.app_background};
                                        color:{theme.text_primary};
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
                                            {translate_text("Acknowledge warnings")}
                                        }
                                    }

                                    if *active_main_tab.read() == MainTab::Errors && has_errors {
                                        button {
                                            style: "
                                        margin-left:auto;
                                        padding:0.25rem 0.7rem;
                                        border-radius:999px;
                                        border:1px solid {theme.tab_shell_border};
                                        background:{theme.app_background};
                                        color:{theme.text_primary};
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
                                            {translate_text("Acknowledge errors")}
                                        }
                                    }
                                }
                            }

                            NetworkTimeBadge { network_time: network_time, language: language_snapshot.clone() }
                        }
                    }

                    // Main body
                    if !notifications.read().is_empty() {
                        div {
                            style: "display:flex; flex-direction:column; gap:8px; margin-bottom:10px;",
                            for n in notifications.read().iter() {
                                div {
                                    style: "display:flex; align-items:center; gap:10px; padding:10px 12px; border:1px solid #2563eb; border-radius:10px; background:#0b1f4d; color:#bfdbfe;",
                                    span { style: "flex:1;", {translate_text(&n.message)} }
                                    if let (Some(action_label), Some(action_cmd)) = (n.action_label.as_deref(), n.action_cmd.as_deref())
                                        && auth::can_send_command(action_cmd)
                                    {
                                        button {
                                            style: "padding:0.2rem 0.65rem; border-radius:999px; border:1px solid #60a5fa; background:#1e3a8a; color:#dbeafe; font-size:0.75rem; cursor:pointer;",
                                            onclick: {
                                                let cmd = action_cmd.to_string();
                                                move |_| {
                                                    send_cmd(&cmd);
                                                }
                                            },
                                            {translate_text(action_label)}
                                        }
                                    }
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
                                        {translate_text("Dismiss")}
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
                                            board_status: board_status,
                                            rocket_gps: rocket_gps,
                                            user_gps: user_gps,
                                            layout: layout.state_tab.clone(),
                                            data_layout: layout.data_tab.clone(),
                                            actions: layout.actions_tab.clone(),
                                            action_policy: action_policy,
                                            default_valve_labels: layout
                                                .data_tab
                                                .tabs
                                                .iter()
                                                .find(|t| t.id == "VALVE_STATE")
                                                .and_then(|t| t.boolean_labels.clone()),
                                            abort_only_mode: *abort_only_mode.read(),
                                            theme: theme.clone(),
                                        }
                                    }
                            },
                            MainTab::ConnectionStatus => rsx! {
                                ConnectionStatusTab {
                                    boards: board_status,
                                    expected_boards: layout.network_tab.expected_boards.clone(),
                                    layout: layout.connection_tab.clone(),
                                    title: _main_tab_label(&layout, MainTab::ConnectionStatus),
                                    theme: theme.clone(),
                                }
                            },
                            MainTab::Detailed => rsx! {
                                DetailedTab {
                                    metrics: frontend_network_metrics,
                                    board_status: board_status,
                                    network_topology: network_topology,
                                    flight_state: flight_state,
                                    warnings: warnings,
                                    errors: errors,
                                    notifications: notifications,
                                    network_time: network_time,
                                }
                            },
                            MainTab::NetworkTopology => rsx! {
                                div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                                    NetworkTopologyTab {
                                        topology: network_topology,
                                        layout: layout.network_tab.clone(),
                                        flow_animation_enabled: *network_flow_animation_enabled.read(),
                                    }
                                }
                            },
                            MainTab::Map => rsx! {
                                MapTab {
                                    key: "{*WS_EPOCH.read()}",
                                    rocket_gps: rocket_gps,
                                    user_gps: user_gps,
                                    distance_units_metric: *distance_units_metric.read(),
                                    theme: theme.clone(),
                                    title: _main_tab_label(&layout, MainTab::Map),
                                }
                            },
                            MainTab::Actions => rsx! {
                                div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                                    ActionsTab {
                                        layout: layout.actions_tab.clone(),
                                        action_policy: action_policy,
                                        abort_only_mode: *abort_only_mode.read(),
                                        theme: theme.clone(),
                                    }
                                }
                            },
                            MainTab::Calibration => rsx! {
                                div { style: "height:100%; overflow-y:auto; overflow-x:hidden;",
                                    CalibrationTab {}
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
                                    active_tab: active_data_tab,
                                    layout: layout.data_tab.clone(),
                                    theme: theme.clone(),
                                }
                            },
                        }
                    }
                }
                }
                {settings_overlay}
                {version_overlay}
            }
}

fn send_cmd(cmd: &str) {
    if !auth::can_send_command(cmd) {
        return;
    }
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
        row.values.first().copied().flatten()? as f64,
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
pub(crate) async fn http_get_json<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
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

    let mut request = Request::get(&url);
    if let Some(token) = auth::current_token() {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if status == 401 {
        auth::clear_current_session();
    }
    if !(200..300).contains(&status) {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("HTTP {status}: {}", snippet.trim()));
    }
    serde_json::from_str::<T>(&body).map_err(|e| e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn native_http_timeouts(path: &str) -> (std::time::Duration, std::time::Duration) {
    if path == "/api/recent" {
        let secs = std::env::var("GS_RECENT_HTTP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(300)
            .clamp(15, 600);
        return (
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(secs),
        );
    }

    (
        std::time::Duration::from_secs(8),
        std::time::Duration::from_secs(8),
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn http_get_json<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
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
    let (connect_timeout, timeout) = native_http_timeouts(&path);

    let client =
        auth::build_native_http_client(UrlConfig::_skip_tls_verify(), connect_timeout, timeout)?;
    let skip_tls = UrlConfig::_skip_tls_verify();
    log!(
        "[http] GET {} skip_tls={} connect_timeout_ms={} timeout_ms={}",
        url,
        skip_tls,
        connect_timeout.as_millis(),
        timeout.as_millis()
    );

    let mut request = client.get(url);
    if let Some(token) = auth::current_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| {
        let msg = format!(
            "request send failed: {e:?} (base={} skip_tls={skip_tls} path={path})",
            UrlConfig::base_http()
        );
        log!("[http] {msg}");
        msg
    })?;

    let status = response.status();
    let body = response.text().await.map_err(|e| {
        let msg = format!(
            "response body read failed: {e:?} (base={} skip_tls={skip_tls} path={path})",
            UrlConfig::base_http()
        );
        log!("[http] {msg}");
        msg
    })?;
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            auth::clear_current_session();
        }
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("HTTP {}: {}", status, snippet.trim()));
    }

    serde_json::from_str::<T>(&body).map_err(|e| {
        let snippet: String = body.chars().take(200).collect();
        format!("invalid JSON ({e}): {}", snippet.trim())
    })
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn http_post_json<B: Serialize, T: for<'de> Deserialize<'de>>(
    path: &str,
    body: &B,
) -> Result<T, String> {
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

    let mut request = Request::post(&url);
    if let Some(token) = auth::current_token() {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let response = request
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if status == 401 {
        auth::clear_current_session();
    }
    if !(200..300).contains(&status) {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("HTTP {status}: {}", snippet.trim()));
    }
    serde_json::from_str::<T>(&body).map_err(|e| e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn http_post_json<B: Serialize, T: for<'de> Deserialize<'de>>(
    path: &str,
    body: &B,
) -> Result<T, String> {
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

    let client = auth::build_native_http_client(
        UrlConfig::_skip_tls_verify(),
        std::time::Duration::from_secs(8),
        std::time::Duration::from_secs(8),
    )?;

    let mut request = client.post(url).json(body);
    if let Some(token) = auth::current_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        auth::clear_current_session();
    }
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("HTTP {}: {}", status, snippet.trim()));
    }
    serde_json::from_str::<T>(&body).map_err(|e| e.to_string())
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

    let mut request = Request::post(&url);
    if let Some(token) = auth::current_token() {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == 401 {
        auth::clear_current_session();
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", body.trim()));
    }
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

    let client = auth::build_native_http_client(
        UrlConfig::_skip_tls_verify(),
        std::time::Duration::from_secs(8),
        std::time::Duration::from_secs(8),
    )?;

    let mut request = client.post(url);
    if let Some(token) = auth::current_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        auth::clear_current_session();
    }
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, body.trim()));
    }
    Ok(())
}

async fn dismiss_notification_remote(id: u64) -> Result<(), String> {
    http_post_empty(&format!("/api/notifications/{id}/dismiss")).await
}

#[cfg(target_arch = "wasm32")]
fn spawn_detached<F>(fut: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(fut);
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_detached<F>(fut: F)
where
    F: Future<Output = ()> + 'static,
{
    spawn(fut);
}

fn auth_ws_url(base_ws: &str) -> String {
    let mut url = format!("{}/ws", base_ws.trim_end_matches('/'));
    if let Some(token) = auth::current_token() {
        let sep = if url.contains('?') { '&' } else { '?' };
        url.push(sep);
        url.push_str("token=");
        url.push_str(&token);
    }
    url
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
    warnings: &mut Signal<Vec<AlertMsg>>,
    errors: &mut Signal<Vec<AlertMsg>>,
    notifications: &mut Signal<Vec<PersistentNotification>>,
    notification_history: &mut Signal<Vec<PersistentNotification>>,
    dismissed_notifications: &mut Signal<Vec<DismissedNotification>>,
    unread_notification_ids: &mut Signal<Vec<u64>>,
    action_policy: &mut Signal<ActionPolicyMsg>,
    network_time: &mut Signal<Option<NetworkTimeSync>>,
    network_topology: &mut Signal<NetworkTopologyMsg>,
    board_status: &mut Signal<Vec<BoardStatusEntry>>,
    rocket_gps: &mut Signal<Option<(f64, f64)>>,
    _user_gps: &mut Signal<Option<(f64, f64)>>,
    ack_warning_ts: &mut Signal<i64>,
    ack_error_ts: &mut Signal<i64>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    log!("[seed] seed_from_db entered");
    struct ReseedGuard;
    impl Drop for ReseedGuard {
        fn drop(&mut self) {
            RESEED_IN_PROGRESS.store(false, Ordering::Relaxed);
            if let Ok(mut v) = RESEED_LIVE_BUFFER.lock() {
                v.clear();
            }
            charts_cache_cancel_reseed_build();
            log!("[seed] seed_from_db exiting");
        }
    }
    RESEED_IN_PROGRESS.store(true, Ordering::Relaxed);
    if let Ok(mut v) = RESEED_LIVE_BUFFER.lock() {
        v.clear();
    }
    charts_cache_begin_reseed_build();
    let _reseed_guard = ReseedGuard;

    fn merge_db_and_live(
        mut db_rows: Vec<TelemetryRow>,
        live_rows: Vec<TelemetryRow>,
    ) -> Vec<TelemetryRow> {
        // Keep full overlap, then compact to the same bucket density the chart can render.
        db_rows.extend(live_rows);
        compact_rows_for_ui(db_rows)
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
    let existing_rows_before_seed = ui_telemetry_rows_snapshot();
    log!(
        "[seed] /api/recent begin existing_rows_before_seed={}",
        existing_rows_before_seed.len()
    );
    match http_get_json::<Vec<TelemetryRow>>("/api/recent").await {
        Ok(mut list) => {
            if !alive.load(Ordering::Relaxed) {
                return Ok(());
            }

            sort_rows(&mut list);
            prune_history(&mut list);
            list = compact_rows_for_ui(list);
            log!("[seed] /api/recent ok compacted_rows={}", list.len());

            // Capture rows that arrived while reseed was running and keep them.
            let mut live_rows = ui_telemetry_rows_snapshot();
            live_rows.extend(queue_snapshot());
            live_rows.extend(ui_telemetry_rows_snapshot());
            live_rows.extend(queue_snapshot());
            if !live_rows.is_empty() {
                sort_rows(&mut live_rows);
                prune_history(&mut live_rows);
                live_rows = compact_rows_for_ui(live_rows);
                log!("[seed] /api/recent merging live_rows={}", live_rows.len());
                list = merge_db_and_live(list, live_rows);
            }

            if let Some(gps) = list.iter().rev().find_map(row_to_gps) {
                rocket_gps.set(Some(gps));
            }

            if list.is_empty() && !existing_rows_before_seed.is_empty() {
                // Treat empty reseed as transient and keep already-visible history.
                log!("[seed] /api/recent empty -> keeping existing rows");
                list = existing_rows_before_seed;
            } else {
                // Build reseed cache in a double buffer while active cache keeps live updates.
                const RESEED_INGEST_CHUNK: usize = 1024;
                for (i, row) in list.iter().enumerate() {
                    charts_cache_reseed_ingest_row(row);
                    if i % RESEED_INGEST_CHUNK == 0 {
                        cooperative_yield().await;
                    }
                }

                // Replay queued rows into reseed cache as a second safety net.
                let post_reset_queued_rows = queue_snapshot();
                for row in &post_reset_queued_rows {
                    charts_cache_reseed_ingest_row(row);
                }
                if !post_reset_queued_rows.is_empty() {
                    list.extend(post_reset_queued_rows);
                    list = compact_rows_for_ui(list);
                }

                // Replay live rows received during reseed build.
                let reseed_live_rows = if let Ok(mut v) = RESEED_LIVE_BUFFER.lock() {
                    let rows = v.clone();
                    v.clear();
                    rows
                } else {
                    Vec::new()
                };
                if !reseed_live_rows.is_empty() {
                    for row in &reseed_live_rows {
                        charts_cache_reseed_ingest_row(row);
                    }
                    list.extend(reseed_live_rows);
                    list = compact_rows_for_ui(list);
                }

                // Atomically swap the prepared reseed cache in.
                charts_cache_finish_reseed_build();
            }
            log!("[seed] applying reseed rows={}", list.len());
            if let Ok(mut store) = UI_TELEMETRY_STORE.lock() {
                store.replace_from_rows(&list);
            }
            reset_latest_telemetry(&list);
            if !list.is_empty() {
                DASHBOARD_HAS_CONNECTED.store(true, Ordering::Relaxed);
            }
            let mut render_epoch = TELEMETRY_RENDER_EPOCH.write();
            *render_epoch = render_epoch.wrapping_add(1);
        }
        Err(err) => {
            log!("[seed] /api/recent failed: {err}");
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

    if let Ok(topology) = http_get_json::<NetworkTopologyMsg>("/api/network_topology").await
        && alive.load(Ordering::Relaxed)
    {
        network_topology.set(topology);
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
        && let Some(rocket) = gps.rocket
    {
        rocket_gps.set(Some((rocket.lat, rocket.lon)));
    }

    Ok(())
}

// ---------------------------------------------------------
// WebSocket supervisor (reconnect loop) — both platforms
// ---------------------------------------------------------
#[allow(clippy::too_many_arguments)]
async fn connect_ws_supervisor(
    epoch: u64,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
    network_topology: Signal<NetworkTopologyMsg>,
    warning_event_counter: Signal<u64>,
    error_event_counter: Signal<u64>,
    flight_state: Signal<FlightState>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut warnings = warnings;
    let mut warning_event_counter = warning_event_counter;

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
                    warnings,
                    errors,
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
                    network_topology,
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
                    warnings,
                    errors,
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
                    network_topology,
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
            note_ws_connect_failure_warning(
                &mut warnings,
                &mut warning_event_counter,
                &auth_ws_url(&UrlConfig::base_ws()),
                &e,
            );
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
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
    network_topology: Signal<NetworkTopologyMsg>,
    warning_event_counter: Signal<u64>,
    error_event_counter: Signal<u64>,
    flight_state: Signal<FlightState>,
    board_status: Signal<Vec<BoardStatusEntry>>,
    rocket_gps: Signal<Option<(f64, f64)>>,
    user_gps: Signal<Option<(f64, f64)>>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    use futures_channel::oneshot;
    use js_sys::Reflect;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;
    use wasm_bindgen::closure::Closure;
    use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};

    if !alive.load(Ordering::Relaxed) {
        return Ok(());
    }

    let base_ws = UrlConfig::base_ws();
    let ws_url = auth_ws_url(&base_ws);

    log!("[WS] connecting to {ws_url} (epoch={epoch})");

    let ws = WebSocket::new(&ws_url).map_err(|_| "failed to create websocket".to_string())?;
    note_ws_connection_state(false, ws_url.clone(), None, epoch);

    *WS_RAW.write() = Some(ws.clone());
    *WS_SENDER.write() = Some(WsSender { ws: ws.clone() });

    let (closed_tx, closed_rx) = oneshot::channel::<()>();
    let closed_tx = std::rc::Rc::new(std::cell::RefCell::new(Some(closed_tx)));

    {
        let ws_url_for_open = ws_url.clone();
        let onopen: Closure<dyn FnMut(Event)> = Closure::new(move |_e: Event| {
            log!("[WS] open");
            note_ws_connection_state(true, ws_url_for_open.clone(), None, epoch);
            bump_seed_epoch();
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();
    }

    {
        let alive_for_message = alive.clone();
        let onmessage: Closure<dyn FnMut(MessageEvent)> = Closure::new(move |e: MessageEvent| {
            if !alive_for_message.load(Ordering::Relaxed) {
                return;
            }
            if let Some(s) = e.data().as_string() {
                handle_ws_message(
                    &s,
                    warnings,
                    errors,
                    notifications,
                    notification_history,
                    dismissed_notifications,
                    unread_notification_ids,
                    action_policy,
                    network_time,
                    network_topology,
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
        let alive_for_error = alive.clone();
        let onerror: Closure<dyn FnMut(ErrorEvent)> = Closure::new(move |e: ErrorEvent| {
            if !alive_for_error.load(Ordering::Relaxed) {
                return;
            }
            let message = Reflect::get(e.as_ref(), &JsValue::from_str("message"))
                .ok()
                .and_then(|v| v.as_string())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "websocket error event".to_string());
            log!("[WS] error: {message}");
            if let Some(tx) = closed_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
        });
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();
    }

    {
        let closed_tx = closed_tx.clone();
        let alive_for_close = alive.clone();
        let onclose: Closure<dyn FnMut(CloseEvent)> = Closure::new(move |e: CloseEvent| {
            if !alive_for_close.load(Ordering::Relaxed) {
                return;
            }
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
        note_ws_connection_state(false, ws_url, Some("websocket closed".to_string()), epoch);
        *WS_SENDER.write() = None;
        *WS_RAW.write() = None;
    }

    Err("websocket closed".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn insecure_rustls_connector() -> Result<tokio_tungstenite::Connector, String> {
    #[cfg(target_os = "windows")]
    {
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|e| format!("native-tls connector build failed: {e}"))?;
        return Ok(tokio_tungstenite::Connector::NativeTls(connector));
    }

    #[cfg(not(target_os = "windows"))]
    {
        #[derive(Debug)]
        struct NoCertificateVerification(std::sync::Arc<rustls::crypto::CryptoProvider>);

        impl rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
            fn verify_server_cert(
                &self,
                _end_entity: &rustls::pki_types::CertificateDer<'_>,
                _intermediates: &[rustls::pki_types::CertificateDer<'_>],
                _server_name: &rustls::pki_types::ServerName<'_>,
                _ocsp_response: &[u8],
                _now: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }

            fn verify_tls12_signature(
                &self,
                message: &[u8],
                cert: &rustls::pki_types::CertificateDer<'_>,
                dss: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                rustls::crypto::verify_tls12_signature(
                    message,
                    cert,
                    dss,
                    &self.0.signature_verification_algorithms,
                )
            }

            fn verify_tls13_signature(
                &self,
                message: &[u8],
                cert: &rustls::pki_types::CertificateDer<'_>,
                dss: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                rustls::crypto::verify_tls13_signature(
                    message,
                    cert,
                    dss,
                    &self.0.signature_verification_algorithms,
                )
            }

            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                self.0.signature_verification_algorithms.supported_schemes()
            }
        }

        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .ok_or_else(|| "rustls default crypto provider is not set".to_string())?;

        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(NoCertificateVerification(
                provider,
            )))
            .with_no_client_auth();

        Ok(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
            config,
        )))
    }
}

#[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
fn platform_rustls_connector() -> Result<tokio_tungstenite::Connector, String> {
    use rustls_platform_verifier::ConfigVerifierExt;
    let tls_config = rustls::ClientConfig::with_platform_verifier()
        .map_err(|e| format!("platform TLS verifier setup failed: {e}"))?;
    Ok(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
        tls_config,
    )))
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn connect_ws_once_native(
    epoch: u64,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
    network_topology: Signal<NetworkTopologyMsg>,
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
    let ws_url = auth_ws_url(&base_ws);

    log!("[WS] connecting to {ws_url} (epoch={epoch})");
    note_ws_connection_state(false, ws_url.clone(), None, epoch);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    *WS_SENDER.write() = Some(WsSender { tx });

    let ws_stream = if UrlConfig::_skip_tls_verify() && ws_url.starts_with("wss://") {
        let tls = insecure_rustls_connector()
            .map_err(|e| format!("[WS] rustls connector build failed: {e}"))?;
        tokio_tungstenite::connect_async_tls_with_config(ws_url.as_str(), None, false, Some(tls))
            .await
            .map_err(|e| format!("[WS] connect failed: {e}"))?
            .0
    } else if ws_url.starts_with("wss://") {
        #[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
        {
            let tls = platform_rustls_connector()
                .map_err(|e| format!("[WS] platform rustls connector build failed: {e}"))?;
            tokio_tungstenite::connect_async_tls_with_config(
                ws_url.as_str(),
                None,
                false,
                Some(tls),
            )
            .await
            .map_err(|e| format!("[WS] connect failed: {e}"))?
            .0
        }
        #[cfg(not(any(target_os = "android", target_os = "ios", target_os = "macos")))]
        {
            tokio_tungstenite::connect_async(ws_url.as_str())
                .await
                .map_err(|e| format!("[WS] connect failed: {e}"))?
                .0
        }
    } else {
        tokio_tungstenite::connect_async(ws_url.as_str())
            .await
            .map_err(|e| format!("[WS] connect failed: {e}"))?
            .0
    };

    let (mut write, mut read) = ws_stream.split();
    note_ws_connection_state(true, ws_url.clone(), None, epoch);
    bump_seed_epoch();

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
                warnings,
                errors,
                notifications,
                notification_history,
                dismissed_notifications,
                unread_notification_ids,
                action_policy,
                network_time,
                network_topology,
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
        note_ws_connection_state(false, ws_url, Some("websocket closed".to_string()), epoch);
        *WS_SENDER.write() = None;
    }

    Err("websocket closed".to_string())
}

#[allow(clippy::too_many_arguments)]
fn handle_ws_message(
    s: &str,
    warnings: Signal<Vec<AlertMsg>>,
    errors: Signal<Vec<AlertMsg>>,
    notifications: Signal<Vec<PersistentNotification>>,
    notification_history: Signal<Vec<PersistentNotification>>,
    dismissed_notifications: Signal<Vec<DismissedNotification>>,
    unread_notification_ids: Signal<Vec<u64>>,
    action_policy: Signal<ActionPolicyMsg>,
    network_time: Signal<Option<NetworkTimeSync>>,
    network_topology: Signal<NetworkTopologyMsg>,
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
    let mut network_topology = network_topology;
    let mut flight_state = flight_state;
    let mut board_status = board_status;
    let mut rocket_gps = rocket_gps;
    let _user_gps = user_gps;

    let Ok(msg) = serde_json::from_str::<WsInMsg>(s) else {
        return;
    };
    note_incoming_ws_message(s.len());

    match msg {
        WsInMsg::Telemetry(row) => {
            note_incoming_telemetry_rows(1, 0);
            charts_cache_ingest_row(&row);
            update_latest_telemetry(&row);
            if RESEED_IN_PROGRESS.load(Ordering::Relaxed)
                && let Ok(mut v) = RESEED_LIVE_BUFFER.lock()
            {
                v.push(row.clone());
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
            note_incoming_telemetry_rows(batch.len(), 1);
            if let Ok(mut q) = TELEMETRY_QUEUE.lock() {
                for row in batch {
                    charts_cache_ingest_row(&row);
                    update_latest_telemetry(&row);
                    if RESEED_IN_PROGRESS.load(Ordering::Relaxed)
                        && let Ok(mut v) = RESEED_LIVE_BUFFER.lock()
                    {
                        v.push(row.clone());
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

        WsInMsg::NetworkTopology(topology) => {
            network_topology.set(topology);
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
#[cfg(any(target_arch = "wasm32", target_os = "ios"))]
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
pub(crate) fn js_eval(js: &str) {
    let _ = js_sys::eval(js);
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn js_eval(js: &str) {
    dioxus::document::eval(js);
}

#[cfg(target_arch = "wasm32")]
fn js_get_tmp_str() -> Option<String> {
    let win = web_sys::window()?;
    let v = js_sys::Reflect::get(&win, &wasm_bindgen::JsValue::from_str("__gs26_tmp_str")).ok()?;
    v.as_string()
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "ios"))]
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
