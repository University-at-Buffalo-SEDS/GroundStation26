use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const DEFAULT_FULL_MASS_KG: f32 = 10.0;
pub const CALIBRATION_CAPTURE_TARGET_SAMPLES: usize = 200;
pub const RAW_LOADCELL_DATA_TYPE_1000KG: &str = "KG1000";
pub const RAW_PRESSURE_TRANSDUCER_DATA_TYPE: &str = "IADC";
pub const DERIVED_WEIGHT_DATA_TYPE: &str = "LOADCELL_WEIGHT_KG";
pub const DERIVED_FILL_PERCENT_DATA_TYPE: &str = "LOADCELL_FILL_PERCENT";
pub const DERIVED_PRESSURE_TRANSDUCER_CALIBRATED_DATA_TYPE: &str = "PRESSURE_TRANSDUCER_CALIBRATED";
#[cfg(feature = "testing")]
const DEFAULT_LOADCELL_CALIBRATION_FILENAME: &str = "loadcell_calibration_testing.json";
#[cfg(all(not(feature = "testing"), feature = "test_fire_mode"))]
const DEFAULT_LOADCELL_CALIBRATION_FILENAME: &str = "loadcell_calibration_test_fire.json";
#[cfg(all(not(feature = "testing"), not(feature = "test_fire_mode")))]
const DEFAULT_LOADCELL_CALIBRATION_FILENAME: &str = "loadcell_calibration.json";

const ALL_CALIBRATION_FIT_MODES: [&str; 9] = [
    "best",
    "linear",
    "linear_zero",
    "parabolic",
    "parabolic_zero",
    "cubic",
    "cubic_zero",
    "quartic",
    "quartic_zero",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalibrationChannel {
    Custom(String),
}

impl CalibrationChannel {
    pub fn from_str(s: &str) -> Self {
        let trimmed = s.trim();
        Self::Custom(if trimmed.is_empty() {
            "unknown".to_string()
        } else {
            trimmed.to_string()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Best,
    Linear,
    LinearZero,
    Poly2,
    Poly2Zero,
    Poly3,
    Poly3Zero,
    Poly4,
    Poly4Zero,
}

impl FitMode {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "best" => Some(Self::Best),
            "linear" => Some(Self::Linear),
            "linear_zero" => Some(Self::LinearZero),
            "poly2" | "parabolic" | "quadratic" => Some(Self::Poly2),
            "poly2_zero" | "parabolic_zero" | "quadratic_zero" => Some(Self::Poly2Zero),
            "poly3" | "cubic" => Some(Self::Poly3),
            "poly3_zero" | "cubic_zero" => Some(Self::Poly3Zero),
            "poly4" | "quartic" => Some(Self::Poly4),
            "poly4_zero" | "quartic_zero" => Some(Self::Poly4Zero),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelLinear {
    pub m: Option<f32>,
    pub b: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FitMeta {
    #[serde(rename = "type")]
    pub fit_type: Option<String>,
    pub a: Option<f32>,
    pub b: Option<f32>,
    pub c: Option<f32>,
    pub d: Option<f32>,
    pub e: Option<f32>,
    pub x0: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointCh1 {
    pub kg: f32,
    pub ch1_raw: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointIadc {
    pub expected: f32,
    pub iadc_raw: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalibrationPoint {
    pub expected: f32,
    pub raw: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GenericCalibrationChannel {
    #[serde(default)]
    pub linear: ChannelLinear,
    #[serde(default)]
    pub zero_raw: Option<f32>,
    #[serde(default)]
    pub points: Vec<CalibrationPoint>,
    #[serde(default)]
    pub fit: Option<FitMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadcellCalibrationFile {
    #[serde(default = "default_calibration_version")]
    pub version: u32,
    #[serde(default)]
    pub full_mass_kg: Option<f32>,
    #[serde(default)]
    pub ch1: ChannelLinear,
    #[serde(default)]
    pub iadc: ChannelLinear,
    #[serde(default)]
    pub ch1_zero_raw: Option<f32>,
    #[serde(default)]
    pub iadc_zero_raw: Option<f32>,
    #[serde(default)]
    pub points_ch1: Vec<PointCh1>,
    #[serde(default)]
    pub points_iadc: Vec<PointIadc>,
    #[serde(default)]
    pub ch1_fit: Option<FitMeta>,
    #[serde(default)]
    pub iadc_fit: Option<FitMeta>,
    #[serde(default)]
    pub weights_kg: Vec<f32>,
    #[serde(default)]
    pub extra_channels: BTreeMap<String, GenericCalibrationChannel>,
}

impl Default for LoadcellCalibrationFile {
    fn default() -> Self {
        Self {
            version: default_calibration_version(),
            full_mass_kg: Some(DEFAULT_FULL_MASS_KG),
            ch1: ChannelLinear {
                m: Some(1.0),
                b: Some(0.0),
            },
            iadc: ChannelLinear {
                m: Some(1.0),
                b: Some(0.0),
            },
            ch1_zero_raw: None,
            iadc_zero_raw: None,
            points_ch1: Vec::new(),
            points_iadc: Vec::new(),
            ch1_fit: Some(FitMeta {
                fit_type: Some("linear".to_string()),
                ..FitMeta::default()
            }),
            iadc_fit: Some(FitMeta {
                fit_type: Some("linear".to_string()),
                ..FitMeta::default()
            }),
            weights_kg: Vec::new(),
            extra_channels: BTreeMap::new(),
        }
    }
}

fn default_calibration_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationSensorSpec {
    pub id: String,
    pub label: String,
    pub data_type: String,
    pub channel: String,
    pub fit_color: String,
    pub raw_label: String,
    pub expected_label: String,
    pub fit_modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationTabLayout {
    pub capture_target_samples: usize,
    pub fit_modes: Vec<String>,
    pub sensors: Vec<CalibrationSensorSpec>,
}

pub fn calibration_tab_layout() -> CalibrationTabLayout {
    let fit_modes: Vec<String> = ALL_CALIBRATION_FIT_MODES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(config) = load_calibration_tab_layout_from_file() {
        return config;
    }
    CalibrationTabLayout {
        capture_target_samples: CALIBRATION_CAPTURE_TARGET_SAMPLES,
        fit_modes: fit_modes.clone(),
        sensors: vec![
            CalibrationSensorSpec {
                id: "KG1000".to_string(),
                label: "1000kg".to_string(),
                data_type: "KG1000".to_string(),
                channel: "ch1".to_string(),
                fit_color: "#22d3ee".to_string(),
                raw_label: "Raw".to_string(),
                expected_label: "kg".to_string(),
                fit_modes: fit_modes.clone(),
            },
            CalibrationSensorSpec {
                id: "IADC".to_string(),
                label: "Tank Pressure".to_string(),
                data_type: "FUEL_TANK_PRESSURE".to_string(),
                channel: "iadc".to_string(),
                fit_color: "#a78bfa".to_string(),
                raw_label: "Raw".to_string(),
                expected_label: "psi".to_string(),
                fit_modes,
            },
        ],
    }
}

fn calibration_config_path() -> PathBuf {
    if let Ok(path) = std::env::var("GS_CALIBRATION_CONFIG_PATH") {
        return PathBuf::from(path);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config")
        .join("calibration_config.json")
}

fn load_calibration_tab_layout_from_file() -> Option<CalibrationTabLayout> {
    let path = calibration_config_path();
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<CalibrationTabLayout>(&raw).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OldSensorCalibration {
    sensor_id: String,
    slope: f32,
    intercept: f32,
    zero_raw: Option<f32>,
    span_raw: Option<f32>,
    span_known_kg: Option<f32>,
    enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OldCalibrationFile {
    version: u32,
    full_mass_kg: f32,
    sensors: Vec<OldSensorCalibration>,
}

fn calibration_path() -> PathBuf {
    if let Ok(path) = std::env::var("GS_LOADCELL_CALIBRATION_PATH") {
        return PathBuf::from(path);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    #[cfg(feature = "testing")]
    {
        manifest_dir
            .join("calibration")
            .join(DEFAULT_LOADCELL_CALIBRATION_FILENAME)
    }
    #[cfg(not(feature = "testing"))]
    {
        manifest_dir
            .join("data")
            .join(DEFAULT_LOADCELL_CALIBRATION_FILENAME)
    }
}

fn from_old_format(old: OldCalibrationFile) -> LoadcellCalibrationFile {
    let mut cfg = LoadcellCalibrationFile {
        full_mass_kg: Some(old.full_mass_kg.max(0.001)),
        ..LoadcellCalibrationFile::default()
    };
    for s in old.sensors {
        match s.sensor_id.as_str() {
            "KG1000" => {
                cfg.ch1.m = Some(s.slope);
                cfg.ch1.b = Some(s.intercept);
                cfg.ch1_zero_raw = s.zero_raw;
                if let (Some(raw), Some(kg)) = (s.span_raw, s.span_known_kg) {
                    cfg.points_ch1.push(PointCh1 { kg, ch1_raw: raw });
                }
            }
            _ => {}
        }
    }
    cfg
}

pub fn load_or_default() -> LoadcellCalibrationFile {
    let path = calibration_path();
    let mut cfg = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<LoadcellCalibrationFile>(&raw).ok())
        .or_else(|| {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<OldCalibrationFile>(&raw).ok())
                .map(from_old_format)
        })
        .unwrap_or_default();
    sync_legacy_channels_into_extra(&mut cfg);
    let _ = save(&cfg);
    cfg
}

fn sync_legacy_channels_into_extra(cfg: &mut LoadcellCalibrationFile) {
    let ch1 = cfg.extra_channels.entry("ch1".to_string()).or_default();
    if ch1.points.is_empty() && !cfg.points_ch1.is_empty() {
        ch1.points = cfg
            .points_ch1
            .iter()
            .map(|point| CalibrationPoint {
                expected: point.kg,
                raw: point.ch1_raw,
            })
            .collect();
    }
    if ch1.linear.m.is_none() && ch1.linear.b.is_none() {
        ch1.linear = cfg.ch1.clone();
    }
    if ch1.zero_raw.is_none() {
        ch1.zero_raw = cfg.ch1_zero_raw;
    }
    if ch1.fit.is_none() {
        ch1.fit = cfg.ch1_fit.clone();
    }

    let iadc = cfg.extra_channels.entry("iadc".to_string()).or_default();
    if iadc.points.is_empty() && !cfg.points_iadc.is_empty() {
        iadc.points = cfg
            .points_iadc
            .iter()
            .map(|point| CalibrationPoint {
                expected: point.expected,
                raw: point.iadc_raw,
            })
            .collect();
    }
    if iadc.linear.m.is_none() && iadc.linear.b.is_none() {
        iadc.linear = cfg.iadc.clone();
    }
    if iadc.zero_raw.is_none() {
        iadc.zero_raw = cfg.iadc_zero_raw;
    }
    if iadc.fit.is_none() {
        iadc.fit = cfg.iadc_fit.clone();
    }
}

pub fn save(cfg: &LoadcellCalibrationFile) -> Result<(), String> {
    let path = calibration_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all({parent:?}): {e}"))?;
    }
    let raw =
        serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize calibration: {e}"))?;
    std::fs::write(&path, raw).map_err(|e| format!("write calibration {path:?}: {e}"))?;
    Ok(())
}

fn points_for_channel(
    cfg: &LoadcellCalibrationFile,
    channel: &CalibrationChannel,
) -> Vec<(f64, f64)> {
    match channel {
        CalibrationChannel::Custom(key) => cfg
            .extra_channels
            .get(key)
            .map(|channel| {
                channel
                    .points
                    .iter()
                    .map(|p| (p.raw as f64, p.expected as f64))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn channel_linear_mut<'a>(
    cfg: &'a mut LoadcellCalibrationFile,
    channel: &CalibrationChannel,
) -> &'a mut ChannelLinear {
    match channel {
        CalibrationChannel::Custom(key) => {
            &mut cfg.extra_channels.entry(key.clone()).or_default().linear
        }
    }
}

fn zero_raw_mut<'a>(
    cfg: &'a mut LoadcellCalibrationFile,
    channel: &CalibrationChannel,
) -> &'a mut Option<f32> {
    match channel {
        CalibrationChannel::Custom(key) => {
            &mut cfg.extra_channels.entry(key.clone()).or_default().zero_raw
        }
    }
}

fn fit_meta_mut<'a>(
    cfg: &'a mut LoadcellCalibrationFile,
    channel: &CalibrationChannel,
) -> &'a mut Option<FitMeta> {
    match channel {
        CalibrationChannel::Custom(key) => {
            &mut cfg.extra_channels.entry(key.clone()).or_default().fit
        }
    }
}

fn update_weights_kg(cfg: &mut LoadcellCalibrationFile) {
    let mut values: Vec<f32> = cfg
        .points_ch1
        .iter()
        .map(|p| p.kg)
        .chain(cfg.points_iadc.iter().map(|p| p.expected))
        .chain(
            cfg.extra_channels
                .values()
                .flat_map(|channel| channel.points.iter().map(|p| p.expected)),
        )
        .filter(|v| v.is_finite())
        .collect();
    values.sort_by(f32::total_cmp);
    values.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    cfg.weights_kg = values;
}

pub fn upsert_point(
    cfg: &mut LoadcellCalibrationFile,
    channel: CalibrationChannel,
    expected: f32,
    raw: f32,
) {
    let expected = expected.max(0.0);
    match &channel {
        CalibrationChannel::Custom(key) => {
            let channel = cfg.extra_channels.entry(key.clone()).or_default();
            if let Some(p) = channel
                .points
                .iter_mut()
                .find(|p| (p.expected - expected).abs() < 1e-6)
            {
                p.raw = raw;
            } else {
                channel.points.push(CalibrationPoint { expected, raw });
            }
        }
    }
    update_weights_kg(cfg);
}

pub fn capture_zero(cfg: &mut LoadcellCalibrationFile, sensor_id: &str, raw: f32) {
    let channel = CalibrationChannel::from_str(sensor_id);
    *zero_raw_mut(cfg, &channel) = Some(raw);
}

pub fn capture_span(cfg: &mut LoadcellCalibrationFile, sensor_id: &str, raw: f32, known_kg: f32) {
    let channel = CalibrationChannel::from_str(sensor_id);
    upsert_point(cfg, channel.clone(), known_kg, raw);
    let mode = if zero_raw(&channel, cfg).is_some() {
        FitMode::LinearZero
    } else {
        FitMode::Linear
    };
    let _ = refit_channel(cfg, channel, mode);
}

fn zero_raw(channel: &CalibrationChannel, cfg: &LoadcellCalibrationFile) -> Option<f32> {
    match channel {
        CalibrationChannel::Custom(key) => cfg.extra_channels.get(key).and_then(|c| c.zero_raw),
    }
}

fn fit_line(xs: &[f64], ys: &[f64]) -> Result<(f64, f64), String> {
    let n = xs.len() as f64;
    let sx: f64 = xs.iter().sum();
    let sy: f64 = ys.iter().sum();
    let sxx: f64 = xs.iter().map(|x| x * x).sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| x * y).sum();
    let denom = n * sxx - sx * sx;
    if denom.abs() < 1e-18 {
        return Err("degenerate points for linear fit".to_string());
    }
    let m = (n * sxy - sx * sy) / denom;
    let b = (sy - m * sx) / n;
    Ok((m, b))
}

fn fit_line_through_zero(xs: &[f64], ys: &[f64]) -> Result<f64, String> {
    let denom: f64 = xs.iter().map(|x| x * x).sum();
    if denom.abs() < 1e-18 {
        return Err("degenerate points for linear-zero fit".to_string());
    }
    Ok(xs.iter().zip(ys).map(|(x, y)| x * y).sum::<f64>() / denom)
}

fn fit_poly2(xs: &[f64], ys: &[f64]) -> Result<(f64, f64, f64), String> {
    let n = xs.len() as f64;
    let sx: f64 = xs.iter().sum();
    let sx2: f64 = xs.iter().map(|x| x * x).sum();
    let sx3: f64 = xs.iter().map(|x| x * x * x).sum();
    let sx4: f64 = xs.iter().map(|x| x * x * x * x).sum();
    let sy: f64 = ys.iter().sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| x * y).sum();
    let sx2y: f64 = xs.iter().zip(ys).map(|(x, y)| x * x * y).sum();

    let a11 = sx4;
    let a12 = sx3;
    let a13 = sx2;
    let a21 = sx3;
    let mut a22 = sx2;
    let mut a23 = sx;
    let a31 = sx2;
    let mut a32 = sx;
    let mut a33 = n;
    let b1 = sx2y;
    let mut b2 = sxy;
    let mut b3 = sy;

    if a11.abs() < 1e-18 {
        return Err("degenerate points for poly2 fit".to_string());
    }
    let f21 = a21 / a11;
    let f31 = a31 / a11;
    a22 -= f21 * a12;
    a23 -= f21 * a13;
    b2 -= f21 * b1;
    a32 -= f31 * a12;
    a33 -= f31 * a13;
    b3 -= f31 * b1;

    if a22.abs() < 1e-18 {
        return Err("degenerate points for poly2 fit".to_string());
    }
    let f32 = a32 / a22;
    a33 -= f32 * a23;
    b3 -= f32 * b2;
    if a33.abs() < 1e-18 {
        return Err("degenerate points for poly2 fit".to_string());
    }

    let c = b3 / a33;
    let b = (b2 - a23 * c) / a22;
    let a = (b1 - a12 * b - a13 * c) / a11;
    Ok((a, b, c))
}

fn fit_poly2_through_zero(xs: &[f64], ys: &[f64]) -> Result<(f64, f64), String> {
    let sx2: f64 = xs.iter().map(|x| x * x).sum();
    let sx3: f64 = xs.iter().map(|x| x * x * x).sum();
    let sx4: f64 = xs.iter().map(|x| x * x * x * x).sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| x * y).sum();
    let sx2y: f64 = xs.iter().zip(ys).map(|(x, y)| x * x * y).sum();
    let det = sx4 * sx2 - sx3 * sx3;
    if det.abs() < 1e-18 {
        return Err("degenerate points for poly2-zero fit".to_string());
    }
    let a = (sx2y * sx2 - sxy * sx3) / det;
    let b = (sx4 * sxy - sx3 * sx2y) / det;
    Ok((a, b))
}

fn solve_linear_system(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Result<Vec<f64>, String> {
    let n = a.len();
    if n == 0 || b.len() != n || a.iter().any(|row| row.len() != n) {
        return Err("invalid linear system dimensions".to_string());
    }
    for i in 0..n {
        let mut pivot = i;
        let mut max_abs = a[i][i].abs();
        for (r, row) in a.iter().enumerate().skip(i + 1) {
            let v = row[i].abs();
            if v > max_abs {
                max_abs = v;
                pivot = r;
            }
        }
        if max_abs < 1e-18 {
            return Err("degenerate system".to_string());
        }
        if pivot != i {
            a.swap(i, pivot);
            b.swap(i, pivot);
        }
        let pivot_val = a[i][i];
        for item in a[i].iter_mut().skip(i) {
            *item /= pivot_val;
        }
        b[i] /= pivot_val;
        for r in 0..n {
            if r == i {
                continue;
            }
            let factor = a[r][i];
            if factor.abs() < 1e-18 {
                continue;
            }
            let pivot_tail = a[i][i..].to_vec();
            for (dest, pivot_entry) in a[r].iter_mut().skip(i).zip(pivot_tail.iter()) {
                *dest -= factor * *pivot_entry;
            }
            b[r] -= factor * b[i];
        }
    }
    Ok(b)
}

fn fit_poly3(xs: &[f64], ys: &[f64]) -> Result<(f64, f64, f64, f64), String> {
    let sx: f64 = xs.iter().sum();
    let sx2: f64 = xs.iter().map(|x| x * x).sum();
    let sx3: f64 = xs.iter().map(|x| x * x * x).sum();
    let sx4: f64 = xs.iter().map(|x| x * x * x * x).sum();
    let sx5: f64 = xs.iter().map(|x| x * x * x * x * x).sum();
    let sx6: f64 = xs.iter().map(|x| x * x * x * x * x * x).sum();
    let sy: f64 = ys.iter().sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| x * y).sum();
    let sx2y: f64 = xs.iter().zip(ys).map(|(x, y)| x * x * y).sum();
    let sx3y: f64 = xs.iter().zip(ys).map(|(x, y)| x * x * x * y).sum();

    let a = vec![
        vec![sx6, sx5, sx4, sx3],
        vec![sx5, sx4, sx3, sx2],
        vec![sx4, sx3, sx2, sx],
        vec![sx3, sx2, sx, xs.len() as f64],
    ];
    let b = vec![sx3y, sx2y, sxy, sy];
    let sol = solve_linear_system(a, b)?;
    Ok((sol[0], sol[1], sol[2], sol[3]))
}

fn fit_poly3_through_zero(xs: &[f64], ys: &[f64]) -> Result<(f64, f64, f64), String> {
    let sx2: f64 = xs.iter().map(|x| x * x).sum();
    let sx3: f64 = xs.iter().map(|x| x * x * x).sum();
    let sx4: f64 = xs.iter().map(|x| x * x * x * x).sum();
    let sx5: f64 = xs.iter().map(|x| x * x * x * x * x).sum();
    let sx6: f64 = xs.iter().map(|x| x * x * x * x * x * x).sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| x * y).sum();
    let sx2y: f64 = xs.iter().zip(ys).map(|(x, y)| x * x * y).sum();
    let sx3y: f64 = xs.iter().zip(ys).map(|(x, y)| x * x * x * y).sum();

    let a = vec![
        vec![sx6, sx5, sx4],
        vec![sx5, sx4, sx3],
        vec![sx4, sx3, sx2],
    ];
    let b = vec![sx3y, sx2y, sxy];
    let sol = solve_linear_system(a, b)?;
    Ok((sol[0], sol[1], sol[2]))
}

fn fit_poly_degree(xs: &[f64], ys: &[f64], degree: usize) -> Result<Vec<f64>, String> {
    if xs.len() <= degree {
        return Err(format!(
            "need at least {} points for poly{degree} fit",
            degree + 1
        ));
    }
    let n = degree + 1;
    let mut a = vec![vec![0.0; n]; n];
    let mut b = vec![0.0; n];
    for (row, row_values) in a.iter_mut().enumerate().take(n) {
        for (col, slot) in row_values.iter_mut().enumerate().take(n) {
            let power = (2 * degree).saturating_sub(row + col) as i32;
            *slot = xs.iter().map(|x| x.powi(power)).sum();
        }
        let power = degree.saturating_sub(row) as i32;
        b[row] = xs.iter().zip(ys).map(|(x, y)| x.powi(power) * y).sum();
    }
    solve_linear_system(a, b)
}

fn fit_poly4(xs: &[f64], ys: &[f64]) -> Result<(f64, f64, f64, f64, f64), String> {
    let sol = fit_poly_degree(xs, ys, 4)?;
    Ok((sol[0], sol[1], sol[2], sol[3], sol[4]))
}

fn fit_poly4_through_zero(xs: &[f64], ys: &[f64]) -> Result<(f64, f64, f64, f64), String> {
    if xs.len() < 4 {
        return Err("need at least 4 points for poly4-zero fit".to_string());
    }
    let a = vec![
        vec![
            xs.iter().map(|x| x.powi(8)).sum(),
            xs.iter().map(|x| x.powi(7)).sum(),
            xs.iter().map(|x| x.powi(6)).sum(),
            xs.iter().map(|x| x.powi(5)).sum(),
        ],
        vec![
            xs.iter().map(|x| x.powi(7)).sum(),
            xs.iter().map(|x| x.powi(6)).sum(),
            xs.iter().map(|x| x.powi(5)).sum(),
            xs.iter().map(|x| x.powi(4)).sum(),
        ],
        vec![
            xs.iter().map(|x| x.powi(6)).sum(),
            xs.iter().map(|x| x.powi(5)).sum(),
            xs.iter().map(|x| x.powi(4)).sum(),
            xs.iter().map(|x| x.powi(3)).sum(),
        ],
        vec![
            xs.iter().map(|x| x.powi(5)).sum(),
            xs.iter().map(|x| x.powi(4)).sum(),
            xs.iter().map(|x| x.powi(3)).sum(),
            xs.iter().map(|x| x.powi(2)).sum(),
        ],
    ];
    let b = vec![
        xs.iter().zip(ys).map(|(x, y)| x.powi(4) * y).sum(),
        xs.iter().zip(ys).map(|(x, y)| x.powi(3) * y).sum(),
        xs.iter().zip(ys).map(|(x, y)| x.powi(2) * y).sum(),
        xs.iter().zip(ys).map(|(x, y)| x * y).sum(),
    ];
    let sol = solve_linear_system(a, b)?;
    Ok((sol[0], sol[1], sol[2], sol[3]))
}

fn sse_line(xs: &[f64], ys: &[f64], m: f64, b: f64) -> f64 {
    xs.iter()
        .zip(ys)
        .map(|(x, y)| {
            let e = y - (m * x + b);
            e * e
        })
        .sum()
}

fn sse_poly2(xs: &[f64], ys: &[f64], a: f64, b: f64, c: f64) -> f64 {
    xs.iter()
        .zip(ys)
        .map(|(x, y)| {
            let e = y - (a * x * x + b * x + c);
            e * e
        })
        .sum()
}

fn sse_poly3(xs: &[f64], ys: &[f64], a: f64, b: f64, c: f64, d: f64) -> f64 {
    xs.iter()
        .zip(ys)
        .map(|(x, y)| {
            let e = y - (a * x * x * x + b * x * x + c * x + d);
            e * e
        })
        .sum()
}

fn sse_poly4(xs: &[f64], ys: &[f64], a: f64, b: f64, c: f64, d: f64, e0: f64) -> f64 {
    xs.iter()
        .zip(ys)
        .map(|(x, y)| {
            let e = y - (a * x.powi(4) + b * x.powi(3) + c * x * x + d * x + e0);
            e * e
        })
        .sum()
}

fn aic(sse: f64, n: usize, k: usize) -> f64 {
    if n == 0 {
        return f64::INFINITY;
    }
    let s = sse.max(1e-18);
    (n as f64) * (s / n as f64).ln() + 2.0 * (k as f64)
}

pub fn refit_channel(
    cfg: &mut LoadcellCalibrationFile,
    channel: CalibrationChannel,
    mode: FitMode,
) -> Result<(), String> {
    let pts = points_for_channel(cfg, &channel);
    if pts.len() < 2 {
        return Err("need at least 2 points".to_string());
    }

    let xs: Vec<f64> = pts.iter().map(|(x, _)| *x).collect();
    let ys: Vec<f64> = pts.iter().map(|(_, y)| *y).collect();
    let zero_hint = zero_raw(&channel, cfg)
        .map(|v| v as f64)
        .or_else(|| pts.iter().find(|(_, y)| y.abs() < 1e-9).map(|(x, _)| *x));

    let mut candidates: Vec<(FitMode, f64)> = Vec::new();

    let (lin_m, lin_b) = fit_line(&xs, &ys)?;
    candidates.push((
        FitMode::Linear,
        aic(sse_line(&xs, &ys, lin_m, lin_b), xs.len(), 2),
    ));

    let mut lin0_m = None;
    if let Some(x0) = zero_hint {
        let xs_shift: Vec<f64> = xs.iter().map(|x| x - x0).collect();
        let m = fit_line_through_zero(&xs_shift, &ys)?;
        lin0_m = Some((m, x0));
        candidates.push((
            FitMode::LinearZero,
            aic(sse_line(&xs_shift, &ys, m, 0.0), xs_shift.len(), 1),
        ));
    }

    let mut poly2 = None;
    if xs.len() >= 3 {
        let (a, b, c) = fit_poly2(&xs, &ys)?;
        poly2 = Some((a, b, c));
        candidates.push((
            FitMode::Poly2,
            aic(sse_poly2(&xs, &ys, a, b, c), xs.len(), 3),
        ));
    }

    let mut poly2_zero = None;
    if let Some(x0) = zero_hint
        && xs.len() >= 2
    {
        let xs_shift: Vec<f64> = xs.iter().map(|x| x - x0).collect();
        let (a, b) = fit_poly2_through_zero(&xs_shift, &ys)?;
        poly2_zero = Some((a, b, x0));
        candidates.push((
            FitMode::Poly2Zero,
            aic(sse_poly2(&xs_shift, &ys, a, b, 0.0), xs_shift.len(), 2),
        ));
    }

    let mut poly3 = None;
    if xs.len() >= 4 {
        let (a, b, c, d) = fit_poly3(&xs, &ys)?;
        poly3 = Some((a, b, c, d));
        candidates.push((
            FitMode::Poly3,
            aic(sse_poly3(&xs, &ys, a, b, c, d), xs.len(), 4),
        ));
    }

    let mut poly3_zero = None;
    if let Some(x0) = zero_hint
        && xs.len() >= 3
    {
        let xs_shift: Vec<f64> = xs.iter().map(|x| x - x0).collect();
        let (a, b, c) = fit_poly3_through_zero(&xs_shift, &ys)?;
        poly3_zero = Some((a, b, c, x0));
        candidates.push((
            FitMode::Poly3Zero,
            aic(sse_poly3(&xs_shift, &ys, a, b, c, 0.0), xs_shift.len(), 3),
        ));
    }

    let mut poly4 = None;
    if xs.len() >= 5 {
        let (a, b, c, d, e) = fit_poly4(&xs, &ys)?;
        poly4 = Some((a, b, c, d, e));
        candidates.push((
            FitMode::Poly4,
            aic(sse_poly4(&xs, &ys, a, b, c, d, e), xs.len(), 5),
        ));
    }

    let mut poly4_zero = None;
    if let Some(x0) = zero_hint
        && xs.len() >= 4
    {
        let xs_shift: Vec<f64> = xs.iter().map(|x| x - x0).collect();
        let (a, b, c, d) = fit_poly4_through_zero(&xs_shift, &ys)?;
        poly4_zero = Some((a, b, c, d, x0));
        candidates.push((
            FitMode::Poly4Zero,
            aic(
                sse_poly4(&xs_shift, &ys, a, b, c, d, 0.0),
                xs_shift.len(),
                4,
            ),
        ));
    }

    let chosen = if mode == FitMode::Best {
        candidates
            .iter()
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(m, _)| *m)
            .ok_or_else(|| "no fit candidates".to_string())?
    } else {
        mode
    };

    match chosen {
        FitMode::Best => unreachable!(),
        FitMode::Linear => {
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(lin_m as f32);
            lin_slot.b = Some(lin_b as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("linear".to_string()),
                x0: None,
                ..FitMeta::default()
            });
        }
        FitMode::LinearZero => {
            let (m, x0) = lin0_m.ok_or_else(|| "linear_zero fit unavailable".to_string())?;
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(m as f32);
            lin_slot.b = Some((-m * x0) as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("linear".to_string()),
                x0: Some(x0 as f32),
                ..FitMeta::default()
            });
        }
        FitMode::Poly2 => {
            let (a, b, c) = poly2.ok_or_else(|| "poly2 fit unavailable".to_string())?;
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(b as f32);
            lin_slot.b = Some(c as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("poly2".to_string()),
                a: Some(a as f32),
                b: Some(b as f32),
                c: Some(c as f32),
                d: None,
                x0: None,
                ..FitMeta::default()
            });
        }
        FitMode::Poly2Zero => {
            let (a, b, x0) = poly2_zero.ok_or_else(|| "poly2_zero fit unavailable".to_string())?;
            let m_lin = a + b;
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(m_lin as f32);
            lin_slot.b = Some((-m_lin * x0) as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("poly2".to_string()),
                a: Some(a as f32),
                b: Some(b as f32),
                c: Some(0.0),
                d: Some(0.0),
                x0: Some(x0 as f32),
                ..FitMeta::default()
            });
        }
        FitMode::Poly3 => {
            let (a, b, c, d) = poly3.ok_or_else(|| "poly3 fit unavailable".to_string())?;
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(c as f32);
            lin_slot.b = Some(d as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("poly3".to_string()),
                a: Some(a as f32),
                b: Some(b as f32),
                c: Some(c as f32),
                d: Some(d as f32),
                x0: None,
                ..FitMeta::default()
            });
        }
        FitMode::Poly3Zero => {
            let (a, b, c, x0) =
                poly3_zero.ok_or_else(|| "poly3_zero fit unavailable".to_string())?;
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(c as f32);
            lin_slot.b = Some((-c * x0) as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("poly3".to_string()),
                a: Some(a as f32),
                b: Some(b as f32),
                c: Some(c as f32),
                d: Some(0.0),
                x0: Some(x0 as f32),
                ..FitMeta::default()
            });
        }
        FitMode::Poly4 => {
            let (a, b, c, d, e) = poly4.ok_or_else(|| "poly4 fit unavailable".to_string())?;
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(d as f32);
            lin_slot.b = Some(e as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("poly4".to_string()),
                a: Some(a as f32),
                b: Some(b as f32),
                c: Some(c as f32),
                d: Some(d as f32),
                e: Some(e as f32),
                x0: None,
            });
        }
        FitMode::Poly4Zero => {
            let (a, b, c, d, x0) =
                poly4_zero.ok_or_else(|| "poly4_zero fit unavailable".to_string())?;
            let lin_slot = channel_linear_mut(cfg, &channel);
            lin_slot.m = Some(d as f32);
            lin_slot.b = Some((-d * x0) as f32);
            *fit_meta_mut(cfg, &channel) = Some(FitMeta {
                fit_type: Some("poly4".to_string()),
                a: Some(a as f32),
                b: Some(b as f32),
                c: Some(c as f32),
                d: Some(d as f32),
                e: Some(0.0),
                x0: Some(x0 as f32),
            });
        }
    }
    Ok(())
}

fn eval_channel_with_fit(linear: &ChannelLinear, fit: Option<&FitMeta>, raw: f32) -> Option<f32> {
    let fit_type = fit.and_then(|f| f.fit_type.as_deref());
    if let Some(meta) = fit {
        let x = raw - meta.x0.unwrap_or(0.0);
        if fit_type == Some("poly4") {
            let a = meta.a?;
            let b = meta.b?;
            let c = meta.c.unwrap_or(0.0);
            let d = meta.d.unwrap_or(0.0);
            let e = meta.e.unwrap_or(0.0);
            return Some((a * x.powi(4) + b * x.powi(3) + c * x * x + d * x + e).max(0.0));
        }
        if fit_type == Some("poly3") {
            let a = meta.a?;
            let b = meta.b?;
            let c = meta.c.unwrap_or(0.0);
            let d = meta.d.unwrap_or(0.0);
            return Some((a * x * x * x + b * x * x + c * x + d).max(0.0));
        }
        if fit_type == Some("poly2") {
            let a = meta.a?;
            let b = meta.b?;
            let c = meta.c.unwrap_or(0.0);
            return Some((a * x * x + b * x + c).max(0.0));
        }
    }
    let m = linear.m?;
    let b = linear.b.unwrap_or(0.0);
    Some((m * raw + b).max(0.0))
}

pub fn calibrated_weight_kg(
    cfg: &LoadcellCalibrationFile,
    sensor_id: &str,
    raw: f32,
) -> Option<f32> {
    match sensor_id {
        "KG1000" => eval_channel_with_fit(&cfg.ch1, cfg.ch1_fit.as_ref(), raw),
        _ => None,
    }
}

pub fn calibrated_sensor_value(
    cfg: &LoadcellCalibrationFile,
    sensor_id: &str,
    raw: f32,
) -> Option<f32> {
    match sensor_id {
        RAW_LOADCELL_DATA_TYPE_1000KG => calibrated_weight_kg(cfg, sensor_id, raw),
        RAW_PRESSURE_TRANSDUCER_DATA_TYPE => {
            eval_channel_with_fit(&cfg.iadc, cfg.iadc_fit.as_ref(), raw)
        }
        _ => None,
    }
}

pub fn fill_percent(cfg: &LoadcellCalibrationFile, weight_kg: f32) -> f32 {
    let denom = cfg.full_mass_kg.unwrap_or(DEFAULT_FULL_MASS_KG).max(0.0001);
    ((weight_kg / denom) * 100.0).clamp(0.0, 100.0)
}
