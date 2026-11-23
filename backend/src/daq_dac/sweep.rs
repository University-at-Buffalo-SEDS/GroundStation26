//! DC sweep across a target's valid range with a least-squares fit report.
use super::analysis::{self, Check, SweepPoint};
use super::model::{PathModel, Polarity, Target, Tolerances};
use super::wire::Waveform;
use super::{Collector, ConfigRequest, Runtime};
use crate::state::AppState;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) const DEFAULT_STEPS: usize = 11;
pub(super) const DEFAULT_DWELL_MS: u64 = 3_000;
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(100);
const MIN_SAMPLES: usize = 3;
/// Applied DC level must match the commanded one this closely (pin volts).
const LEVEL_MATCH_V: f64 = 0.005;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    #[default]
    Idle,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Report {
    pub target: &'static str,
    pub channel: u8,
    pub attenuation: f64,
    pub load_cell_polarity: Polarity,
    pub started_utc_ms: u64,
    pub finished_utc_ms: u64,
    pub points: Vec<SweepPoint>,
    pub offset_error: Option<f64>,
    pub gain_error_pct: Option<f64>,
    pub max_nonlinearity_pct_fs: Option<f64>,
    pub noise_rms: Option<f64>,
    pub tolerances: Tolerances,
    pub checks: Vec<Check>,
    pub pass: bool,
    pub saved_path: Option<String>,
    /// How the sweep ended and why, also saved in the file.
    pub outcome: State,
    pub message: Option<String>,
    /// Frequency-response preset only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub frequency_points: Vec<FrequencyPoint>,
}

#[derive(Default)]
pub(super) struct Ctl {
    pub state: State,
    pub step: usize,
    pub steps: usize,
    pub message: Option<String>,
    pub report: Option<Report>,
    pub cancel: Option<Arc<AtomicBool>>,
    pub channel: Option<u8>,
}

pub(super) struct Plan {
    pub target: &'static Target,
    pub channel: u8,
    pub attenuation: f64,
    pub polarity: Polarity,
    pub levels: Vec<f64>,
    pub dwell: Duration,
    /// Frequency-response preset: sine steps instead of DC levels.
    pub response: Option<Response>,
}

/// A frequency-response sweep: a sine of fixed pin amplitude stepped through
/// `frequencies_hz`, each point's gain fitted at the DAQ's achieved frequency.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Response {
    pub frequencies_hz: Vec<f64>,
    pub offset_pin_v: f64,
    pub amplitude_pin_v: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct FrequencyPoint {
    pub frequency_hz: f64,
    pub expected_amplitude: f64,
    pub measured_amplitude: Option<f64>,
    pub gain_db: Option<f64>,
    pub samples: usize,
}

/// Fit one frequency-response point: the fundamental's amplitude at the
/// achieved frequency against the path model's expected amplitude.
pub(super) fn response_point(
    times_ms: &[i64],
    values: &[f64],
    frequency_hz: f64,
    expected_amplitude: f64,
) -> FrequencyPoint {
    let n = times_ms.len().min(values.len());
    let t0 = times_ms.first().copied().unwrap_or(0);
    let t: Vec<f64> = times_ms[..n].iter().map(|t| (t - t0) as f64 / 1000.0).collect();
    let measured = analysis::fit_fundamental(&t, &values[..n], frequency_hz).map(|(_, a, _)| a);
    FrequencyPoint {
        frequency_hz,
        expected_amplitude,
        measured_amplitude: measured,
        gain_db: measured
            .filter(|_| expected_amplitude > 0.0)
            .map(|m| 20.0 * (m / expected_amplitude).max(1e-12).log10()),
        samples: n,
    }
}

/// Frequency-response check: every point within +/-3 dB.
pub(super) fn response_check(points: &[FrequencyPoint]) -> Option<Check> {
    let worst = points
        .iter()
        .filter_map(|p| p.gain_db)
        .fold(None::<f64>, |w, g| Some(w.map_or(g.abs(), |w| w.max(g.abs()))))?;
    Some(Check {
        name: "max_gain_deviation_db",
        value: worst,
        limit: 3.0,
        pass: worst <= 3.0,
    })
}

/// Log-spaced frequencies, capped at a quarter of the target's stream rate.
pub(super) fn response_frequencies(start_hz: f64, stop_hz: f64, points: usize) -> Vec<f64> {
    let points = points.max(2);
    (0..points)
        .map(|i| start_hz * (stop_hz / start_hz).powf(i as f64 / (points - 1) as f64))
        .collect()
}

enum Stop {
    Cancelled,
    Failed(String),
}

/// Build the report from measured points (fit fields stay empty below two points).
pub(super) fn build_report(
    plan_target: &'static Target,
    channel: u8,
    attenuation: f64,
    polarity: Polarity,
    points: Vec<SweepPoint>,
    started_utc_ms: u64,
    finished_utc_ms: u64,
    outcome: State,
    message: Option<String>,
) -> Report {
    let tolerances = plan_target.tolerances;
    let fit = analysis::evaluate_sweep(&points, plan_target.full_scale(), &tolerances);
    let pass = outcome == State::Done && fit.as_ref().is_some_and(|f| f.pass);
    Report {
        target: plan_target.id,
        channel,
        attenuation,
        load_cell_polarity: polarity,
        started_utc_ms,
        finished_utc_ms,
        points,
        offset_error: fit.as_ref().map(|f| f.offset_error),
        gain_error_pct: fit.as_ref().map(|f| f.gain_error_pct),
        max_nonlinearity_pct_fs: fit.as_ref().map(|f| f.max_nonlinearity_pct_fs),
        noise_rms: fit.as_ref().map(|f| f.noise_rms),
        tolerances,
        checks: fit.map(|f| f.checks).unwrap_or_default(),
        pass,
        saved_path: None,
        outcome,
        message,
        frequency_points: Vec::new(),
    }
}

/// Next to the current recording, or in the backend data directory.
pub(super) fn report_dir(state: &AppState) -> PathBuf {
    let recording = state.recording_status_snapshot();
    if recording.mode != crate::telemetry_db::RecordingModeWire::Idle
        && let Some(path) = recording.db_path.filter(|p| p != "sqlite::memory:")
        && let Some(parent) = Path::new(&path).parent()
    {
        return parent.to_path_buf();
    }
    Path::new(&state.placeholder_db_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Runtime {
    fn set_collector(&self, collector: Option<Collector>) -> Option<Collector> {
        let mut inner = self.inner.lock().unwrap();
        let previous = std::mem::replace(&mut inner.collector, collector);
        self.refresh_collecting(&inner);
        previous
    }
    fn set_progress(&self, step: usize) {
        self.inner.lock().unwrap().sweep.step = step;
    }
}

async fn pause(
    state: &AppState,
    plan: &Plan,
    cancel: &AtomicBool,
    seq: u32,
    total: Duration,
) -> Result<(), Stop> {
    let until = Instant::now() + total;
    loop {
        check(state, plan, cancel, seq)?;
        let now = Instant::now();
        if now >= until {
            return Ok(());
        }
        tokio::time::sleep(POLL.min(until - now)).await;
    }
}

/// Cancelled, or the keepalive dropped the output (state change, abort, stale status).
fn check(state: &AppState, plan: &Plan, cancel: &AtomicBool, seq: u32) -> Result<(), Stop> {
    if cancel.load(Ordering::Acquire) {
        return Err(Stop::Cancelled);
    }
    if state.daq_dac.desired_sequence(usize::from(plan.channel)) != Some(seq) {
        return Err(Stop::Failed(state.daq_dac.last_drop().unwrap_or_else(
            || "DAC output was turned off during the sweep.".to_string(),
        )));
    }
    Ok(())
}

async fn run_steps(
    state: &AppState,
    plan: &Plan,
    model: PathModel,
    cancel: &AtomicBool,
    points: &mut Vec<SweepPoint>,
) -> Result<(), Stop> {
    let channel = usize::from(plan.channel);
    for (i, pin) in plan.levels.iter().copied().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Err(Stop::Cancelled);
        }
        state.daq_dac.set_progress(i + 1);
        let request = ConfigRequest {
            channel: plan.channel,
            waveform: Waveform::Dc,
            amplitude_v: 0.0,
            offset_v: pin * super::model::BUFFER_GAIN,
            frequency_hz: None,
            steps: None,
            dwell_ms: None,
            ..Default::default()
        };
        let enabled =
            super::api::enable(state, &request).map_err(|(_, message)| Stop::Failed(message))?;
        let seq = enabled.command.sequence;
        let commanded = f64::from(enabled.command.offset_v);
        // Wait for the DAQ to confirm this exact level.
        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        let applied = loop {
            check(state, plan, cancel, seq)?;
            if let Some((status, _)) = super::daq_status(state, Instant::now()) {
                let ch = status.channels[channel];
                if ch.active
                    && ch.sequence == seq
                    && (f64::from(ch.offset_v) - commanded).abs() < LEVEL_MATCH_V
                {
                    break f64::from(ch.offset_v);
                }
            }
            if Instant::now() >= deadline {
                return Err(Stop::Failed(format!(
                    "The DAQ did not confirm DC level {:.3} V (pin) on channel {} within {} s.",
                    commanded,
                    plan.channel,
                    CONFIRM_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(POLL).await;
        };
        pause(state, plan, cancel, seq, SETTLE).await?;
        state.daq_dac.set_collector(Some(Collector {
            target: plan.target,
            channel: plan.channel,
            values: Vec::new(),
            times: Vec::new(),
            readbacks: Vec::new(),
            last_primary_ms: None,
        }));
        let waited = pause(state, plan, cancel, seq, plan.dwell).await;
        let collected = state.daq_dac.set_collector(None);
        waited?;
        let collected = collected.expect("collector set above");
        if collected.values.len() < MIN_SAMPLES {
            return Err(Stop::Failed(format!(
                "Only {} samples from {} in {:.1} s at {:.3} V (pin); check the stream and the loopback cable.",
                collected.values.len(),
                plan.target.label,
                plan.dwell.as_secs_f64(),
                applied
            )));
        }
        points.push(SweepPoint {
            dac_pin_v: applied,
            expected: model.reported(applied),
            measured_mean: analysis::mean(&collected.values).unwrap_or(f64::NAN),
            measured_std: analysis::std_dev(&collected.values).unwrap_or(f64::NAN),
            readback_v: analysis::mean(&collected.readbacks),
            samples: collected.values.len(),
        });
    }
    Ok(())
}

async fn run_response(
    state: &AppState,
    plan: &Plan,
    model: PathModel,
    cancel: &AtomicBool,
    points: &mut Vec<FrequencyPoint>,
) -> Result<(), Stop> {
    let response = plan.response.as_ref().expect("response plan");
    let channel = usize::from(plan.channel);
    for (i, f) in response.frequencies_hz.iter().copied().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Err(Stop::Cancelled);
        }
        state.daq_dac.set_progress(i + 1);
        let request = ConfigRequest {
            channel: plan.channel,
            waveform: Waveform::Sine,
            amplitude_v: response.amplitude_pin_v * super::model::BUFFER_GAIN,
            offset_v: response.offset_pin_v * super::model::BUFFER_GAIN,
            frequency_hz: Some(f),
            ..Default::default()
        };
        let enabled =
            super::api::enable(state, &request).map_err(|(_, message)| Stop::Failed(message))?;
        let seq = enabled.command.sequence;
        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        let (achieved, amplitude_pin) = loop {
            check(state, plan, cancel, seq)?;
            if let Some((status, _)) = super::daq_status(state, Instant::now()) {
                let ch = status.channels[channel];
                if ch.active && ch.sequence == seq && ch.frequency_hz > 0.0 {
                    break (f64::from(ch.frequency_hz), f64::from(ch.amplitude_v));
                }
            }
            if Instant::now() >= deadline {
                return Err(Stop::Failed(format!(
                    "The DAQ did not confirm the {f} Hz sine on channel {} within {} s.",
                    plan.channel,
                    CONFIRM_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(POLL).await;
        };
        pause(state, plan, cancel, seq, SETTLE).await?;
        state.daq_dac.set_collector(Some(Collector {
            target: plan.target,
            channel: plan.channel,
            values: Vec::new(),
            times: Vec::new(),
            readbacks: Vec::new(),
            last_primary_ms: None,
        }));
        // At least four periods per point.
        let dwell = plan.dwell.max(Duration::from_secs_f64(4.0 / achieved));
        let waited = pause(state, plan, cancel, seq, dwell).await;
        let collected = state.daq_dac.set_collector(None);
        waited?;
        let collected = collected.expect("collector set above");
        if collected.values.len() < 8 {
            return Err(Stop::Failed(format!(
                "Only {} samples from {} at {f} Hz; check the stream and the loopback cable.",
                collected.values.len(),
                plan.target.label
            )));
        }
        let expected = (model.slope * amplitude_pin).abs();
        points.push(response_point(&collected.times, &collected.values, achieved, expected));
    }
    Ok(())
}

pub(super) async fn run(state: Arc<AppState>, plan: Plan, cancel: Arc<AtomicBool>) {
    let started = crate::telemetry_task::get_current_timestamp_ms();
    let capture_key = format!("sweep-{started}");
    crate::recording_store::begin(
        &state,
        crate::recording_store::BeginRequest {
            kind: crate::recording_store::EventKind::DaqDac,
            key: &capture_key,
            label: &format!("DAC sweep {} ch{}", plan.target.label, plan.channel),
            operator: None,
            details: serde_json::json!({"channel": plan.channel, "attenuation": plan.attenuation}),
        },
    );
    let model = plan.target.model(plan.attenuation, plan.polarity);
    let mut points = Vec::new();
    let mut response_points = Vec::new();
    let result = if plan.response.is_some() {
        run_response(&state, &plan, model, &cancel, &mut response_points).await
    } else {
        run_steps(&state, &plan, model, &cancel, &mut points).await
    };
    // Always leave the DAC off, whatever happened.
    state.daq_dac.set_collector(None);
    let disable = state
        .daq_dac
        .request_disable(Some(plan.channel), Instant::now());
    if let Some(router) = state.topology_router.get()
        && let Err(error) = super::send(router, &disable)
    {
        log::warn!("DAQ DAC sweep disable could not be queued: {error}");
    }
    state.daq_dac.wake.notify_one();
    let (outcome, mut message) = match result {
        Ok(()) => (State::Done, None),
        Err(Stop::Cancelled) => (
            State::Cancelled,
            Some("Sweep cancelled; DAC output turned off.".to_string()),
        ),
        Err(Stop::Failed(message)) => (State::Failed, Some(message)),
    };
    let mut report = build_report(
        plan.target,
        plan.channel,
        plan.attenuation,
        plan.polarity,
        points,
        started,
        crate::telemetry_task::get_current_timestamp_ms(),
        outcome,
        message.clone(),
    );
    if plan.response.is_some() {
        let check = response_check(&response_points);
        report.pass = outcome == State::Done && check.as_ref().is_some_and(|c| c.pass);
        report.checks = check.into_iter().collect();
        report.frequency_points = response_points;
    }
    if !report.points.is_empty() || !report.frequency_points.is_empty() {
        let path = report_dir(&state).join(format!("daq_dac_sweep_{started}.json"));
        report.saved_path = Some(path.display().to_string());
        let saved = match serde_json::to_vec_pretty(&report) {
            Ok(bytes) => tokio::fs::write(&path, bytes)
                .await
                .map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = saved {
            report.saved_path = None;
            let note = format!(
                "Sweep report could not be saved to {}: {error}",
                path.display()
            );
            log::warn!("{note}");
            message = Some(match message {
                Some(m) => format!("{m} {note}"),
                None => note,
            });
        }
    }
    crate::recording_store::end(
        &state,
        crate::recording_store::EventKind::DaqDac,
        &capture_key,
        &format!("{outcome:?}{}", message.as_deref().map(|m| format!(": {m}")).unwrap_or_default()),
    );
    let mut inner = state.daq_dac.inner.lock().unwrap();
    inner.sweep.state = outcome;
    inner.sweep.message = message;
    inner.sweep.report = Some(report);
    inner.sweep.cancel = None;
    inner.sweep.channel = None;
}
