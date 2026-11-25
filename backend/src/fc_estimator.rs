//! The Flight Computer's flight estimator (Kalman filter) status.
//!
//! Launch resumes the FC's evaluator, which runs the ascent Kalman filter.
//! The FC reports the filter in its BOARD_HEALTH (FlightComputer26
//! `fc_estimator_status.h`), words other boards use only with word 1 flags 4
//! and 64, which the FC never sets:
//!
//! - word 13: bits 24..31 the 'K' tag (0x4B), bits 20..23 the phase (0 idle,
//!   1 initializing, 2 running, 3 converged, 4 paused, 5 stopped,
//!   6 diverged), bits 0..19 the completed filter cycles this flight (wraps);
//! - word 15: bits 0..17 the FC uptime tick (word 2's low word, word 4 Hz)
//!   the filter started running at, in 16-tick units modulo 2^18 (70 min at
//!   1 kHz); 0 before it has run. Bits 18..31 the barometer ground
//!   reference (FlightComputer26 `baro_reference.h`): bits 18..19 its mode
//!   (0 none, 1 tracking, 2 paused, 3 frozen), bits 20..31 its shift since the
//!   FC booted in Pa (signed, saturating at +-2047).
//!
//! The reference pressure itself is the FC's BAROMETER_DATA pressure
//! converted back through the altitude it reports from it (the FC's own
//! hypsometric formula), so it needs no wire field. The FC tracks it against
//! weather and sensor drift while the vehicle sits still on the pad, pauses
//! while it is handled, and freezes it at Launch:
//! "Baro ref 1003.2 hPa · drift +1.8 hPa · tracking".
//!
//! The FC sends a report at once on a phase change and again 1 s later, then
//! every 5 s. The station raises a notification when the filter starts
//! ("FC flight estimator running (Kalman filter started at T+0.3 s)", T being
//! the accepted Launch), and a warning when Launch was accepted but no
//! running report came within [`START_TIMEOUT`], when the filter is stopped
//! or diverged after Launch, or when its cycle counter stops advancing. A
//! test fire has no FC, so nothing warns there.
use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// 'K' in FC BOARD_HEALTH word 13 bits 24..31.
pub(crate) const TAG: u32 = 0x4B;
/// Launch accepted and still no running filter after this long: warn. The
/// FC reports the start at once and repeats it 1 s later, so this covers
/// one lost report plus the Launch command's and the report's radio latency.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(3);
/// Two running reports at least this far apart (FC uptime) with the same
/// cycle count: the filter has stopped cycling.
const STALL_SPAN: Duration = Duration::from_secs(1);
/// Older than this, the last report no longer describes the FC (reports
/// every 5 s).
const REPORT_FRESH: Duration = Duration::from_secs(12);
/// Republish ages (time since start, report age) at most this often.
const PUBLISH_EVERY: Duration = Duration::from_secs(1);
const CYCLE_MASK: u32 = 0xF_FFFF;
const START_MASK: u32 = 0x3_FFFF;
const START_SHIFT: u32 = 4;
/// FC baro_reference.h BARO_ISA_T0_L, BARO_ISA_EXP and BARO_ISA_P0.
const ISA_T0_L: f64 = 44_330.77;
const ISA_EXP: f64 = 0.190_263;
const ISA_P0: f64 = 101_325.0;

/// Word 15 as the FC builds it (`FC_EST_WORD15`): the start tick in 16-tick
/// units and the barometer reference bits (mode, shift << 2).
pub(crate) fn word15(start_tick: u32, baro_bits: u32) -> u32 {
    let mut start = (start_tick >> START_SHIFT) & START_MASK;
    if start_tick != 0 && start == 0 {
        start = 1;
    }
    (baro_bits << 18) | start
}

/// The FC's barometer ground reference mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaroMode {
    /// Following slow weather/sensor drift while the vehicle is still.
    Tracking,
    /// On the pad but not still (handled, a pressure transient): held.
    Paused,
    /// Launch or later: fixed.
    Frozen,
}

impl BaroMode {
    fn from_wire(value: u32) -> Option<Self> {
        match value {
            1 => Some(BaroMode::Tracking),
            2 => Some(BaroMode::Paused),
            3 => Some(BaroMode::Frozen),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            BaroMode::Tracking => "tracking",
            BaroMode::Paused => "paused (not still)",
            BaroMode::Frozen => "frozen",
        }
    }
}

/// Word 15 bits 18..31.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaroRef {
    pub mode: BaroMode,
    /// Reference minus the FC's boot reference (Pa), saturating at +-2047.
    pub shift_pa: i16,
}

/// The FC's barometer ground reference from one BAROMETER_DATA sample
/// (alt m, pressure Pa, temperature C): the pressure its altitude is
/// measured from. `None` for an implausible sample.
pub fn reference_pressure(values: &[f32]) -> Option<f64> {
    use crate::telemetry_schema::baro;
    let (alt, pressure) =
        (f64::from(*values.get(baro::ALTITUDE_M)?), f64::from(*values.get(baro::PRESSURE_PA)?));
    if !(30_000.0..=125_000.0).contains(&pressure) || !(-1_000.0..=10_000.0).contains(&alt) {
        return None;
    }
    // The FC's alt is the ISA pressure altitude of `pressure` minus that of the
    // reference (baro_agl), so the reference's is the sample's minus alt.
    let base = (pressure / ISA_P0).powf(ISA_EXP) + alt / ISA_T0_L;
    if base <= 0.0 {
        return None;
    }
    let reference = ISA_P0 * base.powf(1.0 / ISA_EXP);
    (60_000.0..=125_000.0).contains(&reference).then_some(reference)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Initializing,
    Running,
    Converged,
    Paused,
    Stopped,
    Diverged,
}

impl Phase {
    fn from_wire(value: u32) -> Option<Self> {
        Some(match value {
            0 => Phase::Idle,
            1 => Phase::Initializing,
            2 => Phase::Running,
            3 => Phase::Converged,
            4 => Phase::Paused,
            5 => Phase::Stopped,
            6 => Phase::Diverged,
            _ => return None,
        })
    }
    pub fn running(self) -> bool {
        matches!(self, Phase::Running | Phase::Converged)
    }
    pub fn faulted(self) -> bool {
        matches!(self, Phase::Stopped | Phase::Diverged)
    }
    /// Running, paused or faulted: it has left Idle/Initializing.
    fn started(self) -> bool {
        !matches!(self, Phase::Idle | Phase::Initializing)
    }
    pub fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Initializing => "initializing",
            Phase::Running => "running",
            Phase::Converged => "running (converged)",
            Phase::Paused => "paused",
            Phase::Stopped => "stopped",
            Phase::Diverged => "diverged",
        }
    }
}

/// One decoded FC report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub phase: Phase,
    /// Completed filter cycles, modulo 2^20.
    pub cycles: u32,
    /// FC uptime tick (low word) at the filter's start, to 16 ticks.
    pub start_tick: Option<u32>,
    /// The barometer ground reference; `None` before the FC has one.
    pub baro: Option<BaroRef>,
    pub uptime_tick: u64,
    pub frequency_hz: u32,
}

impl Report {
    /// How long the filter had been running when the FC sent the report.
    pub fn since_start(&self) -> Option<Duration> {
        let start = self.start_tick?;
        let ticks = (self.uptime_tick as u32).wrapping_sub(start);
        Some(Duration::from_secs_f64(f64::from(ticks) / f64::from(self.frequency_hz)))
    }
}

/// The start tick from word 15's 16-tick units: the latest tick at or before
/// the report's uptime that matches them (unambiguous for 70 min at 1 kHz).
fn start_tick(word15: u32, uptime_low: u32) -> Option<u32> {
    let units = word15 & START_MASK;
    if units == 0 {
        return None;
    }
    let span = START_MASK << START_SHIFT | ((1 << START_SHIFT) - 1);
    let since = uptime_low.wrapping_sub(units << START_SHIFT) & span;
    Some(uptime_low.wrapping_sub(since))
}

/// The estimator words of an FC BOARD_HEALTH; `None` for older firmware or
/// another board's use of words 13/15.
pub fn decode(words: &[u32]) -> Option<Report> {
    if words.len() != 24 || words[0] != 1 || words[4] == 0 || words[1] & (4 | 64) != 0 || words[13] >> 24 != TAG {
        return None;
    }
    let baro = words[15] >> 18;
    Some(Report {
        phase: Phase::from_wire((words[13] >> 20) & 0xF)?,
        cycles: words[13] & CYCLE_MASK,
        start_tick: start_tick(words[15], words[2]),
        baro: BaroMode::from_wire(baro & 3).map(|mode| BaroRef {
            mode,
            // Bits 2..13: a signed 12-bit count.
            shift_pa: ((baro << 18) as i32 >> 20) as i16,
        }),
        uptime_tick: u64::from(words[2]) | (u64::from(words[3]) << 32),
        frequency_hz: words[4],
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// An operator notification.
    Info(String),
    /// A warning.
    Warning(String),
}

/// What the station knows besides the report.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    /// Launch was accepted: the station's flight state (or the FC's own) is
    /// Launch or later, not Aborted.
    pub launched: bool,
    /// An FC belongs to this configuration (no FC in a test fire).
    pub expected: bool,
}

#[derive(Clone, Copy, Debug)]
struct Seen {
    report: Report,
    at: Instant,
    network_ms: Option<u64>,
}

impl Seen {
    fn started_at(&self) -> Option<Instant> {
        self.at.checked_sub(self.report.since_start()?)
    }
}

#[derive(Default)]
struct Inner {
    last: Option<Seen>,
    rate_hz: Option<f64>,
    stalled: bool,
    launch: Option<Instant>,
    /// The start tick the running notification was raised for.
    notified_start: Option<u32>,
    warned_no_start: bool,
    warned_fault: Option<Phase>,
    /// No FC in this configuration (test fire); until known, one is expected.
    not_expected: bool,
    published_at: Option<Instant>,
    /// The latest reference pressure from the FC's BAROMETER_DATA.
    baro_reference: Option<(f64, Instant)>,
}

/// `fc_estimator` in the API and WebSocket.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Status {
    /// running, not_running, faulted, unknown (no fresh report) or
    /// not_expected (no FC in this configuration).
    pub state: &'static str,
    /// "FC estimator: running".
    pub label: String,
    pub phase: Option<Phase>,
    pub cycles: Option<u32>,
    /// Filter cycles per second between the last two reports.
    pub rate_hz: Option<f64>,
    /// Time since the filter started, now (estimated from the last report).
    pub running_for_ms: Option<u64>,
    /// When it started relative to the accepted Launch.
    pub started_t_plus_ms: Option<i64>,
    /// When it started on the SEDSnet network clock (ms).
    pub started_network_ms: Option<u64>,
    pub report_age_ms: Option<u64>,
    pub launch_seen: bool,
    pub expected: bool,
    pub stalled: bool,
    /// The active warning, if any.
    pub warning: Option<String>,
    /// The barometer ground reference pressure (hPa), from the FC's
    /// barometer samples.
    pub baro_ref_hpa: Option<f64>,
    /// How far the FC moved it since it booted (hPa).
    pub baro_drift_hpa: Option<f64>,
    pub baro_mode: Option<BaroMode>,
    /// "Baro ref 1003.2 hPa · drift +1.8 hPa · tracking".
    pub baro_label: Option<String>,
}

impl Status {
    /// Fields whose change is news; ages alone are republished at 1 Hz.
    fn same_kind(&self, other: &Status) -> bool {
        self.state == other.state
            && self.phase == other.phase
            && self.warning == other.warning
            && self.started_network_ms == other.started_network_ms
            && self.started_t_plus_ms == other.started_t_plus_ms
            && self.launch_seen == other.launch_seen
            && self.expected == other.expected
            && self.baro_mode == other.baro_mode
    }
}

pub struct Tracker {
    inner: Mutex<Inner>,
    tx: watch::Sender<Status>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self { inner: Mutex::new(Inner::default()), tx: watch::channel(Status::default()).0 }
    }
}

fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

fn running_text(seen: &Seen, launch: Option<Instant>) -> String {
    let start = seen.started_at().unwrap_or(seen.at);
    match launch {
        Some(launch) if start + Duration::from_millis(500) >= launch => format!(
            "FC flight estimator running (Kalman filter started at T+{:.1} s)",
            seconds(start.saturating_duration_since(launch))
        ),
        Some(launch) => format!(
            "FC flight estimator running (Kalman filter started {:.1} s before Launch)",
            seconds(launch.duration_since(start))
        ),
        None => format!(
            "FC flight estimator running (Kalman filter started {:.1} s ago)",
            seconds(seen.at.saturating_duration_since(start))
        ),
    }
}

fn fault_text(phase: Phase) -> String {
    match phase {
        Phase::Diverged => "FC flight estimator diverged: its Kalman filter state is not finite or the FC no longer trusts it; the FC falls back to its barometer for flight events.".into(),
        _ => "FC flight estimator stopped: the FC's Kalman filter is no longer running (Evaluation Abort or a sensor restart).".into(),
    }
}

impl Tracker {
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.tx.subscribe()
    }

    /// One FC BAROMETER_DATA sample (alt, pressure, temperature).
    pub fn observe_barometer(&self, values: &[f32], now: Instant) {
        if let Some(reference) = reference_pressure(values) {
            self.inner.lock().unwrap().baro_reference = Some((reference, now));
        }
    }

    /// One FC BOARD_HEALTH report. Returns the notices to raise.
    pub fn observe(&self, words: &[u32], now: Instant, network_ms: Option<u64>, ctx: Context) -> Vec<Notice> {
        let Some(report) = decode(words) else {
            return Vec::new();
        };
        let mut inner = self.inner.lock().unwrap();
        inner.not_expected = !ctx.expected;
        let seen = Seen { report, at: now, network_ms };
        let previous = inner.last.replace(seen);
        let mut cycling = None;
        if let Some(previous) = previous
            && report.frequency_hz == previous.report.frequency_hz
            && report.uptime_tick > previous.report.uptime_tick
        {
            let span = Duration::from_secs_f64(
                (report.uptime_tick - previous.report.uptime_tick) as f64 / f64::from(report.frequency_hz),
            );
            let advanced = report.cycles.wrapping_sub(previous.report.cycles) & CYCLE_MASK;
            if span >= Duration::from_millis(500) {
                inner.rate_hz = Some(f64::from(advanced) / seconds(span));
            }
            if report.phase.running() && previous.report.phase.running() && report.start_tick == previous.report.start_tick && span >= STALL_SPAN {
                cycling = Some(advanced != 0);
            }
        }
        let mut notices = Vec::new();
        if report.phase == Phase::Idle {
            inner.notified_start = None;
            inner.warned_fault = None;
            inner.stalled = false;
            inner.rate_hz = None;
        }
        if report.phase.running() && report.start_tick.is_some() && inner.notified_start != report.start_tick {
            inner.notified_start = report.start_tick;
            inner.warned_fault = None;
            inner.stalled = false;
            notices.push(Notice::Info(running_text(&seen, inner.launch)));
        }
        if report.phase.running() {
            inner.warned_fault = None;
        }
        match cycling {
            Some(false) if !inner.stalled => {
                inner.stalled = true;
                if ctx.expected {
                    notices.push(Notice::Warning(format!(
                        "FC flight estimator stalled: its Kalman filter cycle count has not advanced in {:.1} s.",
                        seconds(Duration::from_secs_f64(
                            (report.uptime_tick - previous.unwrap().report.uptime_tick) as f64
                                / f64::from(report.frequency_hz)
                        ))
                    )));
                }
            }
            Some(true) if inner.stalled => {
                inner.stalled = false;
                notices.push(Notice::Info("FC flight estimator cycling again.".into()));
            }
            _ => {}
        }
        if !report.phase.running() && report.phase != Phase::Paused {
            inner.stalled = false;
        }
        if report.phase.faulted() && inner.warned_fault != Some(report.phase) {
            // A Stop/reset on the pad stops it too; only flight matters.
            if ctx.expected && (ctx.launched || report.phase == Phase::Diverged) {
                inner.warned_fault = Some(report.phase);
                notices.push(Notice::Warning(fault_text(report.phase)));
            }
        }
        notices
    }

    /// Periodic check (and every flight-state change). Returns the notices.
    pub fn tick(&self, now: Instant, ctx: Context) -> Vec<Notice> {
        let mut inner = self.inner.lock().unwrap();
        inner.not_expected = !ctx.expected;
        if !ctx.launched {
            inner.launch = None;
            inner.warned_no_start = false;
            return Vec::new();
        }
        let launch = *inner.launch.get_or_insert(now);
        let started = inner.last.is_some_and(|seen| seen.report.phase.started());
        if started || inner.warned_no_start || !ctx.expected {
            return Vec::new();
        }
        let waited = now.saturating_duration_since(launch);
        if waited < START_TIMEOUT {
            return Vec::new();
        }
        inner.warned_no_start = true;
        let reported = match inner.last {
            Some(seen) if now.saturating_duration_since(seen.at) <= REPORT_FRESH => {
                format!("it reports its Kalman filter {}", seen.report.phase.label())
            }
            _ => "no FC report has said its Kalman filter is running".into(),
        };
        vec![Notice::Warning(format!(
            "FC flight estimator not running: Launch was accepted {:.1} s ago and {reported}.",
            seconds(waited)
        ))]
    }

    pub fn status(&self, now: Instant) -> Status {
        let inner = self.inner.lock().unwrap();
        let fresh = inner.last.filter(|seen| now.saturating_duration_since(seen.at) <= REPORT_FRESH);
        let phase = inner.last.map(|seen| seen.report.phase);
        let waited = inner.launch.map(|launch| now.saturating_duration_since(launch));
        let no_start = waited.is_some_and(|w| w >= START_TIMEOUT) && !phase.is_some_and(Phase::started);
        let state = if inner.not_expected {
            "not_expected"
        } else if fresh.is_some_and(|seen| seen.report.phase.faulted()) || (inner.stalled && fresh.is_some()) {
            "faulted"
        } else if fresh.is_some_and(|seen| seen.report.phase.running()) {
            "running"
        } else if fresh.is_some() || inner.launch.is_some() {
            "not_running"
        } else {
            "unknown"
        };
        let started = fresh.filter(|seen| seen.report.start_tick.is_some() && seen.report.phase.started());
        let warning = if inner.not_expected {
            None
        } else if let Some(seen) = fresh.filter(|seen| seen.report.phase.faulted()) {
            Some(fault_text(seen.report.phase))
        } else if inner.stalled && fresh.is_some() {
            Some("FC flight estimator stalled: its Kalman filter cycle count has stopped advancing.".into())
        } else if no_start {
            Some(format!(
                "FC flight estimator not running {:.1} s after Launch.",
                seconds(waited.unwrap_or_default())
            ))
        } else {
            None
        };
        let baro = fresh.and_then(|seen| seen.report.baro);
        let baro_ref_hpa = inner
            .baro_reference
            .filter(|(_, at)| now.saturating_duration_since(*at) <= REPORT_FRESH)
            .map(|(pa, _)| (pa / 10.0).round() / 10.0);
        let baro_drift_hpa = baro.map(|b| f64::from(b.shift_pa) / 100.0);
        let baro_label = baro.map(|b| {
            let mut parts = Vec::new();
            if let Some(hpa) = baro_ref_hpa {
                parts.push(format!("Baro ref {hpa:.1} hPa"));
            } else {
                parts.push("Baro ref".to_string());
            }
            let saturated = b.shift_pa.unsigned_abs() >= 2047;
            parts.push(format!("drift {}{:+.1} hPa", if saturated { "beyond " } else { "" }, f64::from(b.shift_pa) / 100.0));
            parts.push(b.mode.label().to_string());
            parts.join(" · ")
        });
        let label = match state {
            "running" => format!("FC estimator: {}", phase.map_or("running", Phase::label)),
            "faulted" => format!("FC estimator: faulted ({})", if inner.stalled && !phase.is_some_and(Phase::faulted) { "stalled" } else { phase.map_or("faulted", Phase::label) }),
            "not_running" => format!("FC estimator: not running{}", phase.map_or(String::new(), |p| format!(" ({})", p.label()))),
            "not_expected" => "FC estimator: no FC in this configuration".into(),
            _ => "FC estimator: no report".into(),
        };
        Status {
            state,
            label,
            phase,
            cycles: inner.last.map(|seen| seen.report.cycles),
            rate_hz: inner.rate_hz.map(|r| (r * 10.0).round() / 10.0),
            running_for_ms: started
                .filter(|seen| seen.report.phase.running() || seen.report.phase == Phase::Paused)
                .and_then(|seen| seen.started_at())
                .map(|start| now.saturating_duration_since(start).as_millis() as u64),
            started_t_plus_ms: started.and_then(|seen| {
                let start = seen.started_at()?;
                let launch = inner.launch?;
                Some(if start >= launch {
                    start.duration_since(launch).as_millis() as i64
                } else {
                    -(launch.duration_since(start).as_millis() as i64)
                })
            }),
            started_network_ms: started.and_then(|seen| {
                Some(seen.network_ms?.checked_sub(seen.report.since_start()?.as_millis() as u64)?)
            }),
            report_age_ms: inner.last.map(|seen| now.saturating_duration_since(seen.at).as_millis() as u64),
            launch_seen: inner.launch.is_some(),
            expected: !inner.not_expected,
            stalled: inner.stalled,
            warning,
            baro_ref_hpa,
            baro_drift_hpa,
            baro_mode: baro.map(|b| b.mode),
            baro_label,
        }
    }

    /// Publishes the status: at once when it changed in kind, else at most
    /// every second. Returns it when it changed in kind (to record).
    pub fn publish(&self, now: Instant) -> Option<Status> {
        let status = self.status(now);
        let news = !self.tx.borrow().same_kind(&status);
        {
            let mut inner = self.inner.lock().unwrap();
            if !news && inner.published_at.is_some_and(|at| now.saturating_duration_since(at) < PUBLISH_EVERY) {
                return None;
            }
            inner.published_at = Some(now);
        }
        self.tx.send_replace(status.clone());
        news.then_some(status)
    }

    pub fn snapshot(&self) -> Status {
        self.tx.borrow().clone()
    }
}

/// An FC belongs to this configuration: not a test fire, and the layout's
/// expected boards (when it names any) include it. The layout file is read
/// at most every 10 s.
fn fc_expected(state: &crate::state::AppState) -> bool {
    static LAYOUT: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    if crate::station_mode::local_mode() == crate::station_mode::Mode::TestFire {
        return false;
    }
    let mut cached = LAYOUT.lock().unwrap();
    match *cached {
        Some((at, expected)) if at.elapsed() < Duration::from_secs(10) => expected,
        _ => {
            let expected = state.layout_expected_boards().contains(&crate::types::Board::FlightComputer);
            *cached = Some((Instant::now(), expected));
            expected
        }
    }
}

#[cfg(test)]
pub(crate) fn fc_expected_for_test(state: &crate::state::AppState) -> bool {
    fc_expected(state)
}

fn launched(state: &crate::state::AppState) -> bool {
    use crate::types::FlightState as F;
    let flying = |s: F| {
        matches!(s, F::Launch | F::Ascent | F::Coast | F::Apogee | F::Descent | F::Reefing | F::Landed | F::Recovery | F::InFlight)
    };
    if flying(state.local_flight_state_snapshot()) {
        return true;
    }
    // The FC's own FLIGHT_STATE (it republishes every 2 s from its Launch).
    let now = crate::telemetry_task::get_current_timestamp_ms();
    state
        .fc_phase
        .snapshot()
        .is_some_and(|seen| flying(seen.phase) && now.saturating_sub(seen.received_ms) <= 10_000)
}

fn raise(state: &crate::state::AppState, notices: Vec<Notice>) {
    let simulated = crate::igt::igt_active(state) && state.igt.simulated();
    for notice in notices {
        let notice = if simulated { simulated_input(notice) } else { notice };
        match notice {
            Notice::Info(text) => {
                log::info!("{text}");
                crate::web::emit_notification_warning(state, text);
            }
            Notice::Warning(text) => {
                log::warn!("{text}");
                crate::web::emit_warning(state, text.clone());
                state.add_temporary_notification(text);
            }
        }
    }
}

/// While the FC flies replayed data in an integrated ground test, every
/// estimator notice says so.
pub(crate) fn simulated_input(notice: Notice) -> Notice {
    let prefix = |text: String| format!("{}{text}", crate::igt::SIMULATED_INPUT_PREFIX);
    match notice {
        Notice::Info(text) => Notice::Info(prefix(text)),
        Notice::Warning(text) => Notice::Warning(prefix(text)),
    }
}

fn publish(state: &crate::state::AppState, now: Instant) {
    let Some(status) = state.fc_estimator.publish(now) else {
        return;
    };
    // Recorded on every change in kind; the raw BOARD_HEALTH is recorded too.
    let _ = state.db_queue_tx.try_send(crate::telemetry_db::DbQueueItem::Write(
        crate::telemetry_db::DbWrite::Telemetry {
            timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
            source_timestamp_ms: None,
            data_type: "GS_FC_ESTIMATOR".into(),
            sender_id: "GS".into(),
            values_json: None,
            payload_json: serde_json::to_string(&status).unwrap_or_default(),
        },
    ));
}

/// An FC BAROMETER_DATA sample arrived.
pub(crate) fn observe_barometer(state: &crate::state::AppState, values: &[f32]) {
    state.fc_estimator.observe_barometer(values, Instant::now());
}

/// An FC BOARD_HEALTH report arrived (`network_ms`: its SEDSnet stamp).
pub(crate) fn observe_health(state: &crate::state::AppState, words: &[u32], network_ms: Option<u64>) {
    if decode(words).is_none() {
        return;
    }
    let now = Instant::now();
    let ctx = Context { launched: launched(state), expected: fc_expected(state) };
    // Launch seen first, so a report right after it measures T+ from it.
    let mut notices = state.fc_estimator.tick(now, ctx);
    notices.extend(state.fc_estimator.observe(words, now, network_ms, ctx));
    raise(state, notices);
    publish(state, now);
}

/// Every 250 ms and on each flight-state change (mission_phase service).
pub(crate) fn tick(state: &crate::state::AppState) {
    let now = Instant::now();
    let notices = state.fc_estimator.tick(now, Context { launched: launched(state), expected: fc_expected(state) });
    raise(state, notices);
    publish(state, now);
    crate::igt::tick(state);
}

#[cfg(test)]
mod tests;
