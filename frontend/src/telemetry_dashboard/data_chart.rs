use dioxus::prelude::*;
use groundstation_shared::TelemetryRow;

use super::HISTORY_MS;

pub fn data_style_chart(
    rows: &[TelemetryRow],
    data_type: &str,
    height: f64,
    title: Option<&str>,
) -> Element {
    let mut tab_rows: Vec<TelemetryRow> = rows
        .iter()
        .filter(|r| r.data_type == data_type)
        .cloned()
        .collect();
    tab_rows.sort_by_key(|r| r.timestamp_ms);

    let view_w = 1200.0_f64;
    let view_h = height.max(220.0);
    let left = 60.0_f64;
    let right = view_w - 20.0_f64;
    let pad_top = 20.0_f64;
    let pad_bottom = 20.0_f64;
    let inner_w = right - left;
    let grid_x_step = inner_w / 6.0_f64;

    let inner_h = view_h - pad_top - pad_bottom;
    let grid_y_step = inner_h / 6.0_f64;

    let (paths, y_min, y_max, span_min) =
        build_polylines(&tab_rows, view_w as f32, view_h as f32);
    let y_mid = (y_min + y_max) * 0.5;

    let labels = labels_for_datatype(data_type);
    let legend_items: Vec<(usize, &'static str)> = labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| if l.is_empty() { None } else { Some((i, *l)) })
        .collect();
    let legend_rows: Vec<(usize, &'static str)> =
        legend_items.iter().map(|(i, label)| (*i, *label)).collect();

    rsx! {
        div { style: "width:100%; background:#020617; border-radius:14px; border:1px solid #334155; padding:12px; display:flex; flex-direction:column; gap:8px;",
            if let Some(title) = title {
                div { style:"color:#94a3b8; font-size:12px; margin-bottom:2px;", "{title}" }
            }
            svg {
                style: "width:100%; height:auto; display:block;",
                view_box: "0 0 {view_w} {view_h}",

                // gridlines
                for i in 1..=5 {
                    line {
                        x1:"{left}", y1:"{pad_top + grid_y_step * (i as f64)}",
                        x2:"{right}", y2:"{pad_top + grid_y_step * (i as f64)}",
                        stroke:"#1f2937", "stroke-width":"1"
                    }
                }
                for i in 1..=5 {
                    line {
                        x1:"{left + grid_x_step * (i as f64)}", y1:"{pad_top}",
                        x2:"{left + grid_x_step * (i as f64)}", y2:"{view_h - pad_bottom}",
                        stroke:"#1f2937", "stroke-width":"1"
                    }
                }

                // axes
                line { x1:"{left}", y1:"{pad_top}",  x2:"{left}",   y2:"{view_h - pad_bottom}", stroke:"#334155", stroke_width:"1" }
                line { x1:"{left}", y1:"{view_h - pad_bottom}", x2:"{right}", y2:"{view_h - pad_bottom}", stroke:"#334155", stroke_width:"1" }

                // y labels
                text { x:"10", y:"{pad_top + 6.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_max)} }
                text { x:"10", y:"{pad_top + inner_h / 2.0 + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_mid)} }
                text { x:"10", y:"{view_h - pad_bottom + 4.0}", fill:"#94a3b8", "font-size":"10", {format!("{:.2}", y_min)} }

                // x labels (span in minutes)
                text { x:"{left + 10.0}",   y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min)} }
                text { x:"{view_w * 0.5}",  y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", {format!("-{:.1} min", span_min * 0.5)} }
                text { x:"{right - 60.0}", y:"{view_h - 5.0}", fill:"#94a3b8", "font-size":"10", "now" }

                // series
                for (i, pts) in paths.iter().enumerate() {
                    if !pts.is_empty() {
                        polyline {
                            points: "{pts}",
                            fill: "none",
                            stroke: "{series_color(i)}",
                            stroke_width: "2",
                            stroke_linejoin: "round",
                            stroke_linecap: "round",
                        }
                    }
                }
            }

            if !legend_rows.is_empty() {
                div { style: "display:flex; flex-wrap:wrap; gap:8px; padding:6px 10px; background:rgba(2,6,23,0.75); border:1px solid #1f2937; border-radius:10px;",
                    for (i, label) in legend_rows.iter() {
                        div { style: "display:flex; align-items:center; gap:6px; font-size:12px; color:#cbd5f5;",
                            svg { width:"26", height:"8", view_box:"0 0 26 8",
                                line { x1:"1", y1:"4", x2:"25", y2:"4", stroke:"{series_color(*i)}", stroke_width:"2", stroke_linecap:"round" }
                            }
                            "{label}"
                        }
                    }
                }
            }
        }
    }
}

pub fn series_color(i: usize) -> &'static str {
    match i {
        0 => "#f97316",
        1 => "#22d3ee",
        2 => "#a3e635",
        3 => "#f43f5e",
        4 => "#8b5cf6",
        5 => "#e879f9",
        6 => "#10b981",
        7 => "#fbbf24",
        _ => "#9ca3af",
    }
}

pub fn labels_for_datatype(dt: &str) -> [&'static str; 8] {
    match dt {
        "GYRO_DATA" => ["Roll", "Pitch", "Yaw", "", "", "", "", ""],
        "ACCEL_DATA" => ["X Accel", "Y Accel", "Z Accel", "", "", "", "", ""],
        "BAROMETER_DATA" => ["Pressure", "Temp", "Altitude", "", "", "", "", ""],
        "BATTERY_VOLTAGE" => ["Voltage", "", "", "", "", "", "", ""],
        "BATTERY_CURRENT" => ["Current", "", "", "", "", "", "", ""],
        "KALMAN_FILTER_DATA" => ["X", "Y", "Z", "", "", "", "", ""],
        "GPS_DATA" => ["Latitude", "Longitude", "", "", "", "", "", ""],
        "FUEL_FLOW" => ["Flow Rate", "", "", "", "", "", "", ""],
        "FUEL_TANK_PRESSURE" => ["Pressure", "", "", "", "", "", "", ""],
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

/// Build eight SVG polyline point strings (v0..v7),
/// plus y-min, y-max, and span_minutes (0-HISTORY_MS).
///
/// `paths[i]` is `"x,y x,y x,y ..."`, suitable for `<polyline points=... />`.
pub fn build_polylines(rows: &[TelemetryRow], width: f32, height: f32) -> ([String; 8], f32, f32, f32) {
    if rows.is_empty() {
        return (std::array::from_fn(|_| String::new()), 0.0, 1.0, 0.0);
    }

    // 1) time window & span
    let newest_ts = rows.iter().map(|r| r.timestamp_ms).max().unwrap_or(0);
    let oldest_ts = rows
        .iter()
        .map(|r| r.timestamp_ms)
        .min()
        .unwrap_or(newest_ts);

    let raw_span_ms = (newest_ts - oldest_ts).max(1);
    let effective_span_ms = raw_span_ms.min(HISTORY_MS);
    let span_minutes = effective_span_ms as f32 / 60_000.0;

    let window_start = newest_ts.saturating_sub(effective_span_ms);

    // 2) rows in window
    let mut window_rows: Vec<&TelemetryRow> = rows
        .iter()
        .filter(|r| r.timestamp_ms >= window_start)
        .collect();
    if window_rows.is_empty() {
        return (
            std::array::from_fn(|_| String::new()),
            0.0,
            1.0,
            span_minutes,
        );
    }
    window_rows.sort_by_key(|r| r.timestamp_ms);

    // 3) min/max across windowed rows
    let mut min_v: Option<f32> = None;
    let mut max_v: Option<f32> = None;

    for r in &window_rows {
        for x in [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7]
            .into_iter()
            .flatten()
        {
            min_v = Some(min_v.map(|m| m.min(x)).unwrap_or(x));
            max_v = Some(max_v.map(|m| m.max(x)).unwrap_or(x));
        }
    }

    let (min_v, mut max_v) = match (min_v, max_v) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return (
                std::array::from_fn(|_| String::new()),
                0.0,
                1.0,
                span_minutes,
            );
        }
    };

    if (max_v - min_v).abs() < 1e-6 {
        max_v = min_v + 1.0;
    }

    // 4) plot geometry
    let left = 60.0;
    let right = width - 20.0;
    let top = 20.0;
    let bottom = height - 20.0;

    let plot_w = right - left;
    let plot_h = bottom - top;

    let map_y = |v: f32| bottom - ((v - min_v) / (max_v - min_v)) * plot_h;

    // 5) downsample into fixed buckets (constant x-density while preserving extrema)
    let max_points: usize = 1200;

    #[derive(Clone)]
    struct BucketAcc {
        min_v: [f64; 8],
        max_v: [f64; 8],
        last_v: [f64; 8],
        has: [bool; 8],
    }

    impl Default for BucketAcc {
        fn default() -> Self {
            Self {
                min_v: [0.0; 8],
                max_v: [0.0; 8],
                last_v: [0.0; 8],
                has: [false; 8],
            }
        }
    }

    let mut buckets = vec![BucketAcc::default(); max_points];
    let total = window_rows.len().max(1);

    for (idx, r) in window_rows.iter().enumerate() {
        let mut bi = (idx * max_points) / total;
        if bi >= max_points {
            bi = max_points - 1;
        }
        let b = &mut buckets[bi];

        let vals = [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7];
        for (j, opt) in vals.iter().enumerate() {
            if let Some(x) = opt {
                let xf = *x as f64;
                if !b.has[j] {
                    b.has[j] = true;
                    b.min_v[j] = xf;
                    b.max_v[j] = xf;
                    b.last_v[j] = xf;
                } else {
                    if xf < b.min_v[j] {
                        b.min_v[j] = xf;
                    }
                    if xf > b.max_v[j] {
                        b.max_v[j] = xf;
                    }
                    b.last_v[j] = xf;
                }
            }
        }
    }

    // 6) build polyline strings with constant x-spacing
    let mut out: [String; 8] = std::array::from_fn(|_| String::new());

    let mut last_seen: [Option<f32>; 8] = [None, None, None, None, None, None, None, None];

    for (idx, b) in buckets.iter().enumerate() {
        let center_x = left + plot_w * ((idx as f32 + 0.5) / max_points as f32);
        for ch in 0..8usize {
            let s = &mut out[ch];

            let mut push_point = |x: f32, y: f32| {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format!("{x:.2},{y:.2}"));
            };

            if b.has[ch] {
                let min_v = b.min_v[ch] as f32;
                let max_v = b.max_v[ch] as f32;

                if (min_v - max_v).abs() < f32::EPSILON {
                    push_point(center_x, map_y(min_v));
                } else {
                    push_point(center_x, map_y(min_v));
                    push_point(center_x, map_y(max_v));
                }

                last_seen[ch] = Some(b.last_v[ch] as f32);
            } else if let Some(v) = last_seen[ch] {
                push_point(center_x, map_y(v));
            }
        }
    }

    (out, min_v, max_v, span_minutes)
}
