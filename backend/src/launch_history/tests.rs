use super::*;
fn path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "gs-clock-test-{:032x}.json",
        rand::random::<u128>()
    ))
}
fn plus(t0: i64) -> LaunchClockMsg {
    LaunchClockMsg {
        kind: LaunchClockKind::TPlus,
        anchor_timestamp_ms: Some(t0),
        duration_ms: None,
    }
}
fn wait(store: &Store) {
    if let Some((writer, ticket)) = store.waiter() {
        writer.wait(ticket).unwrap();
    }
}
#[test]
fn primary_restart_preserves_t0_session_and_literal_daq_deadline() {
    let path = path();
    let mut state = Store::open(path.clone(), false, None, 10_000, 0).unwrap();
    let record = state.update_primary(plus(9000), 10_000, 0).unwrap();
    state.retain_daq([77, 29000], 1234, 10_000, 0).unwrap();
    wait(&state);
    drop(state);
    let restored = Store::open(path.clone(), false, None, 15_000, 0).unwrap();
    assert_eq!(restored.snapshot(), record);
    assert_eq!(restored.daq_clock(), [77, 29000]);
    assert_eq!(restored.elapsed(500), Some(6500));
    std::fs::remove_file(path).unwrap();
}
#[test]
fn recovery_repeats_are_idempotent_clock_corrections_do_not_change_monotonic_elapsed() {
    let path = path();
    let mut state = Store::open(path.clone(), false, None, 10_000, 0).unwrap();
    state.update_primary(plus(9000), 10_000, 0).unwrap();
    let event = state
        .mark_reached("alice", "session-hash", "primary", -10_000, 5000)
        .unwrap();
    assert_eq!(event.t_relative_ms, 6000);
    assert_eq!(event.reached_utc_ms, -10000);
    assert_eq!(
        state
            .mark_reached("bob", "other", "primary", 999999, 9000)
            .unwrap(),
        event
    );
    wait(&state);
    drop(state);
    let restored = Store::open(path.clone(), false, None, 16_000, 0).unwrap();
    assert_eq!(restored.events(), vec![event]);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn offline_reset_and_dual_restart_override_stale_tplus_but_preserve_audit() {
    let primary_path = path();
    let rr_path = path();
    let mut primary = Store::open(primary_path.clone(), false, None, 10000, 0).unwrap();
    let active = primary.update_primary(plus(9000), 10000, 0).unwrap();
    let mut rr = Store::open(rr_path.clone(), true, None, 10000, 0).unwrap();
    assert!(rr.apply_primary(active.clone(), true, 10000, 0).unwrap());
    rr.mark_reached("rr-operator", "hash", "rrls", 11000, 1000)
        .unwrap();
    wait(&rr);
    drop(rr);
    let reset = primary.reset_primary(12000, 2000).unwrap();
    wait(&primary);
    drop(primary);
    let primary = Store::open(primary_path.clone(), false, None, 14000, 0).unwrap();
    assert_eq!(primary.snapshot(), reset);
    let mut rr = Store::open(rr_path.clone(), true, None, 14000, 0).unwrap();
    assert_eq!(rr.snapshot(), active);
    assert!(!rr.synchronized);
    assert!(
        rr.apply_primary(primary.snapshot(), true, 14000, 0)
            .unwrap()
    );
    assert_eq!(rr.snapshot().clock, LaunchClockMsg::idle());
    assert!(!rr.apply_primary(active.clone(), true, 14000, 0).unwrap());
    assert!(rr.apply_primary(active, false, 14000, 0).is_err());
    assert_eq!(rr.events().len(), 1);
    wait(&rr);
    std::fs::remove_file(primary_path).unwrap();
    std::fs::remove_file(rr_path).unwrap();
}
#[test]
fn equal_revision_conflict_is_rejected_and_idle_telemetry_cannot_clear_live_clock() {
    let mut primary = Store::ephemeral();
    let active = primary.update_primary(plus(9000), 10000, 0).unwrap();
    assert_eq!(
        primary
            .update_primary(LaunchClockMsg::idle(), 11000, 1000)
            .unwrap(),
        active
    );
    let mut rr = Store::ephemeral();
    rr.observer = true;
    rr.apply_primary(active.clone(), true, 10000, 0).unwrap();
    let mut conflict = active;
    conflict.clock = plus(8000);
    conflict.t0_utc_ms = Some(8000);
    assert!(rr.apply_primary(conflict, true, 10000, 0).is_err());
}
#[test]
fn coalesced_writes_keep_every_event_and_previous_session() {
    let path = path();
    let mut state = Store::open(path.clone(), false, None, 10000, 0).unwrap();
    for n in 0..20 {
        state.confirm_new_launch_cycle(10000 + n, 0).unwrap();
        state
            .update_primary(
                LaunchClockMsg {
                    kind: LaunchClockKind::TMinus,
                    anchor_timestamp_ms: Some(8000 + n),
                    duration_ms: Some(1000),
                },
                10000 + n,
                0,
            )
            .unwrap();
        state.update_primary(plus(9000 + n), 10000 + n, 0).unwrap();
        state
            .mark_reached("operator", "hash", "primary", 11000 + n, 1)
            .unwrap();
        state.reset_primary(12000 + n, 2).unwrap();
    }
    wait(&state);
    drop(state);
    let state = Store::open(path.clone(), false, None, 13000, 0).unwrap();
    assert_eq!(state.events().len(), 20);
    assert_eq!(state.data.past_sessions.len(), 20);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn write_failure_never_acknowledges_durability_and_no_legacy_deadline_t0_guess() {
    let root = path();
    std::fs::write(&root, b"not-directory").unwrap();
    let mut state = Store::open(root.join("clock.json"), false, None, 10000, 0).unwrap();
    state.update_primary(plus(9000), 10000, 0).unwrap();
    let (writer, ticket) = state.waiter().unwrap();
    assert!(writer.wait(ticket).is_err());
    assert!(state.persistence_error().is_some());
    std::fs::remove_file(root).unwrap();
    let path = path();
    let mut state = Store::open(path, true, Some(([33, 29000], 0)), 10000, 0).unwrap();
    assert_eq!(state.snapshot().clock, LaunchClockMsg::idle());
    assert!(state.snapshot().t0_utc_ms.is_none());
    assert!(state.update_primary(plus(9000), 10000, 0).is_err());
}
#[test]
fn failed_reset_retry_uses_exact_positive_contract_without_another_generation() {
    let path = path();
    let saved = path.with_extension("saved");
    let mut state = Store::open(path.clone(), false, None, 10000, 0).unwrap();
    let expected = state.update_primary(plus(9000), 10000, 0).unwrap();
    wait(&state);
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    let reset = state
        .reset_expected_primary(&expected, 11000, 1000)
        .unwrap();
    let (writer, ticket) = state.waiter().unwrap();
    assert!(writer.wait(ticket).is_err());
    assert_eq!(reset.revision, expected.revision + 1);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&saved, &path).unwrap();
    assert_eq!(
        state
            .reset_expected_primary(&expected, 12000, 2000)
            .unwrap(),
        reset
    );
    wait(&state);
    let restored = Store::open(path.clone(), false, None, 13000, 0).unwrap();
    assert_eq!(restored.snapshot(), reset);
    state.confirm_new_launch_cycle(13000, 3000).unwrap();
    state
        .update_primary(
            LaunchClockMsg {
                kind: LaunchClockKind::TMinus,
                anchor_timestamp_ms: Some(11000),
                duration_ms: Some(1000),
            },
            13000,
            3000,
        )
        .unwrap();
    state.update_primary(plus(12000), 13000, 3000).unwrap();
    let newer = state.snapshot();
    assert!(
        state
            .reset_expected_primary(&expected, 14000, 4000)
            .is_err()
    );
    assert_eq!(state.snapshot(), newer);
    wait(&state);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn a_new_unknown_primary_idle_is_not_an_explicit_reset_of_a_retained_observer_clock() {
    let path = path();
    let mut observer = Store::open(path, true, None, 10000, 0).unwrap();
    let mut primary = Store::ephemeral();
    let active = primary.update_primary(plus(9000), 10000, 0).unwrap();
    observer
        .apply_primary(active.clone(), true, 10000, 0)
        .unwrap();
    let mut replacement = Store::ephemeral();
    assert!(
        observer
            .apply_primary(replacement.snapshot(), true, 11000, 1000)
            .is_err()
    );
    assert_eq!(observer.snapshot(), active);
    let explicit = replacement.reset_primary(12000, 2000).unwrap();
    assert!(observer.apply_primary(explicit, true, 12000, 2000).unwrap());
    assert_eq!(observer.snapshot().clock, LaunchClockMsg::idle());
}
#[test]
fn primary_and_observer_process_restart_keep_clock_then_apply_offline_reset() {
    const CHILD: &str = "GS_CLOCK_JOURNAL_PROCESS_PHASE";
    const NAME: &str = "launch_history::tests::primary_and_observer_process_restart_keep_clock_then_apply_offline_reset";
    if let Ok(phase) = std::env::var(CHILD) {
        let root = PathBuf::from(std::env::var_os("GS_CLOCK_JOURNAL_PROCESS_ROOT").unwrap());
        let primary_path = root.join("primary.json");
        let observer_path = root.join("rrls.json");
        match phase.as_str() {
            "primary-start" => {
                let mut state = Store::open(primary_path, false, None, 10000, 0).unwrap();
                state.update_primary(plus(9000), 10000, 0).unwrap();
                state.retain_daq([77, 29000], 1234, 10000, 0).unwrap();
                wait(&state);
            }
            "observer-start" => {
                let primary = Store::open(primary_path, false, None, 11000, 0).unwrap();
                let mut state = Store::open(observer_path, true, None, 11000, 0).unwrap();
                state
                    .apply_primary(primary.snapshot(), true, 11000, 0)
                    .unwrap();
                state
                    .mark_reached("recovery-operator", "auth-hash", "rrls", 12000, 1000)
                    .unwrap();
                wait(&state);
            }
            "primary-reset" => {
                let mut state = Store::open(primary_path, false, None, 13000, 0).unwrap();
                assert_eq!(state.snapshot().clock, plus(9000));
                assert_eq!(state.daq_snapshot(), ([77, 29000], 1234));
                state.reset_primary(13000, 0).unwrap();
                wait(&state);
            }
            "observer-reconnect" => {
                let mut state = Store::open(observer_path, true, None, 14000, 0).unwrap();
                assert_eq!(state.snapshot().clock, plus(9000));
                assert!(!state.synchronized);
                let primary = Store::open(primary_path, false, None, 14000, 0).unwrap();
                assert_eq!(primary.snapshot().clock, LaunchClockMsg::idle());
                state
                    .apply_primary(primary.snapshot(), true, 14000, 0)
                    .unwrap();
                wait(&state);
            }
            "observer-restart" => {
                let before = std::fs::read(&observer_path).unwrap();
                let state = Store::open(observer_path.clone(), true, None, 15000, 0).unwrap();
                assert_eq!(state.snapshot().clock, LaunchClockMsg::idle());
                assert_eq!(state.events().len(), 1);
                assert_eq!(
                    std::fs::read(observer_path).unwrap(),
                    before,
                    "startup itself must not rewrite/rotate the clock"
                );
            }
            _ => panic!("unknown child phase"),
        }
        return;
    }
    let root = path();
    std::fs::create_dir(&root).unwrap();
    for phase in [
        "primary-start",
        "observer-start",
        "primary-reset",
        "observer-reconnect",
        "observer-restart",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads=1"])
            .env(CHILD, phase)
            .env("GS_CLOCK_JOURNAL_PROCESS_ROOT", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{phase}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn explicit_reset_tombstone_survives_restart_and_stale_postflight_packets() {
    let path = path();
    let mut state = Store::open(path.clone(), false, None, 10000, 0).unwrap();
    state.update_primary(plus(9000), 10000, 0).unwrap();
    let reset = state.reset_primary(11000, 1000).unwrap();
    assert_eq!(
        state.update_primary(plus(9000), 12000, 2000).unwrap(),
        reset
    );
    wait(&state);
    drop(state);
    let mut state = Store::open(path.clone(), false, None, 13000, 0).unwrap();
    assert_eq!(state.update_primary(plus(13000), 13000, 0).unwrap(), reset);
    for stale in [
        plus(9000),
        LaunchClockMsg {
            kind: LaunchClockKind::TMinus,
            anchor_timestamp_ms: Some(13000),
            duration_ms: Some(10000),
        },
        plus(13000),
    ] {
        assert_eq!(state.update_primary(stale, 13000, 0).unwrap(), reset);
    }
    state.confirm_new_launch_cycle(14000, 1000).unwrap();
    state
        .update_primary(
            LaunchClockMsg {
                kind: LaunchClockKind::TMinus,
                anchor_timestamp_ms: Some(14000),
                duration_ms: Some(10000),
            },
            14000,
            1000,
        )
        .unwrap();
    let new_launch = state.update_primary(plus(24000), 24000, 11000).unwrap();
    assert_eq!(new_launch.clock, plus(24000));
    assert_eq!(new_launch.session, reset.session);
    wait(&state);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn a_confirmed_new_cycle_invalidates_old_reset_before_t0_is_available() {
    let mut state = Store::ephemeral();
    let old = state.update_primary(plus(9000), 10000, 0).unwrap();
    let reset = state.reset_primary(11000, 1000).unwrap();
    state.confirm_new_launch_cycle(12000, 2000).unwrap();
    let pending = state.snapshot();
    assert_eq!(pending.clock, LaunchClockMsg::idle());
    assert!(pending.revision > reset.revision);
    assert!(state.reset_expected_primary(&old, 12000, 2000).is_err());
    assert!(state.reset_expected_primary(&reset, 12000, 2000).is_err());
    assert_eq!(state.snapshot(), pending);
}
#[tokio::test]
async fn restarted_primary_ignores_repeated_received_launch_ascent_and_landed_until_new_cycle() {
    let path = path();
    let mut first = Store::open(path.clone(), false, None, 10000, 0).unwrap();
    first.update_primary(plus(9000), 10000, 0).unwrap();
    let reset = first.reset_primary(11000, 1000).unwrap();
    wait(&first);
    drop(first);
    let restored = Store::open(path.clone(), false, None, 12000, 0).unwrap();
    let mut state = crate::state::tests::test_app_state().await;
    let inner = Arc::get_mut(&mut state).unwrap();
    inner.launch_clock = Arc::new(Mutex::new(restored.snapshot().clock));
    inner.launch_history = Arc::new(Mutex::new(restored));
    for phase in [
        crate::types::FlightState::Launch,
        crate::types::FlightState::Ascent,
        crate::types::FlightState::Landed,
    ] {
        *state.state.lock().unwrap() = phase;
        state.update_launch_clock_for_state(phase, 12000);
        assert_eq!(state.local_flight_state_snapshot(), phase);
        assert_eq!(state.launch_clock_snapshot(), LaunchClockMsg::idle());
        assert_eq!(state.launch_history.lock().unwrap().snapshot(), reset);
    }
    state.confirm_new_launch_cycle();
    state.update_launch_clock_for_state(crate::types::FlightState::Launch, 13000);
    assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::TMinus);
    let waiter = state.launch_history.lock().unwrap().waiter();
    flush(waiter).await.unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn shutdown_barrier_flushes_all_prior_events_and_rejects_late_commits() {
    let path = path();
    let mut store = Store::open(path.clone(), false, None, 10000, 0).unwrap();
    let minus = LaunchClockMsg {
        kind: LaunchClockKind::TMinus,
        anchor_timestamp_ms: Some(10000),
        duration_ms: Some(10000),
    };
    store.update_primary(minus, 10000, 0).unwrap();
    let event = store
        .mark_reached("operator", "auth", "primary", 11000, 1000)
        .unwrap();
    let current = store.snapshot();
    let (writer, ticket) = store.close_and_waiter().unwrap();
    assert!(store.update_primary(plus(20000), 20000, 10000).is_err());
    assert!(store.reset_primary(20000, 10000).is_err());
    assert!(
        store
            .mark_reached("operator", "auth", "rrls", 20000, 10000)
            .is_err()
    );
    assert!(store.retry_save().is_err());
    assert_eq!(
        store
            .mark_reached("operator", "auth", "primary", 20000, 10000)
            .unwrap(),
        event
    );
    assert_eq!(store.waiter().unwrap().1, ticket);
    writer.wait(ticket).unwrap();
    let reopened = Store::open(path.clone(), false, None, 21000, 0).unwrap();
    assert_eq!(reopened.snapshot(), current);
    assert_eq!(reopened.events(), vec![event]);
    std::fs::remove_file(path).unwrap();
}
