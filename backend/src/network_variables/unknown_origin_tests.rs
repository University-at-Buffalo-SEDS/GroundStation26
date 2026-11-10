use super::*;
use sedsnet::router::RouterConfig;

fn run(case: &str, retained: Option<(u8, u64)>) {
    run_with(case, retained, true)
}

/// `get`: whether the board asks with a managed GET. Boards back off an
/// unanswered GET to every 4 s, so a station that has or acquires an
/// authoritative state must push it, not wait for the next GET.
fn run_with(case: &str, retained: Option<(u8, u64)>, get: bool) {
    const CHILD: &str = "GS_UNKNOWN_ORIGIN_CHILD";
    if std::env::var(CHILD).ok().as_deref() != Some(case) {
        let root = std::env::temp_dir().join(format!("gs-origin-{}", rand::random::<u128>()));
        fs::create_dir_all(&root).unwrap();
        let name = format!("network_variables::unknown_origin_tests::{case}");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, case)
            .env("GS_NETWORK_VARIABLE_CACHE", root.join("cache.json"))
            .env("GS_FLIGHT_STATE_DEFAULT", "1")
            .env("GS_RRLS_ROLE", "primary")
            .env_remove("GS_RRLS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
        return;
    }
    crate::telemetry_schema::initialize().unwrap();
    if let Some((state, stamp)) = retained {
        let value = serde_json::json!({"flight_state":state,"flight_state_stamp":stamp});
        fs::write(cache_path(), serde_json::to_vec(&value).unwrap()).unwrap();
    }
    let source = Arc::new(Router::new_with_clock(
        RouterConfig::new([]).with_sender("GS"),
        Box::new(|| 0),
    ));
    crate::telemetry_task::refresh_host_network_time(&source, 1_767_225_600_000);
    initialize_for_role(&source, false).unwrap();
    let ty = crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE);
    let expected = retained.filter(|(state, stamp)| *state != 7 && *stamp != 0);
    let cached = source.get_cached_network_variable(ty).unwrap();
    assert_eq!(
        cached.as_ref().map(|p| (p.payload()[0], p.timestamp())),
        expected,
        "unknown-origin default/retained state must never seed actual SDK cache"
    );
    let before = store().lock().unwrap().values;
    assert_eq!(
        before.flight_state_stamp,
        retained.map_or(0, |(_, stamp)| stamp),
        "bootstrap cannot restamp retained intent"
    );
    let peer = Arc::new(Router::new(RouterConfig::new([]).with_sender("RF")));
    peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
        .unwrap();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let output = observations.clone();
    peer.on_network_variable_update(ty, move |p| {
        output.lock().unwrap().push((p.payload()[0], p.timestamp()));
        Ok(())
    })
    .unwrap();
    let src_link: Arc<Mutex<Option<Arc<Router>>>> = Arc::new(Mutex::new(Some(source.clone())));
    let dst_link: Arc<Mutex<Option<Arc<Router>>>> = Arc::new(Mutex::new(Some(peer.clone())));
    let dst = dst_link.clone();
    source.add_side_packet("rf", move |p| {
        let r = dst.lock().unwrap().clone();
        r.map_or(Ok(()), |r| r.rx_from_side(p, 0))
    });
    let src = src_link.clone();
    peer.add_side_packet("gs", move |p| {
        let r = src.lock().unwrap().clone();
        r.map_or(Ok(()), |r| r.rx_from_side(p, 0))
    });
    source.announce_discovery().unwrap();
    peer.announce_discovery().unwrap();
    if get {
        peer.request_managed_variable(ty).unwrap();
    }
    for _ in 0..32 {
        source.process_all_queues().unwrap();
        peer.process_all_queues().unwrap();
    }
    assert!(
        observations
            .lock()
            .unwrap()
            .iter()
            .all(|(_, stamp)| *stamp != 0)
    );
    if let (true, Some(expected)) = (get, expected) {
        assert_eq!(observations.lock().unwrap().last().copied(), Some(expected));
    } else if expected.is_none() {
        assert!(observations.lock().unwrap().is_empty());
    }
    publish_current(&source).unwrap();
    for _ in 0..32 {
        source.process_all_queues().unwrap();
        peer.process_all_queues().unwrap();
    }
    assert!(
        observations
            .lock()
            .unwrap()
            .iter()
            .all(|(_, stamp)| *stamp != 0),
        "refresh must not publish zero origin"
    );
    if let (false, Some(expected)) = (get, expected) {
        assert_eq!(
            observations.lock().unwrap().last().copied(),
            Some(expected),
            "bootstrap pushes retained authoritative state without a board GET"
        );
    }
    assert_eq!(
        store().lock().unwrap().values.flight_state_stamp,
        before.flight_state_stamp
    );
    set_flight_state(&source, 1).unwrap();
    for _ in 0..32 {
        source.process_all_queues().unwrap();
        peer.process_all_queues().unwrap();
    }
    let explicit = observations.lock().unwrap().last().copied().unwrap();
    assert_eq!(
        explicit,
        (1, 1_767_225_600_000),
        "real explicit state producer remains usable with original validated UTC"
    );
    *src_link.lock().unwrap() = None;
    *dst_link.lock().unwrap() = None;
}
#[test]
fn default_startup_unknown() {
    run("default_startup_unknown", None)
}
#[test]
fn legacy_startup_unknown() {
    run("legacy_startup_unknown", Some((1, 0)))
}
#[test]
fn retained_abort_origin() {
    run("retained_abort_origin", Some((15, 1000)))
}
#[test]
fn retained_idle_origin() {
    run("retained_idle_origin", Some((0, 1000)))
}
#[test]
fn retained_launch_is_history() {
    run("retained_launch_is_history", Some((7, 1000)))
}
#[test]
fn retained_idle_pushed_without_get() {
    run_with("retained_idle_pushed_without_get", Some((0, 1000)), false)
}
#[test]
fn retained_abort_pushed_without_get() {
    run_with("retained_abort_pushed_without_get", Some((15, 1000)), false)
}
#[test]
fn unknown_origin_new_state_pushed_without_get() {
    run_with("unknown_origin_new_state_pushed_without_get", None, false)
}
