//! Actual native-worker cancellation at held HTTP phases; not OS-window or backend-auth proof.
use super::{Controller, ShutdownPoll};
use crate::engine::Action;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
enum Phase {
    Challenge,
    Login,
    RoleRead,
}
impl Phase {
    fn path(self) -> &'static str {
        match self {
            Self::Challenge => "/api/auth/challenge",
            Self::Login => "/api/auth/login",
            Self::RoleRead => "/api/video/transcode/hosts",
        }
    }
    fn expected(self) -> Vec<(&'static str, &'static str)> {
        let mut requests = vec![("POST", "/api/auth/challenge")];
        if !matches!(self, Self::Challenge) {
            requests.push(("POST", "/api/auth/login"));
        }
        if matches!(self, Self::RoleRead) {
            requests.push(("GET", "/api/video/transcode/hosts"));
        }
        requests
    }
}

// A failing isolated case may retain an unsettled fixture join capability, never label
// its drop as cleanup. Successful cases join explicitly before accepting their ledger.
static UNSETTLED_SERVERS: OnceLock<Mutex<Vec<JoinHandle<()>>>> = OnceLock::new();

struct HeldServer {
    address: String,
    release: Arc<(Mutex<bool>, Condvar)>,
    stopping: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    held_response_attempted: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl HeldServer {
    fn new(phase: Phase) -> (Self, mpsc::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let held_response_attempted = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered) = mpsc::channel();
        let held = release.clone();
        let stop = stopping.clone();
        let ledger = requests.clone();
        let attempted = held_response_attempted.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut held_once = false;
            loop {
                assert!(Instant::now() < deadline, "loopback fixture deadline");
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(100)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_millis(100)))
                            .unwrap();
                        let (method, path, headers, body) = read_request(&mut stream);
                        {
                            let mut rows = ledger.lock().unwrap();
                            assert!(rows.len() < 32, "request ledger overflow");
                            rows.push((method.clone(), path.clone()));
                        }
                        let reply = match (method.as_str(), path.as_str()) {
                            ("POST", "/api/auth/challenge") => {
                                let value: serde_json::Value =
                                    serde_json::from_slice(&body).unwrap();
                                assert_eq!(value["username"], "worker-user");
                                r#"{"algorithm":"pbkdf2_sha256","iterations":10000,"salt_b64":"MDEyMzQ1Njc4OWFiY2RlZg==","challenge_id":"phase-challenge","server_nonce_b64":"c2VydmVy"}"#
                            }
                            ("POST", "/api/auth/login") => {
                                let value: serde_json::Value =
                                    serde_json::from_slice(&body).unwrap();
                                assert_eq!(value["username"], "worker-user");
                                assert_eq!(value["challenge_id"], "phase-challenge");
                                assert_eq!(value["remember_me"], false);
                                assert!(
                                    value["client_nonce_b64"]
                                        .as_str()
                                        .is_some_and(|s| !s.is_empty())
                                );
                                assert!(value["proof_b64"].as_str().is_some_and(|s| !s.is_empty()));
                                r#"{"token":"loopback-fixture-token"}"#
                            }
                            ("GET", "/api/video/transcode/hosts") => {
                                assert!(headers.lines().any(|line| {
                                    line.eq_ignore_ascii_case(
                                        "authorization: Bearer loopback-fixture-token",
                                    )
                                }));
                                r#"{"hosts":[]}"#
                            }
                            _ => panic!("unexpected post-seal request: {method} {path}"),
                        };
                        if path == phase.path() {
                            assert!(!held_once, "held phase retried unexpectedly");
                            held_once = true;
                            entered_tx.send(()).unwrap();
                            let (lock, wake) = &*held;
                            let guard = lock.lock().unwrap();
                            let (guard, timeout) = wake
                                .wait_timeout_while(guard, Duration::from_secs(10), |released| {
                                    !*released
                                })
                                .unwrap();
                            assert!(
                                !timeout.timed_out() && *guard,
                                "fixture response hold expired"
                            );
                            attempted.store(true, Ordering::SeqCst);
                        }
                        // A canceled request may close the socket. Attempting the original
                        // response is observed; a successful client receipt is never invented.
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                            reply.len()
                        );
                        let result = stream.write_all(response.as_bytes());
                        if path != phase.path() {
                            result.unwrap();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        // Once the actual native worker is joined, release and drain any
                        // already-pending connections before accepting the final ledger.
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            }
        });
        (
            Self {
                address,
                release,
                stopping,
                requests,
                held_response_attempted,
                worker: Some(worker),
            },
            entered,
        )
    }

    fn stop_and_join(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let (lock, wake) = &*self.release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        let deadline = Instant::now() + Duration::from_secs(3);
        if let Some(worker) = self.worker.as_ref() {
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(worker.is_finished(), "loopback fixture did not settle");
        }
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
impl Drop for HeldServer {
    fn drop(&mut self) {
        if !thread::panicking() {
            self.stop_and_join();
            return;
        }
        // Failure cleanup still releases the real response and bounds its observation.
        // It supplies no passing cleanup proof and must not replace the original failure.
        self.stopping.store(true, Ordering::SeqCst);
        let (lock, wake) = &*self.release;
        if let Ok(mut guard) = lock.lock() {
            *guard = true;
            wake.notify_all();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        if let Some(worker) = self.worker.as_ref() {
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        } else if let Some(worker) = self.worker.take() {
            UNSETTLED_SERVERS
                .get_or_init(Mutex::default)
                .lock()
                .unwrap()
                .push(worker);
            eprintln!("FIXTURE_SERVER_JOIN_UNVERIFIED: retained failed-case join capability");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> (String, String, String, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        assert!(Instant::now() < deadline, "loopback request body deadline");
        match stream.read(&mut buffer) {
            Ok(0) => panic!("incomplete loopback request"),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                assert!(bytes.len() <= 65536, "loopback request bound");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => panic!("loopback request read failed: {error}"),
        }
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let size = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(size <= 32768, "loopback body bound");
            if bytes.len() >= end + 4 + size {
                let mut first = headers.lines().next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let path = first.next().unwrap().to_owned();
                return (
                    method,
                    path,
                    headers,
                    bytes[end + 4..end + 4 + size].to_vec(),
                );
            }
        }
    }
}

fn held_phase(phase: Phase) {
    let (mut server, entered) = HeldServer::new(phase);
    let (controller, mut owner) = Controller::start_owned();
    let extra_controller = controller.clone();
    controller
        .tx
        .send(Action::Connect {
            station: server.address.clone(),
            username: "worker-user".into(),
            password: "fixture-only-password".into(),
            local_certificate: false,
        })
        .unwrap();
    entered
        .recv_timeout(Duration::from_secs(3))
        .expect("actual held HTTP phase was not reached");
    assert!(!server.held_response_attempted.load(Ordering::SeqCst));
    assert!(!*server.release.0.lock().unwrap());
    let started = Instant::now();
    extra_controller.request_shutdown();
    // These sends may be accepted into the old queue or refused after rx.close().
    // Neither case permits new HTTP or worker effects after the irreversible seal.
    let _ = controller.tx.send(Action::Connect {
        station: server.address.clone(),
        username: "worker-user".into(),
        password: "fixture-only-password".into(),
        local_certificate: false,
    });
    let _ = extra_controller.tx.send(Action::Test {
        name: "sealed-test".into(),
    });
    let _ = controller.tx.send(Action::Offer(true));
    controller.request_shutdown();
    loop {
        match owner.poll_shutdown() {
            ShutdownPoll::Pending => {
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "native worker did not cancel before the original 15s HTTP timeout"
                );
                thread::sleep(Duration::from_millis(5));
            }
            ShutdownPoll::LocalUncertain(code) => {
                panic!("held-auth local cleanup uncertain: {code}")
            }
            ShutdownPoll::Joined(report) => {
                assert!(report.remote_uncertain().is_none());
                assert!(report.operational_failure().is_none());
                assert!(report.benchmark_ms().is_none());
                break;
            }
        }
    }
    assert!(started.elapsed() < Duration::from_secs(3));
    let native_join_ms = started.elapsed().as_millis();
    assert!(
        !*server.release.0.lock().unwrap(),
        "worker joined only after held response release"
    );
    assert!(!server.held_response_attempted.load(Ordering::SeqCst));
    assert!(matches!(owner.poll_shutdown(), ShutdownPoll::Joined(_)));
    assert!(extra_controller.tx.send(Action::Offer(true)).is_err());
    let view = controller.view.lock().unwrap().clone();
    assert!(!view.connected && !view.passed && !view.offered && view.jobs == 0);
    server.stop_and_join();
    assert!(server.held_response_attempted.load(Ordering::SeqCst));
    let rows = server.requests.lock().unwrap();
    let actual: Vec<_> = rows
        .iter()
        .map(|(method, path)| (method.as_str(), path.as_str()))
        .collect();
    assert_eq!(
        actual,
        phase.expected(),
        "late login/role/offer work escaped shutdown"
    );
    assert!(
        rows.iter()
            .all(|(_, path)| !path.contains("/test") && !path.contains("heartbeat"))
    );
    eprintln!(
        "ACTUAL_OWNED_WORKER_HELD_PHASE {}",
        serde_json::json!({
            "phase":phase.path(),"joined_before_response_release":true,
            "native_join_ms":native_join_ms,"requests":&*rows,
            "late_io":0,"held_response_attempted_after_join":true,"hardware":false,
            "scope":"actual native Controller/WorkerOwner and loopback auth-shaped HTTP; not OS-window or backend-auth proof"
        })
    );
    drop(extra_controller);
    drop(controller);
}

#[test]
fn owned_native_worker_shutdown_interrupts_held_auth_challenge_before_response() {
    if !super::tests::in_isolated_child(concat!(
        module_path!(),
        "::owned_native_worker_shutdown_interrupts_held_auth_challenge_before_response"
    )) {
        return;
    }
    held_phase(Phase::Challenge);
}

#[test]
fn owned_native_worker_shutdown_interrupts_held_auth_login_before_response() {
    if !super::tests::in_isolated_child(concat!(
        module_path!(),
        "::owned_native_worker_shutdown_interrupts_held_auth_login_before_response"
    )) {
        return;
    }
    held_phase(Phase::Login);
}

#[test]
fn owned_native_worker_shutdown_interrupts_held_role_read_before_response() {
    if !super::tests::in_isolated_child(concat!(
        module_path!(),
        "::owned_native_worker_shutdown_interrupts_held_role_read_before_response"
    )) {
        return;
    }
    held_phase(Phase::RoleRead);
}
