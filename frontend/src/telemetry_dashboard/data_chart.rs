// frontend/src/telemetry_dashboard/data_chart.rs
//
// Stable chart buckets (artifact-free):
// - Fixed bucket grid in absolute time (epoch-aligned).
// - Historical bucket values NEVER change (only newest bucket is "live").
// - Rendering uses the last N buckets -> stable X ordering, stable per-bucket Y.
//
// Keeps the API:
//   charts_cache_reset_and_ingest(rows)
//   charts_cache_ingest_row(row)
//   charts_cache_get(data_type, width, height) -> ([d;8], y_min, y_max, span_min)
//   charts_cache_get_channel_minmax(data_type, width, height) -> (mins, maxs)
//
// Y-axis:
// - Expand immediately (never clip)
// - Shrink only on explicit refit (charts_cache_request_refit)
// - Padding added around range
//
// Span:
// - Expand immediately
// - Shrink only on refit
//
// IMPORTANT:
// - This intentionally trades “perfect accuracy for late/out-of-order samples”
//   for visual stability: once a bucket is in the past, it is frozen.

use groundstation_shared::TelemetryRow;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use super::HISTORY_MS;

// -------------------------
// Bucket grid configuration
// -------------------------
//
// This is our downsample timebase.
// Smaller = more detail, more points.
// Larger = smoother, fewer points.
//
// 20ms = 50Hz plotted. 40ms = 25Hz plotted. 100ms = 10Hz plotted.
const BUCKET_MS: i64 = 20;

// Only this many most-recent buckets are kept (hard cap besides HISTORY_MS).
const MAX_BUCKETS_PER_TYPE: usize = 60_000;

// Only the newest bucket is mutable. Older buckets are frozen.
// If you want to allow small reordering/late packets, set this to 2 or 3.
const LIVE_BUCKETS_BACK: i64 = 1;

// Avoid zero span
const MIN_SPAN_MS: i64 = 1_000;

// X-span refit tuning (used only when refit_pending=true)
const X_SHRINK_ALPHA: f32 = 0.18;
const X_SHRINK_EPS_MS: i64 = 250;

// Y-range tuning
const Y_SHRINK_ALPHA: f32 = 0.10; // used only during refit_pending
const Y_PAD_FRAC: f32 = 0.06;
const Y_MIN_PAD_ABS: f32 = 1.0;

pub fn charts_cache_request_refit() {
    CHARTS_CACHE.with(|c| c.borrow_mut().request_refit());
}

// ============================================================
// Global cache
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
/// - [String;8] SVG path `d` strings (may be empty)
/// - y_min, y_max (labels)
/// - span_min (minutes of effective window)
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
// Internals
// ============================================================

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

#[derive(Clone, Default)]
struct Bucket {
    // absolute bucket id (ts_ms / BUCKET_MS)
    id: i64,

    has: [bool; 8],
    last: [f32; 8],
    min: [f32; 8],
    max: [f32; 8],
}

impl Bucket {
    fn new(id: i64) -> Self {
        Self {
            id,
            has: [false; 8],
            last: [0.0; 8],
            min: [0.0; 8],
            max: [0.0; 8],
        }
    }

    fn update(&mut self, v: [Option<f32>; 8]) {
        for ch in 0..8 {
            if let Some(x) = v[ch] {
                if !x.is_finite() {
                    continue;
                }
                if !self.has[ch] {
                    self.has[ch] = true;
                    self.last[ch] = x;
                    self.min[ch] = x;
                    self.max[ch] = x;
                } else {
                    self.last[ch] = x;
                    self.min[ch] = self.min[ch].min(x);
                    self.max[ch] = self.max[ch].max(x);
                }
            }
        }
    }
}

struct CachedChart {
    buckets: VecDeque<Bucket>,
    newest_bucket_id: i64,
    newest_ts: i64,
    dirty: bool,

    // cached output
    paths: [String; 8],

    // per-window min/max (raw)
    raw_min: f32,
    raw_max: f32,

    // per-channel min/max over window
    chan_min: [Option<f32>; 8],
    chan_max: [Option<f32>; 8],

    // displayed (sticky) range
    disp_min: f32,
    disp_max: f32,

    // displayed span (sticky)
    span_min: f32,
    prev_span_ms: i64,

    // last size
    last_w: f32,
    last_h: f32,

    // if true: allow shrink of x-span and y-range until settled
    refit_pending: bool,
}

impl CachedChart {
    fn new() -> Self {
        Self {
            buckets: VecDeque::new(),
            newest_bucket_id: 0,
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
        self.dirty = true;
    }

    fn ingest(&mut self, r: &TelemetryRow) {
        let ts = r.timestamp_ms;
        let bid = ts.div_euclid(BUCKET_MS);

        if self.newest_ts == 0 || ts > self.newest_ts {
            self.newest_ts = ts;
        }
        if self.buckets.is_empty() {
            self.newest_bucket_id = bid;
            self.buckets.push_back(Bucket::new(bid));
        } else if bid > self.newest_bucket_id {
            // append buckets up to new bucket
            let mut cur = self.newest_bucket_id;
            while cur < bid {
                cur += 1;
                self.buckets.push_back(Bucket::new(cur));
            }
            self.newest_bucket_id = bid;
        }

        // Freeze rule: only allow updating buckets within LIVE_BUCKETS_BACK of newest.
        // This guarantees historical buckets never change.
        let live_min = self.newest_bucket_id.saturating_sub(LIVE_BUCKETS_BACK - 1);

        if bid < live_min {
            // ignore late/out-of-order for stability
            return;
        }

        // find bucket
        if let Some(back) = self.buckets.back_mut() {
            if back.id == bid {
                back.update([r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7]);
            } else {
                // bid may be within last LIVE_BUCKETS_BACK; scan from back a tiny amount
                for b in self.buckets.iter_mut().rev().take(LIVE_BUCKETS_BACK as usize + 2) {
                    if b.id == bid {
                        b.update([r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7]);
                        break;
                    }
                }
            }
        }

        // time prune by HISTORY_MS using bucket ids
        let oldest_allowed_ts = self.newest_ts.saturating_sub(HISTORY_MS);
        let oldest_allowed_bid = oldest_allowed_ts.div_euclid(BUCKET_MS);

        while let Some(front) = self.buckets.front() {
            if front.id < oldest_allowed_bid {
                self.buckets.pop_front();
            } else {
                break;
            }
        }

        while self.buckets.len() > MAX_BUCKETS_PER_TYPE {
            self.buckets.pop_front();
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
        let (raw_min, raw_max) = Self::apply_padding(raw_min, raw_max);

        if !self.disp_min.is_finite()
            || !self.disp_max.is_finite()
            || (self.disp_max - self.disp_min).abs() < 1e-6
        {
            self.disp_min = raw_min;
            self.disp_max = raw_max;
            return;
        }

        // expand immediately
        let mut lo = self.disp_min;
        let mut hi = self.disp_max;

        if raw_min < lo {
            lo = raw_min;
        }
        if raw_max > hi {
            hi = raw_max;
        }

        // shrink only during refit
        if self.refit_pending {
            if raw_min > lo {
                lo = lo + (raw_min - lo) * Y_SHRINK_ALPHA;
            }
            if raw_max < hi {
                hi = hi + (raw_max - hi) * Y_SHRINK_ALPHA;
            }
        }

        if (hi - lo).abs() < 1e-6 {
            hi = lo + 1.0;
        }

        self.disp_min = lo;
        self.disp_max = hi;
    }

    fn build_if_needed(&mut self, w: f32, h: f32) {
        let size_changed = (self.last_w - w).abs() > 0.5 || (self.last_h - h).abs() > 0.5;
        if !self.dirty && !size_changed {
            return;
        }
        self.last_w = w;
        self.last_h = h;

        if self.buckets.is_empty() {
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
            self.refit_pending = false;
            self.dirty = false;
            return;
        }

        let newest_bid = self.newest_bucket_id;
        let oldest_bid_available = self.buckets.front().map(|b| b.id).unwrap_or(newest_bid);

        // compute actual available span from buckets (in ms)
        let raw_span_ms = ((newest_bid - oldest_bid_available + 1).max(1)) * BUCKET_MS;

        let desired_span_ms = raw_span_ms.clamp(MIN_SPAN_MS, HISTORY_MS);

        // expand-only span unless refit
        let prev = self.prev_span_ms;
        let mut span_ms = if prev <= 0 {
            desired_span_ms
        } else if desired_span_ms > prev {
            desired_span_ms
        } else if self.refit_pending {
            let diff = (prev - desired_span_ms) as f32;
            let step = (diff * X_SHRINK_ALPHA).max(1.0);
            let mut next = (prev as f32 - step).round() as i64;
            next = next.max(desired_span_ms);
            if (next - desired_span_ms).abs() <= X_SHRINK_EPS_MS {
                desired_span_ms
            } else {
                next
            }
        } else {
            prev
        };
        span_ms = span_ms.min(HISTORY_MS);
        self.prev_span_ms = span_ms;

        // Determine how many buckets to render from that span (stable)
        let want_buckets = (span_ms.div_euclid(BUCKET_MS)).max(1);
        let start_bid = newest_bid.saturating_sub(want_buckets - 1);

        // Build window min/max from buckets in [start_bid, newest_bid]
        let mut chan_min: [Option<f32>; 8] = [None; 8];
        let mut chan_max: [Option<f32>; 8] = [None; 8];
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;

        for b in self.buckets.iter() {
            if b.id < start_bid || b.id > newest_bid {
                continue;
            }
            for ch in 0..8 {
                if b.has[ch] {
                    let v = b.last[ch];
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

        self.update_display_range(self.raw_min, self.raw_max);

        // If refit, clear it when close to target
        if self.refit_pending {
            let span_settled = (self.prev_span_ms - desired_span_ms).abs() <= X_SHRINK_EPS_MS;
            let (pmin, pmax) = Self::apply_padding(self.raw_min, self.raw_max);
            let y_settled =
                (self.disp_min - pmin).abs() < 1e-3 && (self.disp_max - pmax).abs() < 1e-3;
            if span_settled && y_settled {
                self.refit_pending = false;
            }
        }

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

        // Build paths by iterating stable bucket ids in order.
        // If a bucket is missing (pruned gaps), we just skip it.
        //
        // Also: to keep line continuity, we carry-forward last_seen if a bucket has no value.
        // This does NOT mutate historical bucket values; it's just how we draw gaps.
        let mut last_seen: [Option<f32>; 8] = [None; 8];

        let total = (newest_bid - start_bid + 1).max(1) as f32;

        for b in self.buckets.iter() {
            if b.id < start_bid || b.id > newest_bid {
                continue;
            }

            let i = (b.id - start_bid) as f32;
            let x = left + pw * ((i + 0.5) / total);

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

        self.span_min = (want_buckets as f32 * BUCKET_MS as f32) / 60_000.0;
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
        "BATTERY_VOLTAGE" => ["Voltage", "", "", "", "", "", "", ""],
        "BATTERY_CURRENT" => ["Current", "", "", "", "", "", "", ""],
        "FUEL_FLOW" => ["Flow Rate", "", "", "", "", "", "", ""],
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
