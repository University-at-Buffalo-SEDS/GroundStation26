use super::*;
fn marker(mask: u8, policy: Option<ResetProofPolicy>) -> RecoveryPending {
    let backend = "01000000000000000000000000000000".to_string();
    let targets = ["RF", "FC", "PB", "AB", "VB", "GB", "DAQ"]
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, board)| {
            let mut body = [0; 32];
            body[0] = 1;
            body[1] = 1;
            body[4..12].copy_from_slice(&1u64.to_le_bytes());
            body[12..20].copy_from_slice(&1u64.to_le_bytes());
            body[20..24].copy_from_slice(&1u32.to_le_bytes());
            body[24..32].copy_from_slice(&1u64.to_le_bytes());
            RecoveryTarget {
                board: (*board).into(),
                desired: 0,
                boot: 1,
                epoch: 1,
                intent: RecoveryIntent {
                    body,
                    source_utc_ms: 100,
                    backend: backend.clone(),
                    principal_context: "a".repeat(32),
                },
                kind: RecoveryIntentKind::Normal,
                original_first: None,
                uncertain_first: None,
            }
        })
        .collect();
    RecoveryPending {
        reset_policy: policy,
        owner: 5,
        backend,
        generation: 2,
        manual_epoch: 0,
        observed_manual_epoch: 0,
        origin_utc_ms: 100,
        prior_automatic_actions_inhibited: true,
        targets,
        verified: true,
        published: true,
    }
}
fn policy(mode: crate::station_mode::Mode, mask: u8) -> ResetProofPolicy {
    ResetProofPolicy {
        mode,
        required_mask: crate::board_recovery::scope::base_mask(mode),
        verified_mask: mask,
    }
}
#[test]
fn legacy_all_seven_records_keep_wire_shape_and_strict_manual_utc_requirement() {
    let old = marker(0x7f, None);
    old.validate().unwrap();
    assert!(old.verified_startup_policy());
    let json = serde_json::to_value(&old).unwrap();
    assert!(json.get("reset_policy").is_none());
    assert_eq!(
        serde_json::from_value::<RecoveryPending>(json).unwrap(),
        old
    );
    assert!(!marker(0x78, None).verified_startup_policy());
}
#[test]
fn mode_policy_never_infers_unverified_boards_or_weakens_required_masks() {
    use crate::station_mode::Mode::*;
    for (mode, mask) in [(Normal, 0x7f), (TestFire, 0x78), (Hitl, 0)] {
        marker(mask, Some(policy(mode, mask))).validate().unwrap();
    }
    assert!(marker(0x78, Some(policy(Normal, 0x78))).validate().is_err());
    assert!(marker(0, Some(policy(TestFire, 0))).validate().is_err());
    let mut wrong = policy(TestFire, 0x78);
    wrong.verified_mask = 0x7f;
    assert!(marker(0x78, Some(wrong)).validate().is_err());
    assert!(marker(0, None).validate().is_err());
}
#[test]
fn actual_journal_restart_rechecks_saved_subset_against_current_mode_without_schema_migration() {
    use crate::station_mode::Mode::*;
    let path = std::env::temp_dir().join(format!(
        "gs-reset-policy-{:032x}.json",
        rand::random::<u128>()
    ));
    let mut store = Store::open(path.clone(), false, None, 1000, 0).unwrap();
    let local = crate::station_mode::local_mode();
    let base = crate::board_recovery::scope::base_mask(local);
    store.data.recovery_pending = Some(marker(base, Some(policy(local, base))));
    store.data.automatic_actions_inhibited = false;
    atomic_save(&path, &store.data).unwrap();
    drop(store);
    let mut store = Store::open(path.clone(), false, None, 2000, 0).unwrap();
    assert!(!store.automatic_actions_inhibited());
    // A published subset from a different mode is never adopted as a local
    // verified reset; an explicit new recovery remains required.
    let other = if local == Hitl { TestFire } else { Hitl };
    let other_mask = crate::board_recovery::scope::base_mask(other);
    store.data.recovery_pending = Some(marker(other_mask, Some(policy(other, other_mask))));
    atomic_save(&path, &store.data).unwrap();
    drop(store);
    let store = Store::open(path.clone(), false, None, 3000, 0).unwrap();
    assert!(store.automatic_actions_inhibited());
    assert!(store.manual_utc_key().is_none());
    drop(store);
    std::fs::remove_file(path).unwrap();
}
