//! Presentation regressions for the external trim lane; no engine tick or IO.
use super::*;

fn external_trim_runtime() -> Runtime {
    let mut rt = Runtime::default();
    rt.engine.status.phase = Phase::Filling;
    rt.engine.status.message = "Final fill mass verification: Settling".into();
    rt.engine.status.valves = [
        Some(false),
        Some(true),
        Some(false),
        Some(false),
        Some(true),
    ];
    let (controller, _) =
        gse_sequence::fill_trim::Controller::start(5_000, 8.0, 300.0, &rt.engine.config).unwrap();
    rt.fill_trim = Some(controller);
    rt.test_now_ms = Some(5_000);
    rt
}

#[test]
fn external_trim_status_uses_fresh_closed_reports_without_ticking_or_mutating_engine() {
    let mut rt = external_trim_runtime();
    for key in KEYS {
        rt.observe_valve(key, false);
    }
    let engine_before = serde_json::to_value(&rt.engine.status).unwrap();
    let trim_before = rt.fill_trim.as_ref().unwrap().progress_observation();
    let response = rt.presentation_status_at(5_000, true, true);
    assert_eq!(response.valves, [Some(false); 5]);
    assert_eq!(response.phase, Phase::Filling);
    assert_eq!(response.message, "Final fill mass verification: Settling");
    assert_eq!(
        serde_json::to_value(&rt.engine.status).unwrap(),
        engine_before
    );
    assert_eq!(
        rt.fill_trim.as_ref().unwrap().progress_observation(),
        trim_before
    );
    assert_eq!(
        serde_json::to_value(response).unwrap()["valves"],
        serde_json::json!([false, false, false, false, false])
    );
}

#[test]
fn external_trim_status_follows_actual_vent_pulse_even_when_desired_and_engine_differ() {
    let mut rt = external_trim_runtime();
    for key in KEYS {
        rt.observe_valve(key, false);
    }
    rt.observe_valve(1, true);
    let before = rt.fill_trim.as_ref().unwrap().progress_observation();
    assert!(
        !before.desired[1],
        "controller target must differ from observed vent"
    );
    assert_eq!(
        rt.presentation_status_at(5_000, false, false).valves,
        [
            Some(false),
            Some(true),
            Some(false),
            Some(false),
            Some(false)
        ]
    );
    rt.test_now_ms = Some(5_100);
    rt.observe_valve(1, false);
    assert_eq!(
        rt.presentation_status_at(5_100, false, false).valves,
        [Some(false); 5]
    );
    assert_eq!(
        rt.fill_trim.as_ref().unwrap().progress_observation(),
        before
    );
    assert_eq!(
        rt.engine.status.valves[4],
        Some(true),
        "response must not rewrite engine cache"
    );
}

#[test]
fn external_trim_status_expires_missing_stale_and_future_reports_instead_of_reusing_engine_values()
{
    let mut rt = external_trim_runtime();
    rt.valves = [
        None,
        Some(ValveState {
            open: false,
            at_ms: 2_999,
        }),
        Some(ValveState {
            open: true,
            at_ms: 5_001,
        }),
        Some(ValveState {
            open: false,
            at_ms: 3_000,
        }),
        Some(ValveState {
            open: true,
            at_ms: 5_000,
        }),
    ];
    assert_eq!(
        rt.presentation_status_at(5_000, true, true).valves,
        [None, None, None, Some(false), Some(true)]
    );
    assert_eq!(
        rt.presentation_status_at(7_002, true, true).valves,
        [None; 5]
    );
    assert_eq!(
        rt.engine.status.valves,
        [
            Some(false),
            Some(true),
            Some(false),
            Some(false),
            Some(true)
        ]
    );
}
