use super::*;
use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
use std::path::PathBuf;

fn child(name: &str) -> bool {
    if std::env::var_os("GS_CLOCK_ADMISSION_CHILD").is_some() {
        return true;
    }
    let root = std::env::temp_dir().join(format!("gs-clock-admission-{}", rand::random::<u128>()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("clock"), "100000").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env("GS_CLOCK_ADMISSION_CHILD", "1")
        .env("GS_CLOCK_ADMISSION_ROOT", &root)
        .env("GS_NETWORK_VARIABLE_CACHE", root.join("variables.json"))
        .env("GS_LAUNCH_CLOCK_STATE", root.join("launch.json"))
        .env("GS_SIM_CLOCK_FILE", root.join("clock"))
        .env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000")
        .env("GS_STATION_ROLE", "groundstation")
        .env("GS_RRLS_ROLE", "primary")
        .env_remove("GS_RRLS")
        .output()
        .unwrap();
    fs::write(root.join("child.stdout"), &output.stdout).unwrap();
    fs::write(root.join("child.stderr"), &output.stderr).unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "{}\n{}\nEvidence retained at {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        root.display()
    );
    fs::remove_dir_all(root).unwrap();
    false
}
async fn fixture() -> (Arc<crate::state::AppState>, Arc<Router>, PathBuf) {
    crate::telemetry_schema::initialize().unwrap();
    let root = PathBuf::from(std::env::var_os("GS_CLOCK_ADMISSION_ROOT").unwrap());
    let state = crate::state::tests::test_app_state().await;
    let now = crate::telemetry_task::procedure_clock::timestamp_ms();
    *state.launch_history.lock().unwrap() =
        crate::launch_history::Store::open(root.join("launch.json"), false, None, now, 0).unwrap();
    state
        .launch_history
        .lock()
        .unwrap()
        .inhibit_automatic_actions()
        .unwrap();
    let waiter = state.launch_history.lock().unwrap().waiter();
    crate::launch_history::flush(waiter).await.unwrap();
    let router = Arc::new(Router::new(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
    ));
    router
        .enable_network_variable(
            crate::telemetry_schema::data_type("DAQ_LOG_CLOCK"),
            NetworkVariablePermissions::READ_WRITE,
        )
        .unwrap();
    assert!(state.topology_router.set(router.clone()).is_ok());
    (state, router, root)
}
fn launch() -> LaunchClockMsg {
    LaunchClockMsg {
        kind: LaunchClockKind::TMinus,
        anchor_timestamp_ms: Some(crate::telemetry_task::procedure_clock::timestamp_ms()),
        duration_ms: Some(10_000),
    }
}
async fn flush(state: &crate::state::AppState) {
    let waiter = state.launch_history.lock().unwrap().waiter();
    crate::launch_history::flush(waiter).await.unwrap();
}

#[tokio::test]
async fn closed_clock_update_cannot_mutate_journal_session_or_sdk_cache() {
    if !child(
        "network_variables::clock_admission_tests::closed_clock_update_cannot_mutate_journal_session_or_sdk_cache",
    ) {
        return;
    }
    let (state, router, root) = fixture().await;
    let before = state.launch_history.lock().unwrap().snapshot();
    let before_disk = fs::read(root.join("launch.json")).unwrap();
    let before_pair = persisted_daq_log_clock_with_revision();
    state
        .operator_admission_barrier
        .store(701, Ordering::Release);
    state.set_launch_clock(launch());
    flush(&state).await;
    let after = state.launch_history.lock().unwrap().snapshot();
    let cached = router
        .get_cached_network_variable(crate::telemetry_schema::data_type("DAQ_LOG_CLOCK"))
        .unwrap();
    println!(
        "clock_update: journal_changed={} disk_changed={} pair_changed={} SDK_cached={} inflight={}",
        before != after,
        before_disk != fs::read(root.join("launch.json")).unwrap(),
        before_pair != persisted_daq_log_clock_with_revision(),
        cached.is_some(),
        state.operator_dispatch_inflight.load(Ordering::Acquire)
    );
    assert_eq!(after, before);
    assert_eq!(state.launch_clock_snapshot(), LaunchClockMsg::idle());
    assert_eq!(before_disk, fs::read(root.join("launch.json")).unwrap());
    assert_eq!(persisted_daq_log_clock_with_revision(), before_pair);
    assert!(cached.is_none());
    state.operator_admission_barrier.store(0, Ordering::Release);
    state.set_launch_clock(launch());
    flush(&state).await;
    assert_ne!(state.launch_clock_snapshot(), LaunchClockMsg::idle());
    assert!(persisted_daq_log_clock_with_revision().unwrap().0[0] > 0);
}

#[tokio::test]
async fn closed_clock_reset_preserves_existing_t0_and_session() {
    if !child(
        "network_variables::clock_admission_tests::closed_clock_reset_preserves_existing_t0_and_session",
    ) {
        return;
    }
    let (state, _router, root) = fixture().await;
    state.set_launch_clock(launch());
    flush(&state).await;
    let before = state.launch_history.lock().unwrap().snapshot();
    let pair = persisted_daq_log_clock_with_revision();
    let disk = fs::read(root.join("launch.json")).unwrap();
    state
        .operator_admission_barrier
        .store(702, Ordering::Release);
    let result = state.try_reset_launch_clock();
    flush(&state).await;
    println!(
        "clock_reset: success={} journal_changed={} pair_changed={}",
        result.is_ok(),
        before != state.launch_history.lock().unwrap().snapshot(),
        pair != persisted_daq_log_clock_with_revision()
    );
    assert!(result.is_err());
    assert_eq!(state.launch_history.lock().unwrap().snapshot(), before);
    assert_eq!(persisted_daq_log_clock_with_revision(), pair);
    assert_eq!(fs::read(root.join("launch.json")).unwrap(), disk);
    state.operator_admission_barrier.store(0, Ordering::Release);
    state.try_reset_launch_clock().unwrap();
    flush(&state).await;
    assert_eq!(state.launch_clock_snapshot(), LaunchClockMsg::idle());
}

#[tokio::test]
async fn closed_heartbeat_retry_retains_exact_pending_metadata_until_reopen() {
    if !child(
        "network_variables::clock_admission_tests::closed_heartbeat_retry_retains_exact_pending_metadata_until_reopen",
    ) {
        return;
    }
    let (state, router, _root) = fixture().await;
    let ty = crate::telemetry_schema::data_type("DAQ_LOG_CLOCK");
    router
        .enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
        .unwrap();
    state.set_launch_clock(launch());
    flush(&state).await;
    let original = persisted_daq_log_clock_with_revision().unwrap();
    assert_eq!(PENDING_DAQ_CLOCK.lock().unwrap().desired, Some(original));
    router
        .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
        .unwrap();
    state
        .operator_admission_barrier
        .store(703, Ordering::Release);
    let accepted = retry_pending_daq_log_clock_for_state(&state, &router).unwrap();
    println!(
        "heartbeat_retry: accepted={accepted} pending_preserved={}",
        PENDING_DAQ_CLOCK.lock().unwrap().desired == Some(original)
    );
    assert!(!accepted);
    assert_eq!(PENDING_DAQ_CLOCK.lock().unwrap().desired, Some(original));
    state.operator_admission_barrier.store(0, Ordering::Release);
    assert!(retry_pending_daq_log_clock_for_state(&state, &router).unwrap());
    let actual = router.get_cached_network_variable(ty).unwrap().unwrap();
    assert_eq!(actual.timestamp(), original.1);
    assert_eq!(
        actual.payload(),
        daq_log_clock_packet(original.0, original.1)
            .unwrap()
            .payload()
    );
}

#[tokio::test]
async fn cancelled_clock_awaiter_keeps_actual_blocked_publisher_owned() {
    if !child(
        "network_variables::clock_admission_tests::cancelled_clock_awaiter_keeps_actual_blocked_publisher_owned",
    ) {
        return;
    }
    let (state, _router, _root) = fixture().await;
    // Hold the actual variable-store mutex in a separate owner thread. This
    // blocks the real publisher, without adding a fake save/SDK implementation
    // or holding a test mutex across an async suspension.
    struct Hold {
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        owner: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Hold {
        fn drop(&mut self) {
            *self.release.0.lock().unwrap() = true;
            self.release.1.notify_all();
            self.owner.take().unwrap().join().unwrap();
        }
    }
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let held = release.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        let _actual_store = store().lock().unwrap();
        entered_tx.send(()).unwrap();
        let mut released = held.0.lock().unwrap();
        while !*released {
            released = held.1.wait(released).unwrap();
        }
    });
    let hold = Hold {
        release,
        owner: Some(owner),
    };
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let worker_state = state.clone();
    let worker = tokio::task::spawn_blocking(move || {
        worker_state.set_launch_clock(launch());
        let _ = done_tx.send(());
    });
    let awaiter = tokio::spawn(async move {
        worker.await.unwrap();
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while state.launch_clock_snapshot().kind != LaunchClockKind::TMinus {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let generation = crate::gse::automatic_generation(&state);
    state
        .operator_admission_barrier
        .store(704, Ordering::Release);
    awaiter.abort();
    assert!(awaiter.await.unwrap_err().is_cancelled());
    println!(
        "actual blocked clock publisher after awaiter cancellation: inflight={}",
        state.operator_dispatch_inflight.load(Ordering::Acquire)
    );
    assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
    assert!(crate::gse::reset_generation::invalidate_capture_epoch(&state, generation).is_err());
    assert_eq!(crate::gse::automatic_generation(&state), generation);
    // An explicit Stop remains independently retained while storage is busy.
    state.manual_abort_intent.request();
    assert!(state.manual_abort_intent.pending_epoch().is_some());
    drop(hold);
    tokio::time::timeout(std::time::Duration::from_secs(3), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    flush(&state).await;
}
