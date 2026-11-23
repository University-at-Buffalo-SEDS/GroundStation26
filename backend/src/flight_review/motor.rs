//! Motor review: the burn report's values with the burn window's load-cell,
//! chamber and tank series, live during a burn and from any recording.
use super::data::Data;
use crate::burn_report::merge::{Channel, Sample, Source};
use crate::burn_report::{self, BurnReport, Inputs, Meta};
use serde::{Deserialize, Serialize};

/// A predicted thrust curve to draw over the measured one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Predicted {
    /// `apogee_planner` (the planner's motor for the applied plan) or
    /// `hybrid_model` (the physics hybrid motor model, when present).
    pub model: String,
    pub label: String,
    pub total_impulse_ns: Option<f64>,
    pub impulse_sigma_ns: Option<f64>,
    pub burn_time_s: Option<f64>,
    pub peak_thrust_n: Option<f64>,
    /// (t s from ignition, thrust N); empty when only the totals are known.
    pub curve: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Series {
    /// (s from the ignition reference, value) per channel, burn window only.
    pub kg1000_kg: Vec<[f64; 2]>,
    pub kg50_kg: Vec<[f64; 2]>,
    pub chamber_psi: Vec<[f64; 2]>,
    pub tank_psi: Vec<[f64; 2]>,
    /// Where each channel's samples came from: `daq_sd`, `gs_telemetry` or `mixed`.
    pub sources: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotorView {
    pub report: Option<BurnReport>,
    /// `saved` (the capture's burn_report.json), `computed` (from this data
    /// and range) or `live` (from the station's live cache).
    pub report_origin: String,
    /// Why there is no report yet (no burn found, no load-cell data).
    pub reason: Option<String>,
    pub simulated: bool,
    pub ignition_ms: Option<i64>,
    /// The burn window (network ms): the default motor export range.
    pub window_from_ms: Option<i64>,
    pub window_to_ms: Option<i64>,
    pub series: Series,
    pub predicted: Option<Predicted>,
    /// The fire archive record, for a fire archive source.
    pub archive: Option<serde_json::Value>,
}

/// Window kept around the ignition reference for the charts (s).
pub(crate) const PRE_S: f64 = 5.0;
pub(crate) const POST_S: f64 = 12.0;
const MAX_POINTS: usize = 2000;

/// The burn report inputs from review rows: station telemetry, and the DAQ
/// SD rows a merged source holds (sender `DAQ_SD`).
pub(crate) fn inputs(data: &Data, extra_daq: Vec<(Channel, Sample)>) -> Inputs {
    let mut inputs = Inputs::default();
    let mut daq = extra_daq;
    for r in &data.rows {
        if r.sender == crate::recording_replay::sources::sd_sender("DAQ") {
            if let Some(channel) = Channel::ALL.into_iter().find(|c| c.gs_type().0 == r.data_type)
                && let Some(value) = r.v(0)
            {
                daq.push((
                    channel,
                    Sample {
                        t_ms: r.t_ms,
                        value,
                        source: Source::DaqSd,
                    },
                ));
            }
            continue;
        }
        inputs.push_gs(r.t_ms, &r.data_type, &r.sender, &r.values);
    }
    if !daq.is_empty() {
        inputs.merge_daq(daq);
    }
    inputs
}

fn decimate(points: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    let step = points.len().div_ceil(MAX_POINTS).max(1);
    let n = points.len();
    points
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % step == 0 || *i + 1 == n)
        .map(|(_, p)| p)
        .collect()
}

pub(crate) fn channel_key(channel: Channel) -> &'static str {
    match channel {
        Channel::Kg1000 => "kg1000",
        Channel::Kg50 => "kg50",
        Channel::Chamber => "chamber",
        Channel::Tank => "tank",
    }
}

fn series(inputs: &Inputs, t0: i64, burn_end_s: f64) -> Series {
    let mut out = Series::default();
    let (lo, hi) = (t0 - (PRE_S * 1000.0) as i64, t0 + ((burn_end_s.max(0.0) + POST_S) * 1000.0) as i64);
    for channel in Channel::ALL {
        let Some(samples) = inputs.channels.get(&channel) else { continue };
        let mut w: Vec<&Sample> = samples.iter().filter(|s| s.t_ms >= lo && s.t_ms <= hi).collect();
        w.sort_by_key(|s| s.t_ms);
        if w.is_empty() {
            continue;
        }
        let daq = w.iter().filter(|s| s.source == Source::DaqSd).count();
        let source = if daq == w.len() {
            "daq_sd"
        } else if daq == 0 {
            "gs_telemetry"
        } else {
            "mixed"
        };
        out.sources.insert(channel_key(channel).into(), source.into());
        let points = decimate(
            w.iter()
                .map(|s| [((s.t_ms - t0) as f64).round() / 1000.0, (s.value * 1000.0).round() / 1000.0])
                .collect(),
        );
        match channel {
            Channel::Kg1000 => out.kg1000_kg = points,
            Channel::Kg50 => out.kg50_kg = points,
            Channel::Chamber => out.chamber_psi = points,
            Channel::Tank => out.tank_psi = points,
        }
    }
    out
}

/// The physics hybrid motor model's predicted curve for this burn. The
/// model is being added separately (`.workspace-progress/hybrid-motor-*`);
/// until it lands this returns nothing and the planner's prediction is shown.
/// When it does, call it here with the report's fill, tank pressure and
/// temperature; its curve is preferred over the planner's.
pub(crate) fn hybrid_model_prediction(_report: &BurnReport) -> Option<Predicted> {
    None
}

/// The apogee planner's motor for the plan applied before this burn (the
/// capture's `apogee_plan`): the reference curve stretched in time to the
/// planned impulse (the planner also scales the thrust axis with the mean
/// tank pressure; at the reference pressure this is the same curve).
pub(crate) fn planner_prediction(plan: &serde_json::Value) -> Option<Predicted> {
    let impulse = plan["impulse_ns"].as_f64().filter(|v| *v > 0.0)?;
    let (t, f) = crate::apogee_planner::flight::parse_curve(crate::apogee_planner::flight::THRUST_CSV);
    let base: f64 = t
        .windows(2)
        .zip(f.windows(2))
        .map(|(t, f)| (t[1] - t[0]) * (f[0] + f[1]) / 2.0)
        .sum();
    let b = if base > 0.0 { impulse / base } else { 1.0 };
    let curve: Vec<[f64; 2]> = t
        .iter()
        .zip(&f)
        .map(|(t, f)| [(t * b * 1000.0).round() / 1000.0, (f * 10.0).round() / 10.0])
        .collect();
    let peak = f.iter().copied().fold(0.0, f64::max);
    let burn = curve
        .iter()
        .filter(|p| p[1] >= 0.05 * peak)
        .map(|p| p[0])
        .fold(None, |acc: Option<(f64, f64)>, t| {
            Some(acc.map_or((t, t), |(a, b)| (a.min(t), b.max(t))))
        });
    Some(Predicted {
        model: "apogee_planner".into(),
        label: format!(
            "Apogee planner motor for plan {} (fill {} kg)",
            plan["plan_id"].as_str().unwrap_or("?"),
            plan["fill_mass_kg"].as_f64().map_or("?".into(), |m| format!("{m:.2}"))
        ),
        total_impulse_ns: Some(impulse),
        impulse_sigma_ns: plan["impulse_sigma_ns"].as_f64(),
        burn_time_s: burn.map(|(a, b)| ((b - a) * 100.0).round() / 100.0),
        peak_thrust_n: Some(peak),
        curve,
    })
}

pub(crate) fn prediction(report: Option<&BurnReport>, plan: Option<&serde_json::Value>) -> Option<Predicted> {
    report
        .and_then(hybrid_model_prediction)
        .or_else(|| plan.and_then(planner_prediction))
}

pub(crate) struct Context<'a> {
    pub id: String,
    pub label: String,
    pub mode: String,
    pub simulated: bool,
    pub created_ms: i64,
    /// A report already saved for this data (used unless a range is chosen).
    pub saved: Option<BurnReport>,
    pub plan: Option<&'a serde_json::Value>,
    pub extra_daq: Vec<(Channel, Sample)>,
    pub archive: Option<serde_json::Value>,
}

/// Builds the motor view; `None` when the data has no motor channels at all.
pub(crate) fn view(data: &Data, ctx: Context<'_>) -> Option<MotorView> {
    let inputs = inputs(data, ctx.extra_daq);
    let fire_curve: Vec<[f64; 2]> = data
        .of_type(crate::recording_replay::sources::THRUST_TYPE)
        .filter_map(|r| Some([r.t_ms as f64, r.v(0)?]))
        .collect();
    if inputs.channels.is_empty() && fire_curve.is_empty() && ctx.saved.is_none() {
        return None;
    }
    let mut out = MotorView {
        simulated: ctx.simulated,
        archive: ctx.archive,
        ..MotorView::default()
    };
    let computed = burn_report::compute(
        &inputs,
        Meta {
            id: ctx.id.clone(),
            capture_id: None,
            capture_label: ctx.label.clone(),
            mode: ctx.mode.clone(),
            simulated: ctx.simulated,
            created_ms: ctx.created_ms,
        },
    );
    let report = match (ctx.saved, computed) {
        (Some(saved), _) => {
            out.report_origin = "saved".into();
            Some(saved)
        }
        (None, Ok(report)) => {
            out.report_origin = "computed".into();
            Some(report)
        }
        (None, Err(reason)) => {
            out.reason = Some(reason);
            None
        }
    };
    if let Some(r) = &report {
        out.ignition_ms = Some(r.ignition_ms);
        let end = if r.burn_end_s > 0.0 { r.burn_end_s } else { r.burn_time_s + 0.5 };
        out.window_from_ms = Some(r.ignition_ms - (PRE_S * 1000.0) as i64);
        out.window_to_ms = Some(r.ignition_ms + ((end + POST_S) * 1000.0) as i64);
        out.series = series(&inputs, r.ignition_ms, end);
        out.simulated |= r.simulated;
    } else if let (Some(first), Some(last)) = (fire_curve.first(), fire_curve.last()) {
        // A fire archive entry without its capture: its saved curve.
        out.window_from_ms = Some(first[0] as i64);
        out.window_to_ms = Some(last[0] as i64);
    } else if let Some(latest) = inputs.channels.values().flatten().map(|s| s.t_ms).max() {
        // No report yet (the burn is still going, or no burn): the raw
        // channels around the last main valve opening, else the last 20 s.
        let t0 = inputs.valve_open_ms.iter().copied().filter(|t| *t <= latest).max();
        let zero = t0.unwrap_or(latest - 15_000);
        out.ignition_ms = t0;
        out.window_from_ms = Some(zero - (PRE_S * 1000.0) as i64);
        out.window_to_ms = Some(latest);
        out.series = series(&inputs, zero, ((latest - zero) as f64 / 1000.0 - POST_S).max(0.0));
    }
    out.predicted = prediction(report.as_ref(), ctx.plan);
    out.report = report;
    Some(out)
}
