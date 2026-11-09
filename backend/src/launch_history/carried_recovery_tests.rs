use super::*;
const BACKEND: &str = "01000000000000000000000000000000";
fn target(board: &str, source_utc_ms: u64, backend: &str, kind: RecoveryIntentKind) -> RecoveryTarget {
    let mut body = [0; 32];
    body[0] = 1;
    body[1] = 1;
    body[4..12].copy_from_slice(&9u64.to_le_bytes());
    body[12..20].copy_from_slice(&42u64.to_le_bytes());
    body[20..24].copy_from_slice(&3u32.to_le_bytes());
    // The sealed issuer is the first 8 bytes of the issuing backend identity.
    body[24..32].copy_from_slice(&u64::from(u8::from_str_radix(&backend[..2], 16).unwrap()).to_le_bytes());
    RecoveryTarget {
        board: board.into(),
        desired: 0,
        boot: 42,
        epoch: 3,
        intent: RecoveryIntent { body, source_utc_ms, backend: backend.into(), principal_context: "a".repeat(32) },
        kind,
        original_first: None,
        uncertain_first: None,
    }
}
fn marker(targets: Vec<RecoveryTarget>) -> RecoveryPending {
    RecoveryPending {
        reset_policy: None,
        owner: 5,
        backend: BACKEND.into(),
        generation: 2,
        manual_epoch: 0,
        observed_manual_epoch: 0,
        origin_utc_ms: 100,
        prior_automatic_actions_inhibited: false,
        targets,
        verified: false,
        published: false,
    }
}
/// A targeted retry keeps the boards of an earlier unfinished recovery that
/// it does not include: their original intents, sealed earlier (possibly by
/// an earlier backend process), are carried unchanged next to its own.
#[test]
fn a_targeted_retry_carries_earlier_unfinished_targets_unchanged() {
    let other_backend = "02000000000000000000000000000000";
    let own = target("AB", 100, BACKEND, RecoveryIntentKind::Normal);
    assert!(marker(vec![own.clone(), target("VB", 50, other_backend, RecoveryIntentKind::Normal)]).validate().is_ok());
    // A carried target is strictly older than this attempt's boundary.
    assert!(marker(vec![own.clone(), target("VB", 150, other_backend, RecoveryIntentKind::Normal)]).validate().is_err());
    assert!(marker(vec![own.clone(), target("VB", 100, other_backend, RecoveryIntentKind::Normal)]).validate().is_err());
    // Every marker has at least one target of its own attempt.
    assert!(marker(vec![target("VB", 50, BACKEND, RecoveryIntentKind::Normal)]).validate().is_err());
    // Reset all never carries targets.
    let mut reset = marker(vec![own.clone(), target("VB", 50, BACKEND, RecoveryIntentKind::Normal)]);
    reset.reset_policy = Some(ResetProofPolicy { mode: crate::station_mode::local_mode(), required_mask: 0, verified_mask: 0b11000 });
    assert!(reset.validate().is_err());
}
fn closed_store_with(pending: &RecoveryPending, launch_attempt: bool) -> (PathBuf, Store) {
    let path = std::env::temp_dir().join(format!("gs-carried-test-{:032x}.json", rand::random::<u128>()));
    let mut store = Store::open(path.clone(), false, None, 10_000, 0).unwrap();
    store.data.recovery_pending = Some(pending.clone());
    store.data.automatic_actions_inhibited = true;
    store.data.launch_attempt = Some(launch_attempt);
    atomic_save(&path, &store.data).unwrap();
    drop(store);
    let store = Store::open(path.clone(), false, None, 11_000, 0).unwrap();
    assert!(store.recovery_closed(), "an unpublished marker restores closed");
    (path, store)
}
/// A failed recovery whose boards all read back healthy afterwards finishes:
/// the marker is retired (never published as a verified reset), the journal
/// reopens and only a real Launch attempt keeps automation inhibited.
#[test]
fn a_satisfied_pending_recovery_is_retired_and_reopens_the_journal() {
    let pending = marker(vec![target("AB", 100, BACKEND, RecoveryIntentKind::Normal)]);
    for launch_attempt in [false, true] {
        let (path, mut store) = closed_store_with(&pending, launch_attempt);
        assert!(store.complete_satisfied_recovery(6, &pending).is_err(), "another owner");
        let mut changed = pending.clone();
        changed.generation = 3;
        assert!(store.complete_satisfied_recovery(5, &changed).is_err(), "a different marker");
        store.complete_satisfied_recovery(5, &pending).unwrap();
        assert!(store.recovery_pending().is_none());
        assert!(!store.recovery_closed());
        assert_eq!(store.launch_locked(), launch_attempt);
        assert_eq!(store.automatic_actions_inhibited(), launch_attempt);
        if let Some((writer, ticket)) = store.waiter() { writer.wait(ticket).unwrap(); }
        drop(store);
        let restored = Store::open(path, false, None, 12_000, 0).unwrap();
        assert!(restored.recovery_pending().is_none() && !restored.recovery_closed());
        assert_eq!(restored.launch_locked(), launch_attempt);
    }
}
