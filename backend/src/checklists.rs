//! Shared checklists: per-item transactions, durable completion, administrator templates.
use crate::{
    auth::{Permission, StreamRole},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: String,
    pub text: String,
    pub checked: bool,
    #[serde(default)]
    pub completed_by: Option<String>,
    #[serde(default)]
    pub completed_at_ms: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Checklist {
    pub id: String,
    pub title: String,
    pub required_before_launch: bool,
    pub items: Vec<Item>,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub revision: u64,
    pub enforce_before_launch: bool,
    pub checklists: Vec<Checklist>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            revision: 0,
            enforce_before_launch: false,
            checklists: [
                (
                    "setup",
                    "Setup",
                    true,
                    vec![
                        "Pressure transducer reading checked",
                        "Gas lines and connections inspected",
                    ],
                ),
                (
                    "pre-launch",
                    "Pre-launch",
                    true,
                    vec![
                        "Personnel clear of valves and vent/dump paths",
                        "Communications and abort procedure checked",
                    ],
                ),
                ("launch", "Launch", true, vec!["Launch procedure reviewed"]),
                (
                    "recovery",
                    "Recovery",
                    false,
                    vec!["Recovery procedure reviewed"],
                ),
            ]
            .into_iter()
            .map(|(id, title, required, items)| Checklist {
                id: id.into(),
                title: title.into(),
                required_before_launch: required,
                items: items
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| Item {
                        id: format!("{id}-{i}"),
                        text: text.into(),
                        checked: false,
                        completed_by: None,
                        completed_at_ms: None,
                    })
                    .collect(),
            })
            .collect(),
        }
    }
}
impl Document {
    fn validate(&self) -> Result<(), String> {
        if self.checklists.len() > 32 {
            return Err("Maximum 32 checklists".into());
        }
        let mut ids = HashSet::new();
        let mut count = 0;
        for list in &self.checklists {
            if !valid_id(&list.id)
                || !ids.insert(&list.id)
                || list.title.trim().is_empty()
                || list.title.len() > 120
            {
                return Err("Checklist IDs must be unique and titles must be 1–120 bytes".into());
            }
            if list.items.len() > 128 || (list.required_before_launch && list.items.is_empty()) {
                return Err(
                    "Required checklists need items; maximum 128 items per checklist".into(),
                );
            }
            let mut item_ids = HashSet::new();
            for item in &list.items {
                count += 1;
                if !valid_id(&item.id)
                    || !item_ids.insert(&item.id)
                    || item.text.trim().is_empty()
                    || item.text.len() > 500
                {
                    return Err(
                        "Item IDs must be unique within a checklist and text must be 1–500 bytes"
                            .into(),
                    );
                }
            }
        }
        if count > 1024 {
            return Err("Maximum 1024 checklist items".into());
        }
        if self.enforce_before_launch && !self.checklists.iter().any(|c| c.required_before_launch) {
            return Err(
                "Select at least one required checklist before enabling launch enforcement".into(),
            );
        }
        Ok(())
    }
    fn launch_block(&self) -> Option<String> {
        if !self.enforce_before_launch {
            return None;
        }
        let missing: Vec<_> = self
            .checklists
            .iter()
            .filter(|c| c.required_before_launch && c.items.iter().any(|i| !i.checked))
            .map(|c| c.title.as_str())
            .collect();
        (!missing.is_empty()).then(|| {
            format!(
                "Complete required checklists before launch: {}",
                missing.join(", ")
            )
        })
    }
    fn check(&mut self, request: &Check, actor: &str, now: i64) -> Result<(), String> {
        let item = self
            .checklists
            .iter_mut()
            .find(|c| c.id == request.checklist_id)
            .and_then(|c| c.items.iter_mut().find(|i| i.id == request.item_id))
            .ok_or("Checklist item no longer exists; refresh the page")?;
        item.checked = request.checked;
        item.completed_by = request.checked.then(|| actor.into());
        item.completed_at_ms = request.checked.then_some(now);
        Ok(())
    }
    fn edit(&mut self, mut next: Document) -> Result<(), String> {
        if next.revision != self.revision {
            return Err("Checklists changed on another device; reload before saving".into());
        }
        next.validate()?;
        // Completion metadata is server-owned. Changed wording invalidates completion.
        for list in &mut next.checklists {
            for item in &mut list.items {
                let old = self
                    .checklists
                    .iter()
                    .find(|c| c.id == list.id)
                    .and_then(|c| {
                        c.items
                            .iter()
                            .find(|i| i.id == item.id && i.text == item.text)
                    });
                if let Some(old) = old {
                    *item = old.clone()
                } else {
                    item.checked = false;
                    item.completed_by = None;
                    item.completed_at_ms = None;
                }
            }
        }
        *self = next;
        Ok(())
    }
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
struct Store {
    path: PathBuf,
    document: Document,
}
impl Store {
    fn open(path: PathBuf) -> Result<Self, String> {
        let document = match std::fs::read(&path) {
            Ok(data) => {
                if data.len() > 1_048_576 {
                    return Err("Checklist file exceeds 1 MiB".into());
                }
                serde_json::from_slice(&data)
                    .map_err(|e| format!("Cannot read checklist data: {e}"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Document::default(),
            Err(e) => return Err(e.to_string()),
        };
        document.validate()?;
        Ok(Self { path, document })
    }
    fn update(
        &mut self,
        operation: impl FnOnce(&mut Document) -> Result<(), String>,
    ) -> Result<Document, String> {
        let mut next = self.document.clone();
        operation(&mut next)?;
        next.validate()?;
        next.revision = self
            .document
            .revision
            .checked_add(1)
            .ok_or("Checklist revision overflow")?;
        let data = serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let tmp = parent.join(format!(
            ".checklists-{}-{}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(tmp);
            return Err(e.to_string());
        }
        self.document = next.clone();
        Ok(next)
    }
}
static STORE: OnceLock<Mutex<Result<Store, String>>> = OnceLock::new();
fn store() -> &'static Mutex<Result<Store, String>> {
    STORE.get_or_init(|| {
        Mutex::new(Store::open(
            std::env::var_os("GS_CHECKLISTS_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("backend/data/checklists.json")),
        ))
    })
}
fn access<T>(f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let mut guard = store().lock().map_err(|_| "Checklist store unavailable")?;
    match guard.as_mut() {
        Ok(store) => f(store),
        Err(error) => Err(error.clone()),
    }
}
pub fn launch_block() -> Option<String> {
    access(|s| Ok(s.document.launch_block()))
        .unwrap_or_else(|error| Some(format!("Checklist storage unavailable: {error}")))
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Check {
    checklist_id: String,
    item_id: String,
    checked: bool,
}
async fn principal(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    admin: bool,
) -> Result<crate::auth::AuthPrincipal, Response> {
    let p = crate::web::authorize_headers(state, headers, Permission::ViewData).await?;
    if p.anonymous || (admin && !p.roles.contains(&StreamRole::Admin)) {
        return Err((
            StatusCode::FORBIDDEN,
            "Sign in with the required checklist access",
        )
            .into_response());
    }
    Ok(p)
}
fn response(result: Result<Document, String>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => (StatusCode::CONFLICT, e).into_response(),
    }
}
async fn get_lists(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = principal(&state, &headers, false).await {
        return e;
    }
    response(access(|s| Ok(s.document.clone())))
}
async fn check_item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Check>,
) -> Response {
    let p = match principal(&state, &headers, false).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    let result = access(|s| {
        s.update(|doc| {
            doc.check(
                &request,
                p.username.as_deref().unwrap_or("operator"),
                crate::telemetry_task::get_current_timestamp_ms() as i64,
            )
        })
    });
    if result.is_ok() {
        crate::sequences::refresh_action_policy_now(&state);
    }
    response(result)
}
async fn edit_lists(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Document>,
) -> Response {
    if let Err(e) = principal(&state, &headers, true).await {
        return e;
    }
    let result = access(|s| s.update(|doc| doc.edit(request)));
    if result.is_ok() {
        crate::sequences::refresh_action_policy_now(&state);
    }
    response(result)
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/checklists", get(get_lists).post(edit_lists))
        .route("/api/checklists/check", post(check_item))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        std::env::temp_dir()
            .join(format!("gs-checklists-{}", rand::random::<u64>()))
            .join("checklists.json")
    }
    #[test]
    fn concurrent_item_updates_survive_restart_and_record_actors() {
        let path = temp();
        let mut store = Store::open(path.clone()).unwrap();
        for (i, user) in [(0, "alice"), (1, "bob")] {
            store
                .update(|d| {
                    d.check(
                        &Check {
                            checklist_id: "setup".into(),
                            item_id: format!("setup-{i}"),
                            checked: true,
                        },
                        user,
                        123,
                    )
                })
                .unwrap();
        }
        let recovered = Store::open(path.clone()).unwrap();
        assert!(
            recovered.document.checklists[0]
                .items
                .iter()
                .all(|i| i.checked)
        );
        assert_eq!(
            recovered.document.checklists[0].items[1]
                .completed_by
                .as_deref(),
            Some("bob")
        );
        assert_eq!(recovered.document.revision, 2);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn launch_requirements_are_modular_and_recovery_does_not_block() {
        let mut d = Document::default();
        d.enforce_before_launch = true;
        assert!(d.launch_block().is_some());
        for l in &mut d.checklists {
            if l.required_before_launch {
                for i in &mut l.items {
                    i.checked = true
                }
            }
        }
        assert!(d.launch_block().is_none());
        assert!(!d.checklists[3].items[0].checked);
        let mut extra = d.checklists[0].clone();
        extra.id = "extra".into();
        extra.title = "Extra procedure".into();
        extra.items[0].checked = false;
        d.checklists.push(extra);
        assert!(d.launch_block().unwrap().contains("Extra procedure"));
    }
    #[test]
    fn stale_admin_edits_cannot_erase_checkmarks_and_changed_text_invalidates_completion() {
        let mut d = Document::default();
        let stale = d.clone();
        d.check(
            &Check {
                checklist_id: "setup".into(),
                item_id: "setup-0".into(),
                checked: true,
            },
            "alice",
            1,
        )
        .unwrap();
        d.revision += 1;
        assert!(d.edit(stale).is_err());
        let mut next = d.clone();
        next.checklists[0].items[0].text = "Different safety check".into();
        d.edit(next).unwrap();
        assert!(!d.checklists[0].items[0].checked);
        assert!(d.checklists[0].items[0].completed_by.is_none());
    }
    #[test]
    fn invalid_templates_and_corrupt_store_are_not_overwritten() {
        let mut d = Document::default();
        d.checklists.push(d.checklists[0].clone());
        assert!(d.validate().is_err());
        let path = temp();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"invalid").unwrap();
        assert!(Store::open(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn failed_persistence_does_not_change_memory() {
        let path = temp();
        std::fs::create_dir_all(&path).unwrap();
        let mut store = Store {
            path: path.clone(),
            document: Document::default(),
        };
        assert!(
            store
                .update(|d| {
                    d.enforce_before_launch = true;
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(store.document, Document::default());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn signed_in_collaborators_can_check_but_only_admins_can_edit_templates() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"),serde_json::json!({"users":[
            {"username":"administrator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"collaborator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}
        ]}).to_string()).unwrap();
        for (token, name) in [
            ("checklist-admin", "administrator"),
            ("checklist-crew", "collaborator"),
        ] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,0,0,9999999999999)").bind(token).bind(name).execute(&state.auth_db).await.unwrap();
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer checklist-crew".parse().unwrap(),
        );
        assert!(principal(&state, &headers, false).await.is_ok());
        assert_eq!(
            principal(&state, &headers, true)
                .await
                .err()
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer checklist-admin".parse().unwrap(),
        );
        assert!(principal(&state, &headers, true).await.is_ok());
        assert!(principal(&state, &HeaderMap::new(), false).await.is_err());
    }
}
