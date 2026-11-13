//! Actual HTTP -> recording actor -> accepted zero -> real blocked auth SQLite.
//! Clock-file cases are separate processes; no operator files or hardware used.
use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use std::sync::Mutex;
use tower::ServiceExt;

type AcceptedAuthSignal = (usize, tokio::sync::oneshot::Sender<()>);
static AUTH_STARTED: Mutex<Option<AcceptedAuthSignal>> = Mutex::new(None);
pub(crate) fn accepted_auth_started(state: &AppState) {
    let mut slot = AUTH_STARTED.lock().unwrap();
    let sender = if slot
        .as_ref()
        .is_some_and(|(owner, _)| *owner == state as *const AppState as usize)
    {
        slot.take().map(|(_, sender)| sender)
    } else {
        None
    };
    drop(slot);
    if let Some(sender) = sender {
        let _ = sender.send(());
    }
}
fn write_clock(path: &std::path::Path, value: &str) {
    let next = path.with_extension("next");
    std::fs::write(&next, value).unwrap();
    std::fs::rename(next, path).unwrap();
}
async fn actual_auth_case(model: Option<(u64, Option<&'static str>)>, expect_saved: bool) {
    let f =
        crate::fill_recording::tests::Fixture::new_phase(false, gse_sequence::Phase::Passed).await;
    crate::fill_recording::tests::enable_owned_zero(&f);
    let app = crate::web::router(f.state.clone(), "deadline-fixture-password".into());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/command")
                .header("content-type", "application/json")
                .header("authorization", "Bearer fill-token")
                .body(Body::from("\"StartFill\""))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(f.state.auto_zero.lock().unwrap().pending());
    let capture_wall = std::time::Instant::now();
    let original = f
        .state
        .fill_recording
        .continuation
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let original_ticket = original.origin.ticket().original_ownership();
    let receipt_instance = original.receipt.instance;
    let before = f.state.loadcell_calibration.lock().unwrap().clone();
    let path = loadcell::calibration_file_for_test();
    let before_file = std::fs::read(&path).ok();
    let (started, waiting) = tokio::sync::oneshot::channel();
    *AUTH_STARTED.lock().unwrap() = Some((Arc::as_ptr(&f.state) as usize, started));
    // Exhaust the ACTUAL session database pool. The accepted continuation's
    // real authorize_token SQL must wait; no fake permission/result is supplied.
    let connections = f.state.auth_db.options().get_max_connections();
    assert!((1..=16).contains(&connections));
    let mut held = Vec::with_capacity(connections as usize);
    for _ in 0..connections {
        held.push(f.state.auth_db.acquire().await.unwrap());
    }
    let clock =
        model.map(|_| std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap()));
    let delay = if model.is_some() {
        Duration::from_millis(1)
    } else {
        Duration::from_millis(26)
    };
    crate::fill_recording::tests::feed_zero(&f, delay, |id| {
        if let Some(clock) = clock.as_ref() {
            write_clock(clock, &(100000 + id as u64 * 26).to_string());
        }
    })
    .await;
    tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(f.state.auth_db.num_idle(), 0);
    assert!(f.state.auto_zero.lock().unwrap().pending());
    if let Some((release_ms, replace)) = model {
        let clock = clock.as_ref().unwrap();
        write_clock(clock, &release_ms.to_string());
        if let Some(replace) = replace {
            let clock = clock.clone();
            loadcell::set_guarded_replace_test_hook(path.clone(), move || {
                if replace == "remove" {
                    std::fs::remove_file(clock).unwrap();
                } else {
                    write_clock(&clock, replace);
                }
            });
        }
    } else {
        let deadline = Duration::from_millis(7050);
        if let Some(left) = deadline.checked_sub(capture_wall.elapsed()) {
            tokio::time::sleep(left).await;
        }
    }
    // Original request/ticket and the one recording instance remain unchanged
    // across the REAL auth wait. Only the actual existing pool is released.
    assert_eq!(
        original.origin.ticket().original_ownership(),
        original_ticket
    );
    assert_eq!(f.state.fill_recording.ready_instance(), receipt_instance);
    drop(held);
    crate::fill_recording::tests::settle_zero(&f, expect_saved).await;
    assert!(!f.state.auto_zero.lock().unwrap().pending());
    let after = f.state.loadcell_calibration.lock().unwrap().clone();
    if expect_saved {
        assert_eq!(after.ch1_zero_raw, Some(0.2));
        assert!(
            !f.sent.lock().unwrap().is_empty(),
            "owned pre-deadline completion must still start Fill"
        );
    } else {
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert_eq!(std::fs::read(&path).ok(), before_file);
        assert!(
            f.sent.lock().unwrap().is_empty(),
            "expired/unknown original capture must not actuate"
        );
        if let Some(retained) = f.state.fill_recording.continuation.lock().unwrap().as_ref() {
            assert!(
                Arc::ptr_eq(retained, &original),
                "no new continuation may replace the expired one"
            );
        }
        let token = original
            .zero_generation
            .load(std::sync::atomic::Ordering::Acquire);
        let falsely_completed = f
            .state
            .auto_zero
            .lock()
            .unwrap()
            .completed_fill_capture(token);
        // Exercise the original delayed internal callback too, rather than
        // relying on the failed immediate branch simply not calling it.
        crate::gse::start_fill_after_automatic_zero(&f.state, original.origin.generation());
        f.pump();
        assert!(
            !falsely_completed && f.sent.lock().unwrap().is_empty(),
            "failed capture must invalidate its original completion token and every late output; completed={falsely_completed}, sent={:?}",
            f.sent.lock().unwrap()
        );
    }
    assert_eq!(
        f.files().len(),
        1,
        "no replacement recording request or instance"
    );
    assert!(!f.state.abort_indicator_latched());
    assert_ne!(
        f.state.local_flight_state_snapshot(),
        crate::types::FlightState::Aborted
    );
    *AUTH_STARTED.lock().unwrap() = None;
}

#[cfg(not(feature = "firmware_sim"))]
#[tokio::test]
async fn actual_auth_sql_wait_past_original_wall_capture_deadline_never_commits() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_auth_sql_wait_past_original_wall_capture_deadline_never_commits",
    ) {
        return;
    }
    actual_auth_case(None, false).await;
}

macro_rules! model_case {
    ($name:ident, $release:expr, $replace:expr, $saved:expr) => {
        #[tokio::test]
        async fn $name() {
            if !cfg!(feature = "firmware_sim") {
                crate::mode_suite!(Hitl);
            }
            if crate::operator_queue::isolated_case_with_capture_clock(
                module_path!(),
                stringify!($name),
            ) {
                return;
            }
            actual_auth_case(Some(($release, $replace)), $saved).await;
        }
    };
}
model_case!(
    actual_auth_release_just_before_original_seven_seconds_still_commits,
    106999,
    None,
    true
);
model_case!(
    actual_auth_release_at_original_seven_seconds_is_refused,
    107000,
    None,
    false
);
model_case!(
    actual_auth_release_after_original_seven_seconds_is_refused,
    107001,
    None,
    false
);
model_case!(
    actual_original_clock_reaches_deadline_during_staging_cannot_replace_calibration,
    106998,
    Some("107000"),
    false
);
model_case!(
    actual_truncated_model_clock_cannot_publish_tare_using_wall_fallback,
    106998,
    Some(""),
    false
);
model_case!(
    actual_backward_model_clock_cannot_renew_original_capture,
    106998,
    Some("99999"),
    false
);
model_case!(
    actual_missing_model_clock_cannot_publish_tare_using_wall_fallback,
    106998,
    Some("remove"),
    false
);
