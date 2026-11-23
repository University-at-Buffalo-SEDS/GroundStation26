//! Read-only raw telemetry feed for external controllers, overlays and
//! scripts (docs/backend/raw-stream-feed.md).
//!
//! * `GET /api/stream/raw/catalog`: every stream seen since start, with rates
//!   and latest values;
//! * `GET /api/stream/raw/ws`: WebSocket, `hello`, then `rows` batches,
//!   `status` every second and `bye` when access ends;
//! * `GET /api/stream/raw`: the same as NDJSON over one HTTP response;
//! * `POST|GET /api/admin/stream-tokens`, `POST /api/admin/stream-tokens/{id}/revoke`:
//!   administrators issue, list and revoke "stream viewer" tokens.
//!
//! The feed reads only the live telemetry fan-out (`AppState::ws_tx`). It has
//! no command path: a stream viewer token is refused by every other endpoint
//! (`auth::authorize_token` and [`scope_stream_tokens`]).
use crate::auth::{AuthFailure, Permission, StreamGrant, StreamScope};
use crate::state::AppState;
use crate::types::TelemetryRow;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

pub const SCHEMA: &str = "gs26.raw-stream.v1";
pub const FEED_PATH: &str = "/api/stream/raw";
pub const WS_PATH: &str = "/api/stream/raw/ws";
pub const CATALOG_PATH: &str = "/api/stream/raw/catalog";
const TOKENS_PATH: &str = "/api/admin/stream-tokens";

pub const DEFAULT_MAX_HZ: f64 = 50.0;
pub const MIN_MAX_HZ: f64 = 0.1;
pub const MAX_MAX_HZ: f64 = 1000.0;
pub const DEFAULT_MAX_ROWS_PER_S: u32 = 5_000;
pub const MAX_MAX_ROWS_PER_S: u32 = 20_000;
/// Rows a connection may have waiting; beyond it the oldest are dropped.
pub const QUEUE_ROWS: usize = 4_096;
pub const FLUSH_MS: u64 = 50;
pub const STATUS_MS: u64 = 1_000;
/// How often an open feed re-checks its token (revocation, expiry, issuer).
pub const AUTH_RECHECK_MS: u64 = 1_000;
/// A client that accepts no data for this long is disconnected.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Distinct (type, sender) streams tracked per connection and in the catalog.
const MAX_STREAMS: usize = 4_096;
const MAX_FILTER_ITEMS: usize = 64;

/// Row types never sent on the feed: anything that could carry people,
/// accounts or device locations. The live fan-out carries none of these
/// today (the proximity lock publishes names and states only, and no device
/// position is held server side); the list keeps it that way if one appears.
const DENIED_EXACT: &[&str] = &["GS_PROXIMITY_LOCK"];
const DENIED_FRAGMENTS: &[&str] = &[
    "USER",
    "DEVICE_LOCATION",
    "PERSONNEL",
    "PROXIMITY",
    "STATION_POSITION",
];

pub(crate) fn denied(data_type: &str) -> bool {
    DENIED_EXACT.contains(&data_type) || DENIED_FRAGMENTS.iter().any(|f| data_type.contains(f))
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(FEED_PATH, get(ndjson_feed))
        .route(WS_PATH, get(ws_feed))
        .route(CATALOG_PATH, get(catalog))
        .route(TOKENS_PATH, get(list_tokens).post(issue_token))
        .route("/api/admin/stream-tokens/{id}/revoke", post(revoke_token))
}

// ---------------------------------------------------------------------------
// Router scope for stream viewer tokens

fn stream_token_allowed(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", FEED_PATH)
            | ("GET", WS_PATH)
            | ("GET", CATALOG_PATH)
            | ("POST", "/api/auth/logout")
    )
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| percent_decode(value))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 3 <= bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (decoded, bytes[i]) {
            (Some(byte), _) => {
                out.push(byte);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, byte) => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Router layer: a stream viewer token (header or `?token=`) reaches only the
/// feed endpoints and its own logout; everything else answers 403.
pub async fn scope_stream_tokens(request: Request, next: Next) -> Response {
    let header_stream = crate::web::bearer_token_from_headers(request.headers())
        .is_some_and(crate::auth::is_stream_token);
    let query_stream = query_param(request.uri().query(), "token")
        .as_deref()
        .is_some_and(crate::auth::is_stream_token);
    let allowed = stream_token_allowed(request.method(), request.uri().path())
        // Logout takes its token only from the header.
        && !(query_stream && request.uri().path() == "/api/auth/logout");
    if (header_stream || query_stream) && !allowed {
        return (
            StatusCode::FORBIDDEN,
            crate::auth::STREAM_TOKEN_SCOPE_MESSAGE,
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
    label: String,
    #[serde(default)]
    ttl_hours: Option<u64>,
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    senders: Vec<String>,
    #[serde(default)]
    max_hz: Option<f64>,
}

#[derive(Serialize)]
struct TokenListEntry {
    #[serde(flatten)]
    info: crate::auth::StreamTokenInfo,
    last_used_ms: Option<i64>,
}

static LAST_USED: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();

fn note_used(id: &str) {
    if let Ok(mut map) = LAST_USED.get_or_init(Default::default).lock() {
        if map.len() > 1024 && !map.contains_key(id) {
            map.clear();
        }
        map.insert(id.to_string(), now_ms());
    }
}

fn last_used(id: &str) -> Option<i64> {
    LAST_USED.get()?.lock().ok()?.get(id).copied()
}

async fn issue_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<IssueRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let principal = match crate::admin::administrator(&state, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Json(request) = match body {
        Ok(body) => body,
        Err(err) => return (StatusCode::BAD_REQUEST, err.body_text()).into_response(),
    };
    let scope = StreamScope {
        label: request.label,
        types: request
            .types
            .iter()
            .map(|t| t.trim().to_ascii_uppercase())
            .collect(),
        senders: request.senders,
        max_hz: request.max_hz,
    };
    match state
        .auth
        .issue_stream_token(&state.auth_db, &principal, scope, request.ttl_hours)
        .await
    {
        Ok(grant) => {
            log::info!(
                "Stream viewer token {} ({:?}) issued by {}",
                grant.id,
                grant.label,
                grant.issuer
            );
            Json(grant).into_response()
        }
        Err(AuthFailure::Forbidden(msg)) if !msg.contains("administrator") => {
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(err) => auth_failure(err),
    }
}

async fn list_tokens(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::admin::administrator(&state, &headers).await {
        return response;
    }
    match state.auth.list_stream_tokens(&state.auth_db).await {
        Ok(tokens) => Json(
            tokens
                .into_iter()
                .map(|info| TokenListEntry {
                    last_used_ms: last_used(&info.id),
                    info,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(err) => auth_failure(err),
    }
}

#[derive(Serialize)]
struct Revoked {
    revoked: u64,
}

async fn revoke_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let principal = match crate::admin::administrator(&state, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    match state.auth.revoke_stream_token(&state.auth_db, &id).await {
        Ok(revoked) => {
            log::info!(
                "Stream viewer token {id} revoked by {} ({revoked} removed)",
                principal.username.as_deref().unwrap_or("?")
            );
            Json(Revoked { revoked }).into_response()
        }
        Err(AuthFailure::Forbidden(msg)) => (StatusCode::BAD_REQUEST, msg).into_response(),
        Err(err) => auth_failure(err),
    }
}

// ---------------------------------------------------------------------------
// Viewer authorization

#[derive(Clone, Debug)]
enum Viewer {
    Stream { grant: StreamGrant, token: String },
    Session { token: Option<String> },
}

impl Viewer {
    fn scope(&self) -> Option<&StreamScope> {
        match self {
            Viewer::Stream { grant, .. } => Some(&grant.scope),
            Viewer::Session { .. } => None,
        }
    }

    fn hello_json(&self) -> serde_json::Value {
        match self {
            Viewer::Stream { grant, .. } => serde_json::json!({
                "kind": "stream_token",
                "id": grant.id,
                "label": grant.scope.label,
                "expires_at_ms": grant.expires_at_ms,
            }),
            Viewer::Session { .. } => serde_json::json!({"kind": "session"}),
        }
    }
}

/// The header token wins over `?token=`; a stream viewer token or any
/// session with view-data permission may read the feed.
async fn authorize_viewer(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<Viewer, AuthFailure> {
    let token = crate::web::bearer_token_from_headers(headers)
        .map(str::to_string)
        .or_else(|| {
            query_token
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        });
    recheck(state, token).await
}

async fn recheck(state: &Arc<AppState>, token: Option<String>) -> Result<Viewer, AuthFailure> {
    match token {
        Some(token) if crate::auth::is_stream_token(&token) => {
            let grant = state.auth.authorize_stream(&state.auth_db, &token).await?;
            note_used(&grant.id);
            Ok(Viewer::Stream { grant, token })
        }
        token => {
            state
                .auth
                .authorize_token(&state.auth_db, token.as_deref(), Permission::ViewData)
                .await?;
            Ok(Viewer::Session { token })
        }
    }
}

fn token_of(viewer: &Viewer) -> Option<String> {
    match viewer {
        Viewer::Stream { token, .. } => Some(token.clone()),
        Viewer::Session { token } => token.clone(),
    }
}

// ---------------------------------------------------------------------------
// Filters

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Filter {
    pub types: Vec<String>,
    pub senders: Vec<String>,
    pub scope_types: Vec<String>,
    pub scope_senders: Vec<String>,
    pub max_hz: f64,
    pub max_rows_per_s: u32,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            types: Vec::new(),
            senders: Vec::new(),
            scope_types: Vec::new(),
            scope_senders: Vec::new(),
            max_hz: DEFAULT_MAX_HZ,
            max_rows_per_s: DEFAULT_MAX_ROWS_PER_S,
        }
    }
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
        }
        None => pattern.eq_ignore_ascii_case(value),
    }
}

fn list_matches(list: &[String], value: &str) -> bool {
    list.is_empty() || list.iter().any(|p| pattern_matches(p, value))
}

fn parse_list(raw: Option<&String>, name: &str, upper: bool) -> Result<Vec<String>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !crate::auth::valid_stream_filter_item(item) {
            return Err(format!("invalid {name} entry {item:?}"));
        }
        let item = if upper {
            item.to_ascii_uppercase()
        } else {
            item.to_string()
        };
        if !out.contains(&item) {
            out.push(item);
        }
    }
    if out.len() > MAX_FILTER_ITEMS {
        return Err(format!("at most {MAX_FILTER_ITEMS} {name} entries"));
    }
    Ok(out)
}

impl Filter {
    /// Request filters intersected with the token scope: a row must pass
    /// both, so a request can only narrow what its token allows.
    pub(crate) fn from_query(
        query: &BTreeMap<String, String>,
        scope: Option<&StreamScope>,
    ) -> Result<Self, String> {
        let types = parse_list(query.get("types"), "types", true)?;
        let senders = parse_list(query.get("senders"), "senders", false)?;
        let mut max_hz = match query.get("max_hz") {
            None => DEFAULT_MAX_HZ,
            Some(raw) => {
                let hz: f64 = raw
                    .trim()
                    .parse()
                    .map_err(|_| "max_hz must be a number".to_string())?;
                if !(hz.is_finite() && (MIN_MAX_HZ..=MAX_MAX_HZ).contains(&hz)) {
                    return Err(format!("max_hz must be {MIN_MAX_HZ}-{MAX_MAX_HZ}"));
                }
                hz
            }
        };
        let max_rows_per_s = match query.get("max_rows_per_s") {
            None => DEFAULT_MAX_ROWS_PER_S,
            Some(raw) => {
                let n: u32 = raw
                    .trim()
                    .parse()
                    .map_err(|_| "max_rows_per_s must be a whole number".to_string())?;
                if !(1..=MAX_MAX_ROWS_PER_S).contains(&n) {
                    return Err(format!("max_rows_per_s must be 1-{MAX_MAX_ROWS_PER_S}"));
                }
                n
            }
        };
        if let Some(cap) = scope.and_then(|s| s.max_hz) {
            max_hz = max_hz.min(cap);
        }
        Ok(Self {
            types,
            senders,
            scope_types: scope.map(|s| s.types.clone()).unwrap_or_default(),
            scope_senders: scope.map(|s| s.senders.clone()).unwrap_or_default(),
            max_hz,
            max_rows_per_s,
        })
    }

    pub(crate) fn admits(&self, data_type: &str, sender: &str) -> bool {
        !denied(data_type)
            && list_matches(&self.scope_types, data_type)
            && list_matches(&self.types, data_type)
            && list_matches(&self.scope_senders, sender)
            && list_matches(&self.senders, sender)
    }
}

// ---------------------------------------------------------------------------
// Rows

/// One decoded row as the feed sends it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct RowOut {
    pub ty: &'static str,
    pub seq: u64,
    pub data_type: String,
    pub sender: String,
    pub received_ms: i64,
    pub source_ms: Option<i64>,
    pub network_ms: Option<i64>,
    pub values: Vec<Option<f32>>,
    pub fields: &'static [&'static str],
    pub units: &'static [&'static str],
    pub provenance: &'static str,
}

/// The whole station runs on simulated firmware or the internal flight sim.
pub(crate) fn station_simulated() -> bool {
    crate::firmware_sim_mode::enabled()
        || crate::flight_sim_runtime::enabled()
        || crate::flight_sim::sim_mode_enabled()
}

/// `sim` for a simulated station, `replay` for the FC's integrated ground
/// test replay of recorded flight data (`SIM_*` rows, and FC flight states
/// while the FC flies a replay), otherwise `live`.
pub(crate) fn provenance(state: &AppState, data_type: &str, sender: &str) -> &'static str {
    if station_simulated() {
        "sim"
    } else if data_type.starts_with("SIM_")
        || (data_type == "FLIGHT_STATE"
            && crate::types::canonical_sender_id(sender) == "FC"
            && state.igt.simulated())
    {
        "replay"
    } else {
        "live"
    }
}

fn network_now(state: &AppState) -> Option<i64> {
    state
        .topology_router
        .get()
        .and_then(|router| router.network_time_ms())
        .and_then(|ms| i64::try_from(ms).ok())
}

fn row_out(state: &AppState, row: &TelemetryRow) -> RowOut {
    let (fields, units) = schema(&row.data_type);
    RowOut {
        ty: "row",
        seq: 0,
        data_type: row.data_type.clone(),
        sender: row.sender_id.clone(),
        received_ms: row.timestamp_ms,
        source_ms: crate::telemetry_provenance::source(row),
        network_ms: network_now(state),
        values: row.values.clone(),
        fields,
        units,
        provenance: provenance(state, &row.data_type, &row.sender_id),
    }
}

// ---------------------------------------------------------------------------
// Per-connection rate caps and backpressure

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Counters {
    pub sent: u64,
    pub dropped_rate_cap: u64,
    pub dropped_backpressure: u64,
    pub lagged: u64,
}

#[derive(Default)]
struct Slot {
    last_emit: Option<Instant>,
    held: Option<RowOut>,
}

/// Rate gate and bounded outbox of one connection. The broadcast reader
/// offers rows; the writer takes them. A slow writer only drops this
/// connection's oldest rows: ingest and other clients never wait.
pub(crate) struct Feed {
    inner: Mutex<FeedInner>,
}

struct FeedInner {
    filter: Filter,
    min_interval: Duration,
    slots: HashMap<(String, String), Slot>,
    tokens: f64,
    refilled: Option<Instant>,
    queue: VecDeque<RowOut>,
    cap: usize,
    seq: u64,
    counters: Counters,
}

impl Feed {
    pub(crate) fn new(filter: Filter, cap: usize) -> Self {
        let min_interval = Duration::from_secs_f64(1.0 / filter.max_hz);
        let tokens = filter.max_rows_per_s as f64;
        Self {
            inner: Mutex::new(FeedInner {
                filter,
                min_interval,
                slots: HashMap::new(),
                tokens,
                refilled: None,
                queue: VecDeque::new(),
                cap: cap.max(1),
                seq: 0,
                counters: Counters::default(),
            }),
        }
    }

    pub(crate) fn admits(&self, data_type: &str, sender: &str) -> bool {
        self.inner.lock().unwrap().filter.admits(data_type, sender)
    }

    /// Per-stream cap: a row passes when at least `1/max_hz` has elapsed
    /// since that stream's last passed row. Otherwise it is held, replacing
    /// (and counting as dropped) any older held row; the held row is sent
    /// when the window opens ([`Feed::release`]). So the newest value of a
    /// capped stream is never lost, only delayed by at most `1/max_hz`.
    pub(crate) fn offer(&self, row: RowOut, now: Instant) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.filter.admits(&row.data_type, &row.sender) {
            return;
        }
        let key = (row.data_type.clone(), row.sender.clone());
        if !inner.slots.contains_key(&key) && inner.slots.len() >= MAX_STREAMS {
            inner.emit(row, now);
            return;
        }
        let interval = inner.min_interval;
        let slot = inner.slots.entry(key.clone()).or_default();
        let open = slot
            .last_emit
            .is_none_or(|at| now.saturating_duration_since(at) >= interval);
        if open {
            let replaced = slot.held.take().is_some();
            slot.last_emit = Some(now);
            if replaced {
                inner.counters.dropped_rate_cap += 1;
            }
            inner.emit(row, now);
        } else if slot.held.replace(row).is_some() {
            inner.counters.dropped_rate_cap += 1;
        }
    }

    /// Sends held rows whose stream window has opened.
    pub(crate) fn release(&self, now: Instant) {
        let mut inner = self.inner.lock().unwrap();
        let interval = inner.min_interval;
        let mut due = Vec::new();
        for slot in inner.slots.values_mut() {
            if slot.held.is_some()
                && slot
                    .last_emit
                    .is_none_or(|at| now.saturating_duration_since(at) >= interval)
            {
                slot.last_emit = Some(now);
                due.extend(slot.held.take());
            }
        }
        due.sort_by_key(|row| row.received_ms);
        for row in due {
            inner.emit(row, now);
        }
    }

    pub(crate) fn take(&self) -> Vec<RowOut> {
        let mut inner = self.inner.lock().unwrap();
        let rows: Vec<RowOut> = inner.queue.drain(..).collect();
        inner.counters.sent += rows.len() as u64;
        rows
    }

    pub(crate) fn note_lagged(&self, missed: u64) {
        self.inner.lock().unwrap().counters.lagged += missed;
    }

    pub(crate) fn counters(&self) -> Counters {
        self.inner.lock().unwrap().counters
    }

    pub(crate) fn queued(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }
}

impl FeedInner {
    /// Total cap: a token bucket of `max_rows_per_s` (burst one second).
    /// Then the bounded queue: when full the OLDEST queued row is dropped.
    /// Sequence numbers are taken here, so a gap in `seq` is a row lost to
    /// backpressure (rate-cap drops never take a number).
    fn emit(&mut self, mut row: RowOut, now: Instant) {
        let rate = self.filter.max_rows_per_s as f64;
        if let Some(at) = self.refilled {
            self.tokens =
                (self.tokens + now.saturating_duration_since(at).as_secs_f64() * rate).min(rate);
        }
        self.refilled = Some(now);
        if self.tokens < 1.0 {
            self.counters.dropped_rate_cap += 1;
            return;
        }
        self.tokens -= 1.0;
        self.seq += 1;
        row.seq = self.seq;
        if self.queue.len() >= self.cap {
            self.queue.pop_front();
            self.counters.dropped_backpressure += 1;
        }
        self.queue.push_back(row);
    }
}

// ---------------------------------------------------------------------------
// Catalog (one task for the whole station)

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct CatalogEntry {
    pub data_type: String,
    pub sender: String,
    pub rows: u64,
    /// Rows per second over the last five whole seconds.
    pub rate_hz: f64,
    pub last_received_ms: i64,
    pub last_source_ms: Option<i64>,
    pub values: Vec<Option<f32>>,
    pub fields: &'static [&'static str],
    pub units: &'static [&'static str],
    pub provenance: &'static str,
}

struct Stat {
    entry: CatalogEntry,
    /// (whole second since `Catalog::epoch`, rows in it), newest last.
    seconds: VecDeque<(u64, u32)>,
}

pub(crate) struct Catalog {
    epoch: Instant,
    streams: Mutex<BTreeMap<(String, String), Stat>>,
}

const RATE_WINDOW_S: u64 = 5;

impl Catalog {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            streams: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn observe(&self, row: &RowOut, now: Instant) {
        let second = now.saturating_duration_since(self.epoch).as_secs();
        let mut streams = self.streams.lock().unwrap();
        let key = (row.data_type.clone(), row.sender.clone());
        if !streams.contains_key(&key) && streams.len() >= MAX_STREAMS {
            return;
        }
        let stat = streams.entry(key).or_insert_with(|| Stat {
            entry: CatalogEntry {
                data_type: row.data_type.clone(),
                sender: row.sender.clone(),
                rows: 0,
                rate_hz: 0.0,
                last_received_ms: row.received_ms,
                last_source_ms: None,
                values: Vec::new(),
                fields: row.fields,
                units: row.units,
                provenance: row.provenance,
            },
            seconds: VecDeque::new(),
        });
        stat.entry.rows += 1;
        stat.entry.last_received_ms = row.received_ms;
        stat.entry.last_source_ms = row.source_ms;
        stat.entry.values = row.values.clone();
        stat.entry.provenance = row.provenance;
        match stat.seconds.back_mut() {
            Some((s, n)) if *s == second => *n += 1,
            _ => stat.seconds.push_back((second, 1)),
        }
        while stat
            .seconds
            .front()
            .is_some_and(|(s, _)| *s + RATE_WINDOW_S + 1 < second)
        {
            stat.seconds.pop_front();
        }
    }

    pub(crate) fn snapshot(&self, filter: &Filter, now: Instant) -> Vec<CatalogEntry> {
        let second = now.saturating_duration_since(self.epoch).as_secs();
        let streams = self.streams.lock().unwrap();
        streams
            .values()
            .filter(|stat| filter.admits(&stat.entry.data_type, &stat.entry.sender))
            .map(|stat| {
                // The five whole seconds before the current one.
                let rows: u32 = stat
                    .seconds
                    .iter()
                    .filter(|(s, _)| *s < second && *s + RATE_WINDOW_S >= second)
                    .map(|(_, n)| *n)
                    .sum();
                let mut entry = stat.entry.clone();
                entry.rate_hz = (rows as f64 / RATE_WINDOW_S as f64 * 100.0).round() / 100.0;
                entry
            })
            .collect()
    }
}

static CATALOG: OnceLock<Arc<Catalog>> = OnceLock::new();

fn global_catalog() -> &'static Arc<Catalog> {
    CATALOG.get_or_init(|| Arc::new(Catalog::new()))
}

/// Starts the station-wide catalog task once per process.
pub fn start(state: &Arc<AppState>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if STARTED.set(()).is_err() {
        return;
    }
    let catalog = global_catalog().clone();
    let state = state.clone();
    let mut rx = state.ws_tx.subscribe();
    handle.spawn(async move {
        loop {
            match rx.recv().await {
                Ok(row) => {
                    if denied(&row.data_type) {
                        continue;
                    }
                    catalog.observe(&row_out(&state, &row), Instant::now());
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn station_provenance_json(state: &AppState) -> serde_json::Value {
    serde_json::json!({
        "station": if station_simulated() { "sim" } else { "live" },
        "igt_replay": !station_simulated() && state.igt.simulated(),
        "mode": crate::runtime_mode::label(crate::runtime_mode::current()),
    })
}

async fn catalog(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let viewer =
        match authorize_viewer(&state, &headers, query.get("token").map(String::as_str)).await {
            Ok(viewer) => viewer,
            Err(err) => return auth_failure(err),
        };
    let filter = match Filter::from_query(&query, viewer.scope()) {
        Ok(filter) => filter,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    Json(serde_json::json!({
        "schema": SCHEMA,
        "server_time_ms": now_ms(),
        "network_time_ms": network_now(&state),
        "provenance": station_provenance_json(&state),
        "viewer": viewer.hello_json(),
        "streams": global_catalog().snapshot(&filter, Instant::now()),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Feed sessions

fn hello(state: &AppState, viewer: &Viewer, filter: &Filter) -> serde_json::Value {
    serde_json::json!({
        "ty": "hello",
        "schema": SCHEMA,
        "server_time_ms": now_ms(),
        "viewer": viewer.hello_json(),
        "filters": {
            "types": filter.types,
            "senders": filter.senders,
            "scope_types": filter.scope_types,
            "scope_senders": filter.scope_senders,
        },
        "limits": {
            "max_hz": filter.max_hz,
            "max_rows_per_s": filter.max_rows_per_s,
            "queue_rows": QUEUE_ROWS,
            "flush_ms": FLUSH_MS,
            "status_ms": STATUS_MS,
        },
        "provenance": station_provenance_json(state),
    })
}

fn status(feed: &Feed) -> serde_json::Value {
    let c = feed.counters();
    serde_json::json!({
        "ty": "status",
        "sent": c.sent,
        "dropped_rate_cap": c.dropped_rate_cap,
        "dropped_backpressure": c.dropped_backpressure,
        "lagged": c.lagged,
        "queued": feed.queued(),
        "server_time_ms": now_ms(),
    })
}

fn bye(reason: &str) -> serde_json::Value {
    serde_json::json!({"ty": "bye", "reason": reason})
}

fn bye_reason(err: &AuthFailure) -> &'static str {
    match err {
        AuthFailure::Unauthorized(msg) if msg.contains("expired") => "expired",
        AuthFailure::Unauthorized(msg) if msg.contains("administrator") => "issuer_removed",
        AuthFailure::Unauthorized(_) | AuthFailure::Forbidden(_) => "revoked",
        AuthFailure::Internal(_) => "server_error",
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Framing {
    /// One `{"ty":"rows","rows":[...]}` message per flush.
    Batches,
    /// One JSON object per line, rows as `{"ty":"row",...}`.
    Lines,
}

/// Reads the live fan-out into `feed` until `stop` fires.
fn spawn_reader(state: Arc<AppState>, feed: Arc<Feed>, stop: tokio::sync::watch::Receiver<bool>) {
    let mut rx = state.ws_tx.subscribe();
    let mut stop = stop;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                received = rx.recv() => match received {
                    Ok(row) => {
                        if feed.admits(&row.data_type, &row.sender_id) {
                            feed.offer(row_out(&state, &row), Instant::now());
                        }
                    }
                    Err(RecvError::Lagged(missed)) => feed.note_lagged(missed),
                    Err(RecvError::Closed) => break,
                },
            }
        }
    });
}

/// Drives one feed: hello, rows, status, revocation. `send` returns false
/// when the client is gone.
async fn run_feed<S, Fut>(
    state: Arc<AppState>,
    viewer: Viewer,
    filter: Filter,
    framing: Framing,
    mut send: S,
) where
    S: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let line = |value: serde_json::Value| match framing {
        Framing::Lines => format!("{value}\n"),
        Framing::Batches => value.to_string(),
    };
    let feed = Arc::new(Feed::new(filter.clone(), QUEUE_ROWS));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    spawn_reader(state.clone(), feed.clone(), stop_rx);
    let deliver = |text: String| {
        let fut = send(text);
        async move { matches!(tokio::time::timeout(SEND_TIMEOUT, fut).await, Ok(true)) }
    };
    let mut deliver = deliver;
    if deliver(line(hello(&state, &viewer, &filter))).await {
        let mut tick = tokio::time::interval(Duration::from_millis(FLUSH_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_status = Instant::now();
        let mut last_auth = Instant::now();
        let token = token_of(&viewer);
        loop {
            tick.tick().await;
            let now = Instant::now();
            if now.duration_since(last_auth) >= Duration::from_millis(AUTH_RECHECK_MS) {
                last_auth = now;
                if let Err(err) = recheck(&state, token.clone()).await {
                    let _ = deliver(line(bye(bye_reason(&err)))).await;
                    break;
                }
            }
            feed.release(now);
            let rows = feed.take();
            if !rows.is_empty() {
                let text = match framing {
                    Framing::Batches => serde_json::json!({"ty": "rows", "rows": rows}).to_string(),
                    Framing::Lines => rows
                        .iter()
                        .map(|row| serde_json::to_string(row).unwrap_or_default() + "\n")
                        .collect(),
                };
                if !deliver(text).await {
                    break;
                }
            }
            if now.duration_since(last_status) >= Duration::from_millis(STATUS_MS) {
                last_status = now;
                if !deliver(line(status(&feed))).await {
                    break;
                }
            }
        }
    }
    let _ = stop_tx.send(true);
}

async fn ndjson_feed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let viewer =
        match authorize_viewer(&state, &headers, query.get("token").map(String::as_str)).await {
            Ok(viewer) => viewer,
            Err(err) => return auth_failure(err),
        };
    let filter = match Filter::from_query(&query, viewer.scope()) {
        Ok(filter) => filter,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(8);
    tokio::spawn(run_feed(
        state,
        viewer,
        filter,
        Framing::Lines,
        move |text: String| {
            let tx = tx.clone();
            async move { tx.send(Ok(Bytes::from(text))).await.is_ok() }
        },
    ));
    let body = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn ws_feed(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let viewer =
        match authorize_viewer(&state, &headers, query.get("token").map(String::as_str)).await {
            Ok(viewer) => viewer,
            Err(err) => return auth_failure(err),
        };
    let filter = match Filter::from_query(&query, viewer.scope()) {
        Ok(filter) => filter,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    ws.on_upgrade(move |socket| handle_ws(socket, state, viewer, filter))
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, viewer: Viewer, filter: Filter) {
    let (sink, mut incoming) = socket.split();
    let sink = Arc::new(tokio::sync::Mutex::new(sink));
    let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
    // The feed is read-only: incoming messages are ignored apart from close.
    let reader = tokio::spawn(async move {
        while let Some(Ok(message)) = incoming.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
        let _ = closed_tx.send(true);
    });
    let writer = sink.clone();
    run_feed(
        state,
        viewer,
        filter,
        Framing::Batches,
        move |text: String| {
            let writer = writer.clone();
            let closed = *closed_rx.borrow();
            async move {
                !closed
                    && writer
                        .lock()
                        .await
                        .send(Message::Text(text.into()))
                        .await
                        .is_ok()
            }
        },
    )
    .await;
    let mut sink = sink.lock().await;
    let _ = sink.send(Message::Close(None)).await;
    let _ = sink.close().await;
    reader.abort();
}

fn now_ms() -> i64 {
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

// ---------------------------------------------------------------------------
// Field names and units per data type (docs/data-pipeline.md section 2)

const NONE: &[&str] = &[];

/// Field names and units of a data type; empty for unknown types and for
/// raw payload rows that carry no values (health words, SD status).
pub(crate) fn schema(data_type: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match data_type {
        // DAQ
        "KG1000" => (&["kg1000_pin"], &["V"]),
        "KG50" => (&["kg50_pin"], &["V"]),
        "DAQ_KG50_SELECTED" => (&["source", "kg50_pin"], &["", "V"]),
        "DAQ_LOADCELL_CHAMBER" => (
            &["kg1000_pin", "chamber_pressure", "chamber_connector"],
            &["V", "psig", "V"],
        ),
        "LOADCELL_WEIGHT_KG" => (&["kg1000_mass"], &["kg"]),
        "LOADCELL_50_WEIGHT_KG" => (&["kg50_mass"], &["kg"]),
        "LOADCELL_FILL_PERCENT" => (&["fill"], &["%"]),
        "COMBUSTION_CHAMBER_PRESSURE" => (&["chamber_pressure"], &["psig"]),
        "COMBUSTION_CHAMBER_VOLTAGE" => (&["chamber_connector"], &["V"]),
        "COMBUSTION_CHAMBER_RANGE_STATUS" => (&["out_of_range"], &["0/1"]),
        "COMBUSTION_CHAMBER_PT_INSTALLED" => (&["installed"], &["0/1"]),
        "DAQ_ADC_TEMPERATURE" => (&["adc_die_temperature"], &["°C"]),
        "DAQ_SDADC_VOLTAGES" => (
            &["ch2", "ch3", "ch4", "ch5", "hvain1", "hvain2"],
            &["V", "V", "V", "V", "V", "V"],
        ),
        "DAQ_SAR_VOLTAGES" => (
            &[
                "sar_in1", "sar_in2", "sar_in3", "sar_in4", "sar_in5", "sar_in6", "sar_in7",
                "sar_in8",
            ],
            &["V", "V", "V", "V", "V", "V", "V", "V"],
        ),
        "DAQ_CURRENT_SENSE" => (
            &["j7", "j8", "j7_current", "j8_current"],
            &["V", "V", "A", "A"],
        ),
        "DAQ_POWER_MONITOR" => (
            &["vmon", "imon", "vmain", "input_current"],
            &["V", "V", "V", "A"],
        ),
        // Valve board, actuator board, gateway
        "FUEL_TANK_PRESSURE" => (&["tank_pressure_raw"], &["psi"]),
        "PRESSURE_TRANSDUCER_CALIBRATED" => (&["tank_pressure"], &["psi"]),
        "VALVE_STATE" => (
            &[
                "pilot",
                "normally_open_vent",
                "dump",
                "igniter",
                "nitrogen",
                "nitrous",
                "plumbing_retracted",
                "reserved",
            ],
            &[
                "open",
                "open",
                "open",
                "on",
                "open",
                "open",
                "retracted",
                "",
            ],
        ),
        "IGNITER_SENSE_SIGNAL" => (&["raw_counts", "signal", "current"], &["counts", "V", "A"]),
        "BATTERY_VOLTAGE" => (&["voltage"], &["V"]),
        "BATTERY_CURRENT" => (&["current"], &["A"]),
        // Flight computer
        "ACCEL_DATA" | "SIM_ACCEL_DATA" => (&["x", "y", "z"], &["m/s²", "m/s²", "m/s²"]),
        "GYRO_DATA" | "SIM_GYRO_DATA" => (&["x", "y", "z"], &["deg/s", "deg/s", "deg/s"]),
        "IMU_DATA" | "SIM_IMU_DATA" => (
            &[
                "accel_x", "accel_y", "accel_z", "gyro_x", "gyro_y", "gyro_z",
            ],
            &["m/s²", "m/s²", "m/s²", "deg/s", "deg/s", "deg/s"],
        ),
        "BAROMETER_DATA" | "SIM_BAROMETER_DATA" => (
            &["altitude", "pressure", "temperature"],
            &["m", "hPa", "°C"],
        ),
        "ASCENT_STATE" | "SIM_ASCENT_STATE" => (
            &["q0", "q1", "q2", "q3", "altitude", "velocity"],
            &["", "", "", "", "m", "m/s"],
        ),
        "DESCENT_STATE" | "SIM_DESCENT_STATE" => (
            &["latitude", "longitude", "altitude", "velocity"],
            &["deg", "deg", "m", "m/s"],
        ),
        "GPS_DATA" | "SIM_GPS_DATA" => {
            (&["latitude", "longitude", "altitude"], &["deg", "deg", "m"])
        }
        "GPS_SATELLITE_NUMBER" => (&["satellites"], &["count"]),
        "GPS_STATUS" => (
            &["module", "satellites", "last_byte_age", "last_fix_age"],
            &["0 none/1 silent/2 no fix/3 fix", "count", "s", "s"],
        ),
        "VEHICLE_SPEED" => (&["speed"], &["m/s"]),
        "LAUNCH_SITE" => (&["latitude", "longitude", "altitude"], &["deg", "deg", "m"]),
        "FLIGHT_STATE" => (&["state"], &["enum"]),
        "SIM_SAMPLE" => (
            &["w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7"],
            &["", "", "", "", "", "", "", ""],
        ),
        other => derived_battery_schema(other).unwrap_or((NONE, NONE)),
    }
}

/// Derived battery rows (`emit_derived_battery_rows`) share suffixes.
fn derived_battery_schema(
    data_type: &str,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    const TABLE: &[(&str, &[&str], &[&str])] = &[
        ("_PERCENT", &["percent"], &["%"]),
        ("_DROP_RATE_V_PER_MIN", &["drop_rate"], &["V/min"]),
        ("_REMAINING_MINUTES", &["remaining"], &["min"]),
        ("_REMAINING_DAYS", &["days"], &["d"]),
        ("_REMAINING_HOURS", &["hours"], &["h"]),
        ("_REMAINING_MINUTES_PART", &["minutes"], &["min"]),
    ];
    if !(data_type.contains("BATTERY") || data_type.contains("POWER")) {
        return None;
    }
    TABLE
        .iter()
        .find(|(suffix, _, _)| data_type.ends_with(suffix))
        .map(|(_, fields, units)| (*fields, *units))
}

#[cfg(test)]
#[path = "raw_stream_tests.rs"]
mod tests;
