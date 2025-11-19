use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use groundstation_shared::TelemetryRow;
use leptos::__reexports::wasm_bindgen_futures;
use leptos::prelude::*;
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

const HISTORY_MS: i64 = 60_000 * 20; // cap window at 20 minutes

// Global per-thread storage for the WebSocket handle (WASM is single-threaded)
thread_local! {
    static WS_HANDLE: RefCell<Option<WebSocket>> = RefCell::new(None);
}
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

// Build ws://host/ws or wss://host/ws based on current page location
fn make_ws_url() -> String {
    let window = web_sys::window().expect("no global `window`");
    let location = window.location();

    let protocol = location.protocol().unwrap_or_else(|_| "http:".to_string());

    let host = location
        .host()
        .unwrap_or_else(|_| "localhost:3000".to_string());

    let ws_scheme = if protocol == "https:" { "wss" } else { "ws" };

    format!("{ws_scheme}://{host}/ws")
}

#[component]
pub fn TelemetryDashboard() -> impl IntoView {
    // All telemetry rows
    let (rows, set_rows) = signal(Vec::<TelemetryRow>::new());
    // Active sensor tab
    let (active_tab, set_active_tab) = signal("GYRO_DATA".to_string());

    // 1) Initial pull from backend (/api/recent), trimmed + downsampled
    Effect::new({
        let set_rows = set_rows.clone();
        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(resp) = Request::get("/api/recent").send().await {
                    if let Ok(mut list) = resp.json::<Vec<TelemetryRow>>().await {
                        // Ensure sorted by timestamp
                        list.sort_by_key(|r| r.timestamp_ms);

                        // Trim to last HISTORY_MS window (20 minutes)
                        if let Some(last) = list.last() {
                            let cutoff = last.timestamp_ms - HISTORY_MS;
                            let start = list.partition_point(|r| r.timestamp_ms < cutoff);
                            if start > 0 {
                                list.drain(0..start);
                            }
                        }

                        // Downsample so we don't keep/draw every single point
                        const MAX_INIT_POINTS: usize = 5000; // tweak to taste
                        let n = list.len();
                        if n > MAX_INIT_POINTS {
                            let stride = (n as f32 / MAX_INIT_POINTS as f32).ceil() as usize;
                            list = list
                                .into_iter()
                                .enumerate()
                                .filter_map(
                                    |(i, row)| {
                                        if i % stride == 0 { Some(row) } else { None }
                                    },
                                )
                                .collect();
                        }

                        set_rows.set(list);
                    }
                }
            });
        }
    });

    // 2) Live WebSocket updates with pending buffer + tick loop
    Effect::new({
        let set_rows = set_rows.clone();

        move |_| {
            let ws_url = make_ws_url();
            web_sys::console::log_1(&format!("Connecting WebSocket to {ws_url}").into());

            let ws = WebSocket::new(&ws_url).expect("failed to create WebSocket");

            // store a clone so we can send commands from elsewhere
            WS_HANDLE.with(|cell| {
                *cell.borrow_mut() = Some(ws.clone());
            });

            // Pending rows that have arrived over WS but haven't been applied to `rows` yet.
            let pending: Rc<RefCell<Vec<TelemetryRow>>> = Rc::new(RefCell::new(Vec::new()));

            // --- WebSocket handler: just parse + push into `pending` (very cheap) ---
            let pending_for_ws = Rc::clone(&pending);
            let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Some(text) = event.data().as_string() {
                    if let Ok(row) = serde_json::from_str::<TelemetryRow>(&text) {
                        let mut buf = pending_for_ws.borrow_mut();
                        buf.push(row);

                        // Optional: cap pending buffer size so it doesn't grow without bound
                        const MAX_PENDING: usize = 5000;
                        if buf.len() > MAX_PENDING {
                            let drop = buf.len() - MAX_PENDING;
                            buf.drain(0..drop);
                        }
                    }
                }
            });

            ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
            onmessage.forget();

            // --- Tick loop: every FRAME_MS, flush pending into `rows` and recompute once ---
            let pending_for_tick = Rc::clone(&pending);
            let set_rows_for_tick = set_rows.clone();

            wasm_bindgen_futures::spawn_local(async move {
                // target frame budget ~60–100 ms (10–16 FPS). Tweak to taste.
                const FRAME_MS: u32 = 66;

                loop {
                    TimeoutFuture::new(FRAME_MS).await;

                    // Grab all pending rows in one shot
                    let mut buf = pending_for_tick.borrow_mut();
                    if buf.is_empty() {
                        continue;
                    }
                    let mut new_rows = Vec::new();
                    std::mem::swap(&mut *buf, &mut new_rows);
                    drop(buf); // release borrow early

                    let frame_start = now_ms();

                    set_rows_for_tick.update(|v| {
                        v.extend(new_rows);

                        // keep at most 20 minutes of history
                        if let Some(last) = v.last() {
                            let cutoff = last.timestamp_ms - HISTORY_MS;

                            // v is sorted by timestamp_ms, so we can binary search for cutoff
                            let split = v.partition_point(|r| r.timestamp_ms < cutoff);
                            if split > 0 {
                                v.drain(0..split); // drop old rows in one shot
                            }
                        }

                        // Optional: hard cap total samples in memory
                        const MAX_SAMPLES: usize = 10_000;
                        if v.len() > MAX_SAMPLES {
                            let n = v.len();
                            let stride = (n as f32 / MAX_SAMPLES as f32).ceil() as usize;
                            *v = v
                                .iter()
                                .cloned()
                                .enumerate()
                                .filter_map(
                                    |(i, row)| {
                                        if i % stride == 0 { Some(row) } else { None }
                                    },
                                )
                                .collect();
                        }
                    });

                    let frame_end = now_ms();
                    let _cost = frame_end - frame_start;
                    // web_sys::console::log_1(&format!("frame cost: {:.2} ms", cost).into());
                }
            });
        }
    });

    // Helper closure to send commands over WebSocket
    let send_cmd = move |cmd: &str| {
        let msg = format!(r#"{{"cmd":"{}"}}"#, cmd);
        WS_HANDLE.with(|cell| {
            if let Some(ws) = cell.borrow().as_ref() {
                if let Err(err) = ws.send_with_str(&msg) {
                    web_sys::console::error_1(&err);
                }
            } else {
                web_sys::console::log_1(&"WebSocket not connected yet".into());
            }
        });
    };

    // Rows for the selected sensor type, sorted by time
    let tab_rows = Signal::derive(move || {
        let kind = active_tab.get();
        rows.get()
            .into_iter()
            .filter(|r| r.data_type == kind)
            .collect::<Vec<_>>()
    });

    // Latest row for summary cards
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

    view! {
        <div style="
            min-height: 100vh;
            padding: 1.5rem;
            color: #e5e7eb;
            font-family: system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            background-color: #020617;
            display: flex;
            flex-direction: column;
        ">
            <h1 style="color:#f97316; margin-bottom:1rem;">
                "Rocket Dashboard"
            </h1>

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
                    {sensor_tab("GYRO_DATA", "Gyro", Signal::from(active_tab), set_active_tab)}
                    {sensor_tab("ACCEL_DATA", "Accel", Signal::from(active_tab), set_active_tab)}
                    {sensor_tab("BAROMETER_DATA", "Barom", Signal::from(active_tab), set_active_tab)}
                    {sensor_tab("BATTERY_VOLTAGE", "Batt V", Signal::from(active_tab), set_active_tab)}
                    {sensor_tab("BATTERY_CURRENT", "Batt I", Signal::from(active_tab), set_active_tab)}
                    {sensor_tab("GPS_DATA", "GPS", Signal::from(active_tab), set_active_tab)}
                    {sensor_tab("FUEL_FLOW", "Fuel Flow", Signal::from(active_tab), set_active_tab)}
                    {sensor_tab("FUEL_TANK_PRESSURE", "Fuel Press", Signal::from(active_tab), set_active_tab)}
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
                                // Define a static mapping of fields to labels + colors.
                                let fields: [(&str, Option<f32>, &str); 8] = [
                                    ("v0", row.v0, "#f97316"),
                                    ("v1", row.v1, "#22d3ee"),
                                    ("v2", row.v2, "#a3e635"),
                                    ("v3", row.v3, "#9ca3af"),
                                    ("v4", row.v4, "#9ca3af"),
                                    ("v5", row.v5, "#9ca3af"),
                                    ("v6", row.v6, "#9ca3af"),
                                    ("v7", row.v7, "#9ca3af"),
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



                {/* Command buttons (vertical on right side) */}
                {
                    // Define all command buttons in one array
                    let commands = [
                        (
                            "Arm",
                            "Arm",
                            "border:1px solid #22c55e; background:#022c22; color:#bbf7d0;",
                        ),
                        (
                            "Disarm",
                            "Disarm",
                            "border:1px solid #ef4444; background:#450a0a; color:#fecaca;",
                        ),
                        (
                            "Abort",
                            "Abort",
                            "border:1px solid #ef4444; background:#450a0a; color:#fecaca;",
                        ),
                    ];

                    view! {
                        <div
                            style="
                                    position:fixed;
                                    right:1rem;
                                    top:20%;
                                    transform:translateY(-50%);
                                    display:flex;
                                    flex-direction:column;
                                    gap:0.5rem;
                                    background:#020617ee;
                                    padding:0.85rem;
                                    border-radius:0.75rem;
                                    border:1px solid #4b5563;
                                    box-shadow:0 10px 25px rgba(0,0,0,0.45);
                                    backdrop-filter:blur(6px);
                                    min-width: 9rem;
                                "
                        >
                            <div
                                style="
                                        font-size:0.75rem;
                                        text-transform:uppercase;
                                        letter-spacing:0.08em;
                                        color:#9ca3af;
                                        margin-bottom:0.5rem;
                                        border-bottom:1px solid #4b5563;
                                        padding-bottom:0.25rem;
                                    "
                            >
                                "Actions"
                            </div>

                            {
                                commands
                                    .into_iter()
                                    .map(|(label, cmd, style)| {
                                        view! {
                                            <button
                                                style=format!(
                                                    "padding:0.4rem 0.8rem; border-radius:0.5rem; cursor:pointer; width:100%; text-align:center; {}",
                                                    style
                                                )
                                                on:click=move |_| send_cmd(cmd)
                                            >
                                                {label}
                                            </button>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }
                        </div>
                    }
                }
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

/// Build three SVG path strings (v0, v1, v2) for a single graph,
/// plus y-min, y-max, and span_minutes (0–20).
///
/// X is based on timestamp_ms over a *dynamic* window whose size is:
///   min(20 minutes, newest_ts - oldest_ts)
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

    // Find min/max across all v0..v7
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

    // Time window: dynamic span up to 20 minutes
    let newest_ts = rows.iter().map(|r| r.timestamp_ms).max().unwrap_or(0);
    let oldest_ts = rows
        .iter()
        .map(|r| r.timestamp_ms)
        .min()
        .unwrap_or(newest_ts);
    let raw_span_ms = (newest_ts - oldest_ts).max(1); // avoid zero
    let effective_span_ms = raw_span_ms.min(HISTORY_MS); // cap at 20 minutes
    let span_minutes = effective_span_ms as f32 / 60_000.0;

    let window_start = newest_ts - effective_span_ms;
    let denom_time = effective_span_ms as f32;

    // Plot margins inside the SVG
    let left = 60.0;
    let right = width - 20.0;
    let top = 20.0;
    let bottom = height - 20.0;

    let plot_width = right - left;
    let plot_height = bottom - top;

    let map_y = |v: f32| bottom - ((v - min_v) / (max_v - min_v)) * plot_height;

    let mut p0 = String::new();
    let mut p1 = String::new();
    let mut p2 = String::new();
    let mut p3 = String::new();
    let mut p4 = String::new();
    let mut p5 = String::new();
    let mut p6 = String::new();
    let mut p7 = String::new();

    let mut started0 = false;
    let mut started1 = false;
    let mut started2 = false;
    let mut started3 = false;
    let mut started4 = false;
    let mut started5 = false;
    let mut started6 = false;
    let mut started7 = false;

    // Downsample: limit number of points
    let n = rows.len();
    let max_points = 2000; // tweak to taste
    let stride = if n > max_points {
        (n as f32 / max_points as f32).ceil() as usize
    } else {
        1
    };

    for (idx, r) in rows.iter().enumerate() {
        if idx % stride != 0 {
            continue; // skip to thin data
        }

        // Clamp timestamp into [window_start, newest_ts]
        let dt_ms = (r.timestamp_ms - window_start).clamp(0, effective_span_ms) as f32;
        let t = dt_ms / denom_time; // 0.0 = left, 1.0 = now
        let x = left + plot_width * t;

        if let Some(v) = r.v0 {
            let y = map_y(v);
            if !started0 {
                p0.push_str(&format!("M {:.2} {:.2}", x, y));
                started0 = true;
            } else {
                p0.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }

        if let Some(v) = r.v1 {
            let y = map_y(v);
            if !started1 {
                p1.push_str(&format!("M {:.2} {:.2}", x, y));
                started1 = true;
            } else {
                p1.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }

        if let Some(v) = r.v2 {
            let y = map_y(v);
            if !started2 {
                p2.push_str(&format!("M {:.2} {:.2}", x, y));
                started2 = true;
            } else {
                p2.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }

        if let Some(v) = r.v3 {
            let y = map_y(v);
            if !started3 {
                p3.push_str(&format!("M {:.2} {:.2}", x, y));
                started3 = true;
            } else {
                p3.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }

        if let Some(v) = r.v4 {
            let y = map_y(v);
            if !started4 {
                p4.push_str(&format!("M {:.2} {:.2}", x, y));
                started4 = true;
            } else {
                p4.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }

        if let Some(v) = r.v5 {
            let y = map_y(v);
            if !started5 {
                p5.push_str(&format!("M {:.2} {:.2}", x, y));
                started5 = true;
            } else {
                p5.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }

        if let Some(v) = r.v6 {
            let y = map_y(v);
            if !started6 {
                p6.push_str(&format!("M {:.2} {:.2}", x, y));
                started6 = true;
            } else {
                p6.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }

        if let Some(v) = r.v7 {
            let y = map_y(v);
            if !started7 {
                p7.push_str(&format!("M {:.2} {:.2}", x, y));
                started7 = true;
            } else {
                p7.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }
    }

    (p0, p1, p2, p3, p4, p5, p6, p7, min_v, max_v, span_minutes)
}
