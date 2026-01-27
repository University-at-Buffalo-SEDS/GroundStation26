// frontend/src/telemetry_dashboard/chart.rs
//
// Dioxus LineChart with:
//  - Optional time-window trimming (keeps graph near “now”)
//  - Timestamp-based time bucketing downsample (better than stride)
//  - Adaptive feedback loop:
//      * measure build cost
//      * if slow -> reduce bucket count
//      * if fast -> increase bucket count
//
// This version avoids web_sys Window::performance() so it compiles
// without enabling extra assets-sys features.

use dioxus::prelude::*;

/// Monotonic-enough “now” for feedback timing.
/// In wasm this is wall-clock-ish, but fine for relative ms thresholds.
fn _now_ms() -> f64 {
    js_sys::Date::now()
}

/// Build a single-series polyline with optional sliding window trimming.
pub fn _build_time_polyline(
    points: &[(i64, f64)],
    width: f64,
    height: f64,
    window_ms: Option<i64>,
) -> (String, f64, f64, f64) {
    if points.len() < 2 {
        return (String::new(), 0.0, 0.0, 0.0);
    }

    let mut pts: Vec<(i64, f64)> = points.to_vec();
    pts.sort_by_key(|(t, _)| *t);

    if let Some(win) = window_ms
        && let Some(&(newest, _)) = pts.last()
    {
        let start = newest.saturating_sub(win);
        let first_in = pts.partition_point(|(t, _)| *t < start);
        if first_in > 0 {
            pts.drain(0..first_in);
        }
    }

    if pts.len() < 2 {
        return (String::new(), 0.0, 0.0, 0.0);
    }

    let (t_min, t_max) = pts
        .iter()
        .fold((i64::MAX, i64::MIN), |(mn, mx), (t, _)| {
            (mn.min(*t), mx.max(*t))
        });
    let (y_min, y_max) = pts
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(mn, mx), (_, y)| {
            (mn.min(*y), mx.max(*y))
        });

    let t_span = (t_max - t_min).max(1) as f64;
    let mut y_span = y_max - y_min;
    if !y_span.is_finite() || y_span.abs() < 1e-9 {
        y_span = 1.0;
    }

    let pad_l = 60.0;
    let pad_r = 20.0;
    let pad_t = 20.0;
    let pad_b = 20.0;
    let inner_w = width - pad_l - pad_r;
    let inner_h = height - pad_t - pad_b;

    let to_xy = |t: i64, y: f64| -> (f64, f64) {
        let x = pad_l + ((t - t_min) as f64 / t_span) * inner_w;
        let y_norm = (y - y_min) / y_span;
        let y_px = pad_t + (1.0 - y_norm) * inner_h;
        (x, y_px)
    };

    let mut poly = String::new();
    for (i, (t, y)) in pts.iter().enumerate() {
        let (x, yy) = to_xy(*t, *y);
        if i == 0 {
            poly.push_str(&format!("{x:.2},{yy:.2}"));
        } else {
            poly.push_str(&format!(" {x:.2},{yy:.2}"));
        }
    }

    let span_min = t_span / 60_000.0;
    (poly, y_min, y_max, span_min)
}

/// Simple SVG line chart (polyline) for timeseries.
/// `points`: Vec of (t_ms, y)
#[component]
pub fn LineChart(points: Vec<(i64, f64)>, height: i32, title: String) -> Element {
    // ---- Rendering knobs ----
    // Show only the last N ms if set. This is the key “near real-time” knob.
    const WINDOW_MS: Option<i64> = Some(20 * 60_000); // 20 minutes

    // Feedback targets (milliseconds spent building polyline & ranges)
    const TARGET_FAST_MS: f64 = 2.0;
    const TARGET_SLOW_MS: f64 = 8.0;

    // Absolute caps on detail
    const MIN_BUCKETS: usize = 200;
    const MAX_BUCKETS: usize = 6000;

    if points.len() < 2 {
        return rsx! {
            div { style: "padding:12px; border:1px solid #334155; border-radius:12px; background:#0b1220;",
                div { style:"color:#94a3b8; font-size:12px; margin-bottom:8px;", "{title}" }
                div { style:"color:#64748b; font-size:12px;", "Not enough data yet" }
            }
        };
    }

    // Per-chart adaptive state: fast systems drift up, slow systems drift down.
    let mut adaptive_buckets = use_signal(|| 1200usize);

    let width = 900.0_f64; // SVG viewbox width (scales with CSS)
    let h = height.max(120) as f64;

    // ---- Begin timed build section ----
    let t0 = _now_ms();

    // 1) Sort by time and (optionally) window it
    let mut pts = points;
    pts.sort_by_key(|(t, _)| *t);

    if let Some(win) = WINDOW_MS
        && let Some(&(newest, _)) = pts.last()
    {
        let start = newest.saturating_sub(win);
        let first_in = pts.partition_point(|(t, _)| *t < start);
        pts.drain(0..first_in);
    }

    if pts.len() < 2 {
        return rsx! {
            div { style: "padding:12px; border:1px solid #334155; border-radius:12px; background:#0b1220;",
                div { style:"color:#94a3b8; font-size:12px; margin-bottom:8px;", "{title}" }
                div { style:"color:#64748b; font-size:12px;", "Not enough data yet" }
            }
        };
    }

    // 2) Baseline bucket count depends on timestamps + span + width
    let baseline = _choose_bucket_count(&pts, width).clamp(MIN_BUCKETS, MAX_BUCKETS);

    // 3) Combine baseline with machine-speed buckets
    let cur_adaptive = *adaptive_buckets.read();
    let use_buckets = baseline.min(cur_adaptive).clamp(MIN_BUCKETS, MAX_BUCKETS);

    // 4) Timestamp-based bucketing
    let pts_ds = _bucket_by_time(&pts, use_buckets);

    // 5) Compute ranges on downsampled points
    let (t_min, t_max) = pts_ds
        .iter()
        .fold((i64::MAX, i64::MIN), |(mn, mx), (t, _)| {
            (mn.min(*t), mx.max(*t))
        });
    let (y_min, y_max) = pts_ds
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(mn, mx), (_, y)| {
            (mn.min(*y), mx.max(*y))
        });

    let t_span = (t_max - t_min).max(1) as f64;
    let mut y_span = y_max - y_min;
    if !y_span.is_finite() || y_span.abs() < 1e-9 {
        y_span = 1.0;
    }

    // padding inside chart area
    let pad_l = 40.0;
    let pad_r = 10.0;
    let pad_t = 10.0;
    let pad_b = 24.0;

    let inner_w = width - pad_l - pad_r;
    let inner_h = h - pad_t - pad_b;

    let to_xy = |t: i64, y: f64| -> (f64, f64) {
        let x = pad_l + ((t - t_min) as f64 / t_span) * inner_w;
        let y_norm = (y - y_min) / y_span;
        let y_px = pad_t + (1.0 - y_norm) * inner_h;
        (x, y_px)
    };

    // 6) Build polyline points string
    let mut poly = String::new();
    for (i, (t, y)) in pts_ds.iter().enumerate() {
        let (x, yy) = to_xy(*t, *y);
        if i == 0 {
            poly.push_str(&format!("{x:.2},{yy:.2}"));
        } else {
            poly.push_str(&format!(" {x:.2},{yy:.2}"));
        }
    }

    // ---- End timed build section ----
    let dt = _now_ms() - t0;

    // 7) Feedback loop: adjust adaptive buckets for next render
    let next = if dt > TARGET_SLOW_MS {
        // too slow -> reduce by 25%
        (cur_adaptive.saturating_mul(3) / 4).max(MIN_BUCKETS)
    } else if dt < TARGET_FAST_MS {
        // very fast -> increase by 25%
        (cur_adaptive.saturating_mul(5) / 4).min(MAX_BUCKETS)
    } else {
        cur_adaptive
    };
    if next != cur_adaptive {
        adaptive_buckets.set(next);
    }

    let span_ms = (t_max - t_min).max(0) as f64;
    let span_min = span_ms / 60_000.0;

    rsx! {
        div { style: "padding:12px; border:1px solid #334155; border-radius:12px; background:#0b1220;",
            div { style:"display:flex; align-items:center; justify-content:space-between; margin-bottom:8px;",
                div { style:"color:#94a3b8; font-size:12px;", "{title}" }
                div { style:"color:#64748b; font-size:12px;",
                    {format!(
                        "min={:.3} max={:.3} span={:.1}m buckets={} build={:.1}ms",
                        y_min, y_max, span_min, use_buckets, dt
                    )}
                }
            }

            svg {
                style: "width:100%; height:auto; display:block; background:#020617; border-radius:10px; border:1px solid #1f2937;",
                view_box: "0 0 {width} {h}",

                // axes baseline (subtle)
                line { x1:"40", y1:"{h - 24.0}", x2:"{width - 10.0}", y2:"{h - 24.0}",
                    stroke:"#334155", "stroke-width":"1"
                }
                line { x1:"40", y1:"10", x2:"40", y2:"{h - 24.0}",
                    stroke:"#334155", "stroke-width":"1"
                }

                polyline {
                    points: "{poly}",
                    fill: "none",
                    stroke: "#38bdf8",
                    "stroke-width": "2",
                    "stroke-linejoin": "round",
                    "stroke-linecap": "round",
                }
            }
        }
    }
}

/// Baseline bucket count based on time span + inferred sample rate + width.
/// This makes “powerful system shows more points” possible when combined
/// with the feedback loop (adaptive_buckets).
fn _choose_bucket_count(points: &[(i64, f64)], width_px: f64) -> usize {
    if points.len() < 2 {
        return 200;
    }

    let t_min = points.first().unwrap().0;
    let t_max = points.last().unwrap().0;
    let span_ms = (t_max - t_min).max(1) as f64;
    let span_s = span_ms / 1000.0;

    // samples/sec derived from timestamps
    let hz = (points.len().saturating_sub(1) as f64) / span_s.max(1e-6);

    // Don’t chase absurd detail
    let target_hz_cap = 120.0;
    let effective_hz = hz.min(target_hz_cap);

    // buckets ~ seconds * hz
    let mut buckets = (span_s * effective_hz).round() as usize;
    if buckets < 200 {
        buckets = 200;
    }

    // Also cap by width (2 points per px)
    let width_cap = (width_px as usize).saturating_mul(2).clamp(400, 6000);

    buckets.min(width_cap)
}

/// Timestamp-based time bucketing over [t_min, t_max].
/// Each bucket yields (avg_t, avg_y). Ensures newest point is included.
fn _bucket_by_time(points: &[(i64, f64)], bucket_count: usize) -> Vec<(i64, f64)> {
    if points.len() <= 2 || bucket_count < 2 {
        return points.to_vec();
    }

    let t_min = points.first().unwrap().0;
    let t_max = points.last().unwrap().0;
    let span = (t_max - t_min).max(1);

    #[derive(Clone, Default)]
    struct Acc {
        t_sum: i128,
        t_n: i64,
        y_sum: f64,
        y_n: i64,
    }

    let mut buckets = vec![Acc::default(); bucket_count];

    for &(t, y) in points.iter() {
        if !y.is_finite() {
            continue;
        }
        let rel = (t - t_min) as i128;
        let bi = ((rel * bucket_count as i128) / span as i128).clamp(0, bucket_count as i128 - 1)
            as usize;

        let b = &mut buckets[bi];
        b.t_sum += t as i128;
        b.t_n += 1;
        b.y_sum += y;
        b.y_n += 1;
    }

    let mut out = Vec::with_capacity(bucket_count);

    for b in buckets.into_iter() {
        if b.t_n > 0 && b.y_n > 0 {
            let t_avg = (b.t_sum / b.t_n as i128) as i64;
            let y_avg = b.y_sum / b.y_n as f64;
            out.push((t_avg, y_avg));
        }
    }

    out.sort_by_key(|(t, _)| *t);

    // Ensure newest point is present (helps “near now” feel)
    if let Some(&(t_last, y_last)) = points.last()
        && out.last().map(|p| p.0) != Some(t_last)
    {
        out.push((t_last, y_last));
    }
    out
}
