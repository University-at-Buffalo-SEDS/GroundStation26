//! DAQ DAC signal generator and ADC validation.
//!
//! The GS drives one of the DAQ's two DAC outputs (through the x2 buffer on
//! P10) into an ADC input with a loopback cable, then compares what that input
//! reports with what the DAC should produce. Output is allowed only while the
//! GS and the DAQ both see Startup or Idle. The DAQ holds a lease: the GS
//! re-sends the desired command about every second, and stops (and sends
//! disable) when the flight state leaves Startup/Idle, on abort, or when DAQ
//! status goes stale. See `.workspace-progress/.../SPEC.md` for the contract.
mod analysis;
mod api;
mod audio;
mod custom;
mod expr;
mod model;
mod sweep;
mod upload;
mod waveform;
mod wire;

pub(crate) use api::{
    disable, get_status, get_sweep, get_validation, post_config, post_preview, post_sweep,
    post_validation, sweep_cancel,
};
pub(crate) use wire::Status;

use crate::state::AppState;
use crate::types::{BoardDiagnostics, FlightState};
use analysis::Wave;
use model::{Polarity, Target};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wire::{Command, Waveform};

pub(crate) const COMMAND_NAME: &str = "DaqDacControl";
/// DAQ status older than this cannot hold an output on.
pub(crate) const STATUS_FRESH_MS: u64 = 3_000;
const KEEPALIVE: Duration = Duration::from_secs(1);
const LEASE_MS: u16 = 3_000;
const DISABLE_REPEAT: Duration = Duration::from_millis(250);
const DISABLE_FOR: Duration = Duration::from_secs(5);
/// A channel the GS is not driving may be shown active this long (status lag)
/// before the GS turns it off.
const ORPHAN_GRACE: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(200);
const SERIES_WINDOW_MS: i64 = 30_000;
const SERIES_MAX: usize = 4_096;
/// Samples this soon after a new configuration is confirmed are transients.
const SETTLE: Duration = Duration::from_millis(500);
/// A fallback stream counts only while the primary stream has been silent this long.
const FALLBACK_AFTER_MS: i64 = 2_000;

/// What the operator asked for, in connector volts at P10 (before any
/// external attenuator). DAQ-Board docs/dac-waveforms.md defines each wave.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigRequest {
    pub channel: u8,
    pub waveform: Waveform,
    #[serde(default)]
    pub amplitude_v: f64,
    #[serde(default)]
    pub offset_v: f64,
    /// Periodic: frequency. Chirp: start. Noise: sample rate. Arbitrary:
    /// table repetitions per second (WAV: derived from the playback rate).
    #[serde(default)]
    pub frequency_hz: Option<f64>,
    #[serde(default)]
    pub steps: Option<u16>,
    #[serde(default)]
    pub dwell_ms: Option<u32>,
    /// Start phase, degrees (periodic and chirp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_deg: Option<f64>,
    /// Square high time, percent (1..99; default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty_pct: Option<f64>,
    /// Sawtooth falls instead of rising.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ramp_down: bool,
    /// Chirp end frequency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_stop_hz: Option<f64>,
    /// Chirp sweep duration, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_s: Option<f64>,
    /// Chirp sweeps logarithmically.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub log_sweep: bool,
    /// Arbitrary: the table source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<custom::CustomSource>,
}

/// Clamp a request to the DAC's linear range and build the pin-volt command.
/// Returns the command and the names of the fields the GS clamped. An
/// arbitrary request is planned by `custom::build` and the upload instead.
pub(crate) fn plan_command(
    request: &ConfigRequest,
    sequence: u32,
) -> Result<(Command, Vec<&'static str>), String> {
    if request.channel > 1 {
        return Err(format!(
            "DAC channel {} does not exist; use 0 (P10 pin 2) or 1 (P10 pin 5).",
            request.channel
        ));
    }
    let w = request.waveform;
    if w == Waveform::Arbitrary {
        return Err("An arbitrary waveform needs a custom table (expression, values or WAV).".into());
    }
    if request.custom.is_some() {
        return Err(format!("A {} wave takes no custom table.", w.name()));
    }
    if !request.offset_v.is_finite() || !request.amplitude_v.is_finite() {
        return Err("Offset and amplitude must be finite numbers of volts.".into());
    }
    if request.amplitude_v < 0.0 {
        return Err("Amplitude is a peak value and cannot be negative.".into());
    }
    let uses_frequency = w.uses_frequency();
    let frequency = request.frequency_hz.unwrap_or(1.0);
    if !frequency.is_finite() || (uses_frequency && frequency <= 0.0) {
        return Err("Frequency must be a positive number of hertz.".into());
    }
    if uses_frequency && request.frequency_hz.is_none() {
        return Err(format!("A {} wave needs frequency_hz.", w.name()));
    }
    for (value, name) in [
        (request.phase_deg, "Phase"),
        (request.duty_pct, "Duty"),
        (request.frequency_stop_hz, "Stop frequency"),
        (request.sweep_s, "Sweep time"),
    ] {
        if value.is_some_and(|v| !v.is_finite()) {
            return Err(format!("{name} must be a finite number."));
        }
    }
    let mut clamped = Vec::new();
    let mut clamp = |value: f64, lo: f64, hi: f64, name: &'static str, relevant: bool| {
        let c = value.clamp(lo, hi);
        if relevant && (c - value).abs() > 1e-9 {
            clamped.push(name);
        }
        c
    };
    let offset = clamp(
        request.offset_v / model::BUFFER_GAIN,
        model::DAC_MIN_V,
        model::DAC_MAX_V,
        "offset",
        true,
    );
    let amplitude = if w == Waveform::Dc {
        0.0
    } else {
        let room = (offset - model::DAC_MIN_V).min(model::DAC_MAX_V - offset);
        clamp(
            request.amplitude_v / model::BUFFER_GAIN,
            0.0,
            room,
            "amplitude",
            true,
        )
    };
    let (f_lo, f_hi) = if w == Waveform::Noise {
        (waveform::NOISE_RATE_MIN_HZ, waveform::UPDATE_MAX_HZ)
    } else {
        (waveform::FREQ_MIN_HZ, waveform::FREQ_MAX_HZ)
    };
    let frequency = clamp(frequency, f_lo, f_hi, "frequency", uses_frequency);
    let staircase = w == Waveform::Staircase;
    let steps = clamp(
        f64::from(request.steps.unwrap_or(8)),
        f64::from(waveform::STEPS_MIN),
        f64::from(waveform::STEPS_MAX),
        "steps",
        staircase,
    );
    let dwell = clamp(
        f64::from(request.dwell_ms.unwrap_or(1000)),
        f64::from(waveform::DWELL_MIN_MS),
        f64::from(waveform::DWELL_MAX_MS),
        "dwell",
        staircase,
    );
    let phased = w.is_periodic() || w == Waveform::Chirp;
    let phase_q16 = if phased {
        let turns = request.phase_deg.unwrap_or(0.0).rem_euclid(360.0) / 360.0;
        ((turns * 65536.0).round() as u32 % 65536) as u16
    } else {
        0
    };
    let square = w == Waveform::Square;
    let duty_q16 = if square {
        let pct = clamp(request.duty_pct.unwrap_or(50.0), 1.0, 99.0, "duty", true);
        ((pct / 100.0 * 65536.0).round() as u16).clamp(waveform::DUTY_MIN_Q16, waveform::DUTY_MAX_Q16)
    } else {
        0
    };
    let chirp = w == Waveform::Chirp;
    let stop = if chirp {
        let stop = request
            .frequency_stop_hz
            .ok_or("A chirp needs frequency_stop_hz.")?;
        if stop <= 0.0 {
            return Err("Stop frequency must be a positive number of hertz.".into());
        }
        clamp(stop, waveform::FREQ_MIN_HZ, waveform::FREQ_MAX_HZ, "frequency", true)
    } else {
        0.0
    };
    let sweep_ms = if chirp {
        let s = request.sweep_s.ok_or("A chirp needs sweep_s.")?;
        clamp(
            s * 1000.0,
            f64::from(waveform::SWEEP_MIN_MS),
            f64::from(waveform::SWEEP_MAX_MS),
            "sweep",
            true,
        )
        .round() as u32
    } else {
        0
    };
    let flags = match w {
        Waveform::Sawtooth if request.ramp_down => wire::FLAG_RAMP_DOWN,
        Waveform::Chirp if request.log_sweep => wire::FLAG_LOG_SWEEP,
        _ => 0,
    };
    // Exactly the DAQ's float clamp, so it applies these values unchanged.
    let (offset_f, amplitude_f) = waveform::clamp_pin(offset as f32, amplitude as f32, w != Waveform::Dc);
    Ok((
        Command {
            channel: request.channel,
            enable: true,
            waveform: w,
            flags,
            amplitude_v: amplitude_f,
            offset_v: offset_f,
            frequency_hz: if uses_frequency { frequency as f32 } else { 0.0 },
            frequency_stop_hz: stop as f32,
            steps: if staircase { steps as u16 } else { 0 },
            lease_ms: LEASE_MS,
            phase_q16,
            duty_q16,
            table_len: 0,
            dwell_ms: if staircase { dwell as u32 } else { 0 },
            sweep_ms,
            table_crc: 0,
            sequence,
        },
        clamped,
    ))
}

/// The command that plays an uploaded normalized shape: amplitude, offset,
/// phase and frequency clamp exactly as for a sine, then the update rate
/// (frequency x length) is held within the DAC limit.
pub(crate) fn plan_arbitrary(
    request: &ConfigRequest,
    table: &custom::CustomTable,
    sequence: u32,
) -> Result<(Command, Vec<&'static str>), String> {
    let as_sine = ConfigRequest {
        waveform: Waveform::Sine,
        custom: None,
        duty_pct: None,
        frequency_hz: Some(table.frequency_hz),
        ..request.clone()
    };
    let (mut command, mut clamped) = plan_command(&as_sine, sequence)?;
    command.waveform = Waveform::Arbitrary;
    command.table_len = table.samples;
    command.table_crc = table.shape_crc;
    let top = ((waveform::UPDATE_MAX_HZ / f64::from(table.samples)) as f32).min(waveform::FREQ_MAX_HZ as f32);
    if command.frequency_hz > top {
        command.frequency_hz = top;
        clamped.push("frequency");
    }
    Ok((command, clamped))
}

fn flight_state_allows(state: FlightState) -> bool {
    matches!(state, FlightState::Startup | FlightState::Idle)
}

pub(crate) fn daq_flight_state_name(code: u8) -> String {
    crate::types::NetworkFlightState::from_u8(code)
        .map(|s| s.label().to_string())
        .unwrap_or_else(|| {
            if code == 0xFF {
                "unknown".into()
            } else {
                format!("code {code}")
            }
        })
}

/// Why enabling would be refused now, with the HTTP status to use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub status: u16,
    pub message: String,
}
fn refuse(status: u16, message: String) -> Option<Refusal> {
    Some(Refusal { status, message })
}

/// GS gate AND DAQ gate. Disable is never gated.
pub(crate) fn enable_refusal(
    flight_state: FlightState,
    status: Option<(&Status, u64)>,
) -> Option<Refusal> {
    if !flight_state_allows(flight_state) {
        return refuse(
            409,
            format!(
                "Flight state is {flight_state:?}; DAC output is allowed only in Startup or Idle."
            ),
        );
    }
    let Some((status, age_ms)) = status else {
        return refuse(
            503,
            "No DAQ DAC status has been received; DAC output needs a live DAQ running DAC firmware."
                .into(),
        );
    };
    if age_ms > STATUS_FRESH_MS {
        return refuse(
            503,
            format!(
                "DAQ DAC status is stale ({:.1} s old); DAC output needs fresh DAQ status.",
                age_ms as f64 / 1000.0
            ),
        );
    }
    match status.gate {
        0 => {}
        1 => {
            return refuse(
                409,
                format!(
                    "The DAQ reports flight state {}; it allows DAC output only in Startup or Idle.",
                    daq_flight_state_name(status.flight_state)
                ),
            );
        }
        2 => {
            return refuse(
                409,
                "The DAQ does not know the flight state yet; it refuses DAC output until it does."
                    .into(),
            );
        }
        other => {
            return refuse(
                409,
                format!("The DAQ refuses DAC output (gate code {other})."),
            );
        }
    }
    if status.flags & wire::FLAG_HARDWARE_READY == 0 {
        return refuse(503, "The DAQ reports its DAC hardware is not ready.".into());
    }
    None
}

impl Status {
    /// Board diagnostics view: whether any DAC output is driven.
    pub(crate) fn overlay(&self, report: &mut BoardDiagnostics, received_age_ms: u64) {
        report.dac_status_received_age_ms = Some(received_age_ms);
        report.dac_active = Some(self.any_active());
    }
}

#[derive(Clone, Debug)]
struct Desired {
    request: ConfigRequest,
    command: Command,
    last_sent: Option<Instant>,
}

#[derive(Clone, Copy, Debug)]
struct Disabling {
    sequence: u32,
    started: Instant,
    last_sent: Option<Instant>,
}

/// The configuration a channel's status confirms it is driving.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ActiveConfig {
    sequence: u32,
    wave: Wave,
    since: Instant,
}

#[derive(Default)]
struct ChannelCtl {
    desired: Option<Desired>,
    disabling: Option<Disabling>,
    /// Last time the GS wanted this channel on.
    desired_at: Option<Instant>,
    active: Option<ActiveConfig>,
}

struct Validation {
    target: &'static Target,
    channel: u8,
    attenuation: f64,
    polarity: Polarity,
    samples: VecDeque<(i64, f64)>,
    wave: Option<Wave>,
    sequence: Option<u32>,
    last_primary_ms: Option<i64>,
}

struct Collector {
    target: &'static Target,
    channel: u8,
    values: Vec<f64>,
    /// Sample times (ms, GS clock), parallel to `values`.
    times: Vec<i64>,
    readbacks: Vec<f64>,
    last_primary_ms: Option<i64>,
}

#[derive(Default)]
struct Inner {
    sequence: u32,
    channels: [ChannelCtl; 2],
    abort_pending: bool,
    abort_seen: bool,
    last_drop: Option<String>,
    validation: Option<Validation>,
    sweep: sweep::Ctl,
    collector: Option<Collector>,
    upload: api::UploadCtl,
    /// The last shape uploaded (or found verified on the DAQ), by CRC, to
    /// predict the CRC each arbitrary channel should report.
    shape: Option<(u32, Vec<i16>)>,
}

impl Inner {
    fn next_sequence(&mut self) -> u32 {
        if self.sequence == 0 {
            // A fresh process must not reuse the sequence the DAQ last accepted.
            self.sequence = rand::random::<u32>() >> 4;
        }
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.sequence
    }
    /// One disable sequence for all the given channels (sent as "all" when both).
    fn start_disable(&mut self, channels: &[usize], now: Instant) {
        if channels.is_empty() {
            return;
        }
        let sequence = self.next_sequence();
        for &channel in channels {
            let ctl = &mut self.channels[channel];
            if ctl.desired.take().is_some() {
                ctl.desired_at = Some(now);
            }
            ctl.disabling = Some(Disabling {
                sequence,
                started: now,
                last_sent: None,
            });
        }
    }
}

#[derive(Default)]
pub(crate) struct Runtime {
    inner: Mutex<Inner>,
    collecting: AtomicBool,
    wake: tokio::sync::Notify,
}

/// What the GS observed this tick.
#[derive(Clone, Copy)]
struct TickInput<'a> {
    now: Instant,
    flight_state: FlightState,
    abort_latched: bool,
    admission_open: bool,
    status: Option<(&'a Status, u64)>,
}

#[derive(Debug, Default, PartialEq)]
struct TickPlan {
    send: Vec<Command>,
    warnings: Vec<String>,
}

impl Runtime {
    fn refresh_collecting(&self, inner: &Inner) {
        self.collecting.store(
            inner.validation.is_some() || inner.collector.is_some(),
            Ordering::Release,
        );
    }

    /// Any abort the GS latches turns DAC output off at the next tick.
    pub(crate) fn note_abort(&self) {
        self.inner.lock().unwrap().abort_pending = true;
        self.wake.notify_one();
    }

    /// One keepalive step: decide what to send. Pure apart from `inner`.
    fn plan_tick(&self, input: TickInput<'_>) -> TickPlan {
        let mut inner = self.inner.lock().unwrap();
        let mut plan = TickPlan::default();
        let now = input.now;
        let fresh = input.status.filter(|(_, age)| *age <= STATUS_FRESH_MS);
        let abort = inner.abort_pending || (input.abort_latched && !inner.abort_seen);
        inner.abort_pending = false;
        inner.abort_seen = input.abort_latched;
        let any_desired = inner.channels.iter().any(|c| c.desired.is_some());
        if any_desired {
            let reason = if abort {
                Some("An abort was observed".to_string())
            } else if !flight_state_allows(input.flight_state) {
                Some(format!(
                    "Flight state is {:?}; DAC output is allowed only in Startup or Idle",
                    input.flight_state
                ))
            } else if !input.admission_open {
                Some("GroundStation operator commands are closed".to_string())
            } else {
                match input.status {
                    None => Some("DAQ DAC status was lost".to_string()),
                    Some((_, age)) if age > STATUS_FRESH_MS => Some(format!(
                        "DAQ DAC status is stale ({:.1} s old)",
                        age as f64 / 1000.0
                    )),
                    Some((status, _)) if status.gate != 0 => Some(format!(
                        "The DAQ refuses DAC output (DAQ flight state {})",
                        daq_flight_state_name(status.flight_state)
                    )),
                    _ => None,
                }
            };
            if let Some(reason) = reason {
                let message = format!("{reason}; DAC output turned off.");
                let channels: Vec<usize> = (0..2)
                    .filter(|c| inner.channels[*c].desired.is_some())
                    .collect();
                inner.start_disable(&channels, now);
                inner.last_drop = Some(message.clone());
                plan.warnings.push(message);
            }
        }
        // Turn off anything the DAQ drives that the GS does not want (including
        // whatever a previous backend process left on).
        if let Some((status, _)) = fresh {
            let orphans: Vec<usize> = (0..2)
                .filter(|&channel| {
                    let ctl = &inner.channels[channel];
                    status.channels[channel].active
                        && ctl.desired.is_none()
                        && ctl.disabling.is_none()
                        && ctl
                            .desired_at
                            .is_none_or(|at| now.saturating_duration_since(at) > ORPHAN_GRACE)
                })
                .collect();
            inner.start_disable(&orphans, now);
        }
        // Keepalive: re-send the desired command with the same sequence.
        for channel in 0..2 {
            let ctl = &mut inner.channels[channel];
            if let Some(desired) = &mut ctl.desired {
                ctl.desired_at = Some(now);
                if desired
                    .last_sent
                    .is_none_or(|at| now.saturating_duration_since(at) >= KEEPALIVE)
                {
                    desired.last_sent = Some(now);
                    plan.send.push(desired.command);
                }
            }
        }
        // Disable: repeat until status taken after the request shows it off.
        let received_at = input
            .status
            .and_then(|(_, age)| now.checked_sub(Duration::from_millis(age)));
        let mut disables: Vec<(usize, u32)> = Vec::new();
        for channel in 0..2 {
            let ctl = &mut inner.channels[channel];
            let Some(disabling) = &mut ctl.disabling else {
                continue;
            };
            let confirmed = input
                .status
                .is_some_and(|(s, _)| !s.channels[channel].active)
                && received_at.is_some_and(|at| at > disabling.started);
            if confirmed || now.saturating_duration_since(disabling.started) > DISABLE_FOR {
                if !confirmed && fresh.is_some_and(|(s, _)| s.channels[channel].active) {
                    plan.warnings.push(format!(
                        "DAQ DAC channel {channel} still reports output active after {} s of disable commands.",
                        DISABLE_FOR.as_secs()
                    ));
                }
                ctl.disabling = None;
                continue;
            }
            if disabling
                .last_sent
                .is_none_or(|at| now.saturating_duration_since(at) >= DISABLE_REPEAT)
            {
                disabling.last_sent = Some(now);
                disables.push((channel, disabling.sequence));
            }
        }
        match disables.as_slice() {
            [(0, a), (1, b)] if a == b => {
                plan.send.push(Command::disable(wire::CHANNEL_ALL, *a));
            }
            _ => {
                for (channel, sequence) in disables {
                    plan.send.push(Command::disable(channel as u8, sequence));
                }
            }
        }
        plan
    }

    /// Record a desired configuration; the caller sends it right away.
    fn set_desired(&self, request: ConfigRequest, command: Command, now: Instant) {
        let mut inner = self.inner.lock().unwrap();
        let ctl = &mut inner.channels[usize::from(command.channel)];
        ctl.disabling = None;
        ctl.desired_at = Some(now);
        ctl.desired = Some(Desired {
            request,
            command,
            last_sent: Some(now),
        });
    }

    /// Drop the desired state of the channels and start sending disable.
    /// Returns the disable command to send right away.
    fn request_disable(&self, channel: Option<u8>, now: Instant) -> Command {
        let mut inner = self.inner.lock().unwrap();
        let sequence = inner.next_sequence();
        let channels: Vec<usize> = match channel {
            Some(c) => vec![usize::from(c)],
            None => vec![0, 1],
        };
        for c in channels {
            let ctl = &mut inner.channels[c];
            if ctl.desired.take().is_some() {
                ctl.desired_at = Some(now);
            }
            ctl.disabling = Some(Disabling {
                sequence,
                started: now,
                last_sent: Some(now),
            });
        }
        Command::disable(channel.unwrap_or(wire::CHANNEL_ALL), sequence)
    }

    fn desired_request(&self, channel: usize) -> Option<ConfigRequest> {
        self.inner.lock().unwrap().channels[channel]
            .desired
            .as_ref()
            .map(|d| d.request.clone())
    }

    fn desired_sequence(&self, channel: usize) -> Option<u32> {
        self.inner.lock().unwrap().channels[channel]
            .desired
            .as_ref()
            .map(|d| d.command.sequence)
    }

    fn last_drop(&self) -> Option<String> {
        self.inner.lock().unwrap().last_drop.clone()
    }

    fn next_sequence(&self) -> u32 {
        self.inner.lock().unwrap().next_sequence()
    }

    /// A DAQ_DAC_STATUS arrived: track the confirmed configuration per channel
    /// and feed internal-readback samples.
    fn on_status(&self, status: &Status, now: Instant, t_ms: i64) {
        let mut inner = self.inner.lock().unwrap();
        for (c, ch) in status.channels.iter().enumerate() {
            let ctl = &mut inner.channels[c];
            if !ch.active {
                ctl.active = None;
                continue;
            }
            let wave = wave_of(ch);
            if ctl
                .active
                .is_none_or(|a| a.sequence != ch.sequence || a.wave != wave)
            {
                ctl.active = Some(ActiveConfig {
                    sequence: ch.sequence,
                    wave,
                    since: now,
                });
            }
        }
        let channels = inner.channels.iter().map(|c| c.active).collect::<Vec<_>>();
        if let Some(v) = &mut inner.validation {
            let active = channels[usize::from(v.channel)];
            match active {
                Some(cfg) => {
                    if v.sequence != Some(cfg.sequence) || v.wave != Some(cfg.wave) {
                        v.samples.clear();
                        v.sequence = Some(cfg.sequence);
                        v.wave = Some(cfg.wave);
                    }
                    let readback = status.channels[usize::from(v.channel)].readback_v;
                    if v.target.is_internal()
                        && readback.is_finite()
                        && now.saturating_duration_since(cfg.since) >= SETTLE
                    {
                        push_series(&mut v.samples, t_ms, f64::from(readback));
                    }
                }
                None => v.sequence = None,
            }
        }
        if let Some(col) = &mut inner.collector {
            let readback = status.channels[usize::from(col.channel)].readback_v;
            if readback.is_finite() {
                col.readbacks.push(f64::from(readback));
                if col.target.is_internal() {
                    col.values.push(f64::from(readback));
                    col.times.push(t_ms);
                }
            }
        }
    }

    /// A DAQ data packet arrived while validation or a sweep is collecting.
    fn on_values(&self, data_type: &str, values: &[Option<f32>], t_ms: i64, now: Instant) {
        let mut inner = self.inner.lock().unwrap();
        let channels = inner.channels.iter().map(|c| c.active).collect::<Vec<_>>();
        if let Some(v) = &mut inner.validation
            && let Some((value, fallback)) = v.target.extract(data_type, values)
            && accept_source(&mut v.last_primary_ms, fallback, t_ms)
            && channels[usize::from(v.channel)].is_some_and(|cfg| {
                Some(cfg.sequence) == v.sequence
                    && now.saturating_duration_since(cfg.since) >= SETTLE
            })
        {
            push_series(&mut v.samples, t_ms, value);
        }
        if let Some(col) = &mut inner.collector
            && let Some((value, fallback)) = col.target.extract(data_type, values)
            && accept_source(&mut col.last_primary_ms, fallback, t_ms)
        {
            col.values.push(value);
            col.times.push(t_ms);
        }
    }
}

fn accept_source(last_primary_ms: &mut Option<i64>, fallback: bool, t_ms: i64) -> bool {
    if fallback {
        last_primary_ms.is_none_or(|last| t_ms - last > FALLBACK_AFTER_MS)
    } else {
        *last_primary_ms = Some(t_ms);
        true
    }
}

fn push_series(samples: &mut VecDeque<(i64, f64)>, t_ms: i64, value: f64) {
    samples.push_back((t_ms, value));
    while samples.len() > SERIES_MAX
        || samples
            .front()
            .is_some_and(|(t, _)| t_ms - *t > SERIES_WINDOW_MS)
    {
        samples.pop_front();
    }
}

fn wave_of(ch: &wire::ChannelStatus) -> Wave {
    Wave {
        waveform: Waveform::from_u8(ch.waveform).unwrap_or(Waveform::Dc),
        offset_v: f64::from(ch.offset_v),
        amplitude_v: f64::from(ch.amplitude_v),
        frequency_hz: f64::from(ch.frequency_hz),
        steps: ch.steps,
        dwell_ms: ch.dwell_ms,
        samples: ch.samples,
        flags: ch.flags,
        duty_q16: ch.duty_q16,
    }
}

/// A board sample's time on the GS clock: GS time now less the stamp's
/// network age, when the stamp is network UTC within 5 s of the station's
/// network clock in a translatable epoch (better phase than receive time);
/// otherwise GS receive time. The stamp is never compared with the GS clock.
fn sample_time_ms(stamp: crate::network_stamp::Stamp) -> i64 {
    let wall = crate::telemetry_task::get_current_timestamp_ms() as i64;
    if stamp.ms <= 1_577_836_800_000 {
        return wall;
    }
    match crate::telemetry_task::network_time_router()
        .map(|router| crate::network_stamp::age_on(router, stamp))
    {
        Some(sedsnet::timesync::NetworkStampAge::Age(age)) if age.abs() < 5_000 => wall - age,
        _ => wall,
    }
}

/// Telemetry ingress hook for DAQ_DAC_STATUS (after the network-health tracker
/// accepted it).
pub(crate) fn observe_status(state: &AppState, bytes: &[u8], stamp: crate::network_stamp::Stamp) {
    if let Some(status) = Status::decode(bytes) {
        state
            .daq_dac
            .on_status(&status, Instant::now(), sample_time_ms(stamp));
    }
}

/// Telemetry ingress hook for every float DAQ data packet. Costs one atomic
/// load unless validation or a sweep is collecting.
pub(crate) fn observe_values(
    state: &AppState,
    sender_id: &str,
    data_type: sedsnet::config::DataType,
    stamp: crate::network_stamp::Stamp,
    values: &[Option<f32>],
) {
    if !state.daq_dac.collecting.load(Ordering::Acquire)
        || crate::types::canonical_sender_id(sender_id.trim()) != "DAQ"
    {
        return;
    }
    let name = data_type.as_str();
    state
        .daq_dac
        .on_values(&name, values, sample_time_ms(stamp), Instant::now());
}

pub(crate) fn note_abort(state: &AppState) {
    state.daq_dac.note_abort();
}

fn command_packet(command: &Command) -> sedsnet::TelemetryResult<sedsnet::packet::Packet> {
    if command.sequence == 0 {
        return Err(sedsnet::TelemetryError::BadArg);
    }
    gs_packet("DAQ_DAC_COMMAND", Arc::from(command.encode()))
}

fn gs_packet(name: &str, payload: Arc<[u8]>) -> sedsnet::TelemetryResult<sedsnet::packet::Packet> {
    // Stamped on the station's network clock in its time-base epoch when it
    // has one (the DAQ orders the lease by sequence, not by this stamp).
    let (stamp_ms, epoch) = crate::telemetry_task::network_time_router()
        .and_then(|router| router.network_time_ms_with_epoch())
        .filter(|(ms, _)| *ms != 0)
        .unwrap_or((crate::telemetry_task::get_current_timestamp_ms(), 0));
    sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type(name),
        &[crate::telemetry_schema::endpoint("SD_CARD")],
        "GS",
        stamp_ms,
        payload,
    )
    .map(|packet| packet.with_time_epoch(epoch))
}

fn send(router: &sedsnet::router::Router, command: &Command) -> Result<(), String> {
    command_packet(command)
        .and_then(|packet| router.tx_queue(packet))
        .map_err(|error| error.to_string())
}

fn send_table(router: &sedsnet::router::Router, msg: &wire::TableMsg) -> Result<(), String> {
    gs_packet("DAQ_DAC_TABLE", Arc::from(msg.encode()))
        .and_then(|packet| router.tx_queue(packet))
        .map_err(|error| error.to_string())
}

fn daq_status(state: &AppState, now: Instant) -> Option<(Status, u64)> {
    state.network_health.lock().unwrap().daq_dac_status(now)
}

fn tick(state: &AppState, router: &sedsnet::router::Router, now: Instant) {
    let status = daq_status(state, now);
    let plan = state.daq_dac.plan_tick(TickInput {
        now,
        flight_state: state.local_flight_state_snapshot(),
        abort_latched: state.abort_indicator_latched(),
        admission_open: crate::gse::capture_operator_origin(state).is_some(),
        status: status.as_ref().map(|(s, age)| (s, *age)),
    });
    for warning in plan.warnings {
        log::warn!("{warning}");
        crate::web::emit_notification_warning(state, warning);
    }
    for command in plan.send {
        if let Err(error) = send(router, &command) {
            log::warn!("DAQ DAC command could not be queued: {error}");
        }
    }
}

/// Lease keepalive and safety stop. Runs on the primary GroundStation only.
pub async fn service(state: Arc<AppState>, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            _ = state.daq_dac.wake.notified() => {}
            _ = shutdown.recv() => {
                if let Some(router) = state.topology_router.get().filter(|_| !crate::rrls::is_rrls()) {
                    let command = state.daq_dac.request_disable(None, Instant::now());
                    let _ = send(router, &command);
                }
                return;
            }
        }
        if crate::rrls::is_rrls() {
            continue;
        }
        let Some(router) = state.topology_router.get() else {
            continue;
        };
        tick(&state, router, Instant::now());
    }
}

#[cfg(test)]
mod tests;
