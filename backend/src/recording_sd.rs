//! Explicit offline SD recovery. Never opens board transports or touches live caches.
use crate::{
    auth::{Permission, StreamRole},
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;

#[derive(Deserialize)]
pub struct ManifestRequest {
    filename: String,
    manifest: serde_json::Value,
}
static CANCELLATIONS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>>,
> = std::sync::OnceLock::new();
fn cancellations()
-> &'static std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>> {
    CANCELLATIONS.get_or_init(Default::default)
}
struct ActiveCancellation(String);
impl Drop for ActiveCancellation {
    fn drop(&mut self) {
        if let Ok(mut active) = cancellations().lock() {
            active.remove(&self.0);
        }
    }
}
struct StageGuard(std::path::PathBuf);
impl Drop for StageGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn configured_limit(name: &str, default: u64, maximum: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0 && *n <= maximum)
        .unwrap_or(default)
}
fn file_limit() -> u64 {
    configured_limit(
        "GS_SD_IMPORT_MAX_BYTES",
        3 * 1024 * 1024 * 1024,
        4 * 1024 * 1024 * 1024,
    )
}
fn disk_limit() -> u64 {
    configured_limit(
        "GS_SD_IMPORT_MAX_DATABASE_BYTES",
        32 * 1024 * 1024 * 1024,
        1024 * 1024 * 1024 * 1024,
    )
}
fn reserve_bytes() -> u64 {
    configured_limit(
        "GS_SD_IMPORT_DISK_RESERVE_BYTES",
        2 * 1024 * 1024 * 1024,
        64 * 1024 * 1024 * 1024,
    )
}
#[cfg(unix)]
fn available_disk_bytes(blocks: impl Into<u64>, fragment_bytes: impl Into<u64>) -> u64 {
    blocks.into().saturating_mul(fragment_bytes.into())
}
#[cfg(unix)]
fn free_disk(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("Invalid disk path"))?;
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(available_disk_bytes(info.f_bavail, info.f_frsize))
}
#[cfg(not(unix))]
fn free_disk(_: &std::path::Path) -> std::io::Result<u64> {
    Err(std::io::Error::other(
        "SD recovery disk preflight is unavailable on this backend platform",
    ))
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
fn effective_limit(root: &std::path::Path, source: &std::path::Path) -> Result<u64, Response> {
    let free = free_disk(root).map_err(|_| {
        (
            StatusCode::INSUFFICIENT_STORAGE,
            "Cannot establish available disk budget",
        )
            .into_response()
    })?;
    let source = std::fs::metadata(source)
        .map_err(|_| StatusCode::NOT_FOUND.into_response())?
        .len();
    // Reserve original snapshot plus database indexes/provenance/raw source expansion. Factor is a conservative preflight estimate, not a reservation.
    let available = free
        .saturating_sub(reserve_bytes())
        .saturating_sub(source.saturating_mul(2));
    let limit =
        file_limit().min(available / configured_limit("GS_SD_IMPORT_EXPANSION_FACTOR", 128, 1024));
    if limit < 1024 {
        return Err((
            StatusCode::INSUFFICIENT_STORAGE,
            "Insufficient free disk for SD recovery; original recording unchanged",
        )
            .into_response());
    }
    Ok(limit)
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn admin(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), Response> {
    let p = crate::web::authorize_headers(state, headers, Permission::ViewData).await?;
    if p.anonymous || !p.roles.contains(&StreamRole::Admin) {
        return Err((
            StatusCode::FORBIDDEN,
            "Administrator access required to import recording data",
        )
            .into_response());
    }
    Ok(())
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
fn stage(root: &std::path::Path, token: &str) -> Result<std::path::PathBuf, Response> {
    if token.is_empty() || token.len() > 40 || !token.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    let path = root.join(format!(".sd-import-{token}"));
    let meta =
        std::fs::symlink_metadata(&path).map_err(|_| StatusCode::NOT_FOUND.into_response())?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    Ok(path)
}
/// Bounded manifest setup; the subsequent PUT streams a File/Blob directly.
pub async fn init(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ManifestRequest>,
) -> Response {
    if let Err(e) = admin(&state, &headers).await {
        return e;
    }
    if body.filename.len() > 128
        || !body.filename.to_lowercase().ends_with(".csv")
        || body.filename.contains(['/', '\\', '\0'])
        || !body.manifest.is_object()
        || body.manifest.to_string().len() > 1024 * 1024
    {
        return (
            StatusCode::BAD_REQUEST,
            "Choose an uncompressed CSV filename and bounded assignment manifest",
        )
            .into_response();
    }
    let source = match crate::recording_export::recording_path(
        &crate::recording_export::directory(&state),
        &id,
    )
    .await
    {
        Ok(p) => p,
        Err(s) => return s.into_response(),
    };
    if state
        .recording_status_snapshot()
        .db_path
        .as_deref()
        .is_some_and(|p| {
            std::path::Path::new(p)
                .file_name()
                .is_some_and(|n| n == id.as_str())
        })
    {
        return (
            StatusCode::CONFLICT,
            "Select a closed recording before SD recovery",
        )
            .into_response();
    }
    let root = source.parent().unwrap();
    let maximum = match effective_limit(root, &source) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let mut pending = 0;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".sd-import-")
                && let Ok(meta) = entry.metadata() {
                    if meta.is_dir()
                        && !entry.file_type().map_or(true, |t| t.is_symlink())
                        && meta
                            .modified()
                            .ok()
                            .and_then(|t| t.elapsed().ok())
                            .is_some_and(|d| d > Duration::from_secs(24 * 3600))
                    {
                        let _ = std::fs::remove_dir_all(entry.path());
                    } else {
                        pending += 1;
                    }
                }
        }
    }
    if pending >= 8 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Complete or cancel pending SD uploads before starting another",
        )
            .into_response();
    }
    let token = rand::random::<u128>().to_string();
    let directory = root.join(format!(".sd-import-{token}"));
    if std::fs::create_dir(&directory).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut guard = StageGuard(directory.clone());
    let metadata =
        serde_json::json!({"recording":id,"filename":body.filename,"manifest":body.manifest});
    if tokio::fs::write(directory.join("manifest.json"), metadata.to_string())
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    guard.0 = std::path::PathBuf::new();
    Json(serde_json::json!({"upload_id":token,"max_bytes":maximum,"max_database_bytes":disk_limit()})).into_response()
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn stage_metadata(
    directory: &std::path::Path,
    id: &str,
) -> Result<serde_json::Value, Response> {
    let path = directory.join("manifest.json");
    let info = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    if !info.is_file() || info.file_type().is_symlink() || info.len() > 2 * 1024 * 1024 {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    if metadata["recording"].as_str() != Some(id) {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    Ok(metadata)
}
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, token)): Path<(String, String)>,
) -> Response {
    if let Err(e) = admin(&state, &headers).await {
        return e;
    }
    let directory = match stage(&crate::recording_export::directory(&state), &token) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = stage_metadata(&directory, &id).await {
        return e;
    }
    if let Ok(mut active) = cancellations().lock()
        && let Some(signal) = active.remove(&token) {
            let _ = signal.send(());
        }
    match tokio::fs::remove_dir_all(directory).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NO_CONTENT.into_response()
        }
        Err(_) => StatusCode::CONFLICT.into_response(),
    }
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
async fn copy_stream<W: tokio::io::AsyncWrite + Unpin>(
    body: axum::body::Body,
    file: &mut W,
    maximum: u64,
    disk_path: Option<&std::path::Path>,
) -> Result<u64, Response> {
    use futures::StreamExt;
    let mut stream = body.into_data_stream();
    let mut count = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "SD upload interrupted; staging data removed",
            )
                .into_response()
        })?;
        count = count
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| StatusCode::PAYLOAD_TOO_LARGE.into_response())?;
        if count > maximum {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "CSV exceeds configured file limit; staging data removed",
            )
                .into_response());
        }
        if let Some(path) = disk_path
            && (count < chunk.len() as u64 + 1
                || count / (4 * 1024 * 1024) != (count - chunk.len() as u64) / (4 * 1024 * 1024))
            && free_disk(path).map_or(true, |free| free < reserve_bytes()) {
                    return Err((
                        StatusCode::INSUFFICIENT_STORAGE,
                        "Disk reserve reached; staging data removed",
                    )
                        .into_response());
            }
        file.write_all(&chunk).await.map_err(|_| {
            (
                StatusCode::INSUFFICIENT_STORAGE,
                "Could not stage CSV; original recording unchanged",
            )
                .into_response()
        })?;
    }
    if count == 0 {
        return Err((StatusCode::BAD_REQUEST, "CSV upload is empty").into_response());
    }
    file.flush()
        .await
        .map_err(|_| StatusCode::INSUFFICIENT_STORAGE.into_response())?;
    Ok(count)
}

/// Existing recordings remain unchanged. Only explicit imported derivatives have
/// this materialized table; TEMP shadowing is local to a single read connection.
pub(crate) async fn activate_resolved(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM main.sqlite_master WHERE type='table' AND name='sd_replay_telemetry'",
    )
    .fetch_one(db)
    .await?;
    if exists > 0 {
        sqlx::query("CREATE TEMP VIEW IF NOT EXISTS telemetry AS SELECT id,timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json FROM main.sd_replay_telemetry").execute(db).await?;
    }
    Ok(())
}

pub(crate) async fn bundle_provenance(
    db: &SqlitePool,
    maximum_bytes: usize,
) -> anyhow::Result<Option<serde_json::Value>> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM main.sqlite_master WHERE name='sd_resolution_manifest'",
    )
    .fetch_one(db)
    .await?;
    if exists == 0 {
        return Ok(None);
    }
    let summary: String =
        sqlx::query_scalar("SELECT manifest_json FROM sd_resolution_manifest WHERE id=1")
            .fetch_one(db)
            .await?;
    let mut used = summary.len();
    let (mut imports, mut samples, mut unresolved) = (Vec::new(), Vec::new(), Vec::new());
    fn checked(
        value: serde_json::Value,
        used: &mut usize,
        maximum: usize,
    ) -> anyhow::Result<serde_json::Value> {
        *used = used.saturating_add(serde_json::to_vec(&value)?.len() + 64);
        anyhow::ensure!(
            *used <= maximum,
            "Replay including SD source provenance exceeds 64 MiB; original SD sources remain available through streamed download"
        );
        Ok(value)
    }
    let mut after = 0;
    loop {
        let row=sqlx::query_as::<_,(i64,String,String,String)>("SELECT id,manifest_json,metadata_json,file_sha256 FROM sd_imports WHERE id>? ORDER BY id LIMIT 1").bind(after).fetch_optional(db).await?;
        let Some((id, manifest, metadata, sha)) = row else {
            break;
        };
        after = id;
        anyhow::ensure!(
            imports.len() < 1024,
            "Portable replay exceeds 1,024 source files; use source downloads"
        );
        imports.push(checked(serde_json::json!({"id":id,"manifest":serde_json::from_str::<serde_json::Value>(&manifest)?,"calibration_metadata":serde_json::from_str::<serde_json::Value>(&metadata)?,"file_sha256":sha}),&mut used,maximum_bytes)?);
    }
    after = 0;
    loop {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT id,provenance_json FROM sd_replay_telemetry WHERE id>? ORDER BY id LIMIT 64",
        )
        .bind(after)
        .fetch_all(db)
        .await?;
        if rows.is_empty() {
            break;
        }
        for (id, value) in rows {
            after = id;
            anyhow::ensure!(
                samples.len() < 200000,
                "Portable replay exceeds 200,000 rows; original source remains available"
            );
            samples.push(checked(serde_json::json!({"id":id,"provenance":serde_json::from_str::<serde_json::Value>(&value)?}),&mut used,maximum_bytes)?);
        }
    }
    after = 0;
    loop {
        let rows=sqlx::query_as::<_,(i64,String,String)>("SELECT s.id,s.identity_key,s.fields_json FROM sd_samples s WHERE s.id>? AND NOT EXISTS(SELECT 1 FROM sd_replay_telemetry r WHERE json_extract(r.provenance_json,'$.sd_sample_id')=s.id) ORDER BY s.id LIMIT 64").bind(after).fetch_all(db).await?;
        if rows.is_empty() {
            break;
        }
        for (id, identity, fields) in rows {
            after = id;
            anyhow::ensure!(
                unresolved.len() < 200000,
                "Portable replay unresolved data exceeds 200,000 rows; use original source download"
            );
            unresolved.push(checked(serde_json::json!({"id":id,"identity_key":identity,"fields":serde_json::from_str::<serde_json::Value>(&fields)?}),&mut used,maximum_bytes)?);
        }
    }
    Ok(Some(
        serde_json::json!({"resolution":serde_json::from_str::<serde_json::Value>(&summary)?,"imports":imports,"sample_provenance":samples,"raw_unresolved_samples":unresolved}),
    ))
}

pub async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, token)): Path<(String, String)>,
    body: axum::body::Body,
) -> Response {
    if let Err(e) = admin(&state, &headers).await {
        return e;
    }
    let source = match crate::recording_export::recording_path(
        &crate::recording_export::directory(&state),
        &id,
    )
    .await
    {
        Ok(p) => p,
        Err(s) => return s.into_response(),
    };
    let directory = match stage(source.parent().unwrap(), &token) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let metadata = match stage_metadata(&directory, &id).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let _guard = StageGuard(directory.clone());
    if state
        .recording_status_snapshot()
        .db_path
        .as_deref()
        .is_some_and(|p| {
            std::path::Path::new(p)
                .file_name()
                .is_some_and(|n| n == id.as_str())
        })
    {
        return (
            StatusCode::CONFLICT,
            "Recording became active; original unchanged",
        )
            .into_response();
    }
    static JOBS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let Ok(_permit) = JOBS.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Another SD import is running",
        )
            .into_response();
    };
    let maximum = match effective_limit(source.parent().unwrap(), &source) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|n| n > maximum)
    {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let seconds = configured_limit("GS_SD_IMPORT_TIMEOUT_SECONDS", 3600, 7200);
    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    if let Ok(mut active) = cancellations().lock() {
        active.insert(token.clone(), cancel_tx);
    }
    let _active = ActiveCancellation(token.clone());
    let csv_path = directory.join(metadata["filename"].as_str().unwrap_or("source.csv"));
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&csv_path)
        .await
    {
        Ok(f) => f,
        Err(_) => return StatusCode::CONFLICT.into_response(),
    };
    let copied = tokio::select! {
        result=tokio::time::timeout(Duration::from_secs(seconds),copy_stream(body,&mut file,maximum,Some(&directory)))=>result,
        _=&mut cancel_rx=>return(StatusCode::CONFLICT,"SD upload canceled; staging removed").into_response()
    };
    match copied {
        Ok(Ok(_)) => (),
        Ok(Err(e)) => return e,
        Err(_) => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                "SD upload exceeded configured time limit; staging removed",
            )
                .into_response();
        }
    }
    drop(file);
    let output = directory.join("derived.db");
    let output_id = format!("groundstation_recording_{token}.db");
    let final_path = source.parent().unwrap().join(&output_id);
    let request = serde_json::json!({"source":source,"output":output,"csv_path":csv_path,"manifest":metadata["manifest"],"max_bytes":maximum,"max_database_bytes":disk_limit().min(free_disk(source.parent().unwrap()).unwrap_or(0).saturating_sub(reserve_bytes())),"minimum_free_bytes":reserve_bytes()});
    let script = format!(
        "{}\n{}\nworker()",
        include_str!("recording_sd_import.py").replace("if __name__=='__main__':\n    main()", ""),
        include_str!("recording_sd_resolve.py")
    );
    let work = tokio::time::timeout(Duration::from_secs(seconds), async {
        let python = std::env::var("GS_REPORT_PYTHON").unwrap_or_else(|_| "python3".into());
        let mut child = tokio::process::Command::new(python)
            .args(["-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(request.to_string().as_bytes()).await?;
        drop(stdin);
        child.wait_with_output().await
    });
    let result = tokio::select! {result=work=>result,_=&mut cancel_rx=>return(StatusCode::CONFLICT,"SD import canceled; original recording unchanged").into_response()};
    match result {
        Ok(Ok(out)) if out.status.success() => {
            let summary = match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                Ok(v) => v,
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
            if let Err(e) = tokio::fs::hard_link(&output, &final_path).await {
                log::error!("Publishing recovered recording: {e}");
                return StatusCode::CONFLICT.into_response();
            }
            Json(serde_json::json!({"recording":output_id,"source_recording":id,"summary":summary,"original_network_preserved":true})).into_response()
        }
        Ok(Ok(out)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .rev()
                .take(1500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        )
            .into_response(),
        Ok(Err(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Python SD import worker unavailable",
        )
            .into_response(),
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            "SD import exceeded configured time limit; original recording unchanged",
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct SourcePage {
    #[serde(default)]
    after_id: i64,
}
pub async fn sources(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::extract::Query(page): axum::extract::Query<SourcePage>,
) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    if page.after_id < 0 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let path = match crate::recording_export::recording_path(
        &crate::recording_export::directory(&state),
        &id,
    )
    .await
    {
        Ok(p) => p,
        Err(s) => return s.into_response(),
    };
    let db = match sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .read_only(true),
        )
        .await
    {
        Ok(d) => d,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    let exists =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE name='sd_imports'")
            .fetch_one(&db)
            .await
            .unwrap_or(0)
            > 0;
    if !exists {
        return Json(serde_json::json!({"sources":[],"has_more":false})).into_response();
    }
    let rows=match sqlx::query_as::<_,(i64,String,String,String,String,String,Option<String>,i64)>("SELECT id,file_sha256,filename,board_id,boot_id,stream_id,launch_session,COALESCE((SELECT SUM(length(data)) FROM sd_source_chunks c WHERE c.file_sha256=i.file_sha256),0) FROM sd_imports i WHERE id>? ORDER BY id LIMIT 129").bind(page.after_id).fetch_all(&db).await{Ok(r)=>r,Err(_)=>return StatusCode::UNPROCESSABLE_ENTITY.into_response()};
    let more = rows.len() > 128;
    let mut after = page.after_id;
    let sources=rows.into_iter().take(128).map(|(id,sha,name,board,boot,stream,session,bytes)|{after=id;serde_json::json!({"import_id":id,"file_sha256":sha,"filename":name,"board_id":board,"boot_id":boot,"stream_id":stream,"launch_session":session,"bytes":bytes})}).collect::<Vec<_>>();
    Json(serde_json::json!({"sources":sources,"has_more":more,"next_id":after})).into_response()
}

/// Byte-exact original SD source, streamed from fixed 64KiB SQLite chunks.
pub async fn raw_source(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, sha)): Path<(String, String)>,
) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let path = match crate::recording_export::recording_path(
        &crate::recording_export::directory(&state),
        &id,
    )
    .await
    {
        Ok(p) => p,
        Err(s) => return s.into_response(),
    };
    let db = match sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .read_only(true),
        )
        .await
    {
        Ok(d) => d,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    if sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sd_source_chunks WHERE file_sha256=?")
        .bind(&sha)
        .fetch_one(&db)
        .await
        .unwrap_or(0)
        == 0
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = futures::stream::try_unfold(
        (db, sha.clone(), -1i64),
        |(db, sha, last)| async move {
            let chunk=sqlx::query_as::<_,(i64,Vec<u8>)>("SELECT offset,data FROM sd_source_chunks WHERE file_sha256=? AND offset>? ORDER BY offset LIMIT 1").bind(&sha).bind(last).fetch_optional(&db).await.map_err(std::io::Error::other)?;
            Ok::<_, std::io::Error>(chunk.map(|(offset, data)| (data, (db, sha, offset))))
        },
    );
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/csv".to_owned()),
            (axum::http::header::CACHE_CONTROL, "no-store".to_owned()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"sd-original-{sha}.csv\""),
            ),
        ],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_export::tests::{TestDirectory, export_state};
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    async fn json_body(response: Response) -> serde_json::Value {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }
    #[tokio::test]
    async fn import_http_preserves_originals_and_replay_export_resolve_actual_firmware_csv() {
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"),serde_json::json!({"anonymous":{"view_data":true},"users":[{"username":"sd-admin","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}]}).to_string()).unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms)VALUES('sd-token','sd-admin','session',1,0,0,9999999999999)").execute(&state.auth_db).await.unwrap();
        let source = "groundstation_recording_123.db";
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(source))
            .await
            .unwrap();
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(1700000005000,1700000000000,'LOADCELL_50_WEIGHT_KG','DAQ','[9]','[1,2]')").execute(&db).await.unwrap();
        db.close().await;
        let before = std::fs::read(root.0.join(source)).unwrap();
        let app = crate::recording_export::routes().with_state(state.clone());
        let csv = include_str!("test-fixtures/sd-recovery-current-firmware.csv");
        let body = serde_json::json!({"filename":"golden.csv","manifest":serde_json::from_str::<serde_json::Value>(include_str!("test-fixtures/sd-recovery-manifest.json")).unwrap()});
        let forbidden = app
            .clone()
            .oneshot(
                Request::post(format!("/api/recordings/{source}/sd/imports"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        let initialized = json_body(
            app.clone()
                .oneshot(
                    Request::post(format!("/api/recordings/{source}/sd/imports"))
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer sd-token")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let token = initialized["upload_id"].as_str().unwrap();
        let chunks = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(csv[..40].to_owned()),
            Ok(csv[40..].to_owned()),
        ]);
        let imported = json_body(
            app.clone()
                .oneshot(
                    Request::put(format!("/api/recordings/{source}/sd/imports/{token}"))
                        .header("authorization", "Bearer sd-token")
                        .body(Body::from_stream(chunks))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let recovered = imported["recording"].as_str().unwrap();
        assert_eq!(
            imported["summary"]["network_suppressed_in_resolved_view"],
            1
        );
        assert_eq!(std::fs::read(root.0.join(source)).unwrap(), before);
        let read = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(root.0.join(recovered))
                    .read_only(true),
            )
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT values_json FROM main.telemetry")
                .fetch_one(&read)
                .await
                .unwrap(),
            "[9]"
        );
        assert!(
            bundle_provenance(&read, 1).await.is_err(),
            "Provenance respects remaining portable bundle budget before accumulating source arrays"
        );
        read.close().await;
        let snapshot = json_body(
            app.clone()
                .oneshot(
                    Request::get(format!("/api/recordings/{recovered}/replay"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(snapshot["start_ms"], 1700000000000i64);
        assert_eq!(snapshot["upper_id"], 1);
        let window=json_body(app.clone().oneshot(Request::get(format!("/api/recordings/{recovered}/replay/window?upper_id=1&after_id=0&start_ms=1700000000000&end_ms=1700000000010&types=%5B%22LOADCELL_50_WEIGHT_KG%22%5D")).body(Body::empty()).unwrap()).await.unwrap()).await;
        assert_eq!(window["rows"][0]["values"][0], -2.5);
        assert_eq!(window["rows"][0]["sender_id"], "DAQ");
        assert_eq!(window["rows"][0]["valid_until_ms"], 1700000000000i64);
        let bundle = json_body(
            app.clone()
                .oneshot(
                    Request::get(format!("/api/recordings/{recovered}/replay/export"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(bundle["samples"][0]["values"][0], -2.5);
        assert_eq!(
            bundle["sd_provenance"]["imports"][0]["manifest"]["boot_id"],
            "verified-golden-boot"
        );
        assert_eq!(
            bundle["sd_provenance"]["raw_unresolved_samples"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            bundle["sd_provenance"]["sample_provenance"][0]["provenance"]["received_timestamp_ms"]
                .is_null()
        );
        let catalog = json_body(
            app.clone()
                .oneshot(
                    Request::get(format!("/api/recordings/{recovered}/sd/sources"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(catalog["sources"][0]["filename"], "golden.csv");
        let sha = imported["summary"]["import"]["file_sha256"]
            .as_str()
            .unwrap();
        let raw = app
            .clone()
            .oneshot(
                Request::get(format!("/api/recordings/{recovered}/sd/sources/{sha}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(raw.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(raw.into_body(), 100000)
                .await
                .unwrap()
                .as_ref(),
            csv.as_bytes()
        );
        assert!(!root.0.join(format!(".sd-import-{token}")).exists());
        for failure in ["oversize", "interrupted", "corrupt", "cancel"] {
            let prepared = json_body(
                app.clone()
                    .oneshot(
                        Request::post(format!("/api/recordings/{source}/sd/imports"))
                            .header("content-type", "application/json")
                            .header("authorization", "Bearer sd-token")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
            let upload = prepared["upload_id"].as_str().unwrap();
            let url = format!("/api/recordings/{source}/sd/imports/{upload}");
            let request =
                match failure {
                    "oversize" => Request::put(url)
                        .header("content-length", "999999999999")
                        .body(Body::empty()),
                    "interrupted" => {
                        Request::put(url).body(Body::from_stream(futures::stream::iter(vec![
                            Err::<String, _>(std::io::Error::other("client disconnected")),
                        ])))
                    }
                    "corrupt" => Request::put(url).body(Body::from("PK-not-a-CSV")),
                    _ => Request::delete(url).body(Body::empty()),
                }
                .unwrap();
            let (mut parts, body) = request.into_parts();
            parts
                .headers
                .insert("authorization", "Bearer sd-token".parse().unwrap());
            let response = app
                .clone()
                .oneshot(Request::from_parts(parts, body))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                match failure {
                    "oversize" => StatusCode::PAYLOAD_TOO_LARGE,
                    "interrupted" => StatusCode::BAD_REQUEST,
                    "corrupt" => StatusCode::UNPROCESSABLE_ENTITY,
                    _ => StatusCode::NO_CONTENT,
                }
            );
            assert!(!root.0.join(format!(".sd-import-{upload}")).exists());
            assert_eq!(std::fs::read(root.0.join(source)).unwrap(), before);
        }
        let prepared = json_body(
            app.clone()
                .oneshot(
                    Request::post(format!("/api/recordings/{source}/sd/imports"))
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer sd-token")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let pending_token = prepared["upload_id"].as_str().unwrap().to_owned();
        let pending_path = format!("/api/recordings/{source}/sd/imports/{pending_token}");
        let blocked = tokio::spawn(
            app.clone().oneshot(
                Request::put(pending_path.clone())
                    .header("authorization", "Bearer sd-token")
                    .body(Body::from_stream(futures::stream::pending::<
                        Result<String, std::io::Error>,
                    >()))
                    .unwrap(),
            ),
        );
        let mut registered = false;
        for _ in 0..200 {
            if cancellations().lock().unwrap().contains_key(&pending_token) {
                registered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            registered,
            "Streaming request must register cancellation before waiting for CSV bytes"
        );
        let canceled = app
            .clone()
            .oneshot(
                Request::delete(pending_path)
                    .header("authorization", "Bearer sd-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(canceled.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), blocked)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert!(!root.0.join(format!(".sd-import-{pending_token}")).exists());
        assert_eq!(std::fs::read(root.0.join(source)).unwrap(), before);
        let csv = app
            .oneshot(
                Request::get(format!("/api/recordings/{recovered}/csv"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(csv.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(csv.into_body(), 100000).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("-2.5"));
    }
    #[tokio::test]
    async fn unauthorized_import_rejected_before_path_or_worker() {
        let root = TestDirectory::new();
        let state = export_state(&root.0, false).await;
        let response = init(
            State(state),
            HeaderMap::new(),
            Path("../../escape".into()),
            Json(ManifestRequest {
                filename: "x.csv".into(),
                manifest: serde_json::json!({}),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    #[tokio::test]
    async fn streaming_limits_and_enospc_cleanup_are_bounded() {
        struct FullWriter;
        impl tokio::io::AsyncWrite for FullWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::from_raw_os_error(libc::ENOSPC)))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let root = TestDirectory::new();
        let directory = root.0.join("stage");
        std::fs::create_dir(&directory).unwrap();
        {
            let _guard = StageGuard(directory.clone());
            std::fs::write(directory.join("partial.csv"), "partial").unwrap();
            let error = copy_stream(Body::from("bytes"), &mut FullWriter, 100, None)
                .await
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::INSUFFICIENT_STORAGE);
        }
        assert!(!directory.exists());
        let error = copy_stream(
            Body::from_stream(futures::stream::iter(vec![
                Ok::<_, std::io::Error>("123"),
                Ok("456"),
            ])),
            &mut tokio::io::sink(),
            4,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}

#[cfg(all(test, unix))]
mod portable_disk_budget_tests {
    #[test]
    fn available_disk_bytes_widens_both_fields_before_saturating() {
        assert_eq!(
            super::available_disk_bytes(u32::MAX, 4096_u64),
            u64::from(u32::MAX) * 4096
        );
        assert_eq!(super::available_disk_bytes(7_u64, 4096_u64), 28_672);
        assert_eq!(super::available_disk_bytes(u64::MAX, 2_u32), u64::MAX);
        assert_eq!(super::available_disk_bytes(0_u32, u64::MAX), 0);
    }
}
