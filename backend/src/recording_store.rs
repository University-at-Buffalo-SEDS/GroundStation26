//! Always-on rolling recording and protected event captures.
//!
//! The DB actor (`telemetry_task::recording_owner`) always writes into a ring
//! of fixed-size SQLite segments under `data/recordings/rolling/`. When the
//! ring exceeds its byte budget the oldest closed segment is deleted. Each
//! segment is a complete recording (same schema as the legacy session files),
//! so a crash loses at most the unflushed batch and every tool that reads a
//! recording reads a segment.
//!
//! Event captures (launch, each sequence run, abort, manual snapshots, ...) are
//! copied out of the ring into `data/recordings/events/<type>/<folder>/` as a
//! self-contained `recording.db` plus `manifest.json`. Captures are outside the
//! ring budget and are never deleted automatically; segments a pending capture
//! still needs are pinned so rollover cannot remove its pre-roll.
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[path = "recording_store/api.rs"]
pub(crate) mod api;
#[path = "recording_store/board_sd.rs"]
pub(crate) mod board_sd;
#[cfg(test)]
#[path = "recording_store/tests.rs"]
mod tests;

pub(crate) const STORE_DIR: &str = "recordings";
pub(crate) const ROLLING_DIR: &str = "rolling";
pub(crate) const EVENTS_DIR: &str = "events";
pub(crate) const SEGMENT_PREFIX: &str = "segment_";
pub(crate) const CAPTURE_FILE: &str = "recording.db";
pub(crate) const MANIFEST_FILE: &str = "manifest.json";
const PARTIAL_FILE: &str = "recording.partial.db";
const PENDING_FILE: &str = "pending-captures.json";
const SETTINGS_FILE: &str = "settings.json";
const MIB: u64 = 1024 * 1024;
/// Rows reach the segment within one DB batch; captures wait this long past
/// their post-roll before copying so the final rows are committed.
const FLUSH_GRACE_MS: i64 = 2_000;
/// A segment's file stamp is when it was opened; rows queued just before the
/// rotation can carry slightly earlier timestamps.
const SEGMENT_SLACK_MS: i64 = 60_000;

pub(crate) fn store_dir(root: &Path) -> PathBuf {
    root.join(STORE_DIR)
}
pub(crate) fn rolling_dir(root: &Path) -> PathBuf {
    store_dir(root).join(ROLLING_DIR)
}
pub(crate) fn events_dir(root: &Path) -> PathBuf {
    store_dir(root).join(EVENTS_DIR)
}

// ---------------------------------------------------------------------------
// Event kinds and settings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Launch,
    Hitl,
    Abort,
    Fill,
    NitrogenTest,
    ValveSelfTest,
    Turbocharge,
    ColdFlow,
    Igniter,
    GroundTest,
    DaqDac,
    Manual,
}

impl EventKind {
    pub const ALL: [EventKind; 12] = [
        Self::Launch,
        Self::Hitl,
        Self::Abort,
        Self::Fill,
        Self::NitrogenTest,
        Self::ValveSelfTest,
        Self::Turbocharge,
        Self::ColdFlow,
        Self::Igniter,
        Self::GroundTest,
        Self::DaqDac,
        Self::Manual,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Hitl => "hitl",
            Self::Abort => "abort",
            Self::Fill => "fill",
            Self::NitrogenTest => "nitrogen_test",
            Self::ValveSelfTest => "valve_self_test",
            Self::Turbocharge => "turbocharge",
            Self::ColdFlow => "cold_flow",
            Self::Igniter => "igniter",
            Self::GroundTest => "ground_test",
            Self::DaqDac => "daq_dac",
            Self::Manual => "manual",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Launch => "Launch",
            Self::Hitl => "HITL run",
            Self::Abort => "Abort",
            Self::Fill => "Fill",
            Self::NitrogenTest => "Nitrogen pressure test",
            Self::ValveSelfTest => "Valve self-test",
            Self::Turbocharge => "Turbocharge",
            Self::ColdFlow => "Cold flow",
            Self::Igniter => "Igniter sequence",
            Self::GroundTest => "Integrated ground test",
            Self::DaqDac => "DAQ DAC validation",
            Self::Manual => "Manual snapshot",
        }
    }
    fn default_policy(self) -> EventPolicy {
        let (pre, post, max) = match self {
            // Ten minutes covers arming and the final fill top-off.
            Self::Launch => (600, 300, 2 * 3600),
            Self::Hitl => (300, 120, 2 * 3600),
            Self::Abort => (300, 300, 3600),
            Self::Fill => (120, 120, 6 * 3600),
            Self::NitrogenTest => (120, 120, 2 * 3600),
            Self::ValveSelfTest => (60, 60, 3600),
            Self::Turbocharge => (120, 120, 2 * 3600),
            Self::ColdFlow => (120, 180, 2 * 3600),
            Self::Igniter => (120, 120, 3600),
            Self::GroundTest => (120, 120, 8 * 3600),
            Self::DaqDac => (30, 30, 2 * 3600),
            Self::Manual => (120, 0, 3600),
        };
        EventPolicy {
            auto: true,
            pre_roll_s: pre,
            post_roll_s: post,
            max_duration_s: max,
        }
    }
    /// Sequence progress kinds map onto capture folders.
    pub fn from_sequence(kind: crate::sequence_progress::Kind) -> Self {
        use crate::sequence_progress::Kind;
        match kind {
            Kind::ValveSelfTest => Self::ValveSelfTest,
            Kind::NitrogenTest => Self::NitrogenTest,
            Kind::Fill | Kind::ContinueFillSequence => Self::Fill,
            Kind::Turbocharge => Self::Turbocharge,
            Kind::ColdFlow => Self::ColdFlow,
            Kind::IgniterSequence => Self::Igniter,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPolicy {
    pub auto: bool,
    pub pre_roll_s: u32,
    pub post_roll_s: u32,
    pub max_duration_s: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Rolling ring budget ceiling. Event captures are not counted. The ring
    /// also never grows into `reserve_free_mb` (see [`effective_budget`]).
    pub ring_budget_mb: u64,
    /// A segment is closed and a new one opened past this size.
    pub segment_mb: u64,
    /// Warn operators below this much free disk.
    pub warn_free_mb: u64,
    /// Below this the ring shrinks (oldest segments first) to restore it.
    pub critical_free_mb: u64,
    /// Free space the ring always leaves for event captures, board SD
    /// attachments and the OS: it stops growing (oldest segments go) here.
    pub reserve_free_mb: u64,
    pub events: BTreeMap<EventKind, EventPolicy>,
    /// Collect the Flight Computer's SD launch file automatically after it
    /// lands (`sd_download::fc`). Missing in older files: on.
    pub fc_sd_auto_collect: bool,
    /// Also fetch the FC's 60 s lead-up window from its ordinary file.
    pub fc_sd_leadup: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // 50 GB, or until the disk is nearly full, whichever comes first.
            ring_budget_mb: 50 * 1024,
            segment_mb: 64,
            warn_free_mb: 2048,
            critical_free_mb: 512,
            reserve_free_mb: 8 * 1024,
            events: EventKind::ALL
                .into_iter()
                .map(|kind| (kind, kind.default_policy()))
                .collect(),
            fc_sd_auto_collect: true,
            fc_sd_leadup: true,
        }
    }
}

impl Settings {
    /// Clamp into safe ranges; fill in any missing event type.
    pub fn normalized(mut self) -> Self {
        self.ring_budget_mb = self.ring_budget_mb.clamp(256, 1024 * 1024);
        self.segment_mb = self.segment_mb.clamp(8, 1024).min(self.ring_budget_mb / 4);
        self.critical_free_mb = self.critical_free_mb.clamp(64, 1024 * 1024);
        self.warn_free_mb = self.warn_free_mb.clamp(self.critical_free_mb, 4 * 1024 * 1024);
        // The ring stops before captures could push the disk into a warning.
        self.reserve_free_mb = self.reserve_free_mb.clamp(self.warn_free_mb, 4 * 1024 * 1024);
        for kind in EventKind::ALL {
            let policy = self.events.entry(kind).or_insert_with(|| kind.default_policy());
            policy.pre_roll_s = policy.pre_roll_s.min(4 * 3600);
            policy.post_roll_s = policy.post_roll_s.min(4 * 3600);
            policy.max_duration_s = policy.max_duration_s.clamp(60, 24 * 3600);
        }
        self
    }
    pub fn policy(&self, kind: EventKind) -> EventPolicy {
        self.events.get(&kind).copied().unwrap_or_else(|| kind.default_policy())
    }
    pub fn ring_budget_bytes(&self) -> u64 {
        self.ring_budget_mb.saturating_mul(MIB)
    }
    pub fn segment_bytes(&self) -> u64 {
        self.segment_mb.saturating_mul(MIB)
    }
}

/// The ring's budget now: the configured ceiling, but never more than the
/// ring plus the free space above the reserve (so the ring fills the disk
/// only up to the reserve kept for captures and the OS).
pub(crate) fn effective_budget(settings: &Settings, ring_bytes: u64, free: Option<u64>) -> u64 {
    let ceiling = settings.ring_budget_bytes();
    match free {
        Some(free) => ceiling.min(ring_bytes.saturating_add(free).saturating_sub(settings.reserve_free_mb.saturating_mul(MIB))),
        None => ceiling,
    }
}

pub(crate) fn load_settings(root: &Path) -> Settings {
    std::fs::read(store_dir(root).join(SETTINGS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
        .unwrap_or_default()
        .normalized()
}

pub(crate) fn save_settings(root: &Path, settings: &Settings) -> std::io::Result<()> {
    write_atomic(
        &store_dir(root).join(SETTINGS_FILE),
        &serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?,
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

// ---------------------------------------------------------------------------
// Recording identifiers
// ---------------------------------------------------------------------------

/// Where an id lives. Ids stay flat (one URL segment) so every existing
/// `/api/recordings/{id}/...` route keeps working.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RecordingId {
    /// `groundstation_recording_<stamp>.db` in the data directory.
    Legacy(String),
    /// `rolling.segment_<stamp>.db`.
    Segment(String),
    /// `event.<type>.<folder>.db` (the capture) or
    /// `event.<type>.<folder>.<file>.db` (an attachment in that folder, such
    /// as a board SD import resolved against the capture).
    Event {
        kind: EventKind,
        folder: String,
        file: Option<String>,
    },
}

fn stamp_chars(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b'_')
}
fn name_chars(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl RecordingId {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        if let Some(stamp) = id
            .strip_prefix("groundstation_recording_")
            .and_then(|s| s.strip_suffix(".db"))
        {
            return stamp_chars(stamp).then(|| Self::Legacy(id.to_owned()));
        }
        let body = id.strip_suffix(".db")?;
        let parts: Vec<&str> = body.split('.').collect();
        match parts.as_slice() {
            ["rolling", file] => file
                .strip_prefix(SEGMENT_PREFIX)
                .is_some_and(stamp_chars)
                .then(|| Self::Segment((*file).to_owned())),
            ["event", kind, folder] => Some(Self::Event {
                kind: EventKind::from_id(kind)?,
                folder: name_chars(folder, 120).then(|| (*folder).to_owned())?,
                file: None,
            }),
            ["event", kind, folder, file] => {
                let file = name_chars(file, 120).then(|| (*file).to_owned())?;
                if file == "recording" || file.starts_with("recording_partial") {
                    return None;
                }
                Some(Self::Event {
                    kind: EventKind::from_id(kind)?,
                    folder: name_chars(folder, 120).then(|| (*folder).to_owned())?,
                    file: Some(file),
                })
            }
            _ => None,
        }
    }
    /// Path relative to the data directory.
    pub(crate) fn relative_path(&self) -> PathBuf {
        match self {
            Self::Legacy(name) => PathBuf::from(name),
            Self::Segment(file) => Path::new(STORE_DIR).join(ROLLING_DIR).join(format!("{file}.db")),
            Self::Event { kind, folder, file } => Path::new(STORE_DIR)
                .join(EVENTS_DIR)
                .join(kind.id())
                .join(folder)
                .join(match file {
                    Some(file) => format!("{file}.db"),
                    None => CAPTURE_FILE.to_owned(),
                }),
        }
    }
    /// The containing directory relative to the data directory; the
    /// canonical file must resolve exactly there (no symlink escapes).
    pub(crate) fn relative_dir(&self) -> PathBuf {
        self.relative_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }
    pub(crate) fn segment_id(file_stem: &str) -> String {
        format!("rolling.{file_stem}.db")
    }
    pub(crate) fn event_id(kind: EventKind, folder: &str) -> String {
        format!("event.{}.{folder}.db", kind.id())
    }
    /// The id of a file placed beside `self` (an SD import output).
    pub(crate) fn sibling(&self, file_name: &str) -> Option<String> {
        let stem = file_name.strip_suffix(".db")?;
        match self {
            Self::Legacy(_) => Some(file_name.to_owned()),
            Self::Event { kind, folder, .. } => {
                Some(format!("event.{}.{folder}.{stem}.db", kind.id()))
            }
            Self::Segment(_) => None,
        }
    }
}

pub(crate) fn segment_stamp_ms(file_name: &str) -> Option<i64> {
    let stamp = file_name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(".db")?;
    parse_stamp_ms(stamp)
}

fn parse_stamp_ms(stamp: &str) -> Option<i64> {
    static FORMAT: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
        "[year]-[month]-[day]_[hour]-[minute]-[second]_[subsecond digits:3]"
    );
    let dt = time::PrimitiveDateTime::parse(stamp, FORMAT).ok()?;
    Some((dt.assume_utc().unix_timestamp_nanos() / 1_000_000) as i64)
}

pub(crate) fn stamp(ms: i64) -> String {
    crate::telemetry_db::human_readable_session_stamp(ms)
}

fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "capture".into() } else { out }
}

// ---------------------------------------------------------------------------
// Ring segments
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SegmentFile {
    pub name: String,
    pub path: PathBuf,
    pub start_ms: i64,
    /// Database plus any WAL/SHM sidecar.
    pub bytes: u64,
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

pub(crate) fn sqlite_bytes(path: &Path) -> u64 {
    let base = path.display().to_string();
    file_len(path) + file_len(Path::new(&format!("{base}-wal"))) + file_len(Path::new(&format!("{base}-shm")))
}

/// Segments sorted oldest first. Unparseable names are ignored (never deleted).
pub(crate) fn scan_segments(dir: &Path) -> Vec<SegmentFile> {
    let mut segments: Vec<SegmentFile> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let start_ms = segment_stamp_ms(&name)?;
            let meta = std::fs::symlink_metadata(entry.path()).ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(SegmentFile {
                bytes: sqlite_bytes(&entry.path()),
                path: entry.path(),
                name,
                start_ms,
            })
        })
        .collect();
    segments.sort_by(|a, b| (a.start_ms, &a.name).cmp(&(b.start_ms, &b.name)));
    segments
}

/// Oldest-first deletions that bring the ring under `budget` and restore
/// `critical_free` disk headroom. Never the active segment, never a segment a
/// pending capture may still read (its successor starts after `pin_ms`), and
/// always a contiguous oldest run so the ring has no holes.
pub(crate) fn plan_deletions(
    segments: &[SegmentFile],
    active: Option<&Path>,
    pin_ms: Option<i64>,
    budget: u64,
    free: Option<u64>,
    critical_free: u64,
) -> Vec<PathBuf> {
    let total: u64 = segments.iter().map(|s| s.bytes).sum();
    let over_budget = total.saturating_sub(budget);
    let disk_deficit = free.map(|f| critical_free.saturating_sub(f)).unwrap_or(0);
    let need = over_budget.max(disk_deficit);
    let mut freed = 0u64;
    let mut out = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        if freed >= need {
            break;
        }
        if active.is_some_and(|a| a == segment.path) {
            break;
        }
        let next_start = segments.get(index + 1).map(|s| s.start_ms).unwrap_or(i64::MAX);
        if pin_ms.is_some_and(|pin| next_start.saturating_add(SEGMENT_SLACK_MS) > pin) {
            break;
        }
        freed += segment.bytes;
        out.push(segment.path.clone());
    }
    out
}

pub(crate) fn remove_sqlite_files(path: &Path) {
    let base = path.display().to_string();
    for p in [base.clone(), format!("{base}-wal"), format!("{base}-shm"), format!("{base}-journal")] {
        let _ = std::fs::remove_file(p);
    }
}

pub(crate) fn new_segment_path(dir: &Path, now_ms: i64) -> PathBuf {
    let mut ms = now_ms;
    loop {
        let path = dir.join(format!("{SEGMENT_PREFIX}{}.db", stamp(ms)));
        if !path.exists() {
            return path;
        }
        ms += 1;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskState {
    #[default]
    Unknown,
    Ok,
    Low,
    Critical,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RingStatus {
    pub enabled: bool,
    pub segments: usize,
    pub bytes: u64,
    /// The configured ceiling.
    pub budget_bytes: u64,
    /// The budget applied now (ceiling, limited by free disk above the reserve).
    pub effective_budget_bytes: u64,
    pub segment_bytes: u64,
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
    pub active_segment: Option<String>,
    pub free_bytes: Option<u64>,
    pub disk: DiskState,
    pub deleted_segments: u64,
    pub pinned_from_ms: Option<i64>,
    pub warning: Option<String>,
}

// ---------------------------------------------------------------------------
// Capture manager
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub serial: u64,
    pub kind: EventKind,
    /// Deduplicates triggers (a sequence run id, "flight", "igt", ...).
    pub key: String,
    pub label: String,
    pub event_start_ms: i64,
    pub window_start_ms: i64,
    pub event_end_ms: Option<i64>,
    pub post_roll_ms: i64,
    pub max_end_ms: i64,
    pub result: Option<String>,
    pub operator: Option<String>,
    pub station_mode: String,
    /// "hardware", "firmware_sim" or "internal_flight_sim" (None: recorded before provenance was kept).
    #[serde(default)]
    pub provenance: Option<String>,
    #[serde(default)]
    pub details: serde_json::Value,
    #[serde(default, skip)]
    pub materializing: bool,
}

impl Capture {
    fn ready_at(&self) -> Option<i64> {
        self.event_end_ms.map(|end| end.saturating_add(self.post_roll_ms))
    }
    /// Run-keyed captures cannot continue across a restart; state-keyed ones
    /// (launch, abort, ground test) continue until their state ends.
    fn survives_restart(&self) -> bool {
        matches!(self.kind, EventKind::Launch | EventKind::Hitl | EventKind::Abort | EventKind::GroundTest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureBrief {
    pub kind: EventKind,
    pub title: String,
    pub label: String,
    pub event_start_ms: i64,
    /// Set once the event ended and only the post-roll remains.
    pub event_end_ms: Option<i64>,
}

#[derive(Default)]
struct Inner {
    settings: Settings,
    captures: Vec<Capture>,
    next_serial: u64,
    dirty: bool,
    ring: RingStatus,
    saved: u64,
}

/// Shared capture/ring state. Hooks only touch RAM; the DB actor and the
/// capture task own all file work.
#[derive(Default)]
pub struct Store {
    inner: Mutex<Inner>,
    rolling: std::sync::atomic::AtomicBool,
}

pub(crate) struct BeginRequest<'a> {
    pub kind: EventKind,
    pub key: &'a str,
    pub label: &'a str,
    pub operator: Option<&'a str>,
    pub details: serde_json::Value,
}

impl Store {
    /// Production enables the ring before the DB actor starts. Fixtures that
    /// never enable it keep every capture hook inert.
    pub fn enable_rolling(&self) {
        self.rolling.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn rolling_enabled(&self) -> bool {
        self.rolling.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }
    pub(crate) fn set_settings(&self, settings: Settings) {
        self.inner.lock().unwrap().settings = settings;
    }
    pub fn ring_status(&self) -> RingStatus {
        self.inner.lock().unwrap().ring.clone()
    }
    pub(crate) fn set_ring_status(&self, status: RingStatus) {
        self.inner.lock().unwrap().ring = status;
    }
    pub fn captures(&self) -> Vec<Capture> {
        self.inner.lock().unwrap().captures.clone()
    }
    pub fn briefs(&self) -> Vec<CaptureBrief> {
        self.inner
            .lock()
            .unwrap()
            .captures
            .iter()
            .map(|c| CaptureBrief {
                kind: c.kind,
                title: c.kind.title().into(),
                label: c.label.clone(),
                event_start_ms: c.event_start_ms,
                event_end_ms: c.event_end_ms,
            })
            .collect()
    }
    /// Earliest data any pending capture still needs.
    pub fn pin_ms(&self) -> Option<i64> {
        self.inner.lock().unwrap().captures.iter().map(|c| c.window_start_ms).min()
    }

    /// Starts (or re-opens, while only its post-roll remains) a capture.
    /// Returns true when the set of captures changed.
    pub(crate) fn begin(&self, request: BeginRequest<'_>, now_ms: i64, station_mode: &str) -> bool {
        if !self.rolling_enabled() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        let policy = inner.settings.policy(request.kind);
        if !policy.auto && request.kind != EventKind::Manual {
            return false;
        }
        if let Some(existing) = inner
            .captures
            .iter_mut()
            .find(|c| c.kind == request.kind && c.key == request.key && !c.materializing)
        {
            if existing.event_end_ms.is_none() {
                return false;
            }
            existing.event_end_ms = None;
            existing.result = None;
            existing.max_end_ms = now_ms.saturating_add(i64::from(policy.max_duration_s) * 1000);
            inner.dirty = true;
            return true;
        }
        let serial = inner.next_serial;
        inner.next_serial += 1;
        inner.captures.push(Capture {
            serial,
            kind: request.kind,
            key: request.key.to_owned(),
            label: request.label.to_owned(),
            event_start_ms: now_ms,
            window_start_ms: now_ms.saturating_sub(i64::from(policy.pre_roll_s) * 1000),
            event_end_ms: None,
            post_roll_ms: i64::from(policy.post_roll_s) * 1000,
            max_end_ms: now_ms.saturating_add(i64::from(policy.max_duration_s) * 1000),
            result: None,
            operator: request.operator.map(str::to_owned),
            station_mode: station_mode.to_owned(),
            provenance: Some(crate::firmware_sim_mode::provenance().to_owned()),
            details: request.details,
            materializing: false,
        });
        inner.dirty = true;
        true
    }

    pub(crate) fn end(&self, kind: EventKind, key: &str, now_ms: i64, result: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(capture) = inner
            .captures
            .iter_mut()
            .find(|c| c.kind == kind && c.key == key && c.event_end_ms.is_none())
        else {
            return false;
        };
        capture.event_end_ms = Some(now_ms);
        capture.result = Some(result.to_owned());
        inner.dirty = true;
        true
    }

    pub(crate) fn is_open(&self, kind: EventKind, key: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .captures
            .iter()
            .any(|c| c.kind == kind && c.key == key && c.event_end_ms.is_none())
    }

    /// An operator snapshot of the last `before_s` (and optionally the next
    /// `after_s`) seconds. Returns its serial.
    pub(crate) fn manual(
        &self,
        label: &str,
        before_s: u32,
        after_s: u32,
        operator: Option<&str>,
        now_ms: i64,
        station_mode: &str,
    ) -> Option<u64> {
        if !self.rolling_enabled() {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        let serial = inner.next_serial;
        inner.next_serial += 1;
        let end = now_ms.saturating_add(i64::from(after_s) * 1000);
        inner.captures.push(Capture {
            serial,
            kind: EventKind::Manual,
            key: format!("manual-{serial}"),
            label: label.to_owned(),
            event_start_ms: now_ms,
            window_start_ms: now_ms.saturating_sub(i64::from(before_s) * 1000),
            event_end_ms: Some(end),
            post_roll_ms: 0,
            max_end_ms: end,
            result: Some("Saved by operator".into()),
            operator: operator.map(str::to_owned),
            station_mode: station_mode.to_owned(),
            provenance: Some(crate::firmware_sim_mode::provenance().to_owned()),
            details: serde_json::json!({"before_s": before_s, "after_s": after_s}),
            materializing: false,
        });
        inner.dirty = true;
        Some(serial)
    }

    /// Captures whose post-roll (plus the flush grace) has passed. Each is
    /// handed out once; `finish` removes it after its files are written.
    pub(crate) fn due(&self, now_ms: i64) -> Vec<Capture> {
        let mut inner = self.inner.lock().unwrap();
        let mut out = Vec::new();
        let mut changed = false;
        for capture in inner.captures.iter_mut().filter(|c| !c.materializing) {
            if capture.event_end_ms.is_none() && now_ms >= capture.max_end_ms {
                capture.event_end_ms = Some(capture.max_end_ms);
                capture.post_roll_ms = 0;
                capture.result = Some("Maximum capture duration reached".into());
                changed = true;
            }
            if capture.ready_at().is_some_and(|ready| now_ms >= ready.saturating_add(FLUSH_GRACE_MS)) {
                capture.materializing = true;
                out.push(capture.clone());
            }
        }
        if changed {
            inner.dirty = true;
        }
        out
    }

    /// End every open capture now and drop remaining post-rolls (power off,
    /// reboot). Returns how many captures are pending.
    pub(crate) fn close_all_now(&self, now_ms: i64, result: &str) -> usize {
        let mut inner = self.inner.lock().unwrap();
        for capture in inner.captures.iter_mut() {
            if capture.event_end_ms.is_none() {
                capture.event_end_ms = Some(now_ms);
                capture.result = Some(result.to_owned());
            }
            let end = capture.event_end_ms.unwrap_or(now_ms);
            if end.saturating_add(capture.post_roll_ms) > now_ms {
                capture.post_roll_ms = (now_ms - end).max(0);
            }
        }
        inner.dirty = true;
        inner.captures.len()
    }

    pub(crate) fn finish(&self, serial: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.captures.retain(|c| c.serial != serial);
        inner.saved += 1;
        inner.dirty = true;
    }

    /// A failed copy is retried on the next tick.
    pub(crate) fn retry(&self, serial: u64) {
        if let Some(c) = self.inner.lock().unwrap().captures.iter_mut().find(|c| c.serial == serial) {
            c.materializing = false;
        }
    }

    fn take_dirty(&self) -> Option<Vec<Capture>> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.dirty {
            return None;
        }
        inner.dirty = false;
        Some(inner.captures.clone())
    }

    /// Resume captures saved before a restart. Run-keyed captures end at the
    /// restart (`interrupted_at_ms`, the last data before it).
    pub(crate) fn restore(&self, mut captures: Vec<Capture>, interrupted_at_ms: i64) {
        let mut inner = self.inner.lock().unwrap();
        for capture in captures.iter_mut() {
            capture.materializing = false;
            if capture.event_end_ms.is_none() && !capture.survives_restart() {
                capture.event_end_ms = Some(interrupted_at_ms);
                capture.result = Some("Interrupted: the station restarted during this event".into());
            }
        }
        inner.next_serial = captures.iter().map(|c| c.serial + 1).max().unwrap_or(0).max(inner.next_serial);
        inner.captures.extend(captures);
        inner.dirty = true;
    }
}

fn station_mode_id() -> String {
    crate::station_mode_switch::mode_id(crate::runtime_mode::current()).to_owned()
}

fn now_ms() -> i64 {
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

fn publish(state: &AppState) {
    state.update_recording_captures(state.recording_store.briefs());
}

// ---------------------------------------------------------------------------
// Trigger hooks (RAM only; safe from any thread, no await, no file IO)
// ---------------------------------------------------------------------------

pub(crate) fn begin(state: &AppState, request: BeginRequest<'_>) {
    let kind = request.kind;
    if state.recording_store.begin(request, now_ms(), &station_mode_id()) {
        log::info!("event capture started: {}", kind.id());
        publish(state);
    }
}

pub(crate) fn end(state: &AppState, kind: EventKind, key: &str, result: &str) {
    if state.recording_store.end(kind, key, now_ms(), result) {
        log::info!("event capture ending ({}): {result}", kind.id());
        publish(state);
    }
}

/// A level trigger: capture while `active`.
pub(crate) fn level(state: &AppState, kind: EventKind, key: &str, active: bool, label: &str, result: &str) {
    if !state.recording_store.rolling_enabled() {
        return;
    }
    let open = state.recording_store.is_open(kind, key);
    if active && !open {
        begin(state, BeginRequest { kind, key, label, operator: None, details: serde_json::Value::Null });
    } else if !active && open {
        end(state, kind, key, result);
    }
}

/// Every sequence-progress publication (all station sequences).
pub(crate) fn observe_sequence(
    state: &AppState,
    snapshot: &crate::sequence_progress::Snapshot,
) {
    if !state.recording_store.rolling_enabled() {
        return;
    }
    let kind = EventKind::from_sequence(snapshot.kind);
    let key = snapshot.key.run_id.as_str();
    if snapshot.outcome.terminal() {
        let result = format!("{:?}: {}", snapshot.outcome, snapshot.message);
        if !state.recording_store.is_open(kind, key) {
            // A run accepted and finished between observations still gets one.
            begin(state, BeginRequest {
                kind,
                key,
                label: &snapshot.title,
                operator: snapshot.origin_client.as_deref(),
                details: serde_json::json!({"sequence": snapshot.kind, "run_id": key}),
            });
        }
        end(state, kind, key, &result);
    } else if !state.recording_store.is_open(kind, key) {
        begin(state, BeginRequest {
            kind,
            key,
            label: &snapshot.title,
            operator: snapshot.origin_client.as_deref(),
            details: serde_json::json!({"sequence": snapshot.kind, "run_id": key}),
        });
    }
}

const FLIGHT_KEY: &str = "flight";

/// Every FLIGHT_STATE row the DB actor writes.
pub(crate) fn observe_flight_state(state: &AppState, code: i64) {
    use crate::types::FlightState as F;
    if !state.recording_store.rolling_enabled() {
        return;
    }
    let flight_kind = if crate::runtime_mode::is_hitl() { EventKind::Hitl } else { EventKind::Launch };
    let in_flight = [F::Launch, F::Ascent, F::Coast, F::Apogee, F::Descent, F::Reefing, F::InFlight]
        .iter()
        .any(|s| *s as i64 == code);
    let flight_open = [EventKind::Launch, EventKind::Hitl]
        .into_iter()
        .find(|k| state.recording_store.is_open(*k, FLIGHT_KEY));
    if in_flight && flight_open.is_none() {
        begin(state, BeginRequest {
            kind: flight_kind,
            key: FLIGHT_KEY,
            label: flight_kind.title(),
            operator: None,
            details: serde_json::json!({
                "trigger_flight_state": code,
                "apogee_plan": crate::apogee_planner::capture_details(state),
            }),
        });
    } else if let Some(kind) = flight_open {
        let result = if code == F::Landed as i64 {
            Some("Landed")
        } else if code == F::Recovery as i64 {
            Some("Recovery")
        } else if code == F::Aborted as i64 {
            Some("Aborted")
        } else if code == F::Idle as i64 || code == F::Startup as i64 {
            Some("Station returned to idle")
        } else {
            None
        };
        if let Some(result) = result {
            end(state, kind, FLIGHT_KEY, result);
        }
    }
    level(
        state,
        EventKind::Abort,
        "abort",
        code == F::Aborted as i64,
        EventKind::Abort.title(),
        "Left the aborted state",
    );
}

// ---------------------------------------------------------------------------
// Capture task: persistence and materialization
// ---------------------------------------------------------------------------

fn pending_path(root: &Path) -> PathBuf {
    events_dir(root).join(PENDING_FILE)
}

pub(crate) fn load_pending(root: &Path) -> Vec<Capture> {
    std::fs::read(pending_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Persist pending captures if they changed (cheap; once per tick).
pub(crate) fn persist_pending(state: &AppState, root: &Path) {
    if let Some(captures) = state.recording_store.take_dirty() {
        let bytes = serde_json::to_vec_pretty(&captures).unwrap_or_default();
        if let Err(error) = write_atomic(&pending_path(root), &bytes) {
            log::warn!("could not persist pending event captures: {error}");
        }
    }
}

/// Copies due captures out of the ring. Runs beside the DB actor until shutdown.
pub(crate) async fn capture_task(
    state: std::sync::Arc<AppState>,
    root: PathBuf,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks = 0u64;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.recv() => {
                persist_pending(&state, &root);
                return;
            }
        }
        run_due(&state, &root).await;
        persist_pending(&state, &root);
        if crate::sd_download::take_launch_fetch() {
            tokio::spawn(crate::sd_download::fetch_launch_files(state.clone()));
        }
        // Burn reports: the live provisional one and queued capture recomputes.
        crate::burn_report::tick(&state, &root);
        // FC SD collection after landing (trigger, and the worker after a restart).
        crate::sd_download::fc::tick(&state, &root);
        // Pending DAQ lead-up fetches (also after a restart) run in the background.
        ticks = ticks.wrapping_add(1);
        if ticks % 15 == 0 && crate::sd_download::leadup::pending(&root) {
            tokio::spawn(crate::sd_download::leadup::run(state.clone()));
        }
    }
}

pub(crate) async fn run_due(state: &AppState, root: &Path) -> Vec<Result<CaptureSaved, String>> {
    let due = state.recording_store.due(now_ms());
    let mut results = Vec::new();
    for capture in due {
        let result = materialize(root, &capture, now_ms()).await.map_err(|e| e.to_string());
        match &result {
            Ok(saved) => {
                // Test fire and the integrated ground test also get the GSE
                // CSV beside each capture (rolling segments do not).
                if crate::runtime_mode::is_test_fire_or_igt() {
                    let db = saved.folder.join(CAPTURE_FILE).to_string_lossy().into_owned();
                    let csv = crate::test_fire_csv::csv_path_for_db_path(&db);
                    let calibration = state.loadcell_calibration.lock().unwrap().clone();
                    if let Err(error) = crate::test_fire_csv::export_recording_csv(&db, &csv, &calibration).await {
                        log::error!("test-fire CSV for capture {} failed: {error}", saved.id);
                    }
                }
                state.recording_store.finish(capture.serial);
                if matches!(capture.kind, EventKind::Launch | EventKind::Hitl | EventKind::GroundTest) {
                    // Board SD launch files follow the launch capture: attach any
                    // already downloaded, then fetch the DAQ's over the network.
                    let root_owned = root.to_path_buf();
                    let _ = tokio::task::spawn_blocking(move || board_sd::rescan_downloads(&root_owned)).await;
                    // In every mode (HITL, integrated ground test and test fire
                    // too): the burn report's final version needs the DAQ file.
                    crate::sd_download::schedule_launch_fetch();
                    crate::burn_report::capture_saved(&capture, &saved.folder);
                }
                let message = format!(
                    "Saved {} capture \"{}\" ({} telemetry rows) to {}",
                    capture.kind.title(),
                    capture.label,
                    saved.telemetry_rows,
                    saved.folder.display()
                );
                log::info!("{message}");
                state.add_temporary_notification(message);
            }
            Err(error) => {
                log::error!("event capture {} failed: {error}", capture.kind.id());
                crate::web::emit_warning(
                    state,
                    format!("Could not save the {} capture yet (will retry): {error}", capture.kind.title()),
                );
                state.recording_store.retry(capture.serial);
            }
        }
        publish(state);
        results.push(result);
    }
    results
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub id: String,
    pub event_type: EventKind,
    pub title: String,
    pub label: String,
    pub event_start_ms: i64,
    pub event_end_ms: i64,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub pre_roll_s: i64,
    pub post_roll_s: i64,
    pub station_mode: String,
    /// Where the data came from: "hardware", "firmware_sim" (simulated board firmware) or "internal_flight_sim".
    #[serde(default)]
    pub provenance: Option<String>,
    #[serde(default)]
    pub simulated: bool,
    pub versions: serde_json::Value,
    pub operator: Option<String>,
    pub result: Option<String>,
    pub rows: BTreeMap<String, i64>,
    pub data_from_ms: Option<i64>,
    pub data_to_ms: Option<i64>,
    pub pre_roll_truncated: bool,
    pub source_segments: Vec<String>,
    pub saved_at_ms: i64,
    #[serde(default)]
    pub details: serde_json::Value,
    /// Board SD launch files attached automatically (`board_sd/<BOARD>/`).
    #[serde(default)]
    pub board_sd: Vec<board_sd::BoardSdEntry>,
}

pub struct CaptureSaved {
    pub folder: PathBuf,
    pub id: String,
    pub telemetry_rows: i64,
}

fn unique_folder(dir: &Path, base: &str) -> (String, PathBuf) {
    let mut name = base.to_owned();
    let mut n = 2;
    loop {
        let path = dir.join(&name);
        if !path.exists() {
            return (name, path);
        }
        name = format!("{base}_{n}");
        n += 1;
    }
}

/// Copies the capture window out of the ring into its own folder. Segments
/// are read through ATTACH on a separate connection; SQLite WAL readers see
/// only committed rows and never block the actor's writer, so copying while
/// the ring keeps writing is safe.
pub(crate) async fn materialize(root: &Path, capture: &Capture, now: i64) -> anyhow::Result<CaptureSaved> {
    let event_end = capture.event_end_ms.unwrap_or(now);
    let window_start = capture.window_start_ms;
    let window_end = event_end.saturating_add(capture.post_roll_ms).min(now.max(event_end));
    let kind_dir = events_dir(root).join(capture.kind.id());
    std::fs::create_dir_all(&kind_dir)?;
    let base = format!("{}_{}", stamp(capture.event_start_ms), slug(&capture.label));
    let (folder_name, folder) = unique_folder(&kind_dir, &base);
    std::fs::create_dir_all(&folder)?;
    let partial = folder.join(PARTIAL_FILE);
    let (db, _) = crate::telemetry_db::open_telemetry_db(&partial).await?;
    // The ring covers the pre-roll if its oldest needed segment began by then.
    let covered_from = segments_for_window(&scan_segments(&rolling_dir(root)), window_start, window_end)
        .first()
        .map(|s| s.start_ms);
    let copied = copy_window(&db, &rolling_dir(root), window_start, window_end).await;
    let (rows, data_from, data_to, sources) = match copied {
        Ok(v) => v,
        Err(error) => {
            db.close().await;
            remove_sqlite_files(&partial);
            let _ = std::fs::remove_dir(&folder);
            return Err(error);
        }
    };
    crate::telemetry_db::close_and_finalize_sqlite(db, &partial.to_string_lossy()).await;
    std::fs::rename(&partial, folder.join(CAPTURE_FILE))?;
    let id = RecordingId::event_id(capture.kind, &folder_name);
    let manifest = Manifest {
        format: "seds-gs-event-capture".into(),
        version: 1,
        id: id.clone(),
        event_type: capture.kind,
        title: capture.kind.title().into(),
        label: capture.label.clone(),
        event_start_ms: capture.event_start_ms,
        event_end_ms: event_end,
        window_start_ms: window_start,
        window_end_ms: window_end,
        pre_roll_s: (capture.event_start_ms - window_start) / 1000,
        post_roll_s: (window_end - event_end) / 1000,
        station_mode: capture.station_mode.clone(),
        provenance: capture.provenance.clone(),
        simulated: capture.provenance.as_deref().is_some_and(|p| p != "hardware"),
        versions: if capture.provenance.as_deref() == Some("firmware_sim") {
            serde_json::json!({"groundstation_backend": env!("CARGO_PKG_VERSION"),
                               "simulated_firmware": crate::firmware_sim_mode::manifest(),
                               "simulated_conditions": crate::firmware_sim_mode::current_control()})
        } else {
            serde_json::json!({"groundstation_backend": env!("CARGO_PKG_VERSION")})
        },
        operator: capture.operator.clone(),
        result: capture.result.clone(),
        pre_roll_truncated: covered_from.is_none_or(|from| from > window_start),
        data_from_ms: data_from,
        data_to_ms: data_to,
        rows,
        source_segments: sources,
        saved_at_ms: now,
        details: capture.details.clone(),
        board_sd: Vec::new(),
    };
    let telemetry_rows = manifest.rows.get("telemetry").copied().unwrap_or(0);
    write_atomic(&folder.join(MANIFEST_FILE), &serde_json::to_vec_pretty(&manifest)?)?;
    Ok(CaptureSaved { folder, id, telemetry_rows })
}

const CONTEXT_TYPES: [&str; 2] = ["GS_STATION_MODE", "GS_IGT"];
type TelemetryRow = (i64, Option<i64>, String, Option<String>, Option<String>, Option<String>);

async fn table_exists(db: &sqlx::SqlitePool, schema: &str, name: &str) -> bool {
    let sql = format!("SELECT COUNT(*) FROM {schema}.sqlite_master WHERE type='table' AND name=?");
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
        .bind(name)
        .fetch_one(db)
        .await
        .unwrap_or(0)
        > 0
}

/// Segments that can hold rows in `[start, end]`, oldest first.
pub(crate) fn segments_for_window(segments: &[SegmentFile], start: i64, end: i64) -> Vec<SegmentFile> {
    segments
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            let next = segments.get(i + 1).map(|n| n.start_ms).unwrap_or(i64::MAX);
            s.start_ms.saturating_sub(SEGMENT_SLACK_MS) <= end && next.saturating_add(SEGMENT_SLACK_MS) >= start
        })
        .map(|(_, s)| s.clone())
        .collect()
}

async fn attach(db: &sqlx::SqlitePool, path: &Path) -> Result<(), sqlx::Error> {
    sqlx::query("ATTACH DATABASE ? AS src")
        .bind(path.to_string_lossy().into_owned())
        .execute(db)
        .await
        .map(|_| ())
}
async fn detach(db: &sqlx::SqlitePool) {
    let _ = sqlx::query("DETACH DATABASE src").execute(db).await;
}

/// Copy one window of every recording table from the ring into `db`.
/// Context first (the station mode / IGT status and flight state in force at
/// the window start, and every calibration epoch up to its end) so the
/// capture exports and replays exactly like a recording started there.
pub(crate) async fn copy_window(
    db: &sqlx::SqlitePool,
    rolling: &Path,
    start: i64,
    end: i64,
) -> anyhow::Result<(BTreeMap<String, i64>, Option<i64>, Option<i64>, Vec<String>)> {
    let segments = segments_for_window(&scan_segments(rolling), start, end);
    let mut context: BTreeMap<&str, TelemetryRow> = BTreeMap::new();
    let mut state_before: Option<(i64, i64)> = None;
    for segment in &segments {
        attach(db, &segment.path).await?;
        for ty in CONTEXT_TYPES {
            let row: Option<TelemetryRow> = sqlx::query_as(
                "SELECT timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json FROM src.telemetry WHERE data_type=? AND timestamp_ms<? ORDER BY timestamp_ms DESC,id DESC LIMIT 1",
            )
            .bind(ty)
            .bind(start)
            .fetch_optional(db)
            .await?;
            if let Some(row) = row
                && context.get(ty).is_none_or(|old| old.0 <= row.0)
            {
                context.insert(ty, row);
            }
        }
        let fs: Option<(i64, i64)> = sqlx::query_as(
            "SELECT timestamp_ms,f_state FROM src.flight_state WHERE timestamp_ms<? ORDER BY timestamp_ms DESC,id DESC LIMIT 1",
        )
        .bind(start)
        .fetch_optional(db)
        .await?;
        if let Some(fs) = fs
            && state_before.is_none_or(|old| old.0 <= fs.0)
        {
            state_before = Some(fs);
        }
        detach(db).await;
    }
    for row in context.values() {
        sqlx::query("INSERT INTO telemetry (timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES (?,?,?,?,?,?)")
            .bind(row.0).bind(row.1).bind(&row.2).bind(&row.3).bind(&row.4).bind(&row.5)
            .execute(db).await?;
    }
    if let Some((ts, code)) = state_before {
        sqlx::query("INSERT INTO flight_state (timestamp_ms,f_state) VALUES (?,?)")
            .bind(ts).bind(code).execute(db).await?;
    }
    let mut sources = Vec::new();
    for segment in &segments {
        attach(db, &segment.path).await?;
        let result: Result<(), sqlx::Error> = async {
            sqlx::query("INSERT INTO telemetry (timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) SELECT timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json FROM src.telemetry WHERE timestamp_ms>=? AND timestamp_ms<=? ORDER BY id")
                .bind(start).bind(end).execute(db).await?;
            sqlx::query("INSERT INTO alerts (timestamp_ms,severity,message,condition_json) SELECT timestamp_ms,severity,message,condition_json FROM src.alerts WHERE timestamp_ms>=? AND timestamp_ms<=? ORDER BY id")
                .bind(start).bind(end).execute(db).await?;
            sqlx::query("INSERT INTO flight_state (timestamp_ms,f_state) SELECT timestamp_ms,f_state FROM src.flight_state WHERE timestamp_ms>=? AND timestamp_ms<=? ORDER BY id")
                .bind(start).bind(end).execute(db).await?;
            sqlx::query("INSERT OR IGNORE INTO messages (id,timestamp_ms,message,action_label,action_cmd) SELECT id,timestamp_ms,message,action_label,action_cmd FROM src.messages WHERE timestamp_ms>=? AND timestamp_ms<=?")
                .bind(start).bind(end).execute(db).await?;
            if table_exists(db, "src", "calibration_history").await {
                sqlx::query("CREATE TABLE IF NOT EXISTS calibration_history (timestamp_ms INTEGER NOT NULL, sender_id TEXT NOT NULL, sensor_id TEXT NOT NULL, config_json TEXT NOT NULL, PRIMARY KEY(timestamp_ms,sender_id,sensor_id))")
                    .execute(db).await?;
                sqlx::query("INSERT OR REPLACE INTO calibration_history SELECT timestamp_ms,sender_id,sensor_id,config_json FROM src.calibration_history WHERE timestamp_ms<=?")
                    .bind(end).execute(db).await?;
            }
            Ok(())
        }
        .await;
        detach(db).await;
        result?;
        sources.push(segment.name.clone());
    }
    let mut rows = BTreeMap::new();
    for table in ["telemetry", "alerts", "flight_state", "messages"] {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).fetch_one(db).await?;
        rows.insert(table.to_owned(), n);
    }
    let span: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT MIN(timestamp_ms),MAX(timestamp_ms) FROM telemetry WHERE timestamp_ms>=?",
    )
    .bind(start)
    .fetch_one(db)
    .await?;
    Ok((rows, span.0, span.1, sources))
}

// ---------------------------------------------------------------------------
// Ring maintenance (called by the DB actor)
// ---------------------------------------------------------------------------

/// Applies the budget and low-disk policy and refreshes the ring status.
/// Returns the deleted segment paths.
pub(crate) fn maintain_ring(
    state: &AppState,
    root: &Path,
    active: Option<&Path>,
    free: Option<u64>,
) -> Vec<PathBuf> {
    let settings = state.recording_store.settings();
    let dir = rolling_dir(root);
    let segments = scan_segments(&dir);
    let pin = state.recording_store.pin_ms();
    let critical = settings.critical_free_mb.saturating_mul(MIB);
    let ring_bytes: u64 = segments.iter().map(|s| s.bytes).sum();
    let budget = effective_budget(&settings, ring_bytes, free);
    let deletions = plan_deletions(&segments, active, pin, budget, free, critical);
    for path in &deletions {
        log::info!("rolling recording: deleting oldest segment {}", path.display());
        remove_sqlite_files(path);
    }
    let remaining: Vec<&SegmentFile> = segments.iter().filter(|s| !deletions.contains(&s.path)).collect();
    let freed: u64 = segments.iter().filter(|s| deletions.contains(&s.path)).map(|s| s.bytes).sum();
    let free_after = free.map(|f| f.saturating_add(freed));
    let warn = settings.warn_free_mb.saturating_mul(MIB);
    let disk = match free_after {
        None => DiskState::Unknown,
        Some(f) if f < critical => DiskState::Critical,
        Some(f) if f < warn => DiskState::Low,
        Some(_) => DiskState::Ok,
    };
    let warning = match disk {
        DiskState::Critical => Some(format!(
            "Station disk is critically full ({} MB free). The rolling recording has shrunk to {} MB; event captures are never deleted automatically. Download and delete old captures or legacy recordings.",
            free_after.unwrap_or(0) / MIB,
            remaining.iter().map(|s| s.bytes).sum::<u64>() / MIB
        )),
        DiskState::Low => Some(format!(
            "Station disk is low ({} MB free). Below {} MB the rolling recording shrinks; event captures are kept.",
            free_after.unwrap_or(0) / MIB,
            settings.critical_free_mb
        )),
        _ => None,
    };
    let previous = state.recording_store.ring_status();
    state.recording_store.set_ring_status(RingStatus {
        enabled: true,
        segments: remaining.len(),
        bytes: remaining.iter().map(|s| s.bytes).sum(),
        budget_bytes: settings.ring_budget_bytes(),
        effective_budget_bytes: budget,
        segment_bytes: settings.segment_bytes(),
        oldest_ms: remaining.first().map(|s| s.start_ms),
        newest_ms: Some(now_ms()),
        active_segment: active
            .and_then(|p| p.file_stem())
            .map(|s| RecordingId::segment_id(&s.to_string_lossy())),
        free_bytes: free_after,
        disk,
        deleted_segments: previous.deleted_segments + deletions.len() as u64,
        pinned_from_ms: pin,
        warning,
    });
    deletions
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ListedKind {
    Legacy,
    Rolling,
    Capture,
    Attachment,
}

#[derive(Clone, Debug, Serialize)]
pub struct Listed {
    pub id: String,
    pub bytes: u64,
    pub active: bool,
    pub kind: ListedKind,
    pub event_type: Option<EventKind>,
    pub label: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    #[serde(skip)]
    pub path: PathBuf,
}

fn plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

pub(crate) fn read_manifest(folder: &Path) -> Option<Manifest> {
    serde_json::from_slice(&std::fs::read(folder.join(MANIFEST_FILE)).ok()?).ok()
}

/// Legacy session files in the data directory (and SD-resolved derivatives).
pub(crate) fn list_legacy(root: &Path) -> Vec<Listed> {
    let mut out: Vec<Listed> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_string_lossy().into_owned();
            matches!(RecordingId::parse(&id), Some(RecordingId::Legacy(_))).then_some(())?;
            plain_file(&entry.path()).then_some(())?;
            Some(Listed {
                bytes: file_len(&entry.path()),
                active: false,
                kind: ListedKind::Legacy,
                event_type: None,
                label: None,
                start_ms: None,
                end_ms: None,
                path: entry.path(),
                id,
            })
        })
        .collect();
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

pub(crate) fn list_segments(root: &Path, active: Option<&Path>) -> Vec<Listed> {
    let segments = scan_segments(&rolling_dir(root));
    let mut out: Vec<Listed> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let stem = s.name.trim_end_matches(".db");
            Listed {
                id: RecordingId::segment_id(stem),
                bytes: s.bytes,
                active: active.is_some_and(|a| a.file_name() == s.path.file_name()),
                kind: ListedKind::Rolling,
                event_type: None,
                label: None,
                start_ms: Some(s.start_ms),
                end_ms: segments.get(i + 1).map(|n| n.start_ms),
                path: s.path.clone(),
            }
        })
        .collect();
    out.reverse();
    out
}

/// Complete captures (with a manifest) and the attachments in their folders.
pub(crate) fn list_captures(root: &Path) -> Vec<Listed> {
    let mut out = Vec::new();
    for kind in EventKind::ALL {
        let kind_dir = events_dir(root).join(kind.id());
        for entry in std::fs::read_dir(&kind_dir).into_iter().flatten().flatten() {
            let folder_name = entry.file_name().to_string_lossy().into_owned();
            if !name_chars(&folder_name, 120) || !entry.path().is_dir() {
                continue;
            }
            let folder = entry.path();
            let capture = folder.join(CAPTURE_FILE);
            let Some(manifest) = read_manifest(&folder) else { continue };
            if !plain_file(&capture) {
                continue;
            }
            out.push(Listed {
                id: RecordingId::event_id(kind, &folder_name),
                bytes: file_len(&capture),
                active: false,
                kind: ListedKind::Capture,
                event_type: Some(kind),
                label: Some(manifest.label.clone()),
                start_ms: Some(manifest.window_start_ms),
                end_ms: Some(manifest.window_end_ms),
                path: capture,
            });
            for file in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                if name == CAPTURE_FILE || !name.ends_with(".db") || !plain_file(&file.path()) {
                    continue;
                }
                let base = RecordingId::Event { kind, folder: folder_name.clone(), file: None };
                let Some(id) = base.sibling(&name) else { continue };
                if RecordingId::parse(&id).is_none() {
                    continue;
                }
                out.push(Listed {
                    id,
                    bytes: file_len(&file.path()),
                    active: false,
                    kind: ListedKind::Attachment,
                    event_type: Some(kind),
                    label: Some(format!("{} (attachment)", manifest.label)),
                    start_ms: Some(manifest.window_start_ms),
                    end_ms: Some(manifest.window_end_ms),
                    path: file.path(),
                });
            }
        }
    }
    out.sort_by(|a, b| b.start_ms.cmp(&a.start_ms).then(b.id.cmp(&a.id)));
    out
}

/// Every readable recording: rolling segments, captures, then legacy files.
pub(crate) fn list_all(root: &Path, active: Option<&Path>) -> Vec<Listed> {
    let mut all = list_segments(root, active);
    all.extend(list_captures(root));
    all.extend(list_legacy(root));
    all
}

/// The non-duplicated sources for "all recordings" exports: the ring plus
/// legacy files (captures are copies of ring data).
pub(crate) fn primary_sources(root: &Path) -> Vec<Listed> {
    let mut all = list_legacy(root);
    all.extend(list_segments(root, None));
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all
}

pub(crate) fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if meta.is_dir() {
            total += dir_bytes(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// A copy interrupted by a crash leaves a folder with only a partial file and
/// no manifest. That data is a copy of ring data (still pinned by the pending
/// capture, which is retried), so the incomplete folder is removed.
pub(crate) fn remove_incomplete_captures(root: &Path) {
    for kind in EventKind::ALL {
        for entry in std::fs::read_dir(events_dir(root).join(kind.id())).into_iter().flatten().flatten() {
            let folder = entry.path();
            if folder.is_dir()
                && !folder.join(MANIFEST_FILE).exists()
                && (folder.join(PARTIAL_FILE).exists() || folder.join(CAPTURE_FILE).exists())
            {
                log::warn!("removing incomplete capture copy {}", folder.display());
                let _ = std::fs::remove_dir_all(&folder);
            }
        }
    }
}
