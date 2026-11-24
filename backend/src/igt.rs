//! Integrated ground test (docs/integrated-ground-test.md).
//!
//! A real fill and motor fire on the stand with the whole rocket powered as
//! for flight. After Launch the FC replays a flight from CSV files on its SD
//! card into its real estimator and state machine; its pyro outputs are
//! dry-run unless LIVE PYRO was armed.
//!
//! This module holds the station side of it:
//!
//! - the FC's status from BOARD_HEALTH word 14 bits 4..15 ([`decode`]),
//!   tracked by [`Tracker`] (`GET /api/igt`, websocket `IgtStatus`, a
//!   `GS_IGT` recording row on every change, operator notices on every
//!   transition);
//! - the gates of the Ground test controls and of the IGT Launch;
//! - the ticketed `POST /api/igt/prepare` / `POST /api/igt/confirm` flow,
//!   the only way `IgtEnter` and `IgtLivePyro` reach the FC;
//! - `SIM_SAMPLE` decoding into separate `SIM_*` series, so replayed data
//!   never feeds the real IMU/barometer/GPS/estimator series;
//! - the FC image identification and the Launch latch (doc section 7.1);
//! - the admin settings `mock_wet_dress` and `hitl_replay` (sections 7.2,
//!   7.3, `GET/PUT /api/igt/settings`), and [`igt_active`]: integrated ground
//!   test mode, or HITL with the replay on;
//! - the FC SD health and `FcSdRetry` (section 7.5, [`fc_sd`]);
//! - the configured launch site (section 7.4, `crate::launch_site`).
use crate::auth::{AuthPrincipal, Permission};
use crate::fc_estimator::Notice;
use crate::state::AppState;
use crate::types::{FlightState, TelemetryCommand, TelemetryRow};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub(crate) mod fc_sd;
mod settings;
pub(crate) use settings::Settings;

/// FC BOARD_HEALTH word 14 tag (bits 16..31), shared with the cosmetic
/// output bits 0..3 (network_variables).
pub(crate) const TAG: u32 = crate::network_variables::FC_OUTPUTS_TAG;
const SUPPORTED: u32 = 1 << 7;
const LIVE_PYRO: u32 = 1 << 8;
const GPS_SIMULATED: u32 = 1 << 9;
const DROGUE_DRY_RUN: u32 = 1 << 10;
const MAIN_DRY_RUN: u32 = 1 << 11;
/// Older than this, the last report no longer describes the FC (BOARD_HEALTH
/// every 5 s, and at once on a change).
pub(crate) const REPORT_FRESH: Duration = Duration::from_secs(12);
/// Review tickets are single use and last this long.
pub(crate) const TICKET_TTL: Duration = Duration::from_secs(15);
/// The phrase an administrator types to arm LIVE PYRO.
pub(crate) const LIVE_PYRO_PHRASE: &str = "LIVE PYRO";
/// Prefix of every estimator notice while the FC flies replayed data.
pub(crate) const SIMULATED_INPUT_PREFIX: &str = "SIMULATED input: ";
/// Why Launch is latched closed (doc section 7.1).
pub(crate) const LAUNCH_LATCH_REASON: &str =
    "FC is running a ground-test image (dry-run capable): Launch is blocked until the FC reports a flight image";
/// A flight image where the ground-test image is needed (doc section 7.1).
pub(crate) const FLIGHT_IMAGE_WARNING: &str =
    "FC is running a flight image: integrated ground test unavailable, flash the ground-test image";
const HITL_IMAGE_NOTICE: &str =
    "FC is running a ground-test image (dry-run capable). HITL does not block Launch.";
const IGT_IMAGE_NOTICE: &str = "FC is running the ground-test image.";
const LATCH_CLEARED: &str = "FC reports a flight image: the ground-test image Launch block is cleared.";

/// Which FC image is on the network (word 14 bit 7 of the FC's BOARD_HEALTH).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FcImage {
    GroundTest,
    Flight,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Warning,
    Info,
}

/// The image indicator shown with the IGT status (doc section 7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImageNotice {
    pub level: NoticeLevel,
    pub text: String,
}

/// Normal (and testing) and test fire latch Launch closed on a ground-test
/// image; HITL and the integrated ground test never do.
// Safety catalog (About safety): igt-image-latch. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn latch_mode() -> bool {
    use crate::station_mode::Mode;
    matches!(crate::runtime_mode::current(), Mode::Normal | Mode::TestFire | Mode::Testing)
}

/// The integrated ground test behaviour (Ground test controls, IGT status,
/// SIMULATED labels and recording rows, the IGT Launch gate) is on: the
/// integrated ground test mode, or HITL with the HITL replay on.
pub(crate) fn igt_active(state: &AppState) -> bool {
    crate::runtime_mode::is_igt() || (crate::runtime_mode::is_hitl() && state.igt.settings().hitl_replay)
}

/// The fill follows test fire: test fire, and the integrated ground test
/// unless mock wet dress runs the flight fill steps (nitrogen fill step and
/// the normally-open checks). HITL keeps its own (flight) fill.
// Safety catalog (About safety): igt-mock-wet-dress. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn test_fire_fill(state: &AppState) -> bool {
    crate::runtime_mode::is_test_fire()
        || (crate::runtime_mode::is_igt() && !state.igt.settings().mock_wet_dress)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FcMode {
    Off,
    Validating,
    Ready,
    Sim,
    Done,
    Fault,
}

impl FcMode {
    fn from_wire(value: u32) -> Option<Self> {
        Some(match value {
            0 => FcMode::Off,
            1 => FcMode::Validating,
            2 => FcMode::Ready,
            3 => FcMode::Sim,
            4 => FcMode::Done,
            5 => FcMode::Fault,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            FcMode::Off => "Off",
            FcMode::Validating => "Validating",
            FcMode::Ready => "Ready",
            FcMode::Sim => "Sim",
            FcMode::Done => "Done",
            FcMode::Fault => "Fault",
        }
    }
}

/// One decoded FC report (doc section 6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// Bit 7: the FC image was built with ground-test support. Without it
    /// every other field reads as Off/clear.
    pub supported: bool,
    pub mode: FcMode,
    pub live_pyro: bool,
    pub gps_simulated: bool,
    pub drogue_dry_run: bool,
    pub main_dry_run: bool,
    pub reason: u8,
}

impl Report {
    const UNSUPPORTED: Report = Report {
        supported: false,
        mode: FcMode::Off,
        live_pyro: false,
        gps_simulated: false,
        drogue_dry_run: false,
        main_dry_run: false,
        reason: 0,
    };
}

/// The IGT bits of an FC BOARD_HEALTH; `None` when word 14 does not carry the
/// FC tag (older firmware, or another board's use of word 14) or names an
/// unknown mode.
pub fn decode(words: &[u32]) -> Option<Report> {
    if words.len() != 24 || words[1] & 8 != 0 || words[14] >> 16 != TAG {
        return None;
    }
    let word = words[14];
    if word & SUPPORTED == 0 {
        return Some(Report::UNSUPPORTED);
    }
    Some(Report {
        supported: true,
        mode: FcMode::from_wire((word >> 4) & 7)?,
        live_pyro: word & LIVE_PYRO != 0,
        gps_simulated: word & GPS_SIMULATED != 0,
        drogue_dry_run: word & DROGUE_DRY_RUN != 0,
        main_dry_run: word & MAIN_DRY_RUN != 0,
        reason: ((word >> 12) & 0xF) as u8,
    })
}

/// Operator text for a reason code (bits 12..15).
pub fn reason_text(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "SD card unavailable",
        2 => "replay file missing",
        3 => "bad CSV header",
        4 => "CSV parse error or line too long",
        5 => "time not increasing",
        6 => "value out of range",
        7 => "replay data too short",
        8 => "vehicle not stationary at Launch",
        9 => "unused reason code 9",
        10 => "SD ring underrun (last value held)",
        11 => "refused (wrong state)",
        12 => "replay data ended before landing",
        _ => "unknown reason",
    }
}

fn informational(code: u8) -> bool {
    matches!(code, 10 | 12)
}

/// `GET /api/igt` and the websocket frame `IgtStatus`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub supported: bool,
    pub fc_mode: FcMode,
    pub live_pyro: bool,
    pub gps_simulated: bool,
    pub drogue_dry_run: bool,
    pub main_dry_run: bool,
    pub reason: u8,
    pub reason_text: &'static str,
    /// The station itself is in integrated ground test mode.
    pub station_mode_igt: bool,
    /// Wall time of the last FC report; null before the first one.
    pub updated_ms: Option<u64>,
    /// The last FC image reported (doc section 7.1).
    pub fc_image: FcImage,
    /// Launch is latched closed by a ground-test image (normal, testing and
    /// test fire only).
    pub launch_latched: bool,
    pub launch_latch_reason: Option<&'static str>,
    pub image_notice: Option<ImageNotice>,
    /// Integrated ground test mode, or HITL with the HITL replay on.
    pub igt_active: bool,
    pub mock_wet_dress: bool,
    pub hitl_replay: bool,
    /// FC BOARD_HEALTH word 6 bit 30 clear; null without a fresh FC report.
    pub sd_ready: Option<bool>,
    /// Word 6 bit 31 in a fresh FC report.
    pub sd_retry_pending: bool,
}

impl Status {
    fn of(inner: &Inner, now: Instant) -> Self {
        let r = inner.report.unwrap_or(Report::UNSUPPORTED);
        let launch_latched = inner.latched && latch_mode();
        let sd = fc_sd::fresh_bits(inner, now);
        Status {
            supported: r.supported,
            fc_mode: r.mode,
            live_pyro: r.live_pyro,
            gps_simulated: r.gps_simulated,
            drogue_dry_run: r.drogue_dry_run,
            main_dry_run: r.main_dry_run,
            reason: r.reason,
            reason_text: reason_text(r.reason),
            station_mode_igt: crate::runtime_mode::is_igt(),
            updated_ms: inner.updated_ms,
            fc_image: inner.image,
            launch_latched,
            launch_latch_reason: launch_latched.then_some(LAUNCH_LATCH_REASON),
            image_notice: image_notice(inner),
            igt_active: active(&inner.settings),
            mock_wet_dress: inner.settings.mock_wet_dress,
            hitl_replay: inner.settings.hitl_replay,
            sd_ready: sd.map(|bits| bits & fc_sd::NOT_READY == 0),
            sd_retry_pending: sd.is_some_and(|bits| bits & fc_sd::RETRY_PENDING != 0),
        }
    }
}

fn active(settings: &Settings) -> bool {
    crate::runtime_mode::is_igt() || (crate::runtime_mode::is_hitl() && settings.hitl_replay)
}

/// What the station shows about the FC image now (doc section 7.1).
fn image_notice(inner: &Inner) -> Option<ImageNotice> {
    use crate::station_mode::Mode;
    let notice = |level, text: &str| Some(ImageNotice { level, text: text.into() });
    if latch_mode() {
        return if inner.latched { notice(NoticeLevel::Warning, LAUNCH_LATCH_REASON) } else { None };
    }
    let active = active(&inner.settings);
    match (crate::runtime_mode::current(), inner.image) {
        (_, FcImage::Flight) if active => notice(NoticeLevel::Warning, FLIGHT_IMAGE_WARNING),
        (Mode::Hitl, FcImage::GroundTest) => notice(NoticeLevel::Info, HITL_IMAGE_NOTICE),
        (Mode::IntegratedGroundTest, FcImage::GroundTest) => notice(NoticeLevel::Info, IGT_IMAGE_NOTICE),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Enter,
    LivePyro,
}

impl Action {
    fn command(&self) -> TelemetryCommand {
        match self {
            Action::Enter => TelemetryCommand::IgtEnter,
            Action::LivePyro => TelemetryCommand::IgtLivePyro,
        }
    }
    fn id(&self) -> &'static str {
        match self {
            Action::Enter => "enter",
            Action::LivePyro => "live_pyro",
        }
    }
}

struct Ticket {
    id: String,
    action: Action,
    principal: String,
    session: Option<String>,
    created: Instant,
}

#[derive(Default)]
pub(crate) struct Inner {
    report: Option<Report>,
    at: Option<Instant>,
    updated_ms: Option<u64>,
    /// The last image reported; never reset by silence.
    image: FcImage,
    /// The Launch latch (doc section 7.1): set by a ground-test image seen in
    /// normal, testing or test fire, cleared only by a flight-image report.
    latched: bool,
    settings: Settings,
    /// Where the settings and the latch persist (the GS data directory);
    /// none until [`load_persisted`].
    data_dir: Option<PathBuf>,
    /// The image notice raised as a notification, with its id.
    shown: Option<(ImageNotice, Option<u64>)>,
    /// FC BOARD_HEALTH word 6 bits 30..31 and when they arrived.
    sd: Option<(u32, Instant)>,
    /// Word 6 bit 29: the FC image serves the SD read service (port 4511).
    sd_service: Option<(bool, Instant)>,
    sd_alert: Option<u64>,
    sd_last_retry: Option<Instant>,
    /// The FC has been in Sim since it last left Off/Validating: a Done that
    /// follows is still the end of a simulated flight.
    replayed: bool,
    /// Latest replayed altitude (SIM_SAMPLE barometer or ascent state).
    sim_altitude_m: Option<f64>,
    tickets: Vec<Ticket>,
}

pub struct Tracker {
    inner: Mutex<Inner>,
    tx: watch::Sender<Status>,
    /// The configured FC launch site (doc section 7.4).
    pub(crate) launch_site: crate::launch_site::Runtime,
    /// The apogee planner's settings and plans (docs/apogee-planner.md).
    pub(crate) apogee_planner: crate::apogee_planner::Runtime,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            tx: watch::channel(Status::of(&Inner::default(), Instant::now())).0,
            launch_site: Default::default(),
            apogee_planner: Default::default(),
        }
    }
}

impl Tracker {
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.tx.subscribe()
    }

    pub fn status(&self) -> Status {
        let inner = self.inner.lock().unwrap();
        Status::of(&inner, Instant::now())
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings
    }

    #[cfg(test)]
    pub fn fc_image(&self) -> FcImage {
        self.inner.lock().unwrap().image
    }

    /// The Launch latch is set (whatever the mode; it is enforced only in
    /// [`latch_mode`]).
    pub fn latched(&self) -> bool {
        self.inner.lock().unwrap().latched
    }

    /// The last report says Sim (fresh or not: a silent FC may still fly).
    fn reports_sim(&self) -> bool {
        self.inner.lock().unwrap().report.is_some_and(|r| r.supported && r.mode == FcMode::Sim)
    }

    /// Republish when anything shown changed (a report's content, or the
    /// station mode); a repeated identical report only refreshes
    /// `updated_ms` in [`Tracker::status`]. Returns the new status when it
    /// changed.
    pub fn publish(&self) -> Option<Status> {
        let status = self.status();
        let changed = self.tx.send_if_modified(|current| {
            let changed = Status { updated_ms: None, ..current.clone() }
                != Status { updated_ms: None, ..status.clone() };
            if changed {
                *current = status.clone();
            }
            changed
        });
        changed.then_some(status)
    }

    /// The last report while it still describes the FC.
    pub fn fresh_report(&self, now: Instant) -> Option<Report> {
        let inner = self.inner.lock().unwrap();
        inner
            .report
            .filter(|_| inner.at.is_some_and(|at| now.saturating_duration_since(at) <= REPORT_FRESH))
    }

    /// The FC flies (or flew, until it leaves the mode) replayed data: mode
    /// Sim, or Done after a replay.
    pub fn simulated(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.report.is_some_and(|r| {
            r.supported && (r.mode == FcMode::Sim || (r.mode == FcMode::Done && inner.replayed))
        })
    }

    pub fn live_pyro(&self) -> bool {
        self.inner.lock().unwrap().report.is_some_and(|r| r.supported && r.live_pyro)
    }

    pub fn sim_altitude_m(&self) -> Option<f64> {
        self.inner.lock().unwrap().sim_altitude_m
    }

    fn note_sim_altitude(&self, altitude_m: f32) {
        if altitude_m.is_finite() {
            self.inner.lock().unwrap().sim_altitude_m = Some(f64::from(altitude_m));
        }
    }

    /// A decoded report arrived. Returns the notices its transitions raise.
    pub fn observe(&self, report: Report, now: Instant, wall_ms: u64) -> Vec<Notice> {
        let mut inner = self.inner.lock().unwrap();
        let previous = inner.report;
        inner.report = Some(report);
        inner.at = Some(now);
        inner.updated_ms = Some(wall_ms);
        inner.image = if report.supported { FcImage::GroundTest } else { FcImage::Flight };
        // Only a fresh flight-image report clears the latch.
        if !report.supported && inner.latched {
            inner.latched = false;
            settings::persist(&inner);
        }
        Self::latch_if_due(&mut inner);
        match report.mode {
            FcMode::Sim => inner.replayed = true,
            FcMode::Off | FcMode::Validating | FcMode::Ready => {
                inner.replayed = false;
                inner.sim_altitude_m = None;
            }
            FcMode::Done | FcMode::Fault => {}
        }
        let active = active(&inner.settings);
        drop(inner);
        transition_notices(previous, report, active)
    }

    /// Sets the latch while the last image is the ground-test one and the
    /// station is in a latching mode (a report, or a switch into the mode).
    fn latch_if_due(inner: &mut Inner) {
        if !inner.latched && inner.image == FcImage::GroundTest && latch_mode() {
            inner.latched = true;
            settings::persist(inner);
        }
    }
}

/// What changed between two reports, as operator notices.
fn transition_notices(previous: Option<Report>, next: Report, igt_active: bool) -> Vec<Notice> {
    let mut notices = Vec::new();
    let prev = previous.unwrap_or(Report::UNSUPPORTED);
    if !igt_active {
        // An FC in ground-test mode under a flight station is never expected.
        if next.supported && next.mode != FcMode::Off && (prev.mode != next.mode || !prev.supported) {
            notices.push(Notice::Warning(format!(
                "FC reports integrated ground test mode {} while the station is in {} mode. Send IgtExit from an Integrated ground test station or reset the FC to Startup.",
                next.mode.label(),
                crate::runtime_mode::label(crate::runtime_mode::current())
            )));
        }
        return notices;
    }
    // The image itself is shown by the image notice (sync_image_notice).
    if !next.supported {
        return notices;
    }
    if next.mode != prev.mode {
        let pyro = if next.live_pyro { "LIVE PYRO armed" } else { "pyro DRY-RUN" };
        notices.push(match next.mode {
            FcMode::Validating => Notice::Info(
                "FC ground test entered: validating the SD-card replay files (pyro DRY-RUN).".into(),
            ),
            FcMode::Ready => Notice::Info(format!(
                "FC ground test Ready: replay files validated; {pyro}."
            )),
            FcMode::Sim => Notice::Info(format!(
                "FC ground test: SIMULATED flight started; the FC flies replayed IMU, barometer and GPS data ({pyro})."
            )),
            FcMode::Done => Notice::Info(
                "FC ground test: replayed flight done (landed or data ended); the FC is back on real sensors and real GPS."
                    .into(),
            ),
            FcMode::Fault => Notice::Warning(format!(
                "FC ground test fault: {} (code {}). The FC flies real sensor data; Launch stays refused until IGT is Ready.",
                reason_text(next.reason),
                next.reason
            )),
            FcMode::Off if matches!(prev.mode, FcMode::Ready | FcMode::Sim) => Notice::Warning(
                "FC ground test left: FC mode Off (IgtExit, Stop, Abort or an FC reset). The FC runs on real sensors with normal pyro.".into(),
            ),
            FcMode::Off => Notice::Info(
                "FC ground test left: FC mode Off (IgtExit, Stop, Abort or an FC reset).".into(),
            ),
        });
    } else if next.reason != prev.reason && next.reason != 0 && next.mode == FcMode::Fault {
        notices.push(Notice::Warning(format!(
            "FC ground test fault: {} (code {}).",
            reason_text(next.reason),
            next.reason
        )));
    }
    if next.reason != prev.reason && informational(next.reason) && next.mode != FcMode::Fault {
        notices.push(Notice::Info(format!("FC ground test: {}.", reason_text(next.reason))));
    }
    if next.mode != FcMode::Off && prev.supported && prev.mode == next.mode && next.live_pyro != prev.live_pyro {
        notices.push(if next.live_pyro {
            Notice::Warning("FC ground test: LIVE PYRO armed; the drogue and main outputs will fire.".into())
        } else {
            Notice::Info("FC ground test: pyro back to DRY-RUN.".into())
        });
    }
    if prev.gps_simulated && !next.gps_simulated && matches!(next.mode, FcMode::Done | FcMode::Sim | FcMode::Fault) {
        notices.push(Notice::Info("FC ground test: GPS switched to real.".into()));
    }
    notices
}

fn raise(state: &AppState, notices: Vec<Notice>) {
    for notice in notices {
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

pub(crate) fn status_row(status: &Status) -> crate::telemetry_db::DbWrite {
    crate::telemetry_db::DbWrite::Telemetry {
        timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
        source_timestamp_ms: None,
        data_type: "GS_IGT".into(),
        sender_id: "GS".into(),
        values_json: None,
        payload_json: serde_json::to_string(status).unwrap_or_default(),
    }
}

/// Rows written at the start of every recording: the station mode (with
/// `igt_active` and the IGT settings), and while the integrated ground test is
/// active (IGT mode, or HITL with the replay) the FC status the recording
/// starts from.
pub(crate) fn recording_start_rows(state: &AppState) -> Vec<crate::telemetry_db::DbWrite> {
    let mode = crate::runtime_mode::current();
    let settings = state.igt.settings();
    let active = igt_active(state);
    let mut rows = vec![crate::telemetry_db::DbWrite::Telemetry {
        timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
        source_timestamp_ms: None,
        data_type: "GS_STATION_MODE".into(),
        sender_id: "GS".into(),
        values_json: None,
        payload_json: json!({
            "mode": crate::station_mode_switch::mode_id(mode),
            "igt_active": active,
            "mock_wet_dress": settings.mock_wet_dress,
            "hitl_replay": settings.hitl_replay,
            // Where the data comes from, so a replay of any recording (not
            // only a capture with a manifest) shows the simulated-firmware
            // or flight-sim banner it was recorded under.
            "provenance": crate::firmware_sim_mode::provenance(),
            "firmware_sim": crate::firmware_sim_mode::enabled(),
        })
        .to_string(),
    }];
    if active {
        rows.push(status_row(&state.igt.status()));
    }
    rows
}

pub(crate) fn publish(state: &AppState) {
    sync_image_notice(state);
    crate::recording_store::level(
        state,
        crate::recording_store::EventKind::GroundTest,
        "igt",
        igt_active(state),
        "Integrated ground test",
        "Left the integrated ground test",
    );
    let Some(status) = state.igt.publish() else {
        return;
    };
    // Recorded on every change; the raw BOARD_HEALTH is recorded too.
    let _ = state
        .db_queue_tx
        .try_send(crate::telemetry_db::DbQueueItem::Write(status_row(&status)));
    // The sequence loop re-derives the action policy (Ground test controls,
    // IGT Launch) on its next tick; the mission phase's SIMULATED tag now.
    crate::mission_phase::refresh(state);
}

/// Raises or clears the image notification when the image notice changes
/// (doc section 7.1): a warning as soon as a mismatch is seen, cleared when
/// it ends. The integrated ground test's expected-image indicator is shown in
/// the status only.
fn sync_image_notice(state: &AppState) {
    use crate::station_mode::Mode;
    let mut inner = state.igt.inner.lock().unwrap();
    Tracker::latch_if_due(&mut inner);
    let wanted = image_notice(&inner);
    if inner.shown.as_ref().map(|(notice, _)| notice) == wanted.as_ref() {
        return;
    }
    let previous = inner.shown.take();
    let quiet = |notice: &ImageNotice| {
        notice.level == NoticeLevel::Info && crate::runtime_mode::current() == Mode::IntegratedGroundTest
    };
    let cleared_latch = previous.as_ref().is_some_and(|(notice, _)| notice.text == LAUNCH_LATCH_REASON)
        && !inner.latched
        && inner.image == FcImage::Flight;
    // Notifications are raised outside the tracker lock.
    drop(inner);
    if let Some((_, Some(id))) = previous {
        let _ = state.dismiss_notification(id);
    }
    let id = match &wanted {
        Some(notice) if !quiet(notice) => {
            let id = match notice.level {
                NoticeLevel::Warning => {
                    log::warn!("{}", notice.text);
                    crate::web::emit_warning(state, notice.text.clone());
                    state.add_notification(notice.text.clone())
                }
                NoticeLevel::Info => {
                    log::info!("{}", notice.text);
                    state.add_notification(notice.text.clone())
                }
            };
            Some(id)
        }
        _ => None,
    };
    if cleared_latch {
        log::info!("{LATCH_CLEARED}");
        state.add_temporary_notification(LATCH_CLEARED);
    }
    state.igt.inner.lock().unwrap().shown = wanted.map(|notice| (notice, id));
}

/// An FC BOARD_HEALTH report arrived.
pub(crate) fn observe_health(state: &AppState, words: &[u32]) {
    fc_sd::observe_health(state, words);
    let Some(report) = decode(words) else {
        publish(state);
        return;
    };
    let notices = state.igt.observe(report, Instant::now(), crate::telemetry_task::get_current_timestamp_ms());
    raise(state, notices);
    publish(state);
}

/// Every 250 ms (mission_phase service): follows a station mode switch.
pub(crate) fn tick(state: &AppState) {
    publish(state);
}

// ---------------------------------------------------------------------------
// Gates

fn launched(state: &AppState) -> bool {
    use std::sync::atomic::Ordering;
    state.launch_indicator_latched()
        || state.launch_sequence_command_pending.load(Ordering::Acquire)
        || (state.local_flight_state_snapshot() as u8) >= FlightState::Launch as u8
            && state.local_flight_state_snapshot() != FlightState::Aborted
}

/// The Launch latch refusal, in the modes that enforce it (doc section 7.1).
pub(crate) fn latch_refusal(state: &AppState) -> Option<String> {
    (latch_mode() && state.igt.latched()).then(|| LAUNCH_LATCH_REASON.to_string())
}

/// Why `Launch` or `GroundStationLaunch` is refused by the ground-test rules:
/// the Launch latch, the IGT Launch gate ([`launch_refusal`]), and
/// GroundStationLaunch while the HITL replay is on (Launch must go to the FC).
// Safety catalog (About safety): igt-hitl-replay. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn launch_command_refusal(state: &AppState, cmd: &TelemetryCommand) -> Option<String> {
    match cmd {
        TelemetryCommand::Launch => launch_refusal(state),
        TelemetryCommand::GroundStationLaunch => latch_refusal(state).or_else(|| {
            (crate::runtime_mode::is_hitl() && state.igt.settings().hitl_replay).then(|| {
                "GroundStationLaunch is refused while the HITL replay is on: use Launch, which goes to the FC and the Valve Board and needs FC ground test Ready.".to_string()
            })
        }),
        _ => None,
    }
}

/// Advance and Rewind are refused in HITL with the replay on while the FC
/// reports Sim: the replayed flight drives the phase (doc section 7.3).
pub(crate) fn phase_step_refusal(state: &AppState) -> Option<String> {
    (crate::runtime_mode::is_hitl() && state.igt.settings().hitl_replay && state.igt.reports_sim()).then(|| {
        "The FC is flying the ground-test replay (IGT Sim): the replayed flight drives the phase. Advance and Rewind work again once the FC reports Done.".to_string()
    })
}

/// Why Launch is refused now by the ground-test rules: the Launch latch in
/// normal, testing and test fire; while the integrated ground test is active
/// (IGT mode, or HITL with the replay) the FC must report ground-test mode
/// Ready in a fresh BOARD_HEALTH.
pub(crate) fn launch_refusal(state: &AppState) -> Option<String> {
    if let Some(reason) = latch_refusal(state) {
        return Some(reason);
    }
    if !igt_active(state) {
        return None;
    }
    match state.igt.fresh_report(Instant::now()) {
        None => Some("Integrated ground test: no recent FC ground-test status; Launch needs the FC to report IGT Ready.".into()),
        Some(r) if !r.supported => Some("Integrated ground test: the FC image has no ground-test support; load an FC image built with --ground-test.".into()),
        Some(r) if r.mode != FcMode::Ready => Some(format!(
            "Integrated ground test: FC ground test is {}, not Ready{}; Launch is refused.",
            r.mode.label(),
            if r.reason != 0 { format!(" ({})", reason_text(r.reason)) } else { String::new() }
        )),
        Some(_) => None,
    }
}

/// Test fixtures: the FC reports ground test Ready now, while the
/// integrated ground test is active (elsewhere a ground-test image would
/// latch Launch closed, so the fixture leaves the FC unreported).
#[cfg(test)]
pub(crate) fn ready_for_test(state: &AppState) {
    if !igt_active(state) {
        return;
    }
    let ready = Report { mode: FcMode::Ready, ..Report::UNSUPPORTED };
    state.igt.observe(Report { supported: true, ..ready }, Instant::now(), 1);
}

pub(crate) fn launch_ready(state: &AppState) -> bool {
    launch_refusal(state).is_none()
}

pub(crate) fn is_igt_command(cmd: &TelemetryCommand) -> bool {
    matches!(
        cmd,
        TelemetryCommand::IgtEnter
            | TelemetryCommand::IgtLivePyro
            | TelemetryCommand::IgtDryRun
            | TelemetryCommand::IgtExit
    )
}

/// Commands the plain command path never sends: they need the ticketed
/// prepare/confirm flow.
pub(crate) fn requires_ticket(cmd: &TelemetryCommand) -> bool {
    matches!(cmd, TelemetryCommand::IgtEnter | TelemetryCommand::IgtLivePyro)
}

/// The station-side gate of a Ground test control. `None` for other commands.
pub(crate) fn command_gate(state: &AppState, cmd: &TelemetryCommand) -> Option<Result<(), String>> {
    if matches!(cmd, TelemetryCommand::FcSdRetry) {
        return Some(fc_sd::retry_gate(state, Instant::now()));
    }
    if !is_igt_command(cmd) {
        return None;
    }
    Some(gate(state, cmd, Instant::now()))
}

fn gate(state: &AppState, cmd: &TelemetryCommand, now: Instant) -> Result<(), String> {
    if crate::runtime_mode::is_hitl() && !state.igt.settings().hitl_replay {
        return Err(format!(
            "{} needs the HITL replay: an administrator turns it on under Ground test settings.",
            crate::sequences::command_name(cmd)
        ));
    }
    if !igt_active(state) {
        return Err(crate::runtime_mode::unavailable(crate::sequences::command_name(cmd)));
    }
    if crate::rrls::is_rrls() {
        return Err("Ground test controls are sent only by the primary GroundStation.".into());
    }
    match cmd {
        TelemetryCommand::IgtEnter => {
            let flight = state.local_flight_state_snapshot();
            if !matches!(flight, FlightState::Startup | FlightState::Idle) {
                return Err(format!(
                    "Enter ground test is allowed only in Startup or Idle (before Armed); the station is in {flight:?}."
                ));
            }
            if launched(state) {
                return Err("Enter ground test is refused after a Launch; reset the FC to Startup first.".into());
            }
            let report = state.igt.fresh_report(now).ok_or_else(|| {
                "No recent FC ground-test status; wait for the FC's BOARD_HEALTH.".to_string()
            })?;
            if !report.supported {
                return Err("The FC image has no ground-test support; load an FC image built with --ground-test.".into());
            }
            if !matches!(report.mode, FcMode::Off | FcMode::Fault | FcMode::Done) {
                return Err(format!("The FC ground test is already {}.", report.mode.label()));
            }
            match state.fc_phase.snapshot().map(|seen| seen.phase) {
                Some(FlightState::Startup) => Ok(()),
                Some(phase) => Err(format!(
                    "The FC is in {phase:?}; it accepts Enter ground test only in Startup (reset it to Startup first)."
                )),
                None => Err("The FC phase is unknown; wait for the FC's flight state report.".into()),
            }
        }
        TelemetryCommand::IgtLivePyro => {
            if launched(state) {
                return Err("LIVE PYRO can only be armed before Launch.".into());
            }
            match state.igt.fresh_report(now) {
                Some(r) if r.supported && r.mode == FcMode::Ready => Ok(()),
                Some(r) if r.supported => Err(format!(
                    "LIVE PYRO needs the FC ground test Ready; it is {}.",
                    r.mode.label()
                )),
                Some(_) => Err("The FC image has no ground-test support.".into()),
                None => Err("No recent FC ground-test status; LIVE PYRO needs the FC to report Ready.".into()),
            }
        }
        // Dry-run only makes things safer; exit is refused by the FC in Sim.
        _ => Ok(()),
    }
}

/// Sets the Ground test controls' state, FcSdRetry, the Launch latch, the
/// IGT Launch refusal and the HITL replay's GroundStationLaunch and phase-step
/// refusals in the action policy.
pub(crate) fn decorate_policy(state: &AppState, policy: &mut crate::sequences::ActionPolicyMsg) {
    let now = Instant::now();
    let report = state.igt.fresh_report(now);
    let launch = launch_command_refusal(state, &TelemetryCommand::Launch);
    let ground_station_launch = launch_command_refusal(state, &TelemetryCommand::GroundStationLaunch);
    let phase_step = phase_step_refusal(state);
    let disable = |control: &mut crate::sequences::ActionControl, reason: &Option<String>| {
        if let Some(reason) = reason {
            control.enabled = false;
            control.disabled_reason = Some(reason.clone());
            control.blink = crate::sequences::BlinkMode::None;
        }
    };
    for control in &mut policy.controls {
        let cmd = match control.cmd.as_str() {
            "IgtEnter" => TelemetryCommand::IgtEnter,
            "IgtLivePyro" => TelemetryCommand::IgtLivePyro,
            "IgtDryRun" => TelemetryCommand::IgtDryRun,
            "IgtExit" => TelemetryCommand::IgtExit,
            "FcSdRetry" => {
                let result = fc_sd::retry_gate(state, now);
                control.enabled = control.enabled && result.is_ok();
                if let Err(reason) = result {
                    control.disabled_reason = Some(reason);
                }
                control.blink = crate::sequences::BlinkMode::None;
                continue;
            }
            "Launch" => {
                disable(control, &launch);
                continue;
            }
            "GroundStationLaunch" => {
                disable(control, &ground_station_launch);
                continue;
            }
            "AdvanceFlightState" | "RewindFlightState" => {
                disable(control, &phase_step);
                continue;
            }
            _ => continue,
        };
        let result = gate(state, &cmd, now);
        control.enabled = result.is_ok();
        control.disabled_reason = result.err();
        control.blink = crate::sequences::BlinkMode::None;
        control.actuated = report.filter(|r| r.supported).map(|r| match cmd {
            TelemetryCommand::IgtEnter => r.mode != FcMode::Off,
            TelemetryCommand::IgtLivePyro => r.live_pyro,
            TelemetryCommand::IgtDryRun => !r.live_pyro,
            _ => false,
        });
    }
}

/// The FC command byte of a Ground test control.
pub(crate) fn fc_byte(cmd: &TelemetryCommand) -> Option<u8> {
    use crate::rocket_commands::FlightComputerCommands as Fc;
    Some(match cmd {
        TelemetryCommand::IgtEnter => Fc::IgtEnter as u8,
        TelemetryCommand::IgtLivePyro => Fc::IgtLivePyro as u8,
        TelemetryCommand::IgtDryRun => Fc::IgtDryRun as u8,
        TelemetryCommand::IgtExit => Fc::IgtExit as u8,
        _ => return None,
    })
}

/// Queue a Ground test command to the FC.
pub(crate) fn send(state: &AppState, router: &sedsnet::router::Router, cmd: &TelemetryCommand) -> Result<(), String> {
    let byte = fc_byte(cmd).ok_or("not a ground test command")?;
    let name = crate::sequences::command_name(cmd);
    crate::telemetry_task::queue_locally_routed_flight_command(state, router, &format!("{name} command"), &[byte])
        .map_err(|error| format!("{name} was not queued: {error:?}"))?;
    crate::telemetry_task::flush_command_tx(router, "Ground test command tx");
    log::info!("{name} command sent to the flight computer ({byte})");
    Ok(())
}

// ---------------------------------------------------------------------------
// Ticketed prepare/confirm

#[derive(Deserialize)]
struct PrepareRequest {
    action: String,
}

#[derive(Deserialize)]
struct ConfirmRequest {
    ticket: String,
    #[serde(default)]
    confirmation: Option<String>,
}

fn principal_context(principal: &AuthPrincipal) -> String {
    match (&principal.username, principal.anonymous) {
        (Some(name), false) => format!("user:{name}"),
        _ => "anonymous".into(),
    }
}

fn session_binding(headers: &HeaderMap) -> Option<String> {
    crate::web::bearer_token_from_headers(headers).map(|token| {
        let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
        digest.as_ref()[..16].iter().map(|b| format!("{b:02x}")).collect()
    })
}

fn refusal(status: StatusCode, text: impl Into<String>) -> (StatusCode, String) {
    (status, text.into())
}

/// Who may take this action at all.
fn principal_allowed(principal: &AuthPrincipal, action: &Action) -> Result<(), (StatusCode, String)> {
    if principal.anonymous || principal.username.is_none() {
        return Err(refusal(StatusCode::FORBIDDEN, "Ground test reviews need a signed-in operator."));
    }
    match action {
        Action::LivePyro if !principal.is_admin() => Err(refusal(
            StatusCode::FORBIDDEN,
            "Only an administrator may arm LIVE PYRO.",
        )),
        Action::Enter if !principal.allows_command_name("IgtEnter") => Err(refusal(
            StatusCode::FORBIDDEN,
            "This session is not allowed to send IgtEnter.",
        )),
        _ => Ok(()),
    }
}

fn admit(state: &AppState, action: &Action) -> Result<(), (StatusCode, String)> {
    let cmd = action.command();
    gate(state, &cmd, Instant::now()).map_err(|reason| refusal(StatusCode::CONFLICT, reason))?;
    if let Some(reason) = crate::web::abort_fence_reason(state, &cmd) {
        return Err(refusal(StatusCode::CONFLICT, reason));
    }
    if crate::gse::manual_stop_pending(state) {
        return Err(refusal(StatusCode::CONFLICT, "Manual Stop is pending."));
    }
    Ok(())
}

pub(crate) fn prepare(
    state: &AppState,
    principal: &AuthPrincipal,
    session: Option<String>,
    body: &[u8],
) -> Result<serde_json::Value, (StatusCode, String)> {
    let request: PrepareRequest = serde_json::from_slice(body)
        .map_err(|_| refusal(StatusCode::BAD_REQUEST, "Invalid ground test review request."))?;
    let action = match request.action.as_str() {
        "enter" => Action::Enter,
        "live_pyro" => Action::LivePyro,
        _ => return Err(refusal(StatusCode::BAD_REQUEST, "Unknown ground test action.")),
    };
    principal_allowed(principal, &action)?;
    admit(state, &action)?;
    let id = format!("{:032x}", rand::random::<u128>());
    let now_ms = crate::telemetry_task::get_current_timestamp_ms();
    let (required, summary) = match action {
        Action::Enter => (
            None,
            "The FC validates the SD-card replay files and enters ground test mode with its pyro outputs DRY-RUN. After Launch it flies the replayed flight.",
        ),
        Action::LivePyro => (
            Some(LIVE_PYRO_PHRASE),
            "The FC's drogue and main outputs will FIRE when the replayed flight reaches them. Type LIVE PYRO to confirm.",
        ),
    };
    let mut inner = state.igt.inner.lock().unwrap();
    inner.tickets.retain(|t| t.created.elapsed() <= TICKET_TTL);
    inner.tickets.push(Ticket {
        id: id.clone(),
        action: action.clone(),
        principal: principal_context(principal),
        session,
        created: Instant::now(),
    });
    Ok(json!({
        "ticket": id,
        "action": action.id(),
        "command": crate::sequences::command_name(&action.command()),
        "expires_ms": now_ms + TICKET_TTL.as_millis() as u64,
        "expires_in_ms": TICKET_TTL.as_millis() as u64,
        "required_confirmation": required,
        "summary": summary,
    }))
}

/// Checks the ticket, principal, phrase and every gate again, then returns
/// the command to send.
pub(crate) fn confirm(
    state: &AppState,
    principal: &AuthPrincipal,
    session: Option<String>,
    body: &[u8],
) -> Result<TelemetryCommand, (StatusCode, String)> {
    let request: ConfirmRequest = serde_json::from_slice(body)
        .map_err(|_| refusal(StatusCode::BAD_REQUEST, "Invalid ground test confirmation."))?;
    // Single use: any attempt naming the ticket consumes it.
    let ticket = {
        let mut inner = state.igt.inner.lock().unwrap();
        let index = inner.tickets.iter().position(|t| t.id == request.ticket);
        index.map(|index| inner.tickets.remove(index))
    }
    .ok_or_else(|| refusal(StatusCode::CONFLICT, "This ground test review is unknown or was already used. Review again."))?;
    if ticket.created.elapsed() > TICKET_TTL {
        return Err(refusal(StatusCode::CONFLICT, "This ground test review expired. Review again."));
    }
    if ticket.principal != principal_context(principal) || ticket.session != session {
        return Err(refusal(StatusCode::CONFLICT, "This ground test review belongs to another session."));
    }
    principal_allowed(principal, &ticket.action)?;
    if ticket.action == Action::LivePyro
        && request.confirmation.as_deref().map(str::trim) != Some(LIVE_PYRO_PHRASE)
    {
        return Err(refusal(StatusCode::BAD_REQUEST, "Type LIVE PYRO exactly to arm live pyro."));
    }
    admit(state, &ticket.action)?;
    Ok(ticket.action.command())
}

/// The persisted settings and the latch are read from the GS data directory
/// (beside the station database). Called once at start.
pub(crate) fn load_persisted(state: &AppState) {
    let dir = std::path::Path::new(&state.placeholder_db_path)
        .parent()
        .map(|dir| dir.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    settings::load(state, dir.clone());
    crate::launch_site::load(state, dir.clone());
    crate::apogee_planner::load(state, dir.clone());
    crate::los_predict::load(dir.clone());
    crate::proximity_lock::load(state, dir);
}

async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(state.igt.status()),
    )
        .into_response()
}

fn error_response((status, text): (StatusCode, String)) -> Response {
    (status, Json(json!({"error": text}))).into_response()
}

async fn post_prepare(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    match prepare(&state, &principal, session_binding(&headers), &body) {
        Ok(value) => Json(value).into_response(),
        Err(error) => error_response(error),
    }
}

async fn post_confirm(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let cmd = match confirm(&state, &principal, session_binding(&headers), &body) {
        Ok(cmd) => cmd,
        Err(error) => return error_response(error),
    };
    // LIVE PYRO arms the FC's pyro channels: the proximity lock's arming class.
    if let Some(message) = crate::proximity_lock::admission_refusal(&state, &cmd, principal.username.as_deref()) {
        return error_response(refusal(StatusCode::CONFLICT, message));
    }
    let Some(router) = state.topology_router.get() else {
        return error_response(refusal(StatusCode::SERVICE_UNAVAILABLE, "The station network is not running."));
    };
    let name = crate::sequences::command_name(&cmd);
    match send(&state, router, &cmd) {
        Ok(()) => {
            let actor = principal.username.clone().unwrap_or_default();
            let text = match cmd {
                TelemetryCommand::IgtLivePyro => format!("LIVE PYRO requested by {actor}: IgtLivePyro sent to the FC."),
                _ => format!("Enter ground test requested by {actor}: IgtEnter sent to the FC."),
            };
            log::warn!("{text}");
            crate::web::emit_notification_warning(&state, text.clone());
            crate::web::persist_message_db_only(&state, text);
            Json(json!({"accepted": true, "command": name})).into_response()
        }
        Err(error) => error_response(refusal(StatusCode::SERVICE_UNAVAILABLE, error)),
    }
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/igt", get(get_status))
        .route("/api/igt/prepare", post(post_prepare))
        .route("/api/igt/confirm", post(post_confirm))
        .route("/api/igt/settings", get(settings::get_settings).put(settings::put_settings))
}

// ---------------------------------------------------------------------------
// SIM_SAMPLE (doc section 6.2)

/// The live series a SIM_SAMPLE kind feeds, never the real one.
pub fn sim_series(kind: u8) -> Option<(&'static str, usize)> {
    Some(match kind {
        1 => ("SIM_IMU_DATA", 6),
        2 => ("SIM_BAROMETER_DATA", 3),
        3 => ("SIM_GPS_DATA", 3),
        4 => ("SIM_ASCENT_STATE", 6),
        5 => ("SIM_DESCENT_STATE", 4),
        _ => return None,
    })
}

/// One decoded sample: its series, seconds since T0 and the values in the
/// real type's order.
#[derive(Clone, Debug, PartialEq)]
pub struct SimSample {
    pub kind: u8,
    pub series: &'static str,
    pub t_s: f32,
    pub values: Vec<f32>,
}

pub fn decode_sim_sample(values: &[Option<f32>]) -> Option<SimSample> {
    if values.len() != 8 {
        return None;
    }
    let kind = values[0]?;
    if !kind.is_finite() || kind.fract() != 0.0 || !(1.0..=5.0).contains(&kind) {
        return None;
    }
    let kind = kind as u8;
    let (series, count) = sim_series(kind)?;
    Some(SimSample {
        kind,
        series,
        t_s: values[1].unwrap_or(f32::NAN),
        values: values[2..2 + count].iter().map(|v| v.unwrap_or(f32::NAN)).collect(),
    })
}

/// The live UI rows of an FC SIM_SAMPLE (the raw sample is recorded as is).
pub(crate) fn sim_rows(state: &AppState, sender_id: &str, ts_ms: i64, values: &[Option<f32>]) -> Vec<TelemetryRow> {
    if crate::types::canonical_sender_id(sender_id.trim()) != "FC" {
        return Vec::new();
    }
    let Some(sample) = decode_sim_sample(values) else {
        return Vec::new();
    };
    match sample.kind {
        2 => state.igt.note_sim_altitude(sample.values[0]),
        4 => state.igt.note_sim_altitude(sample.values[4]),
        _ => {}
    }
    vec![TelemetryRow {
        timestamp_ms: ts_ms,
        data_type: sample.series.into(),
        sender_id: sender_id.to_string(),
        values: sample.values.into_iter().map(Some).collect(), over_range: 0, over_range_low: 0,
    }]
}

#[cfg(test)]
mod tests;
