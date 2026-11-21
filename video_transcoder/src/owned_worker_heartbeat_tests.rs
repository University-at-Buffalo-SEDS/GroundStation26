//! Real-backend held-heartbeat cancellation; separate from the three auth-phase controls.
#![cfg(target_os = "linux")]
use super::{BENCHMARK_PROBE, BenchmarkProbe, Controller, ShutdownPoll, WorkerOwner};
use crate::engine::Action;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Method, Request, Response, StatusCode, header},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Notify, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

struct Hop {
    method: Method,
    path: String,
    offered: Option<bool>,
    generation: Option<String>,
    status: Option<u16>,
    acknowledgement: Option<serde_json::Value>,
    authorization: Option<axum::http::HeaderValue>,
    after_shutdown: bool,
    forwarded: bool,
    held_request: bool,
}
struct Proxy {
    backend: url::Url,
    client: reqwest::Client,
    jobs: Mutex<JoinSet<()>>,
    ledger: Mutex<Vec<Hop>>,
    held: AtomicBool,
    true_requests: AtomicUsize,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    shutdown_requested: AtomicBool,
    release: AtomicBool,
    wake: Notify,
    original_auth: Mutex<Option<HeaderMap>>,
}
fn hop_headers(mut headers: HeaderMap) -> HeaderMap {
    // These are transport framing fields only. Authentication, content type,
    // and the exact captured application bytes remain unchanged.
    for name in [
        header::HOST,
        header::CONNECTION,
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }
    headers
}

async fn relay(State(state): State<Arc<Proxy>>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 16 << 20)
        .await
        .expect("bounded actual control payload");
    let path = parts.uri.path_and_query().unwrap().as_str().to_owned();
    assert!(path.starts_with('/') && !path.starts_with("//"));
    let target = state.backend.join(&path).unwrap();
    assert_eq!(target.host_str(), state.backend.host_str());
    assert_eq!(
        target.port_or_known_default(),
        state.backend.port_or_known_default()
    );
    let headers = hop_headers(parts.headers);
    let heartbeat =
        parts.method == Method::POST && parts.uri.path().starts_with("/api/video/transcode/host/");
    let value: Option<serde_json::Value> =
        heartbeat.then(|| serde_json::from_slice(&bytes).unwrap());
    let offered = value.as_ref().and_then(|v| v["offered"].as_bool());
    let generation = value
        .as_ref()
        .and_then(|v| v["generation"].as_str())
        .map(str::to_owned);
    // Let one genuine offer complete before holding the next original-G tick.
    let hold = if offered == Some(true) {
        let ordinal = state.true_requests.fetch_add(1, Ordering::SeqCst);
        assert!(ordinal < 2, "unexpected extra offered heartbeat");
        ordinal == 1
    } else {
        false
    };
    if hold {
        assert!(
            !state.held.load(Ordering::SeqCst),
            "unexpected second offered heartbeat"
        );
        assert!(
            generation
                .as_ref()
                .is_some_and(|g| g.len() == 32 && g.bytes().all(|b| b.is_ascii_hexdigit()))
        );
        *state.original_auth.lock().unwrap() = Some(headers.clone());
    }
    let index = {
        let mut rows = state.ledger.lock().unwrap();
        assert!(rows.len() < 64, "actual proxy request bound");
        let index = rows.len();
        rows.push(Hop {
            method: parts.method.clone(),
            path,
            offered,
            generation,
            status: None,
            acknowledgement: None,
            authorization: headers.get(header::AUTHORIZATION).cloned(),
            after_shutdown: state.shutdown_requested.load(Ordering::SeqCst),
            forwarded: false,
            held_request: hold,
        });
        index
    };
    let (sender, response) = oneshot::channel();
    let job_state = state.clone();
    // The fixture owns this task in a JoinSet independently of Hyper's
    // client connection. Client cancellation cannot discard the held Gtrue.
    state.jobs.lock().unwrap().spawn(async move {
        if hold {
            assert!(!job_state.held.swap(true, Ordering::SeqCst));
            job_state
                .entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
            while !job_state.release.load(Ordering::SeqCst) {
                let notified = job_state.wake.notified();
                if job_state.release.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::timeout_at(deadline, notified)
                    .await
                    .expect("held heartbeat fixture deadline");
            }
        }
        // All method/URL/auth/body values above were captured BEFORE the
        // hold. Do not query current generation or rebuild this request.
        job_state.ledger.lock().unwrap()[index].forwarded = true;
        let mut reply = job_state
            .client
            .request(parts.method, target)
            .headers(headers)
            .body(bytes)
            .send()
            .await
            .expect("actual backend forwarding");
        let status = reply.status();
        let response_headers = hop_headers(reply.headers().clone());
        let mut data = Vec::new();
        while let Some(chunk) = reply.chunk().await.unwrap() {
            assert!(
                data.len() + chunk.len() <= 16 << 20,
                "actual backend response bound"
            );
            data.extend_from_slice(&chunk);
        }
        {
            let mut ledger = job_state.ledger.lock().unwrap();
            ledger[index].status = Some(status.as_u16());
            if offered.is_some() && status.is_success() {
                ledger[index].acknowledgement = Some(serde_json::from_slice(&data).unwrap());
            }
        }
        let mut reply = Response::new(Body::from(data));
        *reply.status_mut() = status;
        *reply.headers_mut() = response_headers;
        // The old helper request may already be gone. This only reports
        // the real backend completion; no client receipt is inferred.
        let _ = sender.send(reply);
    });
    response.await.unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::empty())
            .unwrap()
    })
}

async fn wait_view(
    controller: &Controller,
    predicate: impl Fn(&crate::engine::View) -> bool,
    seconds: u64,
) {
    let end = Instant::now() + Duration::from_secs(seconds);
    loop {
        if predicate(&controller.view.lock().unwrap()) {
            return;
        }
        assert!(
            Instant::now() < end,
            "actual owned Controller view deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn actual_held_original_heartbeat() {
    let backend = url::Url::parse(
        &std::env::var("GS_MEDIA_TEST_ORIGIN").expect("real synthetic backend required"),
    )
    .unwrap();
    assert_eq!(backend.host_str(), Some("127.0.0.1"));
    assert_eq!(backend.scheme(), "http");
    assert_eq!(backend.path(), "/");
    assert!(backend.fragment().is_none());
    assert!(
        backend.username().is_empty() && backend.password().is_none() && backend.query().is_none()
    );
    let (entered_tx, entered) = oneshot::channel();
    let state = Arc::new(Proxy {
        backend,
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap(),
        jobs: Mutex::new(JoinSet::new()),
        ledger: Mutex::new(Vec::new()),
        held: AtomicBool::new(false),
        true_requests: AtomicUsize::new(0),
        entered: Mutex::new(Some(entered_tx)),
        shutdown_requested: AtomicBool::new(false),
        release: AtomicBool::new(false),
        wake: Notify::new(),
        original_auth: Mutex::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    let app = Router::new().fallback(relay).with_state(state.clone());
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopping.cancelled_owned())
            .await
    });
    let (benchmark_tx, benchmark_rx) = std::sync::mpsc::channel();
    assert!(
        BENCHMARK_PROBE
            .set(BenchmarkProbe {
                spawned: benchmark_tx,
                stop_owned: false
            })
            .is_ok()
    );
    let (controller, owner) = Controller::start_owned();
    let owner = Arc::new(Mutex::new(owner));
    let scenario_state = state.clone();
    let scenario_controller = controller.clone();
    let scenario_owner = owner.clone();
    let scenario = tokio::spawn(async move {
        scenario(
            scenario_state,
            scenario_controller,
            scenario_owner,
            origin,
            entered,
            benchmark_rx,
        )
        .await
    });
    // Preserve the actual owner outside the assertion task. A failing scenario
    // cannot detach it or bypass fixture server/forward-task settlement.
    let result = scenario.await;
    controller.request_shutdown();
    state.release.store(true, Ordering::SeqCst);
    state.wake.notify_waiters();
    let cleanup_deadline = Instant::now() + Duration::from_secs(8);
    let owner_joined = loop {
        let observed = owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poll_shutdown();
        if matches!(observed, ShutdownPoll::Joined(_)) {
            break true;
        }
        if Instant::now() >= cleanup_deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    stop.cancel();
    let server_result = tokio::time::timeout(Duration::from_secs(4), &mut server).await;
    let server_ok = matches!(server_result, Ok(Ok(Ok(()))));
    if !server.is_finished() {
        server.abort();
        let _ = server.await;
    }
    let mut jobs = std::mem::take(&mut *state.jobs.lock().unwrap());
    let jobs_ok = tokio::time::timeout(Duration::from_secs(22), async {
        let mut successful = true;
        while let Some(job) = jobs.join_next().await {
            successful &= job.is_ok();
        }
        successful
    })
    .await;
    let all_jobs_ok = matches!(jobs_ok, Ok(true));
    if !jobs.is_empty() {
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
    }
    // These assertions are reached only after all owned fixture tasks settled.
    let evidence = result.expect("actual held-heartbeat scenario failed (fixture tasks settled)");
    assert!(
        owner_joined,
        "actual native owner remained unresolved after fixture cleanup"
    );
    assert!(
        server_ok && all_jobs_ok,
        "owned proxy server/forwarders did not join successfully"
    );
    let rows = state.ledger.lock().unwrap();
    assert_eq!(rows.iter().filter(|r| r.offered == Some(true)).count(), 2);
    assert_eq!(rows.iter().filter(|r| r.offered == Some(false)).count(), 1);
    assert!(rows.iter().all(|r| r.status.is_some()));
    assert!(
        rows.iter()
            .all(|r| r.method == Method::GET || r.method == Method::POST)
    );
    assert!(
        rows.iter()
            .filter(|r| r.after_shutdown)
            .all(|r| r.offered == Some(false)),
        "new noncleanup request after shutdown seal"
    );
    assert!(state.jobs.lock().unwrap().is_empty());
    assert!(controller.tx.send(Action::Offer(true)).is_err());
    eprintln!(
        "ACTUAL_OWNED_WORKER_HEARTBEAT {}",
        serde_json::json!({"native_join_ms":evidence,"joined_while_original_Gtrue_held":true,"actual_benchmark_pid_reaped":true,"actual_first_true_ack":true,"actual_false_ack":true,"original_late_true_status":409,"backend_host_online":false,"all_forward_tasks_and_server_joined":true,"postseal_new_noncleanup_requests":0,"requests":rows.len(),"hardware":false})
    );
}

async fn scenario(
    state: Arc<Proxy>,
    controller: Controller,
    owner: Arc<Mutex<WorkerOwner>>,
    origin: String,
    entered: oneshot::Receiver<()>,
    benchmark_rx: std::sync::mpsc::Receiver<u32>,
) -> u128 {
    // Same actual fixture account interface as the qualified finite pipeline.
    // Backend challenge/login/permission/qualification remain real routes.
    controller
        .tx
        .send(Action::Connect {
            station: origin,
            username: "producer".into(),
            password: "fixture-only-password".into(),
            local_certificate: false,
        })
        .unwrap();
    wait_view(&controller, |v| v.connected && !v.busy, 15).await;
    controller
        .tx
        .send(Action::Test {
            name: format!("Owned heartbeat shutdown {}", std::process::id()),
        })
        .unwrap();
    wait_view(
        &controller,
        |v| {
            !v.busy
                && (v.passed
                    || v.message.starts_with("Test failed")
                    || v.message.starts_with("Insufficient"))
        },
        65,
    )
    .await;
    let qualified = controller.view.lock().unwrap().clone();
    assert!(qualified.passed && qualified.encoding_ms > 0 && qualified.encoding_ms <= 2500);
    assert!(qualified.upload_bps >= 12_000_000 && qualified.download_bps >= 12_000_000);
    assert!((1..=8).contains(&qualified.capacity));
    controller.tx.send(Action::Offer(true)).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    assert!(state.held.load(Ordering::SeqCst));
    let benchmark_pid = benchmark_rx
        .try_recv()
        .expect("actual qualified benchmark child was observed");
    assert!(
        benchmark_rx.try_recv().is_err(),
        "unexpected extra benchmark child"
    );
    assert!(
        !std::path::Path::new(&format!("/proc/{benchmark_pid}")).exists(),
        "actual benchmark child not reaped"
    );
    assert!(!state.release.load(Ordering::SeqCst));
    let before = Instant::now();
    state.shutdown_requested.store(true, Ordering::SeqCst);
    controller.request_shutdown();
    loop {
        let observed = owner.lock().unwrap().poll_shutdown();
        match observed {
            ShutdownPoll::Pending => {}
            ShutdownPoll::LocalUncertain(code) => {
                panic!("heartbeat local cleanup unresolved: {code}")
            }
            ShutdownPoll::Joined(report) => {
                assert!(
                    report.remote_uncertain().is_none(),
                    "actual original false acknowledgement required"
                );
                assert!(report.operational_failure().is_none());
                break;
            }
        }
        assert!(
            before.elapsed() < Duration::from_secs(3),
            "native join must predate held response release"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(before.elapsed() < Duration::from_secs(3));
    let native_join_ms = before.elapsed().as_millis();
    assert!(!state.release.load(Ordering::SeqCst));
    {
        let rows = state.ledger.lock().unwrap();
        let old = rows.iter().find(|r| r.held_request).unwrap();
        let first = rows
            .iter()
            .find(|r| r.offered == Some(true) && !r.held_request)
            .unwrap();
        assert_eq!(
            first.status,
            Some(200),
            "first real offered heartbeat must have completed"
        );
        assert!(first.forwarded);
        assert!(
            first.acknowledgement.as_ref().unwrap()["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(first.generation, old.generation);
        assert_eq!(
            first.acknowledgement.as_ref().unwrap()["generation"].as_str(),
            old.generation.as_deref()
        );
        assert!(
            old.status.is_none() && !old.forwarded,
            "old Gtrue must still be held before forwarding"
        );
        let closed = rows.iter().find(|r| r.offered == Some(false)).unwrap();
        assert_eq!(closed.status, Some(200));
        assert_eq!(closed.method, Method::POST);
        assert_eq!(old.method, Method::POST);
        assert_eq!(closed.path, old.path);
        assert!(old.authorization.is_some());
        assert!(
            closed.authorization == old.authorization,
            "cleanup must keep original authorization"
        );
        assert_eq!(closed.generation, old.generation);
        let ack = closed.acknowledgement.as_ref().unwrap();
        assert_eq!(ack["cleanup_complete"], true);
        assert_eq!(ack["selected"], false);
        assert!(ack["jobs"].as_array().unwrap().is_empty());
        assert_eq!(
            ack["stopped_generation"].as_str(),
            old.generation.as_deref()
        );
        assert_ne!(ack["generation"].as_str(), old.generation.as_deref());
    }
    assert!(!std::path::Path::new(&format!("/proc/{benchmark_pid}")).exists());
    assert!(controller.tx.send(Action::Offer(true)).is_err());
    state.release.store(true, Ordering::SeqCst);
    state.wake.notify_waiters();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if state
            .ledger
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.held_request && r.status == Some(409))
        {
            break;
        }
        assert!(
            Instant::now() < end,
            "original late Gtrue was not actually rejected by backend"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (host, headers) = {
        let rows = state.ledger.lock().unwrap();
        let old = rows.iter().find(|r| r.held_request).unwrap();
        (
            old.path
                .strip_prefix("/api/video/transcode/host/")
                .unwrap()
                .to_owned(),
            state.original_auth.lock().unwrap().clone().unwrap(),
        )
    };
    let actual = state
        .client
        .get(state.backend.join("/api/video/transcode/hosts").unwrap())
        .headers(headers)
        .send()
        .await
        .unwrap();
    assert!(actual.status().is_success());
    let hosts: serde_json::Value = actual.json().await.unwrap();
    let current = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == host)
        .unwrap();
    assert_eq!(
        current["online"], false,
        "old Gtrue must not reoffer the backend host"
    );
    native_join_ms
}

#[test]
#[ignore = "manual actual loopback backend/auth/qualification with original held heartbeat and native join"]
fn owned_native_shutdown_fences_held_original_heartbeat_before_join() {
    if !super::tests::in_isolated_child(concat!(
        module_path!(),
        "::owned_native_shutdown_fences_held_original_heartbeat_before_join"
    )) {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(Box::pin(actual_held_original_heartbeat()));
}
