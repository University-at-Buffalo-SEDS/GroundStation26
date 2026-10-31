use super::*;
use crate::{Revision, journal::Limits};
use ring::signature::KeyPair;
fn path(name: &str) -> std::path::PathBuf {
    crate::test_directory::reserve(&format!("gs-journal-replication-{name}"))
        .join("journal.sqlite3")
}
fn doc(actor: &str, n: u64, value: serde_json::Value) -> Document {
    Document {
        heads: vec![Revision {
            clock: std::collections::BTreeMap::from([(actor.into(), n)]),
            value,
        }],
    }
}
async fn journals() -> (Journal, Journal, std::path::PathBuf, std::path::PathBuf) {
    let a = path("a");
    let b = path("b");
    let peers = vec!["gs".into(), "rrls".into()];
    (
        Journal::open(&a, "test".into(), peers.clone(), Limits::default())
            .await
            .unwrap(),
        Journal::open(&b, "test".into(), peers, Limits::default())
            .await
            .unwrap(),
        a,
        b,
    )
}
async fn pump(
    a: &Journal,
    b: &Journal,
    ga: &mut JournalRadio,
    gb: &mut JournalRadio,
    start: u64,
    duration: u64,
    loss: bool,
) -> Vec<(u64, Vec<u8>)> {
    let key = Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap();
    let mut count = 0usize;
    let mut wire_budget = Vec::new();
    for now in (start..start + duration).step_by(250) {
        let mut forward = Vec::new();
        ga.poll(a, now, &key, |_, wire| {
            forward.push(wire.to_vec());
            true
        })
        .await
        .unwrap();
        for wire in forward {
            wire_budget.push((now, wire.clone()));
            count += 1;
            if !loss || !count.is_multiple_of(7) {
                gb.receive(b, &wire, now, key.public_key().as_ref(), |_, _| Ok(()))
                    .await
                    .unwrap();
            }
        }
        let mut reverse = Vec::new();
        gb.poll(b, now, &key, |_, wire| {
            reverse.push(wire.to_vec());
            true
        })
        .await
        .unwrap();
        for wire in reverse {
            count += 1;
            if !loss || !count.is_multiple_of(11) {
                ga.receive(a, &wire, now, key.public_key().as_ref(), |_, _| Ok(()))
                    .await
                    .unwrap();
            }
        }
    }
    wire_budget
}
#[tokio::test]
async fn signed_incremental_replication_recovers_loss_and_sends_only_changed_rows() {
    let (a, b, pa, pb) = journals().await;
    a.merge(
        "definition",
        &doc("gs", 1, serde_json::json!("x".repeat(500))),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    a.merge(
        "progress",
        &doc("gs", 1, serde_json::json!(false)),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    let mut ga = JournalRadio::new("test".into(), "gs".into(), vec!["rrls".into()]).unwrap();
    let mut gb = JournalRadio::new("test".into(), "rrls".into(), vec!["gs".into()]).unwrap();
    let budget = pump(&a, &b, &mut ga, &mut gb, 0, 180_000, true).await;
    assert_eq!(
        a.snapshot().await.unwrap().documents,
        b.snapshot().await.unwrap().documents
    );
    for pair in budget.windows(2) {
        assert!(pair[1].0 - pair[0].0 >= (pair[0].1.len() as u64 * 1000).div_ceil(256));
    }
    let unchanged = b
        .catalog_page("", 64)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.key == "definition")
        .unwrap();
    a.merge(
        "progress",
        &doc("gs", 2, serde_json::json!(true)),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    let delta = pump(&a, &b, &mut ga, &mut gb, 180_000, 120_000, true).await;
    let signing_key = Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap();
    for (_, wire) in &delta {
        let message = open(
            wire,
            "test",
            "rrls",
            &["gs".into()],
            signing_key.public_key().as_ref(),
        )
        .unwrap()
        .message;
        assert!(
            !matches!(message, Message::Chunk {key,..} if key=="definition"),
            "checkbox delta retransmitted unchanged definition text"
        );
    }
    assert_eq!(
        a.snapshot().await.unwrap().documents,
        b.snapshot().await.unwrap().documents
    );
    assert_eq!(
        b.catalog_page("", 64)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.key == "definition")
            .unwrap(),
        unchanged
    );
    a.close().await;
    b.close().await;
    std::fs::remove_dir_all(pa.parent().unwrap()).unwrap();
    std::fs::remove_dir_all(pb.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn signature_domain_and_profile_are_checked_before_staging() {
    let (a, b, pa, pb) = journals().await;
    let key = Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap();
    let mut radio = JournalRadio::new("test".into(), "rrls".into(), vec!["gs".into()]).unwrap();
    let meta = a.metadata().await.unwrap();
    let message = Message::Hello {
        profile: 255,
        incarnation: meta.incarnation,
        revision: 0,
        max_documents: 8192,
        max_bytes: 64 * 1024 * 1024,
    };
    let wire = seal(&key, "test", "gs", "rrls", message).unwrap();
    assert!(
        radio
            .receive(&b, &wire, 0, key.public_key().as_ref(), |_, _| Ok(()))
            .await
            .is_err()
    );
    let mut corrupt = wire.clone();
    corrupt[8] ^= 1;
    assert!(
        radio
            .receive(&b, &corrupt, 0, key.public_key().as_ref(), |_, _| Ok(()))
            .await
            .is_err()
    );
    assert!(
        open(
            &wire,
            "wrong",
            "rrls",
            &["gs".into()],
            key.public_key().as_ref()
        )
        .is_err()
    );
    assert!(
        crate::auth::open(
            &wire,
            "test",
            "rrls",
            &["gs".into()],
            key.public_key().as_ref()
        )
        .is_err()
    );
    assert_eq!(b.metadata().await.unwrap().documents, 0);
    a.close().await;
    b.close().await;
    std::fs::remove_dir_all(pa.parent().unwrap()).unwrap();
    std::fs::remove_dir_all(pb.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn row_change_during_incremental_read_cannot_skip_another_keys_update() {
    let (a, b, pa, pb) = journals().await;
    for key in ["a", "b"] {
        let original = doc("gs", 1, serde_json::json!(0));
        a.merge(key, &original, |_, _| Ok(())).await.unwrap();
        b.merge(key, &original, |_, _| Ok(())).await.unwrap();
    }
    a.merge("a", &doc("gs", 2, serde_json::json!(1)), |_, _| Ok(()))
        .await
        .unwrap();
    a.merge("b", &doc("gs", 2, serde_json::json!(2)), |_, _| Ok(()))
        .await
        .unwrap();
    let cursor = ScanCursor::Changes { after: 2 };
    let page = a.scan(&cursor).await.unwrap();
    let first = page.entry.unwrap();
    assert_eq!(first.key, "a");
    assert_eq!(first.revision, 3);
    let entry = Entry {
        key: first.key,
        hash: first.hash,
        cursor_revision: first.revision,
    };
    let request = Request {
        id: 1,
        cursor: cursor.clone(),
        last_sent: Some(0),
    };
    let mut receiver = Protocol {
        remote: Some(Remote {
            incarnation: page.metadata.incarnation.clone(),
            advertised: page.metadata.revision,
            baseline: 2,
            cursor: cursor.clone(),
            request: Some(request),
            pending: None,
        }),
        last_hello: Some(0),
        request_id: 1,
        ..Default::default()
    };
    receiver
        .receive(
            &b,
            Message::Entry {
                incarnation: page.metadata.incarnation.clone(),
                request: 1,
                cursor: cursor.clone(),
                fence: 4,
                entry: Some(entry.clone()),
            },
            0,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    // A newer A appears after the catalog read, before the actual document read.
    a.merge("a", &doc("gs", 3, serde_json::json!(3)), |_, _| Ok(()))
        .await
        .unwrap();
    let mut sender = Protocol::default();
    sender
        .receive(
            &a,
            Message::Read {
                incarnation: page.metadata.incarnation,
                request: 1,
                cursor: cursor.clone(),
                fence: 4,
                entry,
                missing: None,
            },
            1,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    let updated = sender.poll(&a, 2).await.unwrap().unwrap();
    assert!(
        matches!(&updated,Message::Entry {entry:Some(entry),..} if entry.cursor_revision==3),
        "advancing toA's newest revision would skipB4"
    );
    receiver
        .receive(&b, updated, 2, |_, _| Ok(()))
        .await
        .unwrap();
    let mut ga = JournalRadio::new("test".into(), "gs".into(), vec!["rrls".into()]).unwrap();
    ga.peers[0].protocol = sender;
    let mut gb = JournalRadio::new("test".into(), "rrls".into(), vec!["gs".into()]).unwrap();
    gb.peers[0].protocol = receiver;
    pump(&a, &b, &mut ga, &mut gb, 10, 180_000, true).await;
    assert_eq!(
        a.snapshot().await.unwrap().documents,
        b.snapshot().await.unwrap().documents
    );
    a.close().await;
    b.close().await;
    std::fs::remove_dir_all(pa.parent().unwrap()).unwrap();
    std::fs::remove_dir_all(pb.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn large_slow_transfer_survives_more_than_a_minute_without_forgetting_progress() {
    let (a, b, pa, pb) = journals().await;
    let value = doc("gs", 1, serde_json::json!("x".repeat(48_000)));
    a.merge("large", &value, |_, _| Ok(())).await.unwrap();
    let mut ga = JournalRadio::new("test".into(), "gs".into(), vec!["rrls".into()]).unwrap();
    let mut gb = JournalRadio::new("test".into(), "rrls".into(), vec!["gs".into()]).unwrap();
    pump(&a, &b, &mut ga, &mut gb, 0, 2_000_000, true).await;
    assert_eq!(b.document("large").await.unwrap(), Some(value));
    a.close().await;
    b.close().await;
    std::fs::remove_dir_all(pa.parent().unwrap()).unwrap();
    std::fs::remove_dir_all(pb.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn missing_chunk_mask_retransmits_only_the_missing_piece() {
    let (a, b, pa, pb) = journals().await;
    let value = doc("gs", 1, serde_json::json!("x".repeat(1000)));
    a.merge("large", &value, |_, _| Ok(())).await.unwrap();
    let page = a
        .scan(&ScanCursor::Full {
            after: String::new(),
        })
        .await
        .unwrap();
    let entry = page.entry.unwrap();
    let total = transport::document_bytes(&value).len().div_ceil(128);
    let mut missing = vec![0u8; total.div_ceil(8)];
    missing[0] = 1 << 3;
    let mut protocol = Protocol {
        last_hello: Some(0),
        ..Default::default()
    };
    protocol
        .receive(
            &a,
            Message::Read {
                incarnation: page.metadata.incarnation,
                request: 1,
                cursor: ScanCursor::Full {
                    after: String::new(),
                },
                fence: page.metadata.revision,
                entry: Entry {
                    key: entry.key,
                    hash: entry.hash,
                    cursor_revision: entry.revision,
                },
                missing: Some(B64.encode(missing)),
            },
            1,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert!(matches!(
        protocol.poll(&a, 2).await.unwrap(),
        Some(Message::Chunk { index: 3, .. })
    ));
    assert!(protocol.transfer.is_none());
    a.close().await;
    b.close().await;
    std::fs::remove_dir_all(pa.parent().unwrap()).unwrap();
    std::fs::remove_dir_all(pb.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn failed_send_retries_the_exact_signed_frame_without_an_idle_burst() {
    let (a, b, pa, pb) = journals().await;
    let key = Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap();
    let mut radio = JournalRadio::new("test".into(), "gs".into(), vec!["rrls".into()]).unwrap();
    let mut first = Vec::new();
    let result = radio
        .poll(&a, 100, &key, |_, wire| {
            first = wire.to_vec();
            false
        })
        .await
        .unwrap()
        .unwrap();
    assert!(!result.accepted);
    let retry_at = 100 + (first.len() as u64 * 1000).div_ceil(256);
    for now in [0, 100, retry_at - 1] {
        assert!(
            radio
                .poll(&a, now, &key, |_, _| panic!("unpaced retry"))
                .await
                .unwrap()
                .is_none()
        );
    }
    radio
        .poll(&a, retry_at, &key, |_, wire| {
            assert_eq!(wire, first);
            true
        })
        .await
        .unwrap();
    assert!(
        radio
            .poll(&a, retry_at, &key, |_, _| panic!("same-clock burst"))
            .await
            .unwrap()
            .is_none()
    );
    a.close().await;
    b.close().await;
    std::fs::remove_dir_all(pa.parent().unwrap()).unwrap();
    std::fs::remove_dir_all(pb.parent().unwrap()).unwrap();
}
