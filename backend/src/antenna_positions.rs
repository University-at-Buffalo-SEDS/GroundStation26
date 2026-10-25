//! Administrator-saved station positions. Static fallbacks never create a GPS lock.
use super::{AltitudeReference, Fix};
use crate::{
    auth::{Permission, StreamRole},
    state::AppState,
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
const MAX_BYTES: u64 = 16 * 1024;
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Manual,
    DeviceLocation,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coordinate {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub altitude_m: Option<f64>,
    #[serde(default)]
    pub accuracy_m: Option<f64>,
    #[serde(default)]
    pub source: Source,
}
impl Coordinate {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.latitude.is_finite()
                && (-90.0..=90.0).contains(&self.latitude)
                && self.longitude.is_finite()
                && (-180.0..=180.0).contains(&self.longitude)
                && self
                    .altitude_m
                    .is_none_or(|v| v.is_finite() && (-15_000.0..=100_000.0).contains(&v))
                && self.accuracy_m.is_none_or(|v| v.is_finite() && v >= 0.0),
            "Invalid station coordinate"
        );
        Ok(())
    }
    fn fix(&self, station: &str) -> Fix {
        Fix {
            latitude: self.latitude,
            longitude: self.longitude,
            altitude_m: self.altitude_m,
            altitude_reference: AltitudeReference::Unknown,
            accuracy_m: self.accuracy_m,
            geoid_separation_m: None,
            age_ms: 0,
            stale: false,
            manual: true,
            source: format!(
                "Saved {station} ({})",
                match self.source {
                    Source::Manual => "manual",
                    Source::DeviceLocation => "device location",
                }
            ),
        }
    }
}
/// Saved configuration only: no live GPS, local revision, clock or editor.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedPositions {
    pub ground_station: Option<Coordinate>,
    pub rrls: Option<Coordinate>,
}
impl SavedPositions {
    pub fn validate(&self) -> anyhow::Result<()> {
        for position in [&self.ground_station, &self.rrls].into_iter().flatten() {
            position.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub version: u8,
    pub revision: u64,
    pub ground_station: Option<Coordinate>,
    pub rrls: Option<Coordinate>,
    pub updated_at_ms: Option<i64>,
    pub updated_by: Option<String>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            ground_station: None,
            rrls: None,
            updated_at_ms: None,
            updated_by: None,
        }
    }
}
impl Document {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "Unsupported station position file");
        for p in [&self.ground_station, &self.rrls].into_iter().flatten() {
            p.validate()?;
        }
        anyhow::ensure!(
            self.updated_by.as_ref().is_none_or(|v| v.len() <= 192),
            "Invalid position editor"
        );
        Ok(())
    }
}
struct Store {
    path: PathBuf,
    document: Document,
}
impl Store {
    fn snapshot(&self) -> SavedPositions {
        SavedPositions {
            ground_station: self.document.ground_station.clone(),
            rrls: self.document.rrls.clone(),
        }
    }
    fn compare_and_save(
        &mut self,
        expected: &SavedPositions,
        next: SavedPositions,
    ) -> anyhow::Result<()> {
        next.validate()?;
        anyhow::ensure!(
            self.snapshot() == *expected,
            "Saved station positions changed locally"
        );
        self.save(
            Update {
                expected_revision: self.document.revision,
                ground_station: next.ground_station,
                rrls: next.rrls,
            },
            "paired station",
        )?;
        Ok(())
    }
    fn open(path: PathBuf) -> anyhow::Result<Self> {
        let document = match fs::File::open(&path) {
            Ok(file) => {
                anyhow::ensure!(
                    file.metadata()?.len() <= MAX_BYTES,
                    "Station position file too large"
                );
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                anyhow::ensure!(
                    bytes.len() as u64 <= MAX_BYTES,
                    "Station position file too large"
                );
                serde_json::from_slice(&bytes)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Document::default(),
            Err(e) => return Err(e.into()),
        };
        let value = Self { path, document };
        value.document.validate()?;
        Ok(value)
    }
    fn save(&mut self, request: Update, editor: &str) -> anyhow::Result<Document> {
        anyhow::ensure!(
            request.expected_revision == self.document.revision,
            "Positions changed elsewhere; reload before saving"
        );
        let revision = self
            .document
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Position revision exhausted"))?;
        let next = Document {
            version: 1,
            revision,
            ground_station: request.ground_station,
            rrls: request.rrls,
            updated_at_ms: Some(
                (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            ),
            updated_by: Some(editor.into()),
        };
        next.validate()?;
        let bytes = serde_json::to_vec_pretty(&next)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Station position file too large"
        );
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary = self
            .path
            .with_extension(format!("{}.{nonce}.tmp", std::process::id()));
        let result = (|| -> anyhow::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        self.document = next;
        Ok(self.document.clone())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub expected_revision: u64,
    pub ground_station: Option<Coordinate>,
    pub rrls: Option<Coordinate>,
}
static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
fn store() -> &'static Mutex<Option<Store>> {
    STORE.get_or_init(|| {
        let path = std::env::var_os("GS_STATION_POSITIONS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/station_positions.json")
            });
        Mutex::new(match Store::open(path) {
            Ok(store) => Some(store),
            Err(e) => {
                log::warn!("Cannot load saved station positions: {e}");
                None
            }
        })
    })
}
pub fn saved_snapshot() -> anyhow::Result<SavedPositions> {
    store()
        .lock()
        .unwrap()
        .as_ref()
        .map(Store::snapshot)
        .ok_or_else(|| anyhow::anyhow!("Cannot load saved station positions"))
}
/// The content comparison and durable write share one lock with administrator
/// edits. A concurrent local edit remains intact and must be resolved explicitly.
pub fn compare_and_save(expected: &SavedPositions, next: SavedPositions) -> anyhow::Result<()> {
    store()
        .lock()
        .unwrap()
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("Cannot load saved station positions"))?
        .compare_and_save(expected, next)
}
pub fn fallbacks() -> (Option<Fix>, Option<Fix>) {
    let store = store().lock().unwrap();
    let Some(store) = store.as_ref() else {
        return (None, None);
    };
    (
        store
            .document
            .ground_station
            .as_ref()
            .map(|p| p.fix("GroundStation")),
        store.document.rrls.as_ref().map(|p| p.fix("RRLS")),
    )
}
pub async fn get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    match store().lock().unwrap().as_ref() {
        Some(s) => Json(s.document.clone()).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot load saved station positions",
        )
            .into_response(),
    }
}
pub async fn set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Update>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(p) => p,
            Err(e) => return e,
        };
    if principal.anonymous || !principal.roles.contains(&StreamRole::Admin) {
        return (StatusCode::FORBIDDEN, "Application administrator required").into_response();
    }
    let editor = principal.username.unwrap_or_else(|| "administrator".into());
    match tokio::task::spawn_blocking(move || {
        let mut guard = store().lock().unwrap();
        let s = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Cannot load saved station positions"))?;
        if request.expected_revision != s.document.revision {
            return Ok::<_, anyhow::Error>(Err(()));
        }
        Ok(Ok(s.save(request, &editor)?))
    })
    .await
    {
        Ok(Ok(Ok(document))) => Json(document).into_response(),
        Ok(Ok(Err(()))) => (
            StatusCode::CONFLICT,
            "Positions changed elsewhere; reload before saving",
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot save station positions",
        )
            .into_response(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn position() -> Coordinate {
        Coordinate {
            latitude: 43.0,
            longitude: -78.8,
            altitude_m: None,
            accuracy_m: None,
            source: Source::Manual,
        }
    }
    fn update(revision: u64) -> Update {
        Update {
            expected_revision: revision,
            ground_station: Some(position()),
            rrls: None,
        }
    }
    #[test]
    fn saved_coordinates_survive_restart_and_old_edits_cannot_overwrite() {
        let dir = std::env::temp_dir().join(format!(
            "station-positions-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("positions.json");
        let mut s = Store::open(path.clone()).unwrap();
        assert_eq!(s.document.revision, 0);
        let saved = s.save(update(0), "admin").unwrap();
        assert_eq!(saved.revision, 1);
        assert!(s.save(update(0), "other").is_err());
        let s = Store::open(path).unwrap();
        assert_eq!(s.document.revision, 1);
        assert_eq!(s.document.ground_station.unwrap().latitude, 43.0);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn bad_coordinates_and_oversized_files_are_rejected() {
        let mut p = position();
        for v in [f64::NAN, f64::INFINITY, 91.0] {
            p.latitude = v;
            assert!(p.validate().is_err());
        }
        let dir = std::env::temp_dir().join(format!(
            "station-position-bounds-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("positions.json");
        fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(Store::open(path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn static_position_is_labelled_manual_and_has_no_gps_datum() {
        let fix = position().fix("RRLS");
        assert!(fix.manual && !fix.stale);
        assert_eq!(fix.altitude_reference, AltitudeReference::Unknown);
        assert!(super::super::encode_position(&fix).is_none());
    }
    #[test]
    fn paired_content_excludes_local_metadata_and_rejects_concurrent_edits() {
        let dir = std::env::temp_dir().join(format!(
            "station-position-pairing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("positions.json");
        let mut s = Store::open(path.clone()).unwrap();
        let expected = s.snapshot();
        let remote = SavedPositions {
            ground_station: None,
            rrls: Some(position()),
        };
        s.save(update(0), "local admin").unwrap();
        assert!(s.compare_and_save(&expected, remote.clone()).is_err());
        assert_eq!(s.document.updated_by.as_deref(), Some("local admin"));
        let current = s.snapshot();
        s.compare_and_save(&current, remote.clone()).unwrap();
        assert_eq!(Store::open(path.clone()).unwrap().snapshot(), remote);
        let value = serde_json::to_value(&remote).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert!(value.get("revision").is_none());
        // A failed durable replacement cannot publish a new in-memory value.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(
            s.compare_and_save(&remote, SavedPositions::default())
                .is_err()
        );
        assert_eq!(s.snapshot(), remote);
        fs::remove_dir_all(dir).unwrap();
    }
}
