use super::*;
use crate::{Revision, Store};

fn definition(list: &str, item: &str, generation: &str, order: u16) -> ItemDefinition {
    ItemDefinition {
        list_id: list.into(),
        generation: generation.into(),
        id: item.into(),
        text: "Inspect pressure sensor".into(),
        complete_on: None,
        order,
    }
}
fn receipt(definition: &ItemDefinition) -> Progress {
    let mut progress = Progress::unchecked(definition).unwrap();
    progress.checked = true;
    progress.completed_by = Some("remote operator".into());
    progress.completion_source = Some(CompletionSource::Operator);
    progress.completed_at_ms = Some(100);
    progress
}
fn head(value: impl Serialize) -> Document {
    Document {
        heads: vec![Revision {
            clock: BTreeMap::from([("gs".into(), 1)]),
            value: serde_json::to_value(value).unwrap(),
        }],
    }
}
fn catalog(items: &[ItemDefinition], checked: bool) -> BTreeMap<String, Document> {
    let manifest = ListManifest::from_items(
        "setup".into(),
        "generation-one".into(),
        "Setup".into(),
        true,
        0,
        items,
    )
    .unwrap();
    let policy = Policy::from_lists(true, std::slice::from_ref(&manifest)).unwrap();
    let mut docs = BTreeMap::from([
        (POLICY_KEY.into(), head(policy)),
        (list_key("setup").unwrap(), head(manifest)),
    ]);
    for item in items {
        docs.insert(item_key(&item.list_id, &item.id).unwrap(), head(item));
        if checked {
            docs.insert(
                progress_key(&item.list_id, &item.id).unwrap(),
                head(receipt(item)),
            );
        }
    }
    docs
}
#[test]
fn original_long_ids_hash_without_ambiguous_concatenation_and_bind_semantics() {
    assert!(list_key(&"a".repeat(80)).unwrap().len() <= 128);
    assert!(item_key(&"a".repeat(80), &"b".repeat(80)).unwrap().len() <= 128);
    assert!(
        progress_key(&"a".repeat(80), &"b".repeat(80))
            .unwrap()
            .len()
            <= 128
    );
    assert_ne!(item_key("ab", "c").unwrap(), item_key("a", "bc").unwrap());
    assert!(list_key(&"a".repeat(81)).is_err());
    let mut item = definition("setup", "pressure", "generation-one", 0);
    let completed = receipt(&item);
    item.order = 4;
    assert!(
        completed.matches(&item).unwrap(),
        "visual reordering must preserve checked evidence"
    );
    item.text = "Inspect a different pressure sensor".into();
    assert!(!completed.matches(&item).unwrap());
    item.text = "Inspect pressure sensor".into();
    item.complete_on = Some(Sequence::NitrogenTest);
    assert!(!completed.matches(&item).unwrap());
    item.complete_on = None;
    item.generation = "generation-two".into();
    assert!(!completed.matches(&item).unwrap());
}
#[test]
fn incomplete_membership_cannot_expose_a_partial_template() {
    let first = definition("setup", "one", "generation-one", 0);
    let second = definition("setup", "two", "generation-one", 1);
    let complete = catalog(&[first.clone(), second.clone()], true);
    assert_eq!(project(&complete).unwrap().lists[0].items.len(), 2);
    for key in [
        POLICY_KEY.into(),
        list_key("setup").unwrap(),
        item_key("setup", "one").unwrap(),
        item_key("setup", "two").unwrap(),
    ] {
        let mut missing = complete.clone();
        missing.remove(&key);
        assert!(
            project(&missing).is_err(),
            "incomplete template accepted without{key}"
        );
    }
    let mut changed = complete.clone();
    let mut item = first.clone();
    item.text.push('!');
    changed.insert(item_key("setup", "one").unwrap(), head(item));
    assert!(
        project(&changed).is_err(),
        "new item must await matching list/policy manifests"
    );
    let mut duplicate_order = second;
    duplicate_order.order = 0;
    assert!(
        ListManifest::from_items(
            "setup".into(),
            "generation-one".into(),
            "Setup".into(),
            true,
            0,
            &[first, duplicate_order]
        )
        .is_err()
    );
}
#[test]
fn progress_conflicts_and_stale_receipts_are_unchecked_without_changing_definitions() {
    let item = definition("setup", "pressure", "generation-one", 0);
    let mut docs = catalog(std::slice::from_ref(&item), true);
    let key = progress_key("setup", "pressure").unwrap();
    let peers = vec!["gs".into(), "rrls".into()];
    let mut gs = Store::new("test".into()).unwrap();
    for (key, value) in &docs {
        gs.merge("test", key, value, &peers).unwrap();
    }
    let mut rrls = gs.clone();
    let base = gs.document(&key).unwrap().heads[0].clone();
    let mut changed = receipt(&item);
    changed.completed_by = Some("another operator".into());
    let a = gs
        .edit(
            &key,
            Some(&base),
            serde_json::to_value(changed).unwrap(),
            "gs",
            &peers,
        )
        .unwrap();
    let b = rrls
        .edit(
            &key,
            Some(&base),
            serde_json::to_value(Progress::unchecked(&item).unwrap()).unwrap(),
            "rrls",
            &peers,
        )
        .unwrap();
    gs.merge("test", &key, &b, &peers).unwrap();
    rrls.merge("test", &key, &a, &peers).unwrap();
    assert_eq!(gs, rrls);
    let projected = project(gs.documents()).unwrap();
    assert!(!projected.lists[0].items[0].progress.checked);
    assert_eq!(
        projected.lists[0].items[0].issue,
        Some(ProgressIssue::Conflict)
    );
    let conflict = gs.document(&key).unwrap().clone();
    gs.resolve(
        &key,
        &conflict,
        serde_json::to_value(receipt(&item)).unwrap(),
        "gs",
        &peers,
    )
    .unwrap();
    assert!(
        project(gs.documents()).unwrap().lists[0].items[0]
            .progress
            .checked
    );
    // A new semantic definition needs fresh evidence, even if its old receipt arrives later.
    let mut new_item = item.clone();
    new_item.text = "Inspect another sensor".into();
    let updated = catalog(std::slice::from_ref(&new_item), false);
    for (key, value) in updated {
        docs.insert(key, value);
    }
    let projected = project(&docs).unwrap();
    assert!(!projected.lists[0].items[0].progress.checked);
    assert_eq!(
        projected.lists[0].items[0].issue,
        Some(ProgressIssue::DefinitionChanged)
    );
}
#[test]
fn delete_recreate_and_replay_cannot_resurrect_old_completion() {
    let item = definition("setup", "pressure", "generation-one", 0);
    let docs = catalog(std::slice::from_ref(&item), true);
    let peers = vec!["gs".into(), "rrls".into()];
    let mut store = Store::new("test".into()).unwrap();
    for (key, value) in &docs {
        store.merge("test", key, value, &peers).unwrap();
    }
    let key = list_key("setup").unwrap();
    let base = store.document(&key).unwrap().heads[0].clone();
    store
        .edit(&key, Some(&base), Value::Null, "gs", &peers)
        .unwrap();
    store.merge("test", &key, &docs[&key], &peers).unwrap();
    assert!(store.document(&key).unwrap().heads[0].value.is_null());
    assert!(
        project(store.documents()).is_err(),
        "deleted list cannot satisfy old policy membership"
    );
    let mut recreated = item;
    recreated.generation = "generation-two".into();
    let mut replaced = docs.clone();
    let manifest = ListManifest::from_items(
        "setup".into(),
        "generation-two".into(),
        "Setup".into(),
        true,
        0,
        std::slice::from_ref(&recreated),
    )
    .unwrap();
    replaced.insert(key.clone(), head(manifest.clone()));
    replaced.insert(
        POLICY_KEY.into(),
        head(Policy::from_lists(true, &[manifest]).unwrap()),
    );
    replaced.insert(item_key("setup", "pressure").unwrap(), head(recreated));
    replaced.insert(
        progress_key("setup", "pressure").unwrap(),
        docs[&progress_key("setup", "pressure").unwrap()].clone(),
    );
    let projected = project(&replaced).unwrap();
    assert!(!projected.lists[0].items[0].progress.checked);
    assert_eq!(
        projected.lists[0].items[0].issue,
        Some(ProgressIssue::DefinitionChanged)
    );
}
#[test]
fn malformed_receipts_and_key_aliases_are_rejected_before_projection() {
    let item = definition("setup", "pressure", "generation-one", 0);
    let key = progress_key("setup", "pressure").unwrap();
    let mut completed = receipt(&item);
    completed.completed_by = Some("x".repeat(97));
    assert!(validate_document(&key, &head(completed.clone())).is_err());
    completed.completed_by = Some("operator".into());
    completed.completed_at_ms = Some(-1);
    assert!(validate_document(&key, &head(completed.clone())).is_err());
    completed.completed_at_ms = Some(0);
    completed.checked = false;
    assert!(validate_document(&key, &head(completed)).is_err());
    assert!(
        validate_document(&item_key("other", "pressure").unwrap(), &head(item.clone())).is_err()
    );
    let mut value = serde_json::to_value(item).unwrap();
    value["qualification"] = Value::Bool(true);
    assert!(validate_document(&item_key("setup", "pressure").unwrap(), &head(value)).is_err());
    assert!(validate_document("operational/abort", &head(Value::Null)).is_err());
}
#[test]
fn maximum_nine_head_documents_fit_wire_budget_without_truncating_escaped_text() {
    let item = ItemDefinition {
        list_id: "l".repeat(80),
        generation: "g".repeat(80),
        id: "i".repeat(80),
        text: "\u{1}".repeat(500),
        complete_on: Some(Sequence::NitrogenTest),
        order: 127,
    };
    let mut completed = receipt(&item);
    completed.completed_by = Some("\u{2}".repeat(96));
    let manifest = ListManifest::from_items(
        item.list_id.clone(),
        item.generation.clone(),
        "\u{3}".repeat(120),
        true,
        31,
        std::slice::from_ref(&item),
    )
    .unwrap();
    let actors = (0..9)
        .map(|n| format!("{n}{}", "x".repeat(95)))
        .collect::<Vec<_>>();
    for (key, value) in [
        (
            item_key(&item.list_id, &item.id).unwrap(),
            serde_json::to_value(&item).unwrap(),
        ),
        (
            progress_key(&item.list_id, &item.id).unwrap(),
            serde_json::to_value(completed).unwrap(),
        ),
        (
            list_key(&item.list_id).unwrap(),
            serde_json::to_value(manifest).unwrap(),
        ),
    ] {
        let doc = Document {
            heads: (0..9)
                .map(|n| Revision {
                    clock: actors
                        .iter()
                        .enumerate()
                        .map(|(i, id)| (id.clone(), u64::MAX - u64::from(i != n)))
                        .collect(),
                    value: value.clone(),
                })
                .collect(),
        };
        assert!(serde_json::to_vec(&doc).unwrap().len() < 65_536);
        validate_document(&key, &doc).unwrap();
    }
}
#[test]
fn policy_and_definition_conflicts_defer_the_entire_catalog() {
    let item = definition("setup", "pressure", "generation-one", 0);
    let docs = catalog(&[item], true);
    for key in [
        POLICY_KEY.into(),
        list_key("setup").unwrap(),
        item_key("setup", "pressure").unwrap(),
    ] {
        let mut conflicted = docs.clone();
        let entry = conflicted.get_mut(&key).unwrap();
        let mut other = entry.heads[0].clone();
        other.clock = BTreeMap::from([("rrls".into(), 1)]);
        entry.heads.push(other);
        assert!(
            project(&conflicted).is_err(),
            "conflicting definition accepted:{key}"
        );
    }
}

#[test]
fn maximum_active_catalog_projects_without_transmitting_item_text_in_progress_delta() {
    let mut documents = BTreeMap::new();
    let mut lists = Vec::new();
    for list in 0..32 {
        let name = format!("{list}{}", "l".repeat(78));
        let items = (0..32)
            .map(|item| ItemDefinition {
                list_id: name.clone(),
                generation: "g".repeat(80),
                id: format!("{item}{}", "i".repeat(78)),
                text: "x".repeat(500),
                complete_on: None,
                order: item,
            })
            .collect::<Vec<_>>();
        let manifest = ListManifest::from_items(
            name.clone(),
            "g".repeat(80),
            "T".repeat(120),
            true,
            list,
            &items,
        )
        .unwrap();
        lists.push(manifest.clone());
        documents.insert(list_key(&name).unwrap(), head(manifest));
        for item in items {
            documents.insert(item_key(&name, &item.id).unwrap(), head(&item));
            let progress = Progress::unchecked(&item).unwrap();
            let delta = serde_json::to_vec(&progress).unwrap();
            assert!(!String::from_utf8_lossy(&delta).contains(&item.text));
            assert!(delta.len() < 700);
            documents.insert(progress_key(&name, &item.id).unwrap(), head(progress));
        }
    }
    documents.insert(
        POLICY_KEY.into(),
        head(Policy::from_lists(true, &lists).unwrap()),
    );
    let projected = project(&documents).unwrap();
    assert_eq!(projected.lists.len(), 32);
    assert_eq!(
        projected
            .lists
            .iter()
            .map(|list| list.items.len())
            .sum::<usize>(),
        1024
    );
    assert!(!projected.required_complete());
}
#[test]
fn automated_receipts_keep_engine_origin_but_are_data_not_operational_evidence() {
    let mut item = definition("setup", "pressure", "generation-one", 0);
    item.complete_on = Some(Sequence::NitrogenTest);
    let mut progress = receipt(&item);
    assert!(
        progress.matches(&item).is_err(),
        "operator evidence on an automatic binding must be an explicit override"
    );
    progress.completion_source = Some(CompletionSource::ManualOverride);
    assert!(progress.matches(&item).unwrap());
    progress.completion_source = Some(CompletionSource::AutomatedSequence);
    assert!(
        progress.matches(&item).is_err(),
        "an automated receipt needs its real engine actor"
    );
    progress.completed_by = Some(Sequence::NitrogenTest.actor().into());
    assert!(progress.matches(&item).unwrap());
    let mut docs = catalog(std::slice::from_ref(&item), false);
    docs.insert(progress_key("setup", "pressure").unwrap(), head(progress));
    let projected = project(&docs).unwrap();
    assert!(projected.required_complete());
    // The projection has no callback, saved qualification, abort latch or output.
    assert_eq!(
        projected.lists[0].items[0].progress.completed_by.as_deref(),
        Some("automated:nitrogen_test")
    );
}

#[test]
fn encoded_document_counter_is_exact_and_does_not_grow_after_quota_failure() {
    use std::io::Write;
    let item = definition("setup", "pressure", "generation-one", 0);
    let doc = head(item);
    let key = item_key("setup", "pressure").unwrap();
    assert_eq!(
        validated_size(&key, &doc).unwrap(),
        serde_json::to_vec(&doc).unwrap().len()
    );
    let mut writer = EncodedSize(65_535);
    writer.write_all(b"a").unwrap();
    assert!(writer.write_all(b"b").is_err());
    assert_eq!(writer.0, 65_536);
    let mut invalid = doc;
    invalid.heads[0].value["text"] = Value::String("x".repeat(65_536));
    assert!(validate_document(&key, &invalid).is_err());
}
