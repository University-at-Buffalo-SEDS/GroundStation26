//! HTTP API for the DAC signal generator. Errors are JSON `{"error": "..."}`.
use super::analysis::{self, Analysis};
use super::custom::{self, CustomTable};
use super::model::{self, Polarity, TARGETS, Target};
use super::sweep;
use super::upload::{self, Uploader};
use super::waveform;
use super::wire::{self, Command, Waveform};
use super::{COMMAND_NAME, ConfigRequest, Refusal, STATUS_FRESH_MS, Validation};
use crate::{auth::Permission, state::AppState, types::FlightState};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) type ApiError = (StatusCode, String);

const CHANNEL_LABELS: [&str; 2] = [
    "DAC1 OUT1 (PA4) -> P10 pin 2",
    "DAC1 OUT2 (PA5) -> P10 pin 5",
];
/// Validation warns when the target has been silent this long.
const SILENT_MS: i64 = 3_000;

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}
fn error_response((status, message): ApiError) -> Response {
    json_error(status, message)
}
fn refusal_error(refusal: Refusal) -> ApiError {
    (
        StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::CONFLICT),
        refusal.message,
    )
}

/// Re-wrap a plain-text auth rejection as a JSON error.
async fn json_rejection(response: Response) -> Response {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&body).trim().to_string();
    json_error(
        status,
        if text.is_empty() {
            status
                .canonical_reason()
                .unwrap_or("Request refused")
                .to_string()
        } else {
            text
        },
    )
}

pub(super) fn local_authorization(
    anonymous: bool,
    allowed: bool,
    rrls: bool,
) -> Result<(), ApiError> {
    if anonymous {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Sign in to control the DAQ DAC.".into(),
        ));
    }
    if !allowed {
        return Err((
            StatusCode::FORBIDDEN,
            format!("{COMMAND_NAME} command permission is required."),
        ));
    }
    if rrls {
        return Err((
            StatusCode::FORBIDDEN,
            "RRLS is read-only. Use the primary GroundStation to drive the DAQ DAC.".into(),
        ));
    }
    Ok(())
}

async fn authorize_write(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), Response> {
    let principal =
        match crate::web::authorize_headers(state, headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(response) => return Err(json_rejection(response).await),
        };
    local_authorization(
        principal.anonymous,
        principal.allows_command_name(COMMAND_NAME),
        crate::rrls::is_rrls(),
    )
    .map_err(error_response)
}

async fn authorize_read(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), Response> {
    match crate::web::authorize_headers(state, headers, Permission::ViewData).await {
        Ok(_) => Ok(()),
        Err(response) => Err(json_rejection(response).await),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("Invalid request: {e}")))
}

pub(super) struct Enabled {
    pub command: Command,
    pub clamped: Vec<&'static str>,
}

/// Gate, send and keep alive one configuration. Shared by the config
/// endpoint and the sweep.
pub(super) fn enable(state: &AppState, request: &ConfigRequest) -> Result<Enabled, ApiError> {
    let (command, clamped) =
        super::plan_command(request, 1).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
    enable_command(state, request.clone(), command, clamped)
}

/// Gate, send and keep alive an already planned command.
pub(super) fn enable_command(
    state: &AppState,
    request: ConfigRequest,
    mut command: Command,
    clamped: Vec<&'static str>,
) -> Result<Enabled, ApiError> {
    let closed = || {
        (
            StatusCode::CONFLICT,
            crate::board_recovery::coordinator::admission_closed_message(
                state,
                "Driving the DAQ DAC",
            ),
        )
    };
    let generation = crate::gse::capture_operator_origin(state).ok_or_else(closed)?;
    let _claim = crate::gse::claim_operator_dispatch(state, generation).ok_or_else(closed)?;
    let router = state.topology_router.get().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "DAQ network router is unavailable.".to_string(),
    ))?;
    let now = Instant::now();
    let status = super::daq_status(state, now);
    if let Some(refusal) = super::enable_refusal(
        state.local_flight_state_snapshot(),
        status.as_ref().map(|(s, age)| (s, *age)),
    ) {
        return Err(refusal_error(refusal));
    }
    command.sequence = state.daq_dac.next_sequence();
    super::send(router, &command).map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("DAQ DAC command could not be queued: {e}"),
        )
    })?;
    state.daq_dac.set_desired(request, command, now);
    Ok(Enabled { command, clamped })
}

#[derive(Serialize)]
struct PinView {
    waveform: Waveform,
    amplitude_v: f32,
    offset_v: f32,
    frequency_hz: f32,
    steps: u16,
    dwell_ms: u32,
}

pub(crate) async fn post_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_write(&state, &headers).await {
        return response;
    }
    let request: ConfigRequest = match parse(&body) {
        Ok(r) => r,
        Err(response) => return response,
    };
    if state.daq_dac.inner.lock().unwrap().sweep.state == sweep::State::Running {
        return json_error(
            StatusCode::CONFLICT,
            "A DAC sweep is running; cancel it before setting the output by hand.",
        );
    }
    if request.waveform == Waveform::Arbitrary {
        return post_arbitrary(&state, request);
    }
    match enable(&state, &request) {
        Ok(Enabled { command, clamped }) => {
            let message = if clamped.is_empty() {
                format!(
                    "DAC channel {} output on; keeping it alive.",
                    command.channel
                )
            } else {
                format!(
                    "DAC channel {} output on with {} clamped to the DAC's 0.4..6.2 V connector range.",
                    command.channel,
                    clamped.join(", ")
                )
            };
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "accepted": true,
                    "sequence": command.sequence,
                    "dac_pin": PinView {
                        waveform: command.waveform,
                        amplitude_v: command.amplitude_v,
                        offset_v: command.offset_v,
                        frequency_hz: command.frequency_hz,
                        steps: command.steps,
                        dwell_ms: command.dwell_ms,
                    },
                    "clamped": clamped,
                    "message": message,
                })),
            )
                .into_response()
        }
        Err(error) => error_response(error),
    }
}

/// `{"channel": 0}`, `{"channel": 1}` or `{"channel": "all"}`.
pub(super) fn parse_disable_channel(value: &serde_json::Value) -> Result<Option<u8>, String> {
    match value.get("channel") {
        Some(serde_json::Value::String(s)) if s == "all" => Ok(None),
        Some(serde_json::Value::Number(n)) if n.as_u64().is_some_and(|c| c <= 1) => {
            Ok(Some(n.as_u64().unwrap() as u8))
        }
        _ => Err("channel must be 0, 1 or \"all\".".to_string()),
    }
}

pub(crate) async fn disable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_write(&state, &headers).await {
        return response;
    }
    let value: serde_json::Value = match parse(&body) {
        Ok(v) => v,
        Err(response) => return response,
    };
    let channel = match parse_disable_channel(&value) {
        Ok(c) => c,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };
    // A sweep or table upload for this channel stops too.
    {
        let inner = state.daq_dac.inner.lock().unwrap();
        if inner.upload.state == UploadState::Running
            && (channel.is_none() || inner.upload.channel == channel)
            && let Some(cancel) = &inner.upload.cancel
        {
            cancel.store(true, Ordering::Release);
        }
        if inner.sweep.state == sweep::State::Running
            && (channel.is_none() || inner.sweep.channel == channel)
            && let Some(cancel) = &inner.sweep.cancel
        {
            cancel.store(true, Ordering::Release);
        }
    }
    let command = state.daq_dac.request_disable(channel, Instant::now());
    state.daq_dac.wake.notify_one();
    let Some(router) = state.topology_router.get() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "DAQ network router is unavailable.",
        );
    };
    if let Err(error) = super::send(router, &command) {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "DAQ DAC disable could not be queued ({error}); the GroundStation keeps retrying."
            ),
        );
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "accepted": true,
            "sequence": command.sequence,
            "message": "Disable sent; the GroundStation repeats it until the DAQ reports the output off.",
        })),
    )
        .into_response()
}

#[derive(Serialize)]
struct ChannelView {
    channel: u8,
    label: &'static str,
    desired: Option<ConfigRequest>,
    active: bool,
    waveform: &'static str,
    amplitude_v: f64,
    offset_v: f64,
    frequency_hz: f64,
    steps: u16,
    dwell_ms: u32,
    clamped: Vec<&'static str>,
    output_v: Option<f64>,
    readback_v: Option<f64>,
    last_result: &'static str,
    disable_reason: &'static str,
    sequence: u32,
    update_hz: f64,
    samples: u16,
    phase_deg: f64,
    duty_pct: Option<f64>,
    ramp_down: bool,
    log_sweep: bool,
    table_len: u16,
    /// CRC-32 of the codes the DAQ's DMA plays.
    table_crc: String,
    /// Whether that CRC matches the table the GS computes from the applied
    /// parameters (None when it cannot tell, e.g. an unknown shape).
    table_crc_ok: Option<bool>,
}

#[derive(Serialize)]
struct TableView {
    state: &'static str,
    len: u16,
    upload_id: u32,
    shape_crc: String,
    computed_crc: String,
    chunks_received: u32,
    chunk_count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum UploadState {
    #[default]
    Idle,
    Running,
    Done,
    Failed,
}

/// Progress of the arbitrary-shape upload the GS runs before enabling.
#[derive(Default)]
pub(super) struct UploadCtl {
    pub state: UploadState,
    pub channel: Option<u8>,
    pub phase: Option<upload::Phase>,
    pub chunks_confirmed: usize,
    pub chunk_count: usize,
    pub resent_chunks: usize,
    pub shape_crc: Option<u32>,
    pub message: Option<String>,
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Serialize)]
struct UploadView {
    state: UploadState,
    channel: Option<u8>,
    phase: Option<upload::Phase>,
    chunks_confirmed: usize,
    chunk_count: usize,
    resent_chunks: usize,
    shape_crc: Option<String>,
    message: Option<String>,
}

#[derive(Serialize)]
struct InputView {
    id: &'static str,
    label: &'static str,
    unit: &'static str,
    valid_range: [f64; 2],
    stream_hz: f64,
    needs_attenuator: bool,
    model: &'static str,
}

#[derive(Serialize)]
struct StatusView {
    allowed: bool,
    refusal: Option<String>,
    flight_state: FlightState,
    daq_flight_state: Option<u8>,
    status_age_ms: Option<u64>,
    status_fresh: bool,
    any_active: bool,
    buffer_gain: f64,
    dac_pin_range_v: [f64; 2],
    connector_range_v: [f64; 2],
    channels: Vec<ChannelView>,
    inputs: Vec<InputView>,
    /// Why the GS last turned the output off on its own, if it did.
    last_auto_off: Option<String>,
    table: Option<TableView>,
    upload: UploadView,
    limits: serde_json::Value,
}

fn hex(v: u32) -> String {
    format!("{v:08X}")
}

/// DAC limits for the UI (DAQ-Board docs/dac-waveforms.md).
pub(super) fn limits() -> serde_json::Value {
    serde_json::json!({
        "frequency_hz": [waveform::FREQ_MIN_HZ, waveform::FREQ_MAX_HZ],
        "noise_rate_hz": [waveform::NOISE_RATE_MIN_HZ, waveform::UPDATE_MAX_HZ],
        "update_max_hz": waveform::UPDATE_MAX_HZ,
        "periodic_samples": [waveform::PERIODIC_LEN_MIN, waveform::PERIODIC_LEN_MAX],
        "shape_samples": [waveform::ARB_LEN_MIN, waveform::ARB_LEN_MAX],
        "steps": [waveform::STEPS_MIN, waveform::STEPS_MAX],
        "dwell_ms": [waveform::DWELL_MIN_MS, waveform::DWELL_MAX_MS],
        "sweep_s": [f64::from(waveform::SWEEP_MIN_MS) / 1000.0, f64::from(waveform::SWEEP_MAX_MS) / 1000.0],
        "duty_pct": [1.0, 99.0],
        "dac_bits": 12,
        "code_lsb_v": 3.3 / 4095.0 * model::BUFFER_GAIN,
        "wav_max_bytes": super::audio::MAX_FILE_BYTES,
        "expression": super::expr::vocabulary(),
    })
}

fn connector(pin: f32) -> Option<f64> {
    pin.is_finite().then(|| f64::from(pin) * model::BUFFER_GAIN)
}

fn status_view(state: &AppState) -> StatusView {
    let now = Instant::now();
    let flight_state = state.local_flight_state_snapshot();
    let status = super::daq_status(state, now);
    let refusal = super::enable_refusal(flight_state, status.as_ref().map(|(s, a)| (s, *a)));
    let desired = [
        state.daq_dac.desired_request(0),
        state.daq_dac.desired_request(1),
    ];
    let (shape, upload_view) = {
        let inner = state.daq_dac.inner.lock().unwrap();
        let u = &inner.upload;
        (
            inner.shape.clone(),
            UploadView {
                state: u.state,
                channel: u.channel,
                phase: u.phase,
                chunks_confirmed: u.chunks_confirmed,
                chunk_count: u.chunk_count,
                resent_chunks: u.resent_chunks,
                shape_crc: u.shape_crc.map(hex),
                message: u.message.clone(),
            },
        )
    };
    // Only a shape the DAQ holds verified can predict an arbitrary channel.
    let shape = shape.filter(|(crc, _)| {
        status
            .as_ref()
            .is_some_and(|(s, _)| s.table.state == wire::TABLE_READY && s.table.crc == *crc)
    });
    let channels = (0..2)
        .map(|c| {
            let ch = status
                .as_ref()
                .map(|(s, _)| s.channels[c])
                .unwrap_or_default();
            ChannelView {
                channel: c as u8,
                label: CHANNEL_LABELS[c],
                desired: desired[c].clone(),
                active: ch.active,
                waveform: Waveform::from_u8(ch.waveform)
                    .unwrap_or(Waveform::Dc)
                    .name(),
                amplitude_v: connector(ch.amplitude_v).unwrap_or(0.0),
                offset_v: connector(ch.offset_v).unwrap_or(0.0),
                frequency_hz: f64::from(ch.frequency_hz),
                steps: ch.steps,
                dwell_ms: ch.dwell_ms,
                clamped: wire::clamp_names(ch.clamp_flags),
                output_v: if ch.active {
                    connector(ch.output_v)
                } else {
                    None
                },
                readback_v: connector(ch.readback_v),
                last_result: wire::last_result_name(ch.last_result),
                disable_reason: wire::disable_reason_name(ch.disable_reason),
                sequence: ch.sequence,
                update_hz: f64::from(ch.update_hz),
                samples: ch.samples,
                phase_deg: f64::from(ch.phase_q16) * 360.0 / 65536.0,
                duty_pct: (ch.waveform == Waveform::Square as u8)
                    .then(|| f64::from(ch.duty_q16) * 100.0 / 65536.0),
                ramp_down: ch.waveform == Waveform::Sawtooth as u8 && ch.flags & wire::FLAG_RAMP_DOWN != 0,
                log_sweep: ch.waveform == Waveform::Chirp as u8 && ch.flags & wire::FLAG_LOG_SWEEP != 0,
                table_len: ch.table_len,
                table_crc: hex(ch.table_crc),
                table_crc_ok: ch.active.then(|| expected_crc(&ch, &shape)).flatten().map(|c| c == ch.table_crc),
            }
        })
        .collect::<Vec<_>>();
    let any_active = channels.iter().any(|c| c.active) || desired.iter().any(Option::is_some);
    StatusView {
        allowed: refusal.is_none(),
        refusal: refusal.map(|r| r.message),
        flight_state,
        daq_flight_state: status.as_ref().map(|(s, _)| s.flight_state),
        status_age_ms: status.as_ref().map(|(_, age)| *age),
        status_fresh: status
            .as_ref()
            .is_some_and(|(_, age)| *age <= STATUS_FRESH_MS),
        any_active,
        buffer_gain: model::BUFFER_GAIN,
        dac_pin_range_v: [model::DAC_MIN_V, model::DAC_MAX_V],
        connector_range_v: [
            model::DAC_MIN_V * model::BUFFER_GAIN,
            model::DAC_MAX_V * model::BUFFER_GAIN,
        ],
        channels,
        inputs: TARGETS
            .iter()
            .map(|t| InputView {
                id: t.id,
                label: t.label,
                unit: "V",
                valid_range: [t.valid_vin.0, t.valid_vin.1],
                stream_hz: t.stream_hz,
                needs_attenuator: t.needs_attenuator(),
                model: t.model_text(),
            })
            .collect(),
        last_auto_off: state.daq_dac.last_drop(),
        table: status.as_ref().map(|(s, _)| TableView {
            state: wire::table_state_name(s.table.state),
            len: s.table.len,
            upload_id: s.table.upload_id,
            shape_crc: hex(s.table.expected_crc),
            computed_crc: hex(s.table.crc),
            chunks_received: s.table.chunks.count_ones(),
            chunk_count: usize::from(s.table.len).div_ceil(wire::CHUNK_SAMPLES),
        }),
        upload: upload_view,
        limits: limits(),
    }
}

/// The play-table CRC a channel should report: from its applied parameters,
/// and for an arbitrary wave from the shape the GS uploaded.
fn expected_crc(ch: &wire::ChannelStatus, shape: &Option<(u32, Vec<i16>)>) -> Option<u32> {
    if ch.waveform != Waveform::Arbitrary as u8 {
        return waveform::expected_status_crc(ch);
    }
    let (_, shape) = shape.as_ref().filter(|(_, s)| s.len() == usize::from(ch.table_len))?;
    let params = waveform::Params::from_status(ch)?;
    waveform::fill_shape(&params, shape).map(|t| waveform::crc32(&t))
}

pub(crate) async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_read(&state, &headers).await {
        return response;
    }
    Json(status_view(&state)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidationRequest {
    target: Option<String>,
    #[serde(default)]
    channel: u8,
    #[serde(default = "unity")]
    attenuation: f64,
    #[serde(default)]
    load_cell_polarity: Polarity,
}
fn unity() -> f64 {
    1.0
}

pub(super) fn check_selection(
    target: &str,
    channel: u8,
    attenuation: f64,
) -> Result<&'static Target, String> {
    let target =
        model::target(target).ok_or_else(|| format!("Unknown validation target {target:?}."))?;
    if channel > 1 {
        return Err(format!("DAC channel {channel} does not exist; use 0 or 1."));
    }
    if !attenuation.is_finite() || attenuation <= 0.0 || attenuation > 1.0 {
        return Err(
            "Attenuation k is the external divider ratio: more than 0 and at most 1.".into(),
        );
    }
    Ok(target)
}

#[derive(Serialize, Default)]
pub(super) struct Series {
    pub t_ms: Vec<i64>,
    pub measured: Vec<f64>,
    pub expected: Vec<f64>,
}

#[derive(Serialize)]
struct ValidationView {
    target: Option<&'static str>,
    channel: Option<u8>,
    attenuation: Option<f64>,
    load_cell_polarity: Option<Polarity>,
    warnings: Vec<String>,
    series: Series,
    analysis: Option<Analysis>,
}

fn fmt_num(x: f64) -> String {
    // 2.0 -> "2", 0.25 -> "0.25"
    let s = format!("{:.3}", x);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub(super) struct Snapshot {
    pub target: &'static Target,
    pub channel: u8,
    pub attenuation: f64,
    pub polarity: Polarity,
    pub samples: Vec<(i64, f64)>,
    pub wave: Option<analysis::Wave>,
    pub active: bool,
}

/// Plot series, analysis and operator warnings for one validation snapshot.
pub(super) fn evaluate(
    snap: &Snapshot,
    status_fresh: bool,
    now_ms: i64,
) -> (Series, Option<Analysis>, Vec<String>) {
    let target = snap.target;
    let model = target.model(snap.attenuation, snap.polarity);
    let mut warnings = Vec::new();
    if target.needs_attenuator() && snap.attenuation >= 1.0 {
        warnings.push(format!(
            "{} needs an external attenuator: the DAC's lowest output ({} V) is above its {} V range. Enter the attenuator ratio k.",
            target.label,
            fmt_num(model::DAC_MIN_V * model::BUFFER_GAIN),
            fmt_num(target.valid_vin.1)
        ));
    }
    if !status_fresh {
        warnings.push("DAQ DAC status is not fresh; the output state is unknown.".into());
    } else if !snap.active {
        warnings.push(format!("DAC channel {} is off.", snap.channel));
    }
    if let Some(wave) = snap.wave {
        if let Some(f) = wave.period_hz()
            && f > target.stream_hz / 4.0
        {
            warnings.push(format!(
                "Frequency {} Hz is above a quarter of the {} Hz stream rate.",
                fmt_num(f),
                fmt_num(target.stream_hz)
            ));
        }
        let (lo, hi) = target.reported_range();
        let a = model.reported(wave.offset_v - wave.amplitude_v);
        let b = model.reported(wave.offset_v + wave.amplitude_v);
        let margin = 0.001 * (hi - lo);
        if a.min(b) < lo - margin || a.max(b) > hi + margin {
            warnings.push(format!(
                "The expected signal ({} .. {} V) leaves {}'s valid range ({} .. {} V); readings may clip.",
                fmt_num(a.min(b)),
                fmt_num(a.max(b)),
                target.label,
                fmt_num(lo),
                fmt_num(hi)
            ));
        }
    }
    if snap.active
        && snap
            .samples
            .last()
            .is_none_or(|(t, _)| now_ms - *t > SILENT_MS)
    {
        let hint = match target.id {
            "p6_diff" => " (P6 needs the KG50 input set to P6 differential)",
            "loadcell_kg50" => " (needs the KG50 input set to AMP2)",
            _ => "",
        };
        warnings.push(format!(
            "No samples from {} in the last {} s{hint}.",
            target.label,
            SILENT_MS / 1000
        ));
    }
    if let Some(wave) = snap.wave.filter(|w| !w.modelled()) {
        warnings.push(format!(
            "No expected trace is modelled for a {} output; compare the measured trace with the preview.",
            wave.waveform.name()
        ));
        return (
            Series {
                t_ms: snap.samples.iter().map(|(t, _)| *t).collect(),
                measured: snap.samples.iter().map(|(_, v)| *v).collect(),
                expected: Vec::new(),
            },
            None,
            warnings,
        );
    }
    let Some(wave) = snap.wave.filter(|_| !snap.samples.is_empty()) else {
        return (Series::default(), None, warnings);
    };
    let t_s: Vec<f64> = snap
        .samples
        .iter()
        .map(|(t, _)| *t as f64 / 1000.0)
        .collect();
    let y: Vec<f64> = snap.samples.iter().map(|(_, v)| *v).collect();
    let (expected, result) = analysis::analyse(&t_s, &y, &wave, model);
    (
        Series {
            t_ms: snap.samples.iter().map(|(t, _)| *t).collect(),
            measured: y,
            expected,
        },
        Some(result),
        warnings,
    )
}

fn validation_view(state: &AppState) -> ValidationView {
    let snap = {
        let inner = state.daq_dac.inner.lock().unwrap();
        inner.validation.as_ref().map(|v| Snapshot {
            target: v.target,
            channel: v.channel,
            attenuation: v.attenuation,
            polarity: v.polarity,
            samples: v.samples.iter().copied().collect(),
            wave: v.wave,
            active: inner.channels[usize::from(v.channel)].active.is_some(),
        })
    };
    let Some(snap) = snap else {
        return ValidationView {
            target: None,
            channel: None,
            attenuation: None,
            load_cell_polarity: None,
            warnings: Vec::new(),
            series: Series::default(),
            analysis: None,
        };
    };
    let fresh =
        super::daq_status(state, Instant::now()).is_some_and(|(_, age)| age <= STATUS_FRESH_MS);
    let now_ms = crate::telemetry_task::get_current_timestamp_ms() as i64;
    let (series, analysis, warnings) = evaluate(&snap, fresh, now_ms);
    ValidationView {
        target: Some(snap.target.id),
        channel: Some(snap.channel),
        attenuation: Some(snap.attenuation),
        load_cell_polarity: Some(snap.polarity),
        warnings,
        series,
        analysis,
    }
}

pub(crate) async fn get_validation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_read(&state, &headers).await {
        return response;
    }
    Json(validation_view(&state)).into_response()
}

pub(crate) async fn post_validation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_write(&state, &headers).await {
        return response;
    }
    let request: ValidationRequest = match parse(&body) {
        Ok(r) => r,
        Err(response) => return response,
    };
    {
        let mut inner = state.daq_dac.inner.lock().unwrap();
        match request.target.as_deref() {
            None => inner.validation = None,
            Some(id) => {
                let target = match check_selection(id, request.channel, request.attenuation) {
                    Ok(t) => t,
                    Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
                };
                let active = inner.channels[usize::from(request.channel)].active;
                inner.validation = Some(Validation {
                    target,
                    channel: request.channel,
                    attenuation: request.attenuation,
                    polarity: request.load_cell_polarity,
                    samples: VecDeque::new(),
                    wave: active.map(|a| a.wave),
                    sequence: active.map(|a| a.sequence),
                    last_primary_ms: None,
                });
            }
        }
        state.daq_dac.refresh_collecting(&inner);
    }
    Json(validation_view(&state)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SweepRequest {
    target: String,
    #[serde(default)]
    channel: u8,
    #[serde(default = "unity")]
    attenuation: f64,
    #[serde(default)]
    steps: Option<usize>,
    #[serde(default)]
    dwell_ms: Option<u64>,
    #[serde(default)]
    load_cell_polarity: Polarity,
    /// "linearity" (DC steps, default) or "frequency_response" (sine steps).
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    start_hz: Option<f64>,
    #[serde(default)]
    stop_hz: Option<f64>,
}

#[derive(Serialize)]
struct Progress {
    step: usize,
    steps: usize,
}

#[derive(Serialize)]
struct SweepView {
    state: sweep::State,
    progress: Option<Progress>,
    message: Option<String>,
    report: Option<sweep::Report>,
}

fn sweep_view(state: &AppState) -> SweepView {
    let inner = state.daq_dac.inner.lock().unwrap();
    let s = &inner.sweep;
    SweepView {
        state: s.state,
        progress: (s.state != sweep::State::Idle).then_some(Progress {
            step: s.step,
            steps: s.steps,
        }),
        message: s.message.clone(),
        report: s.report.clone(),
    }
}

pub(crate) async fn get_sweep(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_read(&state, &headers).await {
        return response;
    }
    Json(sweep_view(&state)).into_response()
}

pub(crate) async fn post_sweep(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_write(&state, &headers).await {
        return response;
    }
    let request: SweepRequest = match parse(&body) {
        Ok(r) => r,
        Err(response) => return response,
    };
    let target = match check_selection(&request.target, request.channel, request.attenuation) {
        Ok(t) => t,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };
    let steps = request.steps.unwrap_or(sweep::DEFAULT_STEPS);
    if !(2..=101).contains(&steps) {
        return json_error(StatusCode::BAD_REQUEST, "steps must be 2..101.");
    }
    let dwell_ms = request.dwell_ms.unwrap_or(sweep::DEFAULT_DWELL_MS);
    if !(100..=60_000).contains(&dwell_ms) {
        return json_error(StatusCode::BAD_REQUEST, "dwell_ms must be 100..60000.");
    }
    let model = target.model(request.attenuation, request.load_cell_polarity);
    let levels = model::sweep_levels(target, model, steps);
    let response = match request.preset.as_deref() {
        None | Some("linearity") => None,
        Some("frequency_response") => {
            // A quarter of the stream rate keeps four samples per period.
            let top = target.stream_hz / 4.0;
            let start = request.start_hz.unwrap_or(top / 50.0);
            let stop = request.stop_hz.unwrap_or(top);
            if !(start > 0.0 && stop > start && stop <= waveform::FREQ_MAX_HZ) {
                return json_error(StatusCode::BAD_REQUEST, "Frequency response needs 0 < start_hz < stop_hz <= 10000.");
            }
            if levels.len() < 2 {
                return json_error(StatusCode::BAD_REQUEST, format!("{} has no usable DAC range with this attenuator.", target.label));
            }
            let (lo, hi) = (levels[0], levels[levels.len() - 1]);
            Some(sweep::Response {
                frequencies_hz: sweep::response_frequencies(start, stop, steps.min(25)),
                offset_pin_v: (lo + hi) / 2.0,
                amplitude_pin_v: 0.4 * (hi - lo),
            })
        }
        Some(other) => {
            return json_error(StatusCode::BAD_REQUEST, format!("Unknown sweep preset {other:?}; use linearity or frequency_response."));
        }
    };
    if levels.len() < 2 {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "With k = {} the DAC reaches only {} level(s) inside {}'s range; change the attenuator or polarity.",
                fmt_num(request.attenuation),
                levels.len(),
                target.label
            ),
        );
    }
    let now = Instant::now();
    let status = super::daq_status(&state, now);
    if let Some(refusal) = super::enable_refusal(
        state.local_flight_state_snapshot(),
        status.as_ref().map(|(s, age)| (s, *age)),
    ) {
        return error_response(refusal_error(refusal));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut inner = state.daq_dac.inner.lock().unwrap();
        if inner.sweep.state == sweep::State::Running {
            return json_error(StatusCode::CONFLICT, "A DAC sweep is already running.");
        }
        inner.sweep = sweep::Ctl {
            state: sweep::State::Running,
            step: 0,
            steps: response.as_ref().map_or(levels.len(), |r| r.frequencies_hz.len()),
            message: None,
            report: None,
            cancel: Some(cancel.clone()),
            channel: Some(request.channel),
        };
    }
    let plan = sweep::Plan {
        target,
        channel: request.channel,
        attenuation: request.attenuation,
        polarity: request.load_cell_polarity,
        levels,
        dwell: Duration::from_millis(dwell_ms),
        response,
    };
    tokio::spawn(sweep::run(state.clone(), plan, cancel));
    (StatusCode::ACCEPTED, Json(sweep_view(&state))).into_response()
}

pub(crate) async fn sweep_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_write(&state, &headers).await {
        return response;
    }
    if let Some(cancel) = &state.daq_dac.inner.lock().unwrap().sweep.cancel {
        cancel.store(true, Ordering::Release);
    }
    (StatusCode::ACCEPTED, Json(sweep_view(&state))).into_response()
}

fn bad_request(message: String) -> ApiError {
    (StatusCode::BAD_REQUEST, message)
}

/// The length of the verified shape on the DAQ, to reuse when it still fits.
fn verified_shape(state: &AppState) -> Option<wire::TableStatus> {
    super::daq_status(state, Instant::now())
        .filter(|(_, age)| *age <= STATUS_FRESH_MS)
        .map(|(s, _)| s.table)
        .filter(|t| t.state == wire::TABLE_READY)
}

/// Sample the custom source, then either enable at once (the DAQ already
/// holds this exact shape: a pure amplitude/offset/phase/frequency change) or
/// upload it first in the background.
fn post_arbitrary(state: &Arc<AppState>, request: ConfigRequest) -> Response {
    let Some(source) = request.custom.clone() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "An arbitrary waveform needs a custom source (expression, values or WAV).",
        );
    };
    let on_daq = verified_shape(state);
    let table = match custom::build(&source, request.frequency_hz, on_daq.map(|t| t.len)) {
        Ok(t) => t,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };
    let mut slim = request.clone();
    slim.custom = Some(source.slim());
    slim.frequency_hz = Some(table.frequency_hz);
    let (_, clamped) = match super::plan_arbitrary(&slim, &table, 1) {
        Ok(p) => p,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };
    if on_daq.is_some_and(|t| t.len == table.samples && t.crc == table.shape_crc) {
        state.daq_dac.inner.lock().unwrap().shape = Some((table.shape_crc, table.shape.clone()));
        let (command, clamped) = super::plan_arbitrary(&slim, &table, 1).expect("planned above");
        return match enable_command(state, slim, command, clamped) {
            Ok(Enabled { command, clamped }) => (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "accepted": true,
                    "uploading": false,
                    "sequence": command.sequence,
                    "samples": table.samples,
                    "shape_crc": hex(table.shape_crc),
                    "clamped": clamped,
                    "warnings": table.warnings,
                    "message": format!(
                        "DAC channel {} plays the shape already on the DAQ, re-scaled; no upload needed.",
                        command.channel
                    ),
                })),
            )
                .into_response(),
            Err(error) => error_response(error),
        };
    }
    if let Some(refusal) = super::enable_refusal(
        state.local_flight_state_snapshot(),
        super::daq_status(state, Instant::now()).as_ref().map(|(s, a)| (s, *a)),
    ) {
        return error_response(refusal_error(refusal));
    }
    if crate::gse::capture_operator_origin(state).is_none() {
        return error_response((
            StatusCode::CONFLICT,
            crate::board_recovery::coordinator::admission_closed_message(state, "Driving the DAQ DAC"),
        ));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut inner = state.daq_dac.inner.lock().unwrap();
        if inner.upload.state == UploadState::Running {
            return json_error(StatusCode::CONFLICT, "A DAC shape upload is already running.");
        }
        inner.upload = UploadCtl {
            state: UploadState::Running,
            channel: Some(request.channel),
            phase: Some(upload::Phase::Begin),
            chunk_count: usize::from(table.samples).div_ceil(wire::CHUNK_SAMPLES),
            shape_crc: Some(table.shape_crc),
            cancel: Some(cancel.clone()),
            ..UploadCtl::default()
        };
    }
    let response = serde_json::json!({
        "accepted": true,
        "uploading": true,
        "samples": table.samples,
        "shape_crc": hex(table.shape_crc),
        "clamped": clamped,
        "warnings": table.warnings,
        "message": format!(
            "Uploading a {}-sample shape (CRC {}) to the DAQ; channel {} starts once the DAQ confirms it.",
            table.samples,
            hex(table.shape_crc),
            request.channel
        ),
    });
    tokio::spawn(run_upload(state.clone(), slim, table, cancel));
    (StatusCode::ACCEPTED, Json(response)).into_response()
}

fn upload_finish(state: &AppState, outcome: Result<String, String>) {
    let mut inner = state.daq_dac.inner.lock().unwrap();
    inner.upload.cancel = None;
    match outcome {
        Ok(message) => {
            inner.upload.state = UploadState::Done;
            inner.upload.message = Some(message);
        }
        Err(message) => {
            log::warn!("DAQ DAC shape upload: {message}");
            inner.upload.state = UploadState::Failed;
            inner.upload.message = Some(message);
        }
    }
}

async fn run_upload(state: Arc<AppState>, request: ConfigRequest, table: CustomTable, cancel: Arc<AtomicBool>) {
    let Some(router) = state.topology_router.get() else {
        return upload_finish(&state, Err("DAQ network router is unavailable.".into()));
    };
    let mut up = Uploader::new(rand::random::<u32>() | 1, table.shape.clone(), Instant::now());
    loop {
        if cancel.load(Ordering::Acquire) {
            return upload_finish(&state, Err("Upload cancelled.".into()));
        }
        let now = Instant::now();
        let status = super::daq_status(&state, now);
        if let Some(refusal) = super::enable_refusal(
            state.local_flight_state_snapshot(),
            status.as_ref().map(|(s, a)| (s, *a)),
        ) {
            return upload_finish(&state, Err(format!("Upload stopped: {}", refusal.message)));
        }
        let table_status = status.and_then(|(s, age)| {
            now.checked_sub(Duration::from_millis(age)).map(|at| (s.table, at))
        });
        let messages = up.step(now, table_status);
        for (i, message) in messages.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(upload::CHUNK_GAP).await;
            }
            if let Err(error) = super::send_table(router, message) {
                log::warn!("DAQ DAC table chunk could not be queued: {error}");
            }
        }
        {
            let mut inner = state.daq_dac.inner.lock().unwrap();
            inner.upload.phase = Some(up.phase);
            inner.upload.chunks_confirmed = up.chunks_confirmed;
            inner.upload.resent_chunks = up.resent_chunks;
        }
        match up.phase {
            upload::Phase::Done => break,
            upload::Phase::Failed => {
                return upload_finish(
                    &state,
                    Err(up.error.clone().unwrap_or_else(|| "Upload failed.".into())),
                );
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    state.daq_dac.inner.lock().unwrap().shape = Some((table.shape_crc, table.shape.clone()));
    let outcome = super::plan_arbitrary(&request, &table, 1)
        .map_err(|m| m.to_string())
        .and_then(|(command, clamped)| {
            enable_command(&state, request, command, clamped).map_err(|(_, m)| m)
        })
        .map(|enabled| {
            format!(
                "Shape {} verified on the DAQ ({} chunks, {} resent); channel {} on.",
                hex(table.shape_crc),
                up.chunk_count(),
                up.resent_chunks,
                enabled.command.channel
            )
        });
    upload_finish(&state, outcome);
}

#[derive(Serialize)]
struct PreviewView {
    waveform: Waveform,
    samples: u16,
    update_hz: f64,
    requested_frequency_hz: Option<f64>,
    /// What the DAQ will play (period rate; noise: sample rate).
    frequency_hz: f64,
    frequency_error_ppm: Option<f64>,
    dwell_ms: u32,
    /// CRC-32 the DAQ will report for the play table.
    table_crc: String,
    /// Arbitrary: the shape's CRC and whether a new upload is needed.
    shape_crc: Option<String>,
    upload_needed: Option<bool>,
    /// One period (or the staircase/noise table) in P10 connector volts.
    connector_v: Vec<f64>,
    /// Arbitrary: the normalized shape (-1..1) before amplitude/offset.
    shape: Option<Vec<f64>>,
    duty_achieved_pct: Option<f64>,
    clamped: Vec<&'static str>,
    warnings: Vec<String>,
}

/// What a configuration will play, without sending anything: the exact
/// table, achieved frequency, CRC and warnings.
pub(super) fn preview(state: Option<&AppState>, request: &ConfigRequest) -> Result<serde_json::Value, ApiError> {
    let mut warnings = Vec::new();
    let (command, clamped, plan, shape, upload_needed) = if request.waveform == Waveform::Arbitrary {
        let source = request
            .custom
            .as_ref()
            .ok_or_else(|| bad_request("An arbitrary waveform needs a custom source.".into()))?;
        let on_daq = state.and_then(verified_shape);
        let table = custom::build(source, request.frequency_hz, on_daq.map(|t| t.len)).map_err(bad_request)?;
        warnings.extend(table.warnings.iter().cloned());
        let mut r = request.clone();
        r.frequency_hz = Some(table.frequency_hz);
        let (command, clamped) = super::plan_arbitrary(&r, &table, 1).map_err(bad_request)?;
        let plan = waveform::plan_shape(&command, &table.shape)
            .ok_or_else(|| bad_request("The DAQ timer cannot reach this rate.".into()))?;
        let needed = !on_daq.is_some_and(|t| t.len == table.samples && t.crc == table.shape_crc);
        if table.depends_on_frequency {
            warnings.push("The expression uses t (or T, f, fs), so its shape changes with the frequency; a frequency change re-samples and re-uploads it.".into());
        }
        (command, clamped, plan, Some((table.shape_crc, table.shape)), Some(needed))
    } else {
        let (command, clamped) = super::plan_command(request, 1).map_err(bad_request)?;
        let plan = waveform::plan(&command)
            .ok_or_else(|| bad_request("The DAQ timer cannot reach this rate.".into()))?;
        (command, clamped, plan, None, None)
    };
    let w = command.waveform;
    let requested = w.uses_frequency().then(|| f64::from(command.frequency_hz));
    let error_ppm = requested
        .filter(|f| *f > 0.0 && plan.frequency_hz > 0.0)
        .map(|f| (plan.frequency_hz - f) / f * 1e6);
    if w == Waveform::Chirp {
        warnings.push(format!(
            "The chirp table is one sine period of {} samples; the DAQ retunes it every 50 ms from {} Hz to {} Hz over {} s ({}).",
            plan.samples,
            command.frequency_hz,
            command.frequency_stop_hz,
            f64::from(command.sweep_ms) / 1000.0,
            if command.flags & wire::FLAG_LOG_SWEEP != 0 { "logarithmic" } else { "linear" }
        ));
    }
    if w.is_periodic() && plan.samples < 32 {
        warnings.push(format!(
            "Only {} samples per period at this frequency (200 kS/s limit): expect visible steps.",
            plan.samples
        ));
    }
    let duty = (w == Waveform::Square).then(|| {
        let high = plan.table.iter().filter(|c| **c > waveform::volts_to_code(f64::from(command.offset_v))).count();
        high as f64 * 100.0 / f64::from(plan.samples)
    });
    let view = PreviewView {
        waveform: w,
        samples: plan.samples,
        update_hz: plan.update_hz,
        requested_frequency_hz: requested,
        frequency_hz: plan.frequency_hz,
        frequency_error_ppm: error_ppm,
        dwell_ms: plan.dwell_ms,
        table_crc: hex(plan.table_crc),
        shape_crc: shape.as_ref().map(|(c, _)| hex(*c)),
        upload_needed,
        connector_v: plan
            .table
            .iter()
            .map(|c| waveform::code_to_volts(*c) * model::BUFFER_GAIN)
            .collect(),
        shape: shape.map(|(_, s)| s.iter().map(|q| f64::from(*q) / 32767.0).collect()),
        duty_achieved_pct: duty,
        clamped,
        warnings,
    };
    Ok(serde_json::to_value(view).expect("preview serializes"))
}

pub(crate) async fn post_preview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_read(&state, &headers).await {
        return response;
    }
    let request: ConfigRequest = match parse(&body) {
        Ok(r) => r,
        Err(response) => return response,
    };
    match preview(Some(&state), &request) {
        Ok(view) => Json(view).into_response(),
        Err(error) => error_response(error),
    }
}
