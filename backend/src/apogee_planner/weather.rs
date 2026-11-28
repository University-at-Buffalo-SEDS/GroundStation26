//! Weather "plugin": manual entry, the Open-Meteo forecast (opt-in) and the
//! cached last forecast (docs/apogee-planner.md section 3).
use super::flight::{atmosphere_for_surface, wind_vector, Environment, WindProfile};
use serde::{Deserialize, Serialize};

pub const OPEN_METEO_URL: &str = "https://api.open-meteo.com/v1/forecast";
/// 1000..500 hPa: from below any launch site to well above 10k ft AGL.
pub const PRESSURE_LEVELS: [u32; 10] = [1000, 975, 950, 925, 900, 850, 800, 700, 600, 500];
/// Generic client name: no station, user or project in requests.
pub const USER_AGENT: &str = "groundstation-weather/1";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Site {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_msl_m: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Level {
    pub h_agl_m: f64,
    pub speed_mps: f64,
    pub from_deg: f64,
    #[serde(default)]
    pub temp_c: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    /// "manual" or "open_meteo".
    pub provider: String,
    pub fetched_ms: i64,
    pub valid_time_ms: i64,
    #[serde(default)]
    pub request_url: Option<String>,
    pub surface_temp_c: f64,
    /// None: standard pressure for the site.
    pub surface_pressure_pa: Option<f64>,
    pub wind_mps: f64,
    pub wind_from_deg: f64,
    pub gust_mps: f64,
    #[serde(default)]
    pub levels: Vec<Level>,
    pub site: Site,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ManualWeather {
    pub surface_temp_c: f64,
    pub surface_pressure_hpa: Option<f64>,
    pub wind_mps: f64,
    pub wind_from_deg: f64,
    pub gust_mps: f64,
    pub wind_1km_mps: Option<f64>,
    pub wind_1km_from_deg: Option<f64>,
    pub wind_3km_mps: Option<f64>,
    pub wind_3km_from_deg: Option<f64>,
}

impl Default for ManualWeather {
    fn default() -> Self {
        Self {
            surface_temp_c: 20.0,
            surface_pressure_hpa: None,
            wind_mps: 4.0,
            wind_from_deg: 300.0,
            gust_mps: 1.0,
            wind_1km_mps: None,
            wind_1km_from_deg: None,
            wind_3km_mps: None,
            wind_3km_from_deg: None,
        }
    }
}

impl ManualWeather {
    pub fn validate(&self) -> Result<(), String> {
        let finite = |x: f64| x.is_finite();
        if !(-40.0..=60.0).contains(&self.surface_temp_c) {
            return Err("Surface temperature must be within -40..60 °C.".into());
        }
        if let Some(p) = self.surface_pressure_hpa {
            if !(500.0..=1100.0).contains(&p) {
                return Err("Surface pressure must be within 500..1100 hPa.".into());
            }
        }
        for (name, v) in [("Wind", Some(self.wind_mps)), ("Gust", Some(self.gust_mps)), ("1 km wind", self.wind_1km_mps), ("3 km wind", self.wind_3km_mps)] {
            if let Some(v) = v {
                if !finite(v) || !(0.0..=60.0).contains(&v) {
                    return Err(format!("{name} must be within 0..60 m/s."));
                }
            }
        }
        for d in [Some(self.wind_from_deg), self.wind_1km_from_deg, self.wind_3km_from_deg].into_iter().flatten() {
            if !finite(d) || !(0.0..=360.0).contains(&d) {
                return Err("Wind directions must be within 0..360°.".into());
            }
        }
        Ok(())
    }

    pub fn snapshot(&self, site: Site, now_ms: i64, valid_ms: i64) -> Snapshot {
        let mut levels = Vec::new();
        for (h, s, d) in [(1000.0, self.wind_1km_mps, self.wind_1km_from_deg), (3000.0, self.wind_3km_mps, self.wind_3km_from_deg)] {
            if let Some(s) = s {
                levels.push(Level { h_agl_m: h, speed_mps: s, from_deg: d.unwrap_or(self.wind_from_deg), temp_c: None });
            }
        }
        Snapshot {
            provider: "manual".into(),
            fetched_ms: now_ms,
            valid_time_ms: valid_ms,
            request_url: None,
            surface_temp_c: self.surface_temp_c,
            surface_pressure_pa: Some(
                self.surface_pressure_hpa
                    .map(|p| p * 100.0)
                    .unwrap_or_else(|| super::flight::atmosphere(site.alt_msl_m, 0.0, 101325.0).1),
            ),
            wind_mps: self.wind_mps,
            wind_from_deg: self.wind_from_deg,
            gust_mps: self.gust_mps,
            levels,
            site,
        }
    }
}

/// Coordinates rounded to 0.01° (~1 km): enough for a forecast grid cell.
pub fn rounded(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The exact request the station sends (shown to the operator before opt-in).
pub fn open_meteo_url(site: Site) -> String {
    let mut vars = vec![
        "temperature_2m".to_string(),
        "surface_pressure".into(),
        "wind_speed_10m".into(),
        "wind_direction_10m".into(),
        "wind_gusts_10m".into(),
    ];
    for l in PRESSURE_LEVELS {
        for v in ["temperature", "wind_speed", "wind_direction", "geopotential_height"] {
            vars.push(format!("{v}_{l}hPa"));
        }
    }
    format!(
        "{OPEN_METEO_URL}?latitude={:.2}&longitude={:.2}&hourly={}&wind_speed_unit=ms&timezone=GMT&forecast_days=3",
        rounded(site.lat_deg),
        rounded(site.lon_deg),
        vars.join(",")
    )
}

fn parse_time_ms(text: &str) -> Option<i64> {
    // "2026-11-22T13:00" (GMT)
    let fmt = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]");
    let dt = time::PrimitiveDateTime::parse(text, &fmt).ok()?;
    Some((dt.assume_utc().unix_timestamp_nanos() / 1_000_000) as i64)
}

/// The Open-Meteo hour nearest `valid_ms`, as a snapshot for `site`.
pub fn parse_open_meteo(body: &str, site: Site, valid_ms: i64, fetched_ms: i64, url: &str) -> Result<Snapshot, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("Forecast is not JSON: {e}"))?;
    if v.get("error").and_then(|e| e.as_bool()) == Some(true) {
        return Err(format!("Forecast service error: {}", v.get("reason").and_then(|r| r.as_str()).unwrap_or("unknown")));
    }
    let hourly = v.get("hourly").ok_or("Forecast has no hourly data.")?;
    let times: Vec<i64> = hourly
        .get("time")
        .and_then(|t| t.as_array())
        .ok_or("Forecast has no times.")?
        .iter()
        .filter_map(|t| t.as_str().and_then(parse_time_ms))
        .collect();
    if times.is_empty() {
        return Err("Forecast has no usable times.".into());
    }
    let idx = times
        .iter()
        .enumerate()
        .min_by_key(|(_, t)| (**t - valid_ms).abs())
        .map(|(i, _)| i)
        .unwrap_or(0);
    let series = |name: &str| -> Option<f64> { hourly.get(name)?.as_array()?.get(idx)?.as_f64() };
    let temp = series("temperature_2m").ok_or("Forecast has no surface temperature.")?;
    let wind = series("wind_speed_10m").ok_or("Forecast has no surface wind.")?;
    let dir = series("wind_direction_10m").unwrap_or(0.0);
    let gust = series("wind_gusts_10m").unwrap_or(wind);
    let pressure = series("surface_pressure").map(|hpa| hpa * 100.0);
    let mut levels = Vec::new();
    for l in PRESSURE_LEVELS {
        let (Some(z), Some(s), Some(d)) = (
            series(&format!("geopotential_height_{l}hPa")),
            series(&format!("wind_speed_{l}hPa")),
            series(&format!("wind_direction_{l}hPa")),
        ) else {
            continue;
        };
        let h = z - site.alt_msl_m;
        // Levels below (or within 50 m of) the ground are extrapolated: skip them.
        if h < 50.0 {
            continue;
        }
        levels.push(Level { h_agl_m: h, speed_mps: s, from_deg: d, temp_c: series(&format!("temperature_{l}hPa")) });
    }
    levels.sort_by(|a, b| a.h_agl_m.total_cmp(&b.h_agl_m));
    Ok(Snapshot {
        provider: "open_meteo".into(),
        fetched_ms,
        valid_time_ms: times[idx],
        request_url: Some(url.to_string()),
        surface_temp_c: temp,
        surface_pressure_pa: pressure,
        wind_mps: wind,
        wind_from_deg: dir,
        gust_mps: (gust - wind).max(0.0),
        levels,
        site,
    })
}

/// Fetches the forecast (only called when the administrator has enabled it).
pub async fn fetch_open_meteo(site: Site, valid_ms: i64, now_ms: i64) -> Result<Snapshot, String> {
    let url = open_meteo_url(site);
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(&url).send().await.map_err(|e| format!("Forecast request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| format!("Forecast read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("Forecast service returned {status}"));
    }
    parse_open_meteo(&body, site, valid_ms, now_ms, &url)
}

impl Snapshot {
    /// Flight-model environment: atmosphere shifted to the observed surface,
    /// wind profile through the levels.
    pub fn environment(&self, shear_exp: f64, cap: f64) -> Environment {
        let (dt, p_sl) = atmosphere_for_surface(self.site.alt_msl_m, self.surface_temp_c, self.surface_pressure_pa);
        let mut wind = WindProfile::power_law(self.wind_mps, self.wind_from_deg, shear_exp, cap);
        wind.levels = self
            .levels
            .iter()
            .map(|l| {
                let (e, n) = wind_vector(l.speed_mps, l.from_deg);
                (l.h_agl_m, e, n)
            })
            .collect();
        Environment { site_msl_m: self.site.alt_msl_m, site_lat_deg: self.site.lat_deg, temp_offset_k: dt, p_sl_pa: p_sl, wind }
    }
    /// 1σ temperature (°C) and wind (m/s) uncertainty: larger for manual
    /// entry, growing with forecast lead time.
    pub fn sigmas(&self, launch_ms: i64) -> (f64, f64) {
        let lead_h = ((launch_ms - self.fetched_ms).max(0) as f64) / 3.6e6;
        let (t, w) = if self.provider == "manual" { (3.0, 2.0) } else { (1.5 + 0.2 * lead_h, 1.5 + 0.25 * lead_h) };
        (t.min(8.0), (w + self.gust_mps).min(10.0))
    }
}
