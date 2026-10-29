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
#[cfg(not(feature = "hitl_mode"))]
pub(crate) mod paired;
// Shared wire definitions also serve the bounded paired-checklist model.
// They are data only: importing a completion never invokes an engine transition.
pub use paired_settings::checklists::{CompletionSource, Sequence};
#[cfg(any(test, not(feature = "hitl_mode")))]
#[derive(Clone, Default, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionAction {
    #[default]
    Passed,
    Override {
        operator: String,
    },
    Clear,
}
#[cfg(any(test, not(feature = "hitl_mode")))]
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionEvent {
    pub id: String,
    pub sequence: Sequence,
    pub at_ms: i64,
    #[serde(default)]
    pub action: CompletionAction,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: String,
    pub text: String,
    pub checked: bool,
    #[serde(default)]
    pub complete_on: Option<Sequence>,
    #[serde(default)]
    pub completion_source: Option<CompletionSource>,
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
pub struct Signoff {
    pub checklist_id: String,
    pub checklist_title: String,
    pub signed_by: String,
    pub signed_at_ms: i64,
    pub content_digest: String,
    pub signed_revision: u64,
    #[serde(default)]
    pub invalidated_at_ms: Option<i64>,
    #[serde(default)]
    pub invalidation_reason: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub revision: u64,
    #[serde(default)]
    pub signoffs: Vec<Signoff>,
    #[serde(default)]
    completion_events: Vec<String>,
    pub enforce_before_launch: bool,
    pub checklists: Vec<Checklist>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            revision: 0,
            signoffs: Vec::new(),
            completion_events: Vec::new(),
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
                        complete_on: None,
                        completion_source: None,
                        completed_by: None,
                        completed_at_ms: None,
                    })
                    .collect(),
            })
            .chain(std::iter::once(Checklist {
                id: "sequence-tests".into(),
                title: "Sequence tests".into(),
                required_before_launch: true,
                items: [
                    (
                        "valve-self-test",
                        "Valve self-test passed",
                        Sequence::ValveSelfTest,
                    ),
                    (
                        "nitrogen-cold-flow",
                        "Nitrogen cold-flow test passed",
                        Sequence::NitrogenTest,
                    ),
                ]
                .into_iter()
                .map(|(id, text, sequence)| Item {
                    id: id.into(),
                    text: text.into(),
                    checked: false,
                    complete_on: Some(sequence),
                    completion_source: None,
                    completed_by: None,
                    completed_at_ms: None,
                })
                .collect(),
            }))
            .collect(),
        }
    }
}
impl Document {
    fn validate(&self) -> Result<(), String> {
        if self.signoffs.len() > 1024
            || self.signoffs.iter().any(|signature| {
                !valid_id(&signature.checklist_id)
                    || signature.signed_by.is_empty()
                    || signature.signed_by.len() > 256
                    || signature.checklist_title.len() > 120
                    || signature.content_digest.len() != 64
                    || !signature
                        .content_digest
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err("Invalid checklist sign-off audit history".into());
        }
        if self.completion_events.len() > 128
            || self.completion_events.iter().any(|id| !valid_id(id))
            || self.completion_events.iter().collect::<HashSet<_>>().len()
                != self.completion_events.len()
        {
            return Err("Invalid sequence completion receipt history".into());
        }
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
            .filter(|c| {
                c.required_before_launch
                    && (c.items.iter().any(|i| !i.checked) || !self.signed_off(c))
            })
            .map(|c| c.title.as_str())
            .collect();
        (!missing.is_empty()).then(|| {
            format!(
                "Complete and sign off required checklists before launch: {}",
                missing.join(", ")
            )
        })
    }
    fn signed_off(&self, list: &Checklist) -> bool {
        let digest = checklist_digest(list);
        !list.items.is_empty()
            && list.items.iter().all(|item| item.checked)
            && self.signoffs.iter().any(|signature| {
                signature.checklist_id == list.id
                    && signature.invalidated_at_ms.is_none()
                    && signature.content_digest == digest
            })
    }
    fn preserve_signoffs(&mut self, old: &Document, now: i64) {
        // Audit metadata comes only from the server. Changes invalidate an
        // approval permanently, even if the prior contents are restored later.
        self.signoffs = old.signoffs.clone();
        for signature in &mut self.signoffs {
            if signature.invalidated_at_ms.is_none()
                && !self.checklists.iter().any(|list| {
                    list.id == signature.checklist_id
                        && checklist_digest(list) == signature.content_digest
                })
            {
                signature.invalidated_at_ms = Some(now);
                signature.invalidation_reason =
                    Some("Checklist contents or completion changed".into());
            }
        }
    }
    fn sign(&mut self, request: &Sign, actor: &str, now: i64) -> Result<(), String> {
        if request.revision != self.revision {
            return Err("Checklists changed; review the latest version before signing".into());
        }
        let list = self
            .checklists
            .iter()
            .find(|list| list.id == request.checklist_id)
            .ok_or("Checklist no longer exists")?;
        if list.items.is_empty() || list.items.iter().any(|item| !item.checked) {
            return Err("Complete every checklist item before signing off".into());
        }
        if self.signed_off(list) {
            return Err("Checklist is already signed off".into());
        }
        if self.signoffs.len() >= 1024 {
            return Err("Sign-off audit history is full; archive this checklist document before adding approvals".into());
        }
        self.signoffs.push(Signoff {
            checklist_id: list.id.clone(),
            checklist_title: list.title.clone(),
            signed_by: actor.into(),
            signed_at_ms: now,
            content_digest: checklist_digest(list),
            signed_revision: self.revision,
            invalidated_at_ms: None,
            invalidation_reason: None,
        });
        Ok(())
    }
    fn check(&mut self, request: &Check, actor: &str, now: i64) -> Result<(), String> {
        let item = self
            .checklists
            .iter_mut()
            .find(|c| c.id == request.checklist_id)
            .and_then(|c| c.items.iter_mut().find(|i| i.id == request.item_id))
            .ok_or("Checklist item no longer exists; refresh the page")?;
        item.checked = request.checked;
        item.completion_source = request.checked.then_some(if item.complete_on.is_some() {
            CompletionSource::ManualOverride
        } else {
            CompletionSource::Operator
        });
        item.completed_by = request.checked.then(|| actor.into());
        item.completed_at_ms = request.checked.then_some(now);
        Ok(())
    }
    #[cfg(any(test, not(feature = "hitl_mode")))]
    fn complete_sequence(&mut self, sequence: Sequence, now: i64) -> bool {
        let mut changed = false;
        for item in self.checklists.iter_mut().flat_map(|list| &mut list.items) {
            if item.complete_on == Some(sequence)
                && (!item.checked
                    || item.completion_source != Some(CompletionSource::AutomatedSequence))
            {
                item.checked = true;
                item.completed_by = Some(sequence.actor().into());
                item.completed_at_ms = Some(now);
                item.completion_source = Some(CompletionSource::AutomatedSequence);
                changed = true;
            }
        }
        changed
    }
    #[cfg(any(test, not(feature = "hitl_mode")))]
    fn clear_sequence_completions(&mut self) -> bool {
        let mut changed = false;
        for item in self.checklists.iter_mut().flat_map(|list| &mut list.items) {
            if item.complete_on.is_some() && item.checked {
                item.checked = false;
                item.completed_by = None;
                item.completed_at_ms = None;
                item.completion_source = None;
                changed = true;
            }
        }
        changed
    }
    fn edit(&mut self, mut next: Document) -> Result<(), String> {
        if next.revision != self.revision {
            return Err("Checklists changed on another device; reload before saving".into());
        }
        // Receipt history is server-owned, including when an older UI omits it.
        next.completion_events = self.completion_events.clone();
        next.signoffs = self.signoffs.clone();
        next.validate()?;
        // Completion metadata is server-owned. Changed wording invalidates completion.
        for list in &mut next.checklists {
            for item in &mut list.items {
                let old = self
                    .checklists
                    .iter()
                    .find(|c| c.id == list.id)
                    .and_then(|c| {
                        c.items.iter().find(|i| {
                            i.id == item.id
                                && i.text == item.text
                                && i.complete_on == item.complete_on
                        })
                    });
                if let Some(old) = old {
                    *item = old.clone()
                } else {
                    item.checked = false;
                    item.completion_source = None;
                    item.completed_by = None;
                    item.completed_at_ms = None;
                }
            }
        }
        *self = next;
        Ok(())
    }
}
fn checklist_digest(list: &Checklist) -> String {
    ring::digest::digest(
        &ring::digest::SHA256,
        &serde_json::to_vec(list).expect("checklist serialization"),
    )
    .as_ref()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
const MAX_FILE_BYTES: usize = 1_048_576;
const FILE_LIMIT_ERROR: &str = "Checklist file exceeds 1 MiB";

// Bound serialized bytes too: JSON escapes can expand an otherwise valid template.
#[derive(Default)]
struct DocumentWriter(Vec<u8>);
impl std::io::Write for DocumentWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_FILE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FILE_LIMIT_ERROR,
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn serialize_document(document: &Document) -> Result<Vec<u8>, String> {
    let mut writer = DocumentWriter::default();
    serde_json::to_writer_pretty(&mut writer, document).map_err(|error| error.to_string())?;
    Ok(writer.0)
}
struct Store {
    path: PathBuf,
    document: Document,
}
impl Store {
    fn open(path: PathBuf) -> Result<Self, String> {
        let document = match std::fs::File::open(&path) {
            Ok(file) => {
                use std::io::Read;
                if file.metadata().map_err(|error| error.to_string())?.len() > MAX_FILE_BYTES as u64
                {
                    return Err(FILE_LIMIT_ERROR.into());
                }
                let mut data = Vec::new();
                file.take(MAX_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut data)
                    .map_err(|error| error.to_string())?;
                if data.len() > MAX_FILE_BYTES {
                    return Err(FILE_LIMIT_ERROR.into());
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
        let signatures = next.signoffs.clone();
        next.preserve_signoffs(
            &self.document,
            crate::telemetry_task::get_current_timestamp_ms() as i64,
        );
        // Only a sign operation appends; template editing replaces client audit
        // with server history before reaching here.
        next.signoffs
            .extend(signatures.into_iter().skip(self.document.signoffs.len()));
        next.validate()?;
        next.revision = self
            .document
            .revision
            .checked_add(1)
            .ok_or("Checklist revision overflow")?;
        self.persist(next)
    }
    fn persist(&mut self, next: Document) -> Result<Document, String> {
        next.validate()?;
        let data = serialize_document(&next)?;
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
    #[cfg(not(feature = "hitl_mode"))]
    if let Some(reason) = paired::extra_launch_block() {
        return Some(reason);
    }
    access(|s| Ok(s.document.launch_block()))
        .unwrap_or_else(|error| Some(format!("Checklist storage unavailable: {error}")))
}
/// Called only after a successful, durably recorded sequence completion.
/// It is deliberately event-driven: refreshing/restarting never rechecks a manually cleared item.
#[cfg(not(feature = "hitl_mode"))]
pub(crate) fn sequence_completed(event: &CompletionEvent) -> Result<(), String> {
    access(|store| store.record_sequence(event))
}
#[cfg(any(test, not(feature = "hitl_mode")))]
impl Store {
    fn record_sequence(&mut self, event: &CompletionEvent) -> Result<(), String> {
        if !valid_id(&event.id) {
            return Err("Invalid sequence event ID".into());
        }
        if self.document.completion_events.contains(&event.id) {
            return Ok(());
        }
        self.update(|document| {
            match &event.action {
                CompletionAction::Passed => {
                    document.complete_sequence(event.sequence, event.at_ms);
                }
                CompletionAction::Override { operator } => {
                    for item in document
                        .checklists
                        .iter_mut()
                        .flat_map(|list| &mut list.items)
                    {
                        if item.complete_on == Some(event.sequence) {
                            item.checked = true;
                            item.completed_by = Some(operator.clone());
                            item.completed_at_ms = Some(event.at_ms);
                            item.completion_source = Some(CompletionSource::ManualOverride);
                        }
                    }
                }
                CompletionAction::Clear => {
                    for item in document
                        .checklists
                        .iter_mut()
                        .flat_map(|list| &mut list.items)
                    {
                        if item.complete_on == Some(event.sequence) {
                            item.checked = false;
                            item.completed_by = None;
                            item.completed_at_ms = None;
                            item.completion_source = None;
                        }
                    }
                }
            }
            if document.completion_events.len() == 128 {
                document.completion_events.remove(0);
            }
            document.completion_events.push(event.id.clone());
            Ok(())
        })?;
        Ok(())
    }
}
#[cfg(not(feature = "hitl_mode"))]
pub(crate) fn reset_sequence_completions() -> Result<(), String> {
    if paired::requested() {
        return Ok(());
    } // Explicit reset events are durable in local qualification.
    access(|store| {
        let mut next = store.document.clone();
        if next.clear_sequence_completions() {
            store.update(|document| {
                *document = next;
                Ok(())
            })?;
        }
        Ok(())
    })
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Check {
    checklist_id: String,
    item_id: String,
    checked: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Sign {
    checklist_id: String,
    revision: u64,
}
async fn sign_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Sign>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SignChecklists).await {
            Ok(principal) if !principal.anonymous => principal,
            Ok(_) => {
                return (StatusCode::FORBIDDEN, "Sign in to sign off a checklist").into_response();
            }
            Err(error) => return error,
        };
    #[cfg(not(feature = "hitl_mode"))]
    if let Some(reason) = paired::extra_launch_block() {
        return (StatusCode::CONFLICT, reason).into_response();
    }
    #[cfg(not(feature = "hitl_mode"))]
    if paired::requested() {
        let result = paired::sign(
            &request,
            principal.username.as_deref().expect("authenticated signer"),
            crate::telemetry_task::get_current_timestamp_ms() as i64,
        )
        .await;
        if result.is_ok() {
            crate::sequences::refresh_action_policy_now(&state);
        }
        return response(result);
    }
    let result = access(|store| {
        store.update(|document| {
            document.sign(
                &request,
                principal
                    .username
                    .as_deref()
                    .expect("authenticated checklist signer"),
                crate::telemetry_task::get_current_timestamp_ms() as i64,
            )
        })
    });
    if result.is_ok() {
        crate::sequences::refresh_action_policy_now(&state);
    }
    response(result)
}
async fn export_lists(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = principal(&state, &headers, false).await {
        return error;
    }
    match access(|store| Ok(store.document.clone())) {
        Ok(document) => (
            [(
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=checklists.json",
            )],
            Json(document),
        )
            .into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}
fn signoff_csv(document: &Document) -> String {
    fn cell(value: &str) -> String {
        let safe = if value.trim_start().starts_with(['=', '+', '-', '@']) {
            format!("'{value}")
        } else {
            value.into()
        };
        format!("\"{}\"", safe.replace('"', "\"\""))
    }
    let mut csv = "checklist_id,checklist_title,status,signed_by,signed_at_ms,signed_revision,content_digest,invalidated_at_ms,invalidation_reason\r\n".to_owned();
    for list in &document.checklists {
        let history: Vec<_> = document
            .signoffs
            .iter()
            .filter(|signature| signature.checklist_id == list.id)
            .collect();
        if history.is_empty() {
            csv.push_str(&format!(
                "{},{},unsigned,,,,,,\r\n",
                cell(&list.id),
                cell(&list.title)
            ));
        }
    }
    for signature in &document.signoffs {
        let columns: [&str; 9] = [
            &signature.checklist_id,
            &signature.checklist_title,
            if signature.invalidated_at_ms.is_some() {
                "invalidated"
            } else {
                "signed_off"
            },
            &signature.signed_by,
            &signature.signed_at_ms.to_string(),
            &signature.signed_revision.to_string(),
            &signature.content_digest,
            &signature
                .invalidated_at_ms
                .map(|value| value.to_string())
                .unwrap_or_default(),
            signature.invalidation_reason.as_deref().unwrap_or(""),
        ];
        let row = columns.map(cell).join(",");
        csv.push_str(&row);
        csv.push_str("\r\n");
    }
    csv
}
async fn export_signoffs(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = principal(&state, &headers, false).await {
        return error;
    }
    match access(|store| Ok(signoff_csv(&store.document))) {
        Ok(csv) => (
            [
                (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=checklist-signoffs.csv",
                ),
            ],
            csv,
        )
            .into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
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
    #[cfg(not(feature = "hitl_mode"))]
    if paired::requested() {
        let result = paired::check(
            &request,
            p.username.as_deref().unwrap_or("operator"),
            crate::telemetry_task::get_current_timestamp_ms() as i64,
        )
        .await;
        if result.is_ok() {
            crate::sequences::refresh_action_policy_now(&state);
        }
        return response(result);
    }
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
    #[cfg(not(feature = "hitl_mode"))]
    if paired::requested() {
        let result = paired::edit(request, &state).await;
        if result.is_ok() {
            crate::sequences::refresh_action_policy_now(&state);
        }
        return response(result);
    }
    let result = access(|s| s.update(|doc| doc.edit(request)));
    if result.is_ok() {
        crate::sequences::refresh_action_policy_now(&state);
    }
    response(result)
}
pub fn routes() -> Router<Arc<AppState>> {
    let router = Router::new()
        .route("/api/checklists", get(get_lists).post(edit_lists))
        .route("/api/checklists/check", post(check_item))
        .route("/api/checklists/sign", post(sign_list))
        .route("/api/checklists/export", get(export_lists))
        .route("/api/checklists/export.csv", get(export_signoffs));
    #[cfg(not(feature = "hitl_mode"))]
    let router = router.merge(paired::routes());
    router
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
    fn serialized_file_limit_rejects_escaped_templates_without_mutation() {
        let path = temp();
        let mut store = Store::open(path.clone()).unwrap();
        store.update(|_| Ok(())).unwrap();
        let before = store.document.clone();
        let bytes = std::fs::read(&path).unwrap();
        let mut large = Document::default();
        large.checklists.clear();
        for list in 0..8 {
            large.checklists.push(Checklist {
                id: format!("list-{list}"),
                title: "Escaped content".into(),
                required_before_launch: false,
                items: (0..128)
                    .map(|item| Item {
                        id: format!("item-{item}"),
                        text: "\u{1}".repeat(500),
                        checked: false,
                        complete_on: None,
                        completion_source: None,
                        completed_by: None,
                        completed_at_ms: None,
                    })
                    .collect(),
            });
        }
        large.validate().unwrap();
        let error = store
            .update(|next| {
                *next = large;
                Ok(())
            })
            .unwrap_err();
        assert!(error.contains(FILE_LIMIT_ERROR), "{error}");
        assert_eq!(store.document, before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(Store::open(path.clone()).unwrap().document, before);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn serialized_writer_accepts_exact_limit_and_refuses_extra_byte() {
        use std::io::Write;
        let mut writer = DocumentWriter::default();
        for _ in 0..128 {
            writer.write_all(&[b' '; 8192]).unwrap();
        }
        assert_eq!(writer.0.len(), MAX_FILE_BYTES);
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.0.len(), MAX_FILE_BYTES);
    }
    #[test]
    fn read_accepts_exact_limit_and_rejects_larger_file_without_overwrite() {
        let path = temp();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = Document::default();
        let mut data = serialize_document(&original).unwrap();
        data.resize(MAX_FILE_BYTES, b' ');
        std::fs::write(&path, &data).unwrap();
        assert_eq!(Store::open(path.clone()).unwrap().document, original);
        data.push(b' ');
        std::fs::write(&path, &data).unwrap();
        assert!(
            Store::open(path.clone())
                .err()
                .unwrap()
                .contains(FILE_LIMIT_ERROR)
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (MAX_FILE_BYTES + 1) as u64
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
        assert!(
            d.launch_block().is_some(),
            "completed items still need approval"
        );
        let required: Vec<_> = d
            .checklists
            .iter()
            .filter(|list| list.required_before_launch)
            .map(|list| list.id.clone())
            .collect();
        for checklist_id in required {
            d.sign(
                &Sign {
                    checklist_id,
                    revision: d.revision,
                },
                "reviewer",
                100,
            )
            .unwrap();
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
    #[test]
    fn approvals_are_revision_bound_durable_and_cannot_be_forged_or_resurrected() {
        let path = temp();
        let mut store = Store::open(path.clone()).unwrap();
        assert!(store.document.signoffs.is_empty());
        assert!(
            store
                .update(|doc| doc.sign(
                    &Sign {
                        checklist_id: "setup".into(),
                        revision: 0
                    },
                    "reviewer",
                    10
                ))
                .is_err()
        );
        store
            .update(|doc| {
                for item in &mut doc.checklists[0].items {
                    item.checked = true;
                }
                Ok(())
            })
            .unwrap();
        let stale = Sign {
            checklist_id: "setup".into(),
            revision: 0,
        };
        assert!(
            store
                .update(|doc| doc.sign(&stale, "reviewer", 11))
                .is_err()
        );
        let request = Sign {
            checklist_id: "setup".into(),
            revision: store.document.revision,
        };
        let approved = store
            .update(|doc| doc.sign(&request, "reviewer", 123456))
            .unwrap();
        assert!(approved.signed_off(&approved.checklists[0]));
        let reopened = Store::open(path.clone()).unwrap();
        assert_eq!(reopened.document.signoffs[0].signed_by, "reviewer");
        assert_eq!(reopened.document.signoffs[0].signed_at_ms, 123456);
        assert_eq!(
            reopened.document.signoffs[0].signed_revision,
            request.revision
        );
        let export: serde_json::Value =
            serde_json::from_slice(&serialize_document(&reopened.document).unwrap()).unwrap();
        assert_eq!(export["signoffs"][0]["signed_by"], "reviewer");
        assert_eq!(export["signoffs"][0]["signed_at_ms"], 123456);
        let csv = signoff_csv(&reopened.document);
        assert!(csv.contains("reviewer") && csv.contains("123456") && csv.contains("signed_off"));
        let mut forged = approved.clone();
        forged.signoffs[0].signed_by = "forged administrator".into();
        forged.checklists[0].title = "Revised safety procedure".into();
        store.update(|doc| doc.edit(forged)).unwrap();
        assert!(!store.document.signed_off(&store.document.checklists[0]));
        assert_eq!(store.document.signoffs[0].signed_by, "reviewer");
        assert!(store.document.signoffs[0].invalidated_at_ms.is_some());
        store
            .update(|doc| {
                doc.checklists[0].title = approved.checklists[0].title.clone();
                Ok(())
            })
            .unwrap();
        assert!(
            !store.document.signed_off(&store.document.checklists[0]),
            "restoring content cannot resurrect an old approval"
        );
        assert!(serde_json::from_value::<Sign>(serde_json::json!({"checklist_id":"setup","revision":1,"signed_by":"forged","signed_at_ms":1})).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn csv_audit_quotes_cells_and_neutralizes_spreadsheet_formulas() {
        let mut document = Document::default();
        for item in &mut document.checklists[0].items {
            item.checked = true;
        }
        document
            .sign(
                &Sign {
                    checklist_id: "setup".into(),
                    revision: 0,
                },
                "=FORMULA,\"user\"",
                10,
            )
            .unwrap();
        let csv = signoff_csv(&document);
        assert!(csv.contains("\"'=FORMULA,\"\"user\"\"\""));
        assert!(csv.contains("\"setup\",\"Setup\",\"signed_off\""));
    }
    #[test]
    fn any_progress_change_invalidates_only_affected_checklist_approval() {
        let path = temp();
        let mut store = Store::open(path.clone()).unwrap();
        store
            .update(|doc| {
                for list in &mut doc.checklists {
                    for item in &mut list.items {
                        item.checked = true;
                    }
                }
                Ok(())
            })
            .unwrap();
        for checklist_id in ["setup", "pre-launch"] {
            let request = Sign {
                checklist_id: checklist_id.into(),
                revision: store.document.revision,
            };
            store
                .update(|doc| doc.sign(&request, "reviewer", 100))
                .unwrap();
        }
        let request = Check {
            checklist_id: "setup".into(),
            item_id: "setup-0".into(),
            checked: false,
        };
        store
            .update(|doc| doc.check(&request, "operator", 200))
            .unwrap();
        assert!(!store.document.signed_off(&store.document.checklists[0]));
        assert!(store.document.signed_off(&store.document.checklists[1]));
        let mut legacy = serde_json::to_value(Document::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("signoffs");
        assert!(
            serde_json::from_value::<Document>(legacy)
                .unwrap()
                .signoffs
                .is_empty()
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn signed_in_collaborators_can_check_but_only_admins_can_edit_templates() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"),serde_json::json!({"users":[
            {"username":"administrator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"collaborator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"reviewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"sign_checklists":true}}
        ]}).to_string()).unwrap();
        for (token, name) in [
            ("checklist-admin", "administrator"),
            ("checklist-crew", "collaborator"),
            ("checklist-reviewer", "reviewer"),
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
            crate::web::authorize_headers(&state, &headers, Permission::SignChecklists)
                .await
                .err()
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            sign_list(
                State(state.clone()),
                headers.clone(),
                Json(Sign {
                    checklist_id: "setup".into(),
                    revision: 0
                })
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
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
        assert!(
            crate::web::authorize_headers(&state, &headers, Permission::SignChecklists)
                .await
                .is_ok()
        );
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer checklist-reviewer".parse().unwrap(),
        );
        let reviewer = crate::web::authorize_headers(&state, &headers, Permission::SignChecklists)
            .await
            .unwrap();
        assert_eq!(reviewer.username.as_deref(), Some("reviewer"));
        assert!(!reviewer.roles.contains(&StreamRole::Admin));
        assert!(!reviewer.permissions.send_commands);
        let mut accounts: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.0.join("auth.json")).unwrap()).unwrap();
        accounts["users"][2]["permissions"]["sign_checklists"] = serde_json::Value::Bool(false);
        std::fs::write(
            root.0.join("auth.json"),
            serde_json::to_vec(&accounts).unwrap(),
        )
        .unwrap();
        assert_eq!(
            crate::web::authorize_headers(&state, &headers, Permission::SignChecklists)
                .await
                .err()
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "revocation must apply to an existing session"
        );

        assert!(
            crate::web::authorize_headers(&state, &HeaderMap::new(), Permission::SignChecklists)
                .await
                .is_err()
        );
        assert!(principal(&state, &HeaderMap::new(), false).await.is_err());
    }
}

#[cfg(test)]
mod automation_tests {
    use super::*;
    #[test]
    fn successful_events_update_only_linked_items_and_survive_restart() {
        let path =
            std::env::temp_dir().join(format!("gs-automation-{}.json", rand::random::<u64>()));
        let mut store = Store::open(path.clone()).unwrap();
        store
            .update(|d| {
                assert!(d.complete_sequence(Sequence::ValveSelfTest, 10));
                Ok(())
            })
            .unwrap();
        let reopened = Store::open(path.clone()).unwrap();
        let list = reopened
            .document
            .checklists
            .iter()
            .find(|c| c.id == "sequence-tests")
            .unwrap();
        assert!(list.items[0].checked);
        assert!(!list.items[1].checked);
        assert_eq!(
            list.items[0].completion_source,
            Some(CompletionSource::AutomatedSequence)
        );
        assert_eq!(list.items[0].completed_at_ms, Some(10));
        assert_eq!(
            list.items[0].completed_by.as_deref(),
            Some("automated:valve_self_test")
        );
        assert!(
            !store
                .document
                .complete_sequence(Sequence::ValveSelfTest, 20)
        );
        assert_eq!(
            store.document.checklists.last().unwrap().items[0].completed_at_ms,
            Some(10)
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn overrides_are_distinct_and_admin_edits_cannot_forge_success() {
        let mut d = Document::default();
        d.check(
            &Check {
                checklist_id: "sequence-tests".into(),
                item_id: "valve-self-test".into(),
                checked: true,
            },
            "alice",
            1,
        )
        .unwrap();
        assert_eq!(
            d.checklists.last().unwrap().items[0].completion_source,
            Some(CompletionSource::ManualOverride)
        );
        let mut next = d.clone();
        next.checklists.last_mut().unwrap().items[1].checked = true;
        next.checklists.last_mut().unwrap().items[1].completion_source =
            Some(CompletionSource::AutomatedSequence);
        d.edit(next).unwrap();
        assert!(!d.checklists.last().unwrap().items[1].checked);
        assert!(d.complete_sequence(Sequence::ValveSelfTest, 2));
        assert_eq!(
            d.checklists.last().unwrap().items[0].completion_source,
            Some(CompletionSource::AutomatedSequence)
        );
        let mut next = d.clone();
        next.checklists.last_mut().unwrap().items[0].complete_on = Some(Sequence::NitrogenTest);
        d.edit(next).unwrap();
        assert!(!d.checklists.last().unwrap().items[0].checked);
    }
    #[test]
    fn manual_clear_is_not_repopulated_without_a_new_success_and_reset_preserves_manual_items() {
        let mut d = Document::default();
        d.complete_sequence(Sequence::NitrogenTest, 1);
        d.check(
            &Check {
                checklist_id: "setup".into(),
                item_id: "setup-0".into(),
                checked: true,
            },
            "bob",
            2,
        )
        .unwrap();
        d.check(
            &Check {
                checklist_id: "sequence-tests".into(),
                item_id: "nitrogen-cold-flow".into(),
                checked: false,
            },
            "bob",
            3,
        )
        .unwrap();
        let mut restored: Document =
            serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
        assert!(!restored.checklists.last().unwrap().items[1].checked);
        restored.complete_sequence(Sequence::ValveSelfTest, 4);
        assert!(restored.clear_sequence_completions());
        assert!(restored.checklists[0].items[0].checked);
        assert!(
            restored
                .checklists
                .last()
                .unwrap()
                .items
                .iter()
                .all(|i| !i.checked)
        );
    }
}

#[cfg(test)]
mod outbox_receipt_tests {
    use super::*;
    #[test]
    fn replay_after_checkpoint_crash_does_not_undo_a_manual_clear() {
        let path = std::env::temp_dir().join(format!("gs-receipt-{}.json", rand::random::<u128>()));
        let event = CompletionEvent {
            id: "test-unique-event".into(),
            sequence: Sequence::ValveSelfTest,
            at_ms: 12,
            action: CompletionAction::Passed,
        };
        let mut store = Store::open(path.clone()).unwrap();
        store.record_sequence(&event).unwrap();
        store
            .update(|d| {
                d.check(
                    &Check {
                        checklist_id: "sequence-tests".into(),
                        item_id: "valve-self-test".into(),
                        checked: false,
                    },
                    "operator",
                    20,
                )
            })
            .unwrap();
        let mut restarted = Store::open(path.clone()).unwrap();
        let revision = restarted.document.revision;
        restarted.record_sequence(&event).unwrap();
        assert_eq!(restarted.document.revision, revision);
        assert!(!restarted.document.checklists.last().unwrap().items[0].checked);
        // A genuine new run can complete the cleared item.
        restarted
            .record_sequence(&CompletionEvent {
                id: "next-test-event".into(),
                ..event
            })
            .unwrap();
        assert!(restarted.document.checklists.last().unwrap().items[0].checked);
        // An admin template write cannot erase the receipt history.
        let mut edit = restarted.document.clone();
        edit.completion_events.clear();
        restarted.document.edit(edit).unwrap();
        assert_eq!(restarted.document.completion_events.len(), 2);
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod workflow_event_tests {
    use super::*;
    #[test]
    fn fire_clears_only_cold_flow_and_late_receipt_replay_cannot_resurrect_it() {
        let path = std::env::temp_dir().join(format!(
            "gse-fire-checklist-{}.json",
            rand::random::<u128>()
        ));
        let mut store = Store::open(path.clone()).unwrap();
        let event = CompletionEvent {
            id: "cold-flow-result".into(),
            sequence: Sequence::NitrogenTest,
            at_ms: 1,
            action: CompletionAction::Passed,
        };
        store.record_sequence(&event).unwrap();
        store
            .update(|d| {
                d.complete_sequence(Sequence::ValveSelfTest, 2);
                Ok(())
            })
            .unwrap();
        store
            .record_sequence(&CompletionEvent {
                id: "fire-consume".into(),
                at_ms: 3,
                action: CompletionAction::Clear,
                ..event.clone()
            })
            .unwrap();
        store = Store::open(path.clone()).unwrap();
        store.record_sequence(&event).unwrap();
        let list = store.document.checklists.last().unwrap();
        assert!(list.items[0].checked);
        assert!(!list.items[1].checked);
        store
            .record_sequence(&CompletionEvent {
                id: "reviewed-override".into(),
                at_ms: 4,
                action: CompletionAction::Override {
                    operator: "alice".into(),
                },
                ..event
            })
            .unwrap();
        assert_eq!(
            store.document.checklists.last().unwrap().items[1].completion_source,
            Some(CompletionSource::ManualOverride)
        );
        std::fs::remove_file(path).unwrap();
    }
}
