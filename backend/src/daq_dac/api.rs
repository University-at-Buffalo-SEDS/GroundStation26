//! HTTP API for the DAC signal generator. Errors are JSON `{"error": "..."}`.
use super::analysis::{self, Analysis};
use super::model::{self, Polarity, TARGETS, Target};
use super::sweep;
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
    let (mut command, clamped) =
        super::plan_command(request, 1).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
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
    state.daq_dac.set_desired(request.clone(), command, now);
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
    // A sweep driving this channel stops too.
    {
        let inner = state.daq_dac.inner.lock().unwrap();
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
    }
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
            steps: levels.len(),
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
