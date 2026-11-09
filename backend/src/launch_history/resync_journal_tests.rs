use super::*;
fn pending(board: &str, operation: u8, desired: u8, boot: u64) -> RecoveryPending {
    let backend = "01000000000000000000000000000000".to_string();
    let mut body = [0; 32];
    body[0] = 1;
    body[1] = operation;
    body[2] = desired;
    body[4..12].copy_from_slice(&9u64.to_le_bytes());
    body[12..20].copy_from_slice(&boot.to_le_bytes());
    body[20..24].copy_from_slice(&3u32.to_le_bytes());
    body[24..32].copy_from_slice(&1u64.to_le_bytes());
    RecoveryPending {
        reset_policy: None,
        owner: 5,
        backend: backend.clone(),
        generation: 2,
        manual_epoch: 0,
        observed_manual_epoch: 0,
        origin_utc_ms: 100,
        prior_automatic_actions_inhibited: false,
        targets: vec![RecoveryTarget {
            board: board.into(),
            desired,
            boot,
            epoch: 3,
            intent: RecoveryIntent {
                body,
                source_utc_ms: 100,
                backend,
                principal_context: "a".repeat(32),
            },
            kind: RecoveryIntentKind::Normal,
            original_first: None,
            uncertain_first: None,
        }],
        verified: false,
        published: false,
    }
}
/// A durable RESYNC intent survives a restart only for AB/VB, a fill phase or
/// Armed, and a known boot generation. RECOVER keeps Startup/Idle only.
#[test]
fn resync_intent_is_journaled_only_for_ab_vb_fill_phases_and_armed() {
    for board in ["AB", "VB"] {
        for phase in 0..=6 {
            assert!(pending(board, 2, phase, 42).validate().is_ok(), "{board} {phase}");
            if phase > 1 { assert!(pending(board, 1, phase, 42).validate().is_err(), "RECOVER {board} {phase}"); }
        }
        for phase in [7, 8, 13, 14, 15] {
            assert!(pending(board, 2, phase, 42).validate().is_err(), "{board} {phase}");
        }
        assert!(pending(board, 2, 5, u64::MAX).validate().is_err(), "no commissioning resync");
        assert!(pending(board, 3, 5, 42).validate().is_err());
    }
    for board in ["FC", "RF", "PB", "GB", "DAQ"] {
        assert!(pending(board, 2, 5, 42).validate().is_err(), "{board}");
    }
    // A resync marker never counts as a verified Startup reset policy.
    let mut marker = pending("AB", 2, 5, 42);
    marker.verified = true;
    assert!(!marker.verified_startup_policy());
}
