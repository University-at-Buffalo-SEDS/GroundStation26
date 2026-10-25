//! Typed checklist documents for paired, offline-capable stations.
//!
//! This module validates data and prepares a complete projection. It has no file,
//! sequence-engine or hardware side effects. Callers must authenticate causal
//! histories, durably stage them, then activate a complete projection at the
//! approved operational boundary. In particular, a remote receipt never earns
//! local operational qualification.
use crate::Document;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_LISTS: usize = 32;
pub const MAX_ITEMS: usize = 1024;
pub const MAX_ITEMS_PER_LIST: usize = 128;
pub const MAX_HISTORY_DOCUMENTS: usize = 8192;
pub const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
pub const POLICY_KEY: &str = "checklists/policy";
const LIST_PREFIX: &str = "checklists/list/";
const ITEM_PREFIX: &str = "checklists/item/";
const PROGRESS_PREFIX: &str = "checklists/progress/";

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sequence {
    ValveSelfTest,
    NitrogenTest,
    FillTarget,
}
impl Sequence {
    pub fn actor(self) -> &'static str {
        match self {
            Self::ValveSelfTest => "automated:valve_self_test",
            Self::NitrogenTest => "automated:nitrogen_test",
            Self::FillTarget => "automated:fill_target",
        }
    }
}
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionSource {
    Operator,
    ManualOverride,
    AutomatedSequence,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ItemDefinition {
    pub list_id: String,
    pub generation: String,
    pub id: String,
    pub text: String,
    pub complete_on: Option<Sequence>,
    pub order: u16,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub list_id: String,
    pub generation: String,
    pub item_id: String,
    pub definition_digest: String,
    pub checked: bool,
    pub completion_source: Option<CompletionSource>,
    pub completed_by: Option<String>,
    pub completed_at_ms: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListManifest {
    pub id: String,
    pub generation: String,
    pub title: String,
    pub required_before_launch: bool,
    pub order: u16,
    pub item_count: u16,
    pub items_digest: String,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub enforce_before_launch: bool,
    pub list_count: u16,
    pub lists_digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressIssue {
    Conflict,
    DefinitionChanged,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedItem {
    pub definition: ItemDefinition,
    pub progress: Progress,
    pub issue: Option<ProgressIssue>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedList {
    pub manifest: ListManifest,
    pub items: Vec<ProjectedItem>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    pub policy: Policy,
    pub lists: Vec<ProjectedList>,
}
impl Projection {
    pub fn required_complete(&self) -> bool {
        self.lists
            .iter()
            .filter(|list| list.manifest.required_before_launch)
            .all(|list| {
                list.items
                    .iter()
                    .all(|item| item.progress.checked && item.issue.is_none())
            })
    }
}

fn id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn hash(domain: &[u8], parts: &[&[u8]]) -> String {
    let mut context = Context::new(&SHA256);
    context.update(domain);
    for part in parts {
        context.update(&(part.len() as u64).to_le_bytes());
        context.update(part);
    }
    B64.encode(context.finish().as_ref())
}
fn digest_ok(value: &str) -> bool {
    value.len() == 43 && B64.decode(value).is_ok_and(|bytes| bytes.len() == 32)
}
pub fn list_key(list_id: &str) -> anyhow::Result<String> {
    anyhow::ensure!(id(list_id), "invalid checklist ID");
    Ok(format!(
        "{LIST_PREFIX}{}",
        hash(b"gs-checklist-list-v1\0", &[list_id.as_bytes()])
    ))
}
pub fn item_key(list_id: &str, item_id: &str) -> anyhow::Result<String> {
    anyhow::ensure!(id(list_id) && id(item_id), "invalid checklist/item ID");
    Ok(format!(
        "{ITEM_PREFIX}{}",
        hash(
            b"gs-checklist-item-v1\0",
            &[list_id.as_bytes(), item_id.as_bytes()]
        )
    ))
}
pub fn progress_key(list_id: &str, item_id: &str) -> anyhow::Result<String> {
    Ok(item_key(list_id, item_id)?.replacen(ITEM_PREFIX, PROGRESS_PREFIX, 1))
}
fn value_digest<T: Serialize>(domain: &[u8], value: &T) -> anyhow::Result<String> {
    Ok(hash(domain, &[&serde_json::to_vec(value)?]))
}
/// The semantic binding excludes visual order, so rearranging a checklist does
/// not invalidate successful completion. IDs, generation, wording and binding do.
pub fn definition_digest(definition: &ItemDefinition) -> anyhow::Result<String> {
    definition.validate()?;
    value_digest(
        b"gs-checklist-definition-v1\0",
        &(
            &definition.list_id,
            &definition.generation,
            &definition.id,
            &definition.text,
            definition.complete_on,
        ),
    )
}
fn manifest_digest<T: Serialize>(
    domain: &[u8],
    values: &BTreeMap<String, T>,
) -> anyhow::Result<String> {
    value_digest(domain, values)
}
impl ItemDefinition {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            id(&self.list_id) && id(&self.generation) && id(&self.id),
            "invalid checklist item identity"
        );
        anyhow::ensure!(
            !self.text.trim().is_empty() && self.text.len() <= 500,
            "checklist item text exceeds bounds"
        );
        anyhow::ensure!(
            usize::from(self.order) < MAX_ITEMS_PER_LIST,
            "checklist item order exceeds bounds"
        );
        Ok(())
    }
}
impl Progress {
    pub fn unchecked(definition: &ItemDefinition) -> anyhow::Result<Self> {
        Ok(Self {
            list_id: definition.list_id.clone(),
            generation: definition.generation.clone(),
            item_id: definition.id.clone(),
            definition_digest: definition_digest(definition)?,
            checked: false,
            completion_source: None,
            completed_by: None,
            completed_at_ms: None,
        })
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            id(&self.list_id)
                && id(&self.generation)
                && id(&self.item_id)
                && digest_ok(&self.definition_digest),
            "invalid checklist receipt identity"
        );
        if self.checked {
            anyhow::ensure!(
                self.completed_by
                    .as_ref()
                    .is_some_and(|actor| !actor.trim().is_empty() && actor.len() <= 96)
                    && self.completion_source.is_some()
                    && self.completed_at_ms.is_some_and(|time| time >= 0),
                "invalid checklist completion receipt"
            );
        } else {
            anyhow::ensure!(
                self.completed_by.is_none()
                    && self.completion_source.is_none()
                    && self.completed_at_ms.is_none(),
                "unchecked checklist receipt contains completion metadata"
            );
        }
        Ok(())
    }
    pub fn matches(&self, definition: &ItemDefinition) -> anyhow::Result<bool> {
        self.validate()?;
        let bound = self.list_id == definition.list_id
            && self.generation == definition.generation
            && self.item_id == definition.id
            && self.definition_digest == definition_digest(definition)?;
        if bound && self.checked {
            let origin_matches = match self.completion_source {
                Some(CompletionSource::AutomatedSequence) => definition
                    .complete_on
                    .is_some_and(|sequence| self.completed_by.as_deref() == Some(sequence.actor())),
                Some(CompletionSource::ManualOverride) => definition.complete_on.is_some(),
                Some(CompletionSource::Operator) => definition.complete_on.is_none(),
                None => false,
            };
            anyhow::ensure!(
                origin_matches,
                "checklist receipt origin does not match its binding"
            );
        }
        Ok(bound)
    }
}
impl ListManifest {
    pub fn from_items(
        id: String,
        generation: String,
        title: String,
        required: bool,
        order: u16,
        items: &[ItemDefinition],
    ) -> anyhow::Result<Self> {
        let mut definitions = BTreeMap::new();
        let mut orders = BTreeSet::new();
        for item in items {
            item.validate()?;
            anyhow::ensure!(
                item.list_id == id
                    && item.generation == generation
                    && orders.insert(item.order)
                    && definitions.insert(item_key(&id, &item.id)?, item).is_none(),
                "duplicate or mismatched checklist item"
            );
        }
        let this = Self {
            id,
            generation,
            title,
            required_before_launch: required,
            order,
            item_count: u16::try_from(items.len())?,
            items_digest: manifest_digest(b"gs-checklist-items-v1\0", &definitions)?,
        };
        this.validate()?;
        Ok(this)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            id(&self.id) && id(&self.generation) && digest_ok(&self.items_digest),
            "invalid checklist manifest identity"
        );
        anyhow::ensure!(
            !self.title.trim().is_empty()
                && self.title.len() <= 120
                && usize::from(self.order) < MAX_LISTS
                && usize::from(self.item_count) <= MAX_ITEMS_PER_LIST
                && (!self.required_before_launch || self.item_count != 0),
            "invalid checklist manifest bounds"
        );
        Ok(())
    }
}
impl Policy {
    pub fn from_lists(enforce: bool, lists: &[ListManifest]) -> anyhow::Result<Self> {
        let mut manifests = BTreeMap::new();
        let mut orders = BTreeSet::new();
        for list in lists {
            list.validate()?;
            anyhow::ensure!(
                orders.insert(list.order) && manifests.insert(list_key(&list.id)?, list).is_none(),
                "duplicate checklist manifest/order"
            );
        }
        anyhow::ensure!(
            !enforce || lists.iter().any(|list| list.required_before_launch),
            "checklist enforcement requires a required checklist"
        );
        let this = Self {
            enforce_before_launch: enforce,
            list_count: u16::try_from(lists.len())?,
            lists_digest: manifest_digest(b"gs-checklist-lists-v1\0", &manifests)?,
        };
        this.validate()?;
        Ok(this)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            usize::from(self.list_count) <= MAX_LISTS
                && digest_ok(&self.lists_digest)
                && (!self.enforce_before_launch || self.list_count != 0),
            "invalid checklist policy"
        );
        Ok(())
    }
}
/// Validate every causal head before staging it. A null is an explicit deletion,
/// never absence. Authentication and causal clock checks remain the caller's job.
pub fn validate_document(key: &str, document: &Document) -> anyhow::Result<()> {
    validated_size(key, document).map(|_| ())
}
struct EncodedSize(usize);
impl std::io::Write for EncodedSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > 65_536usize.saturating_sub(self.0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "checklist document exceeds64KiB",
            ));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn validated_size(key: &str, document: &Document) -> anyhow::Result<usize> {
    anyhow::ensure!(
        (1..=9).contains(&document.heads.len()) && key.len() <= 128,
        "invalid checklist history bounds"
    );
    // Count with a capped writer: an oversized value must not first allocate
    // a whole additional serialized copy just to discover it exceeds quota.
    let mut bytes = EncodedSize(0);
    serde_json::to_writer(&mut bytes, document)?;
    anyhow::ensure!(
        key == POLICY_KEY
            || [LIST_PREFIX, ITEM_PREFIX, PROGRESS_PREFIX]
                .iter()
                .any(|prefix| key.starts_with(prefix)
                    && key.len() == prefix.len() + 43
                    && digest_ok(&key[prefix.len()..])),
        "unknown checklist document key"
    );
    for head in &document.heads {
        if head.value.is_null() {
            continue;
        }
        if key == POLICY_KEY {
            serde_json::from_value::<Policy>(head.value.clone())?.validate()?;
        } else if key.starts_with(LIST_PREFIX) {
            let value: ListManifest = serde_json::from_value(head.value.clone())?;
            value.validate()?;
            anyhow::ensure!(
                key == list_key(&value.id)?,
                "checklist key does not match original ID"
            );
        } else if key.starts_with(ITEM_PREFIX) {
            let value: ItemDefinition = serde_json::from_value(head.value.clone())?;
            value.validate()?;
            anyhow::ensure!(
                key == item_key(&value.list_id, &value.id)?,
                "checklist key does not match original IDs"
            );
        } else {
            let value: Progress = serde_json::from_value(head.value.clone())?;
            value.validate()?;
            anyhow::ensure!(
                key == progress_key(&value.list_id, &value.item_id)?,
                "checklist receipt key does not match original IDs"
            );
        }
    }
    Ok(bytes.0)
}
fn single(document: &Document) -> anyhow::Result<&Value> {
    anyhow::ensure!(
        document.heads.len() == 1,
        "checklist definition conflict requires review"
    );
    Ok(&document.heads[0].value)
}
/// Complete membership is checked before returning anything activatable. Old
/// generations never contribute items or checked progress to a recreated list.
pub fn project(histories: &BTreeMap<String, Document>) -> anyhow::Result<Projection> {
    anyhow::ensure!(
        histories.len() <= MAX_HISTORY_DOCUMENTS,
        "checklist history catalog exceeds bounds"
    );
    // All accepted keys are ASCII without JSON escapes. Include exact map
    // braces, key quotes, colon and separators, not just document bodies.
    let mut history_bytes = 2usize;
    for (index, (key, document)) in histories.iter().enumerate() {
        history_bytes = history_bytes
            .checked_add(key.len() + validated_size(key, document)? + 3 + usize::from(index != 0))
            .ok_or_else(|| anyhow::anyhow!("checklist history size overflow"))?;
        anyhow::ensure!(
            history_bytes <= MAX_HISTORY_BYTES,
            "checklist history exceeds64MiB"
        );
    }
    // Decode each definition head once. Index by generation so a full32-list
    // projection does not repeatedly clone and parse all1024 item texts.
    let mut indexed: BTreeMap<(String, String), Vec<(usize, ItemDefinition)>> = BTreeMap::new();
    for (key, document) in histories
        .iter()
        .filter(|(key, _)| key.starts_with(ITEM_PREFIX))
    {
        for head in document.heads.iter().filter(|head| !head.value.is_null()) {
            let definition: ItemDefinition = serde_json::from_value(head.value.clone())?;
            anyhow::ensure!(
                *key == item_key(&definition.list_id, &definition.id)?,
                "mismatched item key"
            );
            indexed
                .entry((definition.list_id.clone(), definition.generation.clone()))
                .or_default()
                .push((document.heads.len(), definition));
        }
    }
    let policy: Policy = serde_json::from_value(
        single(
            histories
                .get(POLICY_KEY)
                .ok_or_else(|| anyhow::anyhow!("checklist policy missing"))?,
        )?
        .clone(),
    )?;
    let mut lists = Vec::new();
    let mut total = 0;
    for (key, document) in histories
        .iter()
        .filter(|(key, _)| key.starts_with(LIST_PREFIX))
    {
        let value = single(document)?;
        if value.is_null() {
            continue;
        }
        let manifest: ListManifest = serde_json::from_value(value.clone())?;
        let definitions = indexed
            .remove(&(manifest.id.clone(), manifest.generation.clone()))
            .unwrap_or_default();
        anyhow::ensure!(
            definitions.iter().all(|(heads, _)| *heads == 1),
            "checklist definition conflict requires review"
        );
        let mut definitions = definitions
            .into_iter()
            .map(|(_, definition)| definition)
            .collect::<Vec<_>>();
        let checked = ListManifest::from_items(
            manifest.id.clone(),
            manifest.generation.clone(),
            manifest.title.clone(),
            manifest.required_before_launch,
            manifest.order,
            &definitions,
        )?;
        anyhow::ensure!(
            checked == manifest,
            "checklist item membership incomplete or changed: {key}"
        );
        total += definitions.len();
        anyhow::ensure!(
            total <= MAX_ITEMS,
            "checklist active item count exceeds bounds"
        );
        definitions.sort_by_key(|item| item.order);
        let mut items = Vec::new();
        for definition in definitions {
            let history = histories.get(&progress_key(&definition.list_id, &definition.id)?);
            items.push(project_progress(&definition, history)?);
        }
        lists.push(ProjectedList { manifest, items });
    }
    let manifests = lists
        .iter()
        .map(|list| list.manifest.clone())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        Policy::from_lists(policy.enforce_before_launch, &manifests)? == policy,
        "checklist list membership incomplete or changed"
    );
    lists.sort_by_key(|list| list.manifest.order);
    Ok(Projection { policy, lists })
}

/// Project one progress row without parsing or hashing the template catalog.
pub fn project_progress(
    definition: &ItemDefinition,
    history: Option<&Document>,
) -> anyhow::Result<ProjectedItem> {
    let mut progress = Progress::unchecked(definition)?;
    let mut issue = None;
    if let Some(history) = history {
        validate_document(&progress_key(&definition.list_id, &definition.id)?, history)?;
        if history.heads.len() != 1 {
            issue = Some(ProgressIssue::Conflict);
        } else if !history.heads[0].value.is_null() {
            let received: Progress = serde_json::from_value(history.heads[0].value.clone())?;
            if received.matches(definition)? {
                progress = received;
            } else {
                issue = Some(ProgressIssue::DefinitionChanged);
            }
        }
    }
    Ok(ProjectedItem {
        definition: definition.clone(),
        progress,
        issue,
    })
}

#[cfg(test)]
mod tests;
