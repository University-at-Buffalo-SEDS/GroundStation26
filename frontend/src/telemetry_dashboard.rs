use gloo_net::http::Request;
use groundstation_shared::TelemetryRow;
use leptos::prelude::*;
use std::cell::RefCell;
use leptos::__reexports::wasm_bindgen_futures;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

const HISTORY_MS: i64 = 60_000 * 20; // cap window at 20 minutes

// Global per-thread storage for the WebSocket handle (WASM is single-threaded)
thread_local! {
    static WS_HANDLE: RefCell<Option<WebSocket>> = RefCell::new(None);
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

    // Initial pull from DB (keeps graph on refresh)
    Effect::new({
        let set_rows = set_rows.clone();
        move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(resp) = Request::get("/api/recent").send().await {
                    if let Ok(list) = resp.json::<Vec<TelemetryRow>>().await {
                        set_rows.set(list);
                    }
                }
            });
        }
    });

    // Live WebSocket updates + stash handle in thread-local
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

            let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Some(text) = event.data().as_string() {
                    if let Ok(row) = serde_json::from_str::<TelemetryRow>(&text) {
                        set_rows.update(|v| {
                            v.push(row);
                            // keep at most 20 minutes of history
                            if let Some(last) = v.last() {
                                let cutoff = last.timestamp_ms - HISTORY_MS;
                                v.retain(|r| r.timestamp_ms >= cutoff);
                            }
                        });
                    }
                }
            });

            ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
            onmessage.forget();
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
        let mut v: Vec<_> = rows
            .get()
            .into_iter()
            .filter(|r| r.data_type == kind)
            .collect();
        v.sort_by_key(|r| r.timestamp_ms);
        v
    });

    // Latest row for summary cards
    let latest_row = Signal::derive(move || tab_rows.get().last().cloned());

    // Build SVG data: paths + y-scale + span in minutes
    let graph_data = Signal::derive(move || {
        let data = tab_rows.get();
        build_three_polyline(&data, 1200.0, 360.0)
    });

    let v0_path = Signal::derive(move || graph_data.get().0.clone());
    let v1_path = Signal::derive(move || graph_data.get().1.clone());
    let v2_path = Signal::derive(move || graph_data.get().2.clone());
    let y_min = Signal::derive(move || graph_data.get().3);
    let y_max = Signal::derive(move || graph_data.get().4);
    let span_min = Signal::derive(move || graph_data.get().5); // minutes
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
                            // Build a vector of cards to render only when a value exists
                            let mut cards = Vec::new();

                            if row.v0.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v0"
                                        value=fmt_opt(row.v0)
                                        color="#f97316"   // matches v0 line color
                                    />
                                });
                            }
                            if row.v1.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v1"
                                        value=fmt_opt(row.v1)
                                        color="#22d3ee"   // matches v1 line color
                                    />
                                });
                            }
                            if row.v2.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v2"
                                        value=fmt_opt(row.v2)
                                        color="#a3e635"   // matches v2 line color
                                    />
                                });
                            }

                            // Optional: extra fields v3..v7 stay neutral gray,
                            // since they don't have graph lines yet.
                            if row.v3.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v3"
                                        value=fmt_opt(row.v3)
                                        color="#9ca3af"
                                    />
                                });
                            }
                            if row.v4.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v4"
                                        value=fmt_opt(row.v4)
                                        color="#9ca3af"
                                    />
                                });
                            }
                            if row.v5.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v5"
                                        value=fmt_opt(row.v5)
                                        color="#9ca3af"
                                    />
                                });
                            }
                            if row.v6.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v6"
                                        value=fmt_opt(row.v6)
                                        color="#9ca3af"
                                    />
                                });
                            }
                            if row.v7.is_some() {
                                cards.push(view! {
                                    <SummaryCard
                                        label="v7"
                                        value=fmt_opt(row.v7)
                                        color="#9ca3af"
                                    />
                                });
                            }

                            view! {
                                <div style="display:flex; gap:0.75rem; margin-left:1rem;">
                                    { cards }
                                </div>
                            }
                        }).into_view()
                    }}
                </Show>



                {/* Command buttons */}
                <div style="display:flex; gap:0.5rem; margin-left:1rem;">
                    <button
                        style="
                            padding:0.4rem 0.8rem;
                            border-radius:0.5rem;
                            border:1px solid #22c55e;
                            background:#022c22;
                            color:#bbf7d0;
                            cursor:pointer;
                        "
                        on:click=move |_| send_cmd("Arm")
                    >
                        "Arm"
                    </button>
                    <button
                        style="
                            padding:0.4rem 0.8rem;
                            border-radius:0.5rem;
                            border:1px solid #ef4444;
                            background:#450a0a;
                            color:#fecaca;
                            cursor:pointer;
                        "
                        on:click=move |_| send_cmd("Disarm")
                    >
                        "Disarm"
                    </button>
                    <button
                        style="
                            padding:0.4rem 0.8rem;
                            border-radius:0.5rem;
                            border:1px solid #ef4444;
                            background:#450a0a;
                            color:#fecaca;
                            cursor:pointer;
                        "
                        on:click=move |_| send_cmd("Abort")
                    >
                        "Abort"
                    </button>
                </div>
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

                        {/* v0 = orange, v1 = cyan, v2 = lime */}
                        <path d=move || v0_path.get() stroke="#f97316" fill="none" stroke-width="2"/>
                        <path d=move || v1_path.get() stroke="#22d3ee" fill="none" stroke-width="2"/>
                        <path d=move || v2_path.get() stroke="#a3e635" fill="none" stroke-width="2"/>
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
fn build_three_polyline(
    rows: &[TelemetryRow],
    width: f32,
    height: f32,
) -> (String, String, String, f32, f32, f32) {
    if rows.is_empty() {
        return (String::new(), String::new(), String::new(), 0.0, 1.0, 0.0);
    }

    // Find min/max across all v0/v1/v2
    let mut min_v: Option<f32> = None;
    let mut max_v: Option<f32> = None;

    for r in rows {
        for v in [r.v0, r.v1, r.v2] {
            if let Some(x) = v {
                min_v = Some(min_v.map(|m| m.min(x)).unwrap_or(x));
                max_v = Some(max_v.map(|m| m.max(x)).unwrap_or(x));
            }
        }
    }

    let (min_v, mut max_v) = match (min_v, max_v) {
        (Some(a), Some(b)) => (a, b),
        _ => return (String::new(), String::new(), String::new(), 0.0, 1.0, 0.0),
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

    let mut started0 = false;
    let mut started1 = false;
    let mut started2 = false;

    for r in rows {
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
    }

    (p0, p1, p2, min_v, max_v, span_minutes)
}
