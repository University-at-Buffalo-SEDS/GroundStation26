use super::*;
use crate::recording_export::tests::TestDirectory;
use crate::recording_store::EventKind;
use crate::sd_download::fc::{Signals, Stage};
use crate::sd_download::tests::pump;
use sedsnet::router::RouterConfig;

const T0: i64 = 1_800_000_000_000;

fn fc_csv(from: i64, to: i64) -> Vec<u8> {
    let mut text = String::from("timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence\r\n# seds-sd,format=fc,version=2,board=FC,firmware=abc,tick_hz=1000,schema=deadbeef\r\n");
    let mut ts = from;
    while ts < to {
        text.push_str(&format!("{ts},{},BARO,\"1 2 3\",\"\",\"\",\"\",network,,,1,1000,1\r\n", ts - T0));
        ts += 100;
    }
    text.into_bytes()
}

fn launch_name() -> String {
    format!("irec_LAUNCH-{}-000.csv", (T0 - 10_000) / 1000)
}

fn link() -> (Arc<Router>, Arc<Router>) {
    let gs = Arc::new(Router::new(RouterConfig::default().with_hostname("GS").with_static_address(0x2001)));
    let rrls = Arc::new(Router::new(RouterConfig::default().with_hostname("RRLS").with_static_address(0x2004)));
    let r = rrls.clone();
    gs.add_side_packet("to-rrls", move |p| r.rx_from_side(p, 0));
    let g = gs.clone();
    rrls.add_side_packet("to-gs", move |p| g.rx_from_side(p, 0));
    gs.announce_discovery().unwrap();
    rrls.announce_discovery().unwrap();
    for _ in 0..3 {
        gs.process_all_queues().unwrap();
        rrls.process_all_queues().unwrap();
    }
    assert!(gs.resolve_hostname(RRLS_HOST).is_some());
    (gs, rrls)
}

async fn capture(root: &FsPath) -> PathBuf {
    let capture = crate::recording_store::Capture {
        serial: 1,
        kind: EventKind::Launch,
        key: "flight".into(),
        label: "Launch".into(),
        event_start_ms: T0,
        window_start_ms: T0 - 600_000,
        event_end_ms: Some(T0 + 120_000),
        post_roll_ms: 0,
        max_end_ms: T0 + 7_200_000,
        result: Some("Landed".into()),
        operator: None,
        station_mode: "normal".into(),
        provenance: None,
        details: serde_json::Value::Null,
        materializing: false,
    };
    crate::recording_store::materialize(root, &capture, T0 + 200_000).await.unwrap().folder
}

/// An RRLS data root with this flight's launch file and lead-up in its outbox.
fn rrls_outbox(root: &FsPath, launch: &[u8], with_lead_up: bool) {
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("l.csv"), launch).unwrap();
    register(root, &src.join("l.csv"), &launch_name(), "launch", &launch_name(), 3, false).unwrap();
    if with_lead_up {
        std::fs::write(src.join("u.csv"), fc_csv(T0 - 70_000, T0 - 10_000)).unwrap();
        let lead = format!("{}.leadup.csv", launch_name().trim_end_matches(".csv"));
        register(root, &src.join("u.csv"), &lead, "lead_up", &launch_name(), 0, true).unwrap();
        resolve_lead_up(root, &launch_name());
    }
}

fn pull_env(manager: &'static Manager, router: Arc<Router>, root: &FsPath, gate: Gate) -> (PullEnv, Arc<Mutex<Vec<String>>>) {
    let notes = Arc::new(Mutex::new(Vec::new()));
    let n = notes.clone();
    (
        PullEnv { manager, router, root: root.to_path_buf(), rate: 10_000_000, rate_fn: None, gate, notify: Arc::new(move |m| n.lock().unwrap().push(m)) },
        notes,
    )
}

#[test]
fn path_selection_prefers_a_clean_transmitting_rrls_only_when_direct_is_bad() {
    let good = PathMetrics {
        gs_direct_age_ms: Some(200),
        gs_good: 600,
        gs_crc: 2,
        rrls_online: true,
        rrls_hears_av_bay: true,
        rrls_good: 600,
        rrls_crc: 0,
        rrls_can_transmit: true,
    };
    assert_eq!(choose_path(&good).0, Path::Direct, "direct good: stay direct");
    let lost = PathMetrics { gs_direct_age_ms: Some(5_000), ..good };
    assert_eq!(choose_path(&lost).0, Path::Rrls);
    let poor = PathMetrics { gs_good: 100, gs_crc: 60, ..good };
    assert_eq!(choose_path(&poor), (Path::Rrls, "direct link poor; RRLS link clean"));
    assert_eq!(choose_path(&PathMetrics { rrls_online: false, ..lost }).0, Path::Direct, "RRLS out of range");
    assert_eq!(choose_path(&PathMetrics { rrls_hears_av_bay: false, ..lost }).0, Path::Direct);
    assert_eq!(choose_path(&PathMetrics { rrls_good: 50, rrls_crc: 50, ..lost }).1, "RRLS link to the AV bay is poor");
    assert_eq!(choose_path(&PathMetrics { rrls_can_transmit: false, ..lost }).1, "RRLS is in standby (radio silent)");
    assert_eq!(choose_path(&PathMetrics { gs_direct_age_ms: None, ..good }).0, Path::Rrls, "never heard directly");
}

#[test]
fn power_off_is_ready_only_after_a_verified_complete_copy() {
    let none = Holding::default();
    let launch_only = Holding { launch_verified: true, lead_up_resolved: false };
    let full = Holding { launch_verified: true, lead_up_resolved: true };
    assert!(!power_off_ready(true, none, none).ready);
    assert!(!power_off_ready(true, launch_only, none).ready, "lead-up still wanted");
    assert!(power_off_ready(false, launch_only, none).ready, "lead-up not wanted");
    let at_rrls = power_off_ready(true, none, full);
    assert_eq!((at_rrls.ready, at_rrls.at.as_deref()), (true, Some("RRLS")), "complete at the RRLS counts before the forward");
    assert_eq!(at_rrls.message, "FC SD fully received by RRLS, checksums verified: AV bay can be powered off");
    assert_eq!(power_off_ready(true, full, full).at.as_deref(), Some("GS"));
    // From the job and the RRLS listing.
    let mut job = FcJob { triggered_at_ms: 10, launch_file: Some("a.csv".into()), stage: Stage::Launch, ..FcJob::default() };
    let mut relay = RelayState::default();
    assert!(!power_off_for(Some(&job), true, &relay).ready, "a partial download is not ready");
    relay.rrls_outbox.push(OutboxEntry { name: "a.csv".into(), kind: "launch".into(), sha256: "ab".into(), lead_up_resolved: true, ..OutboxEntry::default() });
    assert!(power_off_for(Some(&job), true, &relay).ready);
    relay.rrls_outbox.clear();
    job.sha256 = Some("ab".into());
    job.stage = Stage::LeadUp;
    assert!(!power_off_for(Some(&job), true, &relay).ready);
    job.stage = Stage::Done;
    assert!(power_off_for(Some(&job), true, &relay).ready);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gs_forwards_verifies_confirms_and_attaches_with_relay_provenance() {
    let gs_root = TestDirectory::new();
    let rrls_root = TestDirectory::new();
    let folder = capture(&gs_root.0).await;
    let launch = fc_csv(T0 - 10_000, T0 + 20_000);
    rrls_outbox(&rrls_root.0, &launch, true);
    let (gs, rrls) = link();
    let corrupt = Arc::new(AtomicU64::new(2));
    serve(&rrls, rrls_root.0.clone(), Some(corrupt.clone())).unwrap();
    let (running, task) = pump(gs.clone(), rrls.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (env, notes) = pull_env(manager, gs.clone(), &gs_root.0, Arc::new(|| None));
    assert_eq!(pull(&env).await.unwrap(), 2);
    assert_eq!(corrupt.load(Ordering::SeqCst), 0, "two corrupt answers were requested again");
    let manifest = crate::recording_store::read_manifest(&folder).unwrap();
    let entry = &manifest.board_sd[0];
    assert_eq!(std::fs::read(folder.join("board_sd/FC").join(launch_name())).unwrap(), launch);
    let relay = entry.relay.as_ref().unwrap();
    assert_eq!((relay.via.as_str(), relay.relay_id.as_str(), relay.hops.len()), ("RRLS", "RRLS", 2));
    assert_eq!(relay.hops[0].crc_errors, 3, "the FC->RRLS hop's count");
    assert_eq!(relay.hops[1].sha256, entry.sha256);
    assert_eq!(entry.source, "network_download");
    assert_eq!(entry.lead_up_rows, 600, "the forwarded lead-up sits beside it");
    // The RRLS keeps its copies, now confirmed by hash.
    let outbox = load_outbox(&rrls_root.0);
    assert!(outbox.iter().all(|e| e.confirmed_at_ms.is_some()), "{outbox:?}");
    assert!(outbox_path(&rrls_root.0, &launch_name()).exists(), "never deleted automatically");
    let state = load_state(&gs_root.0);
    assert!(state.received.iter().all(|r| r.confirmed) && state.received.len() == 2);
    assert!(notes.lock().unwrap().iter().any(|n| n.contains("SHA-256 verified")));
    // A second pass forwards nothing.
    assert_eq!(pull(&env).await.unwrap(), 0);
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_forward_resumes_across_a_link_drop_and_a_restart() {
    let gs_root = TestDirectory::new();
    let rrls_root = TestDirectory::new();
    capture(&gs_root.0).await;
    let launch = fc_csv(T0 - 10_000, T0 + 60_000);
    rrls_outbox(&rrls_root.0, &launch, false);
    let (gs, rrls) = link();
    serve(&rrls, rrls_root.0.clone(), None).unwrap();
    let (running, task) = pump(gs.clone(), rrls.clone());
    let part = gs_root.0.join("sd-downloads/FC/RRLS").join(format!("{}.part", launch_name()));
    let p = part.clone();
    // The link drops once 8 KiB have arrived.
    let gate: Gate = Arc::new(move || (std::fs::metadata(&p).map_or(0, |m| m.len()) >= 8192).then(|| "RRLS link lost".to_string()));
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (env, _) = pull_env(manager, gs.clone(), &gs_root.0, gate);
    assert!(matches!(pull(&env).await, Err(StreamError::Gate(_))));
    let kept = std::fs::metadata(&part).unwrap().len();
    assert!(kept >= 8192 && kept < launch.len() as u64);
    assert_eq!(load_state(&gs_root.0).forwarding.as_ref().map(|f| f.0.clone()), Some(launch_name()));
    // GS restart: fresh manager; the RRLS restarts too (re-served from disk).
    let manager2: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (env2, _) = pull_env(manager2, gs.clone(), &gs_root.0, Arc::new(|| None));
    rrls.clear_p2p_stream_port(RELAY_STREAM_PORT);
    serve(&rrls, rrls_root.0.clone(), None).unwrap();
    assert_eq!(pull(&env2).await.unwrap(), 1);
    let job = manager2.list().into_iter().find(|j| j.name == launch_name()).unwrap();
    assert_eq!(job.resumed_from, kept, "resumed by offset");
    assert_eq!(std::fs::read(received_path(&gs_root.0, &launch_name())).unwrap(), launch);
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_hop_is_refused_by_sha256_and_the_forward_yields_to_commands() {
    let gs_root = TestDirectory::new();
    let rrls_root = TestDirectory::new();
    let folder = capture(&gs_root.0).await;
    rrls_outbox(&rrls_root.0, &fc_csv(T0 - 10_000, T0 + 5_000), false);
    // The RRLS copy no longer matches its recorded hash (a damaged hop).
    modify_outbox(&rrls_root.0, |e| e[0].sha256 = "00".repeat(32));
    let (gs, rrls) = link();
    serve(&rrls, rrls_root.0.clone(), None).unwrap();
    let (running, task) = pump(gs.clone(), rrls.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    // Command traffic on the RRLS link holds the forward.
    let signals = Arc::new(Signals::new(60_000));
    signals.note_command(now_ms());
    let s = signals.clone();
    let quiet_gate: Gate = Arc::new(move || s.reason(now_ms()));
    let (held, _) = pull_env(manager, gs.clone(), &gs_root.0, quiet_gate);
    assert!(matches!(pull(&held).await, Err(StreamError::Gate(r)) if r.contains("commands")));
    let (env, notes) = pull_env(manager, gs.clone(), &gs_root.0, Arc::new(|| None));
    for _ in 0..2 {
        assert!(matches!(pull(&env).await, Err(StreamError::Other(e)) if e.contains("SHA-256")));
    }
    assert_eq!(pull(&env).await.unwrap(), 1, "third mismatch: refused");
    let state = load_state(&gs_root.0);
    assert_eq!(state.refused, vec![launch_name()]);
    assert!(state.received.is_empty());
    assert!(!received_path(&gs_root.0, &launch_name()).exists(), "the bad copy is not kept");
    assert!(crate::recording_store::read_manifest(&folder).unwrap().board_sd.is_empty(), "never attached");
    assert!(load_outbox(&rrls_root.0)[0].confirmed_at_ms.is_none(), "never confirmed");
    assert!(notes.lock().unwrap().iter().any(|n| n.contains("refused")));
    assert_eq!(pull(&env).await.unwrap(), 0, "a refused file is not fetched again");
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}
