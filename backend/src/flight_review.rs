//! Motor and flight review: the dashboard's motor section (test fire, HITL,
//! integrated ground test) and flight summary (normal, HITL, integrated
//! ground test, firmware simulation), live and from any recording, and the
//! motor / flight export bundles.
//!
//! - `GET /api/flight_review/live`: the station's current flight capture,
//!   computed from the live cache (the motor section during and after the
//!   burn, the flight summary as the FC reports it), plus the newest saved
//!   launch capture for after the capture closes.
//! - `GET /api/flight_review/recording/{id}?from_ms&to_ms`: the same from any
//!   replay source (`recording_replay::sources`: event captures, rolling
//!   segments and ranges, `merge-sd.*`, `sd-only.*`, `fire.*`).
//! - `GET /api/flight_review/recording/{id}/motor_export?from_ms&to_ms` and
//!   `.../flight_export`: ZIP bundles (`export`); without a range, the burn
//!   window / the flight. `raster=false` leaves the PNG charts and the PDF
//!   report out (as a replay bundle's exports and the UI's offline answers).
//!
//! A replay bundle carries what these need for any range (`offline`), so a
//! client answers custom windows with no station.
//!
//! Read only: nothing here writes a recording, a capture or station state.
use crate::auth::Permission;
use crate::burn_report::merge::{Channel, Sample};
use crate::burn_report::{self, BurnReport};
use crate::recording_replay::sources::{self, Source};
use crate::recording_store::EventKind;
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) mod data;
pub(crate) mod export;
pub(crate) mod flight;
pub(crate) mod motor;
pub(crate) mod offline;
pub(crate) mod plot;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod zip;

pub(crate) use data::{LiveEntry, note};
use flight::FlightSummary;
use motor::MotorView;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingReview {
    pub id: String,
    /// `capture`, `rolling`, `range`, `merge_sd`, `sd_only`, `fire`, ...
    pub source_kind: String,
    pub label: String,
    pub mode: Option<String>,
    pub provenance: Option<String>,
    pub integrated_ground_test: bool,
    pub simulated: bool,
    /// The requested range (None: the whole source).
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub data_from_ms: Option<i64>,
    pub data_to_ms: Option<i64>,
    pub motor: Option<MotorView>,
    pub flight: Option<FlightSummary>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveCapture {
    pub label: String,
    pub kind: String,
    pub open: bool,
    pub window_start_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveReview {
    pub mode: String,
    pub igt_active: bool,
    pub firmware_sim: bool,
    pub simulated: bool,
    /// Which sections this mode shows.
    pub show_motor: bool,
    pub show_flight: bool,
    pub capture: Option<LiveCapture>,
    pub motor: Option<MotorView>,
    pub flight: Option<FlightSummary>,
    /// The newest saved launch / HITL capture (a replay id), for review
    /// after the live capture has closed.
    pub latest_capture_id: Option<String>,
    pub latest_capture_label: Option<String>,
}

/// Which sections a station mode shows: the motor in a test fire, HITL and
/// the integrated ground test (the motor burns on the stand), the flight in
/// normal, HITL, the integrated ground test and under firmware simulation.
pub(crate) fn sections(mode: &str, firmware_sim: bool) -> (bool, bool) {
    let motor = matches!(mode, "test_fire" | "hitl" | "integrated_ground_test" | "testing");
    let flight = firmware_sim || matches!(mode, "normal" | "hitl" | "integrated_ground_test" | "testing");
    (motor, flight)
}

#[derive(Deserialize, Default)]
pub(crate) struct Range {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// Exports only: `false` leaves the PNG charts and PDF report out.
    pub raster: Option<bool>,
}

#[cfg(test)]
thread_local! {
    static PINNED_NOW: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Pins the clock the review routes stamp (`created_ms`, `generated_ms`) on
/// this thread, so fixtures made through the routes are reproducible.
#[cfg(test)]
pub(crate) fn pin_now(now_ms: Option<i64>) {
    PINNED_NOW.with(|c| c.set(now_ms));
}

fn now_ms() -> i64 {
    #[cfg(test)]
    if let Some(now) = PINNED_NOW.with(std::cell::Cell::get) {
        return now;
    }
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

// ------------------------------------------------------------------ sources

/// A fire archive record without its capture, as a report.
fn archive_report(record: &crate::fire_archive::FireRecord, curve: Vec<[f64; 2]>, t0: i64) -> BurnReport {
    let peak = curve.iter().map(|p| p[1]).fold(0.0, f64::max);
    let above: Vec<f64> = curve.iter().filter(|p| p[1] >= 0.05 * peak).map(|p| p[0]).collect();
    let end = above.last().copied().unwrap_or_default();
    let burn = record.burn_time_s.unwrap_or(end - above.first().copied().unwrap_or_default());
    let impulse = record
        .impulse_ns
        .unwrap_or_else(|| curve.windows(2).map(|w| (w[1][0] - w[0][0]) * (w[0][1] + w[1][1]) / 2.0).sum());
    let consumed = record
        .fill_before_kg
        .zip(record.fill_after_kg)
        .map(|(a, b)| a - b)
        .filter(|m| *m > 0.05);
    let (oxidizer_kg, fuel_kg, of_ratio) = burn_report::split_propellant(record.fill_kg.unwrap_or_default(), consumed);
    BurnReport {
        id: format!("fire.{}", record.id),
        capture_id: record.capture_id.clone(),
        capture_label: record.label.clone(),
        status: burn_report::Status::Final,
        simulated: record.simulated,
        mode: "test_fire".into(),
        created_ms: 0,
        ignition_ms: t0,
        ignition_source: format!("fire_archive_{}", record.source),
        total_impulse_ns: impulse,
        burn_time_s: burn,
        peak_thrust_n: record.peak_thrust_n.unwrap_or(peak),
        average_thrust_n: if burn > 0.0 { impulse / burn } else { 0.0 },
        impulse_sigma_ns: record.impulse_sigma_ns.unwrap_or_default(),
        flags: vec![],
        curve,
        propellant_consumed_kg: consumed,
        isp_s: consumed.map(|m| (impulse / (m * burn_report::thrust::G) * 10.0).round() / 10.0),
        fill_at_ignition_kg: record.fill_kg,
        fill_after_kg: record.fill_after_kg,
        tank_psi_at_ignition: record.tank_psi_at_ignition,
        tank_psi_end: None,
        burn_end_s: end,
        oxidizer_kg,
        fuel_kg,
        of_ratio,
        chamber: None,
        coverage: Default::default(),
        quality_flags: record.quality_flags.clone(),
        kg1000_impulse_ns: 0.0,
        kg50_term_ns: 0.0,
        startup_fill_impulse_ns: 0.0,
        impulse_lower_bound: false,
        over_range_gap_s: 0.0,
        archive_id: Some(record.id.clone()),
        tank_curve: Vec::new(),
        chamber_curve: Vec::new(),
        // the hybrid motor model for the archived conditions (fill, tank, supercharge, ambient, grain)
        predicted: record.fill_kg.and_then(|fill| {
            let (tank_c, sc) = crate::apogee_planner::hybrid_fit::conditions(record.tank_psi_at_ignition, record.supercharge_psi, record.ambient_c);
            burn_report::predict_for(fill, tank_c, sc, record.grain.as_deref(), 99_000.0)
        }),
    }
}

/// What a review takes from the source besides its rows: the event
/// capture's manifest (label, mode, provenance, applied plan) and the DAQ
/// launch file attached to a plain capture (every row; a review keeps those
/// in its range).
#[derive(Default)]
pub(crate) struct SourceContext {
    pub label: Option<String>,
    pub mode: Option<String>,
    pub provenance: Option<String>,
    pub simulated: bool,
    pub plan: Option<serde_json::Value>,
    pub extra_daq: Vec<(Channel, Sample)>,
    /// The capture folder (for its saved burn report).
    pub folder: Option<std::path::PathBuf>,
}

pub(crate) async fn source_context(root: &std::path::Path, source: &Source) -> SourceContext {
    let mut out = SourceContext::default();
    if let Some(capture) = source.capture_id()
        && let Some(folder) = sources::capture_folder(root, &capture).await
        && let Some(manifest) = crate::recording_store::read_manifest(&folder)
    {
        out.label = Some(manifest.label.clone());
        out.mode = Some(manifest.station_mode.clone());
        out.provenance = manifest.provenance.clone();
        out.simulated = manifest.simulated;
        out.plan = manifest.details.get("apogee_plan").cloned().filter(|v| !v.is_null());
        // A plain capture: the attached DAQ launch file is merged in as the
        // burn report does (a merged source already holds it).
        if source.kind() == "capture" {
            let (f, m) = (folder.clone(), manifest.clone());
            out.extra_daq = tokio::task::spawn_blocking(move || burn_report::daq_rows(&f, &m))
                .await
                .unwrap_or_default();
        }
        out.folder = Some(folder);
    }
    out
}

/// Everything one recording review needs.
pub(crate) struct Loaded {
    pub data: data::Data,
    pub review: RecordingReview,
    pub extra_daq: Vec<(Channel, Sample)>,
}

/// Reads and reviews one replay source. `root` is the station's data
/// directory; the database comes from `pool`.
pub(crate) async fn load(root: &std::path::Path, id: &str, pool: &sqlx::SqlitePool, range: &Range, now_ms: i64) -> anyhow::Result<Loaded> {
    let source = Source::parse(id).ok_or_else(|| anyhow::anyhow!("invalid source"))?;
    let mut data = data::read_pool(pool, range.from_ms, range.to_ms).await?;
    let mut review = RecordingReview {
        id: id.to_owned(),
        source_kind: source.kind().into(),
        label: id.to_owned(),
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        ..RecordingReview::default()
    };
    let custom_range = range.from_ms.is_some() || range.to_ms.is_some();
    let (mode, igt, provenance) = flight::context(&data);
    review.mode = mode;
    review.integrated_ground_test = igt;
    review.provenance = provenance;
    let mut saved = None;
    let mut archive = None;
    let context = source_context(root, &source).await;
    if let Some(label) = context.label {
        review.label = label;
    }
    review.mode = review.mode.or(context.mode);
    review.provenance = review.provenance.or(context.provenance);
    let mut manifest_simulated = context.simulated;
    let mut plan = context.plan;
    if !custom_range && let Some(folder) = &context.folder {
        saved = burn_report::read_report(folder);
    }
    let mut extra_daq = context.extra_daq;
    extra_daq.retain(|(_, s)| range.from_ms.is_none_or(|f| s.t_ms >= f) && range.to_ms.is_none_or(|t| s.t_ms <= t));
    if let Source::Fire(fire) = &source
        && let Some(record) = crate::fire_archive::get(root, fire)
    {
        review.label = record.label.clone();
        archive = serde_json::to_value(&record).ok();
        manifest_simulated |= record.simulated;
        plan = plan.or(record.applied_plan.clone()).filter(|v| !v.is_null());
        if !custom_range {
            saved = record.burn_report_id.as_deref().and_then(|r| burn_report::find(root, r));
            if saved.is_none() && !data.rows.iter().any(|r| r.data_type == "LOADCELL_WEIGHT_KG") {
                let curve = crate::fire_archive::curve(root, fire).unwrap_or_default();
                saved = Some(archive_report(&record, curve, sources::fire_t0(root, &record)));
            }
        }
    }
    review.simulated = manifest_simulated
        || igt
        || review.mode.as_deref() == Some("hitl")
        || review.provenance.as_deref().is_some_and(|p| p != "hardware");
    review.data_from_ms = data.first_ms();
    review.data_to_ms = data.last_ms();
    review.motor = motor::view(
        &data,
        motor::Context {
            id: format!("review.{id}"),
            label: review.label.clone(),
            mode: review.mode.clone().unwrap_or_default(),
            simulated: review.simulated,
            created_ms: now_ms,
            saved,
            plan: plan.as_ref(),
            extra_daq: extra_daq.clone(),
            archive,
        },
    );
    review.flight = flight::compute(&data, review.mode.as_deref(), review.provenance.as_deref());
    if let Some(f) = &mut review.flight {
        f.simulated |= review.simulated;
    }
    data.sort();
    Ok(Loaded { data, review, extra_daq })
}

async fn authorized(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), Response> {
    crate::web::authorize_headers(state, headers, Permission::ViewData)
        .await
        .map(|_| ())
}

fn error(status: StatusCode, text: &str) -> Response {
    (status, Json(serde_json::json!({"error": text}))).into_response()
}

pub(crate) async fn open_and_load(state: &Arc<AppState>, id: &str, range: &Range) -> Result<Loaded, Response> {
    if range.from_ms.zip(range.to_ms).is_some_and(|(a, b)| b <= a) {
        return Err(error(StatusCode::BAD_REQUEST, "The range must end after it starts."));
    }
    let pool = sources::open(state, id).await.map_err(|status| {
        error(
            status,
            match status {
                StatusCode::NOT_FOUND => "Recording not found. Refresh the recording list.",
                StatusCode::PAYLOAD_TOO_LARGE => "The recording is too large to review at once; choose a shorter range.",
                _ => "The recording cannot be read.",
            },
        )
    })?;
    let root = crate::recording_export::directory(state);
    let result = load(&root, id, &pool, range, now_ms()).await;
    if matches!(Source::parse(id), Some(Source::File(_))) {
        pool.close().await;
    }
    result.map_err(|e| {
        log::info!("flight review of {id}: {e:#}");
        error(StatusCode::UNPROCESSABLE_ENTITY, "The recording cannot be reviewed.")
    })
}

async fn get_recording(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    Query(range): Query<Range>,
) -> Response {
    if let Err(response) = authorized(&state, &headers).await {
        return response;
    }
    match open_and_load(&state, &id, &range).await {
        Ok(loaded) => ([(header::CACHE_CONTROL, "no-store")], Json(loaded.review)).into_response(),
        Err(response) => response,
    }
}

pub(crate) fn provenance(
    review: &RecordingReview,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    rows: usize,
    now_ms: i64,
) -> export::Provenance {
    export::Provenance {
        source_id: review.id.clone(),
        source_kind: review.source_kind.clone(),
        label: review.label.clone(),
        mode: review.mode.clone(),
        data_provenance: review.provenance.clone(),
        simulated: review.simulated,
        integrated_ground_test: review.integrated_ground_test,
        from_ms,
        to_ms,
        generated_ms: now_ms,
        station_software: format!("groundstation_backend {}", env!("CARGO_PKG_VERSION")),
        telemetry_rows: rows,
        daq_sd_rows: 0,
        notes: Vec::new(),
    }
}

/// The motor bundle of a loaded review; without a requested range, clipped
/// to the burn window.
pub(crate) async fn motor_export_bytes(mut loaded: Loaded, now_ms: i64, raster: bool) -> Option<(String, Vec<u8>)> {
    let view = loaded.review.motor.clone()?;
    let (mut from, mut to) = (loaded.review.from_ms, loaded.review.to_ms);
    if from.is_none() && to.is_none() {
        (from, to) = (view.window_from_ms, view.window_to_ms);
        loaded.data.clip(from, to);
        loaded
            .extra_daq
            .retain(|(_, s)| from.is_none_or(|f| s.t_ms >= f) && to.is_none_or(|t| s.t_ms <= t));
    }
    let prov = provenance(&loaded.review, from, to, loaded.data.rows.len(), now_ms);
    let name = format!("motor-{}.zip", export::file_stem(&loaded.review.label));
    Some((
        name,
        export::motor_bundle(&loaded.data, &loaded.extra_daq, &view, prov, raster).await,
    ))
}

/// The flight bundle of a loaded review; without a requested range, from
/// 30 s before Launch to a minute after touchdown.
pub(crate) async fn flight_export_bytes(mut loaded: Loaded, now_ms: i64, raster: bool) -> Option<(String, Vec<u8>)> {
    let summary = loaded.review.flight.clone()?;
    let (mut from, mut to) = (loaded.review.from_ms, loaded.review.to_ms);
    if from.is_none() && to.is_none() {
        from = summary
            .launch_accepted_ms
            .or(summary.t0_ms)
            .or(summary.liftoff_ms)
            .map(|t| t - 30_000);
        to = summary.landed_ms.map(|t| t + 60_000);
        loaded.data.clip(from, to);
    }
    let prov = provenance(&loaded.review, from, to, loaded.data.rows.len(), now_ms);
    let name = format!("flight-{}.zip", export::file_stem(&loaded.review.label));
    Some((name, export::flight_bundle(&loaded.data, &summary, prov, raster).await))
}

/// One default-window export of a recording, for a replay bundle.
pub(crate) struct BundledExport {
    /// `motor_export` or `flight_export`.
    pub kind: &'static str,
    /// The file name the route's `Content-Disposition` gives.
    pub filename: String,
    /// The window the export covers (the burn window / the flight).
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub bytes: Vec<u8>,
}

/// The motor and flight exports `GET .../motor_export` and `.../flight_export`
/// give without a range, generated at `now_ms` and without raster charts
/// (the SVG charts, data and HTML report are kept), so a bundle made at a
/// fixed time is reproducible. Exports that do not apply are left out.
pub(crate) async fn bundle_exports(state: &Arc<AppState>, id: &str, now_ms: i64) -> Vec<BundledExport> {
    let mut out = Vec::new();
    for kind in ["motor_export", "flight_export"] {
        let Ok(pool) = sources::open(state, id).await else { return out };
        let root = crate::recording_export::directory(state);
        let loaded = load(&root, id, &pool, &Range::default(), now_ms).await;
        if matches!(Source::parse(id), Some(Source::File(_))) {
            pool.close().await;
        }
        let Ok(loaded) = loaded else { continue };
        let window = if kind == "motor_export" {
            loaded.review.motor.as_ref().map(|m| (m.window_from_ms, m.window_to_ms))
        } else {
            loaded.review.flight.as_ref().map(|f| {
                (f.launch_accepted_ms.or(f.t0_ms).or(f.liftoff_ms).map(|t| t - 30_000), f.landed_ms.map(|t| t + 60_000))
            })
        };
        let built = if kind == "motor_export" { motor_export_bytes(loaded, now_ms, false).await } else { flight_export_bytes(loaded, now_ms, false).await };
        if let (Some((filename, bytes)), Some((from_ms, to_ms))) = (built, window) {
            out.push(BundledExport { kind, filename, from_ms, to_ms, bytes });
        }
    }
    out
}

fn zip_response(name: String, bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        bytes,
    )
        .into_response()
}

/// One export at a time: the bundle renders charts and may run the report Python.
static EXPORTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

async fn get_export(state: Arc<AppState>, headers: HeaderMap, id: String, range: Range, motor_kind: bool) -> Response {
    if let Err(response) = authorized(&state, &headers).await {
        return response;
    }
    let Ok(_permit) = EXPORTS.try_acquire() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Another export is being prepared. Retry when it finishes.",
        );
    };
    let loaded = match open_and_load(&state, &id, &range).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let (now, raster) = (now_ms(), range.raster.unwrap_or(true));
    let built = if motor_kind {
        motor_export_bytes(loaded, now, raster).await
    } else {
        flight_export_bytes(loaded, now, raster).await
    };
    match built {
        Some((name, bytes)) => zip_response(name, bytes),
        None if motor_kind => error(
            StatusCode::NOT_FOUND,
            "No motor data (load cells, chamber or tank PT) in this recording or range.",
        ),
        None => error(
            StatusCode::NOT_FOUND,
            "No flight data (FC phases, altitude or velocity) in this recording or range.",
        ),
    }
}

async fn get_motor_export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    Query(range): Query<Range>,
) -> Response {
    get_export(state, headers, id, range, true).await
}

async fn get_flight_export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    Query(range): Query<Range>,
) -> Response {
    get_export(state, headers, id, range, false).await
}

// ------------------------------------------------------------------ live

fn latest_capture(root: &std::path::Path) -> Option<(String, String)> {
    crate::recording_store::list_captures(root)
        .into_iter()
        .filter(|l| matches!(l.event_type, Some(EventKind::Launch | EventKind::Hitl)))
        .max_by_key(|l| l.start_ms.unwrap_or_default())
        .map(|l| (l.id.clone(), l.label.clone().unwrap_or_else(|| l.id.clone())))
}

pub(crate) fn live_review(state: &AppState, now_ms: i64) -> LiveReview {
    let mode = crate::station_mode_switch::mode_id(crate::runtime_mode::current()).to_owned();
    let firmware_sim = crate::firmware_sim_mode::enabled();
    let igt_active = crate::igt::igt_active(state);
    let (show_motor, show_flight) = sections(&mode, firmware_sim);
    let mut out = LiveReview {
        simulated: mode == "hitl" || igt_active || firmware_sim || crate::flight_sim_runtime::enabled(),
        mode,
        igt_active,
        firmware_sim,
        show_motor,
        show_flight,
        ..LiveReview::default()
    };
    let root = crate::recording_export::directory(state);
    if let Some((id, label)) = latest_capture(&root) {
        out.latest_capture_id = Some(id);
        out.latest_capture_label = Some(label);
    }
    let capture = state
        .recording_store
        .captures()
        .into_iter()
        .filter(|c| matches!(c.kind, EventKind::Launch | EventKind::Hitl))
        .max_by_key(|c| c.event_start_ms);
    let Some(capture) = capture else { return out };
    out.capture = Some(LiveCapture {
        label: capture.label.clone(),
        kind: capture.kind.id().into(),
        open: capture.event_end_ms.is_none(),
        window_start_ms: capture.window_start_ms,
    });
    let mut data = {
        let cache = state.recent_telemetry_cache.lock().unwrap();
        data::from_cache(cache.iter(), capture.window_start_ms)
    };
    data::add_live_log(&mut data, capture.window_start_ms);
    let simulated = out.simulated || capture.provenance.as_deref().is_some_and(|p| p != "hardware");
    let plan = capture.details.get("apogee_plan").cloned().filter(|v| !v.is_null());
    if out.show_motor {
        out.motor = motor::view(
            &data,
            motor::Context {
                id: format!("live-{}", capture.serial),
                label: capture.label.clone(),
                mode: capture.station_mode.clone(),
                simulated,
                created_ms: now_ms,
                saved: None,
                plan: plan.as_ref(),
                extra_daq: Vec::new(),
                archive: None,
            },
        )
        .map(|mut m| {
            m.report_origin = "live".into();
            m
        });
    }
    if out.show_flight {
        out.flight = flight::compute(&data, Some(&capture.station_mode), capture.provenance.as_deref()).map(|mut f| {
            f.simulated |= simulated;
            f
        });
    }
    out
}

static LIVE_CACHE: Mutex<Option<(Instant, usize, LiveReview)>> = Mutex::new(None);

async fn get_live(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorized(&state, &headers).await {
        return response;
    }
    let key = Arc::as_ptr(&state) as usize;
    if let Some((at, k, review)) = LIVE_CACHE.lock().unwrap().as_ref()
        && *k == key
        && at.elapsed() < Duration::from_secs(1)
    {
        return ([(header::CACHE_CONTROL, "no-store")], Json(review.clone())).into_response();
    }
    let s = state.clone();
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    let review = tokio::task::spawn_blocking(move || live_review(&s, now)).await.unwrap_or_default();
    *LIVE_CACHE.lock().unwrap() = Some((Instant::now(), key, review.clone()));
    ([(header::CACHE_CONTROL, "no-store")], Json(review)).into_response()
}

// ------------------------------------------------------------------ hooks

/// The FC's detailed phase changed: kept for the live summary and recorded
/// as a `GS_FC_PHASE` row (the state report itself is not recorded), so a
/// recording's summary knows the Apogee/Descent/Reefing/Landed edges.
pub(crate) fn fc_phase_changed(state: &AppState, phase: crate::types::FlightState, source: crate::fc_phase::Source, event_ms: i64) {
    note(event_ms, LiveEntry::FcPhase(phase as u8));
    let _ = state
        .db_queue_tx
        .try_send(crate::telemetry_db::DbQueueItem::Write(crate::telemetry_db::DbWrite::Telemetry {
            timestamp_ms: event_ms,
            source_timestamp_ms: None,
            data_type: "GS_FC_PHASE".into(),
            sender_id: "GS".into(),
            values_json: Some(format!("[{}]", phase as u8)),
            payload_json: serde_json::json!({
                "value": phase as u8,
                "label": crate::mission_phase::detailed_label(phase),
                "source": source,
            })
            .to_string(),
        }));
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/flight_review/live", get(get_live))
        .route("/api/flight_review/recording/{id}", get(get_recording))
        .route("/api/flight_review/recording/{id}/motor_export", get(get_motor_export))
        .route("/api/flight_review/recording/{id}/flight_export", get(get_flight_export))
}
