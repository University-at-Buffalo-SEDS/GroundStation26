//! Keeps operator-set board config in sync with this station after a board
//! reboots, is reflashed, or comes up with wiped or defaulted storage.
//!
//! The station is the source of truth. Each board reports a compact identity
//! of the config it applied:
//! - DAQ calibration: BOARD_HEALTH words 13..15 ("CFG1", CRC-32 of the four
//!   calibration payloads as applied, flags), every 5 s
//!   (`network_variables::daq_calibration_identity`).
//! - AB fill drive: AB_FILL_DRIVE_STATUS bytes 34..35, a 16-bit fold of the
//!   request time of the station write that saved it (0: none), on the
//!   station's 1 s drive query (`actuator_plumbing::wire::setting_identity`).
//! - DAQ 50 kg input: the DAQ's report stream (`kg50_input_sync`).
//!
//! Per item the station compares that identity with its own value:
//! - equal: synced (a resync is only "verified" by this readback);
//! - the board's built-in default, or a value this station once sent: the
//!   station re-pushes it, but only in Startup or Idle with the item's own
//!   preconditions (no motion, outputs off), up to `MAX_QUICK_ATTEMPTS`, then
//!   once a minute; otherwise the item is out of sync and says why;
//! - anything else (a value this station never sent): a conflict. Nothing is
//!   pushed until the operator resolves it (keep the station's value, or for
//!   the drive, keep the board's).
//!
//! Identities this station sent are kept in `backend/data/board_config_sync.json`
//! so a station restart can still tell stale from unknown.
use crate::{auth::Permission, state::AppState};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// "CFG1" little-endian in DAQ BOARD_HEALTH word 13.
pub(crate) const DAQ_CONFIG_TAG: u32 = 0x3147_4643;
/// DAQ flags (word 15): the four calibration variables arrived from a station
/// this boot.
const DAQ_ALL_FROM_STATION: u32 = 0x0F;
/// Health reports come every 5 s; two missed ones is an outage.
const DAQ_REPORT_FRESH: Duration = Duration::from_secs(12);
/// The DAQ pulls the station's values after discovery; give it this long
/// after boot before judging a value it still holds from flash.
const DAQ_LOAD_GRACE_MS: u64 = 30_000;
const MAX_QUICK_ATTEMPTS: u32 = 3;
const SLOW_RETRY: Duration = Duration::from_secs(60);
const KNOWN_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Item {
    DaqCalibration,
    DaqKg50Input,
    AbFillDrive,
}
impl Item {
    fn board(self) -> &'static str {
        match self {
            Self::DaqCalibration | Self::DaqKg50Input => "DAQ",
            Self::AbFillDrive => "AB",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::DaqCalibration => "Load-cell calibration, thermal terms and filters",
            Self::DaqKg50Input => "50 kg load-cell input",
            Self::AbFillDrive => "Fill-line drive selection",
        }
    }
    /// Pushes before falling back to one a minute. A drive push is a
    /// tracked change that re-sends itself until the actuator confirms it
    /// (actuator_plumbing::set_resend), so it is the drive's only quick
    /// attempt: quick re-pushes on top would multiply its sends.
    fn quick_attempts(self) -> u32 {
        match self {
            Self::AbFillDrive => 1,
            Self::DaqCalibration | Self::DaqKg50Input => MAX_QUICK_ATTEMPTS,
        }
    }
    /// Time to wait for the board's readback after a push.
    fn settle(self) -> Duration {
        match self {
            Self::DaqCalibration => Duration::from_secs(12),
            Self::DaqKg50Input => Duration::from_secs(2),
            Self::AbFillDrive => Duration::from_secs(4),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Synced,
    Resyncing,
    OutOfSync,
    Conflict,
    /// No current readback (board offline or not reporting).
    #[default]
    Unknown,
}
impl Phase {
    fn rank(self) -> u8 {
        match self {
            Self::Synced => 0,
            Self::Unknown => 1,
            Self::Resyncing => 2,
            Self::OutOfSync => 3,
            Self::Conflict => 4,
        }
    }
}

/// How a board value that differs from the station's compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// The board's built-in default (wiped or factory storage).
    Default,
    /// A value this station sent earlier.
    Known,
    /// A value this station never sent.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observed {
    NoReadback(String),
    Match,
    /// The board is still taking the station's values (DAQ pull at boot, or
    /// a station write awaiting its readback).
    Loading(String),
    Mismatch {
        class: Class,
        board: String,
        identity: u64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    None,
    Push,
}

/// One item's resync state machine. Pure: the caller supplies the readback,
/// whether a push is allowed now, and does the push.
#[derive(Clone, Debug, Default)]
pub(crate) struct ItemSync {
    boot: Option<u64>,
    attempts: u32,
    last_push: Option<Instant>,
    pub(crate) phase: Phase,
    pub(crate) message: String,
    pub(crate) board_value: Option<String>,
    pub(crate) conflict_identity: Option<u64>,
    /// A push is awaiting its readback; reported once verified.
    unverified: bool,
}
/// What changed in a step, for operator notices.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Events {
    pub(crate) verified: bool,
    pub(crate) new_conflict: bool,
    pub(crate) gave_up: bool,
}
impl ItemSync {
    pub(crate) fn step(
        &mut self,
        item: Item,
        now: Instant,
        boot: Option<u64>,
        observed: Observed,
        allowed: Result<(), String>,
        overwrite: Option<u64>,
    ) -> (Decision, Events) {
        let mut events = Events::default();
        if boot.is_some() && boot != self.boot {
            // A new boot starts a fresh resync. First contact keeps what was
            // already counted (a drive change that gave up before the
            // board's boot was first seen).
            if self.boot.is_some() {
                self.attempts = 0;
                self.last_push = None;
            }
            self.boot = boot;
        }
        let was_conflict = self.conflict_identity;
        self.conflict_identity = None;
        match observed {
            Observed::NoReadback(reason) => {
                self.phase = Phase::Unknown;
                self.message = reason;
                self.conflict_identity = was_conflict;
                (Decision::None, events)
            }
            Observed::Match => {
                events.verified = self.unverified;
                self.unverified = false;
                self.attempts = 0;
                self.last_push = None;
                self.phase = Phase::Synced;
                self.board_value = None;
                self.message = if events.verified {
                    "Restored from the station and verified by the board's readback.".into()
                } else {
                    "The board's readback matches the station.".into()
                };
                (Decision::None, events)
            }
            Observed::Loading(reason) => {
                self.phase = Phase::Resyncing;
                self.message = reason;
                (Decision::None, events)
            }
            Observed::Mismatch { class, board, identity } => {
                self.board_value = Some(board.clone());
                if class == Class::Unknown && overwrite != Some(identity) {
                    self.phase = Phase::Conflict;
                    self.conflict_identity = Some(identity);
                    events.new_conflict = was_conflict != Some(identity);
                    self.message = format!(
                        "The board holds {board}, which this station never sent. Nothing is overwritten until an operator \
                         chooses which value to keep."
                    );
                    return (Decision::None, events);
                }
                let why = match class {
                    Class::Default => "the board is on its built-in default (reflashed or wiped storage)",
                    Class::Known => "the board holds an older value from this station",
                    Class::Unknown => "the operator chose to overwrite the board's value",
                };
                if let Err(reason) = allowed {
                    self.phase = Phase::OutOfSync;
                    self.message = format!("Config out of sync: {why}. {reason}");
                    return (Decision::None, events);
                }
                let since = self.last_push.map(|at| now.saturating_duration_since(at));
                if since.is_some_and(|since| since < item.settle()) {
                    self.phase = Phase::Resyncing;
                    self.message = format!("Re-applying the station's value: {why}; waiting for the board's readback.");
                    return (Decision::None, events);
                }
                let quick = item.quick_attempts();
                if self.attempts >= quick && since.is_some_and(|since| since < SLOW_RETRY) {
                    // Announced once per boot, not on every slow retry.
                    events.gave_up = self.phase != Phase::OutOfSync && self.attempts == quick;
                    self.phase = Phase::OutOfSync;
                    self.message = if item == Item::AbFillDrive {
                        format!(
                            "Config out of sync: {why}, and the actuator did not confirm the station's drive after \
                             its re-sends. The station retries once a minute."
                        )
                    } else {
                        format!(
                            "Config out of sync: {why}, and the board did not take the station's value after {} attempts. \
                             The station retries once a minute.",
                            self.attempts
                        )
                    };
                    return (Decision::None, events);
                }
                self.attempts = self.attempts.saturating_add(1);
                self.last_push = Some(now);
                self.unverified = true;
                self.phase = Phase::Resyncing;
                self.message = format!("Re-applying the station's value: {why}.");
                (Decision::Push, events)
            }
        }
    }
}

impl ItemSync {
    /// A tracked drive change gave up at `at` without the actuator
    /// confirming it: that was this item's quick attempt, so the next push
    /// waits for the once-a-minute retry.
    pub(crate) fn note_gave_up(&mut self, item: Item, at: Instant) {
        self.attempts = self.attempts.max(item.quick_attempts());
        self.last_push = Some(at);
        self.unverified = true;
    }
}

// ---- Identities this station sent (process-wide, persisted) ----

#[derive(Clone, Default, Serialize, Deserialize)]
struct Known {
    #[serde(default)]
    version: u8,
    #[serde(default)]
    daq_calibration: Vec<u32>,
    #[serde(default)]
    ab_drive_sources: Vec<u64>,
}
struct KnownStore {
    path: Option<PathBuf>,
    known: Known,
    /// Latest DAQ calibration publish, per router (one per station; tests
    /// run several stations in one process).
    last_daq_publish: HashMap<usize, Instant>,
}
static KNOWN: Mutex<Option<KnownStore>> = Mutex::new(None);

fn default_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    Some(
        std::env::var_os("GS_BOARD_CONFIG_SYNC_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("backend/data/board_config_sync.json")),
    )
}
fn with_known<T>(f: impl FnOnce(&mut KnownStore) -> T) -> T {
    let mut slot = KNOWN.lock().unwrap();
    let store = slot.get_or_insert_with(|| {
        let path = default_path();
        let known = path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        KnownStore {
            path,
            known,
            last_daq_publish: HashMap::new(),
        }
    });
    f(store)
}
fn persist(store: &KnownStore) {
    let Some(path) = &store.path else { return };
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(
            &temporary,
            serde_json::to_vec_pretty(&Known {
                version: 1,
                ..store.known.clone()
            })?,
        )?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = result {
        log::warn!("saving board config identities failed: {error}");
    }
}
fn remember<T: PartialEq + Copy>(list: &mut Vec<T>, value: T) -> bool {
    if list.last() == Some(&value) {
        return false;
    }
    list.retain(|v| *v != value);
    list.push(value);
    if list.len() > KNOWN_LIMIT {
        list.remove(0);
    }
    true
}
fn router_key(router: &sedsnet::router::Router) -> usize {
    router as *const _ as usize
}
/// This station published DAQ calibration with `identity` through `router`.
pub(crate) fn note_daq_published(router: &sedsnet::router::Router, identity: u32) {
    with_known(|store| {
        store
            .last_daq_publish
            .retain(|_, at| at.elapsed() < Duration::from_secs(60));
        store.last_daq_publish.insert(router_key(router), Instant::now());
        if remember(&mut store.known.daq_calibration, identity) {
            persist(store);
        }
    });
}
/// This station sent a drive SET stamped `source_ms`.
pub(crate) fn note_ab_drive_source(source_ms: u64) {
    with_known(|store| {
        if remember(&mut store.known.ab_drive_sources, source_ms) {
            persist(store);
        }
    });
}
fn daq_known(identity: u32) -> bool {
    with_known(|store| store.known.daq_calibration.contains(&identity))
}
fn ab_known(setting_id: u16) -> bool {
    with_known(|store| {
        store
            .known
            .ab_drive_sources
            .iter()
            .any(|source| crate::actuator_plumbing::setting_identity(*source) == setting_id)
    })
}

// ---- Per-station runtime ----

#[derive(Clone, Copy, Debug)]
struct DaqReport {
    identity: u32,
    flags: u32,
    uptime_ms: u64,
    boot: u64,
    at: Instant,
}
#[derive(Default)]
struct Inner {
    daq: Option<DaqReport>,
    daq_boots: u64,
    items: HashMap<Item, ItemSync>,
    /// Board identities an operator chose to overwrite with the station's value.
    overwrite: HashMap<Item, u64>,
    /// Out-of-sync notices raised, so each episode is announced once.
    announced: HashSet<Item>,
}
#[derive(Default)]
pub(crate) struct Runtime {
    inner: Mutex<Inner>,
}

/// A DAQ BOARD_HEALTH report arrived.
pub(crate) fn observe_daq_health(state: &AppState, words: &[u32]) {
    observe_daq_health_at(state, words, Instant::now());
}
fn observe_daq_health_at(state: &AppState, words: &[u32], now: Instant) {
    if words.len() != 24 || words[13] != DAQ_CONFIG_TAG || words[4] == 0 {
        return;
    }
    let ticks = u64::from(words[2]) | (u64::from(words[3]) << 32);
    let uptime_ms = ticks.saturating_mul(1000) / u64::from(words[4]);
    let mut inner = state.board_config_sync.inner.lock().unwrap();
    // Uptime going backwards is a reboot.
    if inner.daq.is_none_or(|old| uptime_ms < old.uptime_ms) {
        inner.daq_boots += 1;
    }
    let boot = inner.daq_boots;
    inner.daq = Some(DaqReport {
        identity: words[14],
        flags: words[15],
        uptime_ms,
        boot,
        at: now,
    });
}

fn configuration_allowed(state: &AppState) -> Result<(), String> {
    if crate::rrls::is_rrls() {
        return Err("Only the primary GroundStation re-applies board config.".into());
    }
    match state.local_flight_state_snapshot() {
        crate::types::FlightState::Startup | crate::types::FlightState::Idle => Ok(()),
        other => Err(format!(
            "Deferred: board config is only re-applied in Startup or Idle (station is in {other:?})."
        )),
    }
}

fn daq_observed(state: &AppState, report: Option<DaqReport>, now: Instant) -> (Option<u64>, Observed, Option<u32>) {
    let calibration = state.loadcell_calibration.lock().unwrap().clone();
    let desired = crate::network_variables::daq_calibration_identity(&calibration).ok();
    let Some(report) = report.filter(|r| now.saturating_duration_since(r.at) <= DAQ_REPORT_FRESH) else {
        return (
            None,
            Observed::NoReadback("No current DAQ config report.".into()),
            desired,
        );
    };
    let Some(desired) = desired else {
        return (
            Some(report.boot),
            Observed::NoReadback("The station's calibration file holds a value the DAQ cannot take.".into()),
            None,
        );
    };
    if report.identity == desired {
        return (Some(report.boot), Observed::Match, Some(desired));
    }
    if report.flags & DAQ_ALL_FROM_STATION != DAQ_ALL_FROM_STATION && report.uptime_ms < DAQ_LOAD_GRACE_MS {
        return (
            Some(report.boot),
            Observed::Loading("The DAQ is loading the station's values after its boot.".into()),
            Some(desired),
        );
    }
    let key = state.topology_router.get().map(|router| router_key(router));
    let published = key.and_then(|key| with_known(|store| store.last_daq_publish.get(&key).copied()));
    if published.is_some_and(|at| now.saturating_duration_since(at) < Item::DaqCalibration.settle()) {
        return (
            Some(report.boot),
            Observed::Loading("Waiting for the DAQ's readback of the station's latest calibration.".into()),
            Some(desired),
        );
    }
    let class = if report.identity == crate::network_variables::daq_default_identity() {
        Class::Default
    } else if daq_known(report.identity) {
        Class::Known
    } else {
        Class::Unknown
    };
    let board = format!("calibration {:08x} (station {:08x})", report.identity, desired);
    (
        Some(report.boot),
        Observed::Mismatch {
            class,
            board,
            identity: u64::from(report.identity),
        },
        Some(desired),
    )
}

fn ab_observed(state: &AppState) -> (Option<u64>, Observed) {
    let drive = crate::actuator_plumbing::drive_sync(state);
    if !drive.managed {
        return (None, Observed::Match);
    }
    if drive.pending {
        return (
            None,
            Observed::Loading("A station drive change is awaiting the actuator's confirmation.".into()),
        );
    }
    let Some((result, mode, setting_id, boot, _busy)) = drive.board else {
        return (None, Observed::NoReadback("No fresh actuator drive readback.".into()));
    };
    if !matches!(result, 0 | 6) {
        return (
            Some(boot),
            Observed::NoReadback(format!("The actuator answered the drive query with result {result}.")),
        );
    }
    if result == 0 && mode.map(|m| m as u8) == Some(drive.desired_mode) {
        return (Some(boot), Observed::Match);
    }
    let class = if setting_id == 0 {
        Class::Default
    } else if ab_known(setting_id) || crate::actuator_plumbing::setting_identity(drive.desired_source_ms) == setting_id
    {
        Class::Known
    } else {
        Class::Unknown
    };
    let board = format!(
        "the {} drive (station saved {})",
        mode.map_or("unknown", |m| crate::actuator_plumbing::mode_label(m as u8)),
        crate::actuator_plumbing::mode_label(drive.desired_mode)
    );
    (
        Some(boot),
        Observed::Mismatch {
            class,
            board,
            identity: u64::from(setting_id),
        },
    )
}

/// One pass: classify every item, push where due, raise notices.
pub(crate) fn tick(state: &Arc<AppState>, now: Instant) {
    if crate::rrls::is_rrls() || state.topology_router.get().is_none() {
        return;
    }
    // Follow a drive change first (check, re-send or give up), so the
    // drive item below sees its current state.
    crate::actuator_plumbing::service_drive_set(state, now);
    if crate::actuator_plumbing::take_drive_set_give_up(state) {
        let mut inner = state.board_config_sync.inner.lock().unwrap();
        inner.items.entry(Item::AbFillDrive).or_default().note_gave_up(Item::AbFillDrive, now);
    }
    let report = state.board_config_sync.inner.lock().unwrap().daq;
    let (daq_boot, daq, _) = daq_observed(state, report, now);
    let (ab_boot, ab) = ab_observed(state);
    let ab_allowed = configuration_allowed(state)
        .and_then(|()| crate::actuator_plumbing::resync_block_reason(state).map_or(Ok(()), Err));
    let plans = [
        (Item::DaqCalibration, daq_boot, daq, configuration_allowed(state)),
        (Item::AbFillDrive, ab_boot, ab, ab_allowed),
    ];
    for (item, boot, observed, allowed) in plans {
        let (decision, events, message) = {
            let mut inner = state.board_config_sync.inner.lock().unwrap();
            let overwrite = inner.overwrite.get(&item).copied();
            let sync = inner.items.entry(item).or_default();
            let (decision, events) = sync.step(item, now, boot, observed, allowed, overwrite);
            let message = sync.message.clone();
            let phase = sync.phase;
            if phase == Phase::Synced {
                inner.overwrite.remove(&item);
            }
            let announce = matches!(phase, Phase::OutOfSync) && inner.announced.insert(item);
            if phase == Phase::Synced {
                inner.announced.remove(&item);
            }
            (
                decision,
                Events {
                    gave_up: events.gave_up || announce,
                    ..events
                },
                message,
            )
        };
        if events.new_conflict {
            crate::web::emit_notification_warning(
                state,
                format!(
                    "{} {} config conflict: {message} Resolve it under Board config sync.",
                    item.board(),
                    item.label()
                ),
            );
        }
        if events.gave_up {
            crate::web::emit_notification_warning(state, format!("{} {}: {message}", item.board(), item.label()));
        }
        if events.verified {
            state.add_notification(format!(
                "{} {} restored from the station and verified.",
                item.board(),
                item.label()
            ));
        }
        if decision == Decision::Push {
            let pushed = match item {
                Item::DaqCalibration => push_daq(state),
                Item::AbFillDrive => crate::actuator_plumbing::resync_drive(state),
                Item::DaqKg50Input => Ok(()),
            };
            match pushed {
                Ok(()) => log::info!("board config resync: re-applied {} {}", item.board(), item.label()),
                Err(error) => {
                    log::warn!(
                        "board config resync of {} {} failed: {error}",
                        item.board(),
                        item.label()
                    );
                    let mut inner = state.board_config_sync.inner.lock().unwrap();
                    if let Some(sync) = inner.items.get_mut(&item) {
                        sync.message = format!("Re-applying failed: {error}");
                    }
                }
            }
        }
    }
}

fn push_daq(state: &AppState) -> Result<(), String> {
    let router = state.topology_router.get().ok_or("Telemetry router unavailable")?;
    let generation = crate::gse::capture_operator_origin(state).ok_or("Operator admission is closed.")?;
    let _claim =
        crate::gse::claim_operator_dispatch(state, generation).ok_or("Recovery changed the station context.")?;
    let calibration = state.loadcell_calibration.lock().unwrap().clone();
    crate::network_variables::set_daq_calibration(router, &calibration).map_err(|e| e.to_string())
}

pub async fn service(state: Arc<AppState>, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = shutdown.recv() => return,
        }
        let worker = state.clone();
        // Drive resync does file IO and takes the settings gate.
        let _ = tokio::task::spawn_blocking(move || tick(&worker, Instant::now())).await;
    }
}

// ---- Operator view and conflict resolution ----

fn kg50_view(state: &AppState) -> (Phase, String) {
    use crate::kg50_input_sync::SyncState;
    let selected = state.loadcell_calibration.lock().unwrap().kg50_input;
    let status = state.kg50_input_sync.status(selected);
    let deferred = configuration_allowed(state).err();
    match (status.state, deferred) {
        (SyncState::Synced, _) => (Phase::Synced, "The DAQ reports the station's input.".into()),
        (SyncState::NoReadback, _) => (Phase::Unknown, "No current DAQ input report.".into()),
        (_, Some(reason)) => (
            Phase::OutOfSync,
            format!("Config out of sync: the DAQ scans another 50 kg input. {reason}"),
        ),
        (SyncState::Syncing, None) => (Phase::Resyncing, "Re-sending the station's input selection.".into()),
        (SyncState::Mismatch, None) => (
            Phase::OutOfSync,
            "Config out of sync: the DAQ keeps reporting another 50 kg input after re-sends.".into(),
        ),
    }
}

pub(crate) fn view(state: &AppState) -> Value {
    let (kg50_phase, kg50_message) = kg50_view(state);
    let inner = state.board_config_sync.inner.lock().unwrap();
    let row = |item: Item| -> (Phase, Value) {
        let (phase, message, board, conflict) = match item {
            Item::DaqKg50Input => (kg50_phase, kg50_message.clone(), None, None),
            _ => {
                let sync = inner.items.get(&item).cloned().unwrap_or_default();
                (sync.phase, sync.message, sync.board_value, sync.conflict_identity)
            }
        };
        (
            phase,
            json!({
                "item": item, "label": item.label(), "state": phase, "message": message,
                "board_value": board, "conflict": conflict.is_some(),
                "can_keep_board": conflict.is_some() && item == Item::AbFillDrive,
            }),
        )
    };
    let boards: Vec<Value> = [
        ("DAQ", vec![Item::DaqCalibration, Item::DaqKg50Input]),
        ("AB", vec![Item::AbFillDrive]),
    ]
    .into_iter()
    .map(|(board, items)| {
        let rows: Vec<(Phase, Value)> = items.into_iter().map(row).collect();
        let worst = rows
            .iter()
            .map(|(p, _)| *p)
            .max_by_key(|p| p.rank())
            .unwrap_or_default();
        json!({"board": board, "state": worst, "items": rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>()})
    })
    .collect();
    json!({ "boards": boards })
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(view(&state)).into_response()
}

#[derive(Deserialize)]
struct ResolveRequest {
    item: Item,
    /// "station" overwrites the board with the station's value (when allowed);
    /// "board" keeps the board's value as the station's (drive only).
    keep: String,
}
async fn resolve(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<ResolveRequest>) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "Command permission required.").into_response();
    }
    if body.item == Item::AbFillDrive && !principal.allows_command_name("RetractPlumbing") {
        return (StatusCode::FORBIDDEN, "Fill-line command permission required.").into_response();
    }
    let worker = state.clone();
    match tokio::task::spawn_blocking(move || resolve_conflict(&worker, body.item, &body.keep)).await {
        Ok(Ok(message)) => Json(json!({"resolved": true, "message": message, "status": view(&state)})).into_response(),
        Ok(Err(message)) => (StatusCode::CONFLICT, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Resolve worker failed.").into_response(),
    }
}

pub(crate) fn resolve_conflict(state: &AppState, item: Item, keep: &str) -> Result<String, String> {
    let identity = {
        let inner = state.board_config_sync.inner.lock().unwrap();
        inner.items.get(&item).and_then(|sync| sync.conflict_identity)
    }
    .ok_or("There is no current conflict for this item.")?;
    match (keep, item) {
        ("station", _) => {
            let mut inner = state.board_config_sync.inner.lock().unwrap();
            inner.overwrite.insert(item, identity);
            Ok("The station's value will be re-applied when the board's state allows it.".into())
        }
        ("board", Item::AbFillDrive) => {
            let mode = crate::actuator_plumbing::adopt_board_drive(state)?;
            let mut inner = state.board_config_sync.inner.lock().unwrap();
            if let Some(sync) = inner.items.get_mut(&item) {
                sync.conflict_identity = None;
            }
            Ok(format!("The station now keeps the actuator's {mode} drive."))
        }
        ("board", _) => Err(
            "The DAQ's calibration cannot be read back as values; keep the station's calibration, or recalibrate."
                .into(),
        ),
        _ => Err("Choose keep \"station\" or \"board\".".into()),
    }
}

pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/api/board-config/status", get(status))
        .route("/api/board-config/resolve", post(resolve))
}

#[cfg(test)]
pub(crate) fn daq_health_words(identity: u32, flags: u32, uptime_ms: u64) -> [u32; 24] {
    let mut words = [0u32; 24];
    words[0] = 1;
    words[2] = uptime_ms as u32;
    words[3] = (uptime_ms >> 32) as u32;
    words[4] = 1000;
    words[13] = DAQ_CONFIG_TAG;
    words[14] = identity;
    words[15] = flags;
    words
}
#[cfg(test)]
pub(crate) fn observe_daq_health_for_test(state: &AppState, words: &[u32], now: Instant) {
    observe_daq_health_at(state, words, now);
}
#[cfg(test)]
pub(crate) fn item_for_test(state: &AppState, item: Item) -> ItemSync {
    state
        .board_config_sync
        .inner
        .lock()
        .unwrap()
        .items
        .get(&item)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "board_config_sync_tests.rs"]
mod tests;
