use super::*;

#[test]
fn same_tick_opposite_capture_retains_anchor_and_both_replays_are_rejected() {
    for source in [0, 1234, 1_798_000_000_000] {
        let mut history = ValveCaptureHistory::default();
        let receipt = if source >= 1_577_836_800_000 {
            source
        } else {
            1_798_000_000_000
        };
        let first = history
            .admit(source, receipt, 5000, true, 11, None)
            .unwrap();
        let second = history
            .admit(source, receipt + 100, 5100, false, 12, Some(first))
            .unwrap();
        assert_eq!(
            second, first,
            "opposite capture must not restart settle/freshness"
        );
        let accepted = history;
        assert_eq!(
            history.admit(source, receipt + 200, 5200, true, 11, Some(second)),
            None
        );
        assert_eq!(
            history.admit(source, receipt + 200, 5200, true, 13, Some(second)),
            None,
            "a new nonce cannot resurrect an already-seen value from this tick"
        );
        assert_eq!(
            history.admit(source, receipt + 200, 5200, false, 14, Some(second)),
            None
        );
        assert_eq!(history, accepted);
    }
}

#[test]
fn utc_age_future_backwards_and_underflow_guards_survive_opposite_exception() {
    let utc = 1_798_000_000_000;
    let mut history = ValveCaptureHistory::default();
    let first = history.admit(utc - 1500, utc, 5000, true, 1, None).unwrap();
    assert_eq!(first, 3500);
    assert_eq!(
        history.admit(utc - 1500, utc + 100, 5100, false, 2, Some(first)),
        Some(first)
    );
    let accepted = history;
    for (source, receipt, now, id) in [
        (utc - 3000, utc, 5000, 3),
        (utc + 101, utc, 5000, 4),
        (utc - 1600, utc, 5000, 5),
        (utc - 1000, utc, 100, 6),
    ] {
        assert_eq!(
            history.admit(source, receipt, now, true, id, Some(first)),
            None
        );
        assert_eq!(history, accepted);
    }
    let mut future = ValveCaptureHistory::default();
    assert_eq!(
        future.admit(utc + 100, utc, 5000, true, 0, None),
        Some(5000)
    );
    let mut expired = ValveCaptureHistory::default();
    let at = expired.admit(utc - 1500, utc, 5000, true, 1, None).unwrap();
    assert_eq!(
        expired.admit(utc - 1500, utc + 501, 5501, false, 2, Some(at)),
        None
    );
}

#[test]
fn rebooted_boot_clock_is_compatible_but_stale_or_regressed_tick_does_not_rearm() {
    let mut history = ValveCaptureHistory::default();
    let mut previous_at = None;
    for (index, (stamp, value)) in [(100, true), (200, false), (0, true), (1, false)]
        .into_iter()
        .enumerate()
    {
        previous_at = history.admit(
            stamp,
            1_798_000_000_000,
            5000 + index as u64,
            value,
            index as u64,
            previous_at,
        );
        assert!(previous_at.is_some());
    }
    let mut history = ValveCaptureHistory::default();
    assert_eq!(
        history.admit(1234, 1_798_000_000_000, 10, true, 0, None),
        Some(10)
    );
    let first = history;
    assert_eq!(
        history.admit(1234, 1_798_000_000_000, 9, false, 1, Some(10)),
        None
    );
    assert_eq!(
        history.admit(1234, 1_798_000_000_000, 2011, false, 1, Some(10)),
        None
    );
    assert_eq!(history, first);
    assert_eq!(
        history.admit(-1, 1_798_000_000_000, 20, false, 1, Some(10)),
        None
    );
}

#[test]
fn history_is_bounded_and_only_populated_id_slots_participate() {
    assert!(std::mem::size_of::<ValveCaptureHistory>() <= 32);
    assert!(
        5 * (std::mem::size_of::<ValveCaptureHistory>() - std::mem::size_of::<Option<i64>>()) <= 80
    );
    let mut history = ValveCaptureHistory::default();
    assert_eq!(
        history.admit(0, 0, 100, true, 0, None),
        Some(100),
        "ID zero is valid"
    );
    assert_eq!(
        history.admit(0, 0, 101, false, 0, Some(100)),
        None,
        "an actual populated ID cannot be reused"
    );
    assert_eq!(history.admit(0, 0, 101, false, 1, Some(100)), Some(100));
    assert_eq!(history.packet_ids, [1, 0]);
}

#[tokio::test]
async fn source_age_is_backdated_in_display_and_does_not_acknowledge_replayed_target() {
    let state = crate::state::tests::test_app_state().await;
    state.gse.lock().unwrap().clock = Instant::now() - std::time::Duration::from_secs(5);
    let utc = 1_798_000_000_000;
    let barrier = Instant::now() - std::time::Duration::from_millis(1000);
    assert!(state.set_umbilical_valve_state_source(0, true, utc - 1500, utc, 1));
    let at = state.gse.lock().unwrap().valves[0].unwrap().at_ms;
    assert!(
        !state.umbilical_report_after(0, barrier),
        "receipt cannot turn old source age into new capture provenance"
    );
    assert!(state.set_umbilical_valve_state_source(0, false, utc - 1500, utc + 100, 2));
    assert!(state.gse.lock().unwrap().valves[0].unwrap().at_ms <= at);
    assert!(!state.umbilical_report_after(0, barrier));
    assert_eq!(state.get_umbilical_valve_state(0), Some(false));
    state.set_pending_umbilical_valve_state(0, true);
    for id in [1, 3] {
        assert!(!state.set_umbilical_valve_state_source(0, true, utc - 1500, utc + 200, id));
        assert_eq!(state.get_pending_umbilical_valve_state(0), Some(true));
        assert_eq!(state.get_umbilical_valve_state(0), None);
        assert!(!state.gse.lock().unwrap().valves[0].unwrap().open);
    }
}

#[test]
fn delayed_or_regressed_processing_clock_never_commits_capture_or_history() {
    for (model_at, processing_now) in [(1000, 3001), (1000, 999)] {
        let mut runtime = Runtime::default();
        let before = runtime.last_valve_captures;
        let receipt = ValveReceipt {
            network_ms: 1_798_000_000_000,
            monotonic_at: Instant::now(),
            simulation_ms: Some(model_at),
        };
        assert_eq!(
            runtime.observe_valve_source_at(0, true, 1234, 1, receipt, processing_now),
            None
        );
        assert_eq!(runtime.last_valve_captures, before);
        assert!(runtime.valves[0].is_none());
    }
    let mut runtime = Runtime::default();
    runtime.clock = Instant::now() - std::time::Duration::from_secs(5);
    let before = runtime.last_valve_captures;
    let receipt = ValveReceipt {
        network_ms: 1_798_000_000_000,
        monotonic_at: runtime.clock + std::time::Duration::from_secs(1),
        simulation_ms: None,
    };
    assert_eq!(
        runtime.observe_valve_source(0, true, 1234, 1, receipt),
        None
    );
    assert_eq!(runtime.last_valve_captures, before);
    assert!(runtime.valves[0].is_none());
}

#[test]
fn model_receipt_processing_delay_keeps_original_capture_anchor() {
    let mut runtime = Runtime::default();
    let make = |at| ValveReceipt {
        network_ms: 1_798_000_000_000,
        monotonic_at: Instant::now(),
        simulation_ms: Some(at),
    };
    assert_eq!(
        runtime.observe_valve_source_at(0, true, 1234, 1, make(1000), 1100),
        Some(0)
    );
    assert_eq!(runtime.valves[0].unwrap().at_ms, 1000);
    assert_eq!(
        runtime.observe_valve_source_at(0, false, 1234, 2, make(1500), 2000),
        Some(500)
    );
    assert_eq!(runtime.valves[0].unwrap().at_ms, 1000);
    let before = runtime.last_valve_captures;
    assert_eq!(
        runtime.observe_valve_source_at(0, true, 1234, 3, make(2000), 2000),
        None
    );
    assert_eq!(runtime.last_valve_captures, before);
    assert!(!runtime.valves[0].unwrap().open);
}
