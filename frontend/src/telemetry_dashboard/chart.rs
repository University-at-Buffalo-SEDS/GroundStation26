use dioxus::prelude::*;

/// Simple SVG line chart (polyline) for timeseries.
/// `points`: Vec of (t_ms, y)
#[component]
pub fn LineChart(
    points: Vec<(i64, f64)>,
    height: i32,
    title: String,
) -> Element {
    if points.len() < 2 {
        return rsx! {
            div { style: "padding:12px; border:1px solid #334155; border-radius:12px; background:#0b1220;",
                div { style:"color:#94a3b8; font-size:12px; margin-bottom:8px;", "{title}" }
                div { style:"color:#64748b; font-size:12px;", "Not enough data yet" }
            }
        };
    }

    let width = 900.0; // SVG viewbox width (scales with CSS)
    let h = height.max(120) as f64;

    let (t_min, t_max) = points.iter().fold((i64::MAX, i64::MIN), |(mn, mx), (t, _)| {
        (mn.min(*t), mx.max(*t))
    });
    let (y_min, y_max) = points.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(mn, mx), (_, y)| {
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

    // (optional) downsample to keep SVG light
    let max_pts = 1200usize;
    let stride = (points.len() / max_pts).max(1);
    let mut poly = String::new();
    for (i, (t, y)) in points.iter().enumerate().step_by(stride) {
        let (x, yy) = to_xy(*t, *y);
        if i == 0 { poly.push_str(&format!("{x:.2},{yy:.2}")); }
        else { poly.push_str(&format!(" {x:.2},{yy:.2}")); }
    }

    rsx! {
        div { style: "padding:12px; border:1px solid #334155; border-radius:12px; background:#0b1220;",
            div { style:"display:flex; align-items:center; justify-content:space-between; margin-bottom:8px;",
                div { style:"color:#94a3b8; font-size:12px;", "{title}" }
                div { style:"color:#64748b; font-size:12px;",
                    {format!("min={:.3} max={:.3}", y_min, y_max)}

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
