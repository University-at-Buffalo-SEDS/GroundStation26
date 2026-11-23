//! Fire archive: one protected record per motor burn, for the motor-model fit
//! (docs/apogee-planner.md section 9).
//!
//! `<data>/fire_archive/<id>/fire.json` + `thrust.csv`, outside the rolling
//! store and the event captures, never deleted automatically. Records come
//! from burn reports (pending review), from the seeds shipped with the
//! station (the 2026-11-16 and 2026-10-31 fires) or from an administrator's
//! import. Only records an operator accepted, that are not excluded and not
//! simulated, enter the fit ([`fit_inputs`]).
use crate::auth::Permission;
use crate::burn_report::{BurnReport, thrust::Flag};
use crate::state::AppState;
use axum::{
    Json,
    body::Bytes,
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const DIR: &str = "fire_archive";
const RECORD_FILE: &str = "fire.json";
const CURVE_FILE: &str = "thrust.csv";
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Review {
    /// `pending`, `accepted` or `rejected`.
    pub status: String,
    pub exclude_from_fit: bool,
    pub notes: String,
    pub reviewed_by: Option<String>,
    pub reviewed_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FireRecord {
    pub id: String,
    pub label: String,
    /// `YYYY-MM-DD` (UTC).
    pub date: String,
    /// `seed`, `burn_report` or `import`.
    pub source: String,
    pub burn_report_id: Option<String>,
    pub capture_id: Option<String>,
    /// The station's fill reading at ignition (KG50, GS calibration).
    pub fill_kg: Option<f64>,
    pub fill_before_kg: Option<f64>,
    pub fill_after_kg: Option<f64>,
    pub tank_psi_at_ignition: Option<f64>,
    pub tank_temp_c: Option<f64>,
    pub supercharge_psi: Option<f64>,
    pub ambient_c: Option<f64>,
    pub weather: Option<serde_json::Value>,
    pub impulse_ns: Option<f64>,
    pub impulse_sigma_ns: Option<f64>,
    pub burn_time_s: Option<f64>,
    pub peak_thrust_n: Option<f64>,
    pub chamber_valid: bool,
    pub result: Option<String>,
    pub applied_plan: Option<serde_json::Value>,
    pub quality_flags: Vec<Flag>,
    pub simulated: bool,
    pub review: Review,
    /// Served only: whether the fit uses this record now.
    #[serde(skip_deserializing)]
    pub in_fit: bool,
}

impl FireRecord {
    fn usable(&self) -> bool {
        self.review.status == "accepted"
            && !self.review.exclude_from_fit
            && !self.simulated
            && self.fill_kg.is_some_and(|m| m > 0.0)
            && self.impulse_ns.is_some_and(|i| i > 0.0)
    }
}

/// One accepted fire as the motor fit sees it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FitInput {
    pub id: String,
    pub label: String,
    pub fill_kg: f64,
    pub tank_psi_at_ignition: Option<f64>,
    pub supercharge_psi: Option<f64>,
    pub ambient_c: Option<f64>,
    pub impulse_ns: f64,
    pub impulse_sigma_ns: f64,
}

pub(crate) fn data_dir(state: &AppState) -> PathBuf {
    crate::recording_export::directory(state)
}

fn archive_dir(data: &Path) -> PathBuf {
    data.join(DIR)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(tmp, path)
}

fn save_record(data: &Path, record: &FireRecord, curve: Option<&str>) -> std::io::Result<()> {
    let folder = archive_dir(data).join(&record.id);
    std::fs::create_dir_all(&folder)?;
    if let Some(curve) = curve {
        write_atomic(&folder.join(CURVE_FILE), curve.as_bytes())?;
    }
    let mut stored = record.clone();
    stored.in_fit = false;
    write_atomic(
        &folder.join(RECORD_FILE),
        &serde_json::to_vec_pretty(&stored).unwrap_or_default(),
    )
}

fn read_record(folder: &Path) -> Option<FireRecord> {
    serde_json::from_slice(&std::fs::read(folder.join(RECORD_FILE)).ok()?).ok()
}

/// Every record, newest first, with `in_fit` filled in.
pub(crate) fn load_all(data: &Path) -> Vec<FireRecord> {
    let mut out: Vec<FireRecord> = std::fs::read_dir(archive_dir(data))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_record(&e.path()))
        .collect();
    for r in &mut out {
        r.in_fit = r.usable();
    }
    out.sort_by(|a, b| b.date.cmp(&a.date).then(b.id.cmp(&a.id)));
    out
}

pub(crate) fn get(data: &Path, id: &str) -> Option<FireRecord> {
    if !safe_id(id) {
        return None;
    }
    let mut r = read_record(&archive_dir(data).join(id))?;
    r.in_fit = r.usable();
    Some(r)
}

pub(crate) fn curve(data: &Path, id: &str) -> Option<Vec<[f64; 2]>> {
    if !safe_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(archive_dir(data).join(id).join(CURVE_FILE)).ok()?;
    Some(
        text.lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("t_s"))
            .filter_map(|l| {
                let (t, f) = l.split_once(',')?;
                Some([t.trim().parse().ok()?, f.trim().parse().ok()?])
            })
            .collect(),
    )
}

pub(crate) fn fit_inputs_in(data: &Path) -> Vec<FitInput> {
    load_all(data)
        .into_iter()
        .filter(FireRecord::usable)
        .map(|r| {
            let impulse = r.impulse_ns.unwrap_or(0.0);
            FitInput {
                id: r.id,
                label: r.label,
                fill_kg: r.fill_kg.unwrap_or(0.0),
                tank_psi_at_ignition: r.tank_psi_at_ignition,
                supercharge_psi: r.supercharge_psi,
                ambient_c: r.ambient_c,
                impulse_ns: impulse,
                impulse_sigma_ns: r
                    .impulse_sigma_ns
                    .filter(|s| *s > 0.0)
                    .unwrap_or(0.1 * impulse),
            }
        })
        .collect()
}

/// Accepted, not excluded, not simulated fires for the motor fit.
pub(crate) fn fit_inputs(state: &AppState) -> Vec<FitInput> {
    fit_inputs_in(&data_dir(state))
}

// ------------------------------------------------------------------ seeds

struct Seed {
    id: &'static str,
    record: &'static str,
    curve: Option<&'static str>,
}

const SEEDS: &[Seed] = &[
    Seed {
        id: "2026-11-16_static_fire",
        record: include_str!("../config/fire_archive_seed/2026-11-16_static_fire/fire.json"),
        curve: Some(include_str!(
            "../config/fire_archive_seed/2026-11-16_static_fire/thrust.csv"
        )),
    },
    Seed {
        id: "2026-10-31_static_fire",
        record: include_str!("../config/fire_archive_seed/2026-10-31_static_fire/fire.json"),
        curve: None,
    },
];

/// Writes the shipped fires that are not in the archive yet (an operator's
/// review of a seed is never overwritten).
pub(crate) fn ensure_seeded(data: &Path) -> std::io::Result<usize> {
    let _guard = LOCK.lock().unwrap();
    let mut written = 0;
    for seed in SEEDS {
        if archive_dir(data).join(seed.id).join(RECORD_FILE).exists() {
            continue;
        }
        let record: FireRecord =
            serde_json::from_str(seed.record).map_err(std::io::Error::other)?;
        save_record(data, &record, seed.curve)?;
        written += 1;
    }
    Ok(written)
}

/// Called once at start, beside the other persisted settings.
pub(crate) fn load_persisted(state: &AppState) {
    match ensure_seeded(&data_dir(state)) {
        Ok(0) => {}
        Ok(n) => log::info!("fire archive: {n} seed fire(s) added"),
        Err(error) => log::warn!("fire archive seeds were not written: {error}"),
    }
}

// ------------------------------------------------------------------ ingest

fn utc_date(ms: i64) -> (String, String) {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    (
        format!("{:04}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day()),
        format!("{:02}{:02}{:02}", t.hour(), t.minute(), t.second()),
    )
}

/// A burn report was saved: create or update its pending record. Returns the
/// record id.
pub(crate) fn ingest(
    data: &Path,
    report: &BurnReport,
    manifest: &crate::recording_store::Manifest,
    applied_plan: Option<serde_json::Value>,
    supercharge_psi: Option<f64>,
) -> Result<String, String> {
    let _guard = LOCK.lock().unwrap();
    let existing = load_all(data)
        .into_iter()
        .find(|r| r.burn_report_id.as_deref() == Some(report.id.as_str()));
    let (date, hms) = utc_date(report.ignition_ms);
    let mut record = existing.unwrap_or_else(|| FireRecord {
        id: format!("{date}_{hms}_{}", manifest.event_type.id()),
        label: format!("{} {date} {hms} UTC", manifest.event_type.title()),
        date: date.clone(),
        source: "burn_report".into(),
        burn_report_id: Some(report.id.clone()),
        simulated: report.simulated,
        review: Review {
            status: "pending".into(),
            exclude_from_fit: report.simulated,
            ..Default::default()
        },
        supercharge_psi,
        ..Default::default()
    });
    record.capture_id = report.capture_id.clone();
    record.fill_kg = report.fill_at_ignition_kg;
    record.fill_after_kg = report.fill_after_kg;
    record.tank_psi_at_ignition = report.tank_psi_at_ignition;
    record.impulse_ns = Some(report.total_impulse_ns);
    record.impulse_sigma_ns = Some(report.impulse_sigma_ns);
    record.burn_time_s = Some(report.burn_time_s);
    record.peak_thrust_n = Some(report.peak_thrust_n);
    record.chamber_valid = report.chamber.as_ref().is_some_and(|c| c.valid);
    record.result = manifest.result.clone();
    record.quality_flags = report.quality_flags.clone();
    record.simulated = report.simulated;
    if let Some(plan) = applied_plan {
        if record.ambient_c.is_none() {
            record.ambient_c = plan
                .pointer("/plan/weather/surface_temp_c")
                .or_else(|| plan.pointer("/weather/surface_temp_c"))
                .and_then(|v| v.as_f64());
        }
        if record.weather.is_none() {
            record.weather = plan
                .pointer("/plan/weather")
                .or_else(|| plan.pointer("/weather"))
                .cloned();
        }
        record.applied_plan = Some(plan);
    }
    if !safe_id(&record.id) {
        return Err(format!("bad fire id {}", record.id));
    }
    save_record(
        data,
        &record,
        Some(&crate::burn_report::curve_csv(&report.curve)),
    )
    .map_err(|e| e.to_string())?;
    Ok(record.id)
}

// ------------------------------------------------------------------ review

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Patch {
    pub status: Option<String>,
    pub exclude_from_fit: Option<bool>,
    pub notes: Option<String>,
    pub label: Option<String>,
    pub fill_kg: Option<f64>,
    pub ambient_c: Option<f64>,
    pub tank_psi_at_ignition: Option<f64>,
    pub supercharge_psi: Option<f64>,
}

fn finite(v: Option<f64>, name: &str) -> Result<(), String> {
    match v {
        Some(x) if !x.is_finite() => Err(format!("{name} must be a finite number")),
        _ => Ok(()),
    }
}

pub(crate) fn apply_patch(
    data: &Path,
    id: &str,
    patch: Patch,
    actor: &str,
    now_ms: i64,
) -> Result<(FireRecord, String), (StatusCode, String)> {
    let _guard = LOCK.lock().unwrap();
    let mut record = get(data, id).ok_or((StatusCode::NOT_FOUND, "No such fire.".to_string()))?;
    let bad = |e: String| (StatusCode::BAD_REQUEST, e);
    for (v, n) in [
        (patch.fill_kg, "fill_kg"),
        (patch.ambient_c, "ambient_c"),
        (patch.tank_psi_at_ignition, "tank_psi_at_ignition"),
        (patch.supercharge_psi, "supercharge_psi"),
    ] {
        finite(v, n).map_err(bad)?;
    }
    let mut changes = Vec::new();
    if let Some(status) = patch.status {
        if !matches!(status.as_str(), "pending" | "accepted" | "rejected") {
            return Err(bad("status must be pending, accepted or rejected".into()));
        }
        changes.push(format!("status {}→{status}", record.review.status));
        record.review.status = status;
    }
    if let Some(x) = patch.exclude_from_fit {
        changes.push(format!("exclude_from_fit {x}"));
        record.review.exclude_from_fit = x;
    }
    if let Some(n) = patch.notes {
        record.review.notes = n.chars().take(4000).collect();
        changes.push("notes".into());
    }
    if let Some(l) = patch.label {
        record.label = l.chars().take(200).collect();
        changes.push("label".into());
    }
    let mut set = |field: &mut Option<f64>, v: Option<f64>, name: &str| {
        if let Some(v) = v {
            changes.push(format!("{name} {v}"));
            *field = Some(v);
        }
    };
    set(&mut record.fill_kg, patch.fill_kg, "fill_kg");
    set(&mut record.ambient_c, patch.ambient_c, "ambient_c");
    set(
        &mut record.tank_psi_at_ignition,
        patch.tank_psi_at_ignition,
        "tank_psi_at_ignition",
    );
    set(
        &mut record.supercharge_psi,
        patch.supercharge_psi,
        "supercharge_psi",
    );
    record.review.reviewed_by = Some(actor.to_owned());
    record.review.reviewed_ms = Some(now_ms);
    save_record(data, &record, None)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    record.in_fit = record.usable();
    let text = format!(
        "Fire archive {} reviewed by {actor}: {}.",
        record.id,
        changes.join(", ")
    );
    Ok((record, text))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Import {
    pub label: String,
    pub date: String,
    #[serde(default)]
    pub id: Option<String>,
    pub fill_kg: f64,
    pub impulse_ns: f64,
    #[serde(default)]
    pub impulse_sigma_ns: Option<f64>,
    #[serde(default)]
    pub fill_before_kg: Option<f64>,
    #[serde(default)]
    pub fill_after_kg: Option<f64>,
    #[serde(default)]
    pub tank_psi_at_ignition: Option<f64>,
    #[serde(default)]
    pub tank_temp_c: Option<f64>,
    #[serde(default)]
    pub supercharge_psi: Option<f64>,
    #[serde(default)]
    pub ambient_c: Option<f64>,
    #[serde(default)]
    pub weather: Option<serde_json::Value>,
    #[serde(default)]
    pub burn_time_s: Option<f64>,
    #[serde(default)]
    pub peak_thrust_n: Option<f64>,
    #[serde(default)]
    pub chamber_valid: bool,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub curve: Vec<[f64; 2]>,
}

pub(crate) fn import(
    data: &Path,
    body: Import,
    actor: &str,
    now_ms: i64,
) -> Result<FireRecord, (StatusCode, String)> {
    let bad = |e: &str| (StatusCode::BAD_REQUEST, e.to_string());
    let date_ok = body.date.len() == 10
        && body.date.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if !date_ok {
        return Err(bad("date must be YYYY-MM-DD"));
    }
    if !(body.fill_kg.is_finite()
        && body.fill_kg > 0.0
        && body.impulse_ns.is_finite()
        && body.impulse_ns > 0.0)
    {
        return Err(bad("fill_kg and impulse_ns must be positive numbers"));
    }
    if body
        .curve
        .iter()
        .any(|[t, f]| !t.is_finite() || !f.is_finite())
    {
        return Err(bad("curve values must be finite"));
    }
    let slug: String = body
        .label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let id = body
        .id
        .clone()
        .unwrap_or_else(|| format!("{}_{slug}", body.date));
    if !safe_id(&id) {
        return Err(bad("id may hold only letters, digits, '-' and '_'"));
    }
    let _guard = LOCK.lock().unwrap();
    if archive_dir(data).join(&id).exists() {
        return Err((
            StatusCode::CONFLICT,
            format!("A fire with id {id} already exists."),
        ));
    }
    let record = FireRecord {
        id,
        label: body.label,
        date: body.date,
        source: "import".into(),
        fill_kg: Some(body.fill_kg),
        fill_before_kg: body.fill_before_kg,
        fill_after_kg: body.fill_after_kg,
        tank_psi_at_ignition: body.tank_psi_at_ignition,
        tank_temp_c: body.tank_temp_c,
        supercharge_psi: body.supercharge_psi,
        ambient_c: body.ambient_c,
        weather: body.weather,
        impulse_ns: Some(body.impulse_ns),
        impulse_sigma_ns: body.impulse_sigma_ns,
        burn_time_s: body.burn_time_s,
        peak_thrust_n: body.peak_thrust_n,
        chamber_valid: body.chamber_valid,
        result: body.result,
        review: Review {
            status: "pending".into(),
            notes: body.notes,
            reviewed_by: Some(actor.into()),
            reviewed_ms: Some(now_ms),
            ..Default::default()
        },
        ..Default::default()
    };
    let csv = (!body.curve.is_empty()).then(|| crate::burn_report::curve_csv(&body.curve));
    save_record(data, &record, csv.as_deref())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(record)
}

// ------------------------------------------------------------------ routes

fn error(status: StatusCode, text: impl Into<String>) -> Response {
    (status, Json(json!({"error": text.into()}))).into_response()
}

async fn get_list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    let data = data_dir(&state);
    let fires = tokio::task::spawn_blocking(move || load_all(&data))
        .await
        .unwrap_or_default();
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({ "fires": fires })),
    )
        .into_response()
}

async fn get_thrust(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    let data = data_dir(&state);
    match tokio::task::spawn_blocking(move || curve(&data, &id))
        .await
        .ok()
        .flatten()
    {
        Some(c) => Json(json!({ "curve": c })).into_response(),
        None => error(StatusCode::NOT_FOUND, "No thrust curve for that fire."),
    }
}

async fn patch_fire(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(response) => return response,
        };
    if principal.anonymous || !principal.permissions.send_commands {
        return error(
            StatusCode::FORBIDDEN,
            "Reviewing fires needs command permission.",
        );
    }
    let patch: Patch = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("Bad review: {e}")),
    };
    let actor = principal.username.clone().unwrap_or_default();
    let data = data_dir(&state);
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    match tokio::task::spawn_blocking(move || apply_patch(&data, &id, patch, &actor, now)).await {
        Ok(Ok((record, text))) => {
            log::info!("{text}");
            crate::web::persist_message_db_only(&state, text);
            Json(record).into_response()
        }
        Ok(Err((status, text))) => error(status, text),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Review failed."),
    }
}

async fn post_import(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(p) => p,
            Err(response) => return response,
        };
    if principal.anonymous || !principal.is_admin() {
        return error(
            StatusCode::FORBIDDEN,
            "Only an administrator may import fires.",
        );
    }
    let parsed: Import = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("Bad fire record: {e}")),
    };
    let actor = principal.username.clone().unwrap_or_default();
    let data = data_dir(&state);
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    let actor_log = actor.clone();
    match tokio::task::spawn_blocking(move || import(&data, parsed, &actor, now)).await {
        Ok(Ok(record)) => {
            crate::web::persist_message_db_only(
                &state,
                format!("Fire archive {} imported by {actor_log}.", record.id),
            );
            (StatusCode::CREATED, Json(record)).into_response()
        }
        Ok(Err((status, text))) => error(status, text),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Import failed."),
    }
}

pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/api/fire_archive", axum::routing::get(get_list))
        .route("/api/fire_archive/import", axum::routing::post(post_import))
        .route("/api/fire_archive/{id}", axum::routing::patch(patch_fire))
        .route(
            "/api/fire_archive/{id}/thrust",
            axum::routing::get(get_thrust),
        )
}

#[cfg(test)]
pub(crate) mod tests;
