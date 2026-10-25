use super::*;
use crate::{Revision, checklists};
fn temp() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!(
            "gs-paired-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .join("journal.sqlite3")
}
fn peers() -> Vec<String> {
    vec!["gs".into(), "rrls".into()]
}
fn doc(actor: &str, n: u64, value: serde_json::Value) -> Document {
    Document {
        heads: vec![Revision {
            clock: BTreeMap::from([(actor.into(), n)]),
            value,
        }],
    }
}
async fn open(path: &Path, limits: Limits) -> Journal {
    Journal::open(path, "test".into(), peers(), limits)
        .await
        .unwrap()
}
#[tokio::test]
async fn only_changed_row_is_persisted_and_replay_is_a_noop() {
    let path = temp();
    let journal = open(&path, Limits::default()).await;
    let first = doc("gs", 1, serde_json::json!("x".repeat(500)));
    journal
        .merge("definition", &first, |_, _| Ok(()))
        .await
        .unwrap();
    let before = journal.catalog_page("", 64).await.unwrap();
    let progress = doc("gs", 1, serde_json::json!({"checked":true}));
    journal
        .merge("progress", &progress, |_, _| Ok(()))
        .await
        .unwrap();
    let delta = journal.changed_page(before[0].revision, 64).await.unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].key, "progress");
    let current = journal.metadata().await.unwrap();
    assert!(
        journal
            .merge("progress", &progress, |_, _| Ok(()))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(journal.metadata().await.unwrap(), current);
    assert_eq!(journal.catalog_page("", 64).await.unwrap()[0], before[0]);
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn concurrent_offline_heads_and_deletion_survive_restart_without_resurrection() {
    let path = temp();
    let journal = open(&path, Limits::default()).await;
    let a = doc("gs", 1, serde_json::json!(true));
    let b = doc("rrls", 1, serde_json::json!(false));
    journal.merge("item", &a, |_, _| Ok(())).await.unwrap();
    journal.merge("item", &b, |_, _| Ok(())).await.unwrap();
    assert_eq!(
        journal.document("item").await.unwrap().unwrap().heads.len(),
        2
    );
    let resolved = Document {
        heads: vec![Revision {
            clock: BTreeMap::from([("gs".into(), 2), ("rrls".into(), 1)]),
            value: serde_json::Value::Null,
        }],
    };
    journal
        .merge("item", &resolved, |_, _| Ok(()))
        .await
        .unwrap();
    journal.close().await;
    let journal = open(&path, Limits::default()).await;
    assert!(
        journal
            .merge("item", &a, |_, _| Ok(()))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(journal.document("item").await.unwrap(), Some(resolved));
    assert_eq!(
        journal.metadata().await.unwrap().documents,
        1,
        "tombstones must count against quota"
    );
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn exact_quota_validation_and_equivocation_fail_without_mutation() {
    let path = temp();
    let value = doc("gs", 1, serde_json::json!(123));
    let bytes = "a".len() + transport::document_bytes(&value).len();
    let journal = open(
        &path,
        Limits {
            documents: 1,
            encoded_bytes: bytes,
        },
    )
    .await;
    journal.merge("a", &value, |_, _| Ok(())).await.unwrap();
    let before = journal.snapshot().await.unwrap();
    for (key, value) in [
        ("b", value.clone()),
        ("a", doc("gs", 1, serde_json::json!(124))),
        ("a", doc("gs", 2, serde_json::json!(1234))),
        ("a", doc("stranger", 3, serde_json::json!(0))),
    ] {
        assert!(journal.merge(key, &value, |_, _| Ok(())).await.is_err());
    }
    assert!(
        journal
            .merge(
                "a",
                &doc("gs", 2, serde_json::json!(0)),
                |_, _| anyhow::bail!("typed rejection")
            )
            .await
            .is_err()
    );
    let after = journal.snapshot().await.unwrap();
    assert_eq!(before.metadata, after.metadata);
    assert_eq!(before.documents, after.documents);
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn concurrent_async_updates_are_serialized_without_losing_metadata() {
    let path = temp();
    let journal = open(&path, Limits::default()).await;
    let mut tasks = Vec::new();
    for n in 0..32 {
        let journal = journal.clone();
        tasks.push(tokio::spawn(async move {
            journal
                .merge(
                    &format!("item-{n}"),
                    &doc("gs", 1, serde_json::json!(n)),
                    |_, _| Ok(()),
                )
                .await
                .unwrap()
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let snapshot = journal.snapshot().await.unwrap();
    assert_eq!(snapshot.documents.len(), 32);
    assert_eq!(snapshot.metadata.revision, 32);
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn typed_checklist_staging_is_inert_and_large_catalog_exceeds_legacy_cap() {
    let path = temp();
    let journal = open(&path, Limits::default()).await;
    for n in 0..300 {
        let item = checklists::ItemDefinition {
            list_id: "setup".into(),
            generation: "one".into(),
            id: format!("item-{n}"),
            text: "Check valve".into(),
            complete_on: Some(checklists::Sequence::ValveSelfTest),
            order: (n % 128) as u16,
        };
        let key = checklists::item_key("setup", &item.id).unwrap();
        journal
            .merge(
                &key,
                &doc("gs", 1, serde_json::to_value(item).unwrap()),
                checklists::validate_document,
            )
            .await
            .unwrap();
    }
    let snapshot = journal.snapshot().await.unwrap();
    assert_eq!(snapshot.documents.len(), 300);
    assert!(
        checklists::project(&snapshot.documents).is_err(),
        "staged definitions without manifests must not activate"
    );
    assert!(!path.parent().unwrap().join("checklists.json").exists());
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn utf8_keys_use_encoded_byte_quotas_and_reopen_consistently() {
    let path = temp();
    let journal = open(&path, Limits::default()).await;
    let key = "settings/é";
    let value = doc("gs", 1, serde_json::json!(1));
    journal.merge(key, &value, |_, _| Ok(())).await.unwrap();
    let metadata = journal.metadata().await.unwrap();
    assert_eq!(
        metadata.encoded_bytes,
        key.len() + transport::document_bytes(&value).len()
    );
    journal.close().await;
    let journal = open(&path, Limits::default()).await;
    assert_eq!(journal.metadata().await.unwrap(), metadata);
    assert!(
        Journal::open(&path, "different".into(), peers(), Limits::default())
            .await
            .is_err()
    );
    assert_eq!(journal.document(key).await.unwrap(), Some(value));
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_storage_does_not_block_unrelated_async_control_work() {
    let path = temp();
    let journal = open(&path, Limits::default()).await;
    let external = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let mut lock = external.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let updating = journal.clone();
    let task = tokio::spawn(async move {
        updating
            .merge(
                "progress",
                &doc("gs", 1, serde_json::json!(true)),
                |_, _| Ok(()),
            )
            .await
    });
    tokio::task::yield_now().await;
    let control = tokio::spawn(async {
        tokio::task::yield_now().await;
        "emergency dispatch remains runnable"
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(500), control)
            .await
            .unwrap()
            .unwrap(),
        "emergency dispatch remains runnable"
    );
    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    drop(lock);
    assert!(task.await.unwrap().is_ok());
    external.close().await;
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn rejected_overflow_leaves_database_bytes_and_existing_rows_unchanged() {
    let path = temp();
    let journal = open(
        &path,
        Limits {
            documents: 1,
            encoded_bytes: 1024,
        },
    )
    .await;
    journal
        .merge("one", &doc("gs", 1, serde_json::json!("kept")), |_, _| {
            Ok(())
        })
        .await
        .unwrap();
    // WAL checkpoint makes the physical-database comparison deterministic.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&journal.pool)
        .await
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(
        journal
            .merge(
                "two",
                &doc("gs", 1, serde_json::json!("refused")),
                |_, _| Ok(())
            )
            .await
            .is_err()
    );
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&journal.pool)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    journal.close().await;
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
