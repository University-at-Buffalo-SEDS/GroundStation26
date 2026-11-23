//! Burn report: the motor's thrust, impulse and quality, computed by the
//! station right after every burn (docs/apogee-planner.md section 8).
//!
//! - **Provisional, live:** 40 s after a launch/HITL capture starts (or when
//!   it ends, if sooner) the report is computed from the station's recent
//!   telemetry cache and announced.
//! - **Provisional, saved:** when the capture is materialized the report is
//!   recomputed from its `recording.db` and saved beside it
//!   (`burn_report.json`, `burn_thrust.csv`).
//! - **Final:** when the DAQ's SD launch file is attached to the capture
//!   (`recording_store::board_sd::offer_file`) the report is recomputed from
//!   the merged DAQ SD + telemetry series (DAQ preferred for the load cells
//!   and the chamber PT) and marked final.
//!
//! Each saved report feeds the fire archive as a pending record; HITL or
//! simulator reports are marked simulated and kept out of the motor fit.
use crate::auth::Permission;
use crate::recording_store::{CAPTURE_FILE, EventKind, MANIFEST_FILE, Manifest};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

pub(crate) mod merge;
#[cfg(test)]
mod tests;
pub(crate) mod thrust;

use merge::{Channel, Sample, Source};
use thrust::Flag;

pub(crate) const REPORT_FILE: &str = "burn_report.json";
pub(crate) const CURVE_FILE: &str = "burn_thrust.csv";
/// The live provisional report waits this long after the capture starts.
const LIVE_AFTER_START_MS: i64 = 40_000;
/// Telemetry before the ignition reference used for the pre-burn level.
const PRE_MS: i64 = 10_000;
/// Telemetry after it (the post-burn KG50 level needs ~12-20 s).
const POST_MS: i64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Provisional,
    Final,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chamber {
    pub valid: bool,
    pub peak_psi: f64,
    pub mean_psi: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    pub daq_sd: bool,
    pub gs_telemetry: bool,
    pub daq_rows: usize,
    pub gs_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BurnReport {
    pub id: String,
    pub capture_id: Option<String>,
    pub capture_label: String,
    pub status: Status,
    pub simulated: bool,
    pub mode: String,
    pub created_ms: i64,
    /// Network time of the ignition reference (main valve open, or detected).
    pub ignition_ms: i64,
    pub ignition_source: String,
    pub total_impulse_ns: f64,
    pub burn_time_s: f64,
    pub peak_thrust_n: f64,
    pub average_thrust_n: f64,
    pub impulse_sigma_ns: f64,
    pub flags: Vec<String>,
    /// (t s from the ignition reference, thrust N).
    pub curve: Vec<[f64; 2]>,
    pub propellant_consumed_kg: Option<f64>,
    pub isp_s: Option<f64>,
    /// The KG50 reading just before ignition (the station's fill reading).
    pub fill_at_ignition_kg: Option<f64>,
    pub fill_after_kg: Option<f64>,
    pub tank_psi_at_ignition: Option<f64>,
    pub chamber: Option<Chamber>,
    pub coverage: Coverage,
    pub quality_flags: Vec<Flag>,
    pub kg1000_impulse_ns: f64,
    pub kg50_term_ns: f64,
    pub startup_fill_impulse_ns: f64,
    pub archive_id: Option<String>,
}

#[derive(Serialize)]
struct Summary<'a> {
    id: &'a str,
    capture_id: &'a Option<String>,
    capture_label: &'a str,
    status: Status,
    simulated: bool,
    mode: &'a str,
    created_ms: i64,
    total_impulse_ns: f64,
    burn_time_s: f64,
    peak_thrust_n: f64,
    flags: &'a [String],
}

impl BurnReport {
    fn summary(&self) -> Summary<'_> {
        Summary {
            id: &self.id,
            capture_id: &self.capture_id,
            capture_label: &self.capture_label,
            status: self.status,
            simulated: self.simulated,
            mode: &self.mode,
            created_ms: self.created_ms,
            total_impulse_ns: self.total_impulse_ns,
            burn_time_s: self.burn_time_s,
            peak_thrust_n: self.peak_thrust_n,
            flags: &self.flags,
        }
    }
}

/// The four channels, on network time, with provenance.
#[derive(Clone, Debug, Default)]
pub(crate) struct Inputs {
    pub channels: HashMap<Channel, Vec<Sample>>,
    /// Main valve open (VB VALVE_STATE valve 0) times seen.
    pub valve_open_ms: Vec<i64>,
}

impl Inputs {
    fn series(&self, channel: Channel) -> &[Sample] {
        self.channels
            .get(&channel)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    /// Adds one station telemetry row (any type; others are ignored).
    pub(crate) fn push_gs(
        &mut self,
        t_ms: i64,
        data_type: &str,
        sender: &str,
        values: &[Option<f64>],
    ) {
        if data_type == "VALVE_STATE" && sender == "VB" {
            if values.first().copied().flatten() == Some(1.0) {
                self.valve_open_ms.push(t_ms);
            }
            return;
        }
        let Some(channel) = Channel::from_gs(data_type, sender) else {
            return;
        };
        if let Some(value) = values.first().copied().flatten().filter(|v| v.is_finite()) {
            self.channels.entry(channel).or_default().push(Sample {
                t_ms,
                value,
                source: Source::GsTelemetry,
            });
        }
    }
    /// Merges DAQ SD rows in, per channel, keeping the preferred source.
    pub(crate) fn merge_daq(&mut self, rows: Vec<(Channel, Sample)>) {
        let mut daq: HashMap<Channel, Vec<Sample>> = HashMap::new();
        for (channel, sample) in rows {
            daq.entry(channel).or_default().push(sample);
        }
        for channel in Channel::ALL {
            let gs = self.channels.remove(&channel).unwrap_or_default();
            let sd = daq.remove(&channel).unwrap_or_default();
            let merged = if channel.preferred() == Source::DaqSd {
                merge::merge(&sd, &gs)
            } else {
                merge::merge(&gs, &sd)
            };
            if !merged.is_empty() {
                self.channels.insert(channel, merged);
            }
        }
    }
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.total_cmp(b));
    Some(xs[xs.len() / 2])
}

/// The ignition reference: the first main-valve open with load-cell data
/// around it, else the first clear unloading of KG50 / load on KG1000
/// (minus the nominal ignition delay).
fn ignition(inputs: &Inputs) -> Option<(i64, &'static str)> {
    let k50 = inputs.series(Channel::Kg50);
    let k1k = inputs.series(Channel::Kg1000);
    let mut valves = inputs.valve_open_ms.clone();
    valves.sort();
    if let Some(t) = valves
        .into_iter()
        .find(|t| k50.iter().any(|s| (s.t_ms - t).abs() < 5_000))
    {
        return Some((t, "main_valve_open"));
    }
    let mut sorted: Vec<&Sample> = k50.iter().collect();
    sorted.sort_by_key(|s| s.t_ms);
    for (i, s) in sorted.iter().enumerate() {
        let before: Vec<f64> = sorted[..i]
            .iter()
            .rev()
            .take_while(|p| s.t_ms - p.t_ms <= 3_000)
            .map(|p| p.value)
            .collect();
        if before.len() < 10 {
            continue;
        }
        let base = median(before).unwrap_or(s.value);
        let next: Vec<f64> = sorted[i..].iter().take(5).map(|p| p.value).collect();
        if next.len() == 5 && median(next).is_some_and(|m| m < base - 3.0) {
            let k1k_first = k1k
                .iter()
                .filter(|p| p.value > 20.0 && p.value < 600.0)
                .map(|p| p.t_ms)
                .min();
            let t = k1k_first.map_or(s.t_ms, |k| k.min(s.t_ms));
            return Some((t - 150, "load_cell_detected"));
        }
    }
    None
}

pub(crate) struct Meta {
    pub id: String,
    pub capture_id: Option<String>,
    pub capture_label: String,
    pub mode: String,
    pub simulated: bool,
    pub created_ms: i64,
}

/// Computes a report from the merged channels.
pub(crate) fn compute(inputs: &Inputs, meta: Meta) -> Result<BurnReport, String> {
    let (t0, ignition_source) =
        ignition(inputs).ok_or("no burn found (no valve open and no load-cell unloading)")?;
    let rel = |channel: Channel| -> Vec<(f64, f64)> {
        let mut v: Vec<(f64, f64)> = inputs
            .series(channel)
            .iter()
            .filter(|s| s.t_ms >= t0 - PRE_MS && s.t_ms <= t0 + POST_MS)
            .map(|s| ((s.t_ms - t0) as f64 / 1000.0, s.value))
            .collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v
    };
    let kg1000 = rel(Channel::Kg1000);
    let kg50 = rel(Channel::Kg50);
    if kg50.is_empty() && kg1000.is_empty() {
        return Err("no load-cell data around the burn".into());
    }
    let thrust = thrust::extract(&kg1000, &kg50, &thrust::Config::default())?;
    let mut flags = thrust.flags.clone();
    let tank = rel(Channel::Tank);
    let tank_at = median(
        tank.iter()
            .filter(|(t, _)| *t >= -1.5 && *t <= -0.05)
            .map(|p| p.1)
            .collect(),
    );
    let chamber_rel = rel(Channel::Chamber);
    let chamber = (!chamber_rel.is_empty()).then(|| {
        let base = median(
            chamber_rel
                .iter()
                .filter(|(t, _)| *t < 0.0)
                .map(|p| p.1)
                .collect(),
        )
        .unwrap_or(0.0);
        let burn: Vec<f64> = chamber_rel
            .iter()
            .filter(|(t, _)| *t >= 0.0 && *t <= thrust.burn_end_s)
            .map(|p| p.1)
            .collect();
        let peak = burn.iter().copied().fold(f64::MIN, f64::max);
        let mean = if burn.is_empty() {
            0.0
        } else {
            burn.iter().sum::<f64>() / burn.len() as f64
        };
        let valid = !burn.is_empty() && peak - base > 5.0;
        Chamber {
            valid,
            peak_psi: (peak.max(0.0) * 10.0).round() / 10.0,
            mean_psi: (mean * 10.0).round() / 10.0,
        }
    });
    if chamber.as_ref().is_some_and(|c| !c.valid) {
        flags.push(Flag::new(
            "chamber_pt_invalid",
            "info",
            "chamber PT shows no pressure rise (disconnected or dead)",
        ));
    }
    let consumed = thrust.kg50_pre_kg - thrust.kg50_post_kg;
    let consumed = (consumed > 0.05).then_some((consumed * 1000.0).round() / 1000.0);
    let isp = consumed.map(|m| (thrust.total_impulse_ns / (m * thrust::G) * 10.0).round() / 10.0);
    let count = |source: Source| {
        inputs
            .channels
            .values()
            .flatten()
            .filter(|s| s.source == source && (s.t_ms - t0).abs() <= POST_MS)
            .count()
    };
    let coverage = Coverage {
        daq_sd: count(Source::DaqSd) > 0,
        gs_telemetry: count(Source::GsTelemetry) > 0,
        daq_rows: count(Source::DaqSd),
        gs_rows: count(Source::GsTelemetry),
    };
    if !coverage.daq_sd {
        flags.push(Flag::new("telemetry_only", "info", "computed from the station telemetry alone; the DAQ SD launch file has not been merged yet"));
    }
    if meta.simulated {
        flags.push(Flag::new(
            "simulated",
            "info",
            "HITL or simulator data: excluded from the motor fit by default",
        ));
    }
    let status = if coverage.daq_sd {
        Status::Final
    } else {
        Status::Provisional
    };
    Ok(BurnReport {
        id: meta.id,
        capture_id: meta.capture_id,
        capture_label: meta.capture_label,
        status,
        simulated: meta.simulated,
        mode: meta.mode,
        created_ms: meta.created_ms,
        ignition_ms: t0,
        ignition_source: ignition_source.into(),
        total_impulse_ns: thrust.total_impulse_ns,
        burn_time_s: thrust.burn_time_s,
        peak_thrust_n: thrust.peak_thrust_n,
        average_thrust_n: thrust.average_thrust_n,
        impulse_sigma_ns: thrust.impulse_sigma_ns,
        flags: flags
            .iter()
            .filter(|f| f.severity != "info")
            .map(|f| f.code.clone())
            .collect(),
        curve: thrust.curve.iter().map(|(t, f)| [*t, *f]).collect(),
        propellant_consumed_kg: consumed,
        isp_s: isp,
        fill_at_ignition_kg: Some((thrust.kg50_pre_kg * 1000.0).round() / 1000.0),
        fill_after_kg: Some((thrust.kg50_post_kg * 1000.0).round() / 1000.0),
        tank_psi_at_ignition: tank_at.map(|p| (p * 10.0).round() / 10.0),
        chamber,
        coverage,
        quality_flags: flags,
        kg1000_impulse_ns: thrust.kg1000_impulse_ns,
        kg50_term_ns: thrust.kg50_term_ns,
        startup_fill_impulse_ns: thrust.startup_fill_impulse_ns,
        archive_id: None,
    })
}

fn mode_is_simulated(mode_id: &str) -> bool {
    mode_id == "hitl" || crate::flight_sim_runtime::enabled() || crate::firmware_sim_mode::enabled()
}

/// A capture recorded from simulated firmware or the internal flight sim is simulated whatever its mode.
fn provenance_is_simulated(provenance: Option<&str>) -> bool {
    provenance.is_some_and(|p| p != "hardware")
}

// ------------------------------------------------------------------ store

#[derive(Default)]
struct Runtime {
    /// Live provisional reports by capture serial.
    live: BTreeMap<u64, BurnReport>,
    /// Capture serials already looked at live.
    seen: HashSet<u64>,
    /// Capture folders waiting for a (re)compute.
    queue: Vec<PathBuf>,
    scanned: bool,
    running: HashSet<PathBuf>,
}

static RUNTIME: LazyLock<Mutex<Runtime>> = LazyLock::new(Default::default);

/// A DAQ SD launch file was attached to this capture folder, or the capture
/// was saved: recompute its report at the next tick. RAM only.
pub(crate) fn request_recompute(folder: &Path) {
    let mut rt = RUNTIME.lock().unwrap();
    if !rt.queue.iter().any(|f| f == folder) {
        rt.queue.push(folder.to_path_buf());
    }
}

/// The capture task saved a launch/HITL capture.
pub(crate) fn capture_saved(capture: &crate::recording_store::Capture, folder: &Path) {
    if !matches!(capture.kind, EventKind::Launch | EventKind::Hitl) {
        return;
    }
    RUNTIME.lock().unwrap().live.remove(&capture.serial);
    request_recompute(folder);
}

fn flight_folders(root: &Path) -> Vec<PathBuf> {
    [EventKind::Launch, EventKind::Hitl]
        .into_iter()
        .flat_map(|kind| {
            std::fs::read_dir(crate::recording_store::events_dir(root).join(kind.id()))
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|e| e.path())
        .filter(|p| p.join(MANIFEST_FILE).exists())
        .collect()
}

/// Once per second from the capture task: the live provisional report and
/// the queued folder recomputes.
pub(crate) fn tick(state: &Arc<AppState>, root: &Path) {
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    live_tick(state, now);
    let folders = {
        let mut rt = RUNTIME.lock().unwrap();
        if !rt.scanned {
            rt.scanned = true;
            // After a restart: captures with a DAQ file but no final report.
            for folder in flight_folders(root) {
                let has_daq = crate::recording_store::read_manifest(&folder)
                    .is_some_and(|m| m.board_sd.iter().any(|e| e.board == "DAQ"));
                let final_saved = read_report(&folder).is_some_and(|r| r.status == Status::Final);
                if has_daq && !final_saved && !rt.queue.contains(&folder) {
                    rt.queue.push(folder);
                }
            }
        }
        let queue = std::mem::take(&mut rt.queue);
        let mut start = Vec::new();
        for folder in queue {
            if rt.running.insert(folder.clone()) {
                start.push(folder);
            } else {
                rt.queue.push(folder);
            }
        }
        start
    };
    for folder in folders {
        let state = state.clone();
        tokio::spawn(async move {
            match recompute_folder(&state, &folder).await {
                Ok(Some(report)) => announce(&state, &report),
                Ok(None) => {}
                Err(error) => log::warn!("burn report for {}: {error}", folder.display()),
            }
            RUNTIME.lock().unwrap().running.remove(&folder);
        });
    }
}

fn announce(state: &AppState, report: &BurnReport) {
    let label = match report.status {
        Status::Provisional => "provisional",
        Status::Final => "final",
    };
    let message = format!(
        "Burn report ({label}{}): {:.2} ± {:.2} kN·s, burn {:.2} s, peak {:.2} kN — {}",
        if report.simulated { ", simulated" } else { "" },
        report.total_impulse_ns / 1000.0,
        report.impulse_sigma_ns / 1000.0,
        report.burn_time_s,
        report.peak_thrust_n / 1000.0,
        report.capture_label
    );
    log::info!("{message}");
    state.add_temporary_notification(message);
}

fn live_tick(state: &Arc<AppState>, now: i64) {
    let captures = state.recording_store.captures();
    for capture in captures
        .iter()
        .filter(|c| matches!(c.kind, EventKind::Launch | EventKind::Hitl))
    {
        let due = now >= capture.event_start_ms + LIVE_AFTER_START_MS
            || capture.event_end_ms.is_some_and(|end| now >= end);
        if !due || !RUNTIME.lock().unwrap().seen.insert(capture.serial) {
            continue;
        }
        let mut inputs = Inputs::default();
        {
            let cache = state.recent_telemetry_cache.lock().unwrap();
            for row in cache
                .iter()
                .filter(|r| r.timestamp_ms >= capture.window_start_ms)
            {
                let values: Vec<Option<f64>> =
                    row.values.iter().map(|v| v.map(f64::from)).collect();
                inputs.push_gs(row.timestamp_ms, &row.data_type, &row.sender_id, &values);
            }
        }
        let meta = Meta {
            id: format!("live-{}", capture.serial),
            capture_id: None,
            capture_label: capture.label.clone(),
            mode: capture.station_mode.clone(),
            simulated: mode_is_simulated(&capture.station_mode) || provenance_is_simulated(capture.provenance.as_deref()),
            created_ms: now,
        };
        match compute(&inputs, meta) {
            Ok(report) => {
                announce(state, &report);
                RUNTIME.lock().unwrap().live.insert(capture.serial, report);
            }
            Err(error) => log::info!("no live burn report for {}: {error}", capture.label),
        }
    }
}

fn read_report(folder: &Path) -> Option<BurnReport> {
    serde_json::from_slice(&std::fs::read(folder.join(REPORT_FILE)).ok()?).ok()
}

fn report_id(kind: EventKind, folder_name: &str) -> String {
    format!("{}.{folder_name}", kind.id())
}

fn folder_for_id(root: &Path, id: &str) -> Option<PathBuf> {
    let (kind, name) = id.split_once('.')?;
    let kind =
        EventKind::from_id(kind).filter(|k| matches!(k, EventKind::Launch | EventKind::Hitl))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(
        crate::recording_store::events_dir(root)
            .join(kind.id())
            .join(name),
    )
}

async fn read_capture_telemetry(db_path: &Path, inputs: &mut Inputs) -> anyhow::Result<()> {
    use sqlx::Row;
    let url = format!("sqlite://{}?mode=ro", db_path.display());
    let pool = sqlx::SqlitePool::connect(&url).await?;
    let rows = sqlx::query(
        "SELECT COALESCE(source_timestamp_ms, timestamp_ms) AS t, data_type, COALESCE(sender_id,'') AS sender, \
         COALESCE(values_json,'[]') AS v FROM telemetry WHERE data_type IN \
         ('LOADCELL_WEIGHT_KG','LOADCELL_50_WEIGHT_KG','PRESSURE_TRANSDUCER_CALIBRATED','COMBUSTION_CHAMBER_PRESSURE','VALVE_STATE') \
         ORDER BY t",
    )
    .fetch_all(&pool)
    .await?;
    for row in rows {
        let values: Vec<Option<f64>> =
            serde_json::from_str(&row.get::<String, _>("v")).unwrap_or_default();
        inputs.push_gs(
            row.get::<i64, _>("t"),
            &row.get::<String, _>("data_type"),
            &row.get::<String, _>("sender"),
            &values,
        );
    }
    pool.close().await;
    Ok(())
}

fn daq_rows(folder: &Path, manifest: &Manifest) -> Vec<(Channel, Sample)> {
    let mut rows = Vec::new();
    for entry in manifest.board_sd.iter().filter(|e| e.board == "DAQ") {
        let dir = folder
            .join(crate::recording_store::board_sd::BOARD_SD_DIR)
            .join("DAQ");
        for name in std::iter::once(&entry.file).chain(entry.lead_up_file.iter()) {
            match merge::read_daq_csv(&dir.join(name)) {
                Ok(mut r) => rows.append(&mut r),
                Err(error) => log::warn!("burn report: cannot read {name}: {error}"),
            }
        }
    }
    rows
}

/// Recomputes and saves the report of one capture folder. None when the
/// capture holds no burn or nothing changed.
pub(crate) async fn recompute_folder(
    state: &AppState,
    folder: &Path,
) -> anyhow::Result<Option<BurnReport>> {
    let supercharge = state.fill_targets_snapshot().nitrous.target_pressure_psi as f64;
    recompute_folder_in(
        &crate::recording_export::directory(state),
        folder,
        Some(supercharge),
    )
    .await
}

pub(crate) fn meta_for(manifest: &Manifest, folder_name: &str, now_ms: i64) -> Meta {
    Meta {
        id: report_id(manifest.event_type, folder_name),
        capture_id: Some(manifest.id.clone()),
        capture_label: manifest.label.clone(),
        mode: manifest.station_mode.clone(),
        simulated: manifest.simulated
            || mode_is_simulated(&manifest.station_mode)
            || provenance_is_simulated(manifest.provenance.as_deref()),
        created_ms: now_ms,
    }
}

/// [`recompute_folder`] with the fire archive under `data` and the
/// supercharge target given.
pub(crate) async fn recompute_folder_in(
    data: &Path,
    folder: &Path,
    supercharge_psi: Option<f64>,
) -> anyhow::Result<Option<BurnReport>> {
    let manifest = crate::recording_store::read_manifest(folder)
        .ok_or_else(|| anyhow::anyhow!("no manifest"))?;
    let mut inputs = Inputs::default();
    let db = folder.join(CAPTURE_FILE);
    if db.exists() {
        read_capture_telemetry(&db, &mut inputs).await?;
    }
    let folder_owned = folder.to_path_buf();
    let manifest_owned = manifest.clone();
    let daq = tokio::task::spawn_blocking(move || daq_rows(&folder_owned, &manifest_owned)).await?;
    inputs.merge_daq(daq);
    let folder_name = folder
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let meta = meta_for(
        &manifest,
        &folder_name,
        crate::telemetry_task::get_current_timestamp_ms() as i64,
    );
    let mut report = match compute(&inputs, meta) {
        Ok(report) => report,
        Err(error) => {
            log::info!("no burn report for {}: {error}", folder.display());
            return Ok(None);
        }
    };
    let previous = read_report(folder);
    if previous.as_ref().is_some_and(|p| p.status == Status::Final)
        && report.status == Status::Provisional
    {
        return Ok(None); // never downgrade a final report
    }
    let plan = manifest
        .details
        .get("apogee_plan")
        .cloned()
        .filter(|v| !v.is_null());
    report.archive_id =
        crate::fire_archive::ingest(data, &report, &manifest, plan, supercharge_psi)
            .map_err(|e| log::warn!("fire archive: {e}"))
            .ok();
    save(folder, &report)?;
    if previous
        .is_some_and(|p| p.status == report.status && p.total_impulse_ns == report.total_impulse_ns)
    {
        return Ok(None); // unchanged: no second notification
    }
    Ok(Some(report))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(tmp, path)
}

pub(crate) fn curve_csv(curve: &[[f64; 2]]) -> String {
    let mut out = String::from("t_s,thrust_n\n");
    for [t, f] in curve {
        out.push_str(&format!("{t:.3},{f:.1}\n"));
    }
    out
}

fn save(folder: &Path, report: &BurnReport) -> std::io::Result<()> {
    write_atomic(
        &folder.join(CURVE_FILE),
        curve_csv(&report.curve).as_bytes(),
    )?;
    write_atomic(
        &folder.join(REPORT_FILE),
        &serde_json::to_vec_pretty(report).unwrap_or_default(),
    )
}

pub(crate) fn list(root: &Path) -> Vec<BurnReport> {
    let mut out: Vec<BurnReport> = RUNTIME.lock().unwrap().live.values().cloned().collect();
    out.extend(flight_folders(root).iter().filter_map(|f| read_report(f)));
    out.sort_by_key(|r| std::cmp::Reverse(r.created_ms));
    out
}

pub(crate) fn find(root: &Path, id: &str) -> Option<BurnReport> {
    if let Some(serial) = id.strip_prefix("live-").and_then(|s| s.parse::<u64>().ok()) {
        return RUNTIME.lock().unwrap().live.get(&serial).cloned();
    }
    read_report(&folder_for_id(root, id)?)
}

// ------------------------------------------------------------------ routes

async fn get_list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    let root = crate::recording_export::directory(&state);
    let reports = tokio::task::spawn_blocking(move || list(&root))
        .await
        .unwrap_or_default();
    let summaries: Vec<_> = reports.iter().map(BurnReport::summary).collect();
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({ "reports": summaries })),
    )
        .into_response()
}

async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    let root = crate::recording_export::directory(&state);
    match tokio::task::spawn_blocking(move || find(&root, &id))
        .await
        .ok()
        .flatten()
    {
        Some(report) => (
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(report),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "No such burn report."})),
        )
            .into_response(),
    }
}

pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/api/burn_reports", axum::routing::get(get_list))
        .route("/api/burn_reports/{id}", axum::routing::get(get_one))
}
