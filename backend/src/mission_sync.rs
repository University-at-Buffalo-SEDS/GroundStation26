//! Paired calibration history travels below live telemetry over the RRLS mesh.
//! Authentication is application-level and independent of TLS/proxy certificates.
use crate::{loadcell, state::AppState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use paired_settings::{Document, coordinator::Coordinator, radio::RadioSync};
use ring::signature::KeyPair;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

static RUNTIME: OnceLock<Arc<Mutex<Runtime>>> = OnceLock::new();
struct Runtime {
    installation: String,
    actors: Vec<String>,
    coordinator: Coordinator,
    radio: RadioSync,
    path: PathBuf,
    dirty: bool,
    needs_publication: bool,
    started: Instant,
    last_error: Option<String>,
}
impl Runtime {
    fn new(
        installation: String,
        station: String,
        peers: Vec<String>,
        snapshot: BTreeMap<String, Value>,
        bootstrap: bool,
        path: PathBuf,
    ) -> anyhow::Result<Self> {
        let mut actors = peers.clone();
        actors.push(station.clone());
        let (coordinator, dirty) = if path.exists() {
            (
                Coordinator::load(&path, &installation, &station, &actors)?,
                false,
            )
        } else {
            (
                Coordinator::new(
                    installation.clone(),
                    station.clone(),
                    snapshot,
                    bootstrap,
                    &actors,
                )?,
                true,
            )
        };
        let radio = RadioSync::new(installation.clone(), station, peers)?;
        Ok(Self {
            installation,
            actors,
            coordinator,
            radio,
            path,
            dirty,
            needs_publication: false,
            started: Instant::now(),
            last_error: None,
        })
    }
    fn checkpoint(&mut self) -> anyhow::Result<()> {
        if self.dirty {
            self.coordinator.save(&self.path)?;
            self.dirty = false;
        }
        Ok(())
    }
    fn validate(
        cfg: &loadcell::LoadcellCalibrationFile,
        key: &str,
        document: &Document,
    ) -> anyhow::Result<()> {
        let live = loadcell::mission::snapshot(cfg)?;
        let expected = live
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("Unsupported calibration document"))?;
        for revision in &document.heads {
            loadcell::mission::replace(cfg, key, expected, &revision.value)?;
        }
        Ok(())
    }
    fn observe(&mut self, cfg: &loadcell::LoadcellCalibrationFile) -> anyhow::Result<()> {
        if self
            .coordinator
            .observe(&loadcell::mission::snapshot(cfg)?, &self.actors)?
        {
            self.dirty = true;
        }
        self.checkpoint()
    }
    fn receive(
        &mut self,
        cfg: &loadcell::LoadcellCalibrationFile,
        wire: &[u8],
        now: u64,
        verify_key: &[u8],
    ) -> anyhow::Result<()> {
        if let Some((key, document)) =
            self.radio
                .receive(self.coordinator.store(), wire, now, verify_key)?
        {
            Self::validate(cfg, &key, &document)?;
            if self
                .coordinator
                .merge(&self.installation, &key, &document, &self.actors)?
            {
                self.dirty = true;
            }
            self.checkpoint()?;
        }
        Ok(())
    }
    fn resolve(
        &mut self,
        cfg: &loadcell::LoadcellCalibrationFile,
        request: Resolve,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            request.expected.heads.len() > 1,
            "Review a current conflict before resolving it"
        );
        let mut next = self.coordinator.clone();
        let document =
            next.resolve(&request.key, &request.expected, request.value, &self.actors)?;
        Self::validate(cfg, &request.key, &document)?;
        // A failed API save cannot leave a queued resolution that applies later.
        next.save(&self.path)?;
        self.coordinator = next;
        self.dirty = false;
        Ok(())
    }
}
#[derive(Serialize)]
struct Status {
    enabled: bool,
    link_available: bool,
    pending: usize,
    conflicts: usize,
    waiting_for_idle: bool,
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    key: String,
    expected: Document,
    value: Value,
}

fn step(
    runtime: &mut Runtime,
    state: &Arc<AppState>,
    mesh: &crate::rrls::Shared,
    signer: &ring::signature::Ed25519KeyPair,
) -> anyhow::Result<()> {
    let now = runtime
        .started
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let current = state.loadcell_calibration.lock().unwrap().clone();
    runtime.observe(&current)?;
    let frames = {
        let mut link = mesh.lock().unwrap();
        (0..4)
            .filter_map(|_| link.take_settings())
            .collect::<Vec<_>>()
    };
    let mut reception_error = None;
    for wire in frames {
        match runtime.receive(&current, &wire, now, signer.public_key().as_ref()) {
            Ok(()) => {
                // Only authenticated, protocol-valid reception establishes presence.
                let mut link = mesh.lock().unwrap();
                let link_now = link.now();
                link.authenticated_settings_peer(link_now);
            }
            Err(error) => reception_error = Some(error),
        }
    }
    // A receive may have merged history before its first checkpoint failed.
    // Never apply or advertise that history until a durable retry succeeds.
    runtime.checkpoint()?;
    for pending in runtime.coordinator.pending() {
        // Same order as the sequence engine. Hold GSE and calibration locks
        // across the safe-state check, CAS and durable file replacement.
        let mut gse = state.gse.lock().unwrap();
        if !gse.calibration_import_allowed() {
            break;
        }
        let auto_zero = state.auto_zero.lock().unwrap();
        if auto_zero.pending() {
            break;
        }
        let mut calibration = state.loadcell_calibration.lock().unwrap();
        let preview = loadcell::mission::replace(
            &calibration,
            &pending.key,
            &pending.expected_local,
            &pending.revision.value,
        )?;
        let source_changed = preview.kg50_input != calibration.kg50_input;
        let recording = state.recording_status.lock().unwrap();
        if source_changed && recording.mode != crate::telemetry_db::RecordingModeWire::Idle {
            break;
        }
        loadcell::mission::apply_and_save(&mut calibration, &pending, loadcell::save)?;
        drop(calibration);
        drop(recording);
        drop(auto_zero);
        if source_changed {
            state.clear_kg50_samples();
            gse.observe_mass(None);
        }
        runtime.coordinator.applied(&pending)?;
        runtime.dirty = true;
        runtime.needs_publication = !crate::rrls::is_rrls();
        runtime.checkpoint()?;
    }
    if runtime.needs_publication {
        let calibration = state.loadcell_calibration.lock().unwrap().clone();
        match crate::web::publish_daq_calibration(state, &calibration) {
            Ok(()) => runtime.needs_publication = false,
            Err(error) => reception_error = Some(anyhow::anyhow!(error)),
        }
    }
    runtime
        .radio
        .poll(runtime.coordinator.store(), now, signer, |_, wire| {
            let mut link = mesh.lock().unwrap();
            let link_now = link.now();
            link.queue_settings(wire, link_now)
        })?;
    if let Some(error) = reception_error {
        return Err(error);
    }
    Ok(())
}

/// HITL retains its existing configuration and sequencing behavior.
pub fn start(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    if cfg!(feature = "hitl_mode") {
        return None;
    }
    let pairing = match crate::station_pairing::Pairing::load() {
        Ok(Some(pairing)) => Arc::new(pairing),
        Ok(None) => return None,
        Err(error) => {
            log::warn!("Paired settings unavailable: {error}");
            return None;
        }
    };
    let Some(mesh) = crate::rrls::settings_transport() else {
        log::warn!("Paired settings require the primary or RRLS radio role");
        return None;
    };
    let snapshot = match loadcell::mission::snapshot(&state.loadcell_calibration.lock().unwrap()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            log::warn!("Invalid local calibration prevents paired settings: {error}");
            return None;
        }
    };
    let path = std::env::var_os("GS_MISSION_SETTINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("data").join(format!(
                "mission-settings-{}.json",
                pairing.public.station_id
            ))
        });
    let runtime = match Runtime::new(
        pairing.public.installation_id.clone(),
        pairing.public.station_id.clone(),
        pairing.public.peers.iter().map(|p| p.id.clone()).collect(),
        snapshot,
        !crate::rrls::is_rrls(),
        path,
    ) {
        Ok(runtime) => Arc::new(Mutex::new(runtime)),
        Err(error) => {
            log::warn!("Paired settings history unavailable: {error}");
            return None;
        }
    };
    if RUNTIME.set(runtime.clone()).is_err() {
        return None;
    }
    let mut shutdown = state.shutdown_subscribe();
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = interval.tick() => {
                    let runtime = runtime.clone(); let state = state.clone();
                    let mesh = mesh.clone(); let pairing = pairing.clone();
                    // File synchronization, serialization and signatures never
                    // block a Tokio network worker or run under the radio lock.
                    if let Err(error) = tokio::task::spawn_blocking(move || {
                        let mut runtime = runtime.lock().unwrap();
                        let result = step(&mut runtime, &state, &mesh, pairing.mission_signing_key());
                        let message = result.as_ref().err().map(|_| "Shared calibration could not be synchronized".to_owned());
                        if runtime.last_error != message {
                            if let Err(error) = &result { log::warn!("Paired calibration synchronization deferred: {error}"); }
                            runtime.last_error = message;
                        }
                    }).await { log::error!("Paired settings worker failed: {error}"); }
                }
            }
        }
    }))
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) =
        crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData).await
    {
        return error;
    }
    let Some(runtime) = RUNTIME.get() else {
        return Json(Status {
            enabled: false,
            link_available: false,
            pending: 0,
            conflicts: 0,
            waiting_for_idle: false,
            error: None,
        })
        .into_response();
    };
    let runtime = runtime.lock().unwrap();
    let pending = runtime.coordinator.pending().len();
    Json(Status {
        enabled: true,
        link_available: crate::rrls::settings_transport().is_some_and(|shared| {
            let mesh = shared.lock().unwrap();
            mesh.settings_available(mesh.now())
        }),
        pending,
        conflicts: runtime
            .coordinator
            .store()
            .documents()
            .values()
            .filter(|d| d.heads.len() > 1)
            .count(),
        waiting_for_idle: pending > 0 && !state.gse.lock().unwrap().calibration_import_allowed(),
        error: runtime.last_error.clone(),
    })
    .into_response()
}
async fn conflicts(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData)
            .await
        {
            Ok(p) => p,
            Err(e) => return e,
        };
    if !principal.calibration_access.view {
        return (
            StatusCode::FORBIDDEN,
            "Calibration view permission required",
        )
            .into_response();
    }
    let Some(runtime) = RUNTIME.get() else {
        return Json(BTreeMap::<String, Document>::new()).into_response();
    };
    let runtime = runtime.lock().unwrap();
    Json(
        runtime
            .coordinator
            .store()
            .documents()
            .iter()
            .filter(|(_, d)| d.heads.len() > 1)
            .map(|(k, d)| (k.clone(), d.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
    .into_response()
}
async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Resolve>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, crate::auth::Permission::ViewData)
            .await
        {
            Ok(p) => p,
            Err(e) => return e,
        };
    if !principal.calibration_access.edit {
        return (
            StatusCode::FORBIDDEN,
            "Calibration edit permission required",
        )
            .into_response();
    }
    let Some(runtime) = RUNTIME.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Paired settings are disabled",
        )
            .into_response();
    };
    let result = {
        let mut runtime = runtime.lock().unwrap();
        let calibration = state.loadcell_calibration.lock().unwrap();
        runtime.resolve(&calibration, request)
    };
    match result {
        Ok(()) => Json(serde_json::json!({"resolved":true})).into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/mission-settings", get(status))
        .route("/api/mission-settings/conflicts", get(conflicts))
        .route("/api/mission-settings/resolve", post(resolve))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrls::{Mesh, Role};
    use ring::signature::Ed25519KeyPair;
    use serde_json::json;

    struct Station {
        id: &'static str,
        runtime: Runtime,
        calibration: loadcell::LoadcellCalibrationFile,
        mesh: Mesh,
        calibration_path: PathBuf,
    }
    fn station(id: &'static str, directory: &std::path::Path, mass: f32) -> Station {
        let calibration = loadcell::LoadcellCalibrationFile {
            full_mass_kg: Some(mass),
            ..Default::default()
        };
        let calibration_path = directory.join(format!("{id}-calibration.json"));
        loadcell::save_at(&calibration, &calibration_path).unwrap();
        let peer = if id == "gs" { "rrls" } else { "gs" };
        let runtime = Runtime::new(
            "launch".into(),
            id.into(),
            vec![peer.into()],
            loadcell::mission::snapshot(&calibration).unwrap(),
            id == "gs",
            directory.join(format!("{id}-history.json")),
        )
        .unwrap();
        Station {
            id,
            runtime,
            calibration,
            calibration_path,
            mesh: Mesh::new(
                if id == "gs" {
                    Role::Primary
                } else {
                    Role::Rrls
                },
                if id == "gs" { 7 } else { 8 },
            ),
        }
    }
    fn directory(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mission-runtime-{name}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }
    fn exchange(stations: &mut [Station; 2], key: &Ed25519KeyPair, base: u64) {
        let mut delayed = Vec::new();
        let mut sent = 0;
        let mut attempts = 0;
        for tick in 0..3000 {
            let now = base + tick * 100;
            // The main station's direct rocket path stays healthy; settings
            // synchronization must also work while RRLS is a standby observer.
            let _ = stations[0]
                .mesh
                .incoming(true, &[b'R', b'S', 1, 0, 0, 0, 0, 0, 0], now);
            for (direction, station) in stations.iter_mut().enumerate() {
                station.runtime.observe(&station.calibration).unwrap();
                station.mesh.tick(now);
                let runtime = &mut station.runtime;
                runtime
                    .radio
                    .poll(runtime.coordinator.store(), now, key, |_, wire| {
                        attempts += 1;
                        if attempts % 17 == 0 {
                            return false;
                        }
                        station.mesh.queue_settings(wire, now)
                    })
                    .unwrap();
                for _ in 0..4 {
                    let Some(frame) = station.mesh.pop(now) else {
                        break;
                    };
                    sent += 1;
                    if sent % 13 != 0 {
                        delayed.push((1 - direction, frame.command, frame.bytes));
                    }
                }
            }
            if tick % 3 == 0 {
                for (to, command, wire) in delayed.drain(..).rev() {
                    stations[to].mesh.incoming(command, &wire, now);
                    if tick % 7 == 0 {
                        stations[to].mesh.incoming(command, &wire, now);
                    }
                }
            }
            for station in &mut *stations {
                while let Some(wire) = station.mesh.take_settings() {
                    station
                        .runtime
                        .receive(&station.calibration, &wire, now, key.public_key().as_ref())
                        .unwrap();
                    station.mesh.authenticated_settings_peer(now);
                }
                for pending in station.runtime.coordinator.pending() {
                    loadcell::mission::apply_and_save(&mut station.calibration, &pending, |cfg| {
                        loadcell::save_at(cfg, &station.calibration_path)
                    })
                    .unwrap();
                    station.runtime.coordinator.applied(&pending).unwrap();
                    station.runtime.dirty = true;
                    station.runtime.checkpoint().unwrap();
                }
            }
        }
        assert!(attempts > 17 && sent > 13);
        assert_eq!(
            stations[0].runtime.coordinator.store(),
            stations[1].runtime.coordinator.store()
        );
        assert!(!stations[0].mesh.active(base + 299_900));
        assert!(!stations[1].mesh.active(base + 299_900));
    }
    #[test]
    fn real_mesh_calibration_sync_retains_offline_conflicts_restart_and_explicit_resolution() {
        let dir = directory("paired");
        let key = Ed25519KeyPair::from_seed_unchecked(&[47; 32]).unwrap();
        let mut stations = [station("gs", &dir, 17.0), station("rrls", &dir, 10.0)];
        exchange(&mut stations, &key, 0);
        assert_eq!(stations[1].calibration.full_mass_kg, Some(17.0));
        for (station, mass) in stations.iter_mut().zip([20.0, 21.0]) {
            station.calibration.full_mass_kg = Some(mass);
            loadcell::save_at(&station.calibration, &station.calibration_path).unwrap();
            station.runtime.observe(&station.calibration).unwrap();
        }
        exchange(&mut stations, &key, 400_000);
        let reviewed = stations[0]
            .runtime
            .coordinator
            .store()
            .document("calibration/full_mass_kg")
            .unwrap()
            .clone();
        assert_eq!(reviewed.heads.len(), 2);
        assert_eq!(stations[0].calibration.full_mass_kg, Some(20.0));
        assert_eq!(stations[1].calibration.full_mass_kg, Some(21.0));
        let rrls = &mut stations[1];
        assert_eq!(rrls.id, "rrls");
        rrls.calibration =
            serde_json::from_slice(&std::fs::read(&rrls.calibration_path).unwrap()).unwrap();
        rrls.runtime = Runtime::new(
            "launch".into(),
            "rrls".into(),
            vec!["gs".into()],
            loadcell::mission::snapshot(&rrls.calibration).unwrap(),
            false,
            dir.join("rrls-history.json"),
        )
        .unwrap();
        assert_eq!(
            rrls.runtime
                .coordinator
                .store()
                .document("calibration/full_mass_kg")
                .unwrap()
                .heads
                .len(),
            2
        );
        {
            let primary = &mut stations[0];
            let history_before = primary.runtime.coordinator.store().clone();
            let original_path = primary.runtime.path.clone();
            let blocked = dir.join("not-a-directory");
            std::fs::write(&blocked, b"block").unwrap();
            primary.runtime.path = blocked.join("history.json");
            assert!(
                primary
                    .runtime
                    .resolve(
                        &primary.calibration,
                        Resolve {
                            key: "calibration/full_mass_kg".into(),
                            expected: reviewed.clone(),
                            value: json!(21.0),
                        }
                    )
                    .is_err()
            );
            assert_eq!(primary.runtime.coordinator.store(), &history_before);
            primary.runtime.path = original_path;
        }
        let primary = &mut stations[0];
        primary
            .runtime
            .resolve(
                &primary.calibration,
                Resolve {
                    key: "calibration/full_mass_kg".into(),
                    expected: reviewed.clone(),
                    value: json!(21.0),
                },
            )
            .unwrap();
        let before = std::fs::read(&primary.runtime.path).unwrap();
        assert!(
            primary
                .runtime
                .resolve(
                    &primary.calibration,
                    Resolve {
                        key: "calibration/full_mass_kg".into(),
                        expected: reviewed,
                        value: json!(20.0),
                    }
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&primary.runtime.path).unwrap(), before);
        exchange(&mut stations, &key, 800_000);
        assert_eq!(stations[0].calibration.full_mass_kg, Some(21.0));
        assert_eq!(stations[1].calibration.full_mass_kg, Some(21.0));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn invalid_typed_document_is_rejected_without_checkpoint_or_live_mutation() {
        let dir = directory("invalid");
        let mut station = station("gs", &dir, 17.0);
        station.runtime.checkpoint().unwrap();
        let before = std::fs::read(&station.runtime.path).unwrap();
        let doc = Document {
            heads: vec![paired_settings::Revision {
                clock: BTreeMap::from([("rrls".into(), 1)]),
                value: json!({"not_a_mass":true}),
            }],
        };
        assert!(Runtime::validate(&station.calibration, "calibration/full_mass_kg", &doc).is_err());
        assert_eq!(std::fs::read(&station.runtime.path).unwrap(), before);
        assert_eq!(station.calibration.full_mass_kg, Some(17.0));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
