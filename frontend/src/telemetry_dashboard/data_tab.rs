use groundstation_shared::TelemetryRow;
use leptos::prelude::*;

use super::HISTORY_MS;

#[component]
pub fn DataTab(
    rows: Signal<Vec<TelemetryRow>>,
    active_tab: Signal<String>,
    set_active_tab: WriteSignal<String>,
) -> impl IntoView {
    // Rows for the selected sensor type, sorted by time
    let tab_rows = Signal::derive(move || {
        let kind = active_tab.get();
        rows.get()
            .into_iter()
            .filter(|r| r.data_type == kind)
            .collect::<Vec<_>>()
    });

    // Latest row for summary cards
    let latest_row = Signal::derive(move || tab_rows.get().last().cloned());

    // Build SVG data: 7 paths + extra + y-scale + span
    let graph_data = Signal::derive(move || {
        let data = tab_rows.get();
        build_polyline(&data, 1200.0, 360.0)
    });

    // v_paths: each signal only clones its one String
    let v_paths: [Signal<String>; 8] = std::array::from_fn(|i| {
        let graph_data = graph_data.clone();
        Signal::derive(move || {
            graph_data.with(
                |(p0, p1, p2, p3, p4, p5, p6, p7, _ymin, _ymax, _span)| match i {
                    0 => p0.clone(),
                    1 => p1.clone(),
                    2 => p2.clone(),
                    3 => p3.clone(),
                    4 => p4.clone(),
                    5 => p5.clone(),
                    6 => p6.clone(),
                    7 => p7.clone(),
                    _ => unreachable!(),
                },
            )
        })
    });

    // Scaling values: no String clones, just copy f32s
    let y_min = Signal::derive({
        let graph_data = graph_data.clone();
        move || graph_data.with(|(_, _, _, _, _, _, _, _, ymin, _, _)| *ymin)
    });

    let y_max = Signal::derive({
        let graph_data = graph_data.clone();
        move || graph_data.with(|(_, _, _, _, _, _, _, _, _, ymax, _)| *ymax)
    });

    let span_min = Signal::derive({
        let graph_data = graph_data.clone();
        move || graph_data.with(|(_, _, _, _, _, _, _, _, _, _, span)| *span)
    });

    // y_mid still just uses the two f32 signals
    let y_mid = Signal::derive(move || {
        let (lo, hi) = (y_min.get(), y_max.get());
        (lo + hi) * 0.5
    });

    let fmt_opt = |v: Option<f32>| {
        v.map(|x| format!("{x:.2}"))
            .unwrap_or_else(|| "-".to_string())
    };

    fn labels_for_datatype(dt: &str) -> [&'static str; 8] {
        match dt {
            "GYRO_DATA" => ["Roll", "Pitch", "Yaw", "", "", "", "", ""],
            "ACCEL_DATA" => ["X Accel", "Y Accel", "Z Accel", "", "", "", "", ""],
            "BAROMETER_DATA" => ["Pressure", "Temp", "Altitude", "", "", "", "", ""],
            "BATTERY_VOLTAGE" => ["Voltage", "", "", "", "", "", "", ""],
            "BATTERY_CURRENT" => ["Current", "", "", "", "", "", "", ""],
            "GPS_DATA" => ["Latitude", "Longitude", "", "", "", "", "", ""],
            "FUEL_FLOW" => ["Flow Rate", "", "", "", "", "", "", ""],
            "FUEL_TANK_PRESSURE" => ["Pressure", "", "", "", "", "", "", ""],
            _ => ["", "", "", "", "", "", "", ""],
        }
    }

    view! {
        {/* Top row: tabs + summary cards + command buttons */}
        <div style="
            display:flex;
            flex-wrap:wrap;
            gap:1rem;
            align-items:flex-start;
            margin-bottom: 1 rem;
        ">
            {/* Tabs */}
            <nav style="display:flex; flex-wrap:wrap; gap:0.5rem;">
                {sensor_tab("GYRO_DATA", "Gyro", active_tab, set_active_tab)}
                {sensor_tab("ACCEL_DATA", "Accel", active_tab, set_active_tab)}
                {sensor_tab("BAROMETER_DATA", "Barom", active_tab, set_active_tab)}
                {sensor_tab("BATTERY_VOLTAGE", "Batt V", active_tab, set_active_tab)}
                {sensor_tab("BATTERY_CURRENT", "Batt I", active_tab, set_active_tab)}
                {sensor_tab("GPS_DATA", "GPS", active_tab, set_active_tab)}
                {sensor_tab("FUEL_FLOW", "Fuel Flow", active_tab, set_active_tab)}
                {sensor_tab("FUEL_TANK_PRESSURE", "Fuel Press", active_tab, set_active_tab)}
            </nav>

            {/* Summary cards */}
            <Show
                when=move || latest_row.get().is_some()
                fallback=move || view! {
                    <p style="color:#9ca3af; margin-left:1rem;">"Waiting for telemetry…"</p>
                }
            >
                {move || {
                    latest_row.get().map(|row| {
                        let labels = labels_for_datatype(&active_tab.get());

                        let fields: [(&str, Option<f32>, &str); 8] = [
                            (labels[0], row.v0, "#f97316"),
                            (labels[1], row.v1, "#22d3ee"),
                            (labels[2], row.v2, "#a3e635"),
                            (labels[3], row.v3, "#9ca3af"),
                            (labels[4], row.v4, "#9ca3af"),
                            (labels[5], row.v5, "#9ca3af"),
                            (labels[6], row.v6, "#9ca3af"),
                            (labels[7], row.v7, "#9ca3af"),
                        ];

                        let cards = fields
                            .iter()
                            .filter_map(|(label, value, color)| {
                                value.map(|v| {
                                    view! {
                                        <SummaryCard
                                            label=*label
                                            value=fmt_opt(Some(v))
                                            color=*color
                                        />
                                    }
                                })
                            })
                            .collect::<Vec<_>>();

                        view! {
                            <div style="display:flex; gap:0.75rem; margin-left:1rem;">
                                {cards}
                            </div>
                        }
                    }).into_view()
                }}
            </Show>
        </div>

        {/* BIG centered graph – main focus */}
        <div style="
            flex: 1;
            display:flex;
            align-items:center;
            justify-content:center;
            margin-bottom: 1.5rem;
        ">
            <div style="
                width: 100%;
                max-width: 1200px;
            ">
                <svg
                    viewBox="0 0 1200 360"
                    width="100%"
                    height="min(60vh, 420px)"
                    style="
                        display:block;
                        margin:0 auto;
                        border:1px solid #4b5563;
                        background:#020617;
                    "
                >
                    {/* Axes */}
                    <line x1="60" y1="20"  x2="60"  y2="340" stroke="#4b5563" stroke-width="1"/>
                    <line x1="60" y1="340" x2="1180" y2="340" stroke="#4b5563" stroke-width="1"/>

                    {/* Y-axis labels */}
                    <text x="10" y="26"  fill="#9ca3af" font-size="10">
                        {move || format!("{:.2}", y_max.get())}
                    </text>
                    <text x="10" y="184" fill="#9ca3af" font-size="10">
                        {move || format!("{:.2}", y_mid.get())}
                    </text>
                    <text x="10" y="344" fill="#9ca3af" font-size="10">
                        {move || format!("{:.2}", y_min.get())}
                    </text>

                    {/* X-axis labels: dynamic span, capped at 20 min */}
                    <text x="70"   y="355" fill="#9ca3af" font-size="10">
                        {move || {
                            let span = span_min.get(); // minutes, may be < 20
                            format!("-{:.1} min", span)
                        }}
                    </text>
                    <text x="600"  y="355" fill="#9ca3af" font-size="10">
                        {move || {
                            let span = span_min.get() / 2.0;
                            format!("-{:.1} min", span)
                        }}
                    </text>
                    <text x="1120" y="355" fill="#9ca3af" font-size="10">
                        "now"
                    </text>

                    {
                    let colors = [
                        "#f97316", // v0
                        "#22d3ee", // v1
                        "#a3e635", // v2
                        "#a3e635", // v3
                        "#a3e635", // v4
                        "#a3e635", // v5
                        "#a3e635", // v6
                        "#a3e547", // v7
                    ];

                    v_paths
                        .iter()
                        .enumerate()
                        .map(|(i, path_sig)| {
                            let color = colors[i];
                            let sig = *path_sig; // deref & copy the Signal

                            view! {
                                <path
                                    d=move || sig.get()
                                    stroke=color
                                    fill="none"
                                    stroke-width="2"
                                />
                            }
                        })
                        .collect_view()
                    }
                </svg>
            </div>
        </div>
    }
}

#[component]
fn SummaryCard(label: &'static str, value: String, color: &'static str) -> impl IntoView {
    view! {
        <div style="
            padding:0.75rem;
            border-radius:0.5rem;
            background:#0f172a;
            border:1px solid #4b5563;
            min-width:90px;
        ">
            <div style=format!("font-size:0.75rem; color:{};", color)>
                {label}
            </div>
            <div style="font-size:1.25rem;">
                {value}
            </div>
        </div>
    }
}

fn sensor_tab(
    tag: &'static str,
    label: &'static str,
    active: Signal<String>,
    set: WriteSignal<String>,
) -> impl IntoView {
    view! {
        <button
            style=move || {
                if active.get() == tag {
                    "padding:0.4rem 0.8rem; border-radius:0.5rem; \
                     border:1px solid #f97316; background:#111827; \
                     color:#f97316; cursor:pointer;"
                } else {
                    "padding:0.4rem 0.8rem; border-radius:0.5rem; \
                     border:1px solid #4b5563; background:#020617; \
                     color:#e5e7eb; cursor:pointer;"
                }
            }
            on:click=move |_| set.set(tag.to_string())
        >
            {label}
        </button>
    }
}

/// Build eight SVG path strings (v0..v7) for a single graph,
/// plus y-min, y-max, and span_minutes (0–20).
/// Build eight SVG path strings (v0..v7) for a single graph,
/// plus y-min, y-max, and span_minutes (0–20).
fn build_polyline(
    rows: &[TelemetryRow],
    width: f32,
    height: f32,
) -> (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    f32,
    f32,
    f32,
) {
    if rows.is_empty() {
        return (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            0.0,
            1.0,
            0.0,
        );
    }

    // ---------- 1. Global min/max across all channels (same as before) ----------
    let mut min_v: Option<f32> = None;
    let mut max_v: Option<f32> = None;

    for r in rows {
        for v in [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7] {
            if let Some(x) = v {
                min_v = Some(min_v.map(|m| m.min(x)).unwrap_or(x));
                max_v = Some(max_v.map(|m| m.max(x)).unwrap_or(x));
            }
        }
    }

    let (min_v, mut max_v) = match (min_v, max_v) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                0.0,
                1.0,
                0.0,
            );
        }
    };

    if (max_v - min_v).abs() < 1e-6 {
        max_v = min_v + 1.0;
    }

    // ---------- 2. Time window & span (unchanged semantics) ----------
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
    let denom_time = effective_span_ms as f32;

    // ---------- 3. Plot geometry ----------
    let left = 60.0;
    let right = width - 20.0;
    let top = 20.0;
    let bottom = height - 20.0;

    let plot_width = right - left;
    let plot_height = bottom - top;

    let map_y = |v: f32| bottom - ((v - min_v) / (max_v - min_v)) * plot_height;

    // ---------- 4. Collect only rows in the current window ----------
    let mut window_rows: Vec<&TelemetryRow> = rows
        .iter()
        .filter(|r| r.timestamp_ms >= window_start)
        .collect();

    if window_rows.is_empty() {
        return (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            min_v,
            max_v,
            span_minutes,
        );
    }

    // Ensure chronological order
    window_rows.sort_by_key(|r| r.timestamp_ms);

    let n = window_rows.len();
    let max_points: usize = 2000; // tweak to taste

    // ---------- 5. Choose representation: raw vs averaged ----------
    // We'll produce a list of (ts, [v0..v7]) to actually plot.
    #[derive(Clone, Default)]
    struct Point {
        ts: u64,
        vals: [Option<f32>; 8],
    }

    let mut points: Vec<Point> = Vec::new();

    if n <= max_points {
        // CASE A: below limit → use every row as-is (no averaging).
        points.reserve(n);
        for r in &window_rows {
            points.push(Point {
                ts: r.timestamp_ms as u64,
                vals: [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7],
            });
        }
    } else {
        // CASE B: above limit → compress into exactly max_points index-buckets.
        // All rows contribute to some bucket; nothing is discarded.
        #[derive(Default, Clone)]
        struct BucketAcc {
            ts_sum: u64,
            ts_count: u64,
            v_sum: [f64; 8],
            v_count: [u64; 8],
        }

        let mut buckets = vec![BucketAcc::default(); max_points];

        for (i, r) in window_rows.iter().enumerate() {
            // Map index 0..n-1 → bucket 0..max_points-1
            let bi = i * max_points / n; // integer division; covers full range
            let b = &mut buckets[bi];

            b.ts_sum += r.timestamp_ms as u64;
            b.ts_count += 1;

            let vals = [r.v0, r.v1, r.v2, r.v3, r.v4, r.v5, r.v6, r.v7];
            for (j, opt) in vals.iter().enumerate() {
                if let Some(x) = opt {
                    b.v_sum[j] += *x as f64;
                    b.v_count[j] += 1;
                }
            }
        }

        // Turn non-empty buckets into averaged points
        for b in &buckets {
            if b.ts_count == 0 {
                continue;
            }

            let ts_avg = b.ts_sum / b.ts_count;
            let mut vals: [Option<f32>; 8] = [None; 8];

            for j in 0..8 {
                if b.v_count[j] > 0 {
                    vals[j] = Some((b.v_sum[j] / b.v_count[j] as f64) as f32);
                }
            }

            points.push(Point { ts: ts_avg, vals });
        }

        // Keep them sorted by time (they already should be, but just in case)
        points.sort_by_key(|p| p.ts);
    }

    // ---------- 6. Build SVG paths from final point list ----------
    let mut p0 = String::new();
    let mut p1 = String::new();
    let mut p2 = String::new();
    let mut p3 = String::new();
    let mut p4 = String::new();
    let mut p5 = String::new();
    let mut p6 = String::new();
    let mut p7 = String::new();

    let mut started = [false; 8];

    let add = |path: &mut String, started: &mut bool, x: f32, v: f32| {
        let y = map_y(v);
        if !*started {
            path.push_str(&format!("M {:.2} {:.2}", x, y));
            *started = true;
        } else {
            path.push_str(&format!(" L {:.2} {:.2}", x, y));
        }
    };

    for p in points {
        let dt_ms = (p.ts.saturating_sub(window_start as u64)).min(effective_span_ms as u64) as f32;
        let t = dt_ms / denom_time; // 0 = left, 1 = now
        let x = left + plot_width * t;

        if let Some(v) = p.vals[0] {
            add(&mut p0, &mut started[0], x, v);
        }
        if let Some(v) = p.vals[1] {
            add(&mut p1, &mut started[1], x, v);
        }
        if let Some(v) = p.vals[2] {
            add(&mut p2, &mut started[2], x, v);
        }
        if let Some(v) = p.vals[3] {
            add(&mut p3, &mut started[3], x, v);
        }
        if let Some(v) = p.vals[4] {
            add(&mut p4, &mut started[4], x, v);
        }
        if let Some(v) = p.vals[5] {
            add(&mut p5, &mut started[5], x, v);
        }
        if let Some(v) = p.vals[6] {
            add(&mut p6, &mut started[6], x, v);
        }
        if let Some(v) = p.vals[7] {
            add(&mut p7, &mut started[7], x, v);
        }
    }

    (p0, p1, p2, p3, p4, p5, p6, p7, min_v, max_v, span_minutes)
}

