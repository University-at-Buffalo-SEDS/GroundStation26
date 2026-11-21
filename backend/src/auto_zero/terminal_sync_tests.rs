//! Actual HTTP/recording/zero task and public SDK setter, disposable peers only.
use super::*;
use axum::{body::{Body, to_bytes}, http::{Request, StatusCode}};
use std::sync::Mutex;
use tokio::sync::oneshot;
use tower::ServiceExt;

struct Hook {
    owner: usize,
    arrived: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    done: Option<oneshot::Sender<()>>,
}
static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
pub(super) struct TaskTerminal(Option<oneshot::Sender<()>>);
impl Drop for TaskTerminal {
    fn drop(&mut self) { if let Some(done) = self.0.take() { let _ = done.send(()); } }
}
pub(super) fn task_terminal(state: &AppState) -> TaskTerminal {
    let mut hook = HOOK.lock().unwrap();
    TaskTerminal(hook.as_mut().filter(|h| h.owner == state as *const AppState as usize).and_then(|h| h.done.take()))
}
pub(super) async fn after_actual_sync_error(state: &AppState) {
    let release = {
        let mut hook = HOOK.lock().unwrap();
        let Some(hook) = hook.as_mut().filter(|h| h.owner == state as *const AppState as usize) else { return; };
        if let Some(arrived) = hook.arrived.take() { let _ = arrived.send(()); }
        hook.release.take()
    };
    if let Some(release) = release { let _ = release.await; }
}
fn install(state: &AppState, pause: bool) -> (oneshot::Receiver<()>, Option<oneshot::Sender<()>>, oneshot::Receiver<()>) {
    let (arrived, waiting) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let (done, terminal) = oneshot::channel();
    *HOOK.lock().unwrap() = Some(Hook { owner: state as *const AppState as usize, arrived: Some(arrived), release: pause.then_some(held), done: Some(done) });
    (waiting, pause.then_some(release), terminal)
}
async fn press(state: Arc<AppState>) {
    let response = crate::web::router(state, "sync-terminal-fixture".into()).oneshot(
        Request::builder().method("POST").uri("/api/command")
            .header("content-type", "application/json").header("authorization", "Bearer fill-token")
            .body(Body::from("\"StartFill\"")).unwrap(),
    ).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}
async fn wait(receiver: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(5), receiver).await.unwrap().unwrap();
}
async fn actual_case(case: &str) {
    let f = crate::fill_recording::tests::Fixture::new_phase(false, gse_sequence::Phase::Passed).await;
    crate::fill_recording::tests::enable_owned_zero(&f);
    let fail = case != "positive";
    let (arrived, release, done) = install(&f.state, fail);
    let router = f.state.topology_router.get().unwrap();
    let types = ["DAQ_LOADCELL_CALIBRATION", "DAQ_KG50_CALIBRATION", "DAQ_THERMAL_CALIBRATION", "DAQ_FILTER_CALIBRATION", "DAQ_KG50_INPUT"];
    let before: Vec<_> = types.iter().map(|name| router.cached_managed_variable(crate::telemetry_schema::data_type(name)).map(|p| p.packet_id())).collect();
    if fail {
        router.enable_network_variable(crate::telemetry_schema::data_type(types[4]), sedsnet::router::NetworkVariablePermissions::READ_ONLY).unwrap();
    }
    let before_progress = crate::gse::fill_recording_progress_snapshot(&f.state);
    press(f.state.clone()).await;
    let original = f.state.fill_recording.continuation.lock().unwrap().clone().unwrap();
    let original_token = original.zero_generation.load(std::sync::atomic::Ordering::Acquire);
    crate::fill_recording::tests::feed_zero(&f, Duration::from_millis(10), |_| {}).await;
    if fail {
        wait(arrived).await; // actual fifth setter returned PermissionDenied
        assert_eq!(f.state.loadcell_calibration.lock().unwrap().ch1_zero_raw, Some(0.2));
        let saved = crate::loadcell::load_or_default();
        assert_eq!(saved.ch1_zero_raw, Some(0.2));
        let after: Vec<_> = types.iter().map(|name| router.cached_managed_variable(crate::telemetry_schema::data_type(name)).map(|p| p.packet_id())).collect();
        for index in 0..4 { assert!(after[index].is_some() && after[index] != before[index], "real earlier setter {} did not update cache", types[index]); }
        assert_eq!(after[4], before[4], "READ_ONLY fifth value must remain unchanged");
        eprintln!("ACTUAL_SETTER_PERMISSION_DENIED_FIRST4_CACHE_IDS {:?} FIFTH {:?}", &after[..4], after[4]);
        if case == "newer" {
            {
                let mut zero = f.state.auto_zero.lock().unwrap();
                zero.kg1000.clear(); zero.kg50.clear();
            }
            f.state.gse.lock().unwrap().observe_mass(Some(0.));
            f.state.gse.lock().unwrap().observe_pressure(Some(0.));
            press(f.state.clone()).await;
            let newer = f.state.fill_recording.continuation.lock().unwrap().clone().unwrap();
            assert!(!Arc::ptr_eq(&newer, &original));
            let token = newer.zero_generation.load(std::sync::atomic::Ordering::Acquire);
            assert_ne!(token, original_token);
            release.unwrap().send(()).unwrap(); wait(done).await;
            assert!(f.state.auto_zero.lock().unwrap().pending());
            assert_eq!(f.state.auto_zero.lock().unwrap().generation, token);
            assert!(Arc::ptr_eq(f.state.fill_recording.continuation.lock().unwrap().as_ref().unwrap(), &newer));
        } else {
            if case == "stop" { f.state.manual_abort_intent.request(); }
            release.unwrap().send(()).unwrap(); wait(done).await;
        }
        for _ in 0..5 { f.pump(); tokio::task::yield_now().await; }
        eprintln!("TERMINAL_SYNC_REAL_VALVE_OWNERS {}", f.sent.lock().unwrap().len());
        assert!(f.sent.lock().unwrap().is_empty(), "terminal sync error must not admit actual valve commands");
        if case != "newer" {
            assert!(f.state.notifications_snapshot().iter().any(|n| n.message.contains("Zero saved locally, but DAQ calibration sync failed") && n.message.contains("Fill was not started")));
            assert!(!f.state.auto_zero.lock().unwrap().completed_fill_capture(original_token));
            assert!(f.state.fill_recording.continuation.lock().unwrap().is_none());
        }
        assert_eq!(crate::gse::fill_recording_progress_snapshot(&f.state), before_progress, "terminal error must not start Fill progress");
    } else {
        wait(done).await;
        crate::fill_recording::tests::settle_zero(&f, true).await;
        assert!(!f.sent.lock().unwrap().is_empty(), "healthy actual zero/sync must still start Fill");
        assert!(f.state.fill_recording.continuation.lock().unwrap().is_none());
    }
}
macro_rules! case {
    ($name:ident,$kind:literal) => {
        #[tokio::test]
        async fn $name() {
            if crate::operator_queue::isolated_case(module_path!(), stringify!($name)) { return; }
            actual_case($kind).await;
        }
    };
}
case!(actual_http_zero_terminal_setter_error_no_fill, "failure");
case!(actual_http_zero_healthy_sync_starts_fill, "positive");
case!(actual_http_zero_old_sync_failure_preserves_newer_owner, "newer");
case!(actual_http_zero_stop_during_sync_error_never_starts_fill, "stop");
