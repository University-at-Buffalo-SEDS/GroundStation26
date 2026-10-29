//! Paired checklist runtime. Raw histories are inert until a complete, validated
//! generation is selected. Operational qualification and outputs remain local.
use super::*;
use paired_settings::{
    Document as History, checklists as model,
    journal::{Journal, Limits},
    journal_replication::JournalRadio,
};
use ring::signature::KeyPair;
use std::{
    collections::{BTreeMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
};
mod storage;
use storage::{Overlay, Selector, Storage};
static REQUESTED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static PREFLIGHT: OnceLock<()> = OnceLock::new();
static SERVICE: OnceLock<Arc<tokio::sync::Mutex<Service>>> = OnceLock::new();
static INBOX: OnceLock<tokio::sync::mpsc::Sender<Input>> = OnceLock::new();
static BINDING: OnceLock<Mutex<BindingState>> = OnceLock::new();
#[derive(Default)]
struct BindingState {
    digest: Option<String>,
    durability_pending: bool,
    definition_pending: bool,
    error: Option<String>,
    saved_events: HashSet<String>,
    queued_events: HashSet<String>,
}
fn binding() -> &'static Mutex<BindingState> {
    BINDING.get_or_init(Default::default)
}
pub(crate) fn preflight() {
    if cfg!(feature = "hitl_mode") {
        return;
    }
    PREFLIGHT.get_or_init(|| {
        if !matches!(crate::station_pairing::Pairing::load(), Ok(None)) {
            REQUESTED.store(true, Ordering::Release);
        }
    });
}
pub(crate) fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}
pub(crate) fn ready() -> bool {
    READY.load(Ordering::Acquire)
}
pub(crate) fn qualification_binding() -> Result<Option<String>, String> {
    if !requested() {
        return Ok(None);
    }
    let state = binding().lock().unwrap();
    if !ready() || state.durability_pending {
        return Err(
            "Shared checklist storage is initializing or awaiting durable selection".into(),
        );
    }
    Ok(state.digest.clone())
}
pub(crate) fn qualification_allowed(expected: Option<&str>, has_evidence: bool) -> bool {
    if !requested() {
        return expected.is_none();
    }
    let state = binding().lock().unwrap();
    ready()
        && !state.durability_pending
        && !state.definition_pending
        && (!has_evidence || expected == state.digest.as_deref())
}
pub(crate) fn extra_launch_block() -> Option<String> {
    if !requested() {
        return None;
    }
    let state = binding().lock().unwrap();
    if !ready() || state.durability_pending {
        Some("Shared checklists are not durably ready".into())
    } else if state.definition_pending {
        Some("Shared checklist definitions are pending; return to Idle and clear qualification to apply them".into())
    } else {
        None
    }
}
pub(crate) fn completion_event_id() -> String {
    if let Ok(Some(digest)) = qualification_binding() {
        format!("{digest}{:016x}", rand::random::<u64>())
    } else {
        format!("{:032x}", rand::random::<u128>())
    }
}
#[derive(Clone)]
enum Input {
    Wire(Vec<u8>),
    Event(CompletionEvent),
}
pub(crate) fn receive_wire(wire: Vec<u8>) -> bool {
    INBOX
        .get()
        .is_some_and(|sender| sender.try_send(Input::Wire(wire)).is_ok())
}
/// Pending is not a persistence error. The operational engine retains its own
/// durable event until this returns true after the journal/progress commit.
pub(crate) fn deliver_event(event: &CompletionEvent) -> Result<bool, String> {
    let mut status = binding().lock().unwrap();
    if status.saved_events.contains(&event.id) {
        return Ok(true);
    }
    if status.queued_events.contains(&event.id) {
        return Ok(false);
    }
    let Some(sender) = INBOX.get() else {
        return Ok(false);
    };
    sender
        .try_send(Input::Event(event.clone()))
        .map_err(|_| "Checklist event queue is busy".to_string())?;
    status.queued_events.insert(event.id.clone());
    Ok(false)
}

fn initial_generation(installation: &str, list: &str) -> String {
    use ring::digest::{Context, SHA256};
    let mut hash = Context::new(&SHA256);
    hash.update(b"gs-checklist-initial-generation-v1\0");
    hash.update(&(installation.len() as u64).to_le_bytes());
    hash.update(installation.as_bytes());
    hash.update(list.as_bytes());
    hash.finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn keys(
    document: &Document,
    generations: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, model::ItemDefinition>, String> {
    let mut out = BTreeMap::new();
    for list in &document.checklists {
        let generation = generations
            .get(&list.id)
            .ok_or("Missing checklist generation")?;
        for (order, item) in list.items.iter().enumerate() {
            let definition = model::ItemDefinition {
                list_id: list.id.clone(),
                generation: generation.clone(),
                id: item.id.clone(),
                text: item.text.clone(),
                complete_on: item.complete_on,
                order: order as u16,
            };
            definition.validate().map_err(|e| e.to_string())?;
            out.insert(
                model::progress_key(&list.id, &item.id).map_err(|e| e.to_string())?,
                definition,
            );
        }
    }
    Ok(out)
}
fn export(
    document: &Document,
    generations: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, serde_json::Value>> {
    let mut out = BTreeMap::new();
    let mut manifests = Vec::new();
    for (order, list) in document.checklists.iter().enumerate() {
        let generation = generations
            .get(&list.id)
            .ok_or_else(|| anyhow::anyhow!("Missing checklist generation"))?;
        let mut definitions = Vec::new();
        for (order, item) in list.items.iter().enumerate() {
            let definition = model::ItemDefinition {
                list_id: list.id.clone(),
                generation: generation.clone(),
                id: item.id.clone(),
                text: item.text.clone(),
                complete_on: item.complete_on,
                order: order as u16,
            };
            out.insert(
                model::item_key(&list.id, &item.id)?,
                serde_json::to_value(&definition)?,
            );
            let mut progress = model::Progress::unchecked(&definition)?;
            if item.checked {
                progress.checked = true;
                progress.completion_source = item.completion_source;
                progress.completed_by = item.completed_by.clone();
                progress.completed_at_ms = item.completed_at_ms;
                // Old files with no verifiable receipt stay visible for review,
                // but never manufacture authenticated shared completion.
                if progress.validate().is_err() || progress.matches(&definition).is_err() {
                    progress = model::Progress::unchecked(&definition)?;
                }
            }
            out.insert(
                model::progress_key(&list.id, &item.id)?,
                serde_json::to_value(progress)?,
            );
            definitions.push(definition);
        }
        let manifest = model::ListManifest::from_items(
            list.id.clone(),
            generation.clone(),
            list.title.clone(),
            list.required_before_launch,
            order as u16,
            &definitions,
        )?;
        out.insert(model::list_key(&list.id)?, serde_json::to_value(&manifest)?);
        manifests.push(manifest);
    }
    out.insert(
        model::POLICY_KEY.into(),
        serde_json::to_value(model::Policy::from_lists(
            document.enforce_before_launch,
            &manifests,
        )?)?,
    );
    Ok(out)
}
fn projected(
    projection: &model::Projection,
) -> (
    Document,
    BTreeMap<String, String>,
    BTreeMap<String, Overlay>,
) {
    let mut document = Document {
        revision: 0,
        signoffs: Vec::new(),
        completion_events: Vec::new(),
        enforce_before_launch: projection.policy.enforce_before_launch,
        checklists: Vec::new(),
    };
    let mut generations = BTreeMap::new();
    let mut progress = BTreeMap::new();
    for list in &projection.lists {
        generations.insert(list.manifest.id.clone(), list.manifest.generation.clone());
        let mut items = Vec::new();
        for item in &list.items {
            items.push(Item {
                id: item.definition.id.clone(),
                text: item.definition.text.clone(),
                checked: item.progress.checked,
                complete_on: item.definition.complete_on,
                completion_source: item.progress.completion_source,
                completed_by: item.progress.completed_by.clone(),
                completed_at_ms: item.progress.completed_at_ms,
            });
            progress.insert(
                model::progress_key(&list.manifest.id, &item.definition.id).unwrap(),
                Overlay {
                    progress: item.progress.clone(),
                    issue: item.issue.clone(),
                },
            );
        }
        document.checklists.push(Checklist {
            id: list.manifest.id.clone(),
            title: list.manifest.title.clone(),
            required_before_launch: list.manifest.required_before_launch,
            items,
        });
    }
    (document, generations, progress)
}
fn apply_overlay(
    document: &mut Document,
    definitions: &BTreeMap<String, model::ItemDefinition>,
    progress: &BTreeMap<String, Overlay>,
) -> Result<(), String> {
    for list in &mut document.checklists {
        for item in &mut list.items {
            let key = model::progress_key(&list.id, &item.id).map_err(|e| e.to_string())?;
            if let Some(overlay) = progress.get(&key) {
                if !overlay
                    .progress
                    .matches(&definitions[&key])
                    .map_err(|e| e.to_string())?
                {
                    return Err("Saved checklist receipt does not match its generation".into());
                }
                item.checked = overlay.progress.checked && overlay.issue.is_none();
                item.completion_source = overlay.progress.completion_source;
                item.completed_by = overlay.progress.completed_by.clone();
                item.completed_at_ms = overlay.progress.completed_at_ms;
            }
        }
    }
    Ok(())
}
struct Service {
    journal: Journal,
    storage: Storage,
    station: String,
    selector: Selector,
    document: Document,
    generations: BTreeMap<String, String>,
    definitions: BTreeMap<String, model::ItemDefinition>,
    progress: BTreeMap<String, Overlay>,
    last_revision: i64,
    pending_reason: Option<String>,
    selection_pending: bool,
    failed_progress: std::collections::BTreeSet<String>,
    last_activation: Option<(i64, bool)>,
    activation_retry: bool,
}
impl Service {
    async fn open(
        path: PathBuf,
        selector_path: PathBuf,
        installation: String,
        station: String,
        peers: Vec<String>,
        legacy: Document,
        primary: bool,
    ) -> anyhow::Result<Self> {
        let legacy_revision = legacy.revision;
        let journal = Journal::open(&path, installation.clone(), peers, Limits::default()).await?;
        let storage = Storage::open(&path, selector_path, installation).await?;
        let (selector, mut document, generations, progress) =
            if let Some(selected) = storage.selected().map_err(anyhow::Error::msg)? {
                let (document, generations, progress) = storage.load(&selected).await?;
                (selected, document, generations, progress)
            } else {
                let generations = legacy
                    .checklists
                    .iter()
                    .map(|list| {
                        (
                            list.id.clone(),
                            initial_generation(&storage.installation, &list.id),
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                let values = export(&legacy, &generations)?;
                let mut progress = BTreeMap::new();
                let definitions = keys(&legacy, &generations).map_err(anyhow::Error::msg)?;
                for (key, definition) in &definitions {
                    let receipt: model::Progress = serde_json::from_value(values[key].clone())?;
                    progress.insert(
                        key.clone(),
                        Overlay {
                            progress: receipt,
                            issue: None,
                        },
                    );
                    let _ = definition;
                }
                let prepared = storage.prepare(legacy, generations, &progress, 0).await?;
                storage.select(&prepared).map_err(anyhow::Error::msg)?;
                storage.sync_selector().map_err(anyhow::Error::msg)?;
                (
                    prepared.selector.clone(),
                    prepared.document.clone(),
                    prepared.generations.clone(),
                    progress,
                )
            };
        let definitions = keys(&document, &generations).map_err(anyhow::Error::msg)?;
        apply_overlay(&mut document, &definitions, &progress).map_err(anyhow::Error::msg)?;
        document.revision = storage
            .next_revision(legacy_revision)
            .await
            .map_err(anyhow::Error::msg)?;
        if primary && (journal.metadata().await?.documents == 0 || storage.seed_pending().await?) {
            // A crash partway through first seed resumes the same selected
            // migration catalog before accepting any remote transport.
            storage.set_seed_pending(true).await?;
            for (key, value) in export(&document, &generations)? {
                if key.starts_with("checklists/progress/")
                    && value.get("checked") == Some(&serde_json::Value::Bool(false))
                {
                    continue;
                }
                journal
                    .edit(&key, value, &station, model::validate_document)
                    .await?;
            }
            storage.set_seed_pending(false).await?;
        }
        storage.sync_selector().map_err(anyhow::Error::msg)?;
        storage.collect(&selector.generation, None).await?;
        Ok(Self {
            journal,
            storage,
            station,
            selector,
            document,
            generations,
            definitions,
            progress,
            last_revision: 0,
            pending_reason: None,
            selection_pending: false,
            failed_progress: Default::default(),
            last_activation: None,
            activation_retry: false,
        })
    }
    async fn publish(&mut self) -> Result<Document, String> {
        let revision =
            access(|store| Ok(store.document.revision)).unwrap_or(self.document.revision);
        self.document.revision = self
            .storage
            .next_revision(revision.max(self.document.revision))
            .await?;
        let mut global = store().lock().map_err(|_| "Checklist store unavailable")?;
        let mut next = self.document.clone();
        let store = global.as_mut().map_err(|error| error.clone())?;
        next.preserve_signoffs(
            &store.document,
            crate::telemetry_task::get_current_timestamp_ms() as i64,
        );
        let saved = store.persist(next)?;
        self.document = saved.clone();
        Ok(saved)
    }
    async fn refresh_progress(
        &mut self,
        key: &str,
        event: Option<&CompletionEvent>,
    ) -> anyhow::Result<()> {
        match self.refresh_progress_inner(key, event).await {
            Ok(()) => {
                self.failed_progress.remove(key);
                if self.failed_progress.is_empty() && !self.selection_pending {
                    binding().lock().unwrap().durability_pending = false;
                }
                Ok(())
            }
            Err(error) => {
                self.failed_progress.insert(key.into());
                binding().lock().unwrap().durability_pending = true;
                Err(anyhow::anyhow!(
                    "Progress history is saved; local view update is pending: {error}"
                ))
            }
        }
    }
    async fn refresh_progress_inner(
        &mut self,
        key: &str,
        event: Option<&CompletionEvent>,
    ) -> anyhow::Result<()> {
        let Some(definition) = self.definitions.get(key) else {
            return Ok(());
        };
        let history = self.journal.document(key).await?;
        let item = model::project_progress(definition, history.as_ref())?;
        if item.issue == Some(model::ProgressIssue::DefinitionChanged) {
            return Ok(());
        }
        let overlay = Overlay {
            progress: item.progress,
            issue: item.issue,
        };
        if self.progress.get(key) == Some(&overlay) && event.is_none() {
            if let Some(history) = history.filter(|history| history.heads.len() == 1) {
                self.journal.acknowledge(key, &history.heads[0]).await?;
            }
            return Ok(());
        }
        self.storage
            .progress(
                &self.selector.generation,
                &BTreeMap::from([(key.into(), overlay.clone())]),
                event,
            )
            .await?;
        self.progress.insert(key.into(), overlay);
        apply_overlay(&mut self.document, &self.definitions, &self.progress)
            .map_err(anyhow::Error::msg)?;
        if let Some(history) = history.filter(|history| history.heads.len() == 1) {
            self.journal.acknowledge(key, &history.heads[0]).await?;
        }
        self.publish().await.map_err(anyhow::Error::msg)?;
        Ok(())
    }
    async fn check(&mut self, request: &Check, actor: &str, now: i64) -> anyhow::Result<Document> {
        let key = model::progress_key(&request.checklist_id, &request.item_id)?;
        let definition = self
            .definitions
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("Checklist item no longer exists"))?;
        let mut progress = model::Progress::unchecked(definition)?;
        if request.checked {
            progress.checked = true;
            progress.completion_source = Some(if definition.complete_on.is_some() {
                CompletionSource::ManualOverride
            } else {
                CompletionSource::Operator
            });
            progress.completed_by = Some(actor.into());
            progress.completed_at_ms = Some(now);
        }
        self.journal
            .edit(
                &key,
                serde_json::to_value(progress)?,
                &self.station,
                model::validate_document,
            )
            .await?;
        self.refresh_progress(&key, None).await?;
        Ok(self.document.clone())
    }
    async fn event(&mut self, event: &CompletionEvent) -> anyhow::Result<()> {
        anyhow::ensure!(
            event.id.len() == 80 && event.id.starts_with(&self.selector.digest),
            "Sequence receipt is not bound to the active checklist template; reset qualification and repeat the test"
        );
        if self.storage.event_saved(event).await? {
            return Ok(());
        }
        let targets = self
            .definitions
            .iter()
            .filter(|(_, definition)| definition.complete_on == Some(event.sequence))
            .map(|(key, definition)| (key.clone(), definition.clone()))
            .collect::<Vec<_>>();
        let mut changed = BTreeMap::new();
        for (key, definition) in targets {
            let mut progress = model::Progress::unchecked(&definition)?;
            match &event.action {
                CompletionAction::Passed => {
                    progress.checked = true;
                    progress.completion_source = Some(CompletionSource::AutomatedSequence);
                    progress.completed_by = Some(event.sequence.actor().into());
                    progress.completed_at_ms = Some(event.at_ms);
                }
                CompletionAction::Override { operator } => {
                    progress.checked = true;
                    progress.completion_source = Some(CompletionSource::ManualOverride);
                    progress.completed_by = Some(operator.clone());
                    progress.completed_at_ms = Some(event.at_ms);
                }
                CompletionAction::Clear => {}
            }
            self.journal
                .edit(
                    &key,
                    serde_json::to_value(progress)?,
                    &self.station,
                    model::validate_document,
                )
                .await?;
            let history = self.journal.document(&key).await?;
            let item = model::project_progress(&definition, history.as_ref())?;
            changed.insert(
                key,
                Overlay {
                    progress: item.progress,
                    issue: item.issue,
                },
            );
        }
        self.storage
            .progress(&self.selector.generation, &changed, Some(event))
            .await?;
        self.progress.extend(changed);
        apply_overlay(&mut self.document, &self.definitions, &self.progress)
            .map_err(anyhow::Error::msg)?;
        self.publish().await.map_err(anyhow::Error::msg)?;
        Ok(())
    }
    async fn finish_selection(
        &mut self,
        snapshot: &paired_settings::journal::Snapshot,
    ) -> anyhow::Result<()> {
        self.storage.sync_selector().map_err(anyhow::Error::msg)?;
        let applied = export(&self.document, &self.generations)?;
        for (key, history) in &snapshot.documents {
            if history.heads.len() == 1
                && (history.heads[0].value.is_null()
                    || applied.get(key) == Some(&history.heads[0].value))
            {
                self.journal.acknowledge(key, &history.heads[0]).await?;
            }
        }
        self.storage
            .collect(&self.selector.generation, None)
            .await?;
        if self.selection_pending {
            self.failed_progress.clear();
        }
        self.selection_pending = false;
        self.pending_reason = None;
        let mut status = binding().lock().unwrap();
        status.durability_pending = !self.failed_progress.is_empty();
        status.definition_pending = false;
        Ok(())
    }
    async fn activate(&mut self, state: &Arc<AppState>) -> anyhow::Result<()> {
        let revision = self.journal.metadata().await?.revision;
        let eligible = {
            let gse = state.gse.lock().unwrap();
            let zero = state.auto_zero.lock().unwrap();
            gse.checklist_definition_edit_allowed() && !zero.pending()
        };
        // Stable pending templates do not reload/hash history on every tick.
        if !self.activation_retry
            && !self.selection_pending
            && self.last_activation == Some((revision, eligible))
        {
            return Ok(());
        }
        let result = self.activate_inner(state).await;
        self.last_activation = Some((revision, eligible));
        self.activation_retry = result.is_err();
        result
    }
    async fn activate_inner(&mut self, state: &Arc<AppState>) -> anyhow::Result<()> {
        let snapshot = self.journal.snapshot().await?;
        if self.selection_pending {
            return self.finish_selection(&snapshot).await;
        }
        let projection = match model::project(&snapshot.documents) {
            Ok(projection) => projection,
            Err(error) => {
                self.pending_reason = Some(format!(
                    "Shared template is incomplete or conflicted: {error}"
                ));
                binding().lock().unwrap().definition_pending = !snapshot.documents.is_empty();
                return Ok(());
            }
        };
        let (document, generations, progress) = projected(&projection);
        let mut definition_document = document.clone();
        for item in definition_document
            .checklists
            .iter_mut()
            .flat_map(|list| &mut list.items)
        {
            item.checked = false;
            item.completion_source = None;
            item.completed_by = None;
            item.completed_at_ms = None;
        }
        definition_document.revision = 0;
        definition_document.completion_events.clear();
        let digest = storage::definition_digest(
            &serialize_document(&definition_document).map_err(anyhow::Error::msg)?,
            &generations,
        )
        .map_err(anyhow::Error::msg)?;
        if digest == self.selector.digest {
            return self.finish_selection(&snapshot).await;
        }
        self.pending_reason =
            Some("Template change waits for Idle and explicitly cleared qualification".into());
        binding().lock().unwrap().definition_pending = true;
        {
            let gse = state.gse.lock().unwrap();
            let zero = state.auto_zero.lock().unwrap();
            if !gse.checklist_definition_edit_allowed() || zero.pending() {
                return Ok(());
            }
        }
        let previous = self.selector.clone();
        self.storage.collect(&previous.generation, None).await?;
        let prepared = self
            .storage
            .prepare(
                document,
                generations,
                &progress,
                previous
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Checklist selector revision exhausted"))?,
            )
            .await?;
        // Construct every large allocation before the operational critical section.
        let mut next_document = prepared.document.clone();
        let next_definitions =
            keys(&next_document, &prepared.generations).map_err(anyhow::Error::msg)?;
        apply_overlay(&mut next_document, &next_definitions, &progress)
            .map_err(anyhow::Error::msg)?;
        next_document.revision = self
            .storage
            .next_revision(
                access(|store| Ok(store.document.revision))
                    .unwrap_or(self.document.revision)
                    .max(self.document.revision),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        let old = access(|store| Ok(store.document.clone())).map_err(anyhow::Error::msg)?;
        next_document.preserve_signoffs(
            &old,
            crate::telemetry_task::get_current_timestamp_ms() as i64,
        );
        let global_document = next_document.clone();
        let next_generations = prepared.generations.clone();
        let legacy_path = access(|store| Ok(store.path.clone())).unwrap_or_else(|_| {
            std::env::var_os("GS_CHECKLISTS_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("backend/data/checklists.json"))
        });
        if self.journal.metadata().await?.revision != snapshot.metadata.revision {
            return Ok(());
        }
        let retired;
        {
            let gse = state.gse.lock().unwrap();
            let zero = state.auto_zero.lock().unwrap();
            if !gse.checklist_definition_edit_allowed()
                || zero.pending()
                || self.selector != previous
            {
                return Ok(());
            }
            let mut global = store()
                .lock()
                .map_err(|_| anyhow::anyhow!("Checklist store unavailable"))?;
            self.storage.select(&prepared).map_err(anyhow::Error::msg)?;
            let mut status = binding().lock().unwrap();
            status.durability_pending = true;
            status.digest = Some(prepared.selector.digest.clone());
            self.selector = prepared.selector.clone();
            self.selection_pending = true;
            retired = (
                std::mem::replace(&mut self.document, next_document),
                std::mem::replace(&mut self.generations, next_generations),
                std::mem::replace(&mut self.definitions, next_definitions),
                std::mem::replace(&mut self.progress, progress),
                std::mem::replace(
                    &mut *global,
                    Ok(Store {
                        path: legacy_path,
                        document: global_document,
                    }),
                ),
            );
        }
        // Retiring old allocations and directory durability never hold GSE locks.
        drop(retired);
        access(|store| store.persist(store.document.clone())).map_err(anyhow::Error::msg)?;
        self.finish_selection(&snapshot).await
    }
    async fn stage_template(&mut self, request: Document) -> Result<(), String> {
        let mut next = self.document.clone();
        next.edit(request)?;
        let generations = next
            .checklists
            .iter()
            .map(|list| {
                (
                    list.id.clone(),
                    self.generations
                        .get(&list.id)
                        .cloned()
                        .unwrap_or_else(|| format!("{:032x}", rand::random::<u128>())),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let values = export(&next, &generations).map_err(|e| e.to_string())?;
        serialize_document(&next)?;
        let old = export(&self.document, &self.generations).map_err(|e| e.to_string())?;
        // A fresh secondary publishes defaults only when an administrator explicitly
        // edits them. Export the full explicit catalog before its first applied policy.
        let bootstrap = self
            .journal
            .observed(model::POLICY_KEY)
            .await
            .map_err(|e| e.to_string())?
            .is_none();
        for key in old
            .keys()
            .chain(values.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
        {
            let value = values.get(&key).cloned().unwrap_or(serde_json::Value::Null);
            if bootstrap || old.get(&key) != Some(&value) {
                self.journal
                    .edit(&key, value, &self.station, model::validate_document)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    async fn changed(&mut self, state: &Arc<AppState>) -> anyhow::Result<()> {
        let changes = self.journal.changed_page(self.last_revision, 64).await?;
        let mut definitions = false;
        for change in changes {
            if change.key.starts_with("checklists/progress/") {
                self.refresh_progress(&change.key, None).await?;
            } else {
                definitions = true;
            }
            self.last_revision = self.last_revision.max(change.revision);
        }
        if definitions || self.pending_reason.is_some() {
            self.activate(state).await?;
        }
        Ok(())
    }
}

pub(crate) fn start(
    state: Arc<AppState>,
    pairing: Arc<crate::station_pairing::Pairing>,
    mesh: crate::rrls::Shared,
) -> tokio::task::JoinHandle<()> {
    REQUESTED.store(true, Ordering::Release);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
    let _ = INBOX.set(sender);
    tokio::spawn(async move {
        let legacy = match access(|store| Ok((store.path.clone(), store.document.clone()))) {
            Ok(value) => value,
            Err(_) => {
                let path = std::env::var_os("GS_CHECKLISTS_FILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("backend/data/checklists.json"));
                match Store::open(path.clone()) {
                    Ok(store) => (path, store.document),
                    Err(error) => {
                        binding().lock().unwrap().error = Some(error);
                        return;
                    }
                }
            }
        };
        let database = legacy
            .0
            .with_extension(format!("{}.paired.sqlite3", pairing.public.station_id));
        let selector = legacy.0.with_extension(format!(
            "{}.paired-selector.json",
            pairing.public.station_id
        ));
        let peers = pairing
            .public
            .peers
            .iter()
            .map(|peer| peer.id.clone())
            .chain(std::iter::once(pairing.public.station_id.clone()))
            .collect::<Vec<_>>();
        let service = match Service::open(
            database,
            selector,
            pairing.public.installation_id.clone(),
            pairing.public.station_id.clone(),
            peers,
            legacy.1,
            !crate::rrls::is_rrls(),
        )
        .await
        {
            Ok(service) => service,
            Err(error) => {
                binding().lock().unwrap().error =
                    Some(format!("Shared checklist initialization failed: {error}"));
                return;
            }
        };
        let mut radio = match JournalRadio::new(
            pairing.public.installation_id.clone(),
            pairing.public.station_id.clone(),
            pairing
                .public
                .peers
                .iter()
                .map(|peer| peer.id.clone())
                .collect(),
        ) {
            Ok(radio) => radio,
            Err(error) => {
                binding().lock().unwrap().error = Some(error.to_string());
                return;
            }
        };
        let service = Arc::new(tokio::sync::Mutex::new(service));
        let _ = SERVICE.set(service.clone());
        {
            let mut service = service.lock().await;
            if let Err(error) = service.publish().await {
                binding().lock().unwrap().error = Some(error);
                return;
            }
            binding().lock().unwrap().digest = Some(service.selector.digest.clone());
        }
        READY.store(true, Ordering::Release);
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut shutdown = state.shutdown_subscribe();
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = interval.tick() => {
                    let mut service = service.lock().await;
                    let now = mesh.lock().unwrap().now();
                    let mut error = None;
                    for _ in 0..8 {
                        match receiver.try_recv() {
                            Ok(Input::Wire(wire)) => {
                                match radio.receive(
                                    &service.journal, &wire, now,
                                    pairing.mission_signing_key().public_key().as_ref(),
                                    model::validate_document,
                                ).await {
                                    Ok(_) => mesh.lock().unwrap().authenticated_settings_peer(now),
                                    Err(failure) => error = Some(format!("Shared checklist receive deferred: {failure}")),
                                }
                            }
                            Ok(Input::Event(event)) => {
                                match service.event(&event).await {
                                    Ok(()) => {
                                        let mut status = binding().lock().unwrap();
                                        status.saved_events.insert(event.id.clone());
                                        status.queued_events.remove(&event.id);
                                    }
                                    Err(failure) => {
                                        binding().lock().unwrap().queued_events.remove(&event.id);
                                        error = Some(format!("Checklist event save deferred: {failure}"));
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if let Err(failure) = service.changed(&state).await {
                        error = Some(format!("Shared checklist projection deferred: {failure}"));
                    }
                    if let Err(failure) = radio.poll(
                        &service.journal, now, pairing.mission_signing_key(),
                        |_, wire| crate::mission_sync::wire::offer(
                            crate::mission_sync::wire::Class::Checklist, wire,
                        ),
                    ).await {
                        error = Some(format!("Shared checklist transmission deferred: {failure}"));
                    }
                    crate::mission_sync::wire::flush(&mesh, now);
                    binding().lock().unwrap().error = error;
                }
            }
        }
    })
}
pub(super) async fn sign(request: &Sign, actor: &str, now: i64) -> Result<Document, String> {
    let mut service = SERVICE
        .get()
        .ok_or("Shared checklists are initializing")?
        .lock()
        .await;
    if let Some(reason) = extra_launch_block() {
        return Err(reason);
    }
    let document = access(|store| store.update(|doc| doc.sign(request, actor, now)))?;
    service.document.signoffs = document.signoffs.clone();
    service.document.revision = document.revision;
    Ok(document)
}
pub(super) async fn check(request: &Check, actor: &str, now: i64) -> Result<Document, String> {
    let service = SERVICE.get().ok_or("Shared checklists are initializing")?;
    service
        .lock()
        .await
        .check(request, actor, now)
        .await
        .map_err(|error| error.to_string())
}
pub(crate) async fn edit(request: Document, state: &Arc<AppState>) -> Result<Document, String> {
    let service = SERVICE.get().ok_or("Shared checklists are initializing")?;
    let mut service = service.lock().await;
    {
        let gse = state.gse.lock().unwrap();
        let zero = state.auto_zero.lock().unwrap();
        if !gse.checklist_definition_edit_allowed() || zero.pending() {
            return Err(
                "Return to Idle and clear qualification before changing checklist definitions"
                    .into(),
            );
        }
    }
    service.stage_template(request).await?;
    service.activate(state).await.map_err(|e| e.to_string())?;
    if service.pending_reason.is_some() {
        return Err("Checklist changes are staged and await a complete conflict-free template at Idle with qualification cleared".into());
    }
    Ok(service.document.clone())
}
#[derive(Serialize)]
struct SyncStatus {
    enabled: bool,
    ready: bool,
    durability_pending: bool,
    definition_pending: bool,
    error: Option<String>,
    pending_reason: Option<String>,
    conflicts: BTreeMap<String, History>,
    conflict_count: usize,
    qualification_binding_valid: bool,
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = principal(&state, &headers, false).await {
        return error;
    }
    let mut response = SyncStatus {
        enabled: requested(),
        ready: ready(),
        durability_pending: false,
        definition_pending: false,
        error: None,
        pending_reason: None,
        conflicts: BTreeMap::new(),
        conflict_count: 0,
        qualification_binding_valid: state.gse.lock().unwrap().checklist_template_bound(),
    };
    {
        let state = binding().lock().unwrap();
        response.durability_pending = state.durability_pending;
        response.definition_pending = state.definition_pending;
        response.error = state.error.clone();
    }
    if let Some(service) = SERVICE.get() {
        let service = service.lock().await;
        response.pending_reason = service.pending_reason.clone();
        if let Ok(conflicts) = service.journal.conflicts_page("", 8).await {
            response.conflicts = conflicts;
        }
        if let Ok(count) = service.journal.conflict_count().await {
            response.conflict_count = count;
        }
    }
    Json(response).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    key: String,
    expected: History,
    value: serde_json::Value,
}
async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Resolve>,
) -> Response {
    if let Err(error) = principal(&state, &headers, true).await {
        return error;
    }
    let Some(service) = SERVICE.get() else {
        return (StatusCode::CONFLICT, "Shared checklists are initializing").into_response();
    };
    let mut service = service.lock().await;
    if !request.key.starts_with("checklists/progress/") {
        let gse = state.gse.lock().unwrap();
        let zero = state.auto_zero.lock().unwrap();
        if !gse.checklist_definition_edit_allowed() || zero.pending() {
            return (
                StatusCode::CONFLICT,
                "Return to Idle and clear qualification before resolving definitions",
            )
                .into_response();
        }
    }
    if request.expected.heads.len() < 2
        || !request
            .expected
            .heads
            .iter()
            .any(|head| head.value == request.value)
    {
        return (
            StatusCode::CONFLICT,
            "Choose one of the reviewed checklist versions",
        )
            .into_response();
    }
    let result = service
        .journal
        .resolve(
            &request.key,
            &request.expected,
            request.value,
            &service.station,
            model::validate_document,
        )
        .await;
    match result {
        Ok(_) => {
            if let Err(error) = service.changed(&state).await {
                return (StatusCode::CONFLICT, error.to_string()).into_response();
            }
            Json(service.document.clone()).into_response()
        }
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}
pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/checklists/sync", get(status))
        .route("/api/checklists/resolve", post(resolve))
}

#[cfg(test)]
mod tests;
