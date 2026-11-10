//! `/api/gse/status` always carries the sequence-progress envelope. The UI
//! decodes it before using any other status field and treats a reply without
//! one as "GSE status unavailable". No IO, engine tick or hardware.
use super::*;

/// The fields the UI's envelope decoder requires (`sequence_progress_model::Envelope`).
fn assert_envelope(status: &serde_json::Value, context: &str) {
    let envelope = &status["sequence_progress"];
    assert!(envelope.is_object(), "{context}: no sequence_progress envelope: {status}");
    assert_eq!(envelope["version"], 1, "{context}: {envelope}");
    for key in ["backend_instance", "reset_epoch"] {
        assert!(
            envelope[key].as_str().is_some_and(|value| !value.is_empty()),
            "{context}: envelope {key} missing: {envelope}"
        );
    }
    assert!(envelope["cancel_api_version"].as_u64().is_some(), "{context}: {envelope}");
    for key in ["active", "terminal"] {
        assert!(envelope[key].is_array(), "{context}: envelope {key} missing: {envelope}");
    }
    assert!(
        envelope.as_object().unwrap().contains_key("reporting_error"),
        "{context}: envelope reporting_error missing: {envelope}"
    );
}

/// A fresh station in each mode (startup, before any sequence has run) and
/// the same station after the mode in force changes underneath it.
#[tokio::test]
async fn primary_status_carries_the_envelope_in_every_mode_from_startup() {
    use crate::station_mode::Mode;
    let state = crate::state::tests::test_app_state().await;
    assert!(!crate::rrls::is_rrls());
    for mode in [Mode::Normal, Mode::Hitl, Mode::TestFire] {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        let fresh = crate::state::tests::test_app_state().await;
        assert_envelope(&status_value(&fresh).expect("primary status"), &format!("startup {mode:?}"));
        assert_envelope(&status_value(&state).expect("primary status"), &format!("switched to {mode:?}"));
    }
}

fn rrls_child(test_name: &str) -> bool {
    if std::env::var("GS_STATUS_ENVELOPE_CHILD").as_deref() == Ok(test_name) {
        assert!(crate::rrls::is_rrls());
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env("GS_STATUS_ENVELOPE_CHILD", test_name)
        .env("GS_RRLS_ROLE", "rrls")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "child failed: {stdout}{stderr}");
    assert_eq!(
        stdout.matches("test result: ok. 1 passed; 0 failed").count(),
        1,
        "the RRLS child must run exactly this test: {stdout}{stderr}"
    );
    true
}

/// An RRLS mirrors the primary's GSE status; that reply needs the envelope
/// too. Without a fresh primary observation the reply stays a 503.
#[tokio::test]
async fn rrls_mirrored_status_carries_the_envelope() {
    if rrls_child("gse::status_envelope_tests::rrls_mirrored_status_carries_the_envelope") {
        return;
    }
    let state = crate::state::tests::test_app_state().await;
    assert!(status_value(&state).is_none(), "no primary observation yet: unavailable, not invented");
    assert!(crate::primary_mirror::accept_control_observation(
        &state,
        &crate::primary_mirror::simulated_idle_observation(),
        0,
    ));
    let status = status_value(&state).expect("fresh primary observation");
    assert!(status["primary_observation"]["fresh"].as_bool().unwrap_or(false), "{status}");
    assert_envelope(&status, "RRLS mirror");
}
