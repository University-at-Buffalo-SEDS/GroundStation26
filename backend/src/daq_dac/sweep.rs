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

pub(super) async fn run(state: Arc<AppState>, plan: Plan, cancel: Arc<AtomicBool>) {
    let started = crate::telemetry_task::get_current_timestamp_ms();
    let model = plan.target.model(plan.attenuation, plan.polarity);
    let mut points = Vec::new();
    let result = run_steps(&state, &plan, model, &cancel, &mut points).await;
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
    if !report.points.is_empty() {
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
    let mut inner = state.daq_dac.inner.lock().unwrap();
    inner.sweep.state = outcome;
    inner.sweep.message = message;
    inner.sweep.report = Some(report);
    inner.sweep.cancel = None;
    inner.sweep.channel = None;
}
