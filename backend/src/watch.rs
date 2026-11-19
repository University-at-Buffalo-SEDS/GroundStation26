//! Watch companion direct link (Seds-Ground-Station-Frontend
//! docs/watch-companion.md). While the phone app is suspended the watch reads
//! a compact status here and sends its opted-in page actions with a scoped
//! watch token issued to the signed-in phone:
//!
//! * `GET  /api/watch/status`  compact, read-only status for the watch (the
//!   same item/target content the phone publishes to it);
//! * `GET  /api/watch/actions` the commands the signed-in account may put on
//!   watch pages (watch-eligible and permitted);
//! * `POST /api/auth/watch-token` and `/api/auth/watch-token/revoke` issue and
//!   revoke the watch token (phone session only).
//!
//! Commands from a watch use the ordinary `POST /api/command` with the watch
//! token, so they pass exactly the same authorization, action policy and
//! admission as the phone. Besides those, a watch token may only read map
//! tiles (`GET /tiles/{z}/{x}/{y}.jpg`, the compass map background) and log
//! itself out; it reaches no other endpoint.
use crate::auth::{AuthFailure, Permission, WatchTokenGrant};
use crate::state::AppState;
use crate::web::{authorize_headers, bearer_token_from_headers};
use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const STATUS_PATH: &str = "/api/watch/status";
pub const ACTIONS_PATH: &str = "/api/watch/actions";
const PROTOCOL_VERSION: u32 = 1;
const LINK_TIMEOUT_S: u64 = 15;
/// Action buttons one watch page may carry.
pub const MAX_ACTIONS_PER_PAGE: usize = 2;

/// Launch- and pyro-class commands that are nevertheless offered on the
/// watch. Empty: nothing that can fire a pyro channel or start a launch may
/// be put on a watch page. Adding a name here is the explicit "watch-safe"
/// mark; the action policy and admission still apply to every press.
pub const WATCH_SAFE_OVERRIDES: &[&str] = &[];

/// Why a command is never offered on the watch, or None when it may be.
pub fn excluded_reason(cmd: &str) -> Option<&'static str> {
    use crate::sequences::{ControlSection, control_section};
    match cmd {
        "Launch" | "GroundStationLaunch" | "ResetLaunchLatch" | "TogglePhysicalLaunchMode"
        | "ToggleLaunchInterlock" => {
            return Some("Launch-class actions are not offered on the watch.");
        }
        "Igniter" | "IgniterSequence" | "DeployParachute" | "ExpandParachute" => {
            return Some("Pyro actions are not offered on the watch.");
        }
        // An HTTP action with its own confirmation flow, not /api/command.
        "RecoverStation" => return Some("Recover station needs the full app."),
        _ => {}
    }
    match control_section(cmd) {
        None => Some("Not an Actions-tab command."),
        Some(ControlSection::Testing) => Some("HITL bench controls stay in the full app."),
        Some(_) => None,
    }
}

/// May this command be put on a watch page and sent with a watch token?
pub fn command_watch_eligible(cmd: &str) -> bool {
    WATCH_SAFE_OVERRIDES.contains(&cmd) || excluded_reason(cmd).is_none()
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(STATUS_PATH, get(get_status))
        .route(ACTIONS_PATH, get(get_actions))
        .route("/api/auth/watch-token", post(issue_token))
        .route("/api/auth/watch-token/revoke", post(revoke_tokens))
}

/// Endpoints a watch token may reach. Everything else answers 403, whatever
/// permission the endpoint itself asks for (including the WebSocket, which
/// takes its token from the query string).
fn watch_token_allowed(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", STATUS_PATH) | ("POST", "/api/command") | ("POST", "/api/auth/logout")
    ) || (method == Method::GET && is_tile_path(path))
}

/// `/tiles/{z}/{x}/{y}` or `/tiles/{z}/{x}/{y}.jpg` with decimal indices:
/// the read-only map tiles the watch compass draws under its arrow.
fn is_tile_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/tiles/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let digits = |s: &str| (1..=9).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    parts.len() == 3
        && digits(parts[0])
        && digits(parts[1])
        && digits(parts[2].strip_suffix(".jpg").unwrap_or(parts[2]))
}

fn query_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "token")
        .map(|(_, value)| value.replace("%2E", ".").replace("%2e", "."))
}

/// Router layer: confines watch tokens to [`watch_token_allowed`].
pub async fn scope_watch_tokens(request: Request, next: Next) -> Response {
    let header_token = bearer_token_from_headers(request.headers())
        .is_some_and(crate::auth::is_watch_token);
    let query_watch = query_token(request.uri().query())
        .as_deref()
        .is_some_and(crate::auth::is_watch_token);
    if (header_token || query_watch)
        && (query_watch || !watch_token_allowed(request.method(), request.uri().path()))
    {
        return (
            StatusCode::FORBIDDEN,
            "A watch token can only read the watch status and map tiles and send its page actions.",
        )
            .into_response();
    }
    next.run(request).await
}

fn auth_failure(err: AuthFailure) -> Response {
    match err {
        AuthFailure::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg).into_response(),
        AuthFailure::Forbidden(msg) => (StatusCode::FORBIDDEN, msg).into_response(),
        AuthFailure::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Tokens

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueRequest {
    device_id: String,
    #[serde(default)]
    commands: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RevokeRequest {
    #[serde(default)]
    device_id: Option<String>,
}

#[derive(Serialize)]
struct Revoked {
    revoked: u64,
}

/// A signed-in phone session (never a watch token) issues the watch token.
async fn issue_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<IssueRequest>,
) -> Response {
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(parent) = bearer_token_from_headers(&headers) else {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    };
    match state
        .auth
        .issue_watch_token(
            &state.auth_db,
            parent,
            &principal,
            &request.device_id,
            &request.commands,
            command_watch_eligible,
        )
        .await
    {
        Ok(grant) => {
            log::info!(
                "Watch token issued to {} for device {} ({} commands)",
                principal.username.as_deref().unwrap_or("?"),
                grant.device_id,
                grant.commands.len()
            );
            Json::<WatchTokenGrant>(grant).into_response()
        }
        Err(err) => auth_failure(err),
    }
}

async fn revoke_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Option<Json<RevokeRequest>>,
) -> Response {
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(username) = principal.username.as_deref().filter(|_| !principal.anonymous) else {
        return (StatusCode::FORBIDDEN, "sign in to manage watch tokens").into_response();
    };
    let request = body.map(|Json(r)| r).unwrap_or_default();
    match state
        .auth
        .revoke_watch_tokens(&state.auth_db, username, request.device_id.as_deref())
        .await
    {
        Ok(revoked) => Json(Revoked { revoked }).into_response(),
        Err(err) => auth_failure(err),
    }
}

// ---------------------------------------------------------------------------
// Action catalog

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CatalogAction {
    pub cmd: String,
    pub label: String,
    pub section: Option<crate::sequences::ControlSection>,
    /// Never needs the hold-to-confirm gesture; always admitted.
    pub abort: bool,
    /// Explicitly marked watch-safe in [`WATCH_SAFE_OVERRIDES`] although its
    /// class (Launch, pyro, bench) is normally excluded. The phone refuses a
    /// Launch/pyro action without this mark.
    pub watch_safe: bool,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExcludedAction {
    pub cmd: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ActionCatalog {
    pub actions: Vec<CatalogAction>,
    pub excluded: Vec<ExcludedAction>,
    pub max_per_page: usize,
}

fn label_for(cmd: &str, labels: &BTreeMap<String, String>) -> String {
    labels.get(cmd).cloned().unwrap_or_else(|| cmd.to_string())
}

/// The commands of this station mode that may go on a watch page for an
/// account allowed to send `allowed`, with the current policy state.
pub fn action_catalog(
    commands: &[&str],
    allowed: impl Fn(&str) -> bool,
    policy: &crate::sequences::ActionPolicyMsg,
    labels: &BTreeMap<String, String>,
) -> ActionCatalog {
    let mut actions = Vec::new();
    let mut excluded = Vec::new();
    for &cmd in commands {
        if !command_watch_eligible(cmd) {
            if let Some(reason) = excluded_reason(cmd) {
                excluded.push(ExcludedAction {
                    cmd: cmd.to_string(),
                    reason,
                });
            }
            continue;
        }
        if !allowed(cmd) {
            continue;
        }
        let state = action_state(cmd, policy);
        actions.push(CatalogAction {
            cmd: cmd.to_string(),
            label: label_for(cmd, labels),
            section: crate::sequences::control_section(cmd),
            abort: cmd == "Abort",
            watch_safe: WATCH_SAFE_OVERRIDES.contains(&cmd),
            enabled: state.enabled,
            reason: state.reason,
        });
    }
    ActionCatalog {
        actions,
        excluded,
        max_per_page: MAX_ACTIONS_PER_PAGE,
    }
}

async fn get_actions(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    crate::sequences::refresh_action_policy_now(&state);
    let policy = state.action_policy_snapshot();
    let labels: BTreeMap<String, String> = crate::layout::load_layout()
        .map(|layout| {
            layout
                .actions_tab
                .actions
                .into_iter()
                .map(|a| (a.cmd, a.label))
                .collect()
        })
        .unwrap_or_default();
    let commands = crate::sequences::all_command_names();
    Json(action_catalog(
        &commands,
        |cmd| principal.allows_command_name(cmd),
        &policy,
        &labels,
    ))
    .into_response()
}

// ---------------------------------------------------------------------------
// Status

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ActionState {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The GS's own enable state for one control. Abort is always pressable:
/// the GS admits a manual Abort under every policy.
pub fn action_state(cmd: &str, policy: &crate::sequences::ActionPolicyMsg) -> ActionState {
    if cmd == "Abort" {
        return ActionState {
            enabled: true,
            reason: None,
        };
    }
    let control = policy.controls.iter().find(|c| c.cmd == cmd);
    let enabled = policy.software_buttons_enabled && control.is_some_and(|c| c.enabled);
    let reason = if enabled {
        None
    } else {
        Some(
            control
                .and_then(|c| c.disabled_reason.clone())
                .unwrap_or_else(|| {
                    if control.is_none() {
                        "Not available in this station mode.".to_string()
                    } else {
                        "Software controls are disabled by GroundStation.".to_string()
                    }
                }),
        )
    };
    ActionState { enabled, reason }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Quantity {
    Length,
    Speed,
    Pressure(&'static str),
    Mass,
    Percent,
    Voltage,
    Count,
}

impl Quantity {
    fn display(self, value: f64, metric: bool) -> (f64, &'static str) {
        match self {
            Quantity::Length if metric => (value, "m"),
            Quantity::Length => (value / 0.3048, "ft"),
            Quantity::Speed if metric => (value, "m/s"),
            Quantity::Speed => (value / 0.3048, "ft/s"),
            Quantity::Pressure(unit) => (value, unit),
            Quantity::Mass => (value, "kg"),
            Quantity::Percent => (value, "%"),
            Quantity::Voltage => (value, "V"),
            Quantity::Count => (value, ""),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Source {
    FlightState,
    LaunchClock,
    StationLink,
    Boards,
    RocketFixAge,
    Telemetry {
        channels: &'static [(&'static str, Option<&'static str>, usize)],
        quantity: Quantity,
        precision: usize,
        stale_after_ms: i64,
    },
}

struct ItemDef {
    id: &'static str,
    label: &'static str,
    source: Source,
}

const FAST: i64 = 10_000;

/// Mirror of the phone registry
/// (Seds-Ground-Station-Frontend src/telemetry_dashboard/watch_companion/registry.rs):
/// same ids, labels, channels, units, precision and staleness, so a page
/// renders the same whether the watch reads the phone or this endpoint.
const ITEMS: &[ItemDef] = &[
    ItemDef {
        id: "flight_state",
        label: "State",
        source: Source::FlightState,
    },
    ItemDef {
        id: "t_clock",
        label: "T-clock",
        source: Source::LaunchClock,
    },
    ItemDef {
        id: "altitude",
        label: "Altitude",
        source: Source::Telemetry {
            channels: &[
                ("ASCENT_STATE", None, 4),
                ("DESCENT_STATE", None, 2),
                ("BAROMETER_DATA", None, 2),
            ],
            quantity: Quantity::Length,
            precision: 0,
            stale_after_ms: FAST,
        },
    },
    ItemDef {
        id: "velocity",
        label: "Velocity",
        source: Source::Telemetry {
            channels: &[("ASCENT_STATE", None, 5), ("DESCENT_STATE", None, 3)],
            quantity: Quantity::Speed,
            precision: 1,
            stale_after_ms: FAST,
        },
    },
    ItemDef {
        id: "gps_altitude",
        label: "GPS altitude",
        source: Source::Telemetry {
            channels: &[
                ("GPS_DATA", None, 2),
                ("GPS", None, 2),
                ("ROCKET_GPS", None, 2),
            ],
            quantity: Quantity::Length,
            precision: 0,
            stale_after_ms: 30_000,
        },
    },
    ItemDef {
        id: "gps_sats",
        label: "GPS satellites",
        source: Source::Telemetry {
            channels: &[("GPS_SATELLITE_NUMBER", None, 0)],
            quantity: Quantity::Count,
            precision: 0,
            stale_after_ms: 30_000,
        },
    },
    ItemDef {
        id: "rocket_fix_age",
        label: "Rocket fix age",
        source: Source::RocketFixAge,
    },
    ItemDef {
        id: "tank_pressure",
        label: "Tank pressure",
        source: Source::Telemetry {
            channels: &[("FUEL_TANK_PRESSURE", None, 0)],
            quantity: Quantity::Pressure("psi"),
            precision: 0,
            stale_after_ms: FAST,
        },
    },
    ItemDef {
        id: "chamber_pressure",
        label: "Chamber pressure",
        source: Source::Telemetry {
            channels: &[("COMBUSTION_CHAMBER_PRESSURE", None, 0)],
            quantity: Quantity::Pressure("psig"),
            precision: 0,
            stale_after_ms: FAST,
        },
    },
    ItemDef {
        id: "loadcell_mass",
        label: "Load cell mass",
        source: Source::Telemetry {
            channels: &[("LOADCELL_WEIGHT_KG", None, 0)],
            quantity: Quantity::Mass,
            precision: 1,
            stale_after_ms: FAST,
        },
    },
    ItemDef {
        id: "fill_percent",
        label: "Fill level",
        source: Source::Telemetry {
            channels: &[("LOADCELL_FILL_PERCENT", None, 0)],
            quantity: Quantity::Percent,
            precision: 0,
            stale_after_ms: FAST,
        },
    },
    ItemDef {
        id: "station_link",
        label: "Station link",
        source: Source::StationLink,
    },
    ItemDef {
        id: "boards",
        label: "Boards seen",
        source: Source::Boards,
    },
    ItemDef {
        id: "av_battery",
        label: "AV battery",
        source: Source::Telemetry {
            channels: &[("BATTERY_VOLTAGE", Some("PB"), 0)],
            quantity: Quantity::Voltage,
            precision: 2,
            stale_after_ms: 30_000,
        },
    },
];

/// Compass targets in the phone's order.
const TARGETS: &[(&str, &str)] = &[
    ("rocket", "Rocket"),
    ("ground_station", "Ground station"),
    ("rrls", "RRLS"),
];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Station {
    pub name: &'static str,
    pub role: &'static str,
    pub link: &'static str,
    pub address: String,
    pub telemetry_age_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Clock {
    pub kind: &'static str,
    pub ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ItemValue {
    pub label: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    pub unit: &'static str,
    pub age_ms: Option<i64>,
    pub stale: bool,
    pub level: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Target {
    pub id: &'static str,
    pub label: String,
    pub lat: f64,
    pub lon: f64,
    pub alt_m: Option<f64>,
    pub age_ms: i64,
    pub stale: bool,
    pub source: String,
    pub fallback: bool,
}

/// Same keys as the phone's snapshot; the watch takes pages, settings and
/// the catalog from the phone's last snapshot.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    pub v: u32,
    /// Server time (ms) of this status; the watch orders direct statuses by it.
    pub seq: i64,
    pub mode: &'static str,
    pub source: &'static str,
    pub station: Station,
    pub items: BTreeMap<&'static str, ItemValue>,
    pub targets: Vec<Target>,
    pub actions: BTreeMap<String, ActionState>,
    pub link_timeout_s: u64,
}

/// One antenna fix as the builder needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    pub alt_m: Option<f64>,
    pub age_ms: i64,
    pub stale: bool,
    pub manual: bool,
    pub source: String,
}

/// Everything the status builder reads, gathered from AppState so the
/// builder itself is pure and tested.
pub struct Inputs<'a> {
    pub now_ms: i64,
    pub rrls: bool,
    pub flight_state: String,
    pub clock: Option<Clock>,
    pub boards: Option<(usize, usize)>,
    /// Newest `(value, timestamp_ms)` for `(data_type, sender, index)`.
    pub telemetry: &'a dyn Fn(&str, Option<&str>, usize) -> Option<(f64, i64)>,
    pub fixes: [Option<Fix>; 3],
    pub metric: bool,
    pub stale_after_s: i64,
    pub actions: BTreeMap<String, ActionState>,
}

pub fn format_age(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn fmt_number(value: f64, precision: usize) -> String {
    let text = format!("{value:.precision$}");
    if text
        .trim_start_matches('-')
        .chars()
        .all(|c| c == '0' || c == '.')
    {
        text.trim_start_matches('-').to_string()
    } else {
        text
    }
}

fn clock_text(clock: &Clock) -> String {
    let s = clock.ms.max(0) / 1000;
    match clock.kind {
        "tminus" => format!("T- {:02}:{:02}", s / 60, s % 60),
        "tplus" => format!("T+ {:02}:{:02}", s / 60, s % 60),
        _ => "Idle".into(),
    }
}

fn unknown(label: &'static str) -> ItemValue {
    ItemValue {
        label,
        text: "--".into(),
        value: None,
        unit: "",
        age_ms: None,
        stale: false,
        level: "unknown",
        clock: None,
    }
}

fn valid_lat_lon(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)
}

fn targets(input: &Inputs<'_>) -> Vec<Target> {
    let stale_after_ms = input.stale_after_s.max(1) * 1000;
    TARGETS
        .iter()
        .zip(input.fixes.iter())
        .filter_map(|((id, label), fix)| {
            let fix = fix.as_ref()?;
            if !valid_lat_lon(fix.lat, fix.lon) {
                return None;
            }
            let age_ms = if fix.manual { 0 } else { fix.age_ms.max(0) };
            Some(Target {
                id,
                label: if fix.manual {
                    format!("{label} (saved)")
                } else {
                    label.to_string()
                },
                lat: fix.lat,
                lon: fix.lon,
                alt_m: fix.alt_m.filter(|a| a.is_finite()),
                age_ms,
                stale: !fix.manual && (fix.stale || age_ms > stale_after_ms),
                source: if fix.source.is_empty() {
                    label.to_string()
                } else {
                    fix.source.clone()
                },
                fallback: fix.manual,
            })
        })
        .collect()
}

fn newest_telemetry_ms(input: &Inputs<'_>) -> Option<i64> {
    ITEMS
        .iter()
        .filter_map(|item| match item.source {
            Source::Telemetry { channels, .. } => Some(channels),
            _ => None,
        })
        .flatten()
        .filter_map(|(dt, sender, index)| (input.telemetry)(dt, *sender, *index).map(|(_, t)| t))
        .max()
}

fn item_value(def: &ItemDef, input: &Inputs<'_>, telemetry_age: Option<i64>, rocket: Option<&Target>) -> ItemValue {
    match def.source {
        Source::FlightState if input.flight_state.is_empty() => unknown(def.label),
        Source::FlightState => ItemValue {
            text: input.flight_state.clone(),
            level: "normal",
            age_ms: Some(0),
            ..unknown(def.label)
        },
        Source::LaunchClock => match &input.clock {
            Some(clock) => ItemValue {
                text: clock_text(clock),
                level: "normal",
                age_ms: Some(0),
                clock: Some(clock.clone()),
                ..unknown(def.label)
            },
            None => unknown(def.label),
        },
        // The watch reached the station itself, so the link is up.
        Source::StationLink => ItemValue {
            text: match telemetry_age {
                Some(age) => format!("OK · {}", format_age(age)),
                None => "OK".into(),
            },
            level: "normal",
            age_ms: Some(0),
            ..unknown(def.label)
        },
        Source::Boards => match input.boards {
            Some((seen, total)) if total > 0 => ItemValue {
                text: format!("{seen}/{total}"),
                value: Some(seen as f64),
                level: if seen == total { "normal" } else { "warn" },
                age_ms: Some(0),
                ..unknown(def.label)
            },
            _ => unknown(def.label),
        },
        Source::RocketFixAge => match rocket {
            Some(t) => ItemValue {
                text: format_age(t.age_ms),
                value: Some(t.age_ms as f64 / 1000.0),
                unit: "s",
                age_ms: Some(t.age_ms),
                stale: t.stale,
                level: if t.stale { "warn" } else { "normal" },
                ..unknown(def.label)
            },
            None => unknown(def.label),
        },
        Source::Telemetry {
            channels,
            quantity,
            precision,
            stale_after_ms,
        } => {
            let newest = channels
                .iter()
                .filter_map(|(dt, sender, index)| (input.telemetry)(dt, *sender, *index))
                .filter(|(v, _)| v.is_finite())
                .max_by_key(|(_, at)| *at);
            let Some((raw, at)) = newest else {
                return unknown(def.label);
            };
            let (value, unit) = quantity.display(raw, input.metric);
            let age = input.now_ms.saturating_sub(at).max(0);
            let stale = age > stale_after_ms;
            ItemValue {
                label: def.label,
                text: fmt_number(value, if quantity == Quantity::Count { 0 } else { precision }),
                value: Some(value),
                unit,
                age_ms: Some(age),
                stale,
                level: if stale { "warn" } else { "normal" },
                clock: None,
            }
        }
    }
}

pub fn build_status(input: &Inputs<'_>) -> Status {
    let targets = targets(input);
    let telemetry_age = newest_telemetry_ms(input).map(|t| input.now_ms.saturating_sub(t).max(0));
    let rocket = targets.iter().find(|t| t.id == "rocket");
    let items = ITEMS
        .iter()
        .map(|def| (def.id, item_value(def, input, telemetry_age, rocket)))
        .collect();
    Status {
        v: PROTOCOL_VERSION,
        seq: input.now_ms,
        mode: "live",
        source: "direct",
        station: Station {
            name: if input.rrls { "RRLS" } else { "GroundStation" },
            role: if input.rrls { "rrls" } else { "primary" },
            link: "connected",
            address: String::new(),
            telemetry_age_ms: telemetry_age,
        },
        items,
        targets,
        actions: input.actions.clone(),
        link_timeout_s: LINK_TIMEOUT_S,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StatusQuery {
    /// `metric` (default) or `imperial`.
    units: Option<String>,
    /// Comma-separated commands on the watch's pages.
    actions: Option<String>,
    /// Compass staleness threshold the watch uses (5–600 s).
    stale_after_s: Option<i64>,
}

fn launch_clock(msg: &crate::telemetry_db::LaunchClockMsg, now_ms: i64) -> Option<Clock> {
    use crate::telemetry_db::LaunchClockKind;
    Some(match msg.kind {
        LaunchClockKind::Idle => Clock { kind: "idle", ms: 0 },
        LaunchClockKind::TMinus => {
            let remaining = match (msg.anchor_timestamp_ms, msg.duration_ms) {
                (Some(anchor), Some(duration)) => duration.saturating_sub(now_ms.saturating_sub(anchor)),
                (Some(anchor), None) => anchor.saturating_sub(now_ms),
                (None, Some(duration)) => duration,
                (None, None) => return None,
            };
            Clock {
                kind: "tminus",
                ms: remaining.max(0),
            }
        }
        LaunchClockKind::TPlus => {
            let elapsed = match (msg.anchor_timestamp_ms, msg.duration_ms) {
                (None, Some(elapsed)) => elapsed.max(0),
                (Some(anchor), _) if anchor <= now_ms.saturating_add(1_000) => {
                    now_ms.saturating_sub(anchor).max(0)
                }
                _ => 0,
            };
            Clock {
                kind: "tplus",
                ms: elapsed,
            }
        }
    })
}

/// Newest value of each requested channel from the live telemetry cache
/// (newest rows last), in one reverse pass.
fn latest_values(
    rows: &std::collections::VecDeque<crate::types::TelemetryRow>,
) -> BTreeMap<(String, Option<String>, usize), (f64, i64)> {
    let wanted: Vec<(&str, Option<&str>, usize)> = ITEMS
        .iter()
        .filter_map(|item| match item.source {
            Source::Telemetry { channels, .. } => Some(channels),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let mut found = BTreeMap::new();
    for row in rows.iter().rev() {
        for (dt, sender, index) in &wanted {
            if row.data_type != *dt || sender.is_some_and(|s| s != row.sender_id) {
                continue;
            }
            let key = (dt.to_string(), sender.map(str::to_string), *index);
            if found.contains_key(&key) {
                continue;
            }
            if let Some(Some(value)) = row.values.get(*index) {
                found.insert(key, (*value as f64, row.timestamp_ms));
            }
        }
        if found.len() == wanted.len() {
            break;
        }
    }
    found
}

fn fix(f: &crate::antenna::Fix) -> Fix {
    Fix {
        lat: f.latitude,
        lon: f.longitude,
        alt_m: f.altitude_m,
        age_ms: f.age_ms as i64,
        stale: f.stale,
        manual: f.manual,
        source: f.source.clone(),
    }
}

async fn get_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<StatusQuery>,
) -> Response {
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let now_ms = crate::telemetry_task::get_current_timestamp_ms() as i64;
    let phase = crate::mission_phase::refresh(&state);
    let flight_state = match phase.detail.as_deref().filter(|d| !d.is_empty()) {
        Some(detail) => format!("{} · {detail}", phase.label),
        None => phase.label.clone(),
    };
    let clock = launch_clock(&state.launch_clock_presentation_snapshot(), now_ms);
    let boards = {
        let list = state.board_status_snapshot(now_ms.max(0) as u64).boards;
        (!list.is_empty()).then(|| (list.iter().filter(|b| b.seen).count(), list.len()))
    };
    let values = latest_values(&state.recent_telemetry_cache.lock().unwrap());
    let telemetry = move |dt: &str, sender: Option<&str>, index: usize| {
        values
            .get(&(dt.to_string(), sender.map(str::to_string), index))
            .copied()
    };
    let antenna = crate::antenna::snapshot();
    crate::sequences::refresh_action_policy_now(&state);
    let policy = state.action_policy_snapshot();
    // Only commands this token may send are reported.
    let actions = query
        .actions
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty() && command_watch_eligible(cmd) && principal.allows_command_name(cmd))
        .take(crate::auth::MAX_WATCH_COMMANDS)
        .map(|cmd| (cmd.to_string(), action_state(cmd, &policy)))
        .collect();
    let input = Inputs {
        now_ms,
        rrls: crate::rrls::is_rrls(),
        flight_state,
        clock,
        boards,
        telemetry: &telemetry,
        fixes: [
            antenna.rocket.as_ref().map(fix),
            antenna.ground_station.as_ref().map(fix),
            antenna.rrls.as_ref().map(fix),
        ],
        metric: query.units.as_deref() != Some("imperial"),
        stale_after_s: query.stale_after_s.unwrap_or(30).clamp(5, 600),
        actions,
    };
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(build_status(&input)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_export::tests::{TestDirectory, export_state};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn none(_: &str, _: Option<&str>, _: usize) -> Option<(f64, i64)> {
        None
    }

    fn base_input<'a>(telemetry: &'a dyn Fn(&str, Option<&str>, usize) -> Option<(f64, i64)>) -> Inputs<'a> {
        Inputs {
            now_ms: 100_000,
            rrls: false,
            flight_state: "In flight · Descent".into(),
            clock: Some(Clock { kind: "tplus", ms: 83_000 }),
            boards: Some((5, 6)),
            telemetry,
            fixes: [None, None, None],
            metric: true,
            stale_after_s: 30,
            actions: BTreeMap::new(),
        }
    }

    #[test]
    fn launch_and_pyro_are_excluded_and_abort_is_eligible() {
        for cmd in [
            "Launch",
            "GroundStationLaunch",
            "Igniter",
            "IgniterSequence",
            "DeployParachute",
            "ExpandParachute",
            "ResetLaunchLatch",
            "ToggleLaunchInterlock",
            "TogglePhysicalLaunchMode",
            "RecoverStation",
            "ToggleButtonInterlock",
            "NotACommand",
        ] {
            assert!(!command_watch_eligible(cmd), "{cmd} must not be watch-eligible");
            assert!(excluded_reason(cmd).is_some());
        }
        for cmd in ["Abort", "Dump", "Nitrogen", "StartWritingNow", "ToggleFlightBuzzer", "ResetTClock"] {
            assert!(command_watch_eligible(cmd), "{cmd} should be watch-eligible");
        }
        // Nothing is marked watch-safe by default.
        assert!(WATCH_SAFE_OVERRIDES.is_empty());
    }

    #[test]
    fn catalog_lists_only_permitted_eligible_commands_with_policy_state() {
        let mut policy = crate::sequences::default_action_policy();
        for control in &mut policy.controls {
            if control.cmd == "Dump" {
                control.enabled = false;
                control.disabled_reason = Some("Fill in progress.".into());
            }
        }
        let labels = BTreeMap::from([("Dump".to_string(), "Dump valve".to_string())]);
        let commands = ["Abort", "Dump", "Launch", "Nitrogen", "Igniter"];
        let catalog = action_catalog(&commands, |cmd| cmd != "Nitrogen", &policy, &labels);
        let cmds: Vec<_> = catalog.actions.iter().map(|a| a.cmd.as_str()).collect();
        assert_eq!(cmds, vec!["Abort", "Dump"]);
        let dump = &catalog.actions[1];
        assert_eq!(dump.label, "Dump valve");
        assert!(!dump.enabled);
        assert_eq!(dump.reason.as_deref(), Some("Fill in progress."));
        assert!(catalog.actions[0].abort && catalog.actions[0].enabled);
        let excluded: Vec<_> = catalog.excluded.iter().map(|a| a.cmd.as_str()).collect();
        assert_eq!(excluded, vec!["Launch", "Igniter"]);
        assert_eq!(catalog.max_per_page, 2);
    }

    #[test]
    fn abort_is_always_pressable_and_others_follow_the_policy() {
        let mut policy = crate::sequences::default_action_policy();
        policy.software_buttons_enabled = false;
        for control in &mut policy.controls {
            control.enabled = false;
        }
        assert_eq!(action_state("Abort", &policy), ActionState { enabled: true, reason: None });
        let dump = action_state("Dump", &policy);
        assert!(!dump.enabled && dump.reason.is_some());
        assert_eq!(
            action_state("NoSuchControl", &policy).reason.as_deref(),
            Some("Not available in this station mode.")
        );
    }

    /// Same values as the phone's snapshot builder for the same inputs
    /// (frontend snapshot.rs `direct_status_matches_gs_builder`).
    #[test]
    fn status_matches_phone_snapshot_content() {
        let telemetry = |dt: &str, _: Option<&str>, index: usize| match (dt, index) {
            ("DESCENT_STATE", 2) => Some((1234.4, 99_800)),
            ("DESCENT_STATE", 3) => Some((-6.15, 99_800)),
            ("FUEL_TANK_PRESSURE", 0) => Some((812.0, 50_000)),
            _ => None,
        };
        let mut input = base_input(&telemetry);
        input.fixes[0] = Some(Fix {
            lat: 32.99,
            lon: -106.97,
            alt_m: Some(1401.0),
            age_ms: 3_000,
            stale: false,
            manual: false,
            source: "Rocket GPS".into(),
        });
        input.fixes[1] = Some(Fix {
            lat: 32.98,
            lon: -106.98,
            alt_m: None,
            age_ms: 99_999,
            stale: false,
            manual: true,
            source: String::new(),
        });
        let status = build_status(&input);
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["mode"], "live");
        assert_eq!(value["source"], "direct");
        assert_eq!(value["station"]["link"], "connected");
        assert_eq!(value["station"]["telemetry_age_ms"], 200);
        assert_eq!(value["items"]["altitude"]["text"], "1234");
        assert_eq!(value["items"]["altitude"]["unit"], "m");
        assert_eq!(value["items"]["velocity"]["text"], "-6.2");
        assert_eq!(value["items"]["tank_pressure"]["text"], "812");
        assert_eq!(value["items"]["tank_pressure"]["stale"], true);
        assert_eq!(value["items"]["tank_pressure"]["level"], "warn");
        assert_eq!(value["items"]["t_clock"]["text"], "T+ 01:23");
        assert_eq!(value["items"]["flight_state"]["text"], "In flight · Descent");
        assert_eq!(value["items"]["boards"]["text"], "5/6");
        assert_eq!(value["items"]["boards"]["level"], "warn");
        assert_eq!(value["items"]["station_link"]["text"], "OK · 0s");
        assert_eq!(value["items"]["rocket_fix_age"]["text"], "3s");
        assert_eq!(value["items"]["gps_sats"]["text"], "--");
        assert_eq!(value["items"]["gps_sats"]["level"], "unknown");
        assert_eq!(value["targets"][0]["id"], "rocket");
        assert_eq!(value["targets"][1]["label"], "Ground station (saved)");
        assert_eq!(value["targets"][1]["age_ms"], 0);
        assert_eq!(value["targets"][1]["fallback"], true);
        assert_eq!(status.items.len(), ITEMS.len());
        // Imperial converts lengths and speeds only.
        let mut imperial = base_input(&telemetry);
        imperial.metric = false;
        let value = serde_json::to_value(build_status(&imperial)).unwrap();
        assert_eq!(value["items"]["altitude"]["text"], "4050");
        assert_eq!(value["items"]["altitude"]["unit"], "ft");
        assert_eq!(value["items"]["tank_pressure"]["unit"], "psi");
        // Small: well under the phone's 32 KiB message limit.
        assert!(serde_json::to_vec(&status).unwrap().len() < 8 * 1024);
        let empty = build_status(&base_input(&none));
        assert_eq!(empty.station.telemetry_age_ms, None);
        assert_eq!(empty.items["station_link"].text, "OK");
    }

    #[test]
    fn registry_mirror_matches_phone_ids() {
        // Keep in sync with the phone registry and watch/test-vectors.json
        // `direct_status.item_ids`.
        let ids: Vec<_> = ITEMS.iter().map(|i| i.id).collect();
        assert_eq!(
            ids,
            vec![
                "flight_state", "t_clock", "altitude", "velocity", "gps_altitude", "gps_sats",
                "rocket_fix_age", "tank_pressure", "chamber_pressure", "loadcell_mass",
                "fill_percent", "station_link", "boards", "av_battery"
            ]
        );
    }

    #[test]
    fn launch_clock_matches_phone_semantics() {
        use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
        let msg = |kind, anchor, duration| LaunchClockMsg {
            kind,
            anchor_timestamp_ms: anchor,
            duration_ms: duration,
        };
        assert_eq!(launch_clock(&msg(LaunchClockKind::Idle, None, None), 0), Some(Clock { kind: "idle", ms: 0 }));
        assert_eq!(
            launch_clock(&msg(LaunchClockKind::TMinus, Some(1_000), Some(10_000)), 4_000),
            Some(Clock { kind: "tminus", ms: 7_000 })
        );
        assert_eq!(
            launch_clock(&msg(LaunchClockKind::TMinus, Some(1_000), Some(10_000)), 40_000),
            Some(Clock { kind: "tminus", ms: 0 })
        );
        assert_eq!(launch_clock(&msg(LaunchClockKind::TMinus, None, None), 0), None);
        assert_eq!(
            launch_clock(&msg(LaunchClockKind::TPlus, Some(1_000), None), 5_000),
            Some(Clock { kind: "tplus", ms: 4_000 })
        );
        assert_eq!(
            launch_clock(&msg(LaunchClockKind::TPlus, None, Some(2_500)), 5_000),
            Some(Clock { kind: "tplus", ms: 2_500 })
        );
    }

    #[test]
    fn latest_values_pick_newest_row_per_channel() {
        let row = |ts, dt: &str, sender: &str, values: Vec<Option<f32>>| crate::types::TelemetryRow {
            timestamp_ms: ts,
            data_type: dt.into(),
            sender_id: sender.into(),
            values,
        };
        let rows: std::collections::VecDeque<_> = vec![
            row(1, "FUEL_TANK_PRESSURE", "VB", vec![Some(10.0)]),
            row(2, "BATTERY_VOLTAGE", "XX", vec![Some(9.0)]),
            row(3, "FUEL_TANK_PRESSURE", "VB", vec![Some(20.0)]),
            row(4, "BATTERY_VOLTAGE", "PB", vec![Some(15.5)]),
            row(5, "DESCENT_STATE", "FC", vec![None, None, Some(100.0), None]),
        ]
        .into();
        let found = latest_values(&rows);
        assert_eq!(found[&("FUEL_TANK_PRESSURE".into(), None, 0)], (20.0, 3));
        assert_eq!(found[&("BATTERY_VOLTAGE".into(), Some("PB".into()), 0)], (15.5, 4));
        assert_eq!(found[&("DESCENT_STATE".into(), None, 2)], (100.0, 5));
        assert!(!found.contains_key(&("DESCENT_STATE".into(), None, 3)));
    }

    // --- HTTP: tokens, scope, revocation, admission ---

    async fn fixture() -> (TestDirectory, Arc<AppState>, axum::Router) {
        let root = TestDirectory::new();
        let state = export_state(&root.0, false).await;
        std::fs::write(
            root.0.join("auth.json"),
            serde_json::json!({"users":[
                {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},
                 "permissions":{"view_data":true,"send_commands":true},
                 "command_access":{"allowed_commands":["Abort","Dump","Launch","Igniter","StartWritingNow"]}},
                {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},
                 "permissions":{"view_data":true}},
                {"username":"administrator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},
                 "permissions":{"view_data":true}}
            ]})
            .to_string(),
        )
        .unwrap();
        for (token, name) in [
            ("operator-phone", "operator"),
            ("operator-phone-2", "operator"),
            ("viewer-phone", "viewer"),
            ("admin-phone", "administrator"),
        ] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
                .bind(token).bind(name).execute(&state.auth_db).await.unwrap();
        }
        let app = Router::new()
            .route("/api/command", post(|| async { "reached" }))
            .route("/api/auth/logout", post(crate::web::logout))
            .route("/api/recent", get(|| async { "reached" }))
            .route("/api/action_policy", get(|| async { "reached" }))
            .route("/ws", get(|| async { "reached" }))
            .route("/tiles/{z}/{x}/{y}", get(|| async { "tile" }))
            .route("/tiles/{z}/{x}/{y}/extra", get(|| async { "reached" }))
            .merge(routes())
            .layer(axum::middleware::from_fn(scope_watch_tokens))
            .with_state(state.clone());
        (root, state, app)
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let mut request = HttpRequest::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn issue(app: &axum::Router, parent: &str, device: &str, commands: &[&str]) -> WatchTokenGrant {
        let (status, body) = call(
            app,
            "POST",
            "/api/auth/watch-token",
            Some(parent),
            Some(serde_json::json!({"device_id": device, "commands": commands})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        WatchTokenGrant {
            token: value["token"].as_str().unwrap().into(),
            device_id: value["device_id"].as_str().unwrap().into(),
            expires_at_ms: value["expires_at_ms"].as_i64().unwrap(),
            commands: serde_json::from_value(value["commands"].clone()).unwrap(),
        }
    }

    #[tokio::test]
    async fn token_scope_excludes_launch_pyro_and_unpermitted_commands() {
        let (_root, state, app) = fixture().await;
        let grant = issue(&app, "operator-phone", "phone-1", &["Abort", "Dump", "Launch", "Igniter", "Nitrogen", "Dump"]).await;
        assert!(grant.token.starts_with("gsw1."));
        // Launch/Igniter are never watch-eligible; Nitrogen is not in the
        // account's command list; duplicates collapse.
        assert_eq!(grant.commands, vec!["Abort", "Dump"]);
        // The table stores only a digest of the token.
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_sessions WHERE token = ?")
            .bind(&grant.token)
            .fetch_one(&state.auth_db)
            .await
            .unwrap();
        assert_eq!(stored, 0);
        let principal = state
            .auth
            .authorize_token(&state.auth_db, Some(&grant.token), Permission::SendCommands)
            .await
            .unwrap();
        assert!(principal.allows_command_name("Abort"));
        assert!(principal.allows_command_name("Dump"));
        assert!(!principal.allows_command_name("Launch"));
        assert!(!principal.allows_command_name("StartWritingNow"));
        assert!(principal.roles.is_empty());
        assert!(!principal.calibration_access.edit && !principal.permissions.sign_checklists);
        assert_eq!(principal.session_type.as_deref(), Some("watch"));
    }

    #[tokio::test]
    async fn admin_watch_token_has_no_admin_authority_or_launch() {
        let (_root, state, app) = fixture().await;
        let grant = issue(&app, "admin-phone", "phone-a", &["Launch", "Abort"]).await;
        assert_eq!(grant.commands, vec!["Abort"]);
        let principal = state
            .auth
            .authorize_token(&state.auth_db, Some(&grant.token), Permission::ViewData)
            .await
            .unwrap();
        assert!(!principal.is_admin());
        assert!(!principal.allows_command_name("Launch"));
        assert!(!principal.permissions.set_system_time);
    }

    #[tokio::test]
    async fn viewer_token_is_read_only() {
        let (_root, state, app) = fixture().await;
        let grant = issue(&app, "viewer-phone", "phone-v", &["Abort"]).await;
        assert!(grant.commands.is_empty());
        assert!(matches!(
            state
                .auth
                .authorize_token(&state.auth_db, Some(&grant.token), Permission::SendCommands)
                .await,
            Err(AuthFailure::Forbidden(_))
        ));
        let (status, _) = call(&app, "GET", STATUS_PATH, Some(&grant.token), None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn watch_tokens_reach_only_the_watch_endpoints() {
        let (_root, _state, app) = fixture().await;
        let grant = issue(&app, "operator-phone", "phone-1", &["Abort"]).await;
        let token = Some(grant.token.as_str());
        let (status, body) = call(&app, "GET", "/api/watch/status?units=imperial&actions=Abort,Launch,Dump", token, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["source"], "direct");
        // Only the token's own commands are reported.
        assert!(value["actions"]["Abort"]["enabled"].as_bool().unwrap());
        assert!(value["actions"].get("Launch").is_none());
        assert!(value["actions"].get("Dump").is_none());
        assert!(body.len() < 8 * 1024);
        assert_eq!(call(&app, "POST", "/api/command", token, Some(serde_json::json!("Abort"))).await.0, StatusCode::OK);
        for (method, uri) in [
            ("GET", "/api/recent"),
            ("GET", "/api/action_policy"),
            ("GET", ACTIONS_PATH),
            ("GET", "/api/command"),
        ] {
            assert_eq!(call(&app, method, uri, token, None).await.0, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        // A watch token can never mint or revoke tokens.
        assert_eq!(
            call(&app, "POST", "/api/auth/watch-token", token, Some(serde_json::json!({"device_id":"x"}))).await.0,
            StatusCode::FORBIDDEN
        );
        // WebSocket tokens travel in the query string.
        let ws = format!("/ws?token={}", grant.token);
        assert_eq!(call(&app, "GET", &ws, None, None).await.0, StatusCode::FORBIDDEN);
        // Map tiles are read-only and allowed (the compass map background);
        // nothing else under /tiles and no other method.
        for uri in ["/tiles/15/9204/12054.jpg", "/tiles/3/1/2"] {
            assert_eq!(call(&app, "GET", uri, token, None).await, (StatusCode::OK, "tile".into()), "{uri}");
        }
        for (method, uri) in [
            ("GET", "/tiles/15/9204/12054/extra"),
            ("GET", "/tiles/15/x/12054.jpg"),
            ("POST", "/tiles/15/9204/12054.jpg"),
            ("GET", "/tiles/15/9204/12054.jpg/../../api/recent"),
        ] {
            assert_eq!(call(&app, method, uri, token, None).await.0, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        let tile_ws = format!("/tiles/1/1/1?token={}", grant.token);
        assert_eq!(call(&app, "GET", &tile_ws, None, None).await.0, StatusCode::FORBIDDEN, "query tokens stay refused");
        // A phone session is not affected.
        assert_eq!(call(&app, "GET", "/api/recent", Some("operator-phone"), None).await.0, StatusCode::OK);
        assert_eq!(call(&app, "GET", ACTIONS_PATH, Some("operator-phone"), None).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn revocation_reissue_and_parent_sign_out() {
        let (_root, state, app) = fixture().await;
        let authorized = |token: String| {
            let state = state.clone();
            async move {
                state
                    .auth
                    .authorize_token(&state.auth_db, Some(&token), Permission::ViewData)
                    .await
                    .is_ok()
            }
        };
        // Reissuing for the same device replaces the old token.
        let first = issue(&app, "operator-phone", "phone-1", &["Abort"]).await;
        let second = issue(&app, "operator-phone", "phone-1", &["Abort"]).await;
        assert!(!authorized(first.token.clone()).await);
        assert!(authorized(second.token.clone()).await);
        // Explicit revoke of one device.
        let other = issue(&app, "operator-phone-2", "phone-2", &[]).await;
        let (status, body) = call(&app, "POST", "/api/auth/watch-token/revoke", Some("operator-phone"),
            Some(serde_json::json!({"device_id":"phone-1"}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"revoked\":1"));
        assert!(!authorized(second.token.clone()).await);
        assert!(authorized(other.token.clone()).await);
        // Signing out the phone session that issued it revokes it.
        let third = issue(&app, "operator-phone", "phone-1", &["Abort"]).await;
        assert_eq!(call(&app, "POST", "/api/auth/logout", Some("operator-phone"), None).await.0, StatusCode::NO_CONTENT);
        assert!(!authorized(third.token.clone()).await);
        assert!(authorized(other.token.clone()).await);
        // The watch can sign itself out.
        assert_eq!(call(&app, "POST", "/api/auth/logout", Some(&other.token), None).await.0, StatusCode::NO_CONTENT);
        assert!(!authorized(other.token).await);
        // Disabling the account stops a live token at once.
        let fourth = issue(&app, "operator-phone-2", "phone-2", &["Abort"]).await;
        let mut config = state.auth.load_users_file().unwrap();
        config.users[0].disabled = true;
        std::fs::write(state.auth.path(), serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(!authorized(fourth.token).await);
    }

    #[tokio::test]
    async fn anonymous_and_invalid_requests_cannot_get_tokens() {
        let (_root, _state, app) = fixture().await;
        assert_eq!(
            call(&app, "POST", "/api/auth/watch-token", None, Some(serde_json::json!({"device_id":"x"}))).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, "POST", "/api/auth/watch-token", Some("operator-phone"),
                Some(serde_json::json!({"device_id":"bad id!"}))).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&app, "GET", STATUS_PATH, Some("gsw1.not-a-real-token"), None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The real command handler admits a watch press exactly like a phone
    /// press: Abort is accepted, a command outside the token is refused
    /// before admission.
    #[tokio::test]
    async fn watch_commands_go_through_the_station_command_admission() {
        let (_root, state, app) = fixture().await;
        let grant = issue(&app, "operator-phone", "phone-1", &["Abort"]).await;
        let real = Router::new()
            .route("/api/command", post(crate::web::send_command_entry))
            .layer(axum::middleware::from_fn(scope_watch_tokens))
            .with_state(state.clone());
        let (status, body) = call(&real, "POST", "/api/command", Some(&grant.token), Some(serde_json::json!("Abort"))).await;
        assert!(status.is_success(), "{status} {body}");
        let (status, body) = call(&real, "POST", "/api/command", Some(&grant.token), Some(serde_json::json!("Dump"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, _) = call(&real, "POST", "/api/command", Some(&grant.token), Some(serde_json::json!("Launch"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
