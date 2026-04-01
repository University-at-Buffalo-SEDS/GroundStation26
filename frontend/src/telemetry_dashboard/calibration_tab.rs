#![allow(clippy::redundant_locals)]

use super::{
    TELEMETRY_RENDER_EPOCH, http_get_json, http_post_json, latest_telemetry_row,
    latest_telemetry_value, translate_text,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Ch0,
    Ch1,
    Iadc,
}

impl Channel {
    fn from_layout(s: &str) -> Self {
        match s {
            "ch0" | "CH0" | "KG50" | "50kg" => Self::Ch0,
            "iadc" | "IADC" | "tank_pressure" | "Tank Pressure" => Self::Iadc,
            _ => Self::Ch1,
        }
    }
    fn api_name(&self) -> &'static str {
        match self {
            Self::Ch0 => "ch0",
            Self::Ch1 => "ch1",
            Self::Iadc => "iadc",
        }
    }
    fn title(&self) -> &'static str {
        match self {
            Self::Ch0 => "50kg",
            Self::Ch1 => "1000kg",
            Self::Iadc => "Tank Pressure",
        }
    }

    fn fit_color(&self) -> &'static str {
        match self {
            Self::Ch0 => "#f59e0b",
            Self::Ch1 => "#22d3ee",
            Self::Iadc => "#a78bfa",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct ChannelLinear {
    m: Option<f32>,
    b: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct FitMeta {
    #[serde(rename = "type")]
    fit_type: Option<String>,
    a: Option<f32>,
    b: Option<f32>,
    c: Option<f32>,
    d: Option<f32>,
    x0: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PointCh0 {
    kg: f32,
    ch0_raw: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PointCh1 {
    kg: f32,
    ch1_raw: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PointIadc {
    expected: f32,
    iadc_raw: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CalibrationFile {
    full_mass_kg: Option<f32>,
    #[serde(default)]
    ch0: ChannelLinear,
    #[serde(default)]
    ch1: ChannelLinear,
    #[serde(default)]
    iadc: ChannelLinear,
    ch0_zero_raw: Option<f32>,
    ch1_zero_raw: Option<f32>,
    iadc_zero_raw: Option<f32>,
    #[serde(default)]
    points: Vec<PointCh0>,
    #[serde(default)]
    points_ch1: Vec<PointCh1>,
    #[serde(default)]
    points_iadc: Vec<PointIadc>,
    ch0_fit: Option<FitMeta>,
    ch1_fit: Option<FitMeta>,
    iadc_fit: Option<FitMeta>,
    #[serde(default)]
    weights_kg: Vec<f32>,
}

impl Default for CalibrationFile {
    fn default() -> Self {
        Self {
            full_mass_kg: Some(10.0),
            ch0: ChannelLinear {
                m: Some(1.0),
                b: Some(0.0),
            },
            ch1: ChannelLinear {
                m: Some(1.0),
                b: Some(0.0),
            },
            iadc: ChannelLinear {
                m: Some(1.0),
                b: Some(0.0),
            },
            ch0_zero_raw: None,
            ch1_zero_raw: None,
            iadc_zero_raw: None,
            points: Vec::new(),
            points_ch1: Vec::new(),
            points_iadc: Vec::new(),
            ch0_fit: None,
            ch1_fit: None,
            iadc_fit: None,
            weights_kg: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct CapturePointReq {
    sensor_id: String,
    raw: f32,
}

#[derive(Serialize)]
struct CaptureSpanReq {
    sensor_id: String,
    raw: f32,
    known_kg: f32,
}

#[derive(Serialize)]
struct RefitReq {
    channel: String,
    mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CalibrationTabLayout {
    #[serde(default = "default_capture_target_samples")]
    capture_target_samples: usize,
    #[serde(default)]
    sensors: Vec<CalibrationSensorSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CalibrationSensorSpec {
    id: String,
    label: String,
    data_type: String,
    channel: String,
    #[serde(default)]
    fit_modes: Vec<String>,
}

fn default_capture_target_samples() -> usize {
    200
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    SequenceZero,
    SequencePoint,
}

fn sleep_ms(ms: u32) -> impl Future<Output = ()> {
    #[cfg(target_arch = "wasm32")]
    {
        gloo_timers::future::TimeoutFuture::new(ms)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::time::sleep(std::time::Duration::from_millis(ms as u64))
    }
}

fn latest_raw(data_type: &str) -> Option<f32> {
    latest_telemetry_value(data_type, None, 0)
}

fn fmt_fixed(v: Option<f32>, width: usize, prec: usize) -> String {
    match v {
        Some(x) => format!("{x:+width$.prec$}", width = width, prec = prec),
        None => "-".to_string(),
    }
}

fn default_sensors() -> Vec<CalibrationSensorSpec> {
    let fit_modes = default_fit_modes();
    vec![
        CalibrationSensorSpec {
            id: "KG50".to_string(),
            label: "50kg".to_string(),
            data_type: "KG50".to_string(),
            channel: "ch0".to_string(),
            fit_modes: fit_modes.clone(),
        },
        CalibrationSensorSpec {
            id: "KG1000".to_string(),
            label: "1000kg".to_string(),
            data_type: "KG1000".to_string(),
            channel: "ch1".to_string(),
            fit_modes: fit_modes.clone(),
        },
        CalibrationSensorSpec {
            id: "IADC".to_string(),
            label: "Tank Pressure".to_string(),
            data_type: "FUEL_TANK_PRESSURE".to_string(),
            channel: "iadc".to_string(),
            fit_modes,
        },
    ]
}

fn sensors_from_layout(layout: &CalibrationTabLayout) -> Vec<CalibrationSensorSpec> {
    if layout.sensors.is_empty() {
        return default_sensors();
    }
    layout.sensors.clone()
}

fn default_fit_modes() -> Vec<String> {
    vec![
        "best".to_string(),
        "linear".to_string(),
        "linear_zero".to_string(),
        "parabolic".to_string(),
        "parabolic_zero".to_string(),
        "cubic".to_string(),
        "cubic_zero".to_string(),
    ]
}

fn channel_points(cfg: &CalibrationFile, channel: Channel) -> Vec<(f32, f32)> {
    match channel {
        Channel::Ch0 => cfg.points.iter().map(|p| (p.ch0_raw, p.kg)).collect(),
        Channel::Ch1 => cfg.points_ch1.iter().map(|p| (p.ch1_raw, p.kg)).collect(),
        Channel::Iadc => cfg
            .points_iadc
            .iter()
            .map(|p| (p.iadc_raw, p.expected))
            .collect(),
    }
}

fn remove_point(cfg: &mut CalibrationFile, channel: Channel, index: usize) -> bool {
    match channel {
        Channel::Ch0 => {
            if index < cfg.points.len() {
                cfg.points.remove(index);
                true
            } else {
                false
            }
        }
        Channel::Ch1 => {
            if index < cfg.points_ch1.len() {
                cfg.points_ch1.remove(index);
                true
            } else {
                false
            }
        }
        Channel::Iadc => {
            if index < cfg.points_iadc.len() {
                cfg.points_iadc.remove(index);
                true
            } else {
                false
            }
        }
    }
}

fn update_point(
    cfg: &mut CalibrationFile,
    channel: Channel,
    index: usize,
    expected: f32,
    raw: f32,
) -> bool {
    let expected = expected.max(0.0);
    match channel {
        Channel::Ch0 => {
            if let Some(p) = cfg.points.get_mut(index) {
                p.kg = expected;
                p.ch0_raw = raw;
                true
            } else {
                false
            }
        }
        Channel::Ch1 => {
            if let Some(p) = cfg.points_ch1.get_mut(index) {
                p.kg = expected;
                p.ch1_raw = raw;
                true
            } else {
                false
            }
        }
        Channel::Iadc => {
            if let Some(p) = cfg.points_iadc.get_mut(index) {
                p.expected = expected;
                p.iadc_raw = raw;
                true
            } else {
                false
            }
        }
    }
}

fn upsert_point(cfg: &mut CalibrationFile, channel: Channel, expected: f32, raw: f32) {
    let expected = expected.max(0.0);
    match channel {
        Channel::Ch0 => {
            if let Some(p) = cfg
                .points
                .iter_mut()
                .find(|p| (p.kg - expected).abs() < 1e-6)
            {
                p.ch0_raw = raw;
            } else {
                cfg.points.push(PointCh0 {
                    kg: expected,
                    ch0_raw: raw,
                });
            }
        }
        Channel::Ch1 => {
            if let Some(p) = cfg
                .points_ch1
                .iter_mut()
                .find(|p| (p.kg - expected).abs() < 1e-6)
            {
                p.ch1_raw = raw;
            } else {
                cfg.points_ch1.push(PointCh1 {
                    kg: expected,
                    ch1_raw: raw,
                });
            }
        }
        Channel::Iadc => {
            if let Some(p) = cfg
                .points_iadc
                .iter_mut()
                .find(|p| (p.expected - expected).abs() < 1e-6)
            {
                p.iadc_raw = raw;
            } else {
                cfg.points_iadc.push(PointIadc {
                    expected,
                    iadc_raw: raw,
                });
            }
        }
    }
}

fn reset_channel(cfg: &mut CalibrationFile, channel: Channel) {
    match channel {
        Channel::Ch0 => {
            cfg.points.clear();
            cfg.ch0_zero_raw = None;
            cfg.ch0_fit = None;
        }
        Channel::Ch1 => {
            cfg.points_ch1.clear();
            cfg.ch1_zero_raw = None;
            cfg.ch1_fit = None;
        }
        Channel::Iadc => {
            cfg.points_iadc.clear();
            cfg.iadc_zero_raw = None;
            cfg.iadc_fit = None;
        }
    }
}

fn fit_for_channel(cfg: &CalibrationFile, channel: Channel) -> Option<&FitMeta> {
    match channel {
        Channel::Ch0 => cfg.ch0_fit.as_ref(),
        Channel::Ch1 => cfg.ch1_fit.as_ref(),
        Channel::Iadc => cfg.iadc_fit.as_ref(),
    }
}

fn linear_for_channel(
    cfg: &CalibrationFile,
    channel: Channel,
) -> (&ChannelLinear, Option<&FitMeta>) {
    match channel {
        Channel::Ch0 => (&cfg.ch0, cfg.ch0_fit.as_ref()),
        Channel::Ch1 => (&cfg.ch1, cfg.ch1_fit.as_ref()),
        Channel::Iadc => (&cfg.iadc, cfg.iadc_fit.as_ref()),
    }
}

fn eval_fit(cfg: &CalibrationFile, channel: Channel, raw: f32) -> Option<f32> {
    let (linear, fit) = linear_for_channel(cfg, channel);
    let fit_type = fit.and_then(|f| f.fit_type.as_deref());
    if let Some(meta) = fit {
        let x = raw - meta.x0.unwrap_or(0.0);
        if fit_type == Some("poly3") {
            let a = meta.a?;
            let b = meta.b?;
            let c = meta.c.unwrap_or(0.0);
            let d = meta.d.unwrap_or(0.0);
            return Some(a * x * x * x + b * x * x + c * x + d);
        }
        if fit_type == Some("poly2") {
            let a = meta.a?;
            let b = meta.b?;
            let c = meta.c.unwrap_or(0.0);
            return Some(a * x * x + b * x + c);
        }
    }
    let m = linear.m?;
    Some(m * raw + linear.b.unwrap_or(0.0))
}

fn fit_details_text(cfg: &CalibrationFile, channel: Channel) -> Option<String> {
    let (linear, fit) = linear_for_channel(cfg, channel);
    let fit_type = fit.and_then(|f| f.fit_type.as_deref()).unwrap_or("linear");

    match fit_type {
        "poly3" => {
            let meta = fit?;
            Some(format!(
                "a={} b={} c={} d={} x0={}",
                fmt_fixed(meta.a, 10, 4),
                fmt_fixed(meta.b, 10, 4),
                fmt_fixed(meta.c, 10, 4),
                fmt_fixed(meta.d, 10, 4),
                fmt_fixed(meta.x0, 10, 4)
            ))
        }
        "poly2" => {
            let meta = fit?;
            Some(format!(
                "a={} b={} c={} x0={}",
                fmt_fixed(meta.a, 10, 4),
                fmt_fixed(meta.b, 10, 4),
                fmt_fixed(meta.c, 10, 4),
                fmt_fixed(meta.x0, 10, 4)
            ))
        }
        _ => {
            if linear.m.is_none() && linear.b.is_none() && fit.and_then(|f| f.x0).is_none() {
                return None;
            }
            Some(format!(
                "m={} b={} x0={}",
                fmt_fixed(linear.m, 10, 4),
                fmt_fixed(linear.b, 10, 4),
                fmt_fixed(fit.and_then(|f| f.x0), 10, 4)
            ))
        }
    }
}

#[component]
pub fn CalibrationTab() -> Element {
    let _ = *TELEMETRY_RENDER_EPOCH.read();
    let layout_cfg = use_signal(|| None::<CalibrationTabLayout>);
    let sensors = layout_cfg
        .read()
        .as_ref()
        .map(sensors_from_layout)
        .unwrap_or_else(default_sensors);
    let capture_target = layout_cfg
        .read()
        .as_ref()
        .map(|v| v.capture_target_samples)
        .unwrap_or_else(default_capture_target_samples)
        .max(10);

    let cfg = use_signal(|| None::<CalibrationFile>);
    let selected_sensor_id = use_signal(|| "KG1000".to_string());
    let fit_mode = use_signal(|| "best".to_string());
    let known_kg = use_signal(|| "1.0".to_string());
    let manual_kg = use_signal(|| "1.0".to_string());
    let manual_raw = use_signal(String::new);
    let selected_point_idx = use_signal(|| None::<usize>);
    let status = use_signal(|| "Loading calibration...".to_string());

    let capture_active = use_signal(|| false);
    let capture_mode = use_signal(|| CaptureMode::SequencePoint);
    let capture_weight = use_signal(|| 0.0f32);
    let capture_vals = use_signal(Vec::<f32>::new);
    let capture_loop_started = use_signal(|| false);

    {
        let mut layout_cfg = layout_cfg;
        let mut status = status;
        use_effect(move || {
            spawn(async move {
                match http_get_json::<CalibrationTabLayout>("/api/calibration_config").await {
                    Ok(v) => layout_cfg.set(Some(v)),
                    Err(e) => status.set(format!(
                        "Failed to load calibration config, using defaults: {e}"
                    )),
                }
            });
        });
    }

    {
        let sensors = sensors.clone();
        let mut selected_sensor_id = selected_sensor_id;
        use_effect(move || {
            let cur = selected_sensor_id.read().clone();
            if sensors.iter().any(|s| s.id == cur) {
                return;
            }
            if let Some(first) = sensors.first() {
                selected_sensor_id.set(first.id.clone());
            }
        });
    }

    {
        let mut cfg = cfg;
        let mut status = status;
        use_effect(move || {
            spawn(async move {
                match http_get_json::<CalibrationFile>("/api/calibration").await {
                    Ok(v) => {
                        cfg.set(Some(v));
                        status.set("Calibration loaded".to_string());
                    }
                    Err(e) => status.set(format!("Failed to load: {e}")),
                }
            });
        });
    }

    {
        let mut capture_loop_started = capture_loop_started;
        let selected_sensor_id = selected_sensor_id;
        let sensors = sensors.clone();
        let capture_target = capture_target;
        let mut capture_active = capture_active;
        let capture_mode = capture_mode;
        let capture_weight = capture_weight;
        let mut capture_vals = capture_vals;
        let fit_mode = fit_mode;
        let mut cfg = cfg;
        let mut status = status;
        use_effect(move || {
            if *capture_loop_started.read() {
                return;
            }
            capture_loop_started.set(true);

            let sensors = sensors.clone();
            spawn(async move {
                loop {
                    sleep_ms(20).await;
                    if !*capture_active.read() {
                        continue;
                    }
                    let selected_id = selected_sensor_id.read().clone();
                    let Some(sensor) = sensors.iter().find(|s| s.id == selected_id) else {
                        continue;
                    };
                    let channel = Channel::from_layout(sensor.channel.as_str());
                    let Some(raw) = latest_raw(sensor.data_type.as_str()) else {
                        continue;
                    };

                    let mut vals = capture_vals.read().clone();
                    vals.push(raw);
                    let count = vals.len();
                    capture_vals.set(vals.clone());

                    if count < capture_target {
                        status.set(format!(
                            "Capturing {}: {count}/{capture_target}",
                            channel.title()
                        ));
                        continue;
                    }

                    let avg = vals.iter().sum::<f32>() / vals.len() as f32;
                    let mode = *capture_mode.read();
                    let weight = *capture_weight.read();
                    capture_active.set(false);
                    capture_vals.set(Vec::new());

                    let sensor_id = channel.api_name().to_string();
                    match mode {
                        CaptureMode::SequenceZero => {
                            let body = CapturePointReq {
                                sensor_id,
                                raw: avg,
                            };
                            match http_post_json::<CapturePointReq, CalibrationFile>(
                                "/api/calibration/capture_zero",
                                &body,
                            )
                            .await
                            {
                                Ok(new_cfg) => {
                                    cfg.set(Some(new_cfg));
                                    status.set(format!(
                                        "Captured zero on {} (avg raw {avg:.6})",
                                        channel.title()
                                    ));
                                }
                                Err(e) => status.set(format!("Zero capture failed: {e}")),
                            }
                        }
                        CaptureMode::SequencePoint => {
                            let body = CaptureSpanReq {
                                sensor_id: sensor_id.clone(),
                                raw: avg,
                                known_kg: weight,
                            };
                            match http_post_json::<CaptureSpanReq, CalibrationFile>(
                                "/api/calibration/capture_span",
                                &body,
                            )
                            .await
                            {
                                Ok(_) => {
                                    let refit = RefitReq {
                                        channel: sensor_id,
                                        mode: fit_mode.read().clone(),
                                    };
                                    match http_post_json::<RefitReq, CalibrationFile>(
                                        "/api/calibration/refit",
                                        &refit,
                                    )
                                    .await
                                    {
                                        Ok(new_cfg) => {
                                            cfg.set(Some(new_cfg));
                                            status.set(format!(
                                                "Captured point {} kg on {} (avg raw {avg:.6})",
                                                weight,
                                                channel.title()
                                            ));
                                        }
                                        Err(e) => status.set(format!("Refit failed: {e}")),
                                    }
                                }
                                Err(e) => status.set(format!("Point capture failed: {e}")),
                            }
                        }
                    }
                }
            });
        });
    }

    let selected_id = selected_sensor_id.read().clone();
    let selected_sensor = sensors
        .iter()
        .find(|s| s.id == selected_id)
        .cloned()
        .or_else(|| sensors.first().cloned());
    let channel = selected_sensor
        .as_ref()
        .map(|s| Channel::from_layout(s.channel.as_str()))
        .unwrap_or(Channel::Ch1);
    let fit_modes = selected_sensor
        .as_ref()
        .map(|s| {
            if s.fit_modes.is_empty() {
                default_fit_modes()
            } else {
                s.fit_modes.clone()
            }
        })
        .unwrap_or_else(default_fit_modes);
    {
        let fit_modes = fit_modes.clone();
        let mut fit_mode = fit_mode;
        use_effect(move || {
            let current = fit_mode.read().clone();
            if fit_modes.iter().any(|m| m == &current) {
                return;
            }
            if let Some(first) = fit_modes.first() {
                fit_mode.set(first.clone());
            }
        });
    }
    let points = cfg
        .read()
        .as_ref()
        .map(|c| channel_points(c, channel))
        .unwrap_or_default();
    let raw_live = selected_sensor
        .as_ref()
        .and_then(|s| latest_raw(s.data_type.as_str()));
    let last_ts_ms = selected_sensor
        .as_ref()
        .and_then(|s| latest_telemetry_row(&s.data_type, None).map(|r| r.timestamp_ms));
    let sequence_started = cfg.read().as_ref().is_some_and(|c| match channel {
        Channel::Ch0 => c.ch0_zero_raw.is_some(),
        Channel::Ch1 => c.ch1_zero_raw.is_some(),
        Channel::Iadc => c.iadc_zero_raw.is_some(),
    });
    let calibrated_live = cfg
        .read()
        .as_ref()
        .and_then(|c| raw_live.and_then(|raw| eval_fit(c, channel, raw)));
    let raw_live_s = fmt_fixed(raw_live, 12, 6);
    let calibrated_live_s = fmt_fixed(calibrated_live, 12, 4);
    let ts_live_s = last_ts_ms
        .map(|v| format!("{v:>13}"))
        .unwrap_or_else(|| "-".to_string());
    let fit_type_s = cfg
        .read()
        .as_ref()
        .and_then(|c| fit_for_channel(c, channel))
        .and_then(|f| f.fit_type.clone())
        .unwrap_or_else(|| "linear".to_string());
    let fit_meta_text = cfg
        .read()
        .as_ref()
        .and_then(|c| fit_details_text(c, channel));
    let fit_equation_text = fit_meta_text
        .clone()
        .unwrap_or_else(|| format!("{}={}", translate_text("type"), translate_text(&fit_type_s)));
    let fit_color = channel.fit_color();

    let plot_w = 900.0_f32;
    let plot_h = 260.0_f32;
    let pad_l = 56.0_f32;
    let pad_r = 14.0_f32;
    let pad_t = 14.0_f32;
    let pad_b = 28.0_f32;
    let mut x_min = points
        .iter()
        .map(|(x, _)| *x)
        .min_by(f32::total_cmp)
        .unwrap_or(0.0);
    let mut x_max = points
        .iter()
        .map(|(x, _)| *x)
        .max_by(f32::total_cmp)
        .unwrap_or(1.0);
    let mut y_min = points
        .iter()
        .map(|(_, y)| *y)
        .min_by(f32::total_cmp)
        .unwrap_or(0.0);
    let mut y_max = points
        .iter()
        .map(|(_, y)| *y)
        .max_by(f32::total_cmp)
        .unwrap_or(1.0);
    if (x_max - x_min).abs() < 1e-6 {
        x_min -= 1.0;
        x_max += 1.0;
    }
    if (y_max - y_min).abs() < 1e-6 {
        y_min -= 1.0;
        y_max += 1.0;
    }
    let x_pad = (x_max - x_min) * 0.1;
    let y_pad = (y_max - y_min) * 0.15;
    x_min -= x_pad;
    x_max += x_pad;
    y_min -= y_pad;
    y_max += y_pad;
    let sx =
        |x: f32| pad_l + ((x - x_min) / (x_max - x_min)).clamp(0.0, 1.0) * (plot_w - pad_l - pad_r);
    let sy = |y: f32| {
        pad_t + (1.0 - ((y - y_min) / (y_max - y_min)).clamp(0.0, 1.0)) * (plot_h - pad_t - pad_b)
    };
    let scatter_xy: Vec<(f32, f32)> = points.iter().map(|(x, y)| (sx(*x), sy(*y))).collect();
    let fit_path = cfg
        .read()
        .as_ref()
        .map(|c| {
            let samples = 80;
            let mut d = String::new();
            for i in 0..samples {
                let t = i as f32 / (samples - 1) as f32;
                let x = x_min + t * (x_max - x_min);
                if let Some(y) = eval_fit(c, channel, x) {
                    let cmd = if d.is_empty() { "M" } else { "L" };
                    d.push_str(&format!("{cmd}{:.2},{:.2} ", sx(x), sy(y)));
                }
            }
            d
        })
        .unwrap_or_default();

    rsx! {
        div { style: "padding:16px; display:flex; flex-direction:column; gap:10px; min-height:100%; overflow:visible;",
            h2 { style: "margin:0; color:#14b8a6;", "Calibration Sequence" }

            div { style: "display:flex; gap:8px; flex-wrap:wrap; align-items:center;",
                span { "Sensors" }
                for sensor in sensors.iter().cloned() {
                    button {
                        style: if sensor.id == selected_id {
                            "padding:6px 10px; border-radius:999px; border:1px solid #22d3ee; background:#0c1f2b; color:#a5f3fc; cursor:pointer;"
                        } else {
                            "padding:6px 10px; border-radius:999px; border:1px solid #334155; background:#111827; color:#cbd5e1; cursor:pointer;"
                        },
                        onclick: {
                            let mut selected_sensor_id = selected_sensor_id;
                            let mut selected_point_idx = selected_point_idx;
                            let sensor_id = sensor.id.clone();
                            move |_| {
                                selected_sensor_id.set(sensor_id.clone());
                                selected_point_idx.set(None);
                            }
                        },
                        "{sensor.label}"
                    }
                }
            }

            div { style: "display:grid; gap:8px; grid-template-columns:repeat(auto-fit,minmax(190px,1fr));",
                {metric_card("Last Timestamp (ms)", ts_live_s.clone())}
                {metric_card("Live Raw", raw_live_s.clone())}
                {metric_card("Calibrated Value", calibrated_live_s.clone())}
                {metric_card("Active Fit", fit_type_s.clone())}
            }

            div { style: "display:flex; gap:8px; flex-wrap:wrap; align-items:center;",
                span { "Regression" }
                select {
                    value: "{fit_mode.read()}",
                    onchange: {
                        let mut fit_mode = fit_mode;
                        move |e| fit_mode.set(e.value())
                    },
                    for mode in fit_modes.iter() {
                        option { value: "{mode}", "{mode}" }
                    }
                }
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; cursor:pointer;",
                    disabled: cfg.read().is_none(),
                    onclick: {
                        let mut cfg = cfg;
                        let selected_sensor_id = selected_sensor_id;
                        let sensors = sensors.clone();
                        let fit_mode = fit_mode;
                        let mut status = status;
                        move |_| {
                            let selected_id = selected_sensor_id.read().clone();
                            let Some(sensor) = sensors.iter().find(|s| s.id == selected_id) else {
                                status.set("Invalid selected sensor".to_string());
                                return;
                            };
                            let body = RefitReq {
                                channel: Channel::from_layout(sensor.channel.as_str()).api_name().to_string(),
                                mode: fit_mode.read().clone(),
                            };
                            spawn(async move {
                                match http_post_json::<RefitReq, CalibrationFile>("/api/calibration/refit", &body).await {
                                    Ok(new_cfg) => {
                                        cfg.set(Some(new_cfg));
                                        status.set("Refit complete".to_string());
                                    }
                                    Err(e) => status.set(format!("Refit failed: {e}")),
                                }
                            });
                        }
                    },
                    "Refit"
                }
            }

            div { style: "display:flex; gap:8px; flex-wrap:wrap; align-items:center;",
                input {
                    r#type: "number",
                    step: "0.01",
                    value: "{manual_kg.read()}",
                    oninput: {
                        let mut manual_kg = manual_kg;
                        move |e| manual_kg.set(e.value())
                    }
                }
                input {
                    r#type: "number",
                    step: "0.000001",
                    placeholder: "raw value",
                    value: "{manual_raw.read()}",
                    oninput: {
                        let mut manual_raw = manual_raw;
                        move |e| manual_raw.set(e.value())
                    }
                }
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #22c55e; background:#052e16; color:#bbf7d0; cursor:pointer;",
                    disabled: cfg.read().is_none(),
                    onclick: {
                        let mut cfg = cfg;
                        let selected_sensor_id = selected_sensor_id;
                        let sensors = sensors.clone();
                        let manual_kg = manual_kg;
                        let manual_raw = manual_raw;
                        let fit_mode = fit_mode;
                        let mut status = status;
                        move |_| {
                            let Ok(kg) = manual_kg.read().parse::<f32>() else {
                                status.set("Invalid manual kg".to_string());
                                return;
                            };
                            let Ok(raw) = manual_raw.read().parse::<f32>() else {
                                status.set("Invalid manual raw".to_string());
                                return;
                            };
                            let selected_id = selected_sensor_id.read().clone();
                            let Some(sensor) = sensors.iter().find(|s| s.id == selected_id) else {
                                status.set("Invalid selected sensor".to_string());
                                return;
                            };
                            let channel = Channel::from_layout(sensor.channel.as_str());
                            let mut next = cfg.read().clone().unwrap_or_default();
                            upsert_point(&mut next, channel, kg, raw);
                            spawn(async move {
                                match http_post_json::<CalibrationFile, CalibrationFile>("/api/calibration", &next).await {
                                    Ok(_) => {
                                        let body = RefitReq {
                                            channel: channel.api_name().to_string(),
                                            mode: fit_mode.read().clone(),
                                        };
                                        match http_post_json::<RefitReq, CalibrationFile>("/api/calibration/refit", &body).await {
                                            Ok(new_cfg) => {
                                                cfg.set(Some(new_cfg));
                                                status.set("Manual point added".to_string());
                                            }
                                            Err(e) => status.set(format!("Refit failed: {e}")),
                                        }
                                    }
                                    Err(e) => status.set(format!("Save failed: {e}")),
                                }
                            });
                        }
                    },
                    "Add/Update Point"
                }
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #a78bfa; background:#22153c; color:#ddd6fe; cursor:pointer;",
                    disabled: cfg.read().is_none() || selected_point_idx.read().is_none(),
                    onclick: {
                        let mut cfg = cfg;
                        let selected_sensor_id = selected_sensor_id;
                        let sensors = sensors.clone();
                        let manual_kg = manual_kg;
                        let manual_raw = manual_raw;
                        let fit_mode = fit_mode;
                        let mut status = status;
                        move |_| {
                            let Some(idx) = *selected_point_idx.read() else {
                                status.set("Select a point first".to_string());
                                return;
                            };
                            let Ok(kg) = manual_kg.read().parse::<f32>() else {
                                status.set("Invalid manual kg".to_string());
                                return;
                            };
                            let Ok(raw) = manual_raw.read().parse::<f32>() else {
                                status.set("Invalid manual raw".to_string());
                                return;
                            };
                            let selected_id = selected_sensor_id.read().clone();
                            let Some(sensor) = sensors.iter().find(|s| s.id == selected_id) else {
                                status.set("Invalid selected sensor".to_string());
                                return;
                            };
                            let channel = Channel::from_layout(sensor.channel.as_str());
                            let mut next = cfg.read().clone().unwrap_or_default();
                            if !update_point(&mut next, channel, idx, kg, raw) {
                                status.set("Invalid selected point".to_string());
                                return;
                            }
                            spawn(async move {
                                match http_post_json::<CalibrationFile, CalibrationFile>("/api/calibration", &next).await {
                                    Ok(_) => {
                                        let body = RefitReq {
                                            channel: channel.api_name().to_string(),
                                            mode: fit_mode.read().clone(),
                                        };
                                        match http_post_json::<RefitReq, CalibrationFile>("/api/calibration/refit", &body).await {
                                            Ok(new_cfg) => {
                                                cfg.set(Some(new_cfg));
                                                status.set("Point edited".to_string());
                                            }
                                            Err(e) => status.set(format!("Refit failed: {e}")),
                                        }
                                    }
                                    Err(e) => status.set(format!("Save failed: {e}")),
                                }
                            });
                        }
                    },
                    "Save Selected Edit"
                }
            }

            div { style: "display:flex; gap:8px; flex-wrap:wrap; align-items:center;",
                input {
                    r#type: "number",
                    step: "0.01",
                    value: "{known_kg.read()}",
                    oninput: {
                        let mut known_kg = known_kg;
                        move |e| known_kg.set(e.value())
                    }
                }
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #f59e0b; background:#3f2500; color:#fde68a; cursor:pointer;",
                    disabled: *capture_active.read(),
                    onclick: {
                        let mut capture_active = capture_active;
                        let mut capture_mode = capture_mode;
                        let mut capture_weight = capture_weight;
                        let mut capture_vals = capture_vals;
                        move |_| {
                            capture_mode.set(CaptureMode::SequenceZero);
                            capture_weight.set(0.0);
                            capture_vals.set(Vec::new());
                            capture_active.set(true);
                        }
                    },
                    "Start New Sequence (0kg)"
                }
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #60a5fa; background:#0b1a33; color:#bfdbfe; cursor:pointer;",
                    disabled: *capture_active.read() || !sequence_started,
                    onclick: {
                        let mut capture_active = capture_active;
                        let mut capture_mode = capture_mode;
                        let mut capture_weight = capture_weight;
                        let mut capture_vals = capture_vals;
                        let known_kg = known_kg;
                        let mut status = status;
                        move |_| {
                            let Ok(kg) = known_kg.read().parse::<f32>() else {
                                status.set("Invalid sequence kg".to_string());
                                return;
                            };
                            if kg <= 0.0 {
                                status.set("Sequence point kg must be > 0".to_string());
                                return;
                            }
                            capture_mode.set(CaptureMode::SequencePoint);
                            capture_weight.set(kg);
                            capture_vals.set(Vec::new());
                            capture_active.set(true);
                        }
                    },
                    "Continue Sequence"
                }
                if *capture_active.read() {
                    span { style: "color:#facc15;", "Capturing {capture_vals.read().len()}/{capture_target}" }
                }
            }

            div { style: "display:flex; gap:8px; flex-wrap:wrap; align-items:flex-start;",
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #ef4444; background:#450a0a; color:#fecaca; cursor:pointer;",
                    disabled: cfg.read().is_none() || selected_point_idx.read().is_none(),
                    onclick: {
                        let mut cfg = cfg;
                        let selected_sensor_id = selected_sensor_id;
                        let sensors = sensors.clone();
                        let mut selected_point_idx = selected_point_idx;
                        let mut status = status;
                        move |_| {
                            let Some(idx) = *selected_point_idx.read() else {
                                status.set("Select a point first".to_string());
                                return;
                            };
                            let selected_id = selected_sensor_id.read().clone();
                            let Some(sensor) = sensors.iter().find(|s| s.id == selected_id) else {
                                status.set("Invalid selected sensor".to_string());
                                return;
                            };
                            let channel = Channel::from_layout(sensor.channel.as_str());
                            let mut next = cfg.read().clone().unwrap_or_default();
                            if !remove_point(&mut next, channel, idx) {
                                status.set("Invalid selected point".to_string());
                                return;
                            }
                            spawn(async move {
                                match http_post_json::<CalibrationFile, CalibrationFile>("/api/calibration", &next).await {
                                    Ok(new_cfg) => {
                                        cfg.set(Some(new_cfg));
                                        selected_point_idx.set(None);
                                        status.set("Point removed".to_string());
                                    }
                                    Err(e) => status.set(format!("Save failed: {e}")),
                                }
                            });
                        }
                    },
                    "Remove Selected"
                }
                button {
                    style: "padding:6px 12px; border-radius:999px; border:1px solid #ef4444; background:#450a0a; color:#fecaca; cursor:pointer;",
                    disabled: cfg.read().is_none(),
                    onclick: {
                        let mut cfg = cfg;
                        let selected_sensor_id = selected_sensor_id;
                        let sensors = sensors.clone();
                        let mut selected_point_idx = selected_point_idx;
                        let mut status = status;
                        move |_| {
                            let selected_id = selected_sensor_id.read().clone();
                            let Some(sensor) = sensors.iter().find(|s| s.id == selected_id) else {
                                status.set("Invalid selected sensor".to_string());
                                return;
                            };
                            let channel = Channel::from_layout(sensor.channel.as_str());
                            let mut next = cfg.read().clone().unwrap_or_default();
                            reset_channel(&mut next, channel);
                            spawn(async move {
                                match http_post_json::<CalibrationFile, CalibrationFile>("/api/calibration", &next).await {
                                    Ok(new_cfg) => {
                                        cfg.set(Some(new_cfg));
                                        selected_point_idx.set(None);
                                        status.set("Channel reset".to_string());
                                    }
                                    Err(e) => status.set(format!("Save failed: {e}")),
                                }
                            });
                        }
                    },
                    "Reset Channel"
                }
            }

            div { style: "display:grid; grid-template-columns:1fr; gap:6px; border:1px solid #334155; border-radius:10px; padding:10px; background:#0f172a;",
                for (idx, (raw, expected)) in points.clone().into_iter().enumerate() {
                    button {
                        style: if *selected_point_idx.read() == Some(idx) {
                            "text-align:left; padding:8px; border-radius:8px; border:1px solid #38bdf8; background:#0b1a33; color:#e2e8f0; cursor:pointer;"
                        } else {
                            "text-align:left; padding:8px; border-radius:8px; border:1px solid #334155; background:#111827; color:#e2e8f0; cursor:pointer;"
                        },
                        onclick: {
                            let mut selected_point_idx = selected_point_idx;
                            let mut manual_kg = manual_kg;
                            let mut manual_raw = manual_raw;
                            move |_| {
                                selected_point_idx.set(Some(idx));
                                manual_kg.set(format!("{expected}"));
                                manual_raw.set(format!("{raw}"));
                            }
                        },
                        "{expected:.4} -> raw {raw:.6}"
                    }
                }
                if points.is_empty() {
                    div { style: "color:#94a3b8;", "(no points for this channel)" }
                }
            }

            div { style: "border:1px solid #334155; border-radius:10px; padding:8px; background:#020617;",
                div {
                    style: "display:flex; align-items:center; gap:10px; flex-wrap:wrap; padding:6px 8px 10px 8px;",
                    svg { width: "30", height: "10", view_box: "0 0 30 10", style: "display:block; flex:0 0 auto;",
                        line { x1:"2", y1:"5", x2:"28", y2:"5", stroke:"{fit_color}", "stroke-width":"2.5", "stroke-linecap":"round" }
                    }
                    div {
                        style: "color:{fit_color}; font-size:12px; font-weight:700; letter-spacing:0.03em; text-transform:uppercase;",
                        "Calibration Fit"
                    }
                    div {
                        style: "color:{fit_color}; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace; font-variant-numeric:tabular-nums; white-space:pre-wrap; word-break:break-word; line-height:1.45; min-height:20px;",
                        "{fit_equation_text}"
                    }
                }
                svg { view_box: "0 0 {plot_w} {plot_h}", style: "width:100%; height:260px; display:block;",
                    rect { x:"0", y:"0", width:"{plot_w}", height:"{plot_h}", fill:"#020617" }
                    line { x1:"{pad_l}", y1:"{pad_t}", x2:"{pad_l}", y2:"{plot_h - pad_b}", stroke:"#334155", "stroke-width":"1" }
                    line { x1:"{pad_l}", y1:"{plot_h - pad_b}", x2:"{plot_w - pad_r}", y2:"{plot_h - pad_b}", stroke:"#334155", "stroke-width":"1" }
                    if !fit_path.is_empty() {
                        path { d: "{fit_path}", fill:"none", stroke:"{fit_color}", "stroke-width":"2.5" }
                    }
                    for (cx, cy) in scatter_xy.iter() {
                        circle { cx:"{cx}", cy:"{cy}", r:"3.5", fill:"#f59e0b" }
                    }
                    text { x:"6", y:"14", fill:"#94a3b8", "font-size":"11", {format!("y max {:.3}", y_max)} }
                    text { x:"6", y:"{plot_h - pad_b + 4.0}", fill:"#94a3b8", "font-size":"11", {format!("y min {:.3}", y_min)} }
                    text { x:"{pad_l}", y:"{plot_h - 6.0}", fill:"#94a3b8", "font-size":"11", {format!("x min {:.3}", x_min)} }
                    text { x:"{plot_w - 130.0}", y:"{plot_h - 6.0}", fill:"#94a3b8", "font-size":"11", {format!("x max {:.3}", x_max)} }
                }
            }

            div { style: "font-size:13px; color:#94a3b8;", "{status.read()}" }
        }
    }
}

fn metric_card(label: &str, value: String) -> Element {
    rsx! {
        div { style: "padding:8px 10px; border:1px solid #334155; border-radius:10px; background:#0f172a;",
            div { style: "font-size:11px; color:#94a3b8; margin-bottom:2px;", "{label}" }
            div {
                style: "font-size:14px; color:#e2e8f0; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; display:inline-block; min-width:14ch; text-align:right; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace; font-variant-numeric:tabular-nums;",
                "{value}"
            }
        }
    }
}
