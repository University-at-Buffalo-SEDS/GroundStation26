//! Per-account client display preferences (currently only the watch
//! companion). Each signed-in account reads and replaces only its own opaque
//! document; nothing here touches telemetry, commands, radio or launch state.
use crate::{auth::Permission, state::AppState};
use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

/// Namespaces a client may store. Each is a small client-owned document.
const NAMESPACES: &[&str] = &["watch"];
const MAX_DOCUMENT_BYTES: usize = 8 * 1024;
const MAX_ACCOUNTS: usize = 512;
const MAX_FILE_BYTES: u64 =
    (MAX_DOCUMENT_BYTES * NAMESPACES.len() * MAX_ACCOUNTS + 64 * 1024) as u64;
const MAX_DEPTH: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub revision: u64,
    pub settings: Option<Value>,
    pub updated_at_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub expected_revision: u64,
    pub settings: Value,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u8,
    /// account -> namespace -> document
    accounts: BTreeMap<String, BTreeMap<String, Document>>,
}

#[derive(Debug, PartialEq)]
enum SaveError {
    Conflict(Document),
    Invalid(&'static str),
    Full,
    Io(String),
}

fn depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn validate(settings: &Value) -> Result<(), &'static str> {
    if !settings.is_object() {
        return Err("Settings must be a JSON object");
    }
    if depth(settings) > MAX_DEPTH {
        return Err("Settings are nested too deeply");
    }
    let bytes = serde_json::to_vec(settings).map_err(|_| "Settings are not serializable")?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err("Settings are too large");
    }
    Ok(())
}

struct Store {
    path: PathBuf,
    file: File,
}

impl Store {
    fn open(path: PathBuf) -> anyhow::Result<Self> {
        let file = match fs::File::open(&path) {
            Ok(handle) => {
                anyhow::ensure!(
                    handle.metadata()?.len() <= MAX_FILE_BYTES,
                    "Client settings file too large"
                );
                let mut bytes = Vec::new();
                handle.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
                let file: File = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(file.version == 1, "Unsupported client settings file");
                file
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File {
                version: 1,
                ..File::default()
            },
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, file })
    }

    fn get(&self, account: &str, namespace: &str) -> Document {
        self.file
            .accounts
            .get(account)
            .and_then(|m| m.get(namespace))
            .cloned()
            .unwrap_or_default()
    }

    fn save(
        &mut self,
        account: &str,
        namespace: &str,
        update: Update,
        now_ms: i64,
    ) -> Result<Document, SaveError> {
        validate(&update.settings).map_err(SaveError::Invalid)?;
        let current = self.get(account, namespace);
        if update.expected_revision != current.revision {
            return Err(SaveError::Conflict(current));
        }
        if !self.file.accounts.contains_key(account) && self.file.accounts.len() >= MAX_ACCOUNTS {
            return Err(SaveError::Full);
        }
        let next = Document {
            revision: current
                .revision
                .checked_add(1)
                .ok_or(SaveError::Invalid("Revision exhausted"))?,
            settings: Some(update.settings),
            updated_at_ms: Some(now_ms),
        };
        let previous = self
            .file
            .accounts
            .entry(account.to_owned())
            .or_default()
            .insert(namespace.to_owned(), next.clone());
        if let Err(e) = self.persist() {
            // Keep memory identical to disk.
            let docs = self.file.accounts.get_mut(account).expect("inserted above");
            match previous {
                Some(doc) => {
                    docs.insert(namespace.to_owned(), doc);
                }
                None => {
                    docs.remove(namespace);
                    if docs.is_empty() {
                        self.file.accounts.remove(account);
                    }
                }
            }
            return Err(SaveError::Io(e.to_string()));
        }
        Ok(next)
    }

    fn persist(&self) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(&self.file)?;
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
        result
    }
}

static STORE: OnceLock<Mutex<Result<Store, String>>> = OnceLock::new();
fn store() -> &'static Mutex<Result<Store, String>> {
    STORE.get_or_init(|| {
        let path = std::env::var_os("GS_CLIENT_SETTINGS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("backend/data/client_settings.json"));
        Mutex::new(Store::open(path).map_err(|e| {
            log::warn!("Cannot load client settings: {e}");
            "Client settings storage unavailable".to_string()
        }))
    })
}

async fn account(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    namespace: &str,
) -> Result<String, Box<Response>> {
    let principal = crate::web::authorize_headers(state, headers, Permission::ViewData)
        .await
        .map_err(Box::new)?;
    if !NAMESPACES.contains(&namespace) {
        return Err(Box::new(
            (StatusCode::NOT_FOUND, "Unknown client settings").into_response(),
        ));
    }
    match principal.username {
        Some(name) if !principal.anonymous && !name.is_empty() => Ok(name),
        _ => Err(Box::new(
            (StatusCode::FORBIDDEN, "Sign in to sync client settings").into_response(),
        )),
    }
}

async fn get_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(namespace): UrlPath<String>,
) -> Response {
    let user = match account(&state, &headers, &namespace).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    match store()
        .lock()
        .map_err(|_| "Client settings storage unavailable".to_string())
        .and_then(|g| {
            g.as_ref()
                .map(|s| s.get(&user, &namespace))
                .map_err(Clone::clone)
        }) {
        Ok(document) => Json(document).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

async fn set_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(namespace): UrlPath<String>,
    Json(update): Json<Update>,
) -> Response {
    let user = match account(&state, &headers, &namespace).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let result = tokio::task::spawn_blocking(move || {
        let mut guard = store()
            .lock()
            .map_err(|_| SaveError::Io("Client settings storage unavailable".into()))?;
        let store = guard.as_mut().map_err(|e| SaveError::Io(e.clone()))?;
        store.save(&user, &namespace, update, now_ms)
    })
    .await;
    match result {
        Ok(Ok(document)) => Json(document).into_response(),
        Ok(Err(SaveError::Conflict(current))) => {
            (StatusCode::CONFLICT, Json(current)).into_response()
        }
        Ok(Err(SaveError::Invalid(reason))) => (StatusCode::BAD_REQUEST, reason).into_response(),
        Ok(Err(SaveError::Full)) => (
            StatusCode::INSUFFICIENT_STORAGE,
            "Too many accounts with saved client settings",
        )
            .into_response(),
        Ok(Err(SaveError::Io(error))) => {
            log::warn!("Cannot save client settings: {error}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Cannot save client settings",
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot save client settings",
        )
            .into_response(),
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/api/account/client-settings/{namespace}",
        get(get_settings).post(set_settings),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp() -> PathBuf {
        std::env::temp_dir()
            .join(format!("gs-client-settings-{}", rand::random::<u64>()))
            .join("client_settings.json")
    }

    #[test]
    fn accounts_are_isolated_and_revisions_guard_writes() {
        let path = temp();
        let mut store = Store::open(path.clone()).unwrap();
        assert_eq!(store.get("alice", "watch"), Document::default());
        let saved = store
            .save(
                "alice",
                "watch",
                Update {
                    expected_revision: 0,
                    settings: json!({"units":"imperial"}),
                },
                5,
            )
            .unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(store.get("bob", "watch"), Document::default());
        // Stale writer gets the current document back.
        let conflict = store
            .save(
                "alice",
                "watch",
                Update {
                    expected_revision: 0,
                    settings: json!({"units":"metric"}),
                },
                6,
            )
            .unwrap_err();
        assert_eq!(conflict, SaveError::Conflict(saved.clone()));
        // Durable across reopen.
        let reopened = Store::open(path.clone()).unwrap();
        assert_eq!(reopened.get("alice", "watch"), saved);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_non_objects_oversize_and_deep_documents_without_mutation() {
        let path = temp();
        let mut store = Store::open(path.clone()).unwrap();
        for bad in [
            json!([1, 2]),
            json!("x"),
            json!({"pad": "x".repeat(MAX_DOCUMENT_BYTES)}),
            json!({"a":{"b":{"c":{"d":{"e":{"f":{"g":{"h":{"i":1}}}}}}}}}),
        ] {
            let err = store
                .save(
                    "alice",
                    "watch",
                    Update {
                        expected_revision: 0,
                        settings: bad,
                    },
                    1,
                )
                .unwrap_err();
            assert!(matches!(err, SaveError::Invalid(_)));
        }
        assert_eq!(store.get("alice", "watch"), Document::default());
        assert!(!path.exists());
    }

    #[test]
    fn failed_persist_keeps_memory_unchanged() {
        let dir = temp();
        // Parent path is a file, so create_dir_all fails.
        fs::create_dir_all(dir.parent().unwrap().parent().unwrap()).unwrap();
        fs::write(dir.parent().unwrap(), b"not a directory").unwrap();
        let mut store = Store {
            path: dir.clone(),
            file: File {
                version: 1,
                ..File::default()
            },
        };
        let err = store
            .save(
                "alice",
                "watch",
                Update {
                    expected_revision: 0,
                    settings: json!({}),
                },
                1,
            )
            .unwrap_err();
        assert!(matches!(err, SaveError::Io(_)));
        assert_eq!(store.get("alice", "watch"), Document::default());
        assert!(store.file.accounts.is_empty());
        let _ = fs::remove_file(dir.parent().unwrap());
    }

    #[test]
    fn unknown_namespaces_are_not_listed() {
        assert_eq!(NAMESPACES, &["watch"]);
    }
}
