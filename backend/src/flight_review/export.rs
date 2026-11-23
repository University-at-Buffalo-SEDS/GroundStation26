//! Motor and flight export bundles (one ZIP each).
//!
//! Motor bundle (`motor-<label>.zip`):
//! - `README.txt`, `provenance.json`;
//! - `motor_summary.json` (the burn report, the prediction, the window) and
//!   `motor_summary.txt` (the same, readable);
//! - `thrust_curve.csv` (+ `predicted_thrust_curve.csv`);
//! - `loadcells_calibrated.csv` (KG1000/KG50 kg, DAQ SD merged in, source per
//!   sample), `loadcells_raw.csv` (pin volts), `chamber_pt.csv`,
//!   `chamber_pt_raw.csv`, `tank_pt.csv`, `tank_pt_raw.csv`,
//!   `temperatures.csv`, `valve_events.csv`, `daq_sd.csv` (every DAQ SD row a
//!   merged source holds);
//! - `plots/*.svg` (thrust, chamber pressure, tank pressure, mass), and
//!   `plots/*.png` + `report.pdf` when the report Python has matplotlib;
//! - `report.html`: everything above in one page, the charts inline.
//!
//! Flight bundle (`flight-<label>.zip`): `flight_summary.json/.txt`,
//! `events.csv`, `altitude.csv`, `velocity.csv`, `acceleration.csv`,
//! `gps_track.csv`, `plots/altitude.svg`, `plots/velocity.svg` (+ PNG/PDF),
//! `report.html`, `README.txt`, `provenance.json`.
//!
//! Times: `network_ms` (UTC ms on network time) and `t_s` (seconds from the
//! ignition reference / T-0, else from the range start).
use super::data::{Data, Row};
use super::flight::FlightSummary;
use super::motor::{MotorView, channel_key};
use super::plot::{self, Line};
use super::zip::Zip;
use crate::burn_report::merge::{Channel, Sample, Source};
use serde::Serialize;
use std::fmt::Write as _;

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Provenance {
    pub source_id: String,
    pub source_kind: String,
    pub label: String,
    pub mode: Option<String>,
    pub data_provenance: Option<String>,
    pub simulated: bool,
    pub integrated_ground_test: bool,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub generated_ms: i64,
    pub station_software: String,
    pub telemetry_rows: usize,
    pub daq_sd_rows: usize,
    pub notes: Vec<String>,
}

fn utc(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| ms.to_string())
}

fn opt(v: Option<f64>, digits: usize, unit: &str) -> String {
    v.map_or("—".into(), |v| format!("{v:.digits$} {unit}").trim_end().to_owned())
}

fn csv_cell(text: &str) -> String {
    if text.contains([',', '"', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The file-name-safe stem of a label.
pub(crate) fn file_stem(label: &str) -> String {
    let s: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let s = s.split('_').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("_");
    if s.is_empty() {
        "export".into()
    } else {
        s.chars().take(80).collect()
    }
}

// ------------------------------------------------------------------ summaries

/// Readable rows of the motor values (the dashboard section shows the same).
pub(crate) fn motor_lines(view: &MotorView) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let Some(r) = &view.report else {
        rows.push(("Burn report".into(), view.reason.clone().unwrap_or_else(|| "none".into())));
        return rows;
    };
    let status = format!("{:?}", r.status).to_uppercase();
    rows.push((
        "Report".into(),
        format!(
            "{status}{} ({})",
            if view.simulated { ", SIMULATED" } else { "" },
            view.report_origin
        ),
    ));
    rows.push((
        "Ignition reference".into(),
        format!("{} ({})", utc(r.ignition_ms), r.ignition_source.replace('_', " ")),
    ));
    rows.push((
        "Total impulse".into(),
        format!("{:.0} ± {:.0} N·s", r.total_impulse_ns, r.impulse_sigma_ns),
    ));
    rows.push(("Burn time (5 % of peak)".into(), format!("{:.2} s", r.burn_time_s)));
    rows.push((
        "Peak / average thrust".into(),
        format!("{:.0} N / {:.0} N", r.peak_thrust_n, r.average_thrust_n),
    ));
    rows.push(("Effective Isp".into(), opt(r.isp_s, 0, "s")));
    rows.push(("Fill at ignition (KG50)".into(), opt(r.fill_at_ignition_kg, 2, "kg")));
    rows.push(("Propellant consumed".into(), opt(r.propellant_consumed_kg, 2, "kg")));
    rows.push((
        "Oxidizer (N2O) / fuel".into(),
        format!("{} / {}", opt(r.oxidizer_kg, 2, "kg"), opt(r.fuel_kg, 2, "kg")),
    ));
    rows.push((
        "O/F".into(),
        r.of_ratio
            .map_or("not derivable".into(), |v| format!("{v:.2} (all loaded N2O taken as expelled)")),
    ));
    rows.push((
        "Chamber pressure".into(),
        match &r.chamber {
            Some(c) if c.valid => format!("peak {:.0} psi, average {:.0} psi", c.peak_psi, c.mean_psi),
            Some(_) => "not valid (no pressure rise)".into(),
            None => "no chamber PT data".into(),
        },
    ));
    rows.push((
        "Tank pressure start / end".into(),
        format!("{} / {}", opt(r.tank_psi_at_ignition, 0, "psi"), opt(r.tank_psi_end, 0, "psi")),
    ));
    rows.push((
        "Impulse terms".into(),
        format!(
            "KG1000 {:.0} N·s, KG50 unloading {:.0} N·s, startup bridge {:.0} N·s",
            r.kg1000_impulse_ns, r.kg50_term_ns, r.startup_fill_impulse_ns
        ),
    ));
    rows.push((
        "Data".into(),
        format!(
            "DAQ SD {} ({} rows), GS telemetry {} ({} rows)",
            if r.coverage.daq_sd { "yes" } else { "no" },
            r.coverage.daq_rows,
            if r.coverage.gs_telemetry { "yes" } else { "no" },
            r.coverage.gs_rows
        ),
    ));
    if let Some(p) = &view.predicted {
        rows.push((
            "Predicted".into(),
            format!(
                "{}: {} ± {}",
                p.label,
                opt(p.total_impulse_ns, 0, "N·s"),
                opt(p.impulse_sigma_ns, 0, "N·s")
            ),
        ));
    }
    rows
}

fn rel(t: i64, zero: Option<i64>) -> String {
    zero.map_or(String::new(), |z| format!("{:.2}", (t - z) as f64 / 1000.0))
}

/// Readable rows of the flight summary (the dashboard section shows the same).
pub(crate) fn flight_lines(f: &FlightSummary) -> Vec<(String, String)> {
    let mut rows = vec![("Data".to_string(), f.data_label.clone())];
    let since = |t: i64| -> String {
        let mut parts = Vec::new();
        if let Some(z) = f.t0_ms {
            parts.push(format!("T{:+.2} s", (t - z) as f64 / 1000.0));
        }
        if let Some(l) = f.launch_accepted_ms {
            parts.push(format!("Launch{:+.2} s", (t - l) as f64 / 1000.0));
        }
        parts.join(", ")
    };
    if let Some(p) = &f.apogee {
        rows.push(("Apogee".into(), format!("{:.1} m AGL ({}) at {}", p.value, p.source, since(p.t_ms))));
    }
    for p in &f.altitude_maxima {
        rows.push((format!("Max {}", p.source), format!("{:.1} m at {}", p.value, since(p.t_ms))));
    }
    if let Some(v) = f.fc_reported_apogee_m {
        rows.push(("FC-reported apogee".into(), format!("{v:.1} m")));
    }
    if let Some(t) = f.time_to_apogee_s {
        rows.push(("Time to apogee".into(), format!("{t:.2} s from {}", f.time_to_apogee_from)));
    }
    for (name, d) in [("Drogue", &f.drogue), ("Main / reefing", &f.main)] {
        rows.push((
            format!("{name} deploy"),
            match d {
                Some(d) => format!(
                    "{} at {}, {}{}",
                    opt(d.altitude_m, 1, "m"),
                    since(d.t_ms),
                    d.evidence,
                    if d.dry_run { " (DRY-RUN, not fired)" } else { "" }
                ),
                None => "not seen".into(),
            },
        ));
    }
    rows.push((
        "Touchdown / Landed".into(),
        f.landed_ms
            .map_or("not seen".into(), |t| format!("{} ({})", since(t), f.landed_source)),
    ));
    rows.push(("Flight time".into(), opt(f.flight_time_s, 2, "s")));
    if let Some(v) = &f.max_velocity {
        rows.push((
            "Max velocity".into(),
            format!(
                "{:.1} m/s (Mach {}) at {} ({})",
                v.value,
                f.max_mach.map_or("—".into(), |m| format!("{m:.2}")),
                since(v.t_ms),
                v.source
            ),
        ));
    }
    if let Some(v) = f.fc_reported_max_velocity_mps {
        rows.push(("FC-reported max velocity".into(), format!("{v:.1} m/s")));
    }
    if let Some(a) = &f.max_acceleration {
        rows.push((
            "Max acceleration".into(),
            format!(
                "{:.1} m/s² ({:.2} g) at {}",
                a.value,
                f.max_acceleration_g.unwrap_or_default(),
                since(a.t_ms)
            ),
        ));
    }
    rows.push(("Descent rate under drogue".into(), opt(f.descent_rate_drogue_mps, 1, "m/s")));
    rows.push(("Descent rate under main".into(), opt(f.descent_rate_main_mps, 1, "m/s")));
    if let Some(p) = &f.pad {
        rows.push(("Pad".into(), format!("{:.6}, {:.6} ({})", p.lat, p.lon, p.source)));
    }
    if let Some(l) = &f.landing {
        rows.push((
            "Landing".into(),
            format!(
                "{:.6}, {:.6} ({}){}",
                l.lat,
                l.lon,
                l.source,
                f.landing_distance_m.map_or(String::new(), |d| format!(
                    ", {d:.0} m from the pad at {:.0}°",
                    f.landing_bearing_deg.unwrap_or_default()
                ))
            ),
        ));
    }
    rows
}

// ------------------------------------------------------------------ raster

#[derive(Serialize)]
struct RasterLine<'a> {
    label: &'a str,
    points: &'a [[f64; 2]],
    dashed: bool,
    color: &'a str,
}

#[derive(Serialize)]
struct RasterChart<'a> {
    file: &'a str,
    title: &'a str,
    xlabel: &'a str,
    ylabel: &'a str,
    lines: Vec<RasterLine<'a>>,
    markers: &'a [(f64, String)],
}

struct Chart<'a> {
    file: &'a str,
    title: &'a str,
    xlabel: &'a str,
    ylabel: &'a str,
    lines: Vec<Line<'a>>,
    markers: Vec<(f64, String)>,
}

/// PNG charts and a PDF report from the report studio's Python
/// (matplotlib, as the PDF export uses). Optional: without it the bundle
/// keeps its SVG charts and HTML report and says so.
async fn raster(
    charts: &[Chart<'_>],
    title: &str,
    lines: &[(String, String)],
    extra: &[(String, String)],
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let dir = std::env::temp_dir().join(format!(
        "gs-review-{}-{}",
        std::process::id(),
        crate::telemetry_task::get_current_timestamp_ms()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let request = serde_json::json!({
        "out_dir": dir.to_string_lossy(),
        "title": title,
        "summary": lines,
        "extra": extra,
        "charts": charts.iter().map(|c| RasterChart {
            file: c.file, title: c.title, xlabel: c.xlabel, ylabel: c.ylabel, markers: &c.markers,
            lines: c.lines.iter().map(|l| RasterLine { label: l.label, points: l.points, dashed: l.dashed, color: l.color }).collect(),
        }).collect::<Vec<_>>(),
    });
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new(crate::recording_report::report_python())
            .args(["-c", include_str!("plots.py")])
            .env("MPLBACKEND", "Agg")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("report Python unavailable: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        stdin.write_all(request.to_string().as_bytes()).await.map_err(|e| e.to_string())?;
        drop(stdin);
        let out = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
            .await
            .map_err(|_| "PNG/PDF rendering exceeded 90 s".to_string())?
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("PNG/PDF rendering failed")
                .chars()
                .take(300)
                .collect());
        }
        let mut files = Vec::new();
        for c in charts {
            let png = c.file.replace(".svg", ".png");
            if let Ok(bytes) = std::fs::read(dir.join(&png)) {
                files.push((format!("plots/{png}"), bytes));
            }
        }
        if let Ok(bytes) = std::fs::read(dir.join("report.pdf")) {
            files.push(("report.pdf".into(), bytes));
        }
        Ok(files)
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

// ------------------------------------------------------------------ HTML

fn html_report(
    title: &str,
    simulated: bool,
    prov: &Provenance,
    lines: &[(String, String)],
    sections: &[(String, String)],
    charts: &[(String, String)],
    files: &[String],
) -> String {
    let mut h = String::new();
    let _ = write!(
        h,
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>\
         body{{font-family:sans-serif;margin:24px;color:#111827;max-width:1000px}}\
         table{{border-collapse:collapse;margin:8px 0 16px}}td,th{{border:1px solid #d1d5db;padding:4px 8px;text-align:left;vertical-align:top}}\
         th{{background:#f3f4f6}}.sim{{background:#7f1d1d;color:#fff;padding:8px 12px;font-weight:bold;border-radius:6px}}\
         svg{{max-width:100%;height:auto;border:1px solid #e5e7eb;margin:8px 0}}code{{background:#f3f4f6;padding:1px 4px}}</style></head><body>\
         <h1>{}</h1>",
        html_escape(title),
        html_escape(title)
    );
    if simulated {
        h += "<p class=\"sim\">SIMULATED DATA: HITL, firmware simulation or the integrated ground test's replayed flight. Not a measurement of a real flight.</p>";
    }
    h += "<h2>Summary</h2><table>";
    for (k, v) in lines {
        let _ = write!(h, "<tr><th>{}</th><td>{}</td></tr>", html_escape(k), html_escape(v));
    }
    h += "</table>";
    for (heading, body) in sections {
        let _ = write!(h, "<h2>{}</h2>{body}", html_escape(heading));
    }
    h += "<h2>Charts</h2>";
    for (name, svg) in charts {
        let _ = write!(
            h,
            "<figure>{svg}<figcaption><code>plots/{}</code></figcaption></figure>",
            html_escape(name)
        );
    }
    h += "<h2>Provenance</h2><table>";
    let p = serde_json::to_value(prov).unwrap_or_default();
    if let Some(map) = p.as_object() {
        for (k, v) in map {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let text = if (k == "from_ms" || k == "to_ms" || k == "generated_ms") && v.is_i64() {
                format!("{text} ({})", utc(v.as_i64().unwrap_or_default()))
            } else {
                text
            };
            let _ = write!(h, "<tr><th>{}</th><td>{}</td></tr>", html_escape(k), html_escape(&text));
        }
    }
    h += "</table><h2>Files</h2><ul>";
    for f in files {
        let _ = write!(h, "<li><code>{}</code></li>", html_escape(f));
    }
    h += "</ul></body></html>\n";
    h
}

fn readme(kind: &str, prov: &Provenance, files: &[(&str, &str)]) -> String {
    let mut s = format!(
        "UB SEDS ground station — {kind} export\n\nSource: {} ({})\nLabel: {}\nMode: {}{}\nRange: {} .. {}\nGenerated: {}\n\n",
        prov.source_id,
        prov.source_kind,
        prov.label,
        prov.mode.as_deref().unwrap_or("unknown"),
        if prov.simulated { " — SIMULATED DATA" } else { "" },
        prov.from_ms.map_or("start".into(), utc),
        prov.to_ms.map_or("end".into(), utc),
        utc(prov.generated_ms),
    );
    s += "Times: network_ms is UTC milliseconds on network time (the DAQ SD file's network-time rows and the station's source timestamps share it); t_s is seconds from the reference named in each file's header comment.\n\nFiles:\n";
    for (name, what) in files {
        let _ = writeln!(s, "  {name:<28} {what}");
    }
    for note in &prov.notes {
        let _ = writeln!(s, "\nNote: {note}");
    }
    s
}

fn row_values_csv(row: &Row) -> String {
    row.values
        .iter()
        .map(|v| v.map_or(String::new(), |v| format!("{v}")))
        .collect::<Vec<_>>()
        .join(",")
}

// ------------------------------------------------------------------ motor

fn merged(data: &Data, extra_daq: &[(Channel, Sample)]) -> crate::burn_report::Inputs {
    super::motor::inputs(data, extra_daq.to_vec())
}

fn source_name(source: Source) -> &'static str {
    match source {
        Source::DaqSd => "daq_sd",
        Source::GsTelemetry => "gs_telemetry",
    }
}

pub(crate) async fn motor_bundle(
    data: &Data,
    extra_daq: &[(Channel, Sample)],
    view: &MotorView,
    mut prov: Provenance,
    raster_too: bool,
) -> Vec<u8> {
    let zero = view.ignition_ms.or(prov.from_ms).or(data.first_ms());
    let zero_name = if view.ignition_ms.is_some() {
        "the ignition reference"
    } else {
        "the range start"
    };
    let inputs = merged(data, extra_daq);
    prov.daq_sd_rows = inputs.channels.values().flatten().filter(|s| s.source == Source::DaqSd).count()
        + data
            .rows
            .iter()
            .filter(|r| r.sender == "DAQ_SD" || r.data_type.starts_with("DAQ_SD_"))
            .filter(|r| Channel::ALL.iter().all(|c| c.gs_type().0 != r.data_type))
            .count();
    let mut zip = Zip::new(prov.generated_ms);
    let mut listed: Vec<(&str, &str)> = Vec::new();
    let header = |what: &str| {
        format!(
            "# {what}; t_s from {zero_name}{}\n",
            zero.map_or(String::new(), |z| format!(" ({})", utc(z)))
        )
    };
    // Calibrated, merged channels.
    let channel_csv = |channels: &[Channel], unit: &str| -> String {
        let mut rows: Vec<(i64, &'static str, f64, Source)> = Vec::new();
        for c in channels {
            for s in inputs.channels.get(c).map(Vec::as_slice).unwrap_or(&[]) {
                rows.push((s.t_ms, channel_key(*c), s.value, s.source));
            }
        }
        rows.sort_by_key(|r| r.0);
        let mut out = format!("network_ms,t_s,channel,value_{unit},source\n");
        for (t, c, v, s) in rows {
            let _ = writeln!(out, "{t},{},{c},{v},{}", rel(t, zero), source_name(s));
        }
        out
    };
    let raw_csv = |types: &[&str]| -> String {
        let mut out = String::from("network_ms,t_s,sender,data_type,values...\n");
        for r in data
            .rows
            .iter()
            .filter(|r| types.contains(&r.data_type.as_str()) && r.sender != "DAQ_SD")
        {
            let _ = writeln!(
                out,
                "{},{},{},{},{}",
                r.t_ms,
                rel(r.t_ms, zero),
                csv_cell(&r.sender),
                r.data_type,
                row_values_csv(r)
            );
        }
        out
    };
    let summary = motor_lines(view);
    let mut text = format!(
        "Motor summary — {}\n{}\n\n",
        prov.label,
        if view.simulated { "SIMULATED DATA\n" } else { "" }
    );
    for (k, v) in &summary {
        let _ = writeln!(text, "{k:<28} {v}");
    }
    if let Some(r) = &view.report
        && !r.quality_flags.is_empty()
    {
        text += "\nQuality flags:\n";
        for f in &r.quality_flags {
            let _ = writeln!(text, "  [{}] {}: {}", f.severity, f.code, f.message);
        }
    }
    let json = serde_json::json!({
        "format": "seds-gs-motor-export", "version": 1,
        "provenance": prov, "motor": view,
    });
    zip.add("motor_summary.json", &serde_json::to_vec_pretty(&json).unwrap_or_default());
    zip.add("motor_summary.txt", text.as_bytes());
    listed.push(("motor_summary.json", "burn report values, prediction, window, sources (JSON)"));
    listed.push(("motor_summary.txt", "the same, readable"));
    let curve: &[[f64; 2]] = view.report.as_ref().map_or(&[], |r| r.curve.as_slice());
    let mut thrust = header("thrust = KG1000 + KG50 unloading, burn report curve");
    thrust += "t_s,thrust_n\n";
    for [t, f] in curve {
        let _ = writeln!(thrust, "{t:.3},{f:.1}");
    }
    zip.add("thrust_curve.csv", thrust.as_bytes());
    listed.push(("thrust_curve.csv", "thrust curve (t from ignition, N)"));
    if let Some(p) = view.predicted.as_ref().filter(|p| !p.curve.is_empty()) {
        let with_pressures = p.tank_psi.len() == p.curve.len() && p.chamber_psi.len() == p.curve.len();
        let mut csv = if with_pressures {
            format!("# predicted: {}\nt_s,thrust_n,tank_psig,chamber_psig\n", p.label)
        } else {
            format!("# predicted: {}\nt_s,thrust_n\n", p.label)
        };
        for (i, [t, f]) in p.curve.iter().enumerate() {
            if with_pressures {
                let _ = writeln!(csv, "{t:.3},{f:.1},{:.1},{:.1}", p.tank_psi[i][1], p.chamber_psi[i][1]);
            } else {
                let _ = writeln!(csv, "{t:.3},{f:.1}");
            }
        }
        zip.add("predicted_thrust_curve.csv", csv.as_bytes());
        listed.push(("predicted_thrust_curve.csv", "predicted thrust curve"));
    }
    let files: [(&str, String, &str); 9] = [
        (
            "loadcells_calibrated.csv",
            header("KG1000/KG50 calibrated (station calibration as recorded), DAQ SD preferred")
                + &channel_csv(&[Channel::Kg1000, Channel::Kg50], "kg"),
            "KG1000 + KG50 kg, DAQ SD merged, source per sample",
        ),
        (
            "loadcells_raw.csv",
            header("load-cell pin volts as recorded") + &raw_csv(&["KG1000", "KG50", "DAQ_KG50_SELECTED", "DAQ_LOADCELL_CHAMBER"]),
            "KG1000/KG50 raw pin volts",
        ),
        (
            "chamber_pt.csv",
            header("chamber pressure psig, DAQ SD preferred") + &channel_csv(&[Channel::Chamber], "psi"),
            "chamber PT psi, merged",
        ),
        (
            "chamber_pt_raw.csv",
            header("chamber PT connector volts") + &raw_csv(&["COMBUSTION_CHAMBER_VOLTAGE"]),
            "chamber PT volts",
        ),
        (
            "tank_pt.csv",
            header("ox tank pressure psi (station calibration)") + &channel_csv(&[Channel::Tank], "psi"),
            "ox tank PT psi, merged",
        ),
        (
            "tank_pt_raw.csv",
            header("ox tank PT as sent by the valve board") + &raw_csv(&["OX_TANK_PRESSURE"]),
            "ox tank PT raw",
        ),
        (
            "temperatures.csv",
            header("every temperature type in the range")
                + &{
                    let types: Vec<&str> = {
                        let mut t: Vec<&str> = data
                            .rows
                            .iter()
                            .filter(|r| r.data_type.contains("TEMP"))
                            .map(|r| r.data_type.as_str())
                            .collect();
                        t.sort();
                        t.dedup();
                        t
                    };
                    raw_csv(&types)
                },
            "temperatures",
        ),
        (
            "valve_events.csv",
            header("valve state changes")
                + &{
                    let names = crate::raw_stream::schema("VALVE_STATE").0;
                    let mut out = String::from("network_ms,t_s,sender,valve,state\n");
                    let mut last: std::collections::HashMap<(String, usize), Option<f64>> = Default::default();
                    for r in data.rows.iter().filter(|r| r.data_type == "VALVE_STATE") {
                        for (i, v) in r.values.iter().enumerate() {
                            let key = (r.sender.clone(), i);
                            if last.get(&key) != Some(v) {
                                if last.contains_key(&key) || v.is_some_and(|v| v != 0.0) {
                                    let state =
                                        v.map_or(
                                            "unknown".to_string(),
                                            |v| if v != 0.0 { "on/open".into() } else { "off/closed".into() },
                                        );
                                    let _ = writeln!(
                                        out,
                                        "{},{},{},{},{state}",
                                        r.t_ms,
                                        rel(r.t_ms, zero),
                                        csv_cell(&r.sender),
                                        names.get(i).copied().unwrap_or("valve")
                                    );
                                }
                                last.insert(key, *v);
                            }
                        }
                    }
                    out
                },
            "valve open/close events",
        ),
        (
            "daq_sd.csv",
            header("DAQ SD rows of a merged source (sender DAQ_SD / DAQ_SD_* types)")
                + &{
                    let mut out = String::from("network_ms,t_s,data_type,values...\n");
                    for r in data
                        .rows
                        .iter()
                        .filter(|r| r.sender == "DAQ_SD" || r.data_type.starts_with("DAQ_SD_"))
                    {
                        let _ = writeln!(out, "{},{},{},{}", r.t_ms, rel(r.t_ms, zero), r.data_type, row_values_csv(r));
                    }
                    out
                },
            "every DAQ SD row in the range (merged sources)",
        ),
    ];
    for (name, body, what) in &files {
        zip.add(name, body.as_bytes());
        listed.push((name, what));
    }
    // Charts.
    let rel_series = |c: Channel| -> Vec<[f64; 2]> {
        inputs
            .channels
            .get(&c)
            .map(|v| {
                let mut v: Vec<[f64; 2]> = v
                    .iter()
                    .filter_map(|s| zero.map(|z| [(s.t_ms - z) as f64 / 1000.0, s.value]))
                    .collect();
                v.sort_by(|a, b| a[0].total_cmp(&b[0]));
                v
            })
            .unwrap_or_default()
    };
    let (kg1000, kg50, chamber, tank) = (
        rel_series(Channel::Kg1000),
        rel_series(Channel::Kg50),
        rel_series(Channel::Chamber),
        rel_series(Channel::Tank),
    );
    let predicted: &[[f64; 2]] = view.predicted.as_ref().map_or(&[], |p| p.curve.as_slice());
    let mut markers: Vec<(f64, String)> = vec![(0.0, "ignition".into())];
    if let Some(r) = &view.report {
        markers.push((r.burn_end_s, "burn end".into()));
    }
    let xlabel = format!("t (s) from {zero_name}");
    let charts = vec![
        Chart {
            file: "thrust.svg",
            title: "Thrust (KG1000 + KG50 unloading)",
            xlabel: &xlabel,
            ylabel: "Thrust (N)",
            lines: vec![
                Line {
                    label: "measured",
                    points: curve,
                    color: plot::BLUE,
                    dashed: false,
                },
                Line {
                    label: "predicted",
                    points: predicted,
                    color: plot::ORANGE,
                    dashed: true,
                },
            ],
            markers: markers.clone(),
        },
        Chart {
            file: "chamber_pressure.svg",
            title: "Chamber pressure",
            xlabel: &xlabel,
            ylabel: "Pc (psig)",
            lines: vec![Line {
                label: "chamber PT",
                points: &chamber,
                color: plot::RED,
                dashed: false,
            }],
            markers: markers.clone(),
        },
        Chart {
            file: "tank_pressure.svg",
            title: "Ox tank pressure",
            xlabel: &xlabel,
            ylabel: "Tank P (psi)",
            lines: vec![Line {
                label: "tank PT",
                points: &tank,
                color: plot::GREEN,
                dashed: false,
            }],
            markers: markers.clone(),
        },
        Chart {
            file: "mass.svg",
            title: "Load cells",
            xlabel: &xlabel,
            ylabel: "Mass (kg)",
            lines: vec![
                Line {
                    label: "KG1000",
                    points: &kg1000,
                    color: plot::BLUE,
                    dashed: false,
                },
                Line {
                    label: "KG50",
                    points: &kg50,
                    color: plot::ORANGE,
                    dashed: false,
                },
            ],
            markers,
        },
    ];
    let mut svgs = Vec::new();
    for c in &charts {
        let svg = plot::chart(c.title, c.xlabel, c.ylabel, &c.lines, &c.markers);
        zip.add(&format!("plots/{}", c.file), svg.as_bytes());
        svgs.push((c.file.to_string(), svg));
    }
    listed.push(("plots/*.svg", "thrust, chamber pressure, tank pressure, load cells"));
    let title = format!("Motor data — {}", prov.label);
    let flags: Vec<(String, String)> = view
        .report
        .iter()
        .flat_map(|r| {
            r.quality_flags
                .iter()
                .map(|f| (format!("[{}] {}", f.severity, f.code), f.message.clone()))
        })
        .collect();
    if raster_too {
        match raster(&charts, &title, &summary, &flags).await {
            Ok(files) => {
                for (name, bytes) in &files {
                    zip.add(name, bytes);
                }
                if !files.is_empty() {
                    listed.push(("plots/*.png, report.pdf", "the charts as PNG and the report as PDF"));
                }
            }
            Err(error) => prov.notes.push(format!(
                "PNG/PDF not generated ({error}); the SVG charts and report.html carry the same."
            )),
        }
    } else {
        prov.notes.push("PNG/PDF not requested.".into());
    }
    let mut sections = Vec::new();
    if !flags.is_empty() {
        let mut body = String::from("<table><tr><th>Flag</th><th>Meaning</th></tr>");
        for (k, v) in &flags {
            let _ = write!(body, "<tr><td>{}</td><td>{}</td></tr>", html_escape(k), html_escape(v));
        }
        body += "</table>";
        sections.push(("Quality flags".to_string(), body));
    }
    listed.push(("report.html", "everything above in one page"));
    listed.push(("provenance.json", "where the data came from"));
    let names: Vec<String> = listed.iter().map(|(n, _)| n.to_string()).collect();
    zip.add(
        "report.html",
        html_report(&title, view.simulated || prov.simulated, &prov, &summary, &sections, &svgs, &names).as_bytes(),
    );
    zip.add("provenance.json", &serde_json::to_vec_pretty(&prov).unwrap_or_default());
    zip.add("README.txt", readme("motor data", &prov, &listed).as_bytes());
    zip.finish()
}

// ------------------------------------------------------------------ flight

pub(crate) async fn flight_bundle(data: &Data, summary: &FlightSummary, mut prov: Provenance, raster_too: bool) -> Vec<u8> {
    let series = super::flight::export_series(data);
    if series.simulated {
        prov.notes.push("Replayed flight (SIM_SAMPLE): the altitude, velocity, acceleration and GPS files hold the FC's replayed series, not the stand's sensors.".into());
    }
    let zero = summary.curve_zero_ms.or(prov.from_ms).or(data.first_ms());
    let zero_name = if summary.t0_ms.is_some() && summary.curve_zero_ms == summary.t0_ms {
        "T-0"
    } else {
        "the first sample / liftoff"
    };
    let mut zip = Zip::new(prov.generated_ms);
    let mut listed: Vec<(&str, &str)> = Vec::new();
    let lines = flight_lines(summary);
    let mut text = format!(
        "Flight summary — {}\n{}\n\n",
        prov.label,
        if summary.simulated { "SIMULATED DATA\n" } else { "" }
    );
    for (k, v) in &lines {
        let _ = writeln!(text, "{k:<28} {v}");
    }
    for note in &summary.notes {
        let _ = writeln!(text, "\nNote: {note}");
    }
    zip.add(
        "flight_summary.json",
        &serde_json::to_vec_pretty(
            &serde_json::json!({"format": "seds-gs-flight-export", "version": 1, "provenance": prov, "flight": summary}),
        )
        .unwrap_or_default(),
    );
    zip.add("flight_summary.txt", text.as_bytes());
    listed.push(("flight_summary.json", "the flight summary (JSON)"));
    listed.push(("flight_summary.txt", "the same, readable"));
    let mut events = String::from("event,network_ms,utc,t_plus_s,t_from_launch_s,altitude_m,source\n");
    for e in &summary.events {
        let _ = writeln!(
            events,
            "{},{},{},{},{},{},{}",
            csv_cell(&e.name),
            e.t_ms,
            utc(e.t_ms),
            e.t_plus_s.map_or(String::new(), |v| format!("{v:.2}")),
            e.t_launch_s.map_or(String::new(), |v| format!("{v:.2}")),
            e.altitude_m.map_or(String::new(), |v| format!("{v:.1}")),
            csv_cell(&e.source)
        );
    }
    zip.add("events.csv", events.as_bytes());
    listed.push(("events.csv", "events with times from T-0 and from Launch acceptance"));
    let multi = |list: &[(String, Vec<(i64, f64)>)], unit: &str| -> String {
        let mut out = format!("# t_s from {zero_name}\nnetwork_ms,t_s,source,value_{unit}\n");
        let mut all: Vec<(i64, &str, f64)> = list
            .iter()
            .flat_map(|(n, pts)| pts.iter().map(move |(t, v)| (*t, n.as_str(), *v)))
            .collect();
        all.sort_by_key(|r| r.0);
        for (t, n, v) in all {
            let _ = writeln!(out, "{t},{},{},{v}", rel(t, zero), csv_cell(n));
        }
        out
    };
    zip.add("altitude.csv", multi(&series.altitude, "m").as_bytes());
    zip.add("velocity.csv", multi(&series.velocity, "mps").as_bytes());
    zip.add(
        "acceleration.csv",
        multi(&[("Raw accelerometer magnitude".into(), series.acceleration.clone())], "mps2").as_bytes(),
    );
    let mut gps = format!("# t_s from {zero_name}\nnetwork_ms,t_s,lat,lon,alt_msl_m\n");
    for (t, lat, lon, alt) in &series.gps {
        let _ = writeln!(
            gps,
            "{t},{},{lat:.7},{lon:.7},{}",
            rel(*t, zero),
            alt.map_or(String::new(), |a| format!("{a:.1}"))
        );
    }
    zip.add("gps_track.csv", gps.as_bytes());
    listed.push(("altitude.csv", "FC Kalman and barometer altitude"));
    listed.push(("velocity.csv", "FC Kalman velocity, GS vehicle speed"));
    listed.push(("acceleration.csv", "raw accelerometer magnitude"));
    listed.push(("gps_track.csv", "GPS fixes"));
    let to_rel = |pts: &[(i64, f64)]| -> Vec<[f64; 2]> {
        pts.iter()
            .filter_map(|(t, v)| zero.map(|z| [(t - z) as f64 / 1000.0, *v]))
            .collect()
    };
    let alt_lines: Vec<(String, Vec<[f64; 2]>)> = series.altitude.iter().map(|(n, p)| (n.clone(), to_rel(p))).collect();
    let vel_lines: Vec<(String, Vec<[f64; 2]>)> = series.velocity.iter().map(|(n, p)| (n.clone(), to_rel(p))).collect();
    let markers: Vec<(f64, String)> = summary
        .events
        .iter()
        .filter(|e| !e.name.starts_with("Max velocity"))
        .filter_map(|e| zero.map(|z| ((e.t_ms - z) as f64 / 1000.0, e.name.clone())))
        .collect();
    let colors = [plot::BLUE, plot::GREY, plot::GREEN];
    let xlabel = format!("t (s) from {zero_name}");
    let charts = vec![
        Chart {
            file: "altitude.svg",
            title: "Altitude",
            xlabel: &xlabel,
            ylabel: "Altitude (m)",
            lines: alt_lines
                .iter()
                .enumerate()
                .map(|(i, (n, p))| Line {
                    label: n,
                    points: p,
                    color: colors[i % 3],
                    dashed: i > 0,
                })
                .collect(),
            markers: markers.clone(),
        },
        Chart {
            file: "velocity.svg",
            title: "Velocity",
            xlabel: &xlabel,
            ylabel: "Velocity (m/s)",
            lines: vel_lines
                .iter()
                .enumerate()
                .map(|(i, (n, p))| Line {
                    label: n,
                    points: p,
                    color: colors[i % 3],
                    dashed: i > 0,
                })
                .collect(),
            markers,
        },
    ];
    let mut svgs = Vec::new();
    for c in &charts {
        let svg = plot::chart(c.title, c.xlabel, c.ylabel, &c.lines, &c.markers);
        zip.add(&format!("plots/{}", c.file), svg.as_bytes());
        svgs.push((c.file.to_string(), svg));
    }
    listed.push(("plots/*.svg", "altitude and velocity with the events marked"));
    let title = format!("Flight summary — {}", prov.label);
    if raster_too {
        match raster(&charts, &title, &lines, &[]).await {
            Ok(files) => {
                for (name, bytes) in &files {
                    zip.add(name, bytes);
                }
                if !files.is_empty() {
                    listed.push(("plots/*.png, report.pdf", "the charts as PNG and the report as PDF"));
                }
            }
            Err(error) => prov.notes.push(format!(
                "PNG/PDF not generated ({error}); the SVG charts and report.html carry the same."
            )),
        }
    } else {
        prov.notes.push("PNG/PDF not requested.".into());
    }
    let mut body = String::from(
        "<table><tr><th>Event</th><th>UTC</th><th>T+ (s)</th><th>From Launch (s)</th><th>Altitude (m)</th><th>Source</th></tr>",
    );
    for e in &summary.events {
        let _ = write!(
            body,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            html_escape(&e.name),
            utc(e.t_ms),
            e.t_plus_s.map_or("—".into(), |v| format!("{v:.2}")),
            e.t_launch_s.map_or("—".into(), |v| format!("{v:.2}")),
            e.altitude_m.map_or("—".into(), |v| format!("{v:.1}")),
            html_escape(&e.source)
        );
    }
    body += "</table>";
    for note in &summary.notes {
        let _ = write!(body, "<p>{}</p>", html_escape(note));
    }
    listed.push(("report.html", "everything above in one page"));
    listed.push(("provenance.json", "where the data came from"));
    let names: Vec<String> = listed.iter().map(|(n, _)| n.to_string()).collect();
    zip.add(
        "report.html",
        html_report(
            &title,
            summary.simulated || prov.simulated,
            &prov,
            &lines,
            &[("Events".into(), body)],
            &svgs,
            &names,
        )
        .as_bytes(),
    );
    zip.add("provenance.json", &serde_json::to_vec_pretty(&prov).unwrap_or_default());
    zip.add("README.txt", readme("flight summary", &prov, &listed).as_bytes());
    zip.finish()
}
