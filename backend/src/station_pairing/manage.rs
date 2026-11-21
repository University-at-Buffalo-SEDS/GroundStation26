//! UI-managed station pairing: storage, the radio worker and admin endpoints.
//!
//! `GS_STATION_PAIRING_FILE` keeps priority. Without it, a pairing saved from
//! the UI lives in `data/station-pairing/` (or `GS_STATION_PAIRING_DIR`) in the
//! `pair_stations.py` layout. A broken pairing is reported here and treated as
//! unpaired for login and primary operation.
use super::exchange::{self, Bundle, Exchange, Side};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) fn managed_dir() -> PathBuf {
    std::env::var_os("GS_STATION_PAIRING_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data").join("station-pairing"))
}
pub(crate) fn managed_file() -> PathBuf {
    managed_dir().join("pairing.json")
}
/// Where the active pairing comes from, if anywhere.
pub(crate) fn configured_path() -> Option<(PathBuf, bool)> {
    if let Some(path) = std::env::var_os("GS_STATION_PAIRING_FILE") {
        return Some((PathBuf::from(path), false));
    }
    let managed = managed_file();
    managed.exists().then_some((managed, true))
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
/// Save a bundle the way `pair_stations.py` lays it out. Never overwrites.
pub(crate) fn save(dir: &Path, bundle: &Bundle) -> anyhow::Result<()> {
    anyhow::ensure!(
        !dir.exists(),
        "{} already exists; unpair first",
        dir.display()
    );
    let parent = dir.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.staging-{:016x}",
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pairing"),
        rand::random::<u64>()
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&staging)?;
    let result = (|| -> anyhow::Result<()> {
        write_private(&staging.join("identity.seed"), &bundle.seed)?;
        let mut json = serde_json::to_vec_pretty(&bundle.pairing_json())?;
        json.push(b'\n');
        write_private(&staging.join("pairing.json"), &json)?;
        // Prove the saved bundle loads before making it visible.
        super::Pairing::from_file(&staging.join("pairing.json"))?;
        std::fs::rename(&staging, dir)?;
        if let Ok(parent) = std::fs::File::open(parent) {
            let _ = parent.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}
pub(crate) fn remove(dir: &Path) -> anyhow::Result<()> {
    for name in ["identity.seed", "pairing.json"] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "removed the identity but {} still holds other files: {e}",
            dir.display()
        )),
    }
}

/// The fingerprint both paired stations show: the shared public key.
pub(crate) fn fingerprint(verify_key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verify_key.as_bytes());
    digest.as_ref()[..6]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|c| c.concat())
        .collect::<Vec<_>>()
        .join("-")
}

static BOOT: OnceLock<Option<String>> = OnceLock::new();
/// Record which pairing the running workers were started with.
pub(crate) fn note_boot() {
    BOOT.get_or_init(|| {
        super::Pairing::load()
            .ok()
            .flatten()
            .map(|p| fingerprint(&p.public.verify_key))
    });
}

struct Worker {
    exchange: Exchange,
    last_saved_error: Option<String>,
}
static WORKER: OnceLock<Mutex<Worker>> = OnceLock::new();
fn worker() -> Option<&'static Mutex<Worker>> {
    WORKER.get()
}
fn mesh_now() -> Option<(crate::rrls::Shared, u64)> {
    let mesh = crate::rrls::settings_transport()?;
    let now = mesh.lock().unwrap().now();
    Some((mesh, now))
}
/// Most frames one exchange step produces; older duplicates are replaced.
const PENDING_LIMIT: usize = 4;
/// Queue pending pairing frames in order. A frame the radio cannot take yet
/// stays pending (with everything after it) and is retried on the next tick
/// until it is queued or the exchange window closes; nothing is dropped.
fn flush(
    pending: &mut std::collections::VecDeque<Vec<u8>>,
    mut queue: impl FnMut(&[u8]) -> bool,
) -> bool {
    while let Some(frame) = pending.front() {
        if !queue(frame) {
            return false;
        }
        pending.pop_front();
    }
    true
}
static PENDING: Mutex<std::collections::VecDeque<Vec<u8>>> =
    Mutex::new(std::collections::VecDeque::new());
/// Add frames to the shared pending queue and try to hand them to the radio.
/// Returns false while some remain deferred (they are retried every tick).
fn send_frames(mesh: &crate::rrls::Shared, frames: Vec<Vec<u8>>, now: u64) -> bool {
    let mut pending = PENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    add_pending(&mut pending, frames);
    flush(&mut pending, |frame| mesh.lock().unwrap().queue_station_pairing(frame, now))
}
fn add_pending(pending: &mut std::collections::VecDeque<Vec<u8>>, frames: Vec<Vec<u8>>) {
    for frame in frames {
        // A resent answer to a retried request replaces its queued copy.
        pending.retain(|queued| *queued != frame);
        if pending.len() >= PENDING_LIMIT {
            pending.pop_front();
        }
        pending.push_back(frame);
    }
}
fn apply(worker: &mut Worker, effects: exchange::Effects) -> Vec<Vec<u8>> {
    if let Some(bundle) = effects.save {
        match save(&managed_dir(), &bundle) {
            Ok(()) => log::info!("Station pairing saved for {}", bundle.side.station_id()),
            Err(error) => {
                let message = error.to_string();
                log::warn!("Station pairing could not be saved: {message}");
                worker.exchange.save_failed(&message);
                worker.last_saved_error = Some(message);
                return Vec::new();
            }
        }
    }
    effects.send
}

/// Start the radio worker. Needs the GS–RRLS mesh; pairing is unavailable without it.
pub(crate) fn start(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    note_boot();
    let mesh = crate::rrls::settings_transport()?;
    let side = if crate::rrls::is_rrls() {
        Side::Rrls
    } else {
        Side::Primary
    };
    WORKER
        .set(Mutex::new(Worker {
            exchange: Exchange::new(side),
            last_saved_error: None,
        }))
        .ok()?;
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(std::time::Duration::from_millis(250));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut deferred_logged = false;
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = ticks.tick() => {}
            }
            let (now, link, frames) = {
                let mut m = mesh.lock().unwrap();
                let now = m.now();
                let frames: Vec<_> = (0..4).filter_map(|_| m.take_station_pairing()).collect();
                // Unpaired and HITL stations run no settings worker. Undrained
                // general SETTINGS frames fill the four-slot inbox, and a full
                // inbox rejects every fragmentation probe, including the BUNDLE.
                if !crate::mission_sync::running() {
                    while m.take_settings().is_some() {}
                }
                if !frames.is_empty() {
                    // The peer is on the air now (link freshness only).
                    m.note_pairing_peer(now);
                }
                (now, m.settings_available(now), frames)
            };
            let Some(lock) = worker() else { break };
            if frames.is_empty() && !lock.lock().unwrap().exchange.active(now) {
                PENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
                continue;
            }
            // Crypto and the small file writes run off the async executor and
            // never under the radio lock.
            let out = tokio::task::spawn_blocking(move || {
                let mut w = lock.lock().unwrap();
                let mut out = Vec::new();
                for frame in frames {
                    let effects = w.exchange.receive(&frame, now);
                    out.extend(apply(&mut w, effects));
                }
                let effects = w.exchange.poll(now, link);
                out.extend(apply(&mut w, effects));
                out
            })
            .await
            .unwrap_or_default();
            let sent = send_frames(&mesh, out, now);
            if !sent && !deferred_logged {
                log::info!(
                    "Pairing frame deferred by radio settings backpressure; retrying every 250 ms until the window closes"
                );
            }
            deferred_logged = !sent;
        }
    }))
}

#[derive(serde::Serialize)]
struct StatusBody {
    role: &'static str,
    paired: bool,
    source: Option<&'static str>,
    error: Option<String>,
    station_id: Option<String>,
    peers: Vec<super::Peer>,
    fingerprint: Option<String>,
    restart_required: bool,
    can_manage: bool,
    radio_available: bool,
    unpair_available: bool,
    exchange: Option<exchange::View>,
    primary_link: Option<serde_json::Value>,
}
fn status_body(can_manage: bool) -> StatusBody {
    let role = if crate::rrls::is_rrls() {
        "rrls"
    } else {
        "primary"
    };
    let configured = configured_path();
    let (pairing, error) = match super::Pairing::load() {
        Ok(p) => (p, None),
        Err(e) => (None, Some(e.to_string())),
    };
    let fingerprint = pairing.as_ref().map(|p| fingerprint(&p.public.verify_key));
    let boot = BOOT.get().cloned().flatten();
    let now = mesh_now().map(|(_, now)| now).unwrap_or(0);
    let exchange = worker().and_then(|w| w.lock().unwrap().exchange.view(now));
    StatusBody {
        role,
        paired: pairing.is_some(),
        source: configured
            .as_ref()
            .map(|(_, managed)| if *managed { "ui" } else { "environment" }),
        error,
        station_id: pairing.as_ref().map(|p| p.public.station_id.clone()),
        peers: pairing.as_ref().map(|p| p.public.peers.clone()).unwrap_or_default(),
        restart_required: BOOT.get().is_some() && fingerprint != boot,
        fingerprint,
        can_manage,
        radio_available: worker().is_some(),
        unpair_available: configured.is_some_and(|(_, managed)| managed),
        exchange,
        primary_link: crate::rrls::is_rrls().then(|| {
            let s = crate::command_proxy::status();
            serde_json::json!({"connected": s.connected, "available": s.available, "reason": s.reason})
        }),
    }
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers return the rejection Response directly, as in admin.rs")]
async fn admin(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), Response> {
    crate::admin::administrator(state, headers)
        .await
        .map(|_| ())
}
async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData)
            .await
        {
            Ok(p) => p,
            Err(e) => return e,
        };
    let can_manage =
        !principal.anonymous && principal.roles.contains(&crate::auth::StreamRole::Admin);
    let mut response = Json(status_body(can_manage)).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartBody {
    /// Optional: without it the station advertises its own address.
    #[serde(default)]
    own_url: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmBody {
    code: String,
}
fn conflict(message: impl Into<String>) -> Response {
    (StatusCode::CONFLICT, message.into()).into_response()
}
async fn start_pairing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<StartBody>,
) -> Response {
    if let Err(e) = admin(&state, &headers).await {
        return e;
    }
    if configured_path().is_some() {
        return conflict(
            "This station already has a pairing. Unpair it first (an environment pairing must be removed from the service configuration).",
        );
    }
    let (Some(lock), Some((_, now))) = (worker(), mesh_now()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Pairing from the UI needs the GroundStation–RRLS radio role on this station.",
        )
            .into_response();
    };
    let mut w = lock.lock().unwrap();
    match w.exchange.start(body.own_url.as_deref(), now) {
        Ok(()) => {
            w.last_saved_error = None;
            drop(w);
            Json(status_body(true)).into_response()
        }
        Err(e) => conflict(e),
    }
}
async fn confirm_pairing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ConfirmBody>,
) -> Response {
    if let Err(e) = admin(&state, &headers).await {
        return e;
    }
    let (Some(lock), Some((mesh, now))) = (worker(), mesh_now()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "No pairing window is open.",
        )
            .into_response();
    };
    let result = tokio::task::spawn_blocking(move || {
        let mut w = lock.lock().unwrap();
        let effects = w.exchange.confirm(&body.code, now)?;
        Ok::<_, String>(apply(&mut w, effects))
    })
    .await
    .unwrap_or_else(|_| Err("pairing worker failed".into()));
    match result {
        Ok(out) => {
            if !out.is_empty() {
                send_frames(&mesh, out, now);
            }
            Json(status_body(true)).into_response()
        }
        Err(e) => conflict(e),
    }
}
async fn cancel_pairing(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = admin(&state, &headers).await {
        return e;
    }
    if let (Some(lock), Some((mesh, now))) = (worker(), mesh_now()) {
        let out = lock.lock().unwrap().exchange.cancel().send;
        if !out.is_empty() {
            send_frames(&mesh, out, now);
        }
    }
    Json(status_body(true)).into_response()
}
async fn unpair(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = admin(&state, &headers).await {
        return e;
    }
    match configured_path() {
        None => return Json(status_body(true)).into_response(),
        Some((_, false)) => {
            return conflict(
                "This pairing comes from GS_STATION_PAIRING_FILE. Remove that setting from the service and restart.",
            );
        }
        Some((_, true)) => {}
    }
    let dir = managed_dir();
    match tokio::task::spawn_blocking(move || remove(&dir)).await {
        Ok(Ok(())) => {
            log::warn!("Station pairing removed from the UI");
            Json(status_body(true)).into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "unpair failed").into_response(),
    }
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/station/pairing", get(get_status))
        .route("/api/station/pairing/start", post(start_pairing))
        .route("/api/station/pairing/confirm", post(confirm_pairing))
        .route("/api/station/pairing/cancel", post(cancel_pairing))
        .route("/api/station/pairing/unpair", post(unpair))
}

#[cfg(test)]
mod pending_tests {
    use super::*;
    #[test]
    fn deferred_pairing_frames_are_kept_in_order_until_the_radio_takes_them() {
        let mut pending = std::collections::VecDeque::new();
        add_pending(&mut pending, vec![b"KEY".to_vec(), b"BUNDLE".to_vec()]);
        let mut sent = Vec::new();
        assert!(!flush(&mut pending, |f| { sent.push(f.to_vec()); false }));
        assert_eq!(pending.len(), 2, "a refused frame is never dropped");
        // A retried request's identical answer replaces, not duplicates.
        add_pending(&mut pending, vec![b"BUNDLE".to_vec()]);
        assert_eq!(pending, [b"KEY".to_vec(), b"BUNDLE".to_vec()]);
        let mut accepted = Vec::new();
        assert!(flush(&mut pending, |f| { accepted.push(f.to_vec()); true }));
        assert_eq!(accepted, [b"KEY".to_vec(), b"BUNDLE".to_vec()]);
        assert!(pending.is_empty());
        for i in 0..10u8 {
            add_pending(&mut pending, vec![vec![i]]);
        }
        assert_eq!(pending.len(), PENDING_LIMIT, "bounded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bundle(side: Side) -> Bundle {
        Bundle {
            side,
            seed: [7; 32],
            installation_id: "0b8f1c7e-3c1d-4d6a-9a52-2f9a3c3f8e11".into(),
            own_url: if side == Side::Primary {
                "https://gs.local"
            } else {
                "http://rrls.local:3000"
            }
            .into(),
            peer_url: if side == Side::Primary {
                "http://rrls.local:3000"
            } else {
                "https://gs.local"
            }
            .into(),
        }
    }
    #[test]
    fn bundles_without_urls_save_and_old_pairing_files_still_load() {
        let root =
            std::env::temp_dir().join(format!("gs-pair-nourl-{:016x}", rand::random::<u64>()));
        // Paired over the radio with no URL known on either side.
        let mut none = bundle(Side::Rrls);
        none.own_url = String::new();
        none.peer_url = String::new();
        save(&root.join("none"), &none).unwrap();
        let loaded = super::super::Pairing::from_file(&root.join("none/pairing.json")).unwrap();
        assert!(loaded.public.peers[0].urls.is_empty());
        // A pairing.json written by the provisioning tool / earlier builds.
        let old = root.join("old");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("identity.seed"), [9u8; 32]).unwrap();
        std::fs::write(
            old.join("pairing.json"),
            r#"{"version":1,"installation_id":"0b8f1c7e-3c1d-4d6a-9a52-2f9a3c3f8e11","station_id":"rrls","signing_seed_file":"identity.seed","scheme":"https","port":443,"peers":[{"id":"primary","role":"primary","urls":["https://192.168.3.7"]}]}"#,
        )
        .unwrap();
        let old = super::super::Pairing::from_file(&old.join("pairing.json")).unwrap();
        assert_eq!(old.public.peers[0].urls, vec!["https://192.168.3.7".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saved_bundles_load_like_the_provisioning_tool_and_never_overwrite() {
        let root =
            std::env::temp_dir().join(format!("gs-pair-save-{:016x}", rand::random::<u64>()));
        let primary = root.join("primary");
        let rrls = root.join("rrls");
        save(&primary, &bundle(Side::Primary)).unwrap();
        save(&rrls, &bundle(Side::Rrls)).unwrap();
        let a = super::super::Pairing::from_file(&primary.join("pairing.json")).unwrap();
        let b = super::super::Pairing::from_file(&rrls.join("pairing.json")).unwrap();
        assert_eq!(a.public.verify_key, b.public.verify_key);
        assert_eq!(
            fingerprint(&a.public.verify_key),
            fingerprint(&b.public.verify_key)
        );
        assert_eq!(a.public.peers[0].id, "rrls");
        assert_eq!(b.public.peers[0].urls, vec!["https://gs.local".to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(primary.join("identity.seed"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let before = std::fs::read(primary.join("identity.seed")).unwrap();
        assert!(save(&primary, &bundle(Side::Primary)).is_err());
        assert_eq!(
            std::fs::read(primary.join("identity.seed")).unwrap(),
            before
        );
        // Same tokens verify on both sides: the pair shares one identity.
        let claims = super::super::Claims {
            installation_id: a.public.installation_id.clone(),
            issuer: "primary".into(),
            username: "operator".into(),
            issued_at_ms: 1,
            expires_at_ms: 10,
            nonce: "n".into(),
            roles: vec![],
            permissions: Default::default(),
            command_access: Default::default(),
            calibration_access: Default::default(),
            remembered: false,
        };
        assert!(b.verify(&a.token(&claims).unwrap(), 5).is_ok());
        remove(&primary).unwrap();
        assert!(!primary.exists());
        remove(&primary).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("staging"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
