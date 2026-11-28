//! Self-contained SVG line charts for the export bundles (no fonts, scripts
//! or external references, so they open anywhere and embed in the HTML
//! report as they are).

pub(crate) struct Line<'a> {
    pub label: &'a str,
    pub points: &'a [[f64; 2]],
    pub color: &'a str,
    pub dashed: bool,
}

/// Categorical colours (light background), distinguishable in greyscale by
/// the dash on predicted/secondary lines.
pub(crate) const BLUE: &str = "#1f6fb2";
pub(crate) const ORANGE: &str = "#d9731a";
pub(crate) const GREEN: &str = "#2e8b57";
pub(crate) const GREY: &str = "#6b7280";
pub(crate) const RED: &str = "#c0392b";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// "Nice" tick step for a span divided into about `n` intervals.
fn tick_step(span: f64, n: f64) -> f64 {
    if !(span.is_finite() && span > 0.0) {
        return 1.0;
    }
    let raw = span / n;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let nice = if norm < 1.5 {
        1.0
    } else if norm < 3.0 {
        2.0
    } else if norm < 7.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

fn fmt_tick(v: f64, step: f64) -> String {
    let v = if v.abs() < step * 1e-6 { 0.0 } else { v };
    let decimals = if step >= 1.0 { 0 } else { (-step.log10().floor()) as usize };
    format!("{v:.decimals$}")
}

/// Vertical markers: (x, label).
pub(crate) fn chart(title: &str, x_label: &str, y_label: &str, lines: &[Line<'_>], markers: &[(f64, String)]) -> String {
    let (w, h) = (900.0, 420.0);
    let (left, right, top, bottom) = (78.0, 20.0, 44.0, 64.0);
    let (pw, ph) = (w - left - right, h - top - bottom);
    let all = lines
        .iter()
        .flat_map(|l| l.points.iter())
        .filter(|p| p[0].is_finite() && p[1].is_finite());
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for p in all {
        x0 = x0.min(p[0]);
        x1 = x1.max(p[0]);
        y0 = y0.min(p[1]);
        y1 = y1.max(p[1]);
    }
    let empty = x0 > x1;
    if empty {
        (x0, x1, y0, y1) = (0.0, 1.0, 0.0, 1.0);
    }
    if y0 > 0.0 && y0 < 0.25 * y1 {
        y0 = 0.0;
    }
    if (x1 - x0).abs() < 1e-9 {
        x1 = x0 + 1.0;
    }
    if (y1 - y0).abs() < 1e-9 {
        y1 = y0 + 1.0;
    }
    let pad = (y1 - y0) * 0.05;
    let (y0, y1) = (y0 - if y0 == 0.0 { 0.0 } else { pad }, y1 + pad);
    let sx = |x: f64| left + (x - x0) / (x1 - x0) * pw;
    let sy = |y: f64| top + ph - (y - y0) / (y1 - y0) * ph;
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" font-family=\"sans-serif\" font-size=\"13\">\n\
         <rect width=\"{w}\" height=\"{h}\" fill=\"#ffffff\"/>\n\
         <text x=\"{left}\" y=\"24\" font-size=\"16\" font-weight=\"bold\" fill=\"#111827\">{}</text>\n",
        escape(title)
    );
    let xs = tick_step(x1 - x0, 8.0);
    let mut x = (x0 / xs).ceil() * xs;
    while x <= x1 + xs * 1e-6 {
        let px = sx(x);
        s += &format!(
            "<line x1=\"{px:.1}\" y1=\"{top}\" x2=\"{px:.1}\" y2=\"{:.1}\" stroke=\"#e5e7eb\"/>\n",
            top + ph
        );
        s += &format!(
            "<text x=\"{px:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#374151\">{}</text>\n",
            top + ph + 18.0,
            fmt_tick(x, xs)
        );
        x += xs;
    }
    let ys = tick_step(y1 - y0, 6.0);
    let mut y = (y0 / ys).ceil() * ys;
    while y <= y1 + ys * 1e-6 {
        let py = sy(y);
        s += &format!(
            "<line x1=\"{left}\" y1=\"{py:.1}\" x2=\"{:.1}\" y2=\"{py:.1}\" stroke=\"#e5e7eb\"/>\n",
            left + pw
        );
        s += &format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"#374151\">{}</text>\n",
            left - 6.0,
            py + 4.0,
            fmt_tick(y, ys)
        );
        y += ys;
    }
    s += &format!(
        "<rect x=\"{left}\" y=\"{top}\" width=\"{pw}\" height=\"{ph}\" fill=\"none\" stroke=\"#6b7280\"/>\n\
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#111827\">{}</text>\n\
         <text transform=\"translate(18 {:.1}) rotate(-90)\" text-anchor=\"middle\" fill=\"#111827\">{}</text>\n",
        left + pw / 2.0,
        h - 26.0,
        escape(x_label),
        top + ph / 2.0,
        escape(y_label)
    );
    for (i, (mx, label)) in markers.iter().enumerate() {
        if *mx >= x0 && *mx <= x1 {
            let px = sx(*mx);
            let ty = top + 12.0 + (i % 4) as f64 * 13.0;
            s += &format!(
                "<line x1=\"{px:.1}\" y1=\"{top}\" x2=\"{px:.1}\" y2=\"{:.1}\" stroke=\"#9ca3af\" stroke-dasharray=\"3 3\"/>\n\
                 <text x=\"{:.1}\" y=\"{ty:.1}\" fill=\"#4b5563\" font-size=\"11\">{}</text>\n",
                top + ph,
                px + 3.0,
                escape(label)
            );
        }
    }
    for line in lines {
        let pts: Vec<String> = line
            .points
            .iter()
            .filter(|p| p[0].is_finite() && p[1].is_finite())
            .map(|p| format!("{:.1},{:.1}", sx(p[0]), sy(p[1])))
            .collect();
        if pts.is_empty() {
            continue;
        }
        s += &format!(
            "<polyline fill=\"none\" stroke=\"{}\" stroke-width=\"2\"{} points=\"{}\"/>\n",
            line.color,
            if line.dashed { " stroke-dasharray=\"7 4\"" } else { "" },
            pts.join(" ")
        );
    }
    // Legend, top right.
    let mut ly = top + 16.0;
    for line in lines.iter().filter(|l| !l.points.is_empty()) {
        let lx = left + pw - 230.0;
        s += &format!(
            "<line x1=\"{lx:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"3\"{}/>\n\
             <text x=\"{:.1}\" y=\"{ly:.1}\" fill=\"#111827\">{}</text>\n",
            ly - 4.0,
            lx + 22.0,
            ly - 4.0,
            line.color,
            if line.dashed { " stroke-dasharray=\"5 3\"" } else { "" },
            lx + 28.0,
            escape(line.label)
        );
        ly += 18.0;
    }
    if empty {
        s += &format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#6b7280\">No data in this range</text>\n",
            left + pw / 2.0,
            top + ph / 2.0
        );
    }
    s += "</svg>\n";
    s
}
