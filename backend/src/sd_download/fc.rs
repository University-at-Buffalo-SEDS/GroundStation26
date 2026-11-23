//! Automatic collection of the Flight Computer's SD launch file after landing.
//!
//! No operator action: when the FC's own state report says Landed and holds
//! it for [`LANDED_CONFIRM_MS`] (a transient Landed that reverts does not
//! count), or the FC reports Recovery, a job is queued in
//! `data/sd-downloads/FC/jobs.json`. The worker then, over SEDSnet P2P port
//! 4511 (SD transfer protocol v2 as the DAQ, FC directory 0 = card root):
//!
//! 1. lists the card root and picks the newest `irec_LAUNCH*` file of this
//!    flight ([`choose_launch_file`]);
//! 2. downloads it into `sd-downloads/FC/ROOT/<name>.part` (resumable by
//!    offset across link drops and station restarts);
//! 3. attaches it to the matching launch, HITL or ground-test capture
//!    (`board_sd/FC/<name>`, provenance `network_download` + link), notifies
//!    the operator and queues the burn-report recompute;
//! 4. optionally (admin setting `fc_sd_leadup`, default on) reads the 60 s
//!    lead-up window from the same boot's ordinary file (`leadup.rs`).
//!
//! It is the lowest-priority user of the link: a streamed transfer whose
//! window and rate the station sets ([`profile`], [`rate`]: radio 460-byte
//! DATA messages, window 6, 768 B/s of DATA rising to 1536 B/s while the
//! link is calm; wired 512-byte messages, window 8, 64 KiB/s), paused (rate 0)
//! while commands
//! were sent in the last 10 s, while recovery GPS stalls, while the radio is
//! congested or poor, while another SD transfer or the DAQ launch fetch runs,
//! and while an operator stop is in force (persisted). Failures back off up
//! to 60 s and are retried until the job completes; nothing gives up after a
//! deadline.
use super::*;
use crate::recording_store::board_sd;
use std::collections::{HashSet, VecDeque};

/// The FC Landed report must hold this long before the fetch starts.
pub(crate) const LANDED_CONFIRM_MS: u64 = 5_000;
/// Recovery GPS counts as degraded once it was arriving and then stopped
/// for this long...
const GPS_GAP_MS: u64 = 3_000;
/// ...until it has been silent this long (then the GPS source itself is gone
/// and waiting for it would only stall the collection).
const GPS_GONE_MS: u64 = 60_000;
/// The FC closes its launch file about 5 s after Landed (`sd_conclude`); an
/// open file is waited for at most this long, then fetched as it is.
const OPEN_FILE_WAIT_MS: u64 = 120_000;
const MAX_BACKOFF_MS: u64 = 60_000;
const JOBS_FILE: &str = "jobs.json";
/// DATA bytes per second (SDX-encoded, headers included; about 6x as many
/// CSV bytes on an FC launch file).
const RADIO_DEFAULT_BPS: u64 = 768;
const WIRED_DEFAULT_BPS: u64 = 64 * 1024;
/// FC rows per millisecond on the pad (FC docs section 7.5: ~65 KB/s).
const FC_PAD_BYTES_PER_MS: u64 = 65;

/// How the station reaches the FC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    /// The AV-bay RFD radio (shared with recovery GPS and commands).
    Radio,
    /// Bench, umbilical, USB or CAN: no radio in the path.
    Wired,
}

impl Link {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Radio => "radio",
            Self::Wired => "wired",
        }
    }
}

/// The FC link. `GS_FC_SD_LINK=radio|wired` overrides. Otherwise the radio
/// is assumed whenever the AV-bay link is an RFD modem (raw-UART serial or a
/// configured RFD profile), unless the FC has been heard only on the
/// umbilical side. Without an RFD modem the FC is reached by wire.
pub(crate) fn link(state: &AppState) -> Link {
    match std::env::var("GS_FC_SD_LINK").ok().as_deref().map(str::trim) {
        Some("radio") => return Link::Radio,
        Some("wired") => return Link::Wired,
        _ => {}
    }
    if crate::comms::rfd_profile::active().is_none() {
        return Link::Wired;
    }
    let sides = state.network_health.lock().map(|t| t.sides("FC")).unwrap_or(None);
    match sides {
        Some((false, true)) => Link::Wired,
        _ => Link::Radio,
    }
}

fn env_rate(name: &str, default: u64, range: std::ops::RangeInclusive<u64>) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| range.contains(v))
        .unwrap_or(default)
}

/// DATA message size and window for the link.
pub(crate) fn profile(link: Link) -> Profile {
    match link {
        Link::Radio => Profile::RADIO,
        Link::Wired => Profile::FC_WIRED,
    }
}

/// Starting DATA bytes per second for the link (the env names are kept from
/// protocol v1, where they counted CSV bytes).
pub(crate) fn rate(link: Link) -> u64 {
    match link {
        Link::Radio => env_rate("GS_FC_SD_RADIO_MAX_BYTES_PER_SEC", RADIO_DEFAULT_BPS, 64..=65_536),
        Link::Wired => env_rate("GS_FC_SD_WIRED_MAX_BYTES_PER_SEC", WIRED_DEFAULT_BPS, 512..=262_144),
    }
}

/// Radio ceiling for the adaptive rate (DATA B/s). The RF board's radio
/// turn (691 B per 200 ms of UART bytes, ~3.4 KB/s for all AV-bay egress)
/// carries ~1.5 KB/s of telemetry. A 460-byte DATA message goes out as three
/// 192-byte frames, about 1.35 wire bytes per DATA byte down (measured by
/// `wire_bytes_per_payload_byte_on_an_instant_link`), so the 768 B/s start
/// uses ~1.0 KB/s of the turn and the 1536 B/s ceiling ~2.1 KB/s: at the
/// ceiling the turn is full with telemetry, which is why the rate rises only
/// while the GS radio queue stays nearly empty and halves on any gap.
pub(crate) fn radio_ceiling() -> u64 {
    env_rate("GS_FC_SD_RADIO_CEILING_BYTES_PER_SEC", 1536, 64..=4096).max(rate(Link::Radio))
}

/// Adaptive radio rate (AIMD): starts at the base rate, rises by `STEP`
/// every `STEP_MS` while recovery traffic is light (no yield reason and no
/// command for [`CALM_AFTER_COMMAND_MS`]) up to the ceiling, halves on a gap
/// or a bad block (not below [`MIN_RATE`]), and drops back to at most the
/// base at once on any yield trigger. The same rule serves the direct and
/// the RRLS path.
#[derive(Debug, Clone)]
pub(crate) struct AdaptiveRate {
    pub base: u64,
    pub ceiling: u64,
    current: u64,
    calm_since: Option<u64>,
    last_step: u64,
}

pub(crate) const CALM_AFTER_COMMAND_MS: u64 = 30_000;
const STEP: u64 = 128;
const STEP_MS: u64 = 2_000;
/// The halving stops here (DATA B/s): one 460-byte message every ~2 s.
pub(crate) const MIN_RATE: u64 = 256;

impl AdaptiveRate {
    pub(crate) fn new(base: u64, ceiling: u64) -> Self {
        Self { base, ceiling: ceiling.max(base), current: base, calm_since: None, last_step: 0 }
    }
    /// `calm`: Landed/Recovery, no yield reason, no recent command, GPS
    /// fresh, the radio queue (nearly) empty. `losses`: gaps or bad blocks
    /// seen since the last call.
    pub(crate) fn update(&mut self, now: u64, calm: bool, losses: u32) -> u64 {
        if losses > 0 {
            self.current = (self.current / 2).max(MIN_RATE.min(self.base));
            self.calm_since = Some(now);
            self.last_step = now;
            return self.current;
        }
        if !calm {
            self.current = self.current.min(self.base);
            self.calm_since = None;
            return self.current;
        }
        let since = *self.calm_since.get_or_insert(now);
        if now.saturating_sub(since) >= STEP_MS && now.saturating_sub(self.last_step) >= STEP_MS {
            self.current = (self.current + STEP).min(self.ceiling);
            self.last_step = now;
        }
        self.current
    }
}

impl Signals {
    /// No command for [`CALM_AFTER_COMMAND_MS`] and GPS not degraded.
    pub(crate) fn calm(&self, now: u64) -> bool {
        let last = self.last_command_ms.load(Ordering::Acquire);
        (last == 0 || now.saturating_sub(last) >= CALM_AFTER_COMMAND_MS) && self.gps_degraded(now).is_none()
    }
}

/// `<time>` of `irec-<time>-<nnn>.csv` / `irec_LAUNCH-<time>-<nnn>.csv` when it
/// is network UTC seconds (the FC uses boot uptime ms before UTC is known).
pub(crate) fn name_utc_seconds(name: &str) -> Option<i64> {
    let stem = name.rsplit_once('.')?.0;
    let mut parts = stem.rsplitn(3, '-');
    let _nnn = parts.next()?;
    let time = parts.next()?.parse::<i64>().ok()?;
    parts.next()?;
    (1_500_000_000..=4_000_000_000).contains(&time).then_some(time)
}

fn name_sequence(name: &str) -> u32 {
    name.rsplit_once('.')
        .and_then(|(stem, _)| stem.rsplit_once('-'))
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

/// The FC launch file of this flight: among `irec_LAUNCH*` files, those dated
/// (by name) between 15 min before the flight capture began and 1 min after
/// the trigger, newest first; without a dated match, the newest dated launch
/// file, else the last one the card lists.
pub(crate) fn choose_launch_file(files: &[RemoteFile], flight_from_ms: Option<i64>, triggered_ms: i64) -> Option<RemoteFile> {
    let launch: Vec<(usize, &RemoteFile)> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| board_sd::classify_name(&f.name) == Some(("FC", true)))
        .collect();
    let key = |(i, f): &(usize, &RemoteFile)| (name_utc_seconds(&f.name).unwrap_or(-1), name_sequence(&f.name), *i);
    let from_s = flight_from_ms.unwrap_or(triggered_ms - 3 * 3_600_000) / 1000 - 15 * 60;
    let to_s = triggered_ms / 1000 + 60;
    let in_flight = launch
        .iter()
        .filter(|(_, f)| name_utc_seconds(&f.name).is_some_and(|s| s >= from_s && s <= to_s))
        .max_by_key(|e| key(e));
    in_flight
        .or_else(|| launch.iter().filter(|(_, f)| name_utc_seconds(&f.name).is_some()).max_by_key(|e| key(e)))
        .or_else(|| launch.last())
        .map(|(_, f)| (*f).clone())
}

// ------------------------------------------------------------ pause signals

/// Command traffic and recovery GPS arrivals, for the pause rules.
pub(crate) struct Signals {
    last_command_ms: AtomicU64,
    gps: Mutex<VecDeque<u64>>,
    quiet_ms: u64,
}

impl Signals {
    pub(crate) const fn new(quiet_ms: u64) -> Self {
        Self { last_command_ms: AtomicU64::new(0), gps: Mutex::new(VecDeque::new()), quiet_ms }
    }
    pub(crate) fn note_command(&self, now: u64) {
        self.last_command_ms.store(now, Ordering::Release);
    }
    pub(crate) fn note_gps(&self, now: u64) {
        let mut gps = self.gps.lock().unwrap_or_else(|e| e.into_inner());
        gps.push_back(now);
        while gps.len() > 4 {
            gps.pop_front();
        }
    }
    pub(crate) fn command_quiet(&self, now: u64) -> Option<String> {
        let last = self.last_command_ms.load(Ordering::Acquire);
        (last != 0 && now.saturating_sub(last) < self.quiet_ms).then(|| "commands were sent recently".into())
    }
    /// Recovery GPS was arriving (3 fixes within 30 s) and has now been
    /// silent for more than 3 s (but less than a minute).
    pub(crate) fn gps_degraded(&self, now: u64) -> Option<String> {
        let gps = self.gps.lock().unwrap_or_else(|e| e.into_inner());
        let last = *gps.back()?;
        let arriving = gps.len() >= 3 && last - gps[gps.len() - 3] <= 30_000;
        let gap = now.saturating_sub(last);
        (arriving && gap > GPS_GAP_MS && gap < GPS_GONE_MS).then(|| format!("recovery GPS has been silent for {} s", gap / 1000))
    }
    pub(crate) fn reason(&self, now: u64) -> Option<String> {
        self.command_quiet(now).or_else(|| self.gps_degraded(now))
    }
}

/// Production signals: commands from `telemetry_task::commands`, GPS from
/// every decoded `GPS_DATA` (RF board, or the FC in Recovery).
pub(crate) static SIGNALS: Signals = Signals::new(leadup::QUIET_AFTER_COMMAND.as_millis() as u64);

/// Called for every decoded `GPS_DATA` row.
pub(crate) fn note_gps() {
    SIGNALS.note_gps(now_ms());
}

/// AV-bay radio congestion or a poor link (radio only), checked once a second.
fn radio_reason() -> Option<String> {
    static CACHE: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, reason)) = cache.as_ref()
        && at.elapsed() < Duration::from_secs(1)
    {
        return reason.clone();
    }
    let quality = crate::comms::link_quality::snapshot();
    let budget = crate::telemetry_task::radio_tx_budget_diagnostics();
    let pending = budget.as_ref().and_then(|b| b["pending_bytes"].as_u64()).unwrap_or(0);
    let parked = budget.as_ref().and_then(|b| b["parked_frames"].as_u64()).unwrap_or(0);
    let reason = radio_decision(quality.good_frames_last_minute, quality.crc_failures_last_minute, pending, parked);
    *cache = Some((Instant::now(), reason.clone()));
    reason
}

/// Pure radio rule: poor = at least 10 CRC rejects and 20 % of frames in the
/// last minute; congested = 1.5 KB queued for the radio or 4 parked frames.
pub(crate) fn radio_decision(good: u64, crc: u64, pending_bytes: u64, parked_frames: u64) -> Option<String> {
    if crc >= 10 && crc * 100 >= (good + crc) * 20 {
        return Some(format!("radio link poor ({crc} of {} frames rejected in the last minute)", good + crc));
    }
    if pending_bytes >= 1536 || parked_frames >= 4 {
        return Some("radio link congested".into());
    }
    None
}

// ------------------------------------------------------------------ trigger

/// FC Landed confirmation from the FC's own state reports.
#[derive(Debug, Default, Clone)]
pub(crate) struct Trigger {
    last: Option<FlightState>,
    landed_since: Option<u64>,
    /// An in-flight phase (or Armed) was seen: a new flight re-arms the job.
    saw_flight: bool,
}

impl Trigger {
    pub(crate) fn observe(&mut self, phase: FlightState, now: u64) {
        use FlightState as F;
        match phase {
            F::Landed => {
                if self.last != Some(F::Landed) {
                    self.landed_since = Some(now);
                }
            }
            _ => self.landed_since = None,
        }
        if phase == F::Armed || phase.is_in_flight() || phase == F::Launch {
            self.saw_flight = true;
        }
        self.last = Some(phase);
    }
    /// `recovery` or `landed_confirmed` once the trigger holds.
    pub(crate) fn poll(&self, now: u64) -> Option<&'static str> {
        match (self.last, self.landed_since) {
            (Some(FlightState::Recovery), _) => Some("recovery"),
            (Some(FlightState::Landed), Some(since)) if now.saturating_sub(since) >= LANDED_CONFIRM_MS => Some("landed_confirmed"),
            _ => None,
        }
    }
}

static TRIGGERS: LazyLock<Mutex<HashMap<PathBuf, Trigger>>> = LazyLock::new(Default::default);

pub(crate) fn observe_at(root: &FsPath, phase: FlightState, now: u64) {
    TRIGGERS.lock().unwrap_or_else(|e| e.into_inner()).entry(root.to_path_buf()).or_default().observe(phase, now);
}

/// Every FC state report (`fc_phase::observe`), detailed phase as recorded.
pub(crate) fn observe_phase(state: &AppState, phase: FlightState) {
    observe_at(&crate::recording_export::directory(state), phase, now_ms());
}

// --------------------------------------------------------------- job store

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    #[default]
    List,
    Launch,
    Attach,
    LeadUp,
    Done,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FcJob {
    pub id: u64,
    /// `landed_confirmed` or `recovery`.
    pub trigger: String,
    pub triggered_at_ms: u64,
    /// Start of the flight capture open at the trigger (launch file choice).
    pub flight_from_ms: Option<i64>,
    pub stage: Stage,
    /// waiting, paused, listing, transferring, attaching, waiting_capture,
    /// lead_up, retrying, complete, unavailable.
    pub phase: String,
    pub message: String,
    pub paused_reason: Option<String>,
    pub launch_file: Option<String>,
    pub launch_size: u64,
    pub launch_open: bool,
    pub open_seen_ms: Option<u64>,
    pub listed_ms: u64,
    pub list_attempts: u32,
    pub manager_job: Option<u64>,
    pub crc_errors: u64,
    pub link: String,
    pub retries: u32,
    pub next_attempt_ms: u64,
    pub downloaded_at_ms: Option<u64>,
    pub sha256: Option<String>,
    pub capture: Option<String>,
    pub lead_up_rows: u64,
    pub done: bool,
    pub completed_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FcState {
    /// A Landed/Recovery trigger may create a job (cleared when it does,
    /// set again when the FC reports a new flight).
    pub armed: bool,
    /// Operator stop: persists until resumed.
    pub stopped: bool,
    pub stopped_by: Option<String>,
    pub next_id: u64,
    pub jobs: Vec<FcJob>,
    /// The job whose "AV bay can be powered off" notice was given.
    pub power_off_notified: Option<u64>,
}

impl Default for FcState {
    fn default() -> Self {
        Self { armed: true, stopped: false, stopped_by: None, next_id: 1, jobs: Vec::new(), power_off_notified: None }
    }
}

/// GS: the RRLS forwarded this flight's launch file (SHA-256 verified): the
/// direct job is done; a lead-up the RRLS cut follows through the relay.
pub(crate) fn relay_received(root: &FsPath, name: &str, sha: &str, capture: Option<&str>) {
    modify(root, |s| {
        let Some(job) = s.jobs.iter_mut().rev().find(|j| !j.done && j.launch_file.as_deref().is_none_or(|n| n == name)) else { return };
        job.launch_file = Some(name.into());
        job.sha256 = Some(sha.into());
        job.capture = capture.map(Into::into);
        job.done = true;
        job.stage = Stage::Done;
        job.phase = "complete".into();
        job.paused_reason = None;
        job.message = format!("{name} received through the RRLS (SHA-256 verified)");
        job.completed_at_ms = Some(now_ms());
    });
}

static STORE_LOCK: Mutex<()> = Mutex::new(());

fn store_dir(root: &FsPath) -> PathBuf {
    root.join("sd-downloads").join("FC")
}

pub(crate) fn load(root: &FsPath) -> FcState {
    std::fs::read(store_dir(root).join(JOBS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(root: &FsPath, state: &FcState) {
    let dir = store_dir(root);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(JOBS_FILE);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(state).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

fn modify<T>(root: &FsPath, apply: impl FnOnce(&mut FcState) -> T) -> T {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load(root);
    let before = state.clone();
    let out = apply(&mut state);
    if state != before {
        save(root, &state);
    }
    out
}

fn update_job(root: &FsPath, id: u64, apply: impl FnOnce(&mut FcJob)) {
    modify(root, |s| {
        if let Some(job) = s.jobs.iter_mut().find(|j| j.id == id) {
            apply(job);
        }
    });
}

/// In-memory operator stop per data root (mirrors the persisted flag so the
/// transfer gate can poll it every 100 ms without reading the file).
fn stop_flag(root: &FsPath) -> Arc<AtomicBool> {
    static FLAGS: LazyLock<Mutex<HashMap<PathBuf, Arc<AtomicBool>>>> = LazyLock::new(Default::default);
    FLAGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(root.to_path_buf())
        .or_insert_with(|| Arc::new(AtomicBool::new(load(root).stopped)))
        .clone()
}

pub(crate) fn set_stopped(root: &FsPath, stopped: bool, by: Option<String>) -> FcState {
    stop_flag(root).store(stopped, Ordering::Release);
    modify(root, |s| {
        s.stopped = stopped;
        s.stopped_by = if stopped { by } else { None };
        s.clone()
    })
}

/// An FC launch file is still owed (the DAQ lead-up yields to it).
pub(crate) fn launch_file_owed(root: &FsPath) -> bool {
    let state = load(root);
    !state.stopped && state.jobs.iter().any(|j| !j.done && matches!(j.stage, Stage::List | Stage::Launch))
}

/// Applies the trigger: creates a job when Landed is confirmed or Recovery is
/// reported, the setting is on and the latch is armed. Returns the new job.
pub(crate) fn evaluate_at(root: &FsPath, enabled: bool, flight_from_ms: Option<i64>, link: Link, now: u64) -> Option<FcJob> {
    let (fired, saw_flight) = {
        let mut triggers = TRIGGERS.lock().unwrap_or_else(|e| e.into_inner());
        let trigger = triggers.entry(root.to_path_buf()).or_default();
        let saw = std::mem::take(&mut trigger.saw_flight);
        (trigger.poll(now), saw)
    };
    if !saw_flight && fired.is_none() {
        return None;
    }
    modify(root, |s| {
        if saw_flight {
            s.armed = true;
        }
        let kind = fired?;
        if !enabled || !s.armed {
            return None;
        }
        s.armed = false;
        if s.jobs.iter().any(|j| !j.done) {
            return None;
        }
        let id = s.next_id.max(1);
        s.next_id = id + 1;
        let job = FcJob {
            id,
            trigger: kind.into(),
            triggered_at_ms: now,
            flight_from_ms,
            phase: "waiting".into(),
            message: format!("FC {} confirmed: collecting its SD launch file", if kind == "recovery" { "Recovery" } else { "Landed" }),
            link: link.as_str().into(),
            ..FcJob::default()
        };
        // Bounded history.
        if s.jobs.len() >= 20 {
            s.jobs.remove(0);
        }
        s.jobs.push(job.clone());
        log::info!("FC SD collection queued ({kind})");
        Some(job)
    })
}

// ------------------------------------------------------------------ worker

type PauseFn = Arc<dyn Fn(Stage) -> Option<String> + Send + Sync>;

/// Everything the worker needs; production builds it from `AppState`, tests
/// from fakes.
#[derive(Clone)]
pub(crate) struct Env {
    pub manager: &'static Manager,
    pub router: Arc<Router>,
    pub root: PathBuf,
    pub link: Link,
    pub rate: u64,
    pub leadup: bool,
    /// Why the collection must not use the link now (all rules but the stop).
    pub pause: PauseFn,
    /// A launch, HITL or ground-test capture is open or being saved.
    pub capture_pending: Arc<dyn Fn() -> bool + Send + Sync>,
    pub notify: Arc<dyn Fn(String) + Send + Sync>,
    pub alive: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The pacing rate, read about every 500 ms (adaptive on the radio).
    pub rate_fn: Option<RateFn>,
    /// Worker poll period while waiting or paused.
    pub poll: Duration,
    /// Backoff unit: retry n waits min(5 * 2^(n-1) units, 60 s worth).
    pub backoff_unit_ms: u64,
    /// A downloaded file without a matching capture waits this long for one.
    pub capture_wait_ms: u64,
    /// Integrated ground test: the FC runs its `--ground-test` image, which
    /// has no SD read service (flash), so a refused or silent stream ends
    /// the job with a clear status instead of retrying forever.
    pub igt: bool,
    /// In IGT, a listing that has not answered within this long counts as
    /// "no service" (a silent stream would otherwise retry for minutes).
    pub igt_list_timeout: Duration,
    /// RRLS role: keep each verified file in the relay outbox for the GS.
    pub outbox: bool,
}

impl Env {
    fn reason(&self, stage: Stage) -> Option<String> {
        if stop_flag(&self.root).load(Ordering::Acquire) {
            return Some("stopped by the operator".into());
        }
        (self.pause)(stage)
    }
    fn gate(&self, stage: Stage) -> Gate {
        let env = self.clone();
        Arc::new(move || env.reason(stage))
    }
    fn downloads(&self) -> PathBuf {
        self.root.join("sd-downloads")
    }
}

enum Step {
    Progress,
    Waiting(String),
    Paused(String),
    Failed(String),
}

impl From<StreamError> for Step {
    fn from(error: StreamError) -> Self {
        match error {
            StreamError::Gate(reason) => Self::Paused(reason),
            StreamError::Cancelled => Self::Paused("cancelled".into()),
            StreamError::Other(e) => Self::Failed(e),
        }
    }
}

static RUNNING: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

pub(crate) fn worker_running(root: &FsPath) -> bool {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).contains(root)
}

/// Works the unfinished jobs to completion, one worker per data root.
pub(crate) async fn run(env: Env) {
    if !RUNNING.lock().unwrap_or_else(|e| e.into_inner()).insert(env.root.clone()) {
        return;
    }
    struct Running(PathBuf);
    impl Drop for Running {
        fn drop(&mut self) {
            RUNNING.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
        }
    }
    let _running = Running(env.root.clone());
    let root = env.root.clone();
    while (env.alive)() {
        let Some(job) = load(&root).jobs.into_iter().find(|j| !j.done) else { return };
        let now = now_ms();
        if now < job.next_attempt_ms {
            tokio::time::sleep(env.poll.min(Duration::from_millis(job.next_attempt_ms - now))).await;
            continue;
        }
        let blocked = env.reason(job.stage).or_else(|| {
            (job.stage != Stage::LeadUp && env.manager.active()).then(|| "another SD transfer is running".to_string())
        });
        let step = match blocked {
            Some(reason) => Step::Paused(reason),
            None => match job.stage {
                Stage::List => list_step(&env, &job).await,
                Stage::Launch => launch_step(&env, &job).await,
                Stage::Attach => attach_step(&env, &job).await,
                Stage::LeadUp => leadup_step(&env, &job).await,
                Stage::Done => {
                    update_job(&root, job.id, |j| j.done = true);
                    Step::Progress
                }
            },
        };
        match step {
            Step::Progress => update_job(&root, job.id, |j| {
                j.retries = 0;
                j.paused_reason = None;
            }),
            Step::Waiting(message) => {
                update_job(&root, job.id, |j| {
                    j.paused_reason = None;
                    j.message = message;
                });
                tokio::time::sleep(env.poll).await;
            }
            Step::Paused(reason) => {
                update_job(&root, job.id, |j| {
                    if j.phase != "paused" || j.paused_reason.as_deref() != Some(reason.as_str()) {
                        j.phase = "paused".into();
                        j.message = format!("paused: {reason}");
                        j.paused_reason = Some(reason);
                    }
                });
                tokio::time::sleep(env.poll).await;
            }
            Step::Failed(error) => {
                let unit = env.backoff_unit_ms;
                update_job(&root, job.id, |j| {
                    j.retries = j.retries.saturating_add(1);
                    let wait = (5 * unit).saturating_mul(1 << (j.retries - 1).min(8)).min(MAX_BACKOFF_MS * unit / 1000);
                    j.next_attempt_ms = now_ms() + wait;
                    j.phase = "retrying".into();
                    j.paused_reason = None;
                    j.message = format!("{error}; retrying in {} s", wait.div_ceil(1000));
                });
                log::warn!("FC SD collection: {error}");
            }
        }
    }
}

async fn list_step(env: &Env, job: &FcJob) -> Step {
    update_job(&env.root, job.id, |j| {
        j.phase = "listing".into();
        j.message = "listing the FC SD card".into();
    });
    let listing = refresh_listing(env.manager, env.router.clone(), Board::FlightComputer, env.gate(Stage::List));
    let listing = if env.igt {
        tokio::time::timeout(env.igt_list_timeout, listing)
            .await
            .unwrap_or_else(|_| Err("no answer on the SD stream".into()))
    } else {
        listing.await
    };
    let listing = match listing {
        Ok(listing) => listing,
        Err(error) if env.reason(Stage::List).is_some() => return Step::Paused(error),
        Err(error) if env.igt => {
            let message = format!("FC SD service not available in the ground-test image ({error}); import the card by hand");
            complete(env, job.id, "unavailable", &message);
            (env.notify)(format!("FC SD collection: {message}."));
            return Step::Progress;
        }
        Err(error) => return Step::Failed(format!("FC SD listing failed: {error}")),
    };
    let Some(file) = choose_launch_file(&listing.files, job.flight_from_ms, job.triggered_at_ms as i64) else {
        let attempts = job.list_attempts + 1;
        if attempts >= 3 {
            let message = "no irec_LAUNCH file on the FC SD card (listed 3 times)".to_string();
            complete(env, job.id, "unavailable", &message);
            (env.notify)(format!("FC SD collection: {message}."));
            return Step::Progress;
        }
        update_job(&env.root, job.id, |j| j.list_attempts = attempts);
        return Step::Failed("no FC launch file on the card yet".into());
    };
    let now = now_ms();
    update_job(&env.root, job.id, |j| {
        j.launch_file = Some(file.name.clone());
        j.launch_size = file.size;
        j.launch_open = file.open;
        j.open_seen_ms = file.open.then_some(now);
        j.listed_ms = now;
        j.stage = Stage::Launch;
        j.phase = "transferring".into();
        j.message = format!("found {} ({} bytes)", file.name, file.size);
    });
    Step::Progress
}

async fn launch_step(env: &Env, job: &FcJob) -> Step {
    let Some(name) = job.launch_file.clone() else {
        update_job(&env.root, job.id, |j| j.stage = Stage::List);
        return Step::Progress;
    };
    let now = now_ms();
    if job.launch_open && now.saturating_sub(job.open_seen_ms.unwrap_or(now)) < OPEN_FILE_WAIT_MS {
        if now.saturating_sub(job.listed_ms) >= 10_000 {
            let listing = refresh_listing(env.manager, env.router.clone(), Board::FlightComputer, env.gate(Stage::Launch)).await;
            match listing {
                Ok(listing) => {
                    let entry = listing.files.iter().find(|f| f.name == name).cloned();
                    update_job(&env.root, job.id, |j| {
                        j.listed_ms = now;
                        if let Some(f) = entry {
                            j.launch_open = f.open;
                            j.launch_size = f.size;
                        }
                    });
                }
                Err(error) => return Step::Failed(format!("FC SD listing failed: {error}")),
            }
        }
        update_job(&env.root, job.id, |j| j.phase = "waiting".into());
        return Step::Waiting(format!("the FC is still writing {name}; waiting for it to close the file"));
    }
    let (id, cancel) = match env.manager.create_job(Board::FlightComputer, "ROOT", &name) {
        Ok(created) => created,
        Err(error) => return Step::Paused(error),
    };
    update_job(&env.root, job.id, |j| {
        j.manager_job = Some(id);
        j.phase = "transferring".into();
        j.message = format!("collecting {name} over the {} link", env.link.as_str());
    });
    let mut req = TransferRequest::new(id, Board::FlightComputer, 0, &name, env.downloads(), env.rate);
    req.attach = false;
    req.profile = profile(env.link);
    req.rate_fn = env.rate_fn.clone();
    req.on_pause = Some(pause_hook(env, job.id));
    let result = transfer(env.manager, env.router.clone(), &req, &env.gate(Stage::Launch), &cancel).await;
    let crc = req.crc_errors.load(Ordering::Relaxed);
    if result.is_err() {
        env.manager.update(id, |s| {
            if s.phase.is_active() {
                s.phase = Phase::Paused;
                s.message = "paused; resumes automatically".into();
            }
        });
    }
    update_job(&env.root, job.id, |j| j.crc_errors += crc);
    match result {
        Ok(()) => {
            let size = completed_file(&env.downloads(), "FC", "ROOT", &name)
                .and_then(|p| std::fs::metadata(p).ok())
                .map_or(0, |m| m.len());
            update_job(&env.root, job.id, |j| {
                j.stage = Stage::Attach;
                j.downloaded_at_ms = Some(now_ms());
                j.launch_size = size;
                j.phase = "attaching".into();
                j.message = format!("{name} received ({size} bytes)");
            });
            Step::Progress
        }
        Err(error) => error.into(),
    }
}

async fn attach_step(env: &Env, job: &FcJob) -> Step {
    let Some(name) = job.launch_file.clone() else { return Step::Failed("no launch file".into()) };
    let Some(path) = completed_file(&env.downloads(), "FC", "ROOT", &name) else {
        update_job(&env.root, job.id, |j| j.stage = Stage::Launch);
        return Step::Failed("the downloaded FC file is missing; fetching it again".into());
    };
    if env.outbox && !relay::load_outbox(&env.root).iter().any(|e| e.name == name) {
        if let Err(error) = relay::register(&env.root, &path, &name, "launch", &name, job.crc_errors, !env.leadup) {
            return Step::Failed(format!("could not keep {name} for the GS: {error}"));
        }
        (env.notify)(format!("FC SD launch file {name} received by the RRLS, checksums verified; forwarding it to the GS when the link allows."));
    }
    let root = env.root.clone();
    let provenance = board_sd::Provenance { source: "network_download".into(), link: Some(env.link.as_str().into()), relay: None };
    let (offer_name, offer_path) = (name.clone(), path.clone());
    let outcome = tokio::task::spawn_blocking(move || board_sd::offer_file_with(&root, &offer_path, &offer_name, &provenance)).await;
    let outcomes = match outcome {
        Ok(Ok(outcomes)) => outcomes,
        Ok(Err(error)) => return Step::Failed(format!("could not attach {name}: {error}")),
        Err(error) => return Step::Failed(format!("could not attach {name}: {error}")),
    };
    let size = job.launch_size;
    if let Some(folder) = outcomes.iter().find_map(|o| match o {
        board_sd::Outcome::Attached(folder) => Some(folder.clone()),
        _ => None,
    }) {
        let manifest = crate::recording_store::read_manifest(&folder);
        let entry = manifest.as_ref().and_then(|m| m.board_sd.iter().find(|e| e.board == "FC" && e.file == name).cloned());
        let capture = folder.strip_prefix(&env.root).unwrap_or(&folder).to_string_lossy().into_owned();
        let label = manifest.as_ref().map_or_else(|| capture.clone(), |m| m.label.clone());
        let has_lead_up = entry.as_ref().is_some_and(|e| e.lead_up_file.is_some());
        (env.notify)(format!(
            "FC SD launch file {name} ({size} bytes) collected over the {} link and attached to the {label} capture{}.",
            env.link.as_str(),
            if env.leadup && !has_lead_up { "; fetching its 60 s lead-up next" } else { "" }
        ));
        let next_lead_up = env.leadup && !has_lead_up && entry.is_some();
        if let (true, Some(entry)) = (next_lead_up, entry.as_ref()) {
            leadup::enqueue_board(&env.root, "FC", &folder, &name, entry.data_from_ms);
        }
        update_job(&env.root, job.id, |j| {
            j.capture = Some(capture.clone());
            j.sha256 = entry.as_ref().map(|e| e.sha256.clone());
            j.lead_up_rows = entry.as_ref().map_or(0, |e| e.lead_up_rows);
        });
        if next_lead_up {
            update_job(&env.root, job.id, |j| {
                j.stage = Stage::LeadUp;
                j.phase = "lead_up".into();
                j.message = "launch file attached; fetching the 60 s lead-up".into();
            });
        } else {
            if env.outbox {
                relay::resolve_lead_up(&env.root, &name);
            }
            complete(env, job.id, "complete", &format!("{name} attached to {label}"));
        }
        return Step::Progress;
    }
    if outcomes.contains(&board_sd::Outcome::Ignored) {
        let message = format!("{name} has no network-UTC rows; kept in sd-downloads/FC/ROOT (import it by hand)");
        complete(env, job.id, "complete", &message);
        (env.notify)(format!("FC SD launch file collected: {message}."));
        return Step::Progress;
    }
    let waited = now_ms().saturating_sub(job.downloaded_at_ms.unwrap_or(0));
    if (env.capture_pending)() || waited < env.capture_wait_ms {
        update_job(&env.root, job.id, |j| j.phase = "waiting_capture".into());
        return Step::Waiting(format!("{name} received; waiting for the flight capture to be saved"));
    }
    let message = format!("{name} received; no capture covers it (kept in sd-downloads/FC/ROOT, attached when one is saved)");
    if env.outbox {
        relay::resolve_lead_up(&env.root, &name);
    }
    complete(env, job.id, "complete", &message);
    (env.notify)(format!("FC SD launch file collected: {message}."));
    Step::Progress
}

async fn leadup_step(env: &Env, job: &FcJob) -> Step {
    let Some(name) = job.launch_file.clone() else { return Step::Failed("no launch file".into()) };
    let Some(lead) = leadup::load_board(&env.root, "FC").into_iter().find(|j| j.launch_file == name) else {
        complete(env, job.id, "complete", "launch file attached (no lead-up job)");
        return Step::Progress;
    };
    if !lead.done {
        update_job(&env.root, job.id, |j| {
            j.phase = "lead_up".into();
            j.message = format!("lead-up: {}", lead.message);
        });
        let params = leadup::Params { rate: env.rate, whole_file_bytes: 0, bytes_per_ms: FC_PAD_BYTES_PER_MS, profile: profile(env.link) };
        if let Err(error) = leadup::step_with(env.manager, &env.root, env.router.clone(), &lead, &env.gate(Stage::LeadUp), params).await {
            return error.into();
        }
    }
    let lead = leadup::load_board(&env.root, "FC").into_iter().find(|j| j.launch_file == name).unwrap_or_default();
    if !lead.done {
        return Step::Waiting(format!("lead-up: {}", lead.message));
    }
    let rows = job
        .capture
        .as_ref()
        .and_then(|c| crate::recording_store::read_manifest(&env.root.join(c)))
        .and_then(|m| m.board_sd.into_iter().find(|e| e.board == "FC" && e.file == name))
        .map_or(0, |e| e.lead_up_rows);
    update_job(&env.root, job.id, |j| j.lead_up_rows = rows);
    if env.outbox {
        let lead_name = format!("{}.leadup.csv", name.trim_end_matches(".csv").trim_end_matches(".CSV"));
        let lead_path = job.capture.as_ref().map(|c| env.root.join(c).join(board_sd::BOARD_SD_DIR).join("FC").join(&lead_name));
        if let Some(path) = lead_path.filter(|p| p.exists())
            && let Err(error) = relay::register(&env.root, &path, &lead_name, "lead_up", &name, 0, true)
        {
            return Step::Failed(format!("could not keep the lead-up for the GS: {error}"));
        }
        relay::resolve_lead_up(&env.root, &name);
    }
    let message = format!("{name} attached; lead-up {}", lead.message);
    complete(env, job.id, "complete", &message);
    (env.notify)(format!("FC SD collection complete: {message}."));
    Step::Progress
}

/// Shows a pause of the open transfer in the job (the stream stays open).
fn pause_hook(env: &Env, id: u64) -> PauseHook {
    let root = env.root.clone();
    let manager = env.manager;
    Arc::new(move |reason: Option<&str>| {
        update_job(&root, id, |j| match reason {
            Some(reason) => {
                j.phase = "paused".into();
                j.message = format!("paused: {reason}");
                j.paused_reason = Some(reason.into());
            }
            None => {
                j.phase = "transferring".into();
                j.message = "transferring".into();
                j.paused_reason = None;
            }
        });
        if let Some(job) = load(&root).jobs.iter().find(|j| j.id == id).and_then(|j| j.manager_job) {
            manager.update(job, |s| s.message = reason.map_or_else(|| "transferring".into(), |r| format!("paused: {r}")));
        }
    })
}

fn complete(env: &Env, id: u64, phase: &str, message: &str) {
    update_job(&env.root, id, |j| {
        j.done = true;
        j.stage = Stage::Done;
        j.phase = phase.into();
        j.message = message.into();
        j.paused_reason = None;
        j.completed_at_ms = Some(now_ms());
    });
}

// -------------------------------------------------------------- production

fn production_env(state: &Arc<AppState>, root: &FsPath, router: Arc<Router>) -> Env {
    let link = link(state);
    let weak = Arc::downgrade(state);
    let pause_root = root.to_path_buf();
    let pause: PauseFn = Arc::new(move |stage| {
        let Some(state) = weak.upgrade() else { return Some("ground station stopped".into()) };
        if state.shutdown_requested.load(Ordering::Acquire) {
            return Some("ground station is shutting down".into());
        }
        if !state.recording_store.settings().fc_sd_auto_collect {
            return Some("automatic FC SD collection is turned off (administrator setting)".into());
        }
        if let Some(reason) = station_gate(&state) {
            return Some(reason);
        }
        if launch_fetch_owed() {
            return Some("the DAQ launch file fetch goes first".into());
        }
        if stage == Stage::LeadUp {
            if MANAGER.active() {
                return Some("another SD transfer is running".into());
            }
            if leadup::transferring(&pause_root) {
                return Some("the DAQ lead-up fetch goes first".into());
            }
        }
        if let Some(reason) = SIGNALS.reason(now_ms()) {
            return Some(reason);
        }
        if crate::rrls::is_rrls() {
            // A standby RRLS keeps its radio silent; it fetches only while it
            // owns the AV bay, and only over a clean link.
            let quality = crate::comms::link_quality::snapshot();
            let hears = state.network_health.lock().ok().and_then(|t| t.connected_for("FC", Instant::now())).is_some();
            let metrics = relay::PathMetrics {
                gs_direct_age_ms: None,
                rrls_online: true,
                rrls_hears_av_bay: hears,
                rrls_good: quality.good_frames_last_minute,
                rrls_crc: quality.crc_failures_last_minute,
                rrls_can_transmit: crate::rrls::takeover_active(),
                ..Default::default()
            };
            if let (relay::Path::Direct, why) = relay::choose_path(&metrics) {
                return Some(format!("RRLS does not fetch now: {why}"));
            }
        } else if relay::rrls_holds_launch(&pause_root) {
            return Some("the RRLS holds the FC launch file; it is forwarded from there".into());
        }
        if link == Link::Radio {
            return radio_reason();
        }
        None
    });
    let rate_fn: Option<RateFn> = (link == Link::Radio).then(|| {
        let adaptive = Mutex::new(AdaptiveRate::new(rate(link), radio_ceiling()));
        let weak = Arc::downgrade(state);
        Arc::new(move |losses: u32| {
            let now = now_ms();
            let landed = weak.upgrade().is_some_and(|s| {
                s.fc_phase.snapshot().is_some_and(|o| matches!(o.phase, FlightState::Landed | FlightState::Recovery))
            });
            let budget = crate::telemetry_task::radio_tx_budget_diagnostics();
            let queue_quiet = budget.as_ref().is_none_or(|b| {
                b["pending_bytes"].as_u64().unwrap_or(0) < 512 && b["parked_frames"].as_u64().unwrap_or(0) == 0
            });
            let calm = landed && SIGNALS.calm(now) && radio_reason().is_none() && queue_quiet;
            adaptive.lock().unwrap_or_else(|e| e.into_inner()).update(now, calm, losses)
        }) as RateFn
    });
    let captures = Arc::downgrade(state);
    let notifier = Arc::downgrade(state);
    let alive = Arc::downgrade(state);
    Env {
        manager: &MANAGER,
        router,
        root: root.to_path_buf(),
        link,
        rate: rate(link),
        leadup: state.recording_store.settings().fc_sd_leadup,
        pause,
        capture_pending: Arc::new(move || {
            captures.upgrade().is_some_and(|s| {
                s.recording_store.briefs().iter().any(|b| {
                    matches!(
                        b.kind,
                        crate::recording_store::EventKind::Launch
                            | crate::recording_store::EventKind::Hitl
                            | crate::recording_store::EventKind::GroundTest
                    )
                })
            })
        }),
        notify: Arc::new(move |message| {
            if let Some(state) = notifier.upgrade() {
                log::info!("{message}");
                state.add_notification_with_persistence(message, true);
            }
        }),
        alive: Arc::new(move || alive.upgrade().is_some_and(|s| !s.shutdown_requested.load(Ordering::Acquire))),
        rate_fn,
        poll: Duration::from_secs(1),
        backoff_unit_ms: 1000,
        capture_wait_ms: 120_000,
        igt: crate::runtime_mode::is_igt(),
        igt_list_timeout: Duration::from_secs(30),
        outbox: crate::rrls::is_rrls(),
    }
}

fn flight_from(state: &AppState) -> Option<i64> {
    state
        .recording_store
        .briefs()
        .iter()
        .filter(|b| {
            matches!(
                b.kind,
                crate::recording_store::EventKind::Launch
                    | crate::recording_store::EventKind::Hitl
                    | crate::recording_store::EventKind::GroundTest
            )
        })
        .map(|b| b.event_start_ms)
        .min()
}

/// Once a second from the capture task: confirm the trigger and keep a
/// worker running while a job is unfinished (also after a restart).
pub(crate) fn tick(state: &Arc<AppState>, root: &FsPath) {
    let settings = state.recording_store.settings();
    if evaluate_at(root, settings.fc_sd_auto_collect, flight_from(state), link(state), now_ms()).is_some() {
        state.add_temporary_notification("FC landed: collecting its SD launch file in the background (lowest priority).");
    }
    let Some(router) = state.topology_router.get().cloned() else { return };
    let settings_leadup = settings.fc_sd_leadup;
    if crate::rrls::is_rrls() {
        static SERVING: AtomicBool = AtomicBool::new(false);
        if !SERVING.swap(true, Ordering::AcqRel)
            && let Err(error) = relay::serve(&router, root.to_path_buf(), None)
        {
            log::warn!("RRLS FC SD relay outbox not served: {error}");
            SERVING.store(false, Ordering::Release);
        }
    } else if router.resolve_hostname(relay::RRLS_HOST).is_some() {
        relay::spawn_pull_loop(state, root, router.clone());
    }
    let store = load(root);
    let power = relay::power_off_for(store.jobs.last(), settings_leadup, &relay::load_state(root));
    if power.ready
        && let Some(job) = store.jobs.last()
        && store.power_off_notified != Some(job.id)
    {
        modify(root, |s| s.power_off_notified = Some(job.id));
        state.add_notification_with_persistence(format!("{}.", power.message), true);
    }
    if worker_running(root) || !store.jobs.iter().any(|j| !j.done) {
        return;
    }
    tokio::spawn(run(production_env(state, root, router)));
}

/// The production pull environment (shares the direct path's pause rules).
pub(crate) fn pull_env(state: &Arc<AppState>, root: &FsPath, router: Arc<Router>) -> relay::PullEnv {
    let env = production_env(state, root, router.clone());
    relay::PullEnv {
        manager: env.manager,
        router,
        root: root.to_path_buf(),
        rate: env.rate,
        rate_fn: env.rate_fn.clone(),
        gate: {
            let env = env.clone();
            Arc::new(move || env.reason(Stage::Launch).filter(|r| !r.starts_with("the RRLS holds")))
        },
        notify: env.notify.clone(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub enabled: bool,
    pub lead_up_enabled: bool,
    pub stopped: bool,
    pub stopped_by: Option<String>,
    pub link: Link,
    pub rate_bytes_per_sec: u64,
    pub active: Option<ActiveStatus>,
    pub jobs: Vec<FcJob>,
    /// "AV bay can be powered off" readiness (GS or RRLS verified copy).
    pub power_off: relay::PowerOff,
    /// RRLS -> GS forward progress: (file, bytes done, total), message.
    pub forward: Option<(String, u64, u64)>,
    pub forward_message: String,
    /// Files waiting in the RRLS outbox (as last listed).
    pub rrls_outbox: Vec<relay::OutboxEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveStatus {
    pub id: u64,
    pub state: String,
    pub file: Option<String>,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub bytes_per_second: u64,
    pub paused_reason: Option<String>,
    pub eta_seconds: Option<u64>,
    pub link: String,
    pub crc_errors: u64,
    pub message: String,
}

pub(crate) fn status_at(root: &FsPath, manager: &Manager, enabled: bool, lead_up_enabled: bool, link: Link) -> Status {
    let store = load(root);
    let rate = rate(link);
    let active = store.jobs.iter().rev().find(|j| !j.done).map(|job| {
        let live = job.manager_job.and_then(|id| manager.get(id)).filter(|s| s.phase.is_active());
        let (mut done, mut total) = (0, job.launch_size);
        let mut measured = 0;
        if let Some(name) = &job.launch_file {
            let (fin, part) = local_paths(&root.join("sd-downloads"), Board::FlightComputer, "ROOT", name);
            done = std::fs::metadata(&part).or_else(|_| std::fs::metadata(&fin)).map_or(0, |m| m.len());
        }
        if job.stage == Stage::LeadUp
            && let Some(lead) = job.launch_file.as_ref().and_then(|n| leadup::load_board(root, "FC").into_iter().find(|l| &l.launch_file == n))
        {
            done = lead.fetched;
            total = ((lead.to_ms - lead.from_ms).max(0) as u64) * FC_PAD_BYTES_PER_MS;
        }
        if let Some(live) = &live {
            done = live.bytes_done;
            total = live.total_bytes.max(total);
            measured = live.bytes_per_second;
        }
        let paused = job.paused_reason.clone();
        // `rate` counts DATA bytes; SDX carries about 5 CSV bytes per DATA
        // byte, so that is the estimate until a rate is measured.
        let eta = (paused.is_none() && total > done).then(|| (total - done) / if measured > 0 { measured } else { rate * 5 }.max(1));
        ActiveStatus {
            id: job.id,
            state: job.phase.clone(),
            file: job.launch_file.clone(),
            bytes_done: done,
            total_bytes: total,
            bytes_per_second: measured,
            paused_reason: paused,
            eta_seconds: eta,
            link: job.link.clone(),
            crc_errors: job.crc_errors + live.map_or(0, |l| l.crc_errors),
            message: job.message.clone(),
        }
    });
    let relay_state = relay::load_state(root);
    let power_off = relay::power_off_for(store.jobs.last(), lead_up_enabled, &relay_state);
    Status {
        enabled,
        lead_up_enabled,
        stopped: store.stopped,
        stopped_by: store.stopped_by,
        link,
        rate_bytes_per_sec: rate,
        active,
        jobs: store.jobs.into_iter().rev().take(5).collect(),
        power_off,
        forward: relay_state.forwarding,
        forward_message: relay_state.message,
        rrls_outbox: if crate::rrls::is_rrls() { relay::load_outbox(root) } else { relay_state.rrls_outbox },
    }
}

pub(crate) fn status(state: &AppState) -> Status {
    let settings = state.recording_store.settings();
    status_at(
        &crate::recording_export::directory(state),
        &MANAGER,
        settings.fc_sd_auto_collect,
        settings.fc_sd_leadup,
        link(state),
    )
}

pub(crate) async fn status_route(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    Json(status(&state)).into_response()
}

async fn set_stop_route(state: Arc<AppState>, headers: HeaderMap, stopped: bool) -> Response {
    if let Err(e) = command_principal(&state, &headers).await {
        return e;
    }
    let who = crate::web::authorize_headers(&state, &headers, Permission::SendCommands)
        .await
        .ok()
        .and_then(|p| p.username.clone());
    let root = crate::recording_export::directory(&state);
    set_stopped(&root, stopped, who.clone());
    log::warn!(
        "FC SD collection {} by {}",
        if stopped { "stopped" } else { "resumed" },
        who.as_deref().unwrap_or("?")
    );
    Json(status(&state)).into_response()
}

pub(crate) async fn stop_route(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    set_stop_route(state, headers, true).await
}

pub(crate) async fn resume_route(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    set_stop_route(state, headers, false).await
}

#[cfg(test)]
#[path = "fc_tests.rs"]
mod tests;
