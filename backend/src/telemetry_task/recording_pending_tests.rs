//! Actual published SDK core ownership + the actual sole SQLite actor.
use super::*;
use crate::fill_recording::{Authority, Captured};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

struct HeldCore {
    pause: crate::gse::nonwaiting_admission_tests::Pause,
    thread: Option<std::thread::JoinHandle<sedsnet::TelemetryResult<()>>>,
}
impl HeldCore {
    fn new(state: &AppState, router: &Arc<sedsnet::router::Router>) -> Self {
        let pause = crate::gse::nonwaiting_admission_tests::Pause::install(state);
        let packet = sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            100,
            Arc::from([0u8]),
        )
        .unwrap();
        let inspected = router.clone();
        let router = router.clone();
        let thread = std::thread::Builder::new()
            .name("production-sdk-core-holder".into())
            .spawn(move || router.seed_managed_variable(packet))
            .unwrap();
        pause
            .core_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("actual published SDK core held");
        assert_eq!(
            inspected.try_has_pending_reliable_tx_type(crate::telemetry_schema::types::ABORT),
            Err(sedsnet::router::TryPendingTypeError::CoreBusy)
        );
        Self {
            pause,
            thread: Some(thread),
        }
    }
    fn release(&mut self) {
        self.pause.release();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap().unwrap();
        }
    }
}
impl Drop for HeldCore {
    fn drop(&mut self) {
        self.release();
    }
}
fn directory() -> PathBuf {
    let path = std::path::PathBuf::from(std::env::var_os("GS_RECOVERY_ISOLATED_ROOT").unwrap())
        .join("actor-recordings");
    std::fs::create_dir(&path).unwrap();
    path
}
fn install_router(state: &AppState) -> Arc<sedsnet::router::Router> {
    let owner = state as *const AppState as usize;
    let started = std::time::Instant::now();
    let router = Arc::new(sedsnet::router::Router::new_with_clock(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        Box::new(move || {
            crate::gse::nonwaiting_admission_tests::clock_pause(owner);
            started.elapsed().as_millis() as u64
        }),
    ));
    assert!(state.topology_router.set(router.clone()).is_ok());
    router
}
async fn begin_request(state: &Arc<AppState>) -> Receipt {
    let origin = Captured::capture(state).bind(
        Authority::Physical {
            current: Arc::new(|| true),
        },
        None,
    );
    let (request, reply) = EnsureRequest::new(origin, "StartFill");
    state
        .db_queue_tx
        .send(DbQueueItem::EnsureRecording(request))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), reply)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
type StagedRequest = (
    Arc<EnsureRequest>,
    tokio::sync::oneshot::Receiver<Result<Receipt, Error>>,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
);
fn staged_request(state: &AppState) -> StagedRequest {
    let (entered, received) = mpsc::channel();
    let (proceed, resumed) = mpsc::channel();
    let resumed = std::sync::Mutex::new(resumed);
    let calls = AtomicUsize::new(0);
    let origin = Captured::capture(state).bind(
        Authority::Physical {
            current: Arc::new(move || {
                // First authority check precedes preparation; the second follows
                // actual pool opening/one-shot instance reservation.
                if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    entered.send(()).unwrap();
                    resumed
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap();
                }
                true
            }),
        },
        None,
    );
    let (request, reply) = EnsureRequest::new(origin, "StartFill");
    (request, reply, received, proceed)
}
fn recording_files(path: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|ext| ext == "db")
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("groundstation_recording_"))
            .then_some(path)
        })
        .collect()
}
#[tokio::test]
async fn actual_actor_services_writes_while_original_publication_is_core_busy_and_resumes_once() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_actor_services_writes_while_original_publication_is_core_busy_and_resumes_once",
    ) {
        return;
    }
    let mut state = crate::state::tests::test_app_state().await;
    let router = install_router(&state);
    let directory = directory();
    let _owner = TestOwner::start(&mut state, &directory);
    let first = begin_request(&state).await;
    let original_pool = state.telemetry_db_pool();
    revoke(&state);
    let (request, mut reply, staged, proceed) = staged_request(&state);
    let original = request.clone();
    state
        .db_queue_tx
        .send(DbQueueItem::EnsureRecording(request))
        .await
        .unwrap();
    staged
        .recv_timeout(Duration::from_secs(2))
        .expect("same instance prepared before core hold");
    let reserved = state.fill_recording.next_instance_for_test();
    let mut core = HeldCore::new(&state, &router);
    proceed.send(()).unwrap();
    for id in 0..16 {
        state
            .db_queue_tx
            .send(DbQueueItem::Write(DbWrite::Telemetry {
                timestamp_ms: id,
                source_timestamp_ms: Some(id),
                data_type: "FAIR_PROBE".into(),
                sender_id: "DAQ".into(),
                values_json: Some("[1]".into()),
                payload_json: "[1]".into(),
            }))
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM telemetry WHERE data_type='FAIR_PROBE'")
                    .fetch_one(&original_pool)
                    .await
                    .unwrap();
            if count == 16 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("actual DB writes must flush while SDK core remains held");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(
        reply.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(original.pending());
    assert_eq!(state.fill_recording.ready_instance(), 0);
    assert_eq!(
        state.fill_recording.next_instance_for_test(),
        reserved,
        "no new instance across 100ms slices"
    );
    assert_eq!(
        recording_files(&directory),
        vec![PathBuf::from(&first.path)]
    );
    core.release();
    let receipt = tokio::time::timeout(Duration::from_secs(2), reply)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(receipt.path, first.path);
    assert_ne!(receipt.instance, first.instance);
    assert_eq!(receipt.instance + 1, reserved);
    assert_eq!(state.fill_recording.next_instance_for_test(), reserved);
    assert!(state.fill_recording.current(&receipt));
    assert!(!state.abort_indicator_latched());
}
#[tokio::test]
async fn actual_recording_control_cleans_only_busy_original_unpublished_pool_before_core_release() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_recording_control_cleans_only_busy_original_unpublished_pool_before_core_release",
    ) {
        return;
    }
    let mut state = crate::state::tests::test_app_state().await;
    let router = install_router(&state);
    let directory = directory();
    std::fs::write(directory.join("archive.db"), b"keep").unwrap();
    let _owner = TestOwner::start(&mut state, &directory);
    let (request, reply, staged, proceed) = staged_request(&state);
    state
        .db_queue_tx
        .send(DbQueueItem::EnsureRecording(request))
        .await
        .unwrap();
    staged.recv_timeout(Duration::from_secs(2)).unwrap();
    let candidate = recording_files(&directory);
    assert_eq!(
        candidate.len(),
        1,
        "real SQLite pool opened once before SDK hold"
    );
    let reserved = state.fill_recording.next_instance_for_test();
    let mut core = HeldCore::new(&state, &router);
    proceed.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(recording_files(&directory), candidate);
    assert_eq!(state.fill_recording.next_instance_for_test(), reserved);
    state
        .db_queue_tx
        .send(DbQueueItem::Control(RecordingCommand::Stop))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), reply)
            .await
            .unwrap()
            .unwrap(),
        Err(Error::Unavailable)
    );
    assert!(recording_files(&directory).is_empty());
    assert_eq!(
        std::fs::read(directory.join("archive.db")).unwrap(),
        b"keep"
    );
    assert_eq!(state.fill_recording.ready_instance(), 0);
    core.release();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        state.fill_recording.ready_instance(),
        0,
        "late SDK release cannot resurrect original request"
    );
}
#[tokio::test]
async fn actual_core_busy_owner_expires_at_original_five_seconds_without_reopening_or_late_publication()
 {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_core_busy_owner_expires_at_original_five_seconds_without_reopening_or_late_publication",
    ) {
        return;
    }
    let mut state = crate::state::tests::test_app_state().await;
    let router = install_router(&state);
    let directory = directory();
    let _owner = TestOwner::start(&mut state, &directory);
    let (request, reply, staged, proceed) = staged_request(&state);
    let began = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(2200)).await;
    state
        .db_queue_tx
        .send(DbQueueItem::EnsureRecording(request))
        .await
        .unwrap();
    staged.recv_timeout(Duration::from_secs(2)).unwrap();
    let reserved = state.fill_recording.next_instance_for_test();
    let mut core = HeldCore::new(&state, &router);
    proceed.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(4), reply)
            .await
            .unwrap()
            .unwrap(),
        Err(Error::Stale)
    );
    assert!(began.elapsed() >= Duration::from_millis(4950));
    assert!(began.elapsed() < Duration::from_secs(7));
    assert!(recording_files(&directory).is_empty());
    assert_eq!(state.fill_recording.next_instance_for_test(), reserved);
    assert_eq!(state.fill_recording.ready_instance(), 0);
    core.release();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(state.fill_recording.ready_instance(), 0);
    assert!(!state.abort_indicator_latched());
}
