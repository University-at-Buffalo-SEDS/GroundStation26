//! Personnel proximity lockout (docs/proximity-lock.md).
//!
//! An administrator may opt the station in. Each signed-in device that
//! participates works out on the device whether it is within the lockout
//! radius of the rocket and reports only a flag:
//! `{participating, in_range, accuracy class, reference kind, heartbeat}`.
//! No latitude, longitude, distance or bearing is accepted (unknown fields
//! are rejected), stored or logged here.
//!
//! While the feature is on and a participating device with a fresh report is
//! in range, the station refuses NEW arming, pressurizing and launch
//! commands (409) and disables those controls with the reason. Safing
//! (Abort, Stop, vent/dump, supply close, pause/cancel, Reset/Recover) is
//! never refused. Nothing is actuated automatically: someone entering range
//! during a fill raises a warning, the fill continues under the operator's
//! control. A participating device that stops reporting is "unknown"; the
//! stale policy decides whether unknown devices block or only warn. An
//! administrator may override the lock for a limited time.
//!
//! State is in memory only (device flags, the override); the settings are
//! persisted in the data directory. Every change is an audit event (station
//! messages and `GET /api/proximity/audit`), with device names, never places.
use crate::auth::{AuthPrincipal, Permission};
use crate::state::AppState;
use crate::types::{FlightState, TelemetryCommand};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[cfg(test)]
mod tests;

const FILE: &str = "proximity_lock.json";
/// Devices report this often, and at once on a change.
pub const HEARTBEAT_INTERVAL_S: u32 = 15;
pub const RADIUS_RANGE_M: std::ops::RangeInclusive<u32> = 25..=2000;
pub const EXIT_MARGIN_RANGE_M: std::ops::RangeInclusive<u32> = 5..=500;
pub const HEARTBEAT_TIMEOUT_RANGE_S: std::ops::RangeInclusive<u32> = 30..=600;
pub const OVERRIDE_RANGE_MIN: std::ops::RangeInclusive<u32> = 1..=30;
/// Rate limit per device: at most this many reports in the window.
const RATE_WINDOW: Duration = Duration::from_secs(30);
const RATE_MAX: usize = 12;
const MAX_DEVICES: usize = 256;
const MAX_DEVICES_PER_USER: usize = 8;
/// A device silent this long is forgotten (an unknown one stops counting).
const FORGET_AFTER: Duration = Duration::from_secs(12 * 3600);
const AUDIT_CAP: usize = 200;
const NAME_MAX: usize = 48;

// ---------------------------------------------------------------------------
// Settings

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StalePolicy {
    /// A participating device with no fresh report is listed and warned
    /// about, but does not block (the default).
    #[default]
    Warn,
    /// A participating device with no fresh report blocks like one in range.
    Block,
}

/// Which command classes the lock refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocks {
    /// Postinit, operator Advance (Fill to Armed), automatic Armed, LIVE PYRO.
    pub arming: bool,
    /// Fill sequences: Start fill/continue, Nitrogen test, Turbocharge.
    pub fill: bool,
    /// Launch sequences: Launch, GroundStationLaunch, igniter on/sequence.
    pub launch: bool,
    /// Single manual valve opens (Nitrogen, Nitrous, PILOT). Off by default:
    /// pad crew work valves by hand next to the vehicle. Valve self-test is
    /// never refused.
    #[serde(default)]
    pub manual_valves: bool,
}

impl Default for Blocks {
    fn default() -> Self {
        Self { arming: true, fill: true, launch: true, manual_valves: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub enabled: bool,
    pub radius_m: u32,
    /// Hysteresis: a device in range leaves only beyond radius + this margin
    /// (or its location accuracy, whichever is larger).
    pub exit_margin_m: u32,
    pub blocks: Blocks,
    pub stale_policy: StalePolicy,
    /// A participating device with no report for this long is unknown.
    pub heartbeat_timeout_s: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            radius_m: 150,
            exit_margin_m: 30,
            blocks: Blocks::default(),
            stale_policy: StalePolicy::Warn,
            heartbeat_timeout_s: 60,
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<(), String> {
        if !RADIUS_RANGE_M.contains(&self.radius_m) {
            return Err(format!("The radius must be {}–{} m.", RADIUS_RANGE_M.start(), RADIUS_RANGE_M.end()));
        }
        if !EXIT_MARGIN_RANGE_M.contains(&self.exit_margin_m) {
            return Err(format!(
                "The exit margin must be {}–{} m.",
                EXIT_MARGIN_RANGE_M.start(),
                EXIT_MARGIN_RANGE_M.end()
            ));
        }
        if !HEARTBEAT_TIMEOUT_RANGE_S.contains(&self.heartbeat_timeout_s) {
            return Err(format!(
                "The heartbeat timeout must be {}–{} s.",
                HEARTBEAT_TIMEOUT_RANGE_S.start(),
                HEARTBEAT_TIMEOUT_RANGE_S.end()
            ));
        }
        if self.enabled && !(self.blocks.arming || self.blocks.fill || self.blocks.launch || self.blocks.manual_valves) {
            return Err("Choose at least one blocked action class, or turn the lock off.".into());
        }
        Ok(())
    }

    fn blocks(&self, class: ActionClass) -> bool {
        match class {
            ActionClass::Arming => self.blocks.arming,
            ActionClass::Fill => self.blocks.fill,
            ActionClass::Launch => self.blocks.launch,
            ActionClass::ManualValves => self.blocks.manual_valves,
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.heartbeat_timeout_s))
    }

    /// One line per changed field, for the audit log.
    fn diff(&self, next: &Settings) -> Vec<String> {
        let mut out = Vec::new();
        let on_off = |b: bool| if b { "on" } else { "off" };
        if self.enabled != next.enabled {
            out.push(format!("lock {}", on_off(next.enabled)));
        }
        if self.radius_m != next.radius_m {
            out.push(format!("radius {} m -> {} m", self.radius_m, next.radius_m));
        }
        if self.exit_margin_m != next.exit_margin_m {
            out.push(format!("exit margin {} m -> {} m", self.exit_margin_m, next.exit_margin_m));
        }
        for (name, before, after) in [
            ("arming", self.blocks.arming, next.blocks.arming),
            ("fill/pressurizing", self.blocks.fill, next.blocks.fill),
            ("launch/ignition", self.blocks.launch, next.blocks.launch),
            ("manual valve opens", self.blocks.manual_valves, next.blocks.manual_valves),
        ] {
            if before != after {
                out.push(format!("block {name} {}", on_off(after)));
            }
        }
        if self.stale_policy != next.stale_policy {
            out.push(format!("unknown devices {}", stale_label(next.stale_policy)));
        }
        if self.heartbeat_timeout_s != next.heartbeat_timeout_s {
            out.push(format!("heartbeat timeout {} s -> {} s", self.heartbeat_timeout_s, next.heartbeat_timeout_s));
        }
        out
    }
}

fn stale_label(policy: StalePolicy) -> &'static str {
    match policy {
        StalePolicy::Warn => "warn",
        StalePolicy::Block => "block",
    }
}

/// `PUT /api/proximity/settings`: partial, admin only.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SettingsUpdate {
    enabled: Option<bool>,
    radius_m: Option<u32>,
    exit_margin_m: Option<u32>,
    blocks: Option<Blocks>,
    stale_policy: Option<StalePolicy>,
    heartbeat_timeout_s: Option<u32>,
}

impl SettingsUpdate {
    fn apply(self, current: Settings) -> Settings {
        Settings {
            enabled: self.enabled.unwrap_or(current.enabled),
            radius_m: self.radius_m.unwrap_or(current.radius_m),
            exit_margin_m: self.exit_margin_m.unwrap_or(current.exit_margin_m),
            blocks: self.blocks.unwrap_or(current.blocks),
            stale_policy: self.stale_policy.unwrap_or(current.stale_policy),
            heartbeat_timeout_s: self.heartbeat_timeout_s.unwrap_or(current.heartbeat_timeout_s),
        }
    }
}

// ---------------------------------------------------------------------------
// Command classes

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    Arming,
    Fill,
    Launch,
    ManualValves,
}

/// The class of a command the lock may refuse, or None (never refused).
/// `opens(name)` says whether a toggle would open its valve / turn its
/// output on now; a toggle that would close or turn off is safing and never
/// refused. Valve self-test (run on the pad with crew nearby), explicit closes, vent, dump, pause, cancel, Abort, Rewind,
/// Reset, Recover and every flight-computer evaluation/abort command are
/// not listed and so never refused.
pub fn classify(name: &str, opens: impl Fn(&str) -> bool) -> Option<ActionClass> {
    match name {
        // Kalman: start starts flight evaluation like Launch does on the FC.
        "Postinit" | "AdvanceFlightState" | "IgtLivePyro" | "KalmanStart" => Some(ActionClass::Arming),
        "StartFill" | "ContinueFillSequence" | "NitrogenTest" | "Turbocharge" => Some(ActionClass::Fill),
        "Nitrogen" | "Nitrous" | "Pilot" => opens(name).then_some(ActionClass::ManualValves),
        "Launch" | "GroundStationLaunch" | "IgniterSequence" => Some(ActionClass::Launch),
        "Igniter" => opens(name).then_some(ActionClass::Launch),
        _ => None,
    }
}

/// Every command name the lock can refuse (toggles in their opening direction).
pub const CLASSIFIED: &[&str] = &[
    "Postinit", "AdvanceFlightState", "IgtLivePyro", "KalmanStart", "StartFill", "ContinueFillSequence", "NitrogenTest",
    "Turbocharge", "Nitrogen", "Nitrous", "Launch", "GroundStationLaunch",
    "IgniterSequence", "Igniter", "Pilot",
];

/// The output key behind a toggle, as the station tracks it.
fn toggle_key(name: &str) -> Option<u8> {
    use crate::rocket_commands::{ActuatorBoardCommands as Ab, ValveBoardCommands as Vb};
    Some(match name {
        "Nitrogen" => Ab::NitrogenOpen as u8,
        "Nitrous" => Ab::NitrousOpen as u8,
        "Igniter" => Ab::IgniterOn as u8,
        "Pilot" => Vb::PilotOpen as u8,
        _ => return None,
    })
}

/// On the primary: a toggle opens unless the station holds it open
/// (requested or confirmed). An unknown position counts as opening.
fn primary_opens(state: &AppState, name: &str) -> bool {
    let Some(key) = toggle_key(name) else { return true };
    let open = state
        .get_pending_umbilical_valve_state(key)
        .or_else(|| state.get_umbilical_valve_state(key));
    open != Some(true)
}

// ---------------------------------------------------------------------------
// Device reports

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Ios,
    Android,
    Web,
    Desktop,
    Watch,
}

/// The device's location-accuracy class (the radius of its fix's accuracy
/// circle): high <= 20 m, medium <= 100 m, low beyond.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accuracy {
    High,
    Medium,
    Low,
}

/// What the device measured against: the rocket's fresh GPS fix, or the
/// configured launch site when it has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reference {
    Rocket,
    Pad,
}

/// Why a device does not participate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptOut {
    /// The user turned participation off on this device.
    User,
    /// The operating system's location permission is denied.
    PermissionDenied,
    /// This device cannot provide a location.
    Unsupported,
}

impl OptOut {
    fn label(self) -> &'static str {
        match self {
            OptOut::User => "opted out",
            OptOut::PermissionDenied => "location permission denied",
            OptOut::Unsupported => "no location on this device",
        }
    }
}

/// Why a participating device cannot tell whether it is in range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CannotTell {
    /// No recent location fix on the device.
    NoFix,
    /// Nothing to measure against: no fresh rocket fix and no launch site
    /// configured at the station.
    NoReference,
    /// The location permission prompt is not answered yet.
    AwaitingPermission,
}

impl CannotTell {
    fn label(self) -> &'static str {
        match self {
            CannotTell::NoFix => "no recent location fix on the device",
            CannotTell::NoReference => "no fresh rocket GPS fix and no launch site configured at the station",
            CannotTell::AwaitingPermission => "location permission not answered on the device",
        }
    }
}

/// `POST /api/proximity/report`. Exactly these fields: anything else
/// (a latitude, a distance) is refused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// Wire version, 1.
    pub v: u8,
    pub device_id: String,
    pub device_name: String,
    pub platform: Platform,
    pub participating: bool,
    /// Participating: true / false, or null while the device cannot tell
    /// (no location fix, or no reference). Ignored when not participating.
    #[serde(default)]
    pub in_range: Option<bool>,
    #[serde(default)]
    pub accuracy: Option<Accuracy>,
    #[serde(default)]
    pub reference: Option<Reference>,
    /// The device keeps reporting while its app is in the background.
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub opt_out: Option<OptOut>,
    /// Participating with `in_range` null: why the device cannot tell.
    #[serde(default)]
    pub cannot_tell: Option<CannotTell>,
    /// Increments with every report from this device (heartbeat counter).
    pub seq: u64,
    /// The device's clock when it sent the report (informational: freshness
    /// is judged by the station's receive time).
    pub sent_ms: i64,
}

fn valid_device_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX || name.chars().any(char::is_control) {
        return Err(format!("device_name must be 1–{NAME_MAX} printable characters."));
    }
    Ok(name.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    InRange,
    OutOfRange,
    /// Participating, but no fresh report or it cannot tell.
    Unknown,
    OptedOut,
}

#[derive(Clone, Debug)]
struct Device {
    username: String,
    name: String,
    platform: Platform,
    participating: bool,
    in_range: Option<bool>,
    accuracy: Option<Accuracy>,
    reference: Option<Reference>,
    background: bool,
    opt_out: Option<OptOut>,
    cannot_tell: Option<CannotTell>,
    last_seen: Instant,
    last_seen_ms: i64,
    recent: VecDeque<Instant>,
    /// The state last written to the audit log.
    announced: Option<DeviceState>,
}

impl Device {
    fn state(&self, now: Instant, timeout: Duration) -> DeviceState {
        if !self.participating {
            return DeviceState::OptedOut;
        }
        if now.saturating_duration_since(self.last_seen) > timeout {
            return DeviceState::Unknown;
        }
        match self.in_range {
            Some(true) => DeviceState::InRange,
            Some(false) => DeviceState::OutOfRange,
            None => DeviceState::Unknown,
        }
    }

    /// Why the device is unknown, for the operator: silent, or what the
    /// device said it lacks. None unless the state is unknown.
    fn unknown_detail(&self, now: Instant, timeout: Duration) -> Option<String> {
        if self.state(now, timeout) != DeviceState::Unknown {
            return None;
        }
        if now.saturating_duration_since(self.last_seen) > timeout {
            return Some(format!("no report for over {} s", timeout.as_secs()));
        }
        Some(match self.cannot_tell {
            Some(reason) => format!("cannot tell: {}", reason.label()),
            None => "cannot tell".to_owned(),
        })
    }

    /// The fresh report's reason, shown only while the device is unknown and
    /// still reporting.
    fn cannot_tell_now(&self, now: Instant, timeout: Duration) -> Option<CannotTell> {
        let fresh = now.saturating_duration_since(self.last_seen) <= timeout;
        (fresh && self.state(now, timeout) == DeviceState::Unknown).then_some(self.cannot_tell).flatten()
    }
}

/// A public device id: a short hash of the account and the device id.
fn public_id(key: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, key.as_bytes())
        .as_ref()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Override and audit

#[derive(Clone, Debug)]
struct Override {
    by: String,
    reason: String,
    until: Instant,
    until_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OverrideView {
    pub by: String,
    pub reason: String,
    pub expires_ms: i64,
    pub remaining_s: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    Settings,
    EnteredRange,
    LeftRange,
    Unknown,
    OptedOut,
    OptedIn,
    Dismissed,
    Override,
    OverrideEnded,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AuditEvent {
    pub at_ms: i64,
    pub kind: AuditKind,
    pub text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideRequest {
    minutes: u32,
    reason: String,
    /// The operator confirmed the dialog.
    confirm: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DismissRequest {
    id: String,
}

// ---------------------------------------------------------------------------
// Evaluation and status

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub user: String,
    pub platform: Platform,
    pub state: DeviceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<Accuracy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
    pub background: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opt_out: Option<OptOut>,
    /// Unknown but still reporting: why the device cannot tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cannot_tell: Option<CannotTell>,
    /// Unknown: the reason in words ("no report for over 60 s", "cannot
    /// tell: no recent location fix on the device").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_reason: Option<String>,
    pub last_report_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Participating devices with a fresh, definite report.
    pub reporting: usize,
    pub in_range: usize,
    pub unknown: usize,
    pub opted_out: usize,
}

/// `GET /api/proximity` and the websocket frame `ProximityLock`. Device
/// names and states only: no positions.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Status {
    pub settings: Settings,
    pub heartbeat_interval_s: u32,
    /// The lock refuses the blocked classes now.
    pub active: bool,
    /// Someone is in range (or unknown under "block"), overridden or not.
    pub condition: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unknown devices under "warn", or someone in range while overridden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub summary: String,
    pub counts: Counts,
    pub devices: Vec<DeviceView>,
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    pub override_view: Option<OverrideView>,
    /// The configured launch site the devices fall back to without a fresh
    /// rocket fix (the rocket's place, never a person's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pad: Option<Pad>,
    /// The commands refused while active.
    pub blocked_commands: Vec<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Pad {
    pub lat_deg: f64,
    pub lon_deg: f64,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The device's name, with a disambiguating account when two devices share it.
fn names(list: &[&Device]) -> String {
    list.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// Unknown devices with the reason for each: "Bo's Pixel: no report for
/// over 60 s; Cy's phone: cannot tell: no recent location fix on the device".
fn unknown_names(list: &[&Device], now: Instant, timeout: Duration) -> String {
    list.iter()
        .map(|d| match d.unknown_detail(now, timeout) {
            Some(detail) => format!("{}: {detail}", d.name),
            None => d.name.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Default)]
struct Inner {
    settings: Settings,
    data_dir: Option<PathBuf>,
    devices: HashMap<String, Device>,
    override_: Option<Override>,
    audit: VecDeque<AuditEvent>,
    /// The automatic Armed hold was announced.
    arming_hold_announced: bool,
    /// Last published lock condition, for transition notices.
    was_active: bool,
}

pub struct Tracker {
    inner: Mutex<Inner>,
    tx: watch::Sender<Status>,
}

impl Default for Tracker {
    fn default() -> Self {
        let inner = Inner::default();
        let status = status_of(&inner, Instant::now(), None);
        Self { inner: Mutex::new(inner), tx: watch::channel(status).0 }
    }
}

struct Blocking {
    reason: String,
    settings: Settings,
}

fn now_ms() -> i64 {
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

/// Builds the status at `now` from the tracker's state.
fn status_of(inner: &Inner, now: Instant, pad: Option<Pad>) -> Status {
    let settings = inner.settings;
    let timeout = settings.timeout();
    let mut devices: Vec<(&String, &Device)> = inner.devices.iter().collect();
    devices.sort_by(|a, b| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()).then(a.0.cmp(b.0)));
    let mut in_range = Vec::new();
    let mut unknown = Vec::new();
    let mut opted_out = Vec::new();
    let mut reporting = 0;
    let mut views = Vec::new();
    for (key, device) in &devices {
        let state = device.state(now, timeout);
        match state {
            DeviceState::InRange => {
                reporting += 1;
                in_range.push(*device);
            }
            DeviceState::OutOfRange => reporting += 1,
            DeviceState::Unknown => unknown.push(*device),
            DeviceState::OptedOut => opted_out.push(*device),
        }
        views.push(DeviceView {
            id: public_id(key),
            name: device.name.clone(),
            user: device.username.clone(),
            platform: device.platform,
            state,
            accuracy: device.accuracy.filter(|_| device.participating),
            reference: device.reference.filter(|_| device.participating),
            background: device.background,
            opt_out: device.opt_out.filter(|_| !device.participating),
            cannot_tell: device.cannot_tell_now(now, timeout),
            unknown_reason: device.unknown_detail(now, timeout),
            last_report_ms: device.last_seen_ms,
        });
    }
    let enabled = settings.enabled;
    let radius = settings.radius_m;
    let mut reasons = Vec::new();
    let mut warnings = Vec::new();
    if enabled && !in_range.is_empty() {
        reasons.push(format!(
            "Proximity lock: {} within {radius} m of the rocket ({})",
            plural(in_range.len(), "person", "people"),
            names(&in_range)
        ));
    }
    if enabled && !unknown.is_empty() {
        let text = format!(
            "{} not reporting or cannot tell ({})",
            plural(unknown.len(), "participating device", "participating devices"),
            unknown_names(&unknown, now, timeout)
        );
        match settings.stale_policy {
            StalePolicy::Block => reasons.push(format!("Proximity lock: {text}; unknown devices block")),
            StalePolicy::Warn => warnings.push(format!("Proximity lock: {text}; unknown devices only warn")),
        }
    }
    let condition = !reasons.is_empty();
    let override_view = inner
        .override_
        .as_ref()
        .filter(|o| enabled && o.until > now)
        .map(|o| OverrideView {
            by: o.by.clone(),
            reason: o.reason.clone(),
            expires_ms: o.until_ms,
            remaining_s: o.until.saturating_duration_since(now).as_secs(),
        });
    let active = condition && override_view.is_none();
    if condition && let Some(o) = &override_view {
        warnings.insert(0, format!("{}. Overridden by {} for {} more s: {}", reasons.join("; "), o.by, o.remaining_s, o.reason));
    }
    let counts = Counts { reporting, in_range: in_range.len(), unknown: unknown.len(), opted_out: opted_out.len() };
    let summary = if !enabled {
        "Proximity lock off".to_owned()
    } else {
        let mut text = format!("Proximity lock: {} reporting", plural(reporting, "device", "devices"));
        if !in_range.is_empty() {
            text.push_str(&format!(", {} in range ({})", in_range.len(), names(&in_range)));
        }
        if !unknown.is_empty() {
            text.push_str(&format!(", {} unknown ({})", unknown.len(), names(&unknown)));
        }
        text.push_str(&format!(", {} opted out", opted_out.len()));
        text
    };
    let blocked_commands = if enabled {
        CLASSIFIED
            .iter()
            .copied()
            .filter(|name| classify(name, |_| true).is_some_and(|class| settings.blocks(class)))
            .collect()
    } else {
        Vec::new()
    };
    Status {
        settings,
        heartbeat_interval_s: HEARTBEAT_INTERVAL_S,
        active,
        condition,
        reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
        warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        summary,
        counts,
        devices: views,
        override_view,
        pad,
        blocked_commands,
    }
}

/// Two statuses that differ only in times (report times, override seconds)
/// are the same for subscribers.
fn same_shape(a: &Status, b: &Status) -> bool {
    let strip = |s: &Status| {
        let mut s = s.clone();
        for d in &mut s.devices {
            d.last_report_ms = 0;
        }
        if let Some(o) = &mut s.override_view {
            o.remaining_s = 0;
        }
        s.warning = None;
        s
    };
    strip(a) == strip(b) && a.warning.is_some() == b.warning.is_some()
}

impl Tracker {
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.tx.subscribe()
    }

    pub fn status_at(&self, now: Instant, pad: Option<Pad>) -> Status {
        status_of(&self.inner.lock().unwrap(), now, pad)
    }

    /// The lock refuses the blocked classes now: the reason and settings.
    fn blocking(&self, now: Instant) -> Option<Blocking> {
        let inner = self.inner.lock().unwrap();
        if !inner.settings.enabled {
            return None;
        }
        let status = status_of(&inner, now, None);
        status.active.then(|| Blocking { reason: status.reason.unwrap_or_default(), settings: inner.settings })
    }

    pub fn audit(&self) -> Vec<AuditEvent> {
        self.inner.lock().unwrap().audit.iter().cloned().collect()
    }

    fn record(inner: &mut Inner, kind: AuditKind, text: String) -> AuditEvent {
        let event = AuditEvent { at_ms: now_ms(), kind, text };
        inner.audit.push_back(event.clone());
        while inner.audit.len() > AUDIT_CAP {
            inner.audit.pop_front();
        }
        event
    }
}

/// The refusal for `cmd` now, or None. Used at every operator admission.
pub(crate) fn refusal(state: &AppState, cmd: &TelemetryCommand) -> Option<String> {
    refusal_at(state, cmd, Instant::now())
}

pub(crate) fn refusal_at(state: &AppState, cmd: &TelemetryCommand, now: Instant) -> Option<String> {
    refusal_for_name(state, admission_name(cmd)?, |name| primary_opens(state, name), now)
}

/// The name the lock classifies. The explicit supply closes share their
/// toggle's control name ("Nitrogen"), so they are named here: never refused.
fn admission_name(cmd: &TelemetryCommand) -> Option<&'static str> {
    match cmd {
        TelemetryCommand::NitrogenClose
        | TelemetryCommand::NitrousClose
        | TelemetryCommand::AutomaticNitrogenClose { .. }
        | TelemetryCommand::AutomaticNitrousClose { .. } => None,
        TelemetryCommand::QueuedOperator { command, .. } => admission_name(command),
        other => Some(crate::sequences::command_name(other)),
    }
}

fn refusal_for_name(state: &AppState, name: &str, opens: impl Fn(&str) -> bool, now: Instant) -> Option<String> {
    let blocking = state.proximity.blocking(now)?;
    let class = classify(name, opens)?;
    blocking.settings.blocks(class).then_some(blocking.reason)
}

/// The admission refusal for `cmd` as a 409 message, with an audit event
/// and a notification. None when admitted.
pub(crate) fn admission_refusal(state: &AppState, cmd: &TelemetryCommand, actor: Option<&str>) -> Option<String> {
    let reason = refusal(state, cmd)?;
    refuse(state, admission_name(cmd)?, actor, reason)
}

fn refuse(state: &AppState, name: &str, actor: Option<&str>, reason: String) -> Option<String> {
    let message = format!("{name} refused. {reason}.");
    {
        let mut inner = state.proximity.inner.lock().unwrap();
        let text = format!("{name} from {} refused: {reason}", actor.unwrap_or("an operator"));
        Tracker::record(&mut inner, AuditKind::Refused, text);
    }
    crate::web::emit_notification_warning(state, message.clone());
    Some(message)
}

/// On an RRLS: refuses a forwarded command by name, judging toggles by the
/// primary's mirrored control state (an unknown position counts as opening;
/// the explicit closes are never refused).
pub(crate) fn rrls_forward_refusal(state: &AppState, name: &str, actor: Option<&str>) -> Option<String> {
    state.proximity.blocking(Instant::now())?;
    let policy = state.action_policy_snapshot();
    let opens = |cmd: &str| {
        policy.controls.iter().find(|c| c.cmd == cmd).and_then(|c| c.actuated) != Some(true)
    };
    let reason = refusal_for_name(state, name, opens, Instant::now())?;
    refuse(state, name, actor, reason)
}

/// The automatic Fill -> Armed publication waits while the lock is active
/// (it asks the FC to arm). Nothing else is held or actuated.
pub(crate) fn holds_automatic_arming(state: &AppState) -> bool {
    let Some(blocking) = state.proximity.blocking(Instant::now()) else {
        state.proximity.inner.lock().unwrap().arming_hold_announced = false;
        return false;
    };
    if !blocking.settings.blocks.arming {
        return false;
    }
    let announce = {
        let mut inner = state.proximity.inner.lock().unwrap();
        !std::mem::replace(&mut inner.arming_hold_announced, true)
    };
    if announce {
        crate::web::emit_notification_warning(
            state,
            format!("Automatic Armed is held. {}. It continues when the lock clears.", blocking.reason),
        );
    }
    true
}

/// Disables the blocked controls in the action policy with the reason. A
/// control already disabled for another reason keeps that reason, with the
/// lock added. `opens` reads each toggle's direction from the policy itself.
pub(crate) fn project_policy(state: &AppState, policy: &mut crate::sequences::ActionPolicyMsg) {
    let Some(blocking) = state.proximity.blocking(Instant::now()) else { return };
    for control in &mut policy.controls {
        let actuated = control.actuated;
        let Some(class) = classify(&control.cmd, |_| actuated != Some(true)) else { continue };
        if !blocking.settings.blocks(class) {
            continue;
        }
        if control.enabled {
            control.enabled = false;
            control.blink = crate::sequences::BlinkMode::None;
            control.disabled_reason = Some(format!("{}.", blocking.reason));
        } else {
            let existing = control.disabled_reason.take().unwrap_or_default();
            control.disabled_reason = Some(if existing.is_empty() {
                format!("{}.", blocking.reason)
            } else {
                format!("{existing} Also: {}.", blocking.reason)
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Changes

fn configured_pad(state: &AppState) -> Option<Pad> {
    crate::launch_site::configured(state).map(|site| Pad { lat_deg: site.lat_deg, lon_deg: site.lon_deg })
}

pub(crate) fn status(state: &AppState) -> Status {
    state.proximity.status_at(Instant::now(), configured_pad(state))
}

/// The vehicle may hold pressure: a fill phase, Armed, or Turbocharge.
fn pressurized_phase(state: &AppState) -> Option<FlightState> {
    let phase = state.local_flight_state_snapshot();
    let filling = matches!(
        phase,
        FlightState::PreFill | FlightState::FillTest | FlightState::NitrogenFill | FlightState::NitrousFill | FlightState::Armed
    );
    (filling || crate::gse::turbocharge_active_nowait(state)).then_some(phase)
}

/// Audit the state transitions since the last call, warn about entries,
/// expire the override, and publish when anything shown changed. Runs after
/// every report and change, and every second.
pub(crate) fn refresh(state: &AppState) {
    refresh_at(state, Instant::now());
}

pub(crate) fn refresh_at(state: &AppState, now: Instant) {
    let mut notices: Vec<(bool, String)> = Vec::new();
    let mut audit_texts: Vec<AuditEvent> = Vec::new();
    let pad = configured_pad(state);
    let pressurized = pressurized_phase(state);
    let (status, active_changed) = {
        let mut inner = state.proximity.inner.lock().unwrap();
        let settings = inner.settings;
        let timeout = settings.timeout();
        // Forget long-silent devices.
        let before = inner.devices.len();
        inner.devices.retain(|_, d| now.saturating_duration_since(d.last_seen) <= FORGET_AFTER);
        if inner.devices.len() != before {
            log::info!("Proximity lock: forgot {} silent device(s).", before - inner.devices.len());
        }
        // Override expiry.
        if inner.override_.as_ref().is_some_and(|o| o.until <= now) {
            let o = inner.override_.take().unwrap();
            let text = format!("Proximity lock override by {} expired.", o.by);
            audit_texts.push(Tracker::record(&mut inner, AuditKind::OverrideEnded, text.clone()));
            notices.push((false, text));
        }
        let radius = settings.radius_m;
        let mut transitions = Vec::new();
        for device in inner.devices.values_mut() {
            let current = device.state(now, timeout);
            if device.announced == Some(current) {
                continue;
            }
            let previous = device.announced.replace(current);
            let detail = device.unknown_detail(now, timeout);
            transitions.push((device.name.clone(), device.username.clone(), previous, current, device.opt_out, detail));
        }
        for (name, user, previous, current, opt_out, detail) in transitions {
            let (kind, text) = match current {
                DeviceState::InRange => (AuditKind::EnteredRange, format!("{name} ({user}) entered the {radius} m lockout radius")),
                DeviceState::OutOfRange if previous.is_none() => {
                    (AuditKind::LeftRange, format!("{name} ({user}) is participating, outside {radius} m"))
                }
                DeviceState::OutOfRange => (AuditKind::LeftRange, format!("{name} ({user}) left the {radius} m lockout radius")),
                DeviceState::Unknown => (
                    AuditKind::Unknown,
                    format!(
                        "{name} ({user}) position unknown ({})",
                        detail.unwrap_or_else(|| format!("no report for {} s, or it cannot tell", settings.heartbeat_timeout_s))
                    ),
                ),
                DeviceState::OptedOut => (
                    AuditKind::OptedOut,
                    format!("{name} ({user}) is not participating: {}", opt_out.unwrap_or(OptOut::User).label()),
                ),
            };
            let kind = if previous == Some(DeviceState::OptedOut) && current != DeviceState::OptedOut {
                AuditKind::OptedIn
            } else {
                kind
            };
            audit_texts.push(Tracker::record(&mut inner, kind, text.clone()));
            if !settings.enabled {
                continue;
            }
            match current {
                DeviceState::InRange => {
                    let loud = pressurized.is_some();
                    let tail = match pressurized {
                        Some(phase) => format!(
                            " while the vehicle may be pressurized ({phase:?}). New arming and pressurizing commands are refused; nothing is actuated automatically, the operation in progress continues under operator control."
                        ),
                        None => ". New arming and pressurizing commands are refused.".into(),
                    };
                    notices.push((loud, format!("Proximity lock: {text}{tail}")));
                }
                DeviceState::Unknown if previous.is_some() => notices.push((
                    pressurized.is_some() && settings.stale_policy == StalePolicy::Block,
                    format!("Proximity lock: {text}. Unknown devices {}.", match settings.stale_policy {
                        StalePolicy::Warn => "only warn",
                        StalePolicy::Block => "block",
                    }),
                )),
                _ => {}
            }
        }
        let status = status_of(&inner, now, pad);
        let changed = std::mem::replace(&mut inner.was_active, status.active) != status.active;
        (status, changed)
    };
    for event in &audit_texts {
        log::info!("{}", event.text);
        crate::web::persist_message_db_only(state, event.text.clone());
    }
    for (loud, text) in notices {
        if loud {
            crate::web::emit_warning(state, text.clone());
        }
        crate::web::emit_notification_warning(state, text);
    }
    publish(state, status, active_changed);
}

fn publish(state: &AppState, status: Status, active_changed: bool) {
    let changed = state.proximity.tx.send_if_modified(|current| {
        if same_shape(current, &status) {
            return false;
        }
        *current = status.clone();
        true
    });
    if changed {
        let _ = state
            .db_queue_tx
            .try_send(crate::telemetry_db::DbQueueItem::Write(status_row(&status)));
    }
    if changed || active_changed {
        state.broadcast_action_policy_snapshot();
    }
}

/// The recorded row: the lock state and the counts, no names.
pub(crate) fn status_row(status: &Status) -> crate::telemetry_db::DbWrite {
    crate::telemetry_db::DbWrite::Telemetry {
        timestamp_ms: now_ms(),
        source_timestamp_ms: None,
        data_type: "GS_PROXIMITY_LOCK".into(),
        sender_id: "GS".into(),
        values_json: None,
        payload_json: json!({
            "enabled": status.settings.enabled,
            "active": status.active,
            "condition": status.condition,
            "overridden": status.override_view.is_some(),
            "radius_m": status.settings.radius_m,
            "stale_policy": status.settings.stale_policy,
            "reporting": status.counts.reporting,
            "in_range": status.counts.in_range,
            "unknown": status.counts.unknown,
            "opted_out": status.counts.opted_out,
        })
        .to_string(),
    }
}

fn refusal_body(status: StatusCode, text: impl Into<String>) -> (StatusCode, String) {
    (status, text.into())
}

fn error_response((status, text): (StatusCode, String)) -> Response {
    (status, Json(json!({"error": text}))).into_response()
}

/// Accepts one device report. `now` is the station's receive time.
pub(crate) fn accept_report(
    state: &AppState,
    principal: &AuthPrincipal,
    body: &[u8],
    now: Instant,
) -> Result<Status, (StatusCode, String)> {
    let username = principal
        .username
        .clone()
        .filter(|u| !principal.anonymous && !u.is_empty())
        .ok_or_else(|| refusal_body(StatusCode::FORBIDDEN, "Sign in to report proximity."))?;
    let report: Report = serde_json::from_slice(body).map_err(|e| {
        refusal_body(
            StatusCode::BAD_REQUEST,
            format!("Invalid proximity report ({e}). Only the in-range flag is accepted: never coordinates or distances."),
        )
    })?;
    if report.v != 1 {
        return Err(refusal_body(StatusCode::BAD_REQUEST, "Unsupported proximity report version."));
    }
    if !valid_device_id(&report.device_id) {
        return Err(refusal_body(StatusCode::BAD_REQUEST, "device_id must be 1–64 letters, digits, '-' or '_'."));
    }
    let name = clean_name(&report.device_name).map_err(|e| refusal_body(StatusCode::BAD_REQUEST, e))?;
    let key = format!("{}:{}", username.to_lowercase(), report.device_id);
    {
        let mut inner = state.proximity.inner.lock().unwrap();
        if !inner.settings.enabled {
            // Nothing is kept while the station has the lock off.
            inner.devices.clear();
        } else {
            if !inner.devices.contains_key(&key) {
                let lower = username.to_lowercase();
                let mine = inner.devices.values().filter(|d| d.username.to_lowercase() == lower).count();
                if mine >= MAX_DEVICES_PER_USER || inner.devices.len() >= MAX_DEVICES {
                    return Err(refusal_body(StatusCode::TOO_MANY_REQUESTS, "Too many proximity devices are registered."));
                }
            }
            let device = inner.devices.entry(key).or_insert_with(|| Device {
                username: username.clone(),
                name: name.clone(),
                platform: report.platform,
                participating: false,
                in_range: None,
                accuracy: None,
                reference: None,
                background: false,
                opt_out: None,
                cannot_tell: None,
                last_seen: now,
                last_seen_ms: now_ms(),
                recent: VecDeque::new(),
                announced: None,
            });
            while device.recent.front().is_some_and(|t| now.saturating_duration_since(*t) > RATE_WINDOW) {
                device.recent.pop_front();
            }
            if device.recent.len() >= RATE_MAX {
                return Err(refusal_body(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("At most {RATE_MAX} proximity reports per {} s per device.", RATE_WINDOW.as_secs()),
                ));
            }
            device.recent.push_back(now);
            device.name = name;
            device.username = username;
            device.platform = report.platform;
            device.participating = report.participating;
            device.in_range = report.in_range.filter(|_| report.participating);
            device.accuracy = report.accuracy.filter(|_| report.participating);
            device.reference = report.reference.filter(|_| report.participating);
            device.background = report.background;
            device.opt_out = if report.participating { None } else { Some(report.opt_out.unwrap_or(OptOut::User)) };
            device.cannot_tell = report.cannot_tell.filter(|_| report.participating && report.in_range.is_none());
            device.last_seen = now;
            device.last_seen_ms = now_ms();
        }
    }
    refresh_at(state, now);
    Ok(state.proximity.status_at(now, configured_pad(state)))
}

pub(crate) fn update_settings(
    state: &AppState,
    principal: &AuthPrincipal,
    body: &[u8],
) -> Result<Status, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err(refusal_body(StatusCode::FORBIDDEN, "Only an administrator may change the proximity lock."));
    }
    let update: SettingsUpdate = serde_json::from_slice(body)
        .map_err(|e| refusal_body(StatusCode::BAD_REQUEST, format!("Invalid proximity lock settings: {e}")))?;
    let actor = principal.username.clone().unwrap_or_default();
    {
        let mut inner = state.proximity.inner.lock().unwrap();
        let current = inner.settings;
        let next = update.apply(current);
        next.validate().map_err(|e| refusal_body(StatusCode::BAD_REQUEST, e))?;
        let changes = current.diff(&next);
        if changes.is_empty() {
            drop(inner);
            return Ok(status(state));
        }
        inner.settings = next;
        if let Err(error) = save(&inner) {
            inner.settings = current;
            return Err(refusal_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Proximity lock settings were not saved: {error}"),
            ));
        }
        if !next.enabled {
            inner.devices.clear();
            inner.override_ = None;
        }
        let text = format!("Proximity lock settings changed by {actor}: {}.", changes.join(", "));
        let event = Tracker::record(&mut inner, AuditKind::Settings, text);
        drop(inner);
        log::info!("{}", event.text);
        crate::web::persist_message_db_only(state, event.text.clone());
        crate::web::emit_notification_warning(state, event.text);
    }
    refresh(state);
    state.broadcast_action_policy_snapshot();
    Ok(status(state))
}

pub(crate) fn start_override(
    state: &AppState,
    principal: &AuthPrincipal,
    body: &[u8],
    now: Instant,
) -> Result<Status, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err(refusal_body(StatusCode::FORBIDDEN, "Only an administrator may override the proximity lock."));
    }
    let request: OverrideRequest = serde_json::from_slice(body).map_err(|_| {
        refusal_body(StatusCode::BAD_REQUEST, "Expected {minutes: 1–30, reason: text, confirm: true}.")
    })?;
    if !request.confirm {
        return Err(refusal_body(StatusCode::BAD_REQUEST, "Confirm the override."));
    }
    if !OVERRIDE_RANGE_MIN.contains(&request.minutes) {
        return Err(refusal_body(StatusCode::BAD_REQUEST, "An override lasts 1–30 minutes."));
    }
    let reason = request.reason.trim().to_owned();
    if reason.chars().count() < 3 || reason.chars().count() > 200 || reason.chars().any(char::is_control) {
        return Err(refusal_body(StatusCode::BAD_REQUEST, "Give a reason (3–200 characters)."));
    }
    let actor = principal.username.clone().unwrap_or_default();
    let text = {
        let mut inner = state.proximity.inner.lock().unwrap();
        if !inner.settings.enabled {
            return Err(refusal_body(StatusCode::CONFLICT, "The proximity lock is off."));
        }
        let duration = Duration::from_secs(u64::from(request.minutes) * 60);
        inner.override_ = Some(Override {
            by: actor.clone(),
            reason: reason.clone(),
            until: now + duration,
            until_ms: now_ms() + duration.as_millis() as i64,
        });
        let text = format!(
            "Proximity lock OVERRIDDEN by {actor} for {} min: {reason}. Arming and pressurizing are allowed with people in range until it expires or is ended.",
            request.minutes
        );
        Tracker::record(&mut inner, AuditKind::Override, text.clone());
        text
    };
    log::warn!("{text}");
    crate::web::persist_message_db_only(state, text.clone());
    crate::web::emit_warning(state, text.clone());
    crate::web::emit_notification_warning(state, text);
    refresh_at(state, now);
    Ok(state.proximity.status_at(now, configured_pad(state)))
}

/// Ending the override early restores the lock: anyone who may send
/// commands may do it.
pub(crate) fn end_override(state: &AppState, principal: &AuthPrincipal) -> Result<Status, (StatusCode, String)> {
    if principal.anonymous || !principal.permissions.send_commands {
        return Err(refusal_body(StatusCode::FORBIDDEN, "Command permission is required to end the override."));
    }
    let actor = principal.username.clone().unwrap_or_default();
    let ended = {
        let mut inner = state.proximity.inner.lock().unwrap();
        inner.override_.take().map(|o| {
            let text = format!("Proximity lock override by {} ended by {actor}.", o.by);
            Tracker::record(&mut inner, AuditKind::OverrideEnded, text.clone());
            text
        })
    };
    if let Some(text) = ended {
        log::info!("{text}");
        crate::web::persist_message_db_only(state, text.clone());
        crate::web::emit_notification_warning(state, text);
    }
    refresh(state);
    Ok(status(state))
}

/// An administrator removes a device (an unknown phone that left).
pub(crate) fn dismiss(state: &AppState, principal: &AuthPrincipal, body: &[u8]) -> Result<Status, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err(refusal_body(StatusCode::FORBIDDEN, "Only an administrator may remove a device."));
    }
    let request: DismissRequest =
        serde_json::from_slice(body).map_err(|_| refusal_body(StatusCode::BAD_REQUEST, "Expected {id}."))?;
    let actor = principal.username.clone().unwrap_or_default();
    let text = {
        let mut inner = state.proximity.inner.lock().unwrap();
        let key = inner.devices.keys().find(|key| public_id(key) == request.id).cloned();
        let Some(key) = key else {
            return Err(refusal_body(StatusCode::NOT_FOUND, "No such device."));
        };
        let device = inner.devices.remove(&key).unwrap();
        let text = format!("{} ({}) removed from the proximity lock by {actor}.", device.name, device.username);
        Tracker::record(&mut inner, AuditKind::Dismissed, text.clone());
        text
    };
    log::info!("{text}");
    crate::web::persist_message_db_only(state, text.clone());
    crate::web::emit_notification_warning(state, text);
    refresh(state);
    Ok(status(state))
}

// ---------------------------------------------------------------------------
// Persistence

fn read(dir: &Path) -> Settings {
    let path = dir.join(FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
            .ok()
            .filter(|s| s.validate().is_ok())
            .unwrap_or_else(|| {
                log::warn!("Ignoring unreadable {}; the proximity lock is off.", path.display());
                Settings::default()
            }),
        Err(_) => Settings::default(),
    }
}

fn save(inner: &Inner) -> std::io::Result<()> {
    use std::io::Write;
    let Some(dir) = &inner.data_dir else { return Ok(()) };
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE);
    let tmp = path.with_extension("pending");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(&inner.settings).unwrap_or_default())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)
}

pub(crate) fn load(state: &AppState, dir: PathBuf) {
    let settings = read(&dir);
    let mut inner = state.proximity.inner.lock().unwrap();
    inner.settings = settings;
    inner.data_dir = Some(dir);
}

/// Re-evaluates every second: staleness, override expiry, publication.
pub(crate) fn spawn_tick(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut shutdown = state.shutdown_subscribe();
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => refresh(&state),
                _ = shutdown.recv() => break,
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Routes

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/proximity", get(get_status))
        .route("/api/proximity/audit", get(get_audit))
        .route("/api/proximity/settings", put(put_settings))
        .route("/api/proximity/report", post(post_report))
        .route("/api/proximity/override", post(post_override))
        .route("/api/proximity/override/end", post(post_end_override))
        .route("/api/proximity/dismiss", post(post_dismiss))
}

fn reply(result: Result<Status, (StatusCode, String)>) -> Response {
    match result {
        Ok(status) => ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(status)).into_response(),
        Err(error) => error_response(error),
    }
}

async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    reply(Ok(status(&state)))
}

async fn get_audit(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(state.proximity.audit())).into_response()
}

async fn put_settings(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    reply(update_settings(&state, &principal, &body))
}

async fn post_report(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    reply(accept_report(&state, &principal, &body, Instant::now()))
}

async fn post_override(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    reply(start_override(&state, &principal, &body, Instant::now()))
}

async fn post_end_override(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    reply(end_override(&state, &principal))
}

async fn post_dismiss(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    reply(dismiss(&state, &principal, &body))
}
