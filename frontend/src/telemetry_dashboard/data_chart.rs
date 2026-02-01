// frontend/src/telemetry_dashboard/data_chart.rs
//
// High-performance chart cache (incremental ingest):
// - Keep a bounded, time-pruned ring of samples per data_type.
// - Rebuild downsampled buckets only when dirty.
// - bucket width is based on *effective span*, not HISTORY_MS.
//
// FIXES:
// - Sticky / hysteretic Y-axis so the scale does NOT jump when old spikes fall out of the window.
//   * Expand immediately (never clip)
//   * Shrink slowly (prevents “random” mid-run rescaling)
//   * Add a small padding around the range
//
// - IMPORTANT FIX (fullscreen / multiple sizes):
//   CachedChart used to keep only ONE `paths` buffer. If you call charts_cache_get()
//   with (w,h) = (1200,360) then later (1200,700) in the same frame, it would not rebuild
//   because `dirty=false`, causing fullscreen to reuse non-fullscreen geometry.
//   We now track last_w/last_h and rebuild when size changes.
//
// API:
//   charts_cache_reset_and_ingest(rows)
//   charts_cache_ingest_row(row)
//   charts_cache_get(data_type, width, height) -> ([d;8], y_min, y_max, span_min)

use groundstation_shared::TelemetryRow;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use super::HISTORY_MS;
// Time-window shrink tuning (only used during explicit "refit")
const X_SHRINK_ALPHA: f32 = 0.12; // 0.05..0.2, higher = faster settle
const X_SHRINK_EPS_MS: i64 = 250; // snap when within this many ms

pub fn charts_cache_request_refit() {
    CHARTS_CACHE.with(|c| c.borrow_mut().request_refit());
}

// ============================================================
// Global cache (updated once per telemetry row)
// ============================================================

thread_local! {
    static CHARTS_CACHE: RefCell<ChartsCache> = RefCell::new(ChartsCache::new());
}

pub fn _charts_cache_is_dirty(data_type: &str) -> bool {
    CHARTS_CACHE.with(|c| {
        let c = c.borrow();
        c.charts.get(data_type).map(|ch| ch.dirty).unwrap_or(false)
    })
}

pub fn charts_cache_reset_and_ingest(rows: &[TelemetryRow]) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.clear();
        for r in rows {
            c.ingest_row(r);
        }
    });
}

pub fn charts_cache_ingest_row(row: &TelemetryRow) {
    CHARTS_CACHE.with(|c| {
        c.borrow_mut().ingest_row(row);
    });
}

/// Returns:
/// - [String;8] where each String is an SVG path `d` (may be empty)
/// - y_min, y_max (for labels)
/// - span_min (minutes of effective history window)
pub fn charts_cache_get(data_type: &str, width: f32, height: f32) -> ([String; 8], f32, f32, f32) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.get(data_type, width, height)
    })
}

pub fn charts_cache_get_channel_minmax(
    data_type: &str,
    width: f32,
    height: f32,
) -> ([Option<f32>; 8], [Option<f32>; 8]) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(ch) = c.charts.get_mut(data_type) {
            ch.build_if_needed(width, height);
            (ch.chan_min, ch.chan_max)
        } else {
            ([None; 8], [None; 8])
        }
    })
}

// ============================================================
// Cache internals
// ============================================================

const MAX_POINTS_CAP: usize = 60000; // cap for downsample points (smooth scrolling at 20min)
const MIN_POINTS: usize = 1; // keep some detail even for tiny spans
const TARGET_BUCKET_MS: i64 = 20; // aim ~5Hz resolution at max span
const X_ALIGN_MS: i64 = 50; // snap window end to reduce x jitter
const MIN_SPAN_MS: i64 = 1_000; // avoid divide-by-zero (1s)

// Keep at most this many samples per datatype as a hard cap (in addition to time pruning).
const MAX_SAMPLES_PER_TYPE: usize = 20_000;

// -------------------------
// Sticky Y-axis tuning
// -------------------------
//
// Expand immediately (alpha=1.0) when the new range exceeds the displayed range.
// Shrink slowly to avoid “jumping” when old extremes fall out of the window.
const Y_SHRINK_ALPHA: f32 = 0.04; // ~ slow convergence; increase for faster shrink
const Y_PAD_FRAC: f32 = 0.06; // 6% padding around range
const Y_MIN_PAD_ABS: f32 = 1.0; // at least +/-1 unit of padding (helps small ranges)

struct ChartsCache {
    charts: HashMap<String, CachedChart>,
}

impl ChartsCache {
    fn new() -> Self {
        Self {
            charts: HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.charts.clear();
    }

    fn ingest_row(&mut self, r: &TelemetryRow) {
        let chart = self
            .charts
            .entry(r.data_type.clone())
            .or_insert_with(CachedChart::new);
        chart.ingest(r);
    }

    fn get(&mut self, dt: &str, w: f32, h: f32) -> ([String; 8], f32, f32, f32) {
        if let Some(c) = self.charts.get_mut(dt) {
            c.build_if_needed(w, h);
            (c.paths.clone(), c.disp_min, c.disp_max, c.span_min)
        } else {
            (std::array::from_fn(|_| String::new()), 0.0, 1.0, 0.0)
        }
    }
    fn request_refit(&mut self) {
        for ch in self.charts.values_mut() {
            ch.request_refit();
        }
    }
}

// A compact sample (store only what we need)
#[derive(Clone)]
struct Sample {
    ts: i64,
    v: [Option<f32>; 8],
}

impl From<&TelemetryRow> for Sample {
    fn from(r: &TelemetryRow) -> Self {
        Self {
            ts: r.timestamp_ms,
            v: [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7],
        }
    }
}

#[derive(Clone, Default)]
struct Bucket {
    has: [bool; 8],
    min: [f32; 8],
    max: [f32; 8],
    last: [f32; 8],
}

struct CachedChart {
    // Raw samples (time-pruned deque)
    samples: VecDeque<Sample>,
    newest_ts: i64,
    dirty: bool,

    // Cached output
    paths: [String; 8], // SVG path `d`

    // Raw range from current effective window
    raw_min: f32,
    raw_max: f32,

    // Per-channel min/max (for summary cards)
    chan_min: [Option<f32>; 8],
    chan_max: [Option<f32>; 8],

    // Displayed (sticky) range
    disp_min: f32,
    disp_max: f32,

    span_min: f32,
    prev_span_ms: i64,

    // ✅ NEW: last size used to build `paths`
    last_w: f32,
    last_h: f32,
    refit_pending: bool,
}

impl CachedChart {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            newest_ts: 0,
            dirty: true,
            paths: std::array::from_fn(|_| String::new()),
            raw_min: 0.0,
            raw_max: 1.0,
            chan_min: [None; 8],
            chan_max: [None; 8],
            disp_min: 0.0,
            disp_max: 1.0,
            span_min: 0.0,
            prev_span_ms: 0,
            last_w: 0.0,
            last_h: 0.0,
            refit_pending: false,
        }
    }
    fn request_refit(&mut self) {
        self.refit_pending = true;
        self.dirty = true; // ensure next build applies it
    }

    fn ingest(&mut self, r: &TelemetryRow) {
        let s: Sample = r.into();

        if s.ts > self.newest_ts {
            self.newest_ts = s.ts;
        } else if self.newest_ts == 0 {
            self.newest_ts = s.ts;
        }

        self.samples.push_back(s);

        // Time-prune using newest_ts as "now"
        let cutoff = self.newest_ts.saturating_sub(HISTORY_MS);
        while let Some(front) = self.samples.front() {
            if front.ts < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        while self.samples.len() > MAX_SAMPLES_PER_TYPE {
            self.samples.pop_front();
        }

        self.dirty = true;
    }

    fn stabilize_raw_range(min: &mut f32, max: &mut f32) {
        if !min.is_finite() || !max.is_finite() {
            *min = 0.0;
            *max = 1.0;
            return;
        }
        let range = *max - *min;
        if range.abs() < 1e-6 {
            let center = *min;
            let pad = (center.abs() * 0.05).max(1.0);
            *min = center - pad;
            *max = center + pad;
        }
    }

    fn apply_padding(min: f32, max: f32) -> (f32, f32) {
        let mut lo = min;
        let mut hi = max;
        let r = (hi - lo).abs().max(1e-6);
        let pad = (r * Y_PAD_FRAC).max(Y_MIN_PAD_ABS);
        lo -= pad;
        hi += pad;
        (lo, hi)
    }

    fn update_display_range(&mut self, raw_min: f32, raw_max: f32) {
        // Always pad the *raw* range first.
        let (raw_min, raw_max) = Self::apply_padding(raw_min, raw_max);

        // First build: snap.
        if !self.disp_min.is_finite()
            || !self.disp_max.is_finite()
            || (self.disp_max - self.disp_min).abs() < 1e-6
        {
            self.disp_min = raw_min;
            self.disp_max = raw_max;
            return;
        }

        // Expand immediately to avoid clipping.
        let mut lo = self.disp_min;
        let mut hi = self.disp_max;

        if raw_min < lo {
            lo = raw_min;
        }
        if raw_max > hi {
            hi = raw_max;
        }

        // Shrink slowly to avoid “jump” rescaling.
        if raw_min > lo {
            lo = lo + (raw_min - lo) * Y_SHRINK_ALPHA;
        }
        if raw_max < hi {
            hi = hi + (raw_max - hi) * Y_SHRINK_ALPHA;
        }

        // Safety: keep non-degenerate.
        if (hi - lo).abs() < 1e-6 {
            hi = lo + 1.0;
        }

        self.disp_min = lo;
        self.disp_max = hi;
    }

    fn build_if_needed(&mut self, w: f32, h: f32) {
        // ✅ Rebuild if size changed (fixes fullscreen / multi-size callers)
        let size_changed = (self.last_w - w).abs() > 0.5 || (self.last_h - h).abs() > 0.5;
        if !self.dirty && !size_changed {
            return;
        }
        self.last_w = w;
        self.last_h = h;

        if self.samples.is_empty() {
            for s in &mut self.paths {
                s.clear();
            }
            self.raw_min = 0.0;
            self.raw_max = 1.0;
            self.chan_min = [None; 8];
            self.chan_max = [None; 8];
            self.disp_min = 0.0;
            self.disp_max = 1.0;
            self.span_min = 0.0;
            self.prev_span_ms = 0;
            self.dirty = false;
            return;
        }

        let oldest_ts = self.samples.front().map(|s| s.ts).unwrap_or(self.newest_ts);
        let newest_ts = self
            .samples
            .back()
            .map(|s| s.ts)
            .unwrap_or(self.newest_ts)
            .max(self.newest_ts);

        let raw_span_ms = newest_ts.saturating_sub(oldest_ts).max(1);

        // Base span from data (clamped)
        let mut span_ms = raw_span_ms.clamp(MIN_SPAN_MS, HISTORY_MS);

        if self.prev_span_ms > 0 {
            if self.refit_pending {
                // Smoothly shrink prev_span_ms toward span_ms, but still expand immediately if needed.
                let prev = self.prev_span_ms;

                if span_ms > prev {
                    // still expand immediately
                    self.prev_span_ms = span_ms;
                    self.refit_pending = false; // already needs expansion, refit done
                } else {
                    // ease down
                    let diff = (prev - span_ms) as f32;
                    let step = (diff * X_SHRINK_ALPHA).max(1.0);
                    let next = (prev as f32 - step).round() as i64;

                    let next = next.max(span_ms); // never go below target
                    self.prev_span_ms = next;

                    if (self.prev_span_ms - span_ms).abs() <= X_SHRINK_EPS_MS {
                        self.prev_span_ms = span_ms;
                        self.refit_pending = false; // done refitting
                    }
                }
                span_ms = self.prev_span_ms;
            } else {
                // Normal runtime: never shrink automatically (prevents jitter)
                span_ms = span_ms.max(self.prev_span_ms);
                self.prev_span_ms = span_ms;
            }
        } else {
            self.prev_span_ms = span_ms;
        }

        span_ms = span_ms.min(HISTORY_MS);

        self.prev_span_ms = span_ms;

        // Pick point count
        let mut points = ((span_ms + TARGET_BUCKET_MS - 1) / TARGET_BUCKET_MS) as usize;
        points = points.clamp(MIN_POINTS, MAX_POINTS_CAP);
        let px_cap = (w.max(1.0) as usize).saturating_mul(3).max(MIN_POINTS);
        points = points.min(px_cap).clamp(MIN_POINTS, MAX_POINTS_CAP);

        // Compute bucket_ms from span/points, then lock the window to bucket boundaries.
        let mut bucket_ms = (span_ms / points as i64).max(1);

        // Align quantum: round bucket_ms up to a multiple of X_ALIGN_MS
        let align = if X_ALIGN_MS > 1 {
            if bucket_ms % X_ALIGN_MS != 0 {
                bucket_ms = ((bucket_ms + X_ALIGN_MS - 1) / X_ALIGN_MS) * X_ALIGN_MS;
            }
            bucket_ms
        } else {
            bucket_ms
        };

        // Snap end_ts to alignment quantum
        let end_ts = if align > 1 {
            (newest_ts / align) * align
        } else {
            newest_ts
        };

        // Define start_ts so the window spans an integer number of buckets.
        let start_ts = end_ts.saturating_sub(bucket_ms.saturating_mul(points as i64));

        // Update span_ms to match exactly the bucket grid
        let span_ms = bucket_ms.saturating_mul(points as i64);
        self.prev_span_ms = self.prev_span_ms.max(span_ms).min(HISTORY_MS);

        let mut buckets = vec![Bucket::default(); points];

        let mut chan_min: [Option<f32>; 8] = [None; 8];
        let mut chan_max: [Option<f32>; 8] = [None; 8];

        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;

        for samp in self.samples.iter() {
            if samp.ts < start_ts {
                continue;
            }

            let bi = ((samp.ts - start_ts) / bucket_ms).clamp(0, (points as i64) - 1) as usize;
            let b = &mut buckets[bi];

            for ch in 0..8 {
                if let Some(v) = samp.v[ch] {
                    if !b.has[ch] {
                        b.has[ch] = true;
                        b.min[ch] = v;
                        b.max[ch] = v;
                        b.last[ch] = v;
                    } else {
                        b.min[ch] = b.min[ch].min(v);
                        b.max[ch] = b.max[ch].max(v);
                        b.last[ch] = v;
                    }

                    min = min.min(v);
                    max = max.max(v);

                    chan_min[ch] = Some(match chan_min[ch] {
                        Some(m) => m.min(v),
                        None => v,
                    });
                    chan_max[ch] = Some(match chan_max[ch] {
                        Some(m) => m.max(v),
                        None => v,
                    });
                }
            }
        }

        Self::stabilize_raw_range(&mut min, &mut max);

        self.raw_min = min;
        self.raw_max = max;
        self.chan_min = chan_min;
        self.chan_max = chan_max;

        // Sticky displayed range
        self.update_display_range(self.raw_min, self.raw_max);

        // Viewport mapping (match DataTab geometry)
        let left = 60.0_f32;
        let right = (w - 20.0).max(left + 1.0);
        let top = 20.0_f32;
        let bottom = (h - 20.0).max(top + 1.0);

        let pw = right - left;
        let ph = bottom - top;

        let y_min = self.disp_min;
        let y_max = self.disp_max;

        let map_y = |v: f32| -> f32 { bottom - (v - y_min) / (y_max - y_min) * ph };

        for s in &mut self.paths {
            s.clear();
        }

        let mut last_seen: [Option<f32>; 8] = [None; 8];

        for idx in 0..buckets.len() {
            let x = left + pw * ((idx as f32 + 0.5) / buckets.len().max(1) as f32);
            let b = &buckets[idx];

            for ch in 0..8 {
                let v_opt = if b.has[ch] {
                    let v = b.last[ch];
                    last_seen[ch] = Some(v);
                    Some(v)
                } else {
                    last_seen[ch]
                };

                let Some(v) = v_opt else { continue };
                let y = map_y(v);

                let out = &mut self.paths[ch];
                if out.is_empty() {
                    out.push_str(&format!("M {:.2} {:.2} ", x, y));
                } else {
                    out.push_str(&format!("L {:.2} {:.2} ", x, y));
                }
            }
        }

        self.span_min = span_ms as f32 / 60_000.0;
        self.dirty = false;
    }
}

// ============================================================
// Labels / colors
// ============================================================

pub fn series_color(i: usize) -> &'static str {
    [
        "#f97316", "#22d3ee", "#a3e635", "#f43f5e", "#8b5cf6", "#e879f9", "#10b981", "#fbbf24",
    ]
    .get(i)
    .copied()
    .unwrap_or("#9ca3af")
}

pub fn labels_for_datatype(dt: &str) -> [&'static str; 8] {
    match dt {
        "GYRO_DATA" => ["Roll", "Pitch", "Yaw", "", "", "", "", ""],
        "ACCEL_DATA" => ["X Accel", "Y Accel", "Z Accel", "", "", "", "", ""],
        "BAROMETER_DATA" => ["Pressure", "Temp", "Altitude", "", "", "", "", ""],
        "KALMAN_FILTER_DATA" => ["X", "Y", "Z", "", "", "", "", ""],
        "GPS_DATA" => ["Lat", "Lon", "", "", "", "", "", ""],
        "FUEL_TANK_PRESSURE" => ["Tank Pressure", "", "", "", "", "", "", ""],
        "VALVE_STATE" => [
            "Pilot",
            "NormallyOpen",
            "Dump",
            "Igniter",
            "Nitrogen",
            "Nitrous",
            "Fill Lines",
            "",
        ],
        _ => ["", "", "", "", "", "", "", ""],
    }
}
