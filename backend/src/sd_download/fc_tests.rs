use super::*;
use crate::recording_export::tests::{TestDirectory, export_state};
use crate::recording_store::EventKind;
use crate::sd_download::tests::{pattern, pump};
use sedsnet::router::{P2pStreamEvent, RouterConfig};
use std::sync::atomic::AtomicUsize;

const T0: i64 = 1_800_000_000_000;
const HEADER: &str = "timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence\r\n";
const FORMAT_LINE: &str = "# seds-sd,format=fc,version=2,board=FC,firmware=0123456789ab,tick_hz=1000,schema=deadbeef\r\n";

/// FC CSV rows every `period` ms over `[from, to)` (network time).
fn fc_csv(from: i64, to: i64, period: i64) -> Vec<u8> {
    let mut text = format!("{HEADER}{FORMAT_LINE}# seds-sd-columns,u64:ms\r\n");
    let mut ts = from;
    let mut seq = 0;
    while ts < to {
        let mono = ts - T0 + 600_000;
        text.push_str(&format!("{ts},{mono},BARO,\"1.0e+02 9.1e+04 2.0e+01\",\"\",\"\",\"\",network,,,{mono},1000,{seq}\r\n"));
        ts += period;
        seq += 1;
    }
    text.into_bytes()
}

fn launch_name() -> String {
    format!("irec_LAUNCH-{}-000.csv", (T0 - 10_000) / 1000)
}

// ------------------------------------------------------------- mock FC

struct MockFc {
    files: Vec<(String, Vec<u8>, bool)>,
    reads: AtomicUsize,
    /// Corrupt the CRC of this many READ answers (from the first).
    corrupt: AtomicUsize,
    /// Run `on_read` at this READ.
    trigger_after: usize,
    on_read: Box<dyn Fn() + Send + Sync>,
}

impl MockFc {
    fn new(files: Vec<(String, Vec<u8>, bool)>) -> Self {
        Self { files, reads: AtomicUsize::new(0), corrupt: AtomicUsize::new(0), trigger_after: usize::MAX, on_read: Box::new(|| {}) }
    }
}

fn routers() -> (Arc<Router>, Arc<Router>) {
    let gs = Arc::new(Router::new(RouterConfig::default().with_hostname("GS").with_static_address(0x2001)));
    let fc = Arc::new(Router::new(RouterConfig::default().with_hostname("FC").with_static_address(0x2003)));
    (gs, fc)
}

/// Instant link (as the DAQ tests).
fn link() -> (Arc<Router>, Arc<Router>) {
    let (gs, fc) = routers();
    let fc_rx = fc.clone();
    gs.add_side_packet("to-fc", move |packet| fc_rx.rx_from_side(packet, 0));
    let gs_rx = gs.clone();
    fc.add_side_packet("to-gs", move |packet| gs_rx.rx_from_side(packet, 0));
    gs.announce_discovery().unwrap();
    fc.announce_discovery().unwrap();
    for _ in 0..3 {
        gs.process_all_queues().unwrap();
        fc.process_all_queues().unwrap();
    }
    assert!(gs.resolve_hostname("FC").is_some());
    (gs, fc)
}

/// The FC side of protocol v1 + CRC: card root only, CRC on the last frame.
fn serve(fc: &Arc<Router>, board: Arc<MockFc>) {
    let reply = fc.clone();
    fc.bind_p2p_stream_port(SD_FILE_STREAM_PORT, move |event: P2pStreamEvent<'_>| {
        if event.kind != P2pStreamEventKind::Data || event.payload.is_empty() {
            return Ok(());
        }
        let p = event.payload;
        let mut frames: Vec<Vec<u8>> = Vec::new();
        match p[0] {
            OP_LIST => {
                let index = u16::from_le_bytes([p[2], p[3]]) as usize;
                let entry = (p[1] == 0).then(|| board.files.get(index)).flatten();
                let mut f = vec![0xa1, if entry.is_some() { 0 } else { STATUS_END }, p[1], p[2], p[3]];
                let (size, name, open) = entry.map_or((0, "", false), |e| (e.1.len() as u32, e.0.as_str(), e.2));
                f.extend_from_slice(&size.to_le_bytes());
                f.extend_from_slice(&[u8::from(open), name.len() as u8, 0]);
                f.extend_from_slice(name.as_bytes());
                frames.push(f);
            }
            OP_READ => {
                let n = board.reads.fetch_add(1, Ordering::SeqCst) + 1;
                if n == board.trigger_after {
                    (board.on_read)();
                }
                let offset = u32::from_le_bytes(p[1..5].try_into().unwrap()) as usize;
                let name = std::str::from_utf8(&p[7..]).unwrap();
                let data = &board.files.iter().find(|f| f.0 == name).unwrap().1;
                let header = |status: u8, flags: u8, off: usize| {
                    let mut f = vec![0xa2, status, flags];
                    f.extend_from_slice(&(off as u32).to_le_bytes());
                    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    f
                };
                if offset >= data.len() {
                    frames.push(header(STATUS_END, FLAG_LAST, offset));
                } else {
                    let end = ((offset / 512 + 1) * 512).min(data.len());
                    let mut crc = crc32(&data[offset..end]);
                    if board.corrupt.load(Ordering::SeqCst) > 0 {
                        board.corrupt.fetch_sub(1, Ordering::SeqCst);
                        crc ^= 1;
                    }
                    let mut at = offset;
                    while at < end {
                        let to = (at + 100).min(end);
                        let last = to == end;
                        let mut f = header(0, if last { FLAG_LAST | FLAG_CRC } else { 0 }, at);
                        f.extend_from_slice(&data[at..to]);
                        if last {
                            f.extend_from_slice(&crc.to_le_bytes());
                        }
                        assert!(f.len() <= 115);
                        frames.push(f);
                        at = to;
                    }
                }
            }
            OP_CANCEL => frames.push(vec![0xa3, 0]),
            _ => {}
        }
        for f in frames {
            reply.send_p2p_stream(event.stream_id, &f)?;
        }
        Ok(())
    })
    .unwrap();
}

async fn capture(root: &FsPath, kind: EventKind) -> PathBuf {
    let capture = crate::recording_store::Capture {
        serial: 1,
        kind,
        key: "flight".into(),
        label: kind.title().into(),
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

struct Fakes {
    signals: Arc<Signals>,
    blocked: Arc<Mutex<Option<String>>>,
    notes: Arc<Mutex<Vec<String>>>,
}

fn env(manager: &'static Manager, router: Arc<Router>, root: &FsPath, rate: u64, leadup: bool) -> (Env, Fakes) {
    let signals = Arc::new(Signals::new(1_500));
    let blocked = Arc::new(Mutex::new(None::<String>));
    let notes = Arc::new(Mutex::new(Vec::new()));
    let (s, b, n) = (signals.clone(), blocked.clone(), notes.clone());
    let env = Env {
        manager,
        router,
        root: root.to_path_buf(),
        link: Link::Radio,
        rate,
        leadup,
        pause: Arc::new(move |_| b.lock().unwrap().clone().or_else(|| s.reason(now_ms()))),
        capture_pending: Arc::new(|| false),
        notify: Arc::new(move |m| n.lock().unwrap().push(m)),
        alive: Arc::new(|| true),
        rate_fn: None,
        poll: Duration::from_millis(20),
        backoff_unit_ms: 10,
        capture_wait_ms: 0,
        igt: false,
        igt_list_timeout: Duration::from_secs(2),
        outbox: false,
    };
    (env, Fakes { signals, blocked, notes })
}

fn new_job(root: &FsPath) -> FcJob {
    observe_at(root, FlightState::Recovery, 1);
    evaluate_at(root, true, Some(T0), Link::Radio, (T0 + 300_000) as u64).unwrap()
}

async fn wait_done(root: &FsPath, limit: Duration) -> FcJob {
    let deadline = Instant::now() + limit;
    loop {
        let job = load(root).jobs.last().cloned().unwrap();
        if job.done || Instant::now() > deadline {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

// ------------------------------------------------------------- trigger

#[test]
fn landed_must_hold_five_seconds_and_recovery_starts_at_once() {
    let root = TestDirectory::new();
    observe_at(&root.0, FlightState::InFlight, 0);
    observe_at(&root.0, FlightState::Landed, 10_000);
    assert!(evaluate_at(&root.0, true, None, Link::Radio, 14_999).is_none(), "not yet confirmed");
    observe_at(&root.0, FlightState::Landed, 12_000); // a republish keeps the edge time
    let job = evaluate_at(&root.0, true, None, Link::Radio, 15_000).expect("Landed confirmed starts the fetch");
    assert_eq!((job.trigger.as_str(), job.stage, job.done), ("landed_confirmed", Stage::List, false));
    // One job per flight: Recovery after it does not queue another.
    observe_at(&root.0, FlightState::Recovery, 16_000);
    assert!(evaluate_at(&root.0, true, None, Link::Radio, 16_001).is_none());
    assert_eq!(load(&root.0).jobs.len(), 1);
    assert!(launch_file_owed(&root.0));
}

#[test]
fn a_transient_landed_does_not_trigger() {
    let root = TestDirectory::new();
    observe_at(&root.0, FlightState::InFlight, 0);
    observe_at(&root.0, FlightState::Landed, 10_000);
    observe_at(&root.0, FlightState::InFlight, 12_000);
    for now in [15_000, 20_000, 60_000] {
        assert!(evaluate_at(&root.0, true, None, Link::Radio, now).is_none(), "{now}");
    }
    // Landing again restarts the 5 s hold.
    observe_at(&root.0, FlightState::Landed, 70_000);
    assert!(evaluate_at(&root.0, true, None, Link::Radio, 74_000).is_none());
    assert!(evaluate_at(&root.0, true, None, Link::Radio, 75_000).is_some());
}

#[test]
fn recovery_starts_it_and_the_setting_off_does_not_and_a_new_flight_rearms() {
    let root = TestDirectory::new();
    observe_at(&root.0, FlightState::Recovery, 1_000);
    assert!(evaluate_at(&root.0, false, None, Link::Radio, 1_001).is_none(), "setting off");
    assert!(load(&root.0).jobs.is_empty());
    let job = evaluate_at(&root.0, true, Some(5), Link::Wired, 1_002).expect("Recovery starts it");
    assert_eq!((job.trigger.as_str(), job.flight_from_ms, job.link.as_str()), ("recovery", Some(5), "wired"));
    modify(&root.0, |s| s.jobs[0].done = true);
    // Still Recovery: the latch holds until the FC flies again.
    assert!(evaluate_at(&root.0, true, None, Link::Radio, 9_000).is_none());
    observe_at(&root.0, FlightState::Armed, 10_000);
    observe_at(&root.0, FlightState::Recovery, 20_000);
    assert!(evaluate_at(&root.0, true, None, Link::Radio, 20_001).is_some(), "re-armed by the next flight");
}

#[tokio::test]
async fn fc_state_reports_reach_the_trigger() {
    let root = TestDirectory::new();
    let state = export_state(&root.0, true).await;
    let data = crate::recording_export::directory(&state);
    crate::fc_phase::observe(&state, FlightState::Descent as u8, crate::fc_phase::Source::FcStateReport, 0);
    crate::fc_phase::observe(&state, FlightState::Landed as u8, crate::fc_phase::Source::FcFlightState, 0);
    let now = now_ms();
    assert!(evaluate_at(&data, state.recording_store.settings().fc_sd_auto_collect, None, Link::Radio, now).is_none());
    assert!(evaluate_at(&data, true, None, Link::Radio, now + LANDED_CONFIRM_MS + 100).is_some());
    assert!(state.recording_store.settings().fc_sd_auto_collect && state.recording_store.settings().fc_sd_leadup);
}

#[test]
fn launch_file_choice_names_and_signals() {
    let f = |name: &str, open| RemoteFile { dir: "ROOT".into(), name: name.into(), size: 10, open };
    let files = vec![
        f("irec-1799990000-000.csv", false),
        f("irec_LAUNCH-1700000000-000.csv", false),
        f("irec_LAUNCH-1799999990-000.csv", false),
        f("irec_LAUNCH-1799999990-001.csv", true),
        f("irec_LAUNCH-1800090000-000.csv", false),
        f("notes.txt", false),
    ];
    let chosen = choose_launch_file(&files, Some(T0), T0 + 200_000).unwrap();
    assert_eq!(chosen.name, "irec_LAUNCH-1799999990-001.csv", "newest of this flight, by time then sequence");
    assert_eq!(choose_launch_file(&files[..2], Some(T0), T0).unwrap().name, "irec_LAUNCH-1700000000-000.csv");
    let undated = vec![f("irec_LAUNCH-734512-000.csv", false), f("irec_LAUNCH-912000-000.csv", false)];
    assert_eq!(choose_launch_file(&undated, None, T0).unwrap().name, "irec_LAUNCH-912000-000.csv", "last listed");
    assert!(choose_launch_file(&files[..1], None, T0).is_none());
    assert_eq!(name_utc_seconds("irec_LAUNCH-1799999990-001.csv"), Some(1_799_999_990));
    assert_eq!(name_utc_seconds("irec-734512-000.csv"), None, "uptime ms names are not dates");

    let s = Signals::new(10_000);
    assert!(s.reason(100_000).is_none());
    s.note_command(100_000);
    assert_eq!(s.reason(109_999).as_deref(), Some("commands were sent recently"));
    assert!(s.reason(110_000).is_none());
    // GPS: never arriving = never degraded; arriving then silent > 3 s = degraded.
    s.note_gps(200_000);
    assert!(s.gps_degraded(210_000).is_none(), "one fix is not a stream");
    s.note_gps(201_000);
    s.note_gps(202_000);
    assert!(s.gps_degraded(205_000).is_none());
    assert!(s.gps_degraded(205_001).unwrap().contains("recovery GPS"));
    assert!(s.gps_degraded(262_001).is_none(), "a GPS gone for a minute no longer holds the link");
    assert!(radio_decision(1000, 5, 0, 0).is_none());
    assert!(radio_decision(40, 10, 0, 0).unwrap().contains("poor"));
    assert!(radio_decision(1000, 0, 2048, 0).unwrap().contains("congested"));
    assert!(radio_decision(1000, 0, 0, 4).is_some());
}

#[test]
fn the_adaptive_rate_rises_while_calm_and_drops_at_once() {
    let mut rate = AdaptiveRate::new(512, 768);
    assert_eq!(rate.update(0, true), 512, "starts at the base");
    assert_eq!(rate.update(4_999, true), 512);
    assert_eq!(rate.update(5_000, true), 576);
    assert_eq!(rate.update(6_000, true), 576, "one step per 5 s");
    let mut now = 5_000;
    for _ in 0..10 {
        now += 5_000;
        rate.update(now, true);
    }
    assert_eq!(rate.update(now, true), 768, "capped at the ceiling");
    assert_eq!(rate.update(now + 1, false), 512, "any yield trigger: back to the base at once");
    assert_eq!(rate.update(now + 2, true), 512, "and the climb starts over");
    let s = Signals::new(10_000);
    assert!(s.calm(1_000));
    s.note_command(1_000);
    assert!(!s.calm(30_999), "a command keeps it slow for 30 s");
    assert!(s.calm(31_000));
    assert!(AdaptiveRate::new(512, 100).ceiling >= 512);
}

// -------------------------------------------------------------- transfer

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crc_mismatch_is_requested_again_and_counted() {
    let (gs, fc) = link();
    let data = pattern(5 * 512 + 33, 7);
    let board = Arc::new(MockFc::new(vec![("irec_LAUNCH-1-000.csv".into(), data.clone(), false)]));
    board.corrupt.store(2, Ordering::SeqCst);
    serve(&fc, board.clone());
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let root = TestDirectory::new();
    let listing = refresh_listing(manager, gs.clone(), Board::FlightComputer, Arc::new(|| None)).await.unwrap();
    assert_eq!(listing.files.len(), 1);
    assert_eq!(listing.files[0].dir, "ROOT");
    let (id, cancel) = manager.create_job(Board::FlightComputer, "ROOT", "irec_LAUNCH-1-000.csv").unwrap();
    let req = TransferRequest::new(id, Board::FlightComputer, 0, "irec_LAUNCH-1-000.csv", root.0.clone(), 10_000_000);
    let open: Gate = Arc::new(|| None);
    transfer(manager, gs.clone(), &req, &open, &cancel).await.unwrap();
    let (final_path, _) = local_paths(&root.0, Board::FlightComputer, "ROOT", "irec_LAUNCH-1-000.csv");
    assert_eq!(std::fs::read(final_path).unwrap(), data, "corrupt answers never reach the file");
    assert_eq!(req.crc_errors.load(Ordering::Relaxed), 2);
    assert_eq!(manager.get(id).unwrap().crc_errors, 2);
    assert_eq!(board.reads.load(Ordering::SeqCst), 6 + 2, "each corrupt block read once more");
    assert!(completed_file(&root.0, "FC", "ROOT", "irec_LAUNCH-1-000.csv").is_some());
    assert!(completed_file(&root.0, "FC", "LAUNCH", "irec_LAUNCH-1-000.csv").is_none());
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

// ---------------------------------------------------------- whole worker

/// (name, bytes, open) as the mock FC lists them.
type CardFile = (String, Vec<u8>, bool);

fn flight_files() -> (Vec<u8>, Vec<u8>, Vec<CardFile>) {
    let launch = fc_csv(T0 - 10_000, T0 + 20_000, 100);
    let ordinary = fc_csv(T0 - 610_000, T0 - 10_000, 100);
    let files = vec![
        (format!("irec-{}-000.csv", (T0 - 610_000) / 1000), ordinary.clone(), false),
        ("irec-1700000000-000.csv".into(), fc_csv(T0 - 9_000_000, T0 - 8_000_000, 1000), false),
        (launch_name(), launch.clone(), false),
    ];
    (launch, ordinary, files)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_collects_attaches_with_provenance_and_cuts_the_lead_up() {
    let root = TestDirectory::new();
    let folder = capture(&root.0, EventKind::Launch).await;
    let (launch, ordinary, files) = flight_files();
    let (gs, fc) = link();
    let board = Arc::new(MockFc::new(files));
    serve(&fc, board.clone());
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (env, fakes) = env(manager, gs.clone(), &root.0, 10_000_000, true);
    new_job(&root.0);
    tokio::time::timeout(Duration::from_secs(60), run(env)).await.unwrap();
    let job = load(&root.0).jobs.pop().unwrap();
    assert!(job.done, "{job:?}");
    assert_eq!((job.phase.as_str(), job.launch_file.as_deref()), ("complete", Some(launch_name().as_str())));
    let manifest = crate::recording_store::read_manifest(&folder).unwrap();
    let entry = manifest.board_sd.iter().find(|e| e.board == "FC").unwrap();
    assert_eq!((entry.source.as_str(), entry.link.as_deref(), entry.bytes), ("network_download", Some("radio"), launch.len() as u64));
    assert_eq!(entry.sha256, job.sha256.clone().unwrap());
    let format = entry.sd_format.as_ref().unwrap();
    assert_eq!((format.format.as_str(), format.version, format.board.as_str(), format.tick_hz), ("fc", 2, "FC", Some(1000)));
    assert_eq!(std::fs::read(folder.join("board_sd/FC").join(launch_name())).unwrap(), launch);
    // The lead-up: the minute before the launch file's first row, window only.
    assert_eq!(entry.lead_up_rows, 600, "{entry:?}");
    let lead = folder.join("board_sd/FC").join(format!("{}.leadup.csv", launch_name().trim_end_matches(".csv")));
    assert!(std::fs::read_to_string(lead).unwrap().contains("# seds-sd,format=fc"));
    let read_bytes = board.reads.load(Ordering::SeqCst) as u64 * BLOCK_BYTES;
    assert!(read_bytes < launch.len() as u64 + ordinary.len() as u64 / 2, "window only: {read_bytes}");
    let notes = fakes.notes.lock().unwrap().join("\n");
    assert!(notes.contains("attached to the Launch capture") && notes.contains("collection complete"), "{notes}");
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_rrls_keeps_verified_files_in_its_outbox_and_reports_power_off_ready() {
    let root = TestDirectory::new();
    capture(&root.0, EventKind::Launch).await;
    let (launch, _, files) = flight_files();
    let (gs, fc) = link();
    serve(&fc, Arc::new(MockFc::new(files)));
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (mut env, fakes) = env(manager, gs.clone(), &root.0, 10_000_000, true);
    env.outbox = true;
    let job = new_job(&root.0);
    tokio::time::timeout(Duration::from_secs(60), run(env)).await.unwrap();
    let outbox = relay::load_outbox(&root.0);
    assert_eq!(outbox.len(), 2, "{outbox:?}");
    let entry = outbox.iter().find(|e| e.kind == "launch").unwrap();
    assert_eq!((entry.bytes, entry.lead_up_resolved), (launch.len() as u64, true));
    assert_eq!(entry.sha256, board_sd::sha256_hex(&root.0.join("sd-downloads/FC/outbox").join(launch_name())).unwrap());
    assert!(outbox.iter().any(|e| e.kind == "lead_up" && e.name.ends_with(".leadup.csv")));
    assert!(fakes.notes.lock().unwrap().iter().any(|n| n.contains("received by the RRLS, checksums verified")));
    // Seen from the GS through the relay listing: ready before any forward.
    let relay_view = relay::RelayState { rrls_outbox: outbox, ..Default::default() };
    let pending = FcJob { triggered_at_ms: job.triggered_at_ms.min(now_ms() - 60_000), ..FcJob::default() };
    let power = relay::power_off_for(Some(&pending), true, &relay_view);
    assert_eq!((power.ready, power.at.as_deref()), (true, Some("RRLS")));
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test]
async fn attachment_follows_ground_test_and_hitl_captures_too() {
    for kind in [EventKind::GroundTest, EventKind::Hitl, EventKind::Launch] {
        let root = TestDirectory::new();
        let folder = capture(&root.0, kind).await;
        let path = root.0.join("sd-downloads/FC/ROOT").join(launch_name());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, fc_csv(T0 - 10_000, T0 + 20_000, 100)).unwrap();
        let provenance = board_sd::Provenance { source: "network_download".into(), link: Some("wired".into()), relay: None };
        let outcome = board_sd::offer_file_with(&root.0, &path, &launch_name(), &provenance).unwrap();
        assert_eq!(outcome, vec![board_sd::Outcome::Attached(folder.clone())], "{kind:?}");
        let manifest = crate::recording_store::read_manifest(&folder).unwrap();
        assert_eq!(manifest.board_sd[0].link.as_deref(), Some("wired"), "{kind:?}");
        assert_eq!(manifest.board_sd[0].sd_format.as_ref().unwrap().schema, "deadbeef");
    }
    // A headerless (older) file still attaches, without a format.
    let root = TestDirectory::new();
    let folder = capture(&root.0, EventKind::Launch).await;
    let path = root.0.join("old").join(launch_name());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let old = String::from_utf8(fc_csv(T0, T0 + 1000, 100)).unwrap().replace(FORMAT_LINE, "");
    std::fs::write(&path, old).unwrap();
    board_sd::offer_file(&root.0, &path, &launch_name(), "sd_import").unwrap();
    let manifest = crate::recording_store::read_manifest(&folder).unwrap();
    assert!(manifest.board_sd[0].sd_format.is_none() && manifest.board_sd[0].link.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pauses_for_commands_gps_and_operator_stop_then_resumes() {
    let root = TestDirectory::new();
    capture(&root.0, EventKind::Launch).await;
    let launch = fc_csv(T0 - 10_000, T0 + 50_000, 100);
    let (gs, fc) = link();
    let board = Arc::new(MockFc::new(vec![(launch_name(), launch.clone(), false)]));
    serve(&fc, board.clone());
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    // 20 KiB/s: a few seconds for the file, time to pause it in the middle.
    let (env, fakes) = env(manager, gs.clone(), &root.0, 20_000, false);
    let job = new_job(&root.0);
    let worker = tokio::spawn(run(env));
    let part = root.0.join("sd-downloads/FC/ROOT").join(format!("{}.part", launch_name()));
    let size = || std::fs::metadata(&part).map_or(0, |m| m.len());
    assert!(wait_for(Duration::from_secs(10), || size() >= 2048).await);
    let paused_with = |reason: &'static str| {
        let root = root.0.clone();
        move || load(&root).jobs.iter().any(|j| j.paused_reason.as_deref().is_some_and(|r| r.contains(reason)))
    };
    // Command traffic: paused for the quiet time, then resumes by itself.
    fakes.signals.note_command(now_ms());
    assert!(wait_for(Duration::from_secs(2), paused_with("commands")).await);
    let frozen = size();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(size(), frozen, "no reads during the command quiet time");
    assert!(wait_for(Duration::from_secs(5), || size() > frozen).await, "resumed after the quiet time");
    // Recovery GPS was arriving and stalls: paused until it is back.
    for i in 0..3 {
        fakes.signals.note_gps(now_ms() - 3_000 + i * 500);
    }
    assert!(wait_for(Duration::from_secs(3), paused_with("GPS")).await);
    let frozen = size();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(size(), frozen);
    fakes.signals.note_gps(now_ms());
    assert!(wait_for(Duration::from_secs(5), || size() > frozen).await, "resumed when GPS returned");
    // Operator stop: persisted, holds until resumed.
    set_stopped(&root.0, true, Some("operator".into()));
    assert!(wait_for(Duration::from_secs(3), paused_with("operator")).await);
    let frozen = size();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(size(), frozen);
    assert!(load(&root.0).stopped, "the stop is persisted");
    assert!(!launch_file_owed(&root.0), "a stopped collection does not hold the DAQ lead-up");
    // Any other block (e.g. congestion) also holds.
    *fakes.blocked.lock().unwrap() = Some("radio link congested".into());
    set_stopped(&root.0, false, None);
    assert!(wait_for(Duration::from_secs(3), paused_with("congested")).await);
    *fakes.blocked.lock().unwrap() = None;
    let done = wait_done(&root.0, Duration::from_secs(20)).await;
    assert!(done.done, "{done:?}");
    assert_eq!(done.id, job.id);
    tokio::time::timeout(Duration::from_secs(5), worker).await.unwrap().unwrap();
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_drop_and_a_restart_resume_from_the_persisted_offset() {
    let root = TestDirectory::new();
    capture(&root.0, EventKind::Hitl).await;
    let launch = fc_csv(T0 - 10_000, T0 + 4_000, 100);
    let (gs, fc) = link();
    let link_up = Arc::new(AtomicBool::new(true));
    let drop_link = link_up.clone();
    let mut mock = MockFc::new(vec![(launch_name(), launch.clone(), true)]);
    mock.files[0].2 = false;
    mock.trigger_after = 6;
    mock.on_read = Box::new(move || drop_link.store(false, Ordering::SeqCst));
    let board = Arc::new(mock);
    serve(&fc, board.clone());
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (mut first, _) = env(manager, gs.clone(), &root.0, 10_000_000, false);
    let up = link_up.clone();
    let inner = first.pause.clone();
    first.pause = Arc::new(move |stage| (!up.load(Ordering::SeqCst)).then(|| "FC unreachable".to_string()).or_else(|| inner(stage)));
    new_job(&root.0);
    let worker = tokio::spawn(run(first));
    let part = root.0.join("sd-downloads/FC/ROOT").join(format!("{}.part", launch_name()));
    assert!(wait_for(Duration::from_secs(10), || load(&root.0).jobs[0].paused_reason.as_deref() == Some("FC unreachable")).await);
    let kept = std::fs::metadata(&part).unwrap().len();
    assert!(kept > 0 && kept < launch.len() as u64 && kept.is_multiple_of(512), "kept {kept}");
    // The station restarts: the worker is gone, a new one (fresh manager)
    // picks the persisted job up and resumes after the kept bytes.
    worker.abort();
    let _ = worker.await;
    assert!(!worker_running(&root.0));
    let reads_before = board.reads.load(Ordering::SeqCst);
    link_up.store(true, Ordering::SeqCst);
    let manager2: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (second, _) = env(manager2, gs.clone(), &root.0, 10_000_000, false);
    tokio::time::timeout(Duration::from_secs(30), run(second)).await.unwrap();
    let job = load(&root.0).jobs.pop().unwrap();
    assert!(job.done, "{job:?}");
    let resumed = manager2.list().into_iter().next().unwrap();
    assert_eq!(resumed.resumed_from, kept, "resumed from the persisted offset");
    let blocks_after = (board.reads.load(Ordering::SeqCst) - reads_before) as u64;
    assert_eq!(blocks_after, (launch.len() as u64 - kept).div_ceil(512) + 1, "only the rest, plus the tail check");
    assert!(job.capture.as_deref().unwrap().contains("hitl"), "{job:?}");
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ground_test_image_without_the_service_ends_with_a_clear_status() {
    let root = TestDirectory::new();
    // The FC answers discovery but serves nothing on port 4511.
    let (gs, fc) = link();
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (mut env, fakes) = env(manager, gs.clone(), &root.0, 10_000_000, false);
    env.igt = true;
    new_job(&root.0);
    let finished = tokio::time::timeout(Duration::from_secs(60), run(env)).await;
    let job = load(&root.0).jobs.pop().unwrap();
    assert!(finished.is_ok(), "{job:?}");
    assert_eq!((job.done, job.phase.as_str()), (true, "unavailable"), "{job:?}");
    assert!(job.message.contains("not available in the ground-test image"), "{job:?}");
    assert_eq!(fakes.notes.lock().unwrap().len(), 1, "one clear notice");
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_to_the_rrls_path_and_back_resumes_the_direct_copy_from_its_offset() {
    use crate::sd_download::relay::{Path as Route, PathMetrics, choose_path};
    let root = TestDirectory::new();
    capture(&root.0, EventKind::Launch).await;
    let launch = fc_csv(T0 - 10_000, T0 + 50_000, 100);
    let (gs, fc) = link();
    let board = Arc::new(MockFc::new(vec![(launch_name(), launch.clone(), false)]));
    serve(&fc, board.clone());
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (mut env, _) = env(manager, gs.clone(), &root.0, 20_000, false);
    let good = PathMetrics { gs_direct_age_ms: Some(100), gs_good: 500, rrls_online: true, rrls_hears_av_bay: true, rrls_good: 500, rrls_can_transmit: true, ..Default::default() };
    let metrics = Arc::new(Mutex::new(good));
    let m = metrics.clone();
    env.pause = Arc::new(move |_| match choose_path(&m.lock().unwrap()) {
        (Route::Rrls, why) => Some(format!("collected through the RRLS: {why}")),
        (Route::Direct, _) => None,
    });
    new_job(&root.0);
    let worker = tokio::spawn(run(env));
    let part = root.0.join("sd-downloads/FC/ROOT").join(format!("{}.part", launch_name()));
    let size = || std::fs::metadata(&part).map_or(0, |m| m.len());
    assert!(wait_for(Duration::from_secs(10), || size() >= 4096).await);
    // The direct link degrades while the RRLS hears the AV bay cleanly.
    metrics.lock().unwrap().gs_direct_age_ms = Some(10_000);
    assert!(wait_for(Duration::from_secs(3), || load(&root.0).jobs[0].paused_reason.as_deref().is_some_and(|r| r.contains("RRLS"))).await);
    let kept = size();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(size(), kept, "the direct fetch yields to the RRLS path");
    // Direct is good again: the direct copy continues from its offset.
    metrics.lock().unwrap().gs_direct_age_ms = Some(100);
    let done = wait_done(&root.0, Duration::from_secs(20)).await;
    assert!(done.done, "{done:?}");
    let resumed = manager.list().into_iter().map(|j| j.resumed_from).max().unwrap();
    assert_eq!(resumed, kept, "resumed from the kept offset, not from zero");
    tokio::time::timeout(Duration::from_secs(5), worker).await.unwrap().unwrap();
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test]
async fn status_and_stop_routes_follow_the_sd_download_permissions() {
    use tower::ServiceExt;
    let root = TestDirectory::new();
    let state = export_state(&root.0, true).await;
    std::fs::write(
        root.0.join("auth.json"),
        serde_json::json!({"anonymous":{"view_data":true},"users":[
            {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}
        ]})
        .to_string(),
    )
    .unwrap();
    for (token, user, commands) in [("view-token", "viewer", 0), ("cmd-token", "operator", 1)] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms)VALUES(?,?,'session',1,?,0,9999999999999)")
            .bind(token).bind(user).bind(commands)
            .execute(&state.auth_db).await.unwrap();
    }
    let call = |method: &str, uri: &str, token: &str| {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let app = crate::sd_download::routes().with_state(state.clone());
    let viewed = app.clone().oneshot(call("GET", "/api/sd_download/fc", "view-token")).await.unwrap();
    assert_eq!(viewed.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(viewed.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!((body["enabled"].as_bool(), body["stopped"].as_bool()), (Some(true), Some(false)));
    assert_eq!(app.clone().oneshot(call("POST", "/api/sd_download/fc/stop", "view-token")).await.unwrap().status(), StatusCode::FORBIDDEN);
    let stopped = app.clone().oneshot(call("POST", "/api/sd_download/fc/stop", "cmd-token")).await.unwrap();
    assert_eq!(stopped.status(), StatusCode::OK);
    let data = crate::recording_export::directory(&state);
    assert!(load(&data).stopped);
    assert_eq!(load(&data).stopped_by.as_deref(), Some("operator"));
    assert_eq!(app.clone().oneshot(call("POST", "/api/sd_download/fc/resume", "cmd-token")).await.unwrap().status(), StatusCode::OK);
    assert!(!load(&data).stopped);
}

// ------------------------------------------------- shared radio link safety

/// A frame on the simulated AV-bay UART: router bytes, or a stand-in for
/// recovery GPS (FC/RF -> GS) or a command (GS -> FC).
enum Frame {
    Router(Vec<u8>),
    Gps(Instant),
    Command(Instant),
}

impl Frame {
    fn len(&self) -> usize {
        match self {
            Self::Router(b) => b.len() + 4,
            Self::Gps(_) => 48 + 4,
            Self::Command(_) => 40 + 4,
        }
    }
}

/// Turn-based radio: per direction at most 691 bytes per 200 ms turn, FIFO
/// (RF board section 4.3), so the whole budget is ~3.4 KB/s each way.
#[derive(Default)]
struct Air {
    down: Mutex<VecDeque<Frame>>,
    up: Mutex<VecDeque<Frame>>,
    stream_down_bytes: AtomicU64,
    stream_up_bytes: AtomicU64,
    gps_latency_ms: Mutex<Vec<u64>>,
    command_latency_ms: Mutex<Vec<u64>>,
}

const TURN_BYTES: usize = 691;
const TURN: Duration = Duration::from_millis(200);

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn on_a_shared_radio_budget_gps_and_commands_keep_rate_and_latency() {
    let root = TestDirectory::new();
    capture(&root.0, EventKind::Launch).await;
    let launch = fc_csv(T0 - 10_000, T0 + 1_500, 100);
    assert!(launch.len() > 3 * 512);
    let (gs, fc) = routers();
    let air = Arc::new(Air::default());
    let a = air.clone();
    let gs_side = gs.add_side_packed("radio", move |bytes| {
        a.up.lock().unwrap().push_back(Frame::Router(bytes.to_vec()));
        Ok(())
    });
    let a = air.clone();
    let fc_side = fc.add_side_packed("radio", move |bytes| {
        a.down.lock().unwrap().push_back(Frame::Router(bytes.to_vec()));
        Ok(())
    });
    let alive = Arc::new(AtomicBool::new(true));
    // Routers run continuously; the air delivers once per turn.
    let (g, f, live) = (gs.clone(), fc.clone(), alive.clone());
    let routers_task = tokio::spawn(async move {
        while live.load(Ordering::Relaxed) {
            let _ = g.process_all_queues();
            let _ = f.process_all_queues();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
    let (a, g, f, live) = (air.clone(), gs.clone(), fc.clone(), alive.clone());
    let air_task = tokio::spawn(async move {
        while live.load(Ordering::Relaxed) {
            for down in [true, false] {
                let queue = if down { &a.down } else { &a.up };
                let mut budget = TURN_BYTES;
                loop {
                    let frame = {
                        let mut q = queue.lock().unwrap();
                        match q.front() {
                            Some(fr) if fr.len() <= budget || budget == TURN_BYTES => q.pop_front(),
                            _ => None,
                        }
                    };
                    let Some(frame) = frame else { break };
                    budget = budget.saturating_sub(frame.len());
                    match frame {
                        Frame::Router(bytes) => {
                            if down {
                                a.stream_down_bytes.fetch_add(bytes.len() as u64 + 4, Ordering::Relaxed);
                                let _ = g.rx_packed_from_side(&bytes, gs_side);
                            } else {
                                a.stream_up_bytes.fetch_add(bytes.len() as u64 + 4, Ordering::Relaxed);
                                let _ = f.rx_packed_from_side(&bytes, fc_side);
                            }
                        }
                        // Delivered at the end of the burst of this turn.
                        Frame::Gps(at) => a.gps_latency_ms.lock().unwrap().push(at.elapsed().as_millis() as u64 + 60),
                        Frame::Command(at) => a.command_latency_ms.lock().unwrap().push(at.elapsed().as_millis() as u64 + 60),
                    }
                }
            }
            tokio::time::sleep(TURN).await;
        }
    });
    gs.announce_discovery().unwrap();
    fc.announce_discovery().unwrap();
    assert!(wait_for(Duration::from_secs(5), || gs.resolve_hostname("FC").is_some()).await, "discovery over the air");
    let board = Arc::new(MockFc::new(vec![(launch_name(), launch.clone(), false)]));
    serve(&fc, board.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let (env, fakes) = env(manager, gs.clone(), &root.0, RADIO_DEFAULT_BPS, false);
    let signals = fakes.signals.clone();
    new_job(&root.0);
    let worker = tokio::spawn(run(env));
    // Recovery GPS at 1 Hz from the vehicle, a command every 3 s from the GS.
    let started = Instant::now();
    let mut gps_sent = 0;
    let mut commands_sent = 0;
    let mut last_gps = Instant::now() - Duration::from_secs(1);
    let mut last_command = Instant::now();
    let mut quiet_checks = Vec::new();
    let part = root.0.join("sd-downloads/FC/ROOT").join(format!("{}.part", launch_name()));
    let size = || std::fs::metadata(&part).map_or(0, |m| m.len());
    while !load(&root.0).jobs[0].done && started.elapsed() < Duration::from_secs(40) {
        if last_gps.elapsed() >= Duration::from_secs(1) {
            last_gps = Instant::now();
            air.down.lock().unwrap().push_back(Frame::Gps(last_gps));
            signals.note_gps(now_ms());
            gps_sent += 1;
        }
        if last_command.elapsed() >= Duration::from_secs(3) && commands_sent < 3 {
            last_command = Instant::now();
            air.up.lock().unwrap().push_back(Frame::Command(last_command));
            signals.note_command(now_ms());
            commands_sent += 1;
            quiet_checks.push((Instant::now(), None::<u64>));
        }
        // The fetch yields: the partial file stays frozen 0.4..1.4 s after a command.
        for check in quiet_checks.iter_mut() {
            let age = check.0.elapsed();
            if check.1.is_none() && age >= Duration::from_millis(400) {
                check.1 = Some(size());
            }
            if let Some(at) = check.1
                && age >= Duration::from_millis(1_400)
                && age < Duration::from_millis(1_450)
            {
                assert_eq!(size(), at, "the fetch yields for the command quiet time");
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let job = load(&root.0).jobs[0].clone();
    alive.store(false, Ordering::Relaxed);
    let _ = worker.await;
    let _ = routers_task.await;
    let _ = air_task.await;
    assert!(job.done, "{job:?}");
    let gps = air.gps_latency_ms.lock().unwrap().clone();
    let commands = air.command_latency_ms.lock().unwrap().clone();
    // Every GPS fix and command got through, each within two turns.
    assert!(gps.len() + 1 >= gps_sent, "{} of {gps_sent} GPS fixes", gps.len());
    assert_eq!(commands.len(), commands_sent);
    let worst_gps = gps.iter().max().copied().unwrap_or(0);
    let worst_command = commands.iter().max().copied().unwrap_or(0);
    assert!(worst_gps <= 2 * TURN.as_millis() as u64 + 80, "GPS latency {gps:?}");
    assert!(worst_command <= 2 * TURN.as_millis() as u64 + 80, "command latency {commands:?}");
    // The fetch kept well inside the budget (payload <= 512 B/s on average).
    let down = air.stream_down_bytes.load(Ordering::Relaxed);
    let up = air.stream_up_bytes.load(Ordering::Relaxed);
    let blocks = launch.len().div_ceil(512) as u64;
    println!(
        "FC SD over the simulated radio: {} B payload in {elapsed:.1} s; wire {down} B down + {up} B up ({} + {} B per 512 B block incl. discovery); GPS worst {worst_gps} ms, commands worst {worst_command} ms",
        launch.len(),
        down / blocks,
        up / blocks
    );
    assert!((launch.len() as f64 / elapsed) <= RADIO_DEFAULT_BPS as f64 * 1.1);
    assert!((down as f64 / elapsed) < 0.35 * (TURN_BYTES as f64 * 5.0), "{down} B down in {elapsed:.1} s");
}

/// Wire cost of the protocol itself (no radio losses or retransmissions):
/// SEDSnet packed bytes per 512-byte block each way, plus the 4-byte radio
/// frame header per packet. Printed for the docs' transfer-time estimate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_bytes_per_block_on_an_instant_link() {
    let (gs, fc) = routers();
    let down = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    let up = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    let fc_side_slot = Arc::new(std::sync::OnceLock::new());
    let gs_side_slot = Arc::new(std::sync::OnceLock::new());
    let (f, u, slot) = (fc.clone(), up.clone(), fc_side_slot.clone());
    let gs_side = gs.add_side_packed("wire", move |b| {
        u.0.fetch_add(b.len() as u64 + 4, Ordering::Relaxed);
        u.1.fetch_add(1, Ordering::Relaxed);
        f.rx_packed_from_side(b, *slot.get().unwrap())
    });
    let (g, d, slot) = (gs.clone(), down.clone(), gs_side_slot.clone());
    let fc_side = fc.add_side_packed("wire", move |b| {
        d.0.fetch_add(b.len() as u64 + 4, Ordering::Relaxed);
        d.1.fetch_add(1, Ordering::Relaxed);
        g.rx_packed_from_side(b, *slot.get().unwrap())
    });
    fc_side_slot.set(fc_side).unwrap();
    gs_side_slot.set(gs_side).unwrap();
    gs.announce_discovery().unwrap();
    fc.announce_discovery().unwrap();
    for _ in 0..5 {
        gs.process_all_queues().unwrap();
        fc.process_all_queues().unwrap();
    }
    assert!(gs.resolve_hostname("FC").is_some());
    let data = pattern(64 * 512, 3);
    let board = Arc::new(MockFc::new(vec![(launch_name(), data.clone(), false)]));
    serve(&fc, board);
    let (running, task) = pump(gs.clone(), fc.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let root = TestDirectory::new();
    let open: Gate = Arc::new(|| None);
    let (id, cancel) = manager.create_job(Board::FlightComputer, "ROOT", &launch_name()).unwrap();
    let req = TransferRequest::new(id, Board::FlightComputer, 0, &launch_name(), root.0.clone(), 10_000_000);
    // Count only the transfer (after connect).
    let mut stream = BoardStream::open(manager, gs.clone(), Board::FlightComputer, &open, &cancel).await.unwrap();
    let (d0, dn0, u0, un0) = (down.0.load(Ordering::Relaxed), down.1.load(Ordering::Relaxed), up.0.load(Ordering::Relaxed), up.1.load(Ordering::Relaxed));
    for block in 0..64u64 {
        read_block(&mut stream, &req, block * 512, &open, &cancel).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let per = |a: u64, b: u64| (a - b) as f64 / 64.0;
    let (down_b, down_n) = (per(down.0.load(Ordering::Relaxed), d0), per(down.1.load(Ordering::Relaxed), dn0));
    let (up_b, up_n) = (per(up.0.load(Ordering::Relaxed), u0), per(up.1.load(Ordering::Relaxed), un0));
    println!("FC SD wire per 512 B block: down {down_b:.0} B in {down_n:.1} packets, up {up_b:.0} B in {up_n:.1} packets");
    // 6 DATA frames (11 B header each + CRC) plus SEDSnet packet headers and
    // the stream ACK; the docs' radio estimate uses these numbers.
    assert!(down_b > 512.0 + 6.0 * 11.0 && down_b < 1400.0, "{down_b}");
    assert!(up_b < 600.0, "{up_b}");
    stream.finish(false);
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}
