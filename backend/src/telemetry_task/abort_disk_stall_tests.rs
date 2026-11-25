//! An operator Abort pressed while a Launch is being saved on a disk that has
//! stopped answering. The production Launch handler, Abort publisher, router
//! worker, HTTP routes and WebSocket run on a two-worker runtime, the size of
//! a starved Pi; only the disk is a test seam (`disk_stall`).
use super::held_launch_tests::immediate_on;
use super::launch_commit_proof_tests::{PAIR, prove};
use super::*;
use crate::disk_stall::{Site, Stall};
use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};
use std::io::{Read, Write};
use std::time::Instant;

/// How long the disk answers nothing after the press.
const STALL: Duration = Duration::from_millis(2_500);
/// Longest any part of the Abort may take. Far below the stall, above the
/// scheduling noise of a busy build host.
const BOUND: Duration = Duration::from_millis(600);

struct Rig {
    state: Arc<AppState>,
    router: Arc<Router>,
    /// What left on the wire, in order.
    wire: Arc<Mutex<Vec<(Instant, &'static str, u64)>>>,
    side: usize,
    /// When the router last handed a board packet to the station.
    heard: Arc<Mutex<Instant>>,
    http: std::net::SocketAddr,
    peers: crate::ignition_pair::fixture::Peers,
    _root: crate::recording_export::tests::TestDirectory,
    _commands: mpsc::Receiver<TelemetryCommand>,
    _services: Vec<tokio::task::JoinHandle<()>>,
    _drain: tokio::sync::oneshot::Sender<()>,
}

async fn rig() -> Rig {
    let (root, mut state, commands, _) = crate::web::command_dispatch_tests::preauth_fixture().await;
    // The session database of a real station: a file, in WAL mode.
    let sessions = root.0.join("users.db");
    std::fs::write(&sessions, b"").unwrap();
    let sessions = sqlx::SqlitePool::connect(&format!("sqlite://{}", sessions.display())).await.unwrap();
    crate::telemetry_db::apply_sqlite_pragmas(&sessions).await;
    crate::ensure_auth_sessions_table(&sessions).await.unwrap();
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('preauth-test','operator','session',1,1,0,9999999999999)")
        .execute(&sessions)
        .await
        .unwrap();
    Arc::get_mut(&mut state).unwrap().auth_db = sessions;
    let control = state.clone();
    let observed = state.clone();
    let heard = Arc::new(Mutex::new(Instant::now()));
    let served = heard.clone();
    let reports = UmbilicalReports::start(&state).unwrap();
    let mut config = RouterConfig::new([
        // The production GROUND_STATION handler's board-report path.
        crate::ignition_pair::fixture::handler(&state, move |packet| {
            if packet.data_type() == crate::telemetry_schema::data_type("UMBILICAL_STATUS")
                && let Some(capture) = capture_umbilical_status_at_ingress(&control, packet)
            {
                reports.receive(&control, packet, capture);
            } else if packet.data_type() == crate::telemetry_schema::data_type("BATTERY_VOLTAGE") {
                *served.lock().unwrap() = Instant::now();
            }
            Ok(())
        }),
        EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("ABORT"), move |packet| {
            crate::deferred_abort::observe(&observed, packet, crate::gse::automatic_generation(&observed));
            Ok(())
        }),
    ]);
    config = config.with_sender("GS");
    for name in ["ABORT", "FLIGHT_COMMAND"] {
        sedsnet::set_type_scheduling_class(crate::telemetry_schema::data_type(name), -1);
    }
    let router = Arc::new(Router::new(config));
    crate::network_variables::initialize(&router).unwrap();
    let wire = Arc::new(Mutex::new(Vec::new()));
    let sent = wire.clone();
    let side = router.add_side_packed_with_options(
        "rocket_comms",
        move |bytes| {
            if let Ok(packet) = sedsnet::wire_format::unpack_packet(bytes) {
                let ty = packet.data_type();
                let name = if ty == crate::telemetry_schema::data_type("ABORT")
                    && packet.payload().starts_with(commands::MANUAL_ABORT_PREFIX.as_bytes())
                {
                    "ABORT"
                } else if ty == crate::telemetry_schema::data_type("FLIGHT_STATE") && packet.payload() == [15] {
                    "FLIGHT_STATE"
                } else if ty == crate::telemetry_schema::data_type("VALVE_COMMAND")
                    && packet.payload() == [ValveBoardCommands::Sequence as u8]
                {
                    "LAUNCH"
                } else {
                    return Ok(());
                };
                sent.lock().unwrap().push((Instant::now(), name, packet.packet_id()));
            }
            Ok(())
        },
        RouterSideOptions { reliable_enabled: false, ..Default::default() },
    );
    let announce = sedsnet::discovery::build_discovery_announce(
        "WIRE",
        123,
        &["ABORT", "FLIGHT_STATE", "VALVE_BOARD"].map(crate::telemetry_schema::endpoint),
    )
    .unwrap();
    router.rx_packed_from_side(&serialize::pack_packet(&announce), side).unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    if !crate::runtime_mode::is_hitl() {
        let _ = crate::gse::armed_fixture_for_test(&state, &router);
    } else {
        state.set_local_flight_state(FlightState::Idle);
        for index in 0..4 {
            if state.hitl_safety_interlock_enabled(index) {
                state.toggle_hitl_safety_interlock(index);
            }
        }
        state.topology_router.set(router.clone()).unwrap();
    }
    let peers = crate::ignition_pair::fixture::Peers::warm(&state, router.clone()).await;
    let now = get_current_timestamp_ms() as i64;
    *state.launch_history.lock().unwrap() =
        crate::launch_history::Store::open(root.0.join("launch-clock.json"), false, None, now, 0).unwrap();

    let (deferred, drain) = crate::deferred_abort::spawn(state.clone());
    drop(deferred);
    let mut services = vec![
        tokio::spawn(crate::board_abort::query_service(state.clone(), state.shutdown_subscribe())),
    ];
    crate::manual_abort_publisher::spawn(state.clone(), router.clone()).unwrap();
    spawn_router_worker_thread(router.clone(), state.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let app = crate::web::router(state.clone(), "test".into());
    services.push(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }));
    Rig { state, router, wire, side, heard, http, peers, _root: root, _commands: commands, _services: services, _drain: drain }
}

/// One HTTP request from outside the station's runtime: the status and how
/// long it took.
fn http(address: std::net::SocketAddr, method: &str, path: &str, body: &str) -> (u16, Duration) {
    let started = Instant::now();
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: station\r\nAuthorization: Bearer preauth-test\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut head = [0; 12];
    let _ = stream.read_exact(&mut head);
    let status = std::str::from_utf8(&head[9..12]).ok().and_then(|text| text.parse().ok()).unwrap_or(0);
    (status, started.elapsed())
}

/// The requests a few open dashboards and an impatient operator make.
const DASHBOARD: &[&str] = &[
    "/api/board-recovery/status", "/api/action_policy", "/api/boards", "/api/board_abort", "/api/launch_clock",
    "/api/mission_phase", "/api/network_topology", "/api/notifications", "/api/recent", "/api/recording_status",
    "/api/fill_targets", "/api/flight_setup", "/api/auto_zero", "/api/alert_conditions", "/api/calibration",
    "/api/network_diagnostics", "/flightstate", "/valvestate", "/api/rrls/status",
];
const PRESSES: &[&str] = &["Launch", "Dump", "NormallyOpen", "Pilot", "PauseFill", "ResetTClock"];

fn dashboards(address: std::net::SocketAddr) {
    for path in DASHBOARD {
        std::thread::spawn(move || http(address, "GET", path, ""));
    }
    for name in PRESSES {
        std::thread::spawn(move || http(address, "POST", "/api/command", &format!("\"{name}\"")));
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Load {
    /// Nothing but the Launch and the Abort.
    Quiet,
    /// Dashboards, other presses and board output reports, all of which wait
    /// on what the Launch holds.
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Press {
    Http,
    /// Over a WebSocket that already carried another press.
    WebSocket,
    Gpio,
    /// Relayed by an RRLS (`dispatch_proxied_command`).
    Proxied,
}

struct Presser {
    principal: crate::auth::AuthPrincipal,
    press: Press,
    socket: Option<std::sync::mpsc::Sender<&'static str>>,
}
impl Presser {
    async fn new(press: Press, rig: &Rig) -> Self {
        let principal = rig
            .state
            .auth
            .authorize_token(&rig.state.auth_db, Some("preauth-test"), crate::auth::Permission::SendCommands)
            .await
            .unwrap_or_else(|_| panic!("operator session"));
        let mut socket = None;
        if press == Press::WebSocket {
            let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
            let (open, opened) = std::sync::mpsc::channel();
            let url = format!("ws://{}/ws?token=preauth-test", rig.http);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                runtime.block_on(async move {
                    use futures::{SinkExt, StreamExt};
                    let (stream, _) = tokio_tungstenite::connect_async(url).await.unwrap();
                    let (mut sink, mut source) = stream.split();
                    tokio::spawn(async move { while source.next().await.is_some() {} });
                    open.send(()).unwrap();
                    loop {
                        match rx.try_recv() {
                            Ok(name) => sink
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    format!("{{\"cmd\":\"{name}\"}}").into(),
                                ))
                                .await
                                .unwrap(),
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                            Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
                        }
                    }
                });
            });
            opened.recv_timeout(Duration::from_secs(10)).expect("WebSocket opened before the stall");
            socket = Some(tx);
        } else if press == Press::Gpio {
            crate::gpio_panel::setup_gpio_panel(rig.state.clone()).unwrap();
        }
        Self { principal, press, socket }
    }
    /// Presses before the Abort on the same connection, where there is one:
    /// the first waits for the dispatch gate, the second behind it.
    fn earlier(&self) {
        if let Some(socket) = &self.socket {
            socket.send("Dump").unwrap();
            socket.send("Launch").unwrap();
        }
    }
    fn abort(&self, rig: &Rig) {
        match self.press {
            Press::Http => {
                let address = rig.http;
                std::thread::spawn(move || {
                    let (status, took) = http(address, "POST", "/api/command", "\"Abort\"");
                    eprintln!("HTTP Abort answered {status} in {took:?}");
                });
            }
            Press::WebSocket => self.socket.as_ref().unwrap().send("Abort").unwrap(),
            Press::Gpio => {
                let state = rig.state.clone();
                std::thread::spawn(move || state.gpio.trigger_input_for_test(crate::gpio_panel::ABORT_PIN, true));
            }
            Press::Proxied => {
                let (state, principal) = (rig.state.clone(), self.principal.clone());
                tokio::spawn(async move {
                    crate::web::dispatch_proxied_command(state, principal, TelemetryCommand::Abort, None).await
                });
            }
        }
    }
}

#[derive(Debug, Default)]
struct Latency {
    abort_on_wire: Option<Duration>,
    flight_state_on_wire: Option<Duration>,
    resend_armed: Option<Duration>,
    station_aborted: Option<Duration>,
    readback_requested: Option<Duration>,
    second_abort_on_wire: Option<Duration>,
    /// Longest the router handed no board packet to the station.
    longest_rx_silence: Duration,
    /// The pressed Launch reached the Valve Board after all.
    launch_on_wire: bool,
}

/// Board output reports keep arriving, as from a live Valve Board.
fn feed_reports(rig: &Rig, outputs: bool) -> Arc<std::sync::atomic::AtomicBool> {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (router, side, done) = (rig.router.clone(), rig.side, stop.clone());
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            let report = Packet::new(
                crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "VB",
                router.network_time_ms().unwrap_or(0),
                Arc::from([ValveBoardCommands::NormallyOpenOpen as u8, 0]),
            )
            .unwrap();
            if outputs {
                let _ = router.rx_queue_from_side(report, side);
            }
            // The flight computer's own state report, which is saved.
            let phase = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("FLIGHT_STATE")],
                "FC",
                router.network_time_ms().unwrap_or(0),
                Arc::from([FlightState::Idle as u8]),
            )
            .unwrap();
            if outputs {
                let _ = router.rx_queue_from_side(phase, side);
            }
            let other = Packet::new(
                crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "VB",
                router.network_time_ms().unwrap_or(0),
                Arc::from(12.0f32.to_le_bytes()),
            )
            .unwrap();
            let _ = router.rx_queue_from_side(other, side);
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    stop
}

async fn measure(site: Site, press: Press, load: Load) -> Latency {
    if site == Site::Log {
        let files = crate::logger::Queued::start(crate::logger::queue_tests::Files::default()).unwrap();
        log::set_boxed_logger(Box::new(files)).unwrap();
        log::set_max_level(log::LevelFilter::Info);
    }
    let rig = rig().await;
    let presser = Presser::new(press, &rig).await;
    let reports = feed_reports(&rig, load == Load::Busy);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stall = Stall::begin(site);
    let launch = {
        let (state, router) = (rig.state.clone(), rig.router.clone());
        tokio::spawn(async move {
            // A stalled log file needs no Launch.
            if site == Site::Log {
                log::info!("held by the log file");
            } else {
                // Nothing pumps the peers now: no query sent since the warm-up
                // is answered.
                prove(&state, &PAIR, Duration::ZERO);
                immediate_on(&state, &router).await;
            }
        })
    };
    let rig = Arc::new(rig);
    // From here on the operator is a thread of its own: the station's two
    // workers are whatever the stall leaves of them.
    let operator = {
        let rig = rig.clone();
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let _runtime = handle.enter();
            assert!(stall.entered(), "the Launch never reached its {site:?} save: {:?}",
                rig.state.notifications_snapshot().iter().map(|n| n.message.clone()).collect::<Vec<_>>());
            let cohort = rig.state.manual_abort_intent.epoch();
            if load == Load::Busy {
                dashboards(rig.http);
                presser.earlier();
            }
            std::thread::sleep(Duration::from_millis(400));
            let pressed = Instant::now();
            presser.abort(&rig);
            let mut seen = Latency::default();
            let mut second = None;
            let query = crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST");
            let queries = || {
                rig.peers
                    .pending_transport_wires()
                    .iter()
                    .filter_map(|wire| sedsnet::wire_format::unpack_packet(wire).ok())
                    .filter(|packet| packet.data_type() == query)
                    .map(|packet| packet.packet_id())
                    .collect::<std::collections::HashSet<_>>()
            };
            let earlier_queries = queries();
            while pressed.elapsed() < STALL {
                let now = pressed.elapsed();
                let wire = rig.wire.lock().unwrap().clone();
                let first = |name| wire.iter().find(|(at, kind, _)| *kind == name && *at >= pressed).map(|(at, ..)| *at - pressed);
                seen.abort_on_wire = seen.abort_on_wire.or(first("ABORT"));
                seen.flight_state_on_wire = seen.flight_state_on_wire.or(first("FLIGHT_STATE"));
                if rig.state.abort_resend.active_resends().is_some() {
                    seen.resend_armed.get_or_insert(now);
                }
                if rig.state.state.try_lock().is_ok_and(|state| *state == FlightState::Aborted)
                    && rig.state.abort_indicator_latched()
                {
                    seen.station_aborted.get_or_insert(now);
                }
                if seen.readback_requested.is_none() && !queries().is_subset(&earlier_queries) {
                    seen.readback_requested = Some(now);
                }
                // The second press, as soon as the GPIO debounce allows one.
                if press_twice(press, load) && second.is_none() && now >= Duration::from_millis(700) {
                    second = Some((Instant::now(), wire.iter().map(|(.., id)| *id).collect::<Vec<_>>()));
                    presser.abort(&rig);
                }
                // A new packet, not a re-send of the first one.
                if let Some((at, before)) = &second
                    && seen.second_abort_on_wire.is_none()
                    && rig.state.manual_abort_intent.epoch() >= cohort + 2
                    && let Some((sent, ..)) =
                        wire.iter().find(|(_, kind, id)| *kind == "ABORT" && !before.contains(id))
                {
                    seen.second_abort_on_wire = Some(sent.saturating_duration_since(*at));
                }
                let silent = rig.heard.lock().unwrap().elapsed();
                seen.longest_rx_silence = seen.longest_rx_silence.max(silent);
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(stall);
            // Its WebSocket stays open: a closed session drops its frames.
            (seen, presser)
        })
    };
    let (mut seen, _presser) = tokio::task::spawn_blocking(move || operator.join().unwrap()).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), launch).await.expect("the Launch handler returns");
    reports.store(true, Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(10), async {
        while rig.state.manual_abort_intent.pending_epoch().is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Abort is recorded once the disk answers");
    tokio::time::sleep(Duration::from_millis(200)).await;
    seen.launch_on_wire = rig.wire.lock().unwrap().iter().any(|(_, kind, _)| *kind == "LAUNCH");
    if press == Press::WebSocket && load == Load::Busy && site != Site::Log {
        let overtaken = "Launch not sent: Abort was pressed after it";
        tokio::time::timeout(Duration::from_secs(10), async {
            while !rig.state.notifications_snapshot().iter().any(|note| note.message.contains(overtaken)) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the Launch the Abort overtook is not sent");
    }
    // What the disk kept, and what the station is.
    assert_eq!(rig.state.local_flight_state_snapshot(), FlightState::Aborted);
    assert!(rig.state.abort_indicator_latched());
    assert_eq!(crate::network_variables::flight_state(), FlightState::Aborted as u8);
    if site != Site::Log {
        assert!(rig.state.launch_mode_gate.launch_hold().is_none(), "an aborted Launch holds no mode change");
        assert!(rig.state.launch_history.lock().unwrap().launch_locked(), "a saved attempt is never released here");
    }
    rig.state.request_shutdown();
    seen
}

/// One press must leave the station in order too.
fn press_twice(press: Press, load: Load) -> bool {
    press != Press::Proxied || load != Load::Quiet
}

fn check(site: Site, press: Press, load: Load, seen: &Latency) {
    eprintln!("ABORT_DISK_STALL site={site:?} press={press:?} load={load:?} {seen:?}");
    let late = [
        ("ABORT on the wire", seen.abort_on_wire),
        ("Aborted FLIGHT_STATE on the wire", seen.flight_state_on_wire),
        ("re-send armed", seen.resend_armed),
        ("station Aborted", seen.station_aborted),
        ("second ABORT on the wire", seen.second_abort_on_wire),
        ("board readback requested", seen.readback_requested),
        ("board packets received", Some(seen.longest_rx_silence)),
    ]
    .into_iter()
    .filter(|(name, _)| press_twice(press, load) || !name.starts_with("second"))
    .filter(|(_, at)| at.is_none_or(|at| at > BOUND))
    .collect::<Vec<_>>();
    // The variables file is written after the SDK owns the Launch.
    assert!(site == Site::Variables || !seen.launch_on_wire, "an Abort during the save invalidates the Launch");
    assert!(late.is_empty(), "{site:?} {press:?} {load:?}: later than {BOUND:?} (None: not before the disk answered): {late:?}");
}

macro_rules! stalled {
    ($($name:ident: $site:ident, $press:ident, $load:ident;)+) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            // HITL saves only the journal.
            if Site::$site == Site::Journal {
                crate::mode_suite!(Normal, Hitl);
            } else {
                crate::mode_suite!(Normal);
            }
            if crate::operator_queue::isolated_native_case(module_path!(), stringify!($name)) {
                return;
            }
            let seen = measure(Site::$site, Press::$press, Load::$load).await;
            check(Site::$site, Press::$press, Load::$load, &seen);
        }
    )+};
}
stalled! {
    qualification_http_quiet: Qualification, Http, Quiet;
    qualification_http: Qualification, Http, Busy;
    qualification_websocket_quiet: Qualification, WebSocket, Quiet;
    qualification_websocket: Qualification, WebSocket, Busy;
    qualification_gpio_quiet: Qualification, Gpio, Quiet;
    qualification_gpio: Qualification, Gpio, Busy;
    qualification_proxied_quiet: Qualification, Proxied, Quiet;
    qualification_proxied: Qualification, Proxied, Busy;
    journal_http_quiet: Journal, Http, Quiet;
    journal_http: Journal, Http, Busy;
    journal_websocket_quiet: Journal, WebSocket, Quiet;
    journal_websocket: Journal, WebSocket, Busy;
    journal_gpio_quiet: Journal, Gpio, Quiet;
    journal_gpio: Journal, Gpio, Busy;
    journal_proxied_quiet: Journal, Proxied, Quiet;
    journal_proxied: Journal, Proxied, Busy;
    checklists_http_quiet: Checklists, Http, Quiet;
    checklists_http: Checklists, Http, Busy;
    checklists_websocket_quiet: Checklists, WebSocket, Quiet;
    checklists_websocket: Checklists, WebSocket, Busy;
    checklists_gpio_quiet: Checklists, Gpio, Quiet;
    checklists_gpio: Checklists, Gpio, Busy;
    checklists_proxied_quiet: Checklists, Proxied, Quiet;
    checklists_proxied: Checklists, Proxied, Busy;
    variables_http_quiet: Variables, Http, Quiet;
    variables_http: Variables, Http, Busy;
    variables_websocket_quiet: Variables, WebSocket, Quiet;
    variables_websocket: Variables, WebSocket, Busy;
    variables_gpio_quiet: Variables, Gpio, Quiet;
    variables_gpio: Variables, Gpio, Busy;
    variables_proxied_quiet: Variables, Proxied, Quiet;
    variables_proxied: Variables, Proxied, Busy;
    log_http: Log, Http, Busy;
    log_websocket: Log, WebSocket, Busy;
    log_gpio: Log, Gpio, Busy;
    log_proxied: Log, Proxied, Busy;
}

/// Sending an Abort runs no received packet's handler: those wait on station
/// state, and the Abort leaves first.
#[test]
fn sending_an_abort_runs_no_received_packet() {
    crate::telemetry_schema::initialize().unwrap();
    let handled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = handled.clone();
    let station = crate::telemetry_schema::endpoint("GROUND_STATION");
    let router = Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(station, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })])
        .with_sender("GS"),
    );
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = sent.clone();
    let side = router.add_side_packed_with_options(
        "rocket_comms",
        move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        RouterSideOptions { reliable_enabled: false, ..Default::default() },
    );
    let abort = crate::telemetry_schema::endpoint("ABORT");
    router.rx_from_side(&sedsnet::discovery::build_discovery_announce("VB", 1, &[abort]).unwrap(), side).unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    sent.store(0, Ordering::SeqCst);
    let report = Packet::new(
        crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
        &[station],
        "VB",
        2,
        Arc::from([ValveBoardCommands::NormallyOpenOpen as u8, 0]),
    )
    .unwrap();
    router.rx_queue_from_side(report, side).unwrap();
    // To the boards only: the station's own copy is not a received packet.
    let stop = Packet::new(crate::telemetry_schema::data_type("ABORT"), &[abort], "GS", 3, Arc::from(&b"stop"[..])).unwrap();
    router.tx_queue(stop).unwrap();
    dispatch_abort_tx(&router).unwrap();
    assert!(sent.load(Ordering::SeqCst) > 0, "the Abort left");
    assert_eq!(handled.load(Ordering::SeqCst), 0, "and the received report still waits for the router worker");
    router.process_all_queues_with_timeout(0).unwrap();
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

/// The Launch an Abort ended while it was being saved holds no mode change,
/// though it is marked as sent only after that Abort released the hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_launch_holds_no_mode_change() {
    crate::mode_suite!(Normal);
    if crate::operator_queue::isolated_native_case(module_path!(), "an_aborted_launch_holds_no_mode_change") {
        return;
    }
    let rig = super::held_launch_tests::rig_with(false, None).await;
    let stall = Stall::begin(Site::Qualification);
    let launch = {
        let (state, router) = (rig.state.clone(), rig.router.clone());
        prove(&state, &PAIR, Duration::ZERO);
        tokio::spawn(async move { immediate_on(&state, &router).await })
    };
    tokio::task::block_in_place(|| assert!(stall.entered()));
    rig.state.manual_abort_intent.request();
    let mut claim = rig.state.manual_abort_intent.try_claim().unwrap();
    let published = crate::manual_abort_utc::publish_local_claim_unrecorded(&rig.state, &rig.router, &mut claim, |_| {});
    assert!(published.unwrap().is_some());
    drop(claim);
    assert_eq!(rig.state.local_flight_state_snapshot(), FlightState::Aborted);
    drop(stall);
    launch.await.unwrap();
    assert!(!rig.state.launch_indicator_latched());
    assert_eq!(rig.state.launch_mode_gate.launch_hold(), None);
    assert!(rig.state.launch_history.lock().unwrap().launch_locked());
    rig.state.request_shutdown();
}

/// An Abort heard from another station while a Launch is being saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qualification_observed() {
    crate::mode_suite!(Normal);
    if crate::operator_queue::isolated_native_case(module_path!(), "qualification_observed") {
        return;
    }
    let rig = rig().await;
    let stall = Stall::begin(Site::Qualification);
    let launch = {
        let (state, router) = (rig.state.clone(), rig.router.clone());
        prove(&state, &PAIR, Duration::ZERO);
        tokio::spawn(async move { immediate_on(&state, &router).await })
    };
    let (state, router, side) = (rig.state.clone(), rig.router.clone(), rig.side);
    let latched = tokio::task::spawn_blocking(move || {
        assert!(stall.entered());
        let heard = Instant::now();
        let abort = Packet::new(
            crate::telemetry_schema::data_type("ABORT"),
            &[crate::telemetry_schema::endpoint("ABORT")],
            "RRLS",
            router.network_time_ms().unwrap(),
            Arc::from(commands::MANUAL_ABORT_PREFIX.as_bytes()),
        )
        .unwrap();
        router.rx_queue_from_side(abort, side).unwrap();
        let mut latched = None;
        while heard.elapsed() < STALL {
            if state.abort_indicator_latched() {
                latched.get_or_insert(heard.elapsed());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(stall);
        latched
    })
    .await
    .unwrap();
    eprintln!("ABORT_DISK_STALL site=Qualification press=Observed latched={latched:?}");
    tokio::time::timeout(Duration::from_secs(10), launch).await.expect("the Launch handler returns").unwrap();
    assert!(latched.is_some_and(|at| at < BOUND), "{latched:?}");
    assert!(!rig.wire.lock().unwrap().iter().any(|(_, kind, _)| *kind == "LAUNCH"));
    rig.state.request_shutdown();
}

/// Tasks that write to the disk themselves hold every worker: the panel's
/// Abort needs none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_worker_writing_gpio() {
    crate::mode_suite!(Normal);
    if crate::operator_queue::isolated_native_case(module_path!(), "every_worker_writing_gpio") {
        return;
    }
    let rig = rig().await;
    let presser = Presser::new(Press::Gpio, &rig).await;
    let stall = Stall::begin(Site::Log);
    let written = (0..8).map(|_| tokio::spawn(async { crate::disk_stall::wait(Site::Log) })).collect::<Vec<_>>();
    let (sent, armed, aborted) = tokio::task::spawn_blocking(move || {
        assert!(stall.entered());
        std::thread::sleep(Duration::from_millis(200));
        let pressed = Instant::now();
        presser.abort(&rig);
        let (mut sent, mut armed, mut aborted) = (None, None, None);
        while pressed.elapsed() < Duration::from_secs(1) {
            let now = pressed.elapsed();
            if rig.wire.lock().unwrap().iter().any(|(_, kind, _)| *kind == "FLIGHT_STATE") {
                sent.get_or_insert(now);
            }
            if rig.state.abort_resend.active_resends().is_some() {
                armed.get_or_insert(now);
            }
            if rig.state.local_flight_state_snapshot() == FlightState::Aborted {
                aborted.get_or_insert(now);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(stall);
        rig.state.request_shutdown();
        (sent, armed, aborted)
    })
    .await
    .unwrap();
    eprintln!("ABORT_DISK_STALL site=EveryWorker press=Gpio flight_state={sent:?} armed={armed:?} aborted={aborted:?}");
    for write in written {
        write.await.unwrap();
    }
    for at in [sent, armed, aborted] {
        assert!(at.is_some_and(|at| at < BOUND), "{at:?}");
    }
}

/// A write to the session database that its disk never finishes (an expired
/// session's removal, a sign-in) keeps no Abort from being authorized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_database_write_http() {
    crate::mode_suite!(Normal);
    if crate::operator_queue::isolated_native_case(module_path!(), "session_database_write_http") {
        return;
    }
    let rig = rig().await;
    let mut writer = rig.state.auth_db.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *writer).await.unwrap();
    let address = rig.http;
    let pressed = Instant::now();
    let (status, took) =
        tokio::task::spawn_blocking(move || http(address, "POST", "/api/command", "\"Abort\"")).await.unwrap();
    let sent = rig.wire.lock().unwrap().iter().find(|(_, kind, _)| *kind == "ABORT").map(|(at, ..)| *at - pressed);
    eprintln!("ABORT_DISK_STALL site=SessionDatabase press=Http status={status} answered={took:?} abort_on_wire={sent:?}");
    sqlx::query("ROLLBACK").execute(&mut *writer).await.unwrap();
    assert_eq!(status, 202);
    assert!(took < BOUND, "{took:?}");
    rig.state.request_shutdown();
}
