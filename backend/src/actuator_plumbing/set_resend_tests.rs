//! A drive SET lost on the Ordered AB_FILL_DRIVE_SETTINGS stream is sent
//! again until the actuator's readback confirms it (set_resend). Real SDK
//! routers and the fake actuator of `http_tests`; the station's 1 s
//! background pass is driven by hand with explicit times.
use super::http_tests::{Fixture, SERIAL};
use super::*;
use std::time::Duration;

const AUX: u8 = 2;
/// set_resend's CONFIRM_WINDOW and CHECK_WINDOW.
const CONFIRM_WINDOW: Duration = Duration::from_secs(6);
const CHECK_WINDOW: Duration = Duration::from_secs(6);

async fn startup(f: &mut Fixture) {
    f.state.set_local_flight_state(FlightState::Startup);
    f.prime().await;
}
async fn post(f: &Fixture, mode: &str, release: bool) -> Response {
    f.http("POST", "/api/actuator/plumbing/settings", Some("operator"), f.body(mode, release))
        .await
}
fn apply(f: &Fixture) -> Value {
    view(&f.state, true)["apply"].clone()
}
fn revision(f: &Fixture) -> u64 {
    f.state.plumbing_drive.inner.lock().unwrap().desired.revision
}
/// Ages every recorded drive request, as if `ms` passed.
fn backdate_requests(f: &Fixture, ms: u64) {
    let mut inner = f.state.plumbing_drive.inner.lock().unwrap();
    for context in inner.requests.iter_mut() {
        context.sent.wall -= Duration::from_millis(ms);
        if let Some(model) = context.sent.model.as_mut() {
            *model = model.saturating_sub(ms);
        }
    }
}
/// The nonce of the latest drive query the station queued.
fn latest_query(f: &Fixture) -> Option<u64> {
    let inner = f.state.plumbing_drive.inner.lock().unwrap();
    inner.requests.iter().rev().find(|context| context.request.mode.is_none()).map(|context| context.request.nonce)
}
/// The station holds no pending drive SET (the change is over).
fn no_pending_set(f: &Fixture) -> bool {
    f.state.plumbing_drive.inner.lock().unwrap().pending_setting.is_none()
}
/// The station's background second: board readback refresh, then the board
/// config resync worker's tick, which follows the drive change first.
fn station_second(f: &mut Fixture, now: Instant) {
    let _ = crate::board_recovery::request_board_query(&f.state, Board::ActuatorBoard);
    crate::board_config_sync::tick(&f.state, now);
    f.pump();
}
/// Runs station seconds until the change is no longer applying; returns
/// the second it settled at.
fn run_until_settled(f: &mut Fixture, start: Instant, seconds: u64) -> Option<u64> {
    for second in 1..=seconds {
        station_second(f, start + Duration::from_secs(second));
        if apply(f)["state"] != "applying" {
            return Some(second);
        }
    }
    None
}
/// With the link down, the drive stream's head runs out of retries; the
/// production router worker pass reports the retired stream.
fn exhaust_drive_stream(f: &mut Fixture) {
    let request = crate::telemetry_schema::data_type(REQUEST_TYPE);
    for _ in 0..400 {
        f.advance_ms(250);
        f.pump();
        if !f.host.export_reliable_pending_stats().iter().any(|row| row.data_type == request) {
            return;
        }
    }
    panic!("the Ordered drive stream never gave up");
}

/// Renode final3clock1: a drive query the actuator never receipted held the
/// Ordered stream until it gave up, and the operator's SET queued behind it
/// was dropped without reaching CAN. The station checks with one query and
/// sends the same change again (same drive, same saved revision); only the
/// actuator's readback of it marks it applied.
#[tokio::test]
async fn a_set_dropped_behind_an_exhausted_query_is_resent_until_readback_confirms_it() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.worker = Some(Default::default());
    startup(&mut f).await;
    f.link_down = true;
    backdate_requests(&f, 1_500);
    request_drive_query(&f.state).unwrap();
    assert_eq!(post(&f, "auxiliary", true).await.status(), StatusCode::ACCEPTED);
    assert_eq!(revision(&f), 1);
    exhaust_drive_stream(&mut f);
    assert_eq!(f.drive_requests().1, 0, "the SET never reached the actuator");
    assert_eq!(view(&f.state, true)["pending"], true);
    f.link_down = false;

    let settled = run_until_settled(&mut f, Instant::now(), 20);
    let apply = apply(&f);
    assert_eq!(apply["state"], "applied", "{apply}");
    assert!(
        settled.is_some_and(|second| second < CONFIRM_WINDOW.as_secs()),
        "the retired stream, not the confirm timeout, started the re-send: {settled:?}"
    );
    assert_eq!(apply["retry"], 1, "{apply}");
    assert_eq!(apply["origin"], "operator");
    assert_eq!((f.mode, f.sets), (AUX, 1), "the actuator got the change once");
    assert_eq!(revision(&f), 1, "the same operator intent and revision");
    let snapshot = view(&f.state, true);
    assert_eq!(snapshot["pending"], false, "{snapshot}");
    assert!(snapshot["message"].as_str().unwrap().contains("applied"), "{snapshot}");
    motion_ready(&f.state).unwrap();
    crate::board_config_sync::tick(&f.state, Instant::now());
    let item = crate::board_config_sync::item_for_test(&f.state, crate::board_config_sync::Item::AbFillDrive);
    assert_eq!(item.phase, crate::board_config_sync::Phase::Synced, "{}", item.message);
}

/// A SET the actuator received but never answered: no retired stream, so
/// the confirm window starts the check. The re-send applies; the first SET
/// handled late afterwards is refused as stale by the actuator, and that
/// reply neither undoes "applied" nor stands as the drive readback.
#[tokio::test]
async fn an_unanswered_set_is_checked_after_the_confirm_window_and_a_late_copy_is_harmless() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.worker = Some(Default::default());
    startup(&mut f).await;
    f.hold_sets = true;
    assert_eq!(post(&f, "auxiliary", true).await.status(), StatusCode::ACCEPTED);
    f.pump();
    assert_eq!(f.sets, 0);
    let start = Instant::now();
    for second in 1..CONFIRM_WINDOW.as_secs() {
        station_second(&mut f, start + Duration::from_secs(second));
        assert_eq!(apply(&f)["state"], "applying");
        assert_eq!(apply(&f)["retry"], 0, "no re-send inside the confirm window");
    }
    f.hold_sets = false;
    let settled = run_until_settled(&mut f, start + CONFIRM_WINDOW - Duration::from_secs(1), 10);
    assert!(settled.is_some(), "{}", apply(&f));
    assert_eq!(apply(&f)["state"], "applied", "{}", apply(&f));
    assert_eq!(apply(&f)["retry"], 1);
    assert_eq!((f.mode, f.sets), (AUX, 1));

    f.release_held_sets();
    assert_eq!(f.drive_requests().1, 2, "the late first SET reached the actuator");
    assert_eq!(f.sets, 1, "the actuator refused it as stale");
    assert_eq!(apply(&f)["state"], "applied");
    let snapshot = view(&f.state, true);
    assert_eq!(snapshot["board"]["result"], "ok", "{snapshot}");
    assert_eq!(snapshot["pending"], false);
    assert_eq!(revision(&f), 1);
    motion_ready(&f.state).unwrap();
}

/// The station's own gates are run again before anything is re-sent: a
/// change the station can no longer make stops and says why.
#[tokio::test]
async fn a_gate_closing_stops_the_resend_and_reports_why() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.worker = Some(Default::default());
    startup(&mut f).await;
    f.link_down = true;
    backdate_requests(&f, 1_500);
    request_drive_query(&f.state).unwrap();
    assert_eq!(post(&f, "auxiliary", true).await.status(), StatusCode::ACCEPTED);
    exhaust_drive_stream(&mut f);
    f.link_down = false;
    f.state.set_local_flight_state(FlightState::Armed);
    let (queries_before, _) = f.drive_requests();
    let settled = run_until_settled(&mut f, Instant::now(), 3);
    assert_eq!(settled, Some(1));
    let apply = apply(&f);
    assert_eq!(apply["state"], "failed", "{apply}");
    assert!(apply["reason"].as_str().unwrap().contains("Startup or Idle"), "{apply}");
    let message = view(&f.state, true)["message"].as_str().unwrap().to_owned();
    assert!(message.starts_with("Drive change not applied: drive settings change only in Startup or Idle"), "{message}");
    assert_eq!(f.drive_requests(), (queries_before, 0), "neither a check nor a re-send was sent");
    assert!(no_pending_set(&f));
    for second in 2..10 {
        station_second(&mut f, Instant::now() + Duration::from_secs(second));
    }
    assert_eq!(f.drive_requests().1, 0, "nothing is sent while Armed");
}

/// The actuator refusing the change outright is its outcome: no re-send.
#[tokio::test]
async fn an_actuator_refusal_fails_the_change_without_a_resend() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.worker = Some(Default::default());
    startup(&mut f).await;
    f.result = 8;
    assert_eq!(post(&f, "auxiliary", true).await.status(), StatusCode::ACCEPTED);
    f.pump();
    let apply = apply(&f);
    assert_eq!(apply["state"], "failed", "{apply}");
    assert!(apply["reason"].as_str().unwrap().contains("refused"), "{apply}");
    run_until_settled(&mut f, Instant::now(), 8);
    assert_eq!(f.drive_requests().1, 1);
}

/// The operator asking for the change already being applied keeps the one
/// tracked change: no new revision and no second SET.
#[tokio::test]
async fn asking_again_for_the_change_being_applied_is_idempotent() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.worker = Some(Default::default());
    startup(&mut f).await;
    f.link_down = true;
    assert_eq!(post(&f, "auxiliary", true).await.status(), StatusCode::ACCEPTED);
    let sets = |f: &Fixture| {
        let inner = f.state.plumbing_drive.inner.lock().unwrap();
        inner.requests.iter().filter(|context| context.request.mode.is_some()).count()
    };
    assert_eq!((revision(&f), sets(&f)), (1, 1));
    let again = post(&f, "auxiliary", true).await;
    assert_eq!(again.status(), StatusCode::OK);
    let message = view(&f.state, true)["message"].as_str().unwrap().to_owned();
    assert!(message.contains("No new setting was saved"), "{message}");
    assert_eq!((revision(&f), sets(&f)), (1, 1));
    assert_eq!(apply(&f)["state"], "applying");
    f.link_down = false;
    for _ in 0..10 {
        f.advance_ms(200);
        f.pump();
    }
    assert_eq!(apply(&f)["state"], "applied", "{}", apply(&f));
    assert_eq!((f.mode, f.sets), (AUX, 1));
}

/// While a SET awaits its reply the background drive query pauses; a check
/// asks one query at a time, and a change that never gets a readback gives
/// up after a bounded number of checks and says so.
#[tokio::test]
async fn queries_do_not_pile_up_behind_a_set_and_an_unanswered_change_gives_up() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.worker = Some(Default::default());
    startup(&mut f).await;
    f.link_down = true;
    assert_eq!(post(&f, "auxiliary", true).await.status(), StatusCode::ACCEPTED);
    let mut refresh = crate::board_abort::ActuatorReadbackRefresh::default();
    let queries = latest_query(&f);
    let motion_queries = f.state.plumbing_drive.inner.lock().unwrap().motion_queries.len();
    for _ in 0..6 {
        // A second passes: the background refresh may ask again.
        backdate_requests(&f, 1_100);
        f.state.network_health.lock().unwrap().observe("AB", Instant::now(), 100);
        let _ = request_drive_query(&f.state);
        let _ = refresh.poll(&f.state);
        f.pump();
    }
    assert_eq!(latest_query(&f), queries, "no drive query queued behind the SET in flight");
    assert_eq!(
        f.state.plumbing_drive.inner.lock().unwrap().motion_queries.len(),
        motion_queries,
        "no motion query on the same stream either"
    );

    let start = Instant::now() + CONFIRM_WINDOW;
    let mut asked = Vec::new();
    for second in 0..30 {
        let before = latest_query(&f);
        // No router pass: the check queries stay unanswered and unretired.
        crate::board_config_sync::tick(&f.state, start + Duration::from_secs(second));
        if latest_query(&f) != before {
            asked.push(second);
        }

        if apply(&f)["state"] != "applying" {
            break;
        }
    }
    assert_eq!(asked, [0, CHECK_WINDOW.as_secs()], "one check query per check window");
    let apply = apply(&f);
    assert_eq!(apply["state"], "failed", "{apply}");
    assert!(apply["reason"].as_str().unwrap().contains("no actuator drive readback"), "{apply}");
    assert_eq!(apply["retry"], 0);
    assert!(no_pending_set(&f));
    // The give-up was the drive's quick attempt: the config resync waits for
    // its once-a-minute retry instead of pushing again at once.
    for second in 1..30 {
        let before = latest_query(&f);
        crate::board_config_sync::tick(&f.state, start + CHECK_WINDOW * 2 + Duration::from_secs(second));
        assert_eq!(latest_query(&f), before);
    }
    assert_eq!(view(&f.state, true)["apply"]["origin"], "operator", "no resync push replaced it");
    let item = crate::board_config_sync::item_for_test(&f.state, crate::board_config_sync::Item::AbFillDrive);
    assert_eq!(item.phase, crate::board_config_sync::Phase::OutOfSync, "{}", item.message);
}

/// The board config resync pushes through the same tracked change: it
/// pushes once and waits while the change re-sends itself, then sees it
/// verified by the readback.
#[tokio::test]
async fn a_dropped_config_resync_is_resent_by_the_same_change_not_pushed_again() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.worker = Some(Default::default());
    set_desired_for_test(&f.state, AUX, 1, 1_000);
    f.prime().await;
    assert_eq!((f.mode, f.drive_id), (0, 0), "factory default actuator");
    f.link_down = true;
    crate::board_config_sync::tick(&f.state, Instant::now());
    assert_eq!(apply(&f)["origin"], "resync");
    exhaust_drive_stream(&mut f);
    f.link_down = false;
    assert_eq!(f.drive_requests().1, 0);
    let settled = run_until_settled(&mut f, Instant::now(), 20);
    assert!(settled.is_some(), "{}", apply(&f));
    let apply = apply(&f);
    assert_eq!(apply["state"], "applied", "{apply}");
    assert_eq!(apply["origin"], "resync");
    assert_eq!(apply["retry"], 1);
    assert_eq!(apply["revision"], 1);
    assert_eq!((f.mode, f.sets), (AUX, 1));
    assert_eq!(f.drive_requests().1, 1, "one re-send, no second resync push");
    crate::board_config_sync::tick(&f.state, Instant::now());
    let item = crate::board_config_sync::item_for_test(&f.state, crate::board_config_sync::Item::AbFillDrive);
    assert_eq!(item.phase, crate::board_config_sync::Phase::Synced, "{}", item.message);
    assert!(item.message.contains("verified"), "{}", item.message);
}
