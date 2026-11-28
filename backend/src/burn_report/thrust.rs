//! Thrust from the stand load cells, including the KG50 unloading
//! (docs/apogee-planner.md section 7; the Python reference is
//! `tools/flight_profile/extract_thrust.py`).
//!
//! The rocket hangs from the 50 kg fill cell (KG50). Thrust first unloads
//! KG50; once it exceeds the hanging weight the rocket lifts, closes the gap
//! and presses the 1000 kg thrust cell (KG1000). With `R(t)` the KG50
//! reading, `R_pre` its pre-ignition value, `R_post` its settled post-burn
//! value and `F(t)` the KG1000 reading (kg):
//!
//! ```text
//! thrust(t) = g * (F(t) + (R_pre - R(t)) - burned(t)),  burned(t) = (R_pre - R_post) * impulse fraction(t)
//! ```
//!
//! Only KG50 differences enter (its tare does not matter, its slope does).
//! Times are seconds from the ignition reference (main valve open).
use serde::{Deserialize, Serialize};

pub(crate) const G: f64 = 9.80665;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Flag {
    pub code: String,
    /// `info`, `warn` or `bad`.
    pub severity: String,
    pub message: String,
}

impl Flag {
    pub(crate) fn new(code: &str, severity: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity: severity.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    /// Grid step of the binned, smoothed curve (s).
    pub dt: f64,
    /// KG1000 readings outside this range are rails/clamps/open lead (kg).
    pub kg1000_valid: (f64, f64),
    /// Valid samples this close to a railed one are not trusted (s).
    pub rail_neighbour_s: f64,
    /// Assumed ignition delay after the reference and the startup ramp used
    /// to bridge a KG1000 that is not readable at the start (s).
    pub ignition_delay_s: f64,
    pub startup_ramp_s: f64,
    pub pre_window: (f64, f64),
    pub post_window: (f64, f64),
    pub kg1000_baseline_window: (f64, f64),
    /// End of the analysed window (s).
    pub end_s: f64,
    /// Scale on the KG50 term (1 = the recorded KG50 calibration).
    pub kg50_scale: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dt: 0.05,
            kg1000_valid: (-50.0, 600.0),
            rail_neighbour_s: 0.06,
            ignition_delay_s: 0.10,
            startup_ramp_s: 0.20,
            pre_window: (-3.0, -0.2),
            post_window: (12.0, 20.0),
            kg1000_baseline_window: (7.0, 11.0),
            end_s: 12.0,
            kg50_scale: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thrust {
    /// (t s, thrust N), starting and ending at zero.
    pub curve: Vec<(f64, f64)>,
    pub total_impulse_ns: f64,
    pub peak_thrust_n: f64,
    pub t_peak_s: f64,
    /// Time above 5 % of peak.
    pub burn_time_s: f64,
    pub burn_end_s: f64,
    pub average_thrust_n: f64,
    /// Impulse of the KG1000 term alone, of the part of it that bridges an
    /// unreadable start, and of the KG50 unloading term.
    pub kg1000_impulse_ns: f64,
    pub startup_fill_impulse_ns: f64,
    pub kg50_term_ns: f64,
    pub impulse_sigma_ns: f64,
    pub kg50_pre_kg: f64,
    pub kg50_post_kg: f64,
    pub kg50_plateau_kg: f64,
    /// KG50 reached a flat zero-load plateau (the rocket lifted off it).
    pub lifted: bool,
    pub lift_off_s: Option<f64>,
    pub drop_back_s: Option<f64>,
    pub flags: Vec<Flag>,
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.total_cmp(b));
    let n = xs.len();
    Some(if n % 2 == 1 {
        xs[n / 2]
    } else {
        0.5 * (xs[n / 2 - 1] + xs[n / 2])
    })
}

fn in_window(series: &[(f64, f64)], w: (f64, f64)) -> Vec<f64> {
    series
        .iter()
        .filter(|(t, _)| *t >= w.0 && *t <= w.1)
        .map(|(_, v)| *v)
        .collect()
}

fn rolling_median(series: &[(f64, f64)], n: usize) -> Vec<(f64, f64)> {
    let h = n / 2;
    (0..series.len())
        .map(|i| {
            let lo = i.saturating_sub(h);
            let hi = (i + h + 1).min(series.len());
            (
                series[i].0,
                median(series[lo..hi].iter().map(|p| p.1).collect()).unwrap_or(series[i].1),
            )
        })
        .collect()
}

fn bins(series: &[(f64, f64)], dt: f64, w: (f64, f64)) -> Vec<(f64, f64)> {
    let mut map = std::collections::BTreeMap::<i64, (f64, usize)>::new();
    for (t, v) in series.iter().filter(|(t, _)| *t >= w.0 && *t <= w.1) {
        let e = map.entry((t / dt).round() as i64).or_insert((0.0, 0));
        e.0 += v;
        e.1 += 1;
    }
    map.into_iter()
        .map(|(k, (s, n))| (k as f64 * dt, s / n as f64))
        .collect()
}

fn interp(pts: &[(f64, f64)], t: f64) -> f64 {
    if pts.is_empty() {
        return 0.0;
    }
    if t <= pts[0].0 {
        return pts[0].1;
    }
    for w in pts.windows(2) {
        let ((a, va), (b, vb)) = (w[0], w[1]);
        if a <= t && t <= b {
            return if b > a {
                va + (vb - va) * (t - a) / (b - a)
            } else {
                va
            };
        }
    }
    pts[pts.len() - 1].1
}

fn moving_average(y: &[f64], n: usize) -> Vec<f64> {
    let h = n / 2;
    (0..y.len())
        .map(|i| {
            let lo = i.saturating_sub(h);
            let hi = (i + h + 1).min(y.len());
            y[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
        })
        .collect()
}

fn trapz(curve: &[(f64, f64)]) -> f64 {
    curve
        .windows(2)
        .map(|w| (w[1].0 - w[0].0) * 0.5 * (w[0].1 + w[1].1))
        .sum()
}

fn max_gap(series: &[(f64, f64)], w: (f64, f64)) -> f64 {
    let ts: Vec<f64> = series
        .iter()
        .map(|p| p.0)
        .filter(|t| *t >= w.0 && *t <= w.1)
        .collect();
    let mut gap = if ts.is_empty() {
        w.1 - w.0
    } else {
        (ts[0] - w.0).max(w.1 - ts[ts.len() - 1])
    };
    for p in ts.windows(2) {
        gap = gap.max(p[1] - p[0]);
    }
    gap
}

/// KG1000 samples marked trustworthy: inside the valid range, not next to a
/// rail (open lead / supply-dip clamp), not an isolated outlier.
fn kg1000_good(raw: &[(f64, f64)], cfg: &Config) -> (Vec<(f64, f64)>, usize, usize) {
    let (lo, hi) = cfg.kg1000_valid;
    let railed: Vec<f64> = raw
        .iter()
        .filter(|(_, v)| !(*v > lo && *v < hi))
        .map(|p| p.0)
        .collect();
    let valid: Vec<(f64, f64)> = raw
        .iter()
        .copied()
        .filter(|(_, v)| *v > lo && *v < hi)
        .collect();
    let near_rail = |t: f64| railed.iter().any(|r| (r - t).abs() <= cfg.rail_neighbour_s);
    let medians = rolling_median(&valid, 7);
    let mut suspect = 0;
    let good = valid
        .iter()
        .zip(&medians)
        .filter(|((t, v), (_, m))| {
            let bad = near_rail(*t) || (v - m).abs() > 40f64.max(0.35 * m.abs());
            suspect += usize::from(bad);
            !bad
        })
        .map(|(p, _)| *p)
        .collect();
    (good, railed.len(), suspect)
}

/// KG50's floor during the burn and how long it sat flat there: the mode of
/// the readings below the pre- and post-burn levels (1 kg bins) when it is held within ±1 kg for at least
/// half a second without a break and lies at the bottom of the readings,
/// else the 2nd percentile (no flat floor).
#[cfg(test)]
pub(crate) fn kg50_floor_for_test(series: &[(f64, f64)], pre: f64, post: f64) -> (f64, f64) {
    kg50_floor(series, pre, post)
}

fn kg50_floor(series: &[(f64, f64)], pre: f64, post: f64) -> (f64, f64) {
    // Below both the loaded and the burnt-out hanging levels.
    let below = pre.min(post) - 3.0;
    let low: Vec<f64> = series
        .iter()
        .filter(|(t, v)| *t >= 0.0 && *v < below)
        .map(|p| p.1)
        .collect();
    if low.is_empty() {
        return (pre, 0.0);
    }
    let mut counts = std::collections::BTreeMap::<i64, usize>::new();
    for v in &low {
        *counts.entry(v.floor() as i64).or_default() += 1;
    }
    let mode = counts
        .iter()
        .max_by_key(|(k, n)| (**n, -**k))
        .map(|(k, _)| *k)
        .unwrap_or(0);
    let centre = mode as f64 + 0.5;
    let level = median(
        low.iter()
            .copied()
            .filter(|v| (v - centre).abs() <= 1.5)
            .collect(),
    )
    .unwrap_or(centre);
    let (mut run, mut best, mut start) = (false, 0.0f64, 0.0);
    for (t, v) in series.iter().filter(|(t, _)| *t >= 0.0) {
        let inside = (v - level).abs() <= 1.0;
        if inside && !run {
            start = *t;
        }
        if inside {
            best = best.max(t - start);
        }
        run = inside;
    }
    let mut sorted = low;
    sorted.sort_by(|a, b| a.total_cmp(b));
    let p = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
    if best >= 0.5 && level <= p(0.10) + 2.0 {
        (level, best)
    } else {
        (p(0.02), 0.0)
    }
}

/// The thrust curve and its statistics from the two load cells.
pub(crate) fn extract(
    kg1000: &[(f64, f64)],
    kg50: &[(f64, f64)],
    cfg: &Config,
) -> Result<Thrust, String> {
    let dt = cfg.dt;
    let n = (cfg.end_s / dt).round() as usize;
    let grid: Vec<f64> = (0..=n)
        .map(|i| (i as f64 * dt * 1000.0).round() / 1000.0)
        .collect();
    let mut flags = Vec::new();

    // ---- KG50
    let pre = median(in_window(kg50, cfg.pre_window)).ok_or("no KG50 samples before ignition")?;
    let post = median(in_window(kg50, cfg.post_window))
        .or_else(|| {
            let last = kg50.last()?.0;
            median(in_window(kg50, (last - 2.0, last)))
        })
        .ok_or("no KG50 samples after the burn")?;
    let k50_window: Vec<(f64, f64)> = kg50
        .iter()
        .copied()
        .filter(|(t, _)| *t >= -1.0 && *t <= cfg.end_s)
        .collect();
    // The floor KG50 settles on: a flat zero-load plateau when the rocket
    // lifts (the most common low level), else the robust minimum.
    let (floor, flat_s) = kg50_floor(&rolling_median(&k50_window, 9), pre, post);
    // Samples outside the physical range are supply-dip/ground spikes.
    let k50_clean: Vec<(f64, f64)> = k50_window
        .iter()
        .copied()
        .filter(|(_, v)| *v >= floor - 5.0 && *v <= pre + 5.0)
        .collect();
    let k50_med = rolling_median(&k50_clean, 5);
    let plateau = floor.min(pre);
    let k50_bins = bins(&k50_med, dt, (-1.0, cfg.end_s));
    let r: Vec<f64> = grid
        .iter()
        .map(|t| interp(&k50_bins, *t).clamp(plateau, pre))
        .collect();

    // ---- KG1000
    let k1k_window: Vec<(f64, f64)> = kg1000
        .iter()
        .copied()
        .filter(|(t, _)| *t >= -1.0 && *t <= cfg.end_s)
        .collect();
    let (good, railed, suspect) = kg1000_good(&k1k_window, cfg);
    let baseline = median(in_window(&good, cfg.kg1000_baseline_window))
        .or_else(|| {
            let last = good.last()?.0;
            median(in_window(&good, (last - 2.0, last)))
        })
        .unwrap_or(0.0);
    let good: Vec<(f64, f64)> = good.into_iter().map(|(t, v)| (t, v - baseline)).collect();
    let smoothed_raw = rolling_median(&good, 5);
    let t_first = smoothed_raw.iter().find(|(t, _)| *t >= 0.0).map(|p| p.0);
    let mut f = vec![0.0; grid.len()];
    let mut fill_until = None;
    if let Some(t_first) = t_first {
        let from = t_first.max(0.0);
        let meas = bins(&smoothed_raw, dt, (from, cfg.end_s));
        let start = grid
            .iter()
            .position(|t| *t >= from - 1e-9)
            .unwrap_or(grid.len());
        let y: Vec<f64> = grid[start..].iter().map(|t| interp(&meas, *t)).collect();
        let ys = moving_average(&moving_average(&y, 3), 3);
        for (i, v) in ys.into_iter().enumerate() {
            f[start + i] = v;
        }
        // KG1000 unreadable at the start: bridge with a ramp to its first good level.
        if from > cfg.ignition_delay_s + 0.05 {
            let first = f[start.min(f.len() - 1)];
            for (i, t) in grid.iter().enumerate().take(start) {
                f[i] = if *t <= cfg.ignition_delay_s {
                    0.0
                } else {
                    first * ((t - cfg.ignition_delay_s) / cfg.startup_ramp_s).min(1.0)
                };
            }
            fill_until = Some(from);
        }
        // After its peak, the thrust cell is done once it is back at its baseline.
        let peak_i = (0..f.len())
            .max_by(|a, b| f[*a].total_cmp(&f[*b]))
            .unwrap_or(0);
        if let Some(end) = (peak_i..f.len()).find(|i| f[*i] < 0.5) {
            for v in f.iter_mut().skip(end) {
                *v = 0.0;
            }
        }
        for v in &mut f {
            *v = v.max(0.0);
        }
    }

    // The rocket left KG50 if it held a flat floor while KG1000 took load.
    let contact = good
        .iter()
        .find(|(t, v)| *t >= 0.0 && *v > 5.0)
        .map(|p| p.0);
    let flat = pre - plateau > 3.0 && flat_s >= 0.5;
    let lifted = flat && contact.is_some();
    let lift_off_s = lifted
        .then(|| {
            k50_med
                .iter()
                .find(|(t, v)| *t >= 0.0 && *v <= plateau + 2.0)
                .map(|p| p.0)
        })
        .flatten();

    // ---- combine
    let s = cfg.kg50_scale;
    let total_burned = s * (pre - post);
    let mut frac = vec![0.0; grid.len()];
    let mut thrust = vec![0.0; grid.len()];
    for _ in 0..4 {
        for i in 0..grid.len() {
            thrust[i] = (G * (f[i] + s * (pre - r[i]) - total_burned * frac[i])).max(0.0);
        }
        let mut cum = vec![0.0; grid.len()];
        for i in 1..grid.len() {
            cum[i] = cum[i - 1] + 0.5 * (thrust[i] + thrust[i - 1]) * dt;
        }
        let total = cum[cum.len() - 1];
        if total <= 0.0 {
            return Err("no thrust in the window".into());
        }
        frac = cum.iter().map(|c| c / total).collect();
    }
    let peak = thrust.iter().copied().fold(0.0, f64::max);
    let last = (0..thrust.len())
        .rev()
        .find(|i| thrust[*i] >= 0.01 * peak)
        .unwrap_or(0);
    let mut curve: Vec<(f64, f64)> = vec![(0.0, 0.0)];
    curve.extend(
        grid.iter()
            .zip(&thrust)
            .take(last + 1)
            .filter(|(t, _)| **t > 0.0)
            .map(|(t, v)| (*t, *v)),
    );
    let end_t = ((grid[last] + dt) * 1000.0).round() / 1000.0;
    curve.push((end_t, 0.0));

    let total = trapz(&curve);
    let (t_peak, peak) = curve
        .iter()
        .copied()
        .fold((0.0, 0.0), |a, p| if p.1 > a.1 { p } else { a });
    let above: Vec<f64> = curve
        .iter()
        .filter(|p| p.1 >= 0.05 * peak)
        .map(|p| p.0)
        .collect();
    let burn = above.last().copied().unwrap_or(0.0) - above.first().copied().unwrap_or(0.0);
    let f_curve: Vec<(f64, f64)> = grid.iter().zip(&f).map(|(t, v)| (*t, G * v)).collect();
    let kg1000_imp = trapz(&f_curve);
    let fill_imp = fill_until
        .map(|until| {
            trapz(
                &f_curve
                    .iter()
                    .copied()
                    .filter(|p| p.0 <= until)
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or(0.0);
    let kg50_term = total - kg1000_imp;
    let sigma = ((0.10 * (kg1000_imp - fill_imp)).powi(2)
        + (0.6 * fill_imp).powi(2)
        + (0.25 * kg50_term).powi(2))
    .sqrt();

    // ---- quality flags
    let burn_window = (-0.5, end_t);
    let rails_in_burn = k1k_window
        .iter()
        .filter(|(t, v)| {
            *t >= burn_window.0
                && *t <= burn_window.1
                && !(*v > cfg.kg1000_valid.0 && *v < cfg.kg1000_valid.1)
        })
        .count();
    if rails_in_burn > 0 {
        flags.push(Flag::new(
            "kg1000_clamp",
            if rails_in_burn > 20 { "bad" } else { "warn" },
            format!("{rails_in_burn} KG1000 samples at a rail/clamp during the burn (open lead or DAQ supply dip, docs/loadcell-investigation-2026-11.md); {suspect} more next to them or isolated outliers were dropped ({railed} rail samples in the whole window)"),
        ));
    }
    if let Some(until) = fill_until {
        flags.push(Flag::new(
            "kg1000_startup_gap",
            "warn",
            format!("KG1000 unreadable until {until:.2} s; bridged with an assumed ramp ({fill_imp:.0} N s, weighted 60 % in the uncertainty)"),
        ));
    }
    let spikes = kg50
        .iter()
        .filter(|(t, v)| *t >= 0.0 && *t <= end_t && (*v < plateau - 5.0 || *v > pre + 5.0))
        .count();
    if spikes > 0 {
        flags.push(Flag::new(
            "kg50_supply_dip_spikes",
            "warn",
            format!("{spikes} KG50 samples outside the physical range during the burn (supply-dip/ground signature); removed by the rolling median"),
        ));
    }
    let gap = max_gap(kg1000, (-0.5, end_t)).max(max_gap(kg50, (-0.5, end_t)));
    if gap > 0.25 {
        flags.push(Flag::new(
            "missing_samples",
            "warn",
            format!("largest load-cell sample gap in the burn {gap:.2} s"),
        ));
    }
    if let (Some(lift), Some(contact)) = (lift_off_s, contact) {
        if contact > lift + 0.02 {
            flags.push(Flag::new(
                "gap_closing_transient",
                "info",
                format!("KG50 unloaded at {lift:.2} s, KG1000 took load at {contact:.2} s: neither cell carried the rocket in between (net impulse about zero)"),
            ));
        }
    }
    let drop_back_s = if lifted {
        let k1k_end = contact.and_then(|c| {
            grid.iter()
                .zip(&f)
                .find(|(t, v)| **t > c + 0.2 && **v <= 0.0)
                .map(|(t, _)| *t)
        });
        let from = k1k_end.unwrap_or(0.0);
        k50_med
            .iter()
            .find(|(t, v)| *t >= from && *t > lift_off_s.unwrap_or(0.0) + 0.3 && *v > plateau + 2.0)
            .map(|p| p.0)
    } else {
        None
    };
    if let Some(drop) = drop_back_s {
        flags.push(Flag::new(
            "drop_back",
            "info",
            format!(
                "rocket back on KG50 from {drop:.2} s; the tail after that is seen only by KG50"
            ),
        ));
    }
    if !flat && pre - post > 0.0 {
        flags.push(Flag::new("not_lifted", "info", "KG50 never reached a zero-load plateau: thrust stayed below the hanging weight or KG50 was not unloaded"));
    }
    if flat && contact.is_none() {
        flags.push(Flag::new(
            "kg1000_no_load",
            "bad",
            "KG50 held a flat floor but KG1000 never took load: if the rocket lifted, thrust above the hanging weight is unknown (lower bound only)",
        ));
    }
    flags.push(Flag::new(
        "kg50_slope",
        "info",
        "the KG50 term scales with the fill-cell slope, which has not been verified with a known mass on the stand",
    ));
    let round1 = |x: f64| (x * 10.0).round() / 10.0;
    Ok(Thrust {
        curve: curve.iter().map(|(t, v)| (*t, round1(*v))).collect(),
        total_impulse_ns: round1(total),
        peak_thrust_n: round1(peak),
        t_peak_s: t_peak,
        burn_time_s: (burn * 100.0).round() / 100.0,
        burn_end_s: end_t,
        average_thrust_n: if burn > 0.0 {
            round1(total / burn)
        } else {
            0.0
        },
        kg1000_impulse_ns: round1(kg1000_imp),
        startup_fill_impulse_ns: round1(fill_imp),
        kg50_term_ns: round1(kg50_term),
        impulse_sigma_ns: round1(sigma),
        kg50_pre_kg: pre,
        kg50_post_kg: post,
        kg50_plateau_kg: plateau,
        lifted,
        lift_off_s,
        drop_back_s,
        flags,
    })
}
