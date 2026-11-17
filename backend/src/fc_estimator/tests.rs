use super::*;

const HZ: u32 = 1000;

/// An FC BOARD_HEALTH at FC uptime `uptime_ms` with the estimator words.
fn health(phase: u32, cycles: u32, start_ms: u32, uptime_ms: u64) -> [u32; 24] {
    let mut words = [0u32; 24];
    words[0] = 1;
    words[1] = 2 | 16 | 128;
    words[2] = uptime_ms as u32;
    words[3] = (uptime_ms >> 32) as u32;
    words[4] = HZ;
    words[13] = (TAG << 24) | (phase << 20) | (cycles & CYCLE_MASK);
    words[14] = 0x4F55 << 16;
    words[15] = start_ms;
    words
}

const FLIGHT: Context = Context { launched: true, expected: true };
const PAD: Context = Context { launched: false, expected: true };
const TEST_FIRE: Context = Context { launched: true, expected: false };

fn warnings(notices: &[Notice]) -> Vec<&str> {
    notices.iter().filter_map(|n| match n { Notice::Warning(w) => Some(w.as_str()), _ => None }).collect()
}
fn infos(notices: &[Notice]) -> Vec<&str> {
    notices.iter().filter_map(|n| match n { Notice::Info(i) => Some(i.as_str()), _ => None }).collect()
}

#[test]
fn decodes_the_fc_words_and_nothing_else() {
    let report = decode(&health(2, 0x1_0005, 40_001, 40_251)).unwrap();
    assert_eq!(report.phase, Phase::Running);
    assert_eq!(report.cycles, 0x1_0005);
    assert_eq!(report.start_tick, Some(40_001));
    assert_eq!(report.since_start(), Some(Duration::from_millis(250)));
    let idle = decode(&health(0, 0, 0, 1_000)).unwrap();
    assert_eq!((idle.phase, idle.start_tick, idle.since_start()), (Phase::Idle, None, None));
    for (phase, expected) in [
        (1, Phase::Initializing), (3, Phase::Converged), (4, Phase::Paused), (5, Phase::Stopped), (6, Phase::Diverged),
    ] {
        assert_eq!(decode(&health(phase, 1, 1, 2)).unwrap().phase, expected);
    }
    // The start tick wraps with the 32-bit uptime word.
    let wrapped = decode(&health(2, 9, u32::MAX - 99, (1u64 << 32) + 100)).unwrap();
    assert_eq!(wrapped.since_start(), Some(Duration::from_millis(200)));
    // Older FC firmware (no tag), another board's generic word 13/15
    // counters (word 1 flags 4/64), an unknown phase, or a short report.
    let mut old = health(2, 5, 1, 2);
    old[13] = 0;
    old[15] = 0;
    assert_eq!(decode(&old), None);
    for flag in [4, 64] {
        let mut generic = health(2, 5, 1, 2);
        generic[1] |= flag;
        assert_eq!(decode(&generic), None, "flag {flag}");
    }
    assert_eq!(decode(&health(7, 5, 1, 2)), None);
    assert_eq!(decode(&health(2, 5, 1, 2)[..23]), None);
}

/// Launch accepted, the FC starts its filter 0.3 s later: one notification
/// with the start relative to Launch, and the status shows it running.
#[test]
fn the_start_after_launch_raises_one_notification_with_t_plus() {
    let tracker = Tracker::default();
    let t0 = Instant::now();
    assert!(tracker.observe(&health(0, 0, 0, 10_000), t0, Some(1_000_000), PAD).is_empty());
    assert_eq!(tracker.status(t0).state, "not_running");
    assert!(tracker.tick(t0 + Duration::from_secs(1), FLIGHT).is_empty());
    let launch = t0 + Duration::from_secs(1);
    // Initializing at once (no start yet), running 250 ms later.
    assert!(tracker.observe(&health(1, 0, 0, 11_050), launch + Duration::from_millis(60), Some(1_001_050), FLIGHT).is_empty());
    let notices = tracker.observe(
        &health(2, 12, 11_051, 11_301),
        launch + Duration::from_millis(550),
        Some(1_001_301),
        FLIGHT,
    );
    assert_eq!(infos(&notices), vec!["FC flight estimator running (Kalman filter started at T+0.3 s)"]);
    assert!(warnings(&notices).is_empty());
    // Its 1 s repeat and later reports do not repeat the notification.
    let notices = tracker.observe(&health(3, 62, 11_051, 12_301), launch + Duration::from_millis(1_550), Some(1_002_301), FLIGHT);
    assert!(notices.is_empty(), "{notices:?}");
    assert!(tracker.tick(launch + Duration::from_secs(5), FLIGHT).is_empty(), "started in time: no warning");
    let status = tracker.status(launch + Duration::from_millis(2_550));
    assert_eq!(status.state, "running");
    assert_eq!(status.label, "FC estimator: running (converged)");
    assert_eq!(status.phase, Some(Phase::Converged));
    assert_eq!(status.rate_hz, Some(50.0));
    assert_eq!(status.running_for_ms, Some(2_250));
    assert_eq!(status.started_t_plus_ms, Some(300));
    assert_eq!(status.started_network_ms, Some(1_001_051), "network stamp minus the time since start");
    assert_eq!(status.report_age_ms, Some(1_000));
    assert!(status.launch_seen && status.expected && status.warning.is_none());
}

#[test]
fn a_start_seen_without_launch_says_how_long_ago() {
    let tracker = Tracker::default();
    let t0 = Instant::now();
    let notices = tracker.observe(&health(2, 600, 20_001, 32_001), t0, None, PAD);
    assert_eq!(infos(&notices), vec!["FC flight estimator running (Kalman filter started 12.0 s ago)"]);
    assert_eq!(tracker.status(t0).started_network_ms, None, "no SEDSnet stamp");
}

/// Launch accepted and no running report within the timeout: one warning,
/// naming what the FC last said; a later start still notifies.
#[test]
fn no_start_within_the_timeout_warns_once() {
    let tracker = Tracker::default();
    let launch = Instant::now();
    assert!(tracker.tick(launch, FLIGHT).is_empty());
    assert!(tracker.tick(launch + START_TIMEOUT - Duration::from_millis(1), FLIGHT).is_empty());
    let notices = tracker.tick(launch + START_TIMEOUT, FLIGHT);
    assert_eq!(
        warnings(&notices),
        vec!["FC flight estimator not running: Launch was accepted 3.0 s ago and no FC report has said its Kalman filter is running."]
    );
    assert!(tracker.tick(launch + Duration::from_secs(4), FLIGHT).is_empty(), "once");
    let status = tracker.status(launch + Duration::from_secs(4));
    assert_eq!(status.state, "not_running");
    assert_eq!(status.warning.as_deref(), Some("FC flight estimator not running 4.0 s after Launch."));
    // It starts late: the notification still comes, and the warning clears.
    let notices = tracker.observe(&health(2, 3, 5_001, 5_101), launch + Duration::from_millis(4_200), None, FLIGHT);
    assert_eq!(infos(&notices), vec!["FC flight estimator running (Kalman filter started at T+4.1 s)"]);
    assert_eq!(tracker.status(launch + Duration::from_millis(4_300)).warning, None);

    // A new flight: back on the pad clears Launch; stuck initializing this time.
    let tracker = Tracker::default();
    tracker.tick(launch, FLIGHT);
    tracker.observe(&health(1, 0, 0, 1_000), launch + Duration::from_millis(100), None, FLIGHT);
    let notices = tracker.tick(launch + Duration::from_secs(3), FLIGHT);
    assert_eq!(
        warnings(&notices),
        vec!["FC flight estimator not running: Launch was accepted 3.0 s ago and it reports its Kalman filter initializing."]
    );
    assert!(tracker.tick(launch + Duration::from_secs(4), PAD).is_empty());
    assert!(!tracker.status(launch + Duration::from_secs(4)).launch_seen);
    assert!(tracker.tick(launch + Duration::from_secs(5), FLIGHT).is_empty(), "a fresh Launch restarts the timeout");
    assert_eq!(warnings(&tracker.tick(launch + Duration::from_secs(8), FLIGHT)).len(), 1);
}

#[test]
fn stopped_or_diverged_after_launch_warns_once_each() {
    let tracker = Tracker::default();
    let t = Instant::now();
    tracker.tick(t, FLIGHT);
    tracker.observe(&health(2, 10, 1_001, 1_201), t, None, FLIGHT);
    let notices = tracker.observe(&health(6, 300, 1_001, 7_201), t + Duration::from_secs(6), None, FLIGHT);
    assert_eq!(warnings(&notices).len(), 1);
    assert!(warnings(&notices)[0].starts_with("FC flight estimator diverged:"), "{notices:?}");
    assert!(tracker.observe(&health(6, 550, 1_001, 12_201), t + Duration::from_secs(11), None, FLIGHT).is_empty(), "no repeat");
    let status = tracker.status(t + Duration::from_secs(11));
    assert_eq!((status.state, status.label.as_str()), ("faulted", "FC estimator: faulted (diverged)"));
    assert!(status.warning.unwrap().starts_with("FC flight estimator diverged:"));
    // Evaluation Abort: stopped, then the sensor restart's Launch runs it again.
    let notices = tracker.observe(&health(5, 560, 1_001, 13_201), t + Duration::from_secs(12), None, FLIGHT);
    assert_eq!(warnings(&notices), vec![
        "FC flight estimator stopped: the FC's Kalman filter is no longer running (Evaluation Abort or a sensor restart)."
    ]);
    let notices = tracker.observe(&health(2, 570, 13_501, 13_701), t + Duration::from_secs(13), None, FLIGHT);
    assert_eq!(infos(&notices), vec!["FC flight estimator running (Kalman filter started at T+12.8 s)"]);
    assert_eq!(tracker.status(t + Duration::from_secs(13)).state, "running");
}

#[test]
fn a_stop_on_the_pad_is_not_a_flight_fault() {
    let tracker = Tracker::default();
    let t = Instant::now();
    assert!(tracker.observe(&health(5, 0, 0, 1_000), t, None, PAD).is_empty());
    // Divergence is always worth a warning when an FC is expected.
    assert_eq!(warnings(&tracker.observe(&health(6, 9, 500, 1_500), t, None, PAD)).len(), 1);
}

#[test]
fn a_cycle_counter_that_stops_is_a_stall() {
    let tracker = Tracker::default();
    let t = Instant::now();
    tracker.tick(t, FLIGHT);
    tracker.observe(&health(3, 0xF_FFF0, 1_001, 10_001), t, None, FLIGHT);
    // Wraps at 20 bits and still advancing.
    assert!(tracker.observe(&health(3, 0x0_0010, 1_001, 10_641), t + Duration::from_millis(640), None, FLIGHT).is_empty());
    assert_eq!(tracker.status(t + Duration::from_millis(640)).rate_hz, Some(50.0));
    let notices = tracker.observe(&health(3, 0x0_0010, 1_001, 15_641), t + Duration::from_millis(5_640), None, FLIGHT);
    assert_eq!(
        warnings(&notices),
        vec!["FC flight estimator stalled: its Kalman filter cycle count has not advanced in 5.0 s."]
    );
    let status = tracker.status(t + Duration::from_millis(5_640));
    assert_eq!((status.state, status.label.as_str(), status.stalled), ("faulted", "FC estimator: faulted (stalled)", true));
    assert_eq!(status.rate_hz, Some(0.0));
    assert!(tracker.observe(&health(3, 0x0_0010, 1_001, 20_641), t + Duration::from_millis(10_640), None, FLIGHT).is_empty(), "once");
    let notices = tracker.observe(&health(3, 0x0_0100, 1_001, 25_641), t + Duration::from_millis(15_640), None, FLIGHT);
    assert_eq!(infos(&notices), vec!["FC flight estimator cycling again."]);
    assert_eq!(tracker.status(t + Duration::from_millis(15_640)).state, "running");
    // A paused filter does not cycle and is not a stall.
    tracker.observe(&health(4, 0x0_0100, 1_001, 30_641), t + Duration::from_secs(20), None, FLIGHT);
    assert!(tracker.observe(&health(4, 0x0_0100, 1_001, 35_641), t + Duration::from_secs(25), None, FLIGHT).is_empty());
    assert_eq!(tracker.status(t + Duration::from_secs(25)).state, "not_running");
}

/// A test fire has no FC: nothing warns, whatever is (or is not) reported.
#[test]
fn a_test_fire_without_an_fc_never_warns() {
    let tracker = Tracker::default();
    let t = Instant::now();
    for s in 0..10 {
        assert!(tracker.tick(t + Duration::from_secs(s), TEST_FIRE).is_empty());
    }
    let status = tracker.status(t + Duration::from_secs(10));
    assert_eq!((status.state, status.warning), ("not_expected", None));
    assert_eq!(status.label, "FC estimator: no FC in this configuration");
    for (phase, cycles, at) in [(2, 5, 1_000), (2, 5, 6_000), (6, 5, 7_000), (5, 5, 8_000)] {
        let notices = tracker.observe(&health(phase, cycles, 500, at), t + Duration::from_millis(at), None, TEST_FIRE);
        assert!(warnings(&notices).is_empty(), "{notices:?}");
    }
}

#[test]
fn publishes_changes_at_once_and_ages_once_a_second() {
    let tracker = Tracker::default();
    let mut rx = tracker.subscribe();
    let t = Instant::now();
    tracker.tick(t, FLIGHT);
    assert_eq!(tracker.publish(t).map(|s| s.state), Some("not_running"), "news is recorded");
    assert!(rx.has_changed().unwrap());
    rx.borrow_and_update();
    assert_eq!(tracker.publish(t + Duration::from_millis(250)), None);
    assert!(!rx.has_changed().unwrap(), "nothing new within a second");
    tracker.observe(&health(2, 3, 1_001, 1_101), t + Duration::from_millis(300), None, FLIGHT);
    let news = tracker.publish(t + Duration::from_millis(300)).unwrap();
    assert_eq!(news.state, "running");
    assert_eq!(rx.borrow_and_update().state, "running");
    assert_eq!(tracker.publish(t + Duration::from_millis(1_400)), None, "an age alone is not recorded");
    assert!(rx.has_changed().unwrap(), "but the time since start is republished");
    assert_eq!(rx.borrow_and_update().running_for_ms, Some(1_200));
    let json = serde_json::to_value(tracker.snapshot()).unwrap();
    assert_eq!(json["state"], "running");
    assert_eq!(json["phase"], "running");
    assert_eq!(json["label"], "FC estimator: running");
}
