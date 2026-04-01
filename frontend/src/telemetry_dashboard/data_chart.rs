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

use super::types::TelemetryRow;
use dioxus::prelude::*;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::HISTORY_MS;

const SENDER_SPLIT_DATA_TYPES: &[&str] = &["BATTERY_VOLTAGE", "BATTERY_CURRENT"];
const BATTERY_COMBINED_CHANNELS: usize = 2;

pub fn sender_scoped_chart_key(data_type: &str, sender_id: &str) -> String {
    format!("{data_type}@@{sender_id}")
}

pub fn combined_battery_chart_key(data_type: &str) -> Option<String> {
    should_split_sender_chart(data_type).then(|| format!("{data_type}@@combined"))
}

fn should_split_sender_chart(data_type: &str) -> bool {
    SENDER_SPLIT_DATA_TYPES.contains(&data_type)
}

fn battery_sender_channel(sender_id: &str) -> Option<usize> {
    match sender_id {
        "PB" => Some(0),
        "GW" => Some(1),
        _ => None,
    }
}

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
const LOD_BUCKET_MS_LEVELS: &[(i64, i64)] = &[
    (2 * 60_000, BUCKET_MS),
    (5 * 60_000, 50),
    (10 * 60_000, 100),
    (15 * 60_000, 200),
    (HISTORY_MS, 500),
];

pub const CHART_GRID_LEFT: f64 = 96.0;
pub const CHART_GRID_RIGHT_PAD: f64 = 20.0;
pub const CHART_GRID_TOP: f64 = 20.0;
pub const CHART_GRID_BOTTOM_PAD: f64 = 40.0;
pub const CHART_X_LABEL_LEFT_INSET: f64 = 28.0;
pub const CHART_X_LABEL_BOTTOM: f64 = 10.0;
pub const CHART_Y_LABEL_LEFT: f64 = 10.0;
pub const CHART_Y_LABEL_MAX_WIDTH: f64 = 64.0;

// Only this many most-recent buckets are kept (hard cap besides HISTORY_MS).
// Keep enough to cover the full HISTORY_MS window at BUCKET_MS granularity.
const MAX_BUCKETS_PER_TYPE: usize = (HISTORY_MS as usize / BUCKET_MS as usize) + 500;

// Only recent buckets are mutable. Older buckets are frozen.
// Allow a few buckets for packet jitter/reordering on slower devices.
const LIVE_BUCKETS_BACK: i64 = 3;
// Always bridge gaps for continuous display, but cap inserted points per gap for performance.
const MAX_INTERP_POINTS_PER_GAP: i64 = 64;
// Only bridge short gaps (packet jitter). Large gaps should remain visually broken.
const MAX_INTERP_GAP_BUCKETS: i64 = 6;
const CURVE_MIN_DELTA_PX: f32 = 0.35;
const RENDER_CHUNK_MS: i64 = 20_000;
const SMOOTHING_MAX_POINTS_PER_SEGMENT: usize = 240;

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
    static RESEED_CACHE: RefCell<Option<ChartsCache>> = const { RefCell::new(None) };
}

pub fn _charts_cache_is_dirty(data_type: &str) -> bool {
    CHARTS_CACHE.with(|c| {
        let c = c.borrow();
        c.charts.get(data_type).map(|ch| ch.dirty).unwrap_or(false)
    })
}

pub fn charts_cache_begin_reseed_build() {
    RESEED_CACHE.with(|c| {
        *c.borrow_mut() = Some(ChartsCache::new());
    });
}

pub fn charts_cache_cancel_reseed_build() {
    RESEED_CACHE.with(|c| {
        c.borrow_mut().take();
    });
}

pub fn charts_cache_reseed_ingest_row(row: &TelemetryRow) {
    RESEED_CACHE.with(|c| {
        if let Some(cache) = c.borrow_mut().as_mut() {
            cache.ingest_row(row);
            cache.ingest_sender_scoped_row(row);
        }
    });
}

pub fn charts_cache_finish_reseed_build() {
    RESEED_CACHE.with(|slot| {
        if let Some(new_cache) = slot.borrow_mut().take() {
            CHARTS_CACHE.with(|active| {
                *active.borrow_mut() = new_cache;
            });
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn charts_cache_reset_and_ingest(rows: &[TelemetryRow]) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c._clear();
        for r in rows {
            c.ingest_row(r);
            c.ingest_sender_scoped_row(r);
        }
    });
}

pub fn charts_cache_ingest_row(row: &TelemetryRow) {
    CHARTS_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        cache.ingest_row(row);
        cache.ingest_sender_scoped_row(row);
    });
}

/// Returns:
/// - chunked path groups for canvas rendering
/// - y_min, y_max (labels)
/// - span_min (minutes of effective window)
pub fn charts_cache_get(
    data_type: &str,
    width: f32,
    height: f32,
) -> (Rc<Vec<ChartRenderChunk>>, f32, f32, f32) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.get(data_type, width, height)
    })
}

pub fn charts_cache_get_subset(
    data_type: &str,
    channels: &[usize],
    width: f32,
    height: f32,
) -> (Rc<Vec<ChartRenderChunk>>, f32, f32, f32) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(chart) = c.charts.get_mut(data_type) {
            chart.build_subset(channels, width, height)
        } else {
            (Rc::new(Vec::new()), 0.0, 1.0, 0.0)
        }
    })
}

pub fn charts_cache_get_subset_per_series(
    data_type: &str,
    channels: &[usize],
    width: f32,
    height: f32,
) -> (Rc<Vec<ChartRenderChunk>>, Rc<Vec<Option<(f32, f32)>>>, f32) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(chart) = c.charts.get_mut(data_type) {
            chart.build_subset_per_series(channels, width, height)
        } else {
            (Rc::new(Vec::new()), Rc::new(Vec::new()), 0.0)
        }
    })
}

pub fn charts_cache_get_channel_minmax(
    data_type: &str,
    width: f32,
    height: f32,
) -> (Vec<Option<f32>>, Vec<Option<f32>>) {
    CHARTS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(ch) = c.charts.get_mut(data_type) {
            ch.build_if_needed(width, height);
            (ch.chan_min.clone(), ch.chan_max.clone())
        } else {
            (Vec::new(), Vec::new())
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

    fn _clear(&mut self) {
        self.charts.clear();
    }

    fn ingest_row(&mut self, r: &TelemetryRow) {
        let chart = self
            .charts
            .entry(r.data_type.clone())
            .or_insert_with(CachedChart::new);
        chart.ingest(r);
    }

    fn ingest_sender_scoped_row(&mut self, r: &TelemetryRow) {
        if !should_split_sender_chart(&r.data_type) || r.sender_id.is_empty() {
            return;
        }
        let chart = self
            .charts
            .entry(sender_scoped_chart_key(&r.data_type, &r.sender_id))
            .or_insert_with(CachedChart::new);
        chart.ingest(r);

        let Some(channel) = battery_sender_channel(&r.sender_id) else {
            return;
        };
        let Some(combined_key) = combined_battery_chart_key(&r.data_type) else {
            return;
        };
        let mut combined_values = vec![None; BATTERY_COMBINED_CHANNELS];
        combined_values[channel] = r.values.first().copied().flatten();
        let combined_row = TelemetryRow {
            timestamp_ms: r.timestamp_ms,
            data_type: combined_key,
            sender_id: String::new(),
            values: combined_values,
        };
        let combined_chart = self
            .charts
            .entry(combined_row.data_type.clone())
            .or_insert_with(CachedChart::new);
        combined_chart.ingest(&combined_row);
    }

    fn get(&mut self, dt: &str, w: f32, h: f32) -> (Rc<Vec<ChartRenderChunk>>, f32, f32, f32) {
        if let Some(c) = self.charts.get_mut(dt) {
            c.build_if_needed(w, h);
            (c.chunks.clone(), c.disp_min, c.disp_max, c.span_min)
        } else {
            (Rc::new(Vec::new()), 0.0, 1.0, 0.0)
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

    has: Vec<bool>,
    last: Vec<f32>,
    min: Vec<f32>,
    max: Vec<f32>,
}

impl Bucket {
    fn new(id: i64, channels: usize) -> Self {
        Self {
            id,
            has: vec![false; channels],
            last: vec![0.0; channels],
            min: vec![0.0; channels],
            max: vec![0.0; channels],
        }
    }

    fn ensure_channels(&mut self, channels: usize) {
        if self.has.len() >= channels {
            return;
        }
        let add = channels - self.has.len();
        self.has.extend(std::iter::repeat_n(false, add));
        self.last.extend(std::iter::repeat_n(0.0, add));
        self.min.extend(std::iter::repeat_n(0.0, add));
        self.max.extend(std::iter::repeat_n(0.0, add));
    }

    fn update(&mut self, v: &[Option<f32>]) {
        self.ensure_channels(v.len());
        for (ch, val) in v.iter().enumerate() {
            if let Some(x) = val {
                if !x.is_finite() {
                    continue;
                }
                if !self.has[ch] {
                    self.has[ch] = true;
                    self.last[ch] = *x;
                    self.min[ch] = *x;
                    self.max[ch] = *x;
                } else {
                    self.last[ch] = *x;
                    self.min[ch] = self.min[ch].min(*x);
                    self.max[ch] = self.max[ch].max(*x);
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
    channel_count: usize,

    // cached output
    chunks: Rc<Vec<ChartRenderChunk>>,
    subset_cache: HashMap<SubsetCacheKey, CachedSubset>,
    subset_per_series_cache: HashMap<SubsetCacheKey, CachedSubsetPerSeries>,

    // per-window min/max (raw)
    raw_min: f32,
    raw_max: f32,

    // per-channel min/max over window
    chan_min: Vec<Option<f32>>,
    chan_max: Vec<Option<f32>>,

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

fn lod_bucket_ms_for_span(span_ms: i64) -> i64 {
    for &(span_threshold_ms, bucket_ms) in LOD_BUCKET_MS_LEVELS {
        if span_ms <= span_threshold_ms {
            return bucket_ms.max(BUCKET_MS);
        }
    }
    BUCKET_MS
}

impl CachedChart {
    fn new() -> Self {
        Self {
            buckets: VecDeque::new(),
            newest_bucket_id: 0,
            newest_ts: 0,
            dirty: true,
            channel_count: 0,
            chunks: Rc::new(Vec::new()),
            subset_cache: HashMap::new(),
            subset_per_series_cache: HashMap::new(),
            raw_min: 0.0,
            raw_max: 1.0,
            chan_min: Vec::new(),
            chan_max: Vec::new(),
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
        let ch_count = r.values.len();
        if ch_count > self.channel_count {
            self.ensure_channels(ch_count);
        }
        let ts = r.timestamp_ms;
        let bid = ts.div_euclid(BUCKET_MS);

        if self.newest_ts == 0 || ts > self.newest_ts {
            self.newest_ts = ts;
        }
        if self.buckets.is_empty() {
            self.newest_bucket_id = bid;
            self.buckets.push_back(Bucket::new(bid, self.channel_count));
        } else if bid > self.newest_bucket_id {
            // append buckets up to new bucket
            let mut cur = self.newest_bucket_id;
            while cur < bid {
                cur += 1;
                self.buckets.push_back(Bucket::new(cur, self.channel_count));
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
                back.update(&r.values);
            } else {
                // bid may be within last LIVE_BUCKETS_BACK; scan from back a tiny amount
                for b in self
                    .buckets
                    .iter_mut()
                    .rev()
                    .take(LIVE_BUCKETS_BACK as usize + 2)
                {
                    if b.id == bid {
                        b.update(&r.values);
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

    fn ensure_channels(&mut self, channels: usize) {
        if self.channel_count >= channels {
            return;
        }
        let add = channels - self.channel_count;
        self.channel_count = channels;
        self.chan_min.extend(std::iter::repeat_n(None, add));
        self.chan_max.extend(std::iter::repeat_n(None, add));
        for b in self.buckets.iter_mut() {
            b.ensure_channels(channels);
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

    fn view_buckets(&self, start_bid: i64, newest_bid: i64, bucket_ms: i64) -> Vec<Bucket> {
        let effective_bucket_ms = bucket_ms.max(BUCKET_MS);
        let start_ts = start_bid.saturating_mul(BUCKET_MS);
        let newest_ts = newest_bid.saturating_mul(BUCKET_MS);

        if effective_bucket_ms <= BUCKET_MS {
            return self
                .buckets
                .iter()
                .filter(|bucket| bucket.id >= start_bid && bucket.id <= newest_bid)
                .cloned()
                .collect();
        }

        let mut out = Vec::new();
        let mut current: Option<Bucket> = None;
        let mut current_id = i64::MIN;

        for bucket in self.buckets.iter() {
            if bucket.id < start_bid || bucket.id > newest_bid {
                continue;
            }

            let bucket_ts = bucket.id.saturating_mul(BUCKET_MS);
            if bucket_ts < start_ts || bucket_ts > newest_ts {
                continue;
            }

            let agg_id = bucket_ts.div_euclid(effective_bucket_ms);
            if current.is_none() || current_id != agg_id {
                if let Some(done) = current.take() {
                    out.push(done);
                }
                current_id = agg_id;
                current = Some(Bucket::new(agg_id, self.channel_count));
            }

            if let Some(agg) = current.as_mut() {
                for ch in 0..self.channel_count {
                    if !bucket.has[ch] {
                        continue;
                    }
                    let last = bucket.last[ch];
                    let min = bucket.min[ch];
                    let max = bucket.max[ch];
                    if !agg.has[ch] {
                        agg.has[ch] = true;
                        agg.last[ch] = last;
                        agg.min[ch] = min;
                        agg.max[ch] = max;
                    } else {
                        agg.last[ch] = last;
                        agg.min[ch] = agg.min[ch].min(min);
                        agg.max[ch] = agg.max[ch].max(max);
                    }
                }
            }
        }

        if let Some(done) = current.take() {
            out.push(done);
        }

        out
    }

    fn build_if_needed(&mut self, w: f32, h: f32) {
        let size_changed = (self.last_w - w).abs() > 0.5 || (self.last_h - h).abs() > 0.5;
        if !self.dirty && !size_changed {
            return;
        }
        self.subset_cache.clear();
        self.subset_per_series_cache.clear();
        self.last_w = w;
        self.last_h = h;

        if self.buckets.is_empty() {
            self.chunks = Rc::new(Vec::new());
            self.raw_min = 0.0;
            self.raw_max = 1.0;
            self.chan_min = vec![None; self.channel_count];
            self.chan_max = vec![None; self.channel_count];
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
        let mut span_ms = if prev <= 0 || desired_span_ms > prev {
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
        let lod_bucket_ms = lod_bucket_ms_for_span(span_ms);

        // Determine how many buckets to render from that span (stable)
        let want_buckets = span_ms.div_euclid(BUCKET_MS).max(1);
        let start_bid = newest_bid.saturating_sub(want_buckets - 1);
        let view_buckets = self.view_buckets(start_bid, newest_bid, lod_bucket_ms);
        let start_view_id = start_bid
            .saturating_mul(BUCKET_MS)
            .div_euclid(lod_bucket_ms);
        let newest_view_id = newest_bid
            .saturating_mul(BUCKET_MS)
            .div_euclid(lod_bucket_ms);

        // Build window min/max from buckets in [start_bid, newest_bid]
        let mut chan_min: Vec<Option<f32>> = vec![None; self.channel_count];
        let mut chan_max: Vec<Option<f32>> = vec![None; self.channel_count];
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;

        for b in &view_buckets {
            for ch in 0..self.channel_count {
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
        let left = CHART_GRID_LEFT as f32;
        let right = (w - CHART_GRID_RIGHT_PAD as f32).max(left + 1.0);
        let top = CHART_GRID_TOP as f32;
        let bottom = (h - CHART_GRID_BOTTOM_PAD as f32).max(top + 1.0);

        let pw = right - left;
        let ph = bottom - top;

        let y_min = self.disp_min;
        let y_max = self.disp_max;
        let map_y = |v: f32| -> f32 { bottom - (v - y_min) / (y_max - y_min) * ph };

        let mut chunks = Vec::new();

        let total = (newest_view_id - start_view_id + 1).max(1) as f32;
        let render_chunk_buckets = (RENDER_CHUNK_MS / lod_bucket_ms).max(1);
        let first_chunk_id = start_view_id.div_euclid(render_chunk_buckets);
        let last_chunk_id = newest_view_id.div_euclid(render_chunk_buckets);
        let max_interp_gap_buckets = ((MAX_INTERP_GAP_BUCKETS * BUCKET_MS) / lod_bucket_ms).max(1);

        for chunk_id in first_chunk_id..=last_chunk_id {
            let chunk_start_bid = (chunk_id * render_chunk_buckets).max(start_view_id);
            let chunk_end_bid = ((chunk_id + 1) * render_chunk_buckets - 1).min(newest_view_id);
            let chunk_bucket_count = (chunk_end_bid - chunk_start_bid + 1).max(1);
            let chunk_start_x = left + pw * ((chunk_start_bid - start_view_id) as f32 / total);
            let chunk_end_x = left + pw * ((chunk_end_bid - start_view_id + 1) as f32 / total);
            let chunk_width = (chunk_end_x - chunk_start_x).max(1.0);
            let allow_interp = true;
            let smooth_chunk =
                lod_bucket_ms <= 100 && should_smooth_chunk(chunk_width, chunk_bucket_count);

            let mut paths = vec![String::new(); self.channel_count];
            let mut gap_paths = vec![String::new(); self.channel_count];
            let mut segment_points: Vec<Vec<(f32, f32)>> = vec![Vec::new(); self.channel_count];
            let mut last_bucket_id_drawn: Vec<Option<i64>> = vec![None; self.channel_count];
            let mut last_point_drawn: Vec<Option<(f32, f32)>> = vec![None; self.channel_count];

            for b in &view_buckets {
                if b.id < chunk_start_bid || b.id > chunk_end_bid {
                    continue;
                }
                let has_any = b.has.iter().any(|v| *v);
                if !has_any {
                    continue;
                }

                let rel_bid = b.id - chunk_start_bid;
                let x = chunk_width * ((rel_bid as f32 + 0.5) / chunk_bucket_count as f32);

                for ch in 0..self.channel_count {
                    if !b.has[ch] {
                        continue;
                    }
                    let v = b.last[ch];
                    let y = map_y(v);
                    if let Some(prev_bid) = last_bucket_id_drawn[ch] {
                        let gap_buckets = b.id - prev_bid;
                        if gap_buckets > 1
                            && let Some((prev_x, prev_y)) = last_point_drawn[ch]
                        {
                            if allow_interp && gap_buckets <= max_interp_gap_buckets {
                                let missing = gap_buckets - 1;
                                let interp_pts = missing.clamp(1, MAX_INTERP_POINTS_PER_GAP);
                                for j in 1..=interp_pts {
                                    let t = j as f32 / (interp_pts + 1) as f32;
                                    let xi = prev_x + (x - prev_x) * t;
                                    let yi = prev_y + (y - prev_y) * t;
                                    push_segment_point(&mut segment_points[ch], xi, yi);
                                }
                            } else {
                                flush_smoothed_segment(
                                    &mut paths[ch],
                                    &segment_points[ch],
                                    smooth_chunk,
                                );
                                segment_points[ch].clear();
                                gap_paths[ch].push_str(&format!(
                                    "M {:.2} {:.2} L {:.2} {:.2} ",
                                    prev_x, prev_y, x, y
                                ));
                            }
                        }
                    }

                    push_segment_point(&mut segment_points[ch], x, y);
                    last_bucket_id_drawn[ch] = Some(b.id);
                    last_point_drawn[ch] = Some((x, y));
                }
            }

            for ch in 0..self.channel_count {
                flush_smoothed_segment(&mut paths[ch], &segment_points[ch], smooth_chunk);
            }

            if paths.iter().all(|p| p.is_empty()) && gap_paths.iter().all(|p| p.is_empty()) {
                continue;
            }

            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            chunk_id.hash(&mut hasher);
            paths.hash(&mut hasher);
            gap_paths.hash(&mut hasher);

            chunks.push(ChartRenderChunk {
                id: chunk_id,
                x: chunk_start_x as f64,
                width: chunk_width as f64,
                right: chunk_end_x as f64,
                paths,
                gap_paths,
                signature: hasher.finish(),
                live: chunk_id == last_chunk_id,
            });
        }

        self.chunks = Rc::new(chunks);

        self.span_min = span_ms as f32 / 60_000.0;
        self.dirty = false;
    }

    fn build_subset(
        &mut self,
        channels: &[usize],
        w: f32,
        h: f32,
    ) -> (Rc<Vec<ChartRenderChunk>>, f32, f32, f32) {
        self.build_if_needed(w, h);

        if self.buckets.is_empty() || channels.is_empty() {
            return (Rc::new(Vec::new()), 0.0, 1.0, 0.0);
        }

        let valid_channels = self.normalize_channels(channels);
        if valid_channels.is_empty() {
            return (Rc::new(Vec::new()), 0.0, 1.0, 0.0);
        }

        let cache_key = SubsetCacheKey::new(&valid_channels, w, h);
        if let Some(cached) = self.subset_cache.get(&cache_key) {
            return (
                cached.chunks.clone(),
                cached.y_min,
                cached.y_max,
                cached.span_min,
            );
        }

        let newest_bid = self.newest_bucket_id;
        let span_ms = self.prev_span_ms.clamp(MIN_SPAN_MS, HISTORY_MS);
        let lod_bucket_ms = lod_bucket_ms_for_span(span_ms);
        let want_buckets = span_ms.div_euclid(BUCKET_MS).max(1);
        let start_bid = newest_bid.saturating_sub(want_buckets - 1);
        let view_buckets = self.view_buckets(start_bid, newest_bid, lod_bucket_ms);
        let start_view_id = start_bid
            .saturating_mul(BUCKET_MS)
            .div_euclid(lod_bucket_ms);
        let newest_view_id = newest_bid
            .saturating_mul(BUCKET_MS)
            .div_euclid(lod_bucket_ms);

        let left = CHART_GRID_LEFT as f32;
        let right = (w - CHART_GRID_RIGHT_PAD as f32).max(left + 1.0);
        let top = CHART_GRID_TOP as f32;
        let bottom = (h - CHART_GRID_BOTTOM_PAD as f32).max(top + 1.0);
        let pw = right - left;
        let ph = bottom - top;

        let mut raw_min = f32::INFINITY;
        let mut raw_max = f32::NEG_INFINITY;
        for b in &view_buckets {
            for &ch in &valid_channels {
                if b.has[ch] {
                    let v = b.last[ch];
                    raw_min = raw_min.min(v);
                    raw_max = raw_max.max(v);
                }
            }
        }

        Self::stabilize_raw_range(&mut raw_min, &mut raw_max);
        let (y_min, y_max) = Self::apply_padding(raw_min, raw_max);
        let map_y = |v: f32| -> f32 { bottom - (v - y_min) / (y_max - y_min) * ph };

        let total = (newest_view_id - start_view_id + 1).max(1) as f32;
        let render_chunk_buckets = (RENDER_CHUNK_MS / lod_bucket_ms).max(1);
        let first_chunk_id = start_view_id.div_euclid(render_chunk_buckets);
        let last_chunk_id = newest_view_id.div_euclid(render_chunk_buckets);
        let max_interp_gap_buckets = ((MAX_INTERP_GAP_BUCKETS * BUCKET_MS) / lod_bucket_ms).max(1);
        let mut chunks = Vec::new();

        for chunk_id in first_chunk_id..=last_chunk_id {
            let chunk_start_bid = (chunk_id * render_chunk_buckets).max(start_view_id);
            let chunk_end_bid = ((chunk_id + 1) * render_chunk_buckets - 1).min(newest_view_id);
            let chunk_bucket_count = (chunk_end_bid - chunk_start_bid + 1).max(1);
            let chunk_start_x = left + pw * ((chunk_start_bid - start_view_id) as f32 / total);
            let chunk_end_x = left + pw * ((chunk_end_bid - start_view_id + 1) as f32 / total);
            let chunk_width = (chunk_end_x - chunk_start_x).max(1.0);
            let smooth_chunk =
                lod_bucket_ms <= 100 && should_smooth_chunk(chunk_width, chunk_bucket_count);

            let mut paths = vec![String::new(); valid_channels.len()];
            let mut gap_paths = vec![String::new(); valid_channels.len()];
            let mut segment_points: Vec<Vec<(f32, f32)>> = vec![Vec::new(); valid_channels.len()];
            let mut last_bucket_id_drawn: Vec<Option<i64>> = vec![None; valid_channels.len()];
            let mut last_point_drawn: Vec<Option<(f32, f32)>> = vec![None; valid_channels.len()];

            for b in &view_buckets {
                if b.id < chunk_start_bid || b.id > chunk_end_bid {
                    continue;
                }
                let rel_bid = b.id - chunk_start_bid;
                let x = chunk_width * ((rel_bid as f32 + 0.5) / chunk_bucket_count as f32);

                for (group_idx, &ch) in valid_channels.iter().enumerate() {
                    if !b.has[ch] {
                        continue;
                    }
                    let v = b.last[ch];
                    let y = map_y(v);
                    if let Some(prev_bid) = last_bucket_id_drawn[group_idx] {
                        let gap_buckets = b.id - prev_bid;
                        if gap_buckets > 1
                            && let Some((prev_x, prev_y)) = last_point_drawn[group_idx]
                        {
                            if gap_buckets <= max_interp_gap_buckets {
                                let missing = gap_buckets - 1;
                                let interp_pts = missing.clamp(1, MAX_INTERP_POINTS_PER_GAP);
                                for j in 1..=interp_pts {
                                    let t = j as f32 / (interp_pts + 1) as f32;
                                    let xi = prev_x + (x - prev_x) * t;
                                    let yi = prev_y + (y - prev_y) * t;
                                    push_segment_point(&mut segment_points[group_idx], xi, yi);
                                }
                            } else {
                                flush_smoothed_segment(
                                    &mut paths[group_idx],
                                    &segment_points[group_idx],
                                    smooth_chunk,
                                );
                                segment_points[group_idx].clear();
                                gap_paths[group_idx].push_str(&format!(
                                    "M {:.2} {:.2} L {:.2} {:.2} ",
                                    prev_x, prev_y, x, y
                                ));
                            }
                        }
                    }

                    push_segment_point(&mut segment_points[group_idx], x, y);
                    last_bucket_id_drawn[group_idx] = Some(b.id);
                    last_point_drawn[group_idx] = Some((x, y));
                }
            }

            for group_idx in 0..valid_channels.len() {
                flush_smoothed_segment(
                    &mut paths[group_idx],
                    &segment_points[group_idx],
                    smooth_chunk,
                );
            }

            if paths.iter().all(|p| p.is_empty()) && gap_paths.iter().all(|p| p.is_empty()) {
                continue;
            }

            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            chunk_id.hash(&mut hasher);
            valid_channels.hash(&mut hasher);
            paths.hash(&mut hasher);
            gap_paths.hash(&mut hasher);

            chunks.push(ChartRenderChunk {
                id: chunk_id,
                x: chunk_start_x as f64,
                width: chunk_width as f64,
                right: chunk_end_x as f64,
                paths,
                gap_paths,
                signature: hasher.finish(),
                live: chunk_id == last_chunk_id,
            });
        }

        let cached = CachedSubset {
            chunks: Rc::new(chunks),
            y_min,
            y_max,
            span_min: self.span_min,
        };
        let result = (
            cached.chunks.clone(),
            cached.y_min,
            cached.y_max,
            cached.span_min,
        );
        self.subset_cache.insert(cache_key, cached);
        result
    }

    fn build_subset_per_series(
        &mut self,
        channels: &[usize],
        w: f32,
        h: f32,
    ) -> (Rc<Vec<ChartRenderChunk>>, Rc<Vec<Option<(f32, f32)>>>, f32) {
        self.build_if_needed(w, h);

        if self.buckets.is_empty() || channels.is_empty() {
            return (Rc::new(Vec::new()), Rc::new(Vec::new()), 0.0);
        }

        let valid_channels = self.normalize_channels(channels);
        if valid_channels.is_empty() {
            return (Rc::new(Vec::new()), Rc::new(Vec::new()), 0.0);
        }

        let cache_key = SubsetCacheKey::new(&valid_channels, w, h);
        if let Some(cached) = self.subset_per_series_cache.get(&cache_key) {
            return (
                cached.chunks.clone(),
                cached.series_scales.clone(),
                cached.span_min,
            );
        }

        let newest_bid = self.newest_bucket_id;
        let span_ms = self.prev_span_ms.clamp(MIN_SPAN_MS, HISTORY_MS);
        let lod_bucket_ms = lod_bucket_ms_for_span(span_ms);
        let want_buckets = span_ms.div_euclid(BUCKET_MS).max(1);
        let start_bid = newest_bid.saturating_sub(want_buckets - 1);
        let view_buckets = self.view_buckets(start_bid, newest_bid, lod_bucket_ms);
        let start_view_id = start_bid
            .saturating_mul(BUCKET_MS)
            .div_euclid(lod_bucket_ms);
        let newest_view_id = newest_bid
            .saturating_mul(BUCKET_MS)
            .div_euclid(lod_bucket_ms);

        let left = CHART_GRID_LEFT as f32;
        let right = (w - CHART_GRID_RIGHT_PAD as f32).max(left + 1.0);
        let top = CHART_GRID_TOP as f32;
        let bottom = (h - CHART_GRID_BOTTOM_PAD as f32).max(top + 1.0);
        let pw = right - left;
        let ph = bottom - top;

        let mut raw_min = f32::INFINITY;
        let mut raw_max = f32::NEG_INFINITY;
        let mut channel_ranges: Vec<Option<(f32, f32)>> = vec![None; valid_channels.len()];

        for b in &view_buckets {
            for (group_idx, &ch) in valid_channels.iter().enumerate() {
                if !b.has[ch] {
                    continue;
                }
                let v = b.last[ch];
                raw_min = raw_min.min(v);
                raw_max = raw_max.max(v);
                channel_ranges[group_idx] = Some(match channel_ranges[group_idx] {
                    Some((min, max)) => (min.min(v), max.max(v)),
                    None => (v, v),
                });
            }
        }

        Self::stabilize_raw_range(&mut raw_min, &mut raw_max);
        let (global_min, global_max) = Self::apply_padding(raw_min, raw_max);
        let zero_ratio = zero_anchor_ratio(global_min, global_max);
        let series_scales: Vec<Option<(f32, f32)>> = channel_ranges
            .iter()
            .map(|range| range.map(|(min, max)| anchored_series_range(min, max, zero_ratio)))
            .collect();

        let total = (newest_view_id - start_view_id + 1).max(1) as f32;
        let render_chunk_buckets = (RENDER_CHUNK_MS / lod_bucket_ms).max(1);
        let first_chunk_id = start_view_id.div_euclid(render_chunk_buckets);
        let last_chunk_id = newest_view_id.div_euclid(render_chunk_buckets);
        let max_interp_gap_buckets = ((MAX_INTERP_GAP_BUCKETS * BUCKET_MS) / lod_bucket_ms).max(1);
        let mut chunks = Vec::new();

        for chunk_id in first_chunk_id..=last_chunk_id {
            let chunk_start_bid = (chunk_id * render_chunk_buckets).max(start_view_id);
            let chunk_end_bid = ((chunk_id + 1) * render_chunk_buckets - 1).min(newest_view_id);
            let chunk_bucket_count = (chunk_end_bid - chunk_start_bid + 1).max(1);
            let chunk_start_x = left + pw * ((chunk_start_bid - start_view_id) as f32 / total);
            let chunk_end_x = left + pw * ((chunk_end_bid - start_view_id + 1) as f32 / total);
            let chunk_width = (chunk_end_x - chunk_start_x).max(1.0);
            let smooth_chunk =
                lod_bucket_ms <= 100 && should_smooth_chunk(chunk_width, chunk_bucket_count);

            let mut paths = vec![String::new(); valid_channels.len()];
            let mut gap_paths = vec![String::new(); valid_channels.len()];
            let mut segment_points: Vec<Vec<(f32, f32)>> = vec![Vec::new(); valid_channels.len()];
            let mut last_bucket_id_drawn: Vec<Option<i64>> = vec![None; valid_channels.len()];
            let mut last_point_drawn: Vec<Option<(f32, f32)>> = vec![None; valid_channels.len()];

            for b in &view_buckets {
                if b.id < chunk_start_bid || b.id > chunk_end_bid {
                    continue;
                }
                let rel_bid = b.id - chunk_start_bid;
                let x = chunk_width * ((rel_bid as f32 + 0.5) / chunk_bucket_count as f32);

                for (group_idx, &ch) in valid_channels.iter().enumerate() {
                    if !b.has[ch] {
                        continue;
                    }
                    let v = b.last[ch];
                    let (series_min, series_max) =
                        series_scales[group_idx].unwrap_or((global_min, global_max));
                    let y = bottom - (v - series_min) / (series_max - series_min) * ph;
                    if let Some(prev_bid) = last_bucket_id_drawn[group_idx] {
                        let gap_buckets = b.id - prev_bid;
                        if gap_buckets > 1
                            && let Some((prev_x, prev_y)) = last_point_drawn[group_idx]
                        {
                            if gap_buckets <= max_interp_gap_buckets {
                                let missing = gap_buckets - 1;
                                let interp_pts = missing.clamp(1, MAX_INTERP_POINTS_PER_GAP);
                                for j in 1..=interp_pts {
                                    let t = j as f32 / (interp_pts + 1) as f32;
                                    let xi = prev_x + (x - prev_x) * t;
                                    let yi = prev_y + (y - prev_y) * t;
                                    push_segment_point(&mut segment_points[group_idx], xi, yi);
                                }
                            } else {
                                flush_smoothed_segment(
                                    &mut paths[group_idx],
                                    &segment_points[group_idx],
                                    smooth_chunk,
                                );
                                segment_points[group_idx].clear();
                                gap_paths[group_idx].push_str(&format!(
                                    "M {:.2} {:.2} L {:.2} {:.2} ",
                                    prev_x, prev_y, x, y
                                ));
                            }
                        }
                    }

                    push_segment_point(&mut segment_points[group_idx], x, y);
                    last_bucket_id_drawn[group_idx] = Some(b.id);
                    last_point_drawn[group_idx] = Some((x, y));
                }
            }

            for group_idx in 0..valid_channels.len() {
                flush_smoothed_segment(
                    &mut paths[group_idx],
                    &segment_points[group_idx],
                    smooth_chunk,
                );
            }

            if paths.iter().all(|p| p.is_empty()) && gap_paths.iter().all(|p| p.is_empty()) {
                continue;
            }

            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            chunk_id.hash(&mut hasher);
            valid_channels.hash(&mut hasher);
            paths.hash(&mut hasher);
            gap_paths.hash(&mut hasher);

            chunks.push(ChartRenderChunk {
                id: chunk_id,
                x: chunk_start_x as f64,
                width: chunk_width as f64,
                right: chunk_end_x as f64,
                paths,
                gap_paths,
                signature: hasher.finish(),
                live: chunk_id == last_chunk_id,
            });
        }

        let cached = CachedSubsetPerSeries {
            chunks: Rc::new(chunks),
            series_scales: Rc::new(series_scales),
            span_min: self.span_min,
        };
        let result = (
            cached.chunks.clone(),
            cached.series_scales.clone(),
            cached.span_min,
        );
        self.subset_per_series_cache.insert(cache_key, cached);
        result
    }

    fn normalize_channels(&self, channels: &[usize]) -> Vec<usize> {
        channels
            .iter()
            .copied()
            .filter(|idx| *idx < self.channel_count)
            .collect()
    }
}

#[derive(Clone)]
struct CachedSubset {
    chunks: Rc<Vec<ChartRenderChunk>>,
    y_min: f32,
    y_max: f32,
    span_min: f32,
}

#[derive(Clone)]
struct CachedSubsetPerSeries {
    chunks: Rc<Vec<ChartRenderChunk>>,
    series_scales: Rc<Vec<Option<(f32, f32)>>>,
    span_min: f32,
}

#[derive(Clone, Eq, PartialEq, Hash)]
struct SubsetCacheKey {
    channels: Vec<usize>,
    width_px: u32,
    height_px: u32,
}

impl SubsetCacheKey {
    fn new(channels: &[usize], width: f32, height: f32) -> Self {
        Self {
            channels: channels.to_vec(),
            width_px: width.max(0.0).round() as u32,
            height_px: height.max(0.0).round() as u32,
        }
    }
}

fn zero_anchor_ratio(min: f32, max: f32) -> f32 {
    let neg = (-min).max(0.0);
    let pos = max.max(0.0);
    let total = neg + pos;
    if total <= 1e-6 {
        0.5
    } else {
        (neg / total).clamp(0.0, 1.0)
    }
}

fn anchored_series_range(min: f32, max: f32, zero_ratio: f32) -> (f32, f32) {
    let neg_needed = (-min).max(0.0);
    let pos_needed = max.max(0.0);
    let ratio = zero_ratio.clamp(0.05, 0.95);
    let span_from_neg = if ratio > 1e-6 {
        neg_needed / ratio
    } else {
        0.0
    };
    let span_from_pos = if (1.0 - ratio) > 1e-6 {
        pos_needed / (1.0 - ratio)
    } else {
        0.0
    };
    let mut span = span_from_neg.max(span_from_pos).max(1.0);
    if !span.is_finite() {
        span = 1.0;
    }
    let padded_span = span * 1.06_f32;
    (-padded_span * ratio, padded_span * (1.0 - ratio))
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

static NEXT_CANVAS_ID: AtomicU64 = AtomicU64::new(1);

fn should_smooth_chunk(chunk_width: f32, chunk_bucket_count: i64) -> bool {
    chunk_width >= 220.0 && chunk_bucket_count <= SMOOTHING_MAX_POINTS_PER_SEGMENT as i64
}

fn flush_smoothed_segment(path: &mut String, points: &[(f32, f32)], smooth: bool) {
    if points.is_empty() {
        return;
    }

    let (x0, y0) = points[0];
    path.push_str(&format!("M {:.2} {:.2} ", x0, y0));

    if points.len() == 1 {
        return;
    }

    if points.len() == 2 || !smooth || points.len() > SMOOTHING_MAX_POINTS_PER_SEGMENT {
        for &(x, y) in &points[1..] {
            path.push_str(&format!("L {:.2} {:.2} ", x, y));
        }
        return;
    }

    for i in 1..(points.len() - 1) {
        let (cx, cy) = points[i];
        let (nx, ny) = points[i + 1];
        let mx = (cx + nx) * 0.5;
        let my = (cy + ny) * 0.5;
        path.push_str(&format!("Q {:.2} {:.2} {:.2} {:.2} ", cx, cy, mx, my));
    }

    let (xl, yl) = points[points.len() - 1];
    path.push_str(&format!("L {:.2} {:.2} ", xl, yl));
}

fn push_segment_point(points: &mut Vec<(f32, f32)>, x: f32, y: f32) {
    if let Some((px, py)) = points.last().copied()
        && (px - x).abs() < CURVE_MIN_DELTA_PX
        && (py - y).abs() < CURVE_MIN_DELTA_PX
    {
        return;
    }
    points.push((x, y));
}

#[derive(Clone, PartialEq, Serialize)]
pub struct ChartRenderChunk {
    pub id: i64,
    pub x: f64,
    pub width: f64,
    pub right: f64,
    pub paths: Vec<String>,
    pub gap_paths: Vec<String>,
    pub signature: u64,
    pub live: bool,
}

#[derive(Serialize)]
struct CanvasChartPayload<'a> {
    view_w: f64,
    view_h: f64,
    chunks: &'a [ChartRenderChunk],
    colors: Vec<&'static str>,
    grid_left: Option<f64>,
    grid_right: Option<f64>,
    grid_top: Option<f64>,
    grid_bottom: Option<f64>,
    signature: u64,
}

#[component]
pub fn ChartCanvas(
    view_w: f64,
    view_h: f64,
    chunks: Rc<Vec<ChartRenderChunk>>,
    grid_left: Option<f64>,
    grid_right: Option<f64>,
    grid_top: Option<f64>,
    grid_bottom: Option<f64>,
    style: String,
) -> Element {
    let canvas_id = use_hook(|| {
        format!(
            "gs26-chart-canvas-{}",
            NEXT_CANVAS_ID.fetch_add(1, Ordering::Relaxed)
        )
    });

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    view_w.to_bits().hash(&mut hasher);
    view_h.to_bits().hash(&mut hasher);
    for chunk in chunks.iter() {
        chunk.id.hash(&mut hasher);
        chunk.signature.hash(&mut hasher);
        chunk.x.to_bits().hash(&mut hasher);
        chunk.width.to_bits().hash(&mut hasher);
        chunk.right.to_bits().hash(&mut hasher);
    }

    let payload = CanvasChartPayload {
        view_w,
        view_h,
        colors: (0..8).map(series_color).collect(),
        grid_left,
        grid_right,
        grid_top,
        grid_bottom,
        chunks: chunks.as_slice(),
        signature: hasher.finish(),
    };
    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let id_json = serde_json::to_string(&canvas_id).unwrap_or_else(|_| "\"\"".to_string());

    let draw_js = format!(
        r##"
                (function() {{
                  const canvasId = {id_json};
                  const data = {payload_json};
                  const isWindows = /Windows/i.test(navigator.userAgent || navigator.platform || "");
                  const cacheRoot = window.__gs26ChartCanvasCache || (window.__gs26ChartCanvasCache = new Map());
                  const draw = () => {{
                    const el = document.getElementById(canvasId);
                    if (!el) return;

                    const rect = el.getBoundingClientRect();
                    const cssW = Math.max(1, Math.round(rect.width || data.view_w || 1));
                    const cssH = Math.max(1, Math.round(rect.height || data.view_h || 1));
                    const mobilePlatform = /iPhone|iPad|iPod|Android/i.test(navigator.userAgent || navigator.platform || "");
                    const maxDpr = mobilePlatform ? 3 : 5;
                    const dpr = Math.max(1, Math.min(maxDpr, (window.devicePixelRatio || 1)));
                    const qualityBoost = 1.0;
                    const maxCanvasEdge = 16384;
                    let renderScale = dpr * qualityBoost;
                    if (cssW * renderScale > maxCanvasEdge) {{
                      renderScale = Math.min(renderScale, maxCanvasEdge / Math.max(1, cssW));
                    }}
                    if (cssH * renderScale > maxCanvasEdge) {{
                      renderScale = Math.min(renderScale, maxCanvasEdge / Math.max(1, cssH));
                    }}
                    renderScale = Math.max(1, renderScale);
                    const pxW = Math.max(1, Math.round(cssW * renderScale));
                    const pxH = Math.max(1, Math.round(cssH * renderScale));

                    if (el.width !== pxW) el.width = pxW;
                    if (el.height !== pxH) el.height = pxH;

                  const get2d = (canvas) => {{
                    return canvas.getContext("2d", isWindows
                      ? {{ alpha: true }}
                      : {{ alpha: true, desynchronized: true }});
                  }};
                  const ctx = get2d(el);
                  if (!ctx) return;
                  function buildPath2d(path) {{
                    if (!path) return null;
                    const tokens = path.trim().split(/[ \t\r\n]+/);
                    if (!tokens.length) return null;
                    const p = new Path2D();
                    let mode = "";
                    for (let i = 0; i < tokens.length; ) {{
                      const tok = tokens[i];
                      if (tok === "M" || tok === "L" || tok === "Q") {{
                        mode = tok;
                        i += 1;
                        continue;
                      }}
                      const x = Number(tok);
                      const y = Number(tokens[i + 1]);
                      if (!Number.isFinite(x) || !Number.isFinite(y)) break;
                      if (mode === "M") {{
                        p.moveTo(x, y);
                        mode = "L";
                      }} else if (mode === "Q") {{
                        const cpx = x;
                        const cpy = y;
                        const qx = Number(tokens[i + 2]);
                        const qy = Number(tokens[i + 3]);
                        if (!Number.isFinite(qx) || !Number.isFinite(qy)) break;
                        p.quadraticCurveTo(cpx, cpy, qx, qy);
                        i += 4;
                        continue;
                      }} else {{
                        p.lineTo(x, y);
                      }}
                      i += 2;
                    }}
                    return p;
                  }}
                  const left = Number.isFinite(data.grid_left) ? data.grid_left : {chart_grid_left};
                  const right = Number.isFinite(data.grid_right) ? data.grid_right : (data.view_w - {chart_grid_right_pad});
                  const top = Number.isFinite(data.grid_top) ? data.grid_top : {chart_grid_top};
                  const bottom = Number.isFinite(data.grid_bottom) ? data.grid_bottom : (data.view_h - {chart_grid_bottom_pad});
                  const drawGrid = (targetCtx, widthPx, heightPx) => {{
                    if (typeof targetCtx.resetTransform === "function") {{
                      targetCtx.resetTransform();
                    }} else {{
                      targetCtx.setTransform(1, 0, 0, 1, 0, 0);
                    }}
                    targetCtx.clearRect(0, 0, widthPx, heightPx);
                    targetCtx.scale(widthPx / data.view_w, heightPx / data.view_h);

                    const gridXStep = (right - left) / 6.0;
                    const gridYStep = (bottom - top) / 6.0;

                    targetCtx.save();
                    targetCtx.strokeStyle = "#1f2937";
                    targetCtx.lineWidth = 1;
                    for (let i = 1; i <= 5; i += 1) {{
                      const y = top + gridYStep * i;
                      targetCtx.beginPath();
                      targetCtx.moveTo(left, y);
                      targetCtx.lineTo(right, y);
                      targetCtx.stroke();
                    }}
                    for (let i = 1; i <= 5; i += 1) {{
                      const x = left + gridXStep * i;
                      targetCtx.beginPath();
                      targetCtx.moveTo(x, top);
                      targetCtx.lineTo(x, bottom);
                      targetCtx.stroke();
                    }}

                    targetCtx.strokeStyle = "#334155";
                    targetCtx.beginPath();
                    targetCtx.moveTo(left, top);
                    targetCtx.lineTo(left, bottom);
                    targetCtx.lineTo(right, bottom);
                    targetCtx.stroke();
                    targetCtx.restore();
                    if (typeof targetCtx.resetTransform === "function") {{
                      targetCtx.resetTransform();
                    }} else {{
                      targetCtx.setTransform(1, 0, 0, 1, 0, 0);
                    }}
                  }};
                  const drawChunkDirect = (targetCtx, chunk, destX, destW) => {{
                    targetCtx.save();
                    targetCtx.translate(destX, 0);
                    targetCtx.scale(destW / Math.max(1, chunk.width), pxH / data.view_h);
                    targetCtx.imageSmoothingEnabled = true;
                    for (let i = 0; i < chunk.paths.length; i += 1) {{
                      const path2d = buildPath2d(chunk.paths[i]);
                      if (!path2d) continue;
                      targetCtx.strokeStyle = data.colors[i] || "#9ca3af";
                      targetCtx.lineWidth = 2.25;
                      targetCtx.lineJoin = "round";
                      targetCtx.lineCap = "round";
                      targetCtx.stroke(path2d);
                    }}
                    for (let i = 0; i < chunk.gap_paths.length; i += 1) {{
                      const path2d = buildPath2d(chunk.gap_paths[i]);
                      if (!path2d) continue;
                      targetCtx.save();
                      targetCtx.strokeStyle = data.colors[i] || "#9ca3af";
                      targetCtx.globalAlpha = 0.7;
                      targetCtx.lineWidth = 2.0;
                      targetCtx.lineJoin = "round";
                      targetCtx.lineCap = "round";
                      targetCtx.setLineDash([7, 6]);
                      targetCtx.stroke(path2d);
                      targetCtx.restore();
                    }}
                    targetCtx.restore();
                  }};
                  let cache = cacheRoot.get(canvasId);
                  const cacheMiss = !cache
                      || cache.signature !== data.signature
                      || cache.pxW !== pxW
                      || cache.pxH !== pxH;

                    if (isWindows) {{
                      if (typeof ctx.resetTransform === "function") {{
                        ctx.resetTransform();
                      }} else {{
                        ctx.setTransform(1, 0, 0, 1, 0, 0);
                      }}
                      ctx.clearRect(0, 0, el.width, el.height);
                      ctx.imageSmoothingEnabled = true;
                      drawGrid(ctx, pxW, pxH);
                      const scaleX = pxW / data.view_w;
                      const firstChunk = data.chunks.length ? data.chunks[0] : null;
                      const alignOffset = firstChunk
                        ? Math.round(firstChunk.x * scaleX) - (firstChunk.x * scaleX)
                        : 0;
                      for (let i = 0; i < data.chunks.length; i += 1) {{
                        const chunk = data.chunks[i];
                        const next = i + 1 < data.chunks.length ? data.chunks[i + 1] : null;
                        const destX = Math.round(chunk.x * scaleX + alignOffset);
                        const rawRight = next
                          ? Math.round(next.x * scaleX + alignOffset)
                          : Math.round(chunk.right * scaleX + alignOffset);
                        const destRight = Math.max(destX + 1, rawRight);
                        const destW = Math.max(1, destRight - destX);
                        drawChunkDirect(ctx, chunk, destX, destW);
                      }}
                      return;
                    }}

                    if (cacheMiss) {{
                      const gridBuffer = document.createElement("canvas");
                      gridBuffer.width = pxW;
                      gridBuffer.height = pxH;
                      const bctx = get2d(gridBuffer);
                      if (!bctx) return;
                      drawGrid(bctx, gridBuffer.width, gridBuffer.height);

                      cache = {{
                        signature: data.signature,
                        pxW,
                        pxH,
                        gridBuffer,
                        // Do not carry old per-signature chunk buffers forward indefinitely.
                        // The live chart signature changes often, and reusing the previous map
                        // causes unbounded canvas growth in the browser over long runs.
                        chunkCache: new Map(),
                        historyBuffer: null,
                        historyKey: null,
                      }};
                      cacheRoot.set(canvasId, cache);
                    }}

                    function buildChunkBuffer(chunk, destW) {{
                      const key = `${{chunk.id}}:${{chunk.signature}}:${{pxH}}:${{destW}}`;
                      let chunkBuffer = cache.chunkCache.get(key);
                      if (chunkBuffer) return chunkBuffer;

                      const widthPx = Math.max(1, destW);
                      const buffer = document.createElement("canvas");
                      buffer.width = widthPx;
                      buffer.height = pxH;
                      const bctx = get2d(buffer);
                      if (!bctx) return null;
                      if (typeof bctx.resetTransform === "function") {{
                        bctx.resetTransform();
                      }} else {{
                        bctx.setTransform(1, 0, 0, 1, 0, 0);
                      }}
                      bctx.clearRect(0, 0, buffer.width, buffer.height);
                      drawChunkDirect(bctx, chunk, 0, widthPx);

                      chunkBuffer = buffer;
                      cache.chunkCache.set(key, chunkBuffer);
                      return chunkBuffer;
                    }}

                    if (typeof ctx.resetTransform === "function") {{
                      ctx.resetTransform();
                    }} else {{
                      ctx.setTransform(1, 0, 0, 1, 0, 0);
                    }}
                    ctx.clearRect(0, 0, el.width, el.height);
                    ctx.imageSmoothingEnabled = true;
                    ctx.drawImage(cache.gridBuffer, 0, 0);
                    const scaleX = pxW / data.view_w;
                    const firstChunk = data.chunks.length ? data.chunks[0] : null;
                    const alignOffset = firstChunk
                      ? Math.round(firstChunk.x * scaleX) - (firstChunk.x * scaleX)
                      : 0;
                    const historyKey = data.chunks
                      .filter(chunk => !chunk.live)
                      .map(chunk => `${{chunk.id}}:${{chunk.signature}}:${{chunk.x.toFixed(3)}}:${{chunk.right.toFixed(3)}}`)
                      .join("|");
                    if (!cache.historyBuffer || cache.historyKey !== historyKey) {{
                      const historyBuffer = document.createElement("canvas");
                      historyBuffer.width = pxW;
                      historyBuffer.height = pxH;
                      const hctx = historyBuffer.getContext("2d", {{ alpha: true, desynchronized: true }});
                      if (!hctx) return;
                      hctx.clearRect(0, 0, historyBuffer.width, historyBuffer.height);
                      hctx.imageSmoothingEnabled = true;
                      for (let i = 0; i < data.chunks.length; i += 1) {{
                        const chunk = data.chunks[i];
                        if (chunk.live) continue;
                        const next = i + 1 < data.chunks.length ? data.chunks[i + 1] : null;
                        const destX = Math.round(chunk.x * scaleX + alignOffset);
                        const rawRight = next
                          ? Math.round(next.x * scaleX + alignOffset)
                          : Math.round(chunk.right * scaleX + alignOffset);
                        const destRight = Math.max(destX + 1, rawRight);
                        const destW = Math.max(1, destRight - destX);
                        const chunkBuffer = buildChunkBuffer(chunk, destW);
                        if (!chunkBuffer) continue;
                        hctx.drawImage(chunkBuffer, destX, 0, destW, pxH);
                      }}
                      cache.historyBuffer = historyBuffer;
                      cache.historyKey = historyKey;
                    }}
                    if (cache.historyBuffer) {{
                      ctx.drawImage(cache.historyBuffer, 0, 0);
                    }}
                    for (let i = 0; i < data.chunks.length; i += 1) {{
                      const chunk = data.chunks[i];
                      if (!chunk.live) continue;
                      const next = i + 1 < data.chunks.length ? data.chunks[i + 1] : null;
                      const destX = Math.round(chunk.x * scaleX + alignOffset);
                      const rawRight = next
                        ? Math.round(next.x * scaleX + alignOffset)
                        : Math.round(chunk.right * scaleX + alignOffset);
                      const destRight = Math.max(destX + 1, rawRight);
                      const destW = Math.max(1, destRight - destX);
                      const chunkBuffer = buildChunkBuffer(chunk, destW);
                      if (!chunkBuffer) continue;
                      ctx.drawImage(chunkBuffer, destX, 0, destW, pxH);
                    }}
                  }};

                  if (typeof requestAnimationFrame === "function") {{
                    requestAnimationFrame(draw);
                  }} else {{
                    setTimeout(draw, 0);
                  }}
                }})();
        "##,
        chart_grid_left = CHART_GRID_LEFT,
        chart_grid_right_pad = CHART_GRID_RIGHT_PAD,
        chart_grid_top = CHART_GRID_TOP,
        chart_grid_bottom_pad = CHART_GRID_BOTTOM_PAD,
    );

    super::js_eval(&draw_js);

    rsx! {
        canvas {
            id: "{canvas_id}",
            width: "{view_w.round() as u32}",
            height: "{view_h.round() as u32}",
            style: "{style}",
        }
    }
}
