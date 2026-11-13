//! Native worker join capability and explicit local resource settlement.
use super::*;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Debug)]
pub struct JoinedReport {
    remote: Option<&'static str>,
    failure: Option<&'static str>,
    benchmark: Option<u64>,
}
impl JoinedReport {
    pub fn remote_uncertain(&self) -> Option<&'static str> { self.remote }
    pub fn operational_failure(&self) -> Option<&'static str> { self.failure }
    pub fn benchmark_ms(&self) -> Option<u64> { self.benchmark }
}
#[derive(Clone, Debug)]
pub enum ShutdownPoll {
    Pending,
    LocalUncertain(&'static str),
    Joined(JoinedReport),
}
#[derive(Default)]
pub(super) struct WorkerExit {
    remote: Option<&'static str>,
    failure: Option<&'static str>,
    benchmark: Option<u64>,
}
impl WorkerExit {
    pub(super) fn stopped(report: StopReport, local: &LocalScope) -> Self {
        let mut exit = Self { failure: report.failed, ..Self::default() };
        if let Some(reason) = report.uncertain {
            // The generation StopReport deliberately retains its dominant
            // local reason. It does not independently export remote proof in
            // that case. Preserve conservative remote uncertainty even if a
            // retained local timeout later settles; never infer a remote ack
            // from local settlement or thread join.
            exit.remote = Some(CleanupUncertain::RemoteCompletion.code());
            match reason {
                CleanupUncertain::RemoteCompletion => exit.remote = Some(reason.code()),
                // These are observation timeouts. Closing can settle them only
                // through the retained resource's actual later wait/join.
                CleanupUncertain::EncoderTimeout | CleanupUncertain::ProxyTimeout => exit.failure = Some(reason.code()),
                _ => local.fail(reason.code()),
            }
        }
        exit
    }
    #[cfg(test)]
    pub(super) fn legacy(self, local: &LocalScope) -> Result<()> {
        if let Some(reason)=local.fault().or(self.remote) { anyhow::bail!(reason); }
        Ok(())
    }
    pub(super) fn benchmark(result: Result<u64>) -> Self {
        match result {
            Ok(ms) => Self { benchmark: Some(ms), ..Self::default() },
            Err(_) => Self { failure: Some("video_processing_benchmark_failed"), ..Self::default() },
        }
    }
}

enum PendingLocal {
    Process(tokio::process::Child),
    Proxy(JoinHandle<std::io::Result<()>>),
    Job(JoinHandle<JobOutcome>),
}
#[derive(Default)]
pub(super) struct LocalScope {
    pending: Mutex<Vec<PendingLocal>>,
    active: AtomicUsize,
    fault: Mutex<Option<&'static str>>,
    waiting: Mutex<Option<&'static str>>,
}
impl LocalScope {
    pub(super) fn fail(&self, reason: &'static str) {
        if let Ok(mut fault) = self.fault.lock() && fault.is_none() { *fault = Some(reason); }
    }
    fn fault(&self) -> Option<&'static str> {
        self.fault.lock().map(|fault| *fault).unwrap_or(Some("video_processing_local_ownership_poisoned"))
    }
    fn begin(&self) { self.active.fetch_add(1, Ordering::SeqCst); }
    fn complete(&self) {
        if self.active.fetch_sub(1, Ordering::SeqCst)==1
            && let Ok(mut waiting)=self.waiting.lock(){*waiting=None;}
    }
    pub(super) fn track_proxy(&self) { self.begin(); }
    pub(super) fn complete_proxy(&self) { self.complete(); }
    pub(super) fn retain_proxy(&self, task: JoinHandle<std::io::Result<()>>) {
        *self.waiting.lock().expect("local pending ownership")=Some("video_processing_proxy_cleanup_pending");
        self.pending.lock().expect("local resource ownership").push(PendingLocal::Proxy(task));
    }
    fn retain_job(&self, task: JoinHandle<JobOutcome>) {
        *self.waiting.lock().expect("local pending ownership")=Some("video_processing_job_join_pending");
        self.pending.lock().expect("local resource ownership").push(PendingLocal::Job(task));
    }
    fn retain_child(&self, child: tokio::process::Child) {
        *self.waiting.lock().expect("local pending ownership")=Some("video_processing_encoder_cleanup_pending");
        self.pending.lock().expect("local resource ownership").push(PendingLocal::Process(child));
    }
    pub(super) async fn observe(&self) {
        let pending = std::mem::take(&mut *self.pending.lock().expect("local resource ownership"));
        let mut keep = Vec::new();
        for resource in pending {
            match resource {
                PendingLocal::Process(mut child) => match child.try_wait() {
                    Ok(Some(_)) => self.complete(),
                    Ok(None) => keep.push(PendingLocal::Process(child)),
                    Err(_) => {
                        self.fail("video_processing_encoder_wait_unverified");
                        // Never signal a possibly recycled pid after a
                        // wait error. Retain ownership and uncertainty.
                        keep.push(PendingLocal::Process(child));
                    }
                },
                PendingLocal::Proxy(task) if task.is_finished() => {
                    match task.await {
                        Ok(Ok(())) => {},
                        _ => self.fail("video_processing_proxy_join_unverified"),
                    }
                    self.complete();
                }
                PendingLocal::Job(task) if task.is_finished() => {
                    match task.await {
                        Ok(JobOutcome::CleanupUncertain(reason)) if !matches!(reason,CleanupUncertain::RemoteCompletion|CleanupUncertain::EncoderTimeout|CleanupUncertain::ProxyTimeout) => self.fail(reason.code()),
                        Err(_) => self.fail("video_processing_job_join_unverified"),
                        _ => {},
                    }
                    self.complete();
                }
                resource => keep.push(resource),
            }
        }
        self.pending.lock().expect("local resource ownership").extend(keep);
    }
    pub(super) async fn settle(&self) {
        // Observation deadlines never discard handles. This same runtime keeps
        // driving the actual retained waits/joins until settlement is known.
        while self.active.load(Ordering::SeqCst) != 0 {
            self.observe().await;
            if self.active.load(Ordering::SeqCst) != 0 { tokio::time::sleep(Duration::from_millis(50)).await; }
        }
    }

}

pub(super) struct OwnedJobTask {
    task: Option<JoinHandle<JobOutcome>>,
    cancelled: CancellationToken,
    local: Arc<LocalScope>,
}
impl OwnedJobTask {
    pub(super) fn new(task:JoinHandle<JobOutcome>,cancelled:CancellationToken,local:Arc<LocalScope>)->Self {
        local.begin();Self{task:Some(task),cancelled,local}
    }
    pub(super) fn is_finished(&self)->bool { self.task.as_ref().is_none_or(JoinHandle::is_finished) }
}
impl std::future::Future for OwnedJobTask {
    type Output=Result<JobOutcome,tokio::task::JoinError>;
    fn poll(self:std::pin::Pin<&mut Self>,context:&mut std::task::Context<'_>)->std::task::Poll<Self::Output> {
        let this=self.get_mut();
        let result=std::future::Future::poll(std::pin::Pin::new(this.task.as_mut().expect("owned job task")),context);
        if result.is_ready(){let _=this.task.take();this.local.complete();}
        result
    }
}
impl Drop for OwnedJobTask {
    fn drop(&mut self){
        self.cancelled.cancel();
        if let Some(task)=self.task.take(){self.local.retain_job(task);}
    }
}

pub(super) struct OwnedProcess {
    child: Option<tokio::process::Child>,
    local: Arc<LocalScope>,
}
impl OwnedProcess {
    pub(super) fn new(child: tokio::process::Child, local: Arc<LocalScope>) -> Self {
        local.begin();
        Self { child: Some(child), local }
    }
    pub(super) fn cleanup_error(&self, reason: CleanupUncertain) {
        if !matches!(reason, CleanupUncertain::EncoderTimeout) { self.local.fail(reason.code()); }
    }
}
impl Deref for OwnedProcess {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target { self.child.as_ref().expect("owned process") }
}
impl DerefMut for OwnedProcess {
    fn deref_mut(&mut self) -> &mut Self::Target { self.child.as_mut().expect("owned process") }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return; };
        match child.try_wait() {
            Ok(Some(_)) => self.local.complete(),
            Ok(None) => {
                #[cfg(unix)]
                if child.id().is_some_and(|pid| !encoder_group::terminate(pid)) {
                    self.local.fail("video_processing_encoder_group_cleanup_unverified");
                }
                let _ = child.start_kill();
                self.local.retain_child(child);
            }
            Err(_) => {
                self.local.fail("video_processing_encoder_wait_unverified");
                self.local.retain_child(child);
            }
        }
    }
}

#[derive(Clone)]
pub struct Controller {
    pub tx: mpsc::UnboundedSender<Action>,
    pub view: Arc<Mutex<View>>,
    shutdown: CancellationToken,
}
pub struct WorkerOwner {
    thread: Option<std::thread::JoinHandle<WorkerExit>>,
    shutdown: CancellationToken,
    local: Arc<LocalScope>,
    view: Arc<Mutex<View>>,
    observed: Option<ShutdownPoll>,
}
// Unexpected owner destruction is not a successful shutdown. Preserve the
// actual join capability instead of silently detaching it. Normal Main keeps
// its owner through Pending/LocalUncertain and never uses this fallback.
struct UnclaimedWorker {
    _thread: std::thread::JoinHandle<WorkerExit>,
    _local: Arc<LocalScope>,
}
static UNCLAIMED: std::sync::OnceLock<Mutex<Vec<UnclaimedWorker>>> = std::sync::OnceLock::new();
fn unclaimed() -> &'static Mutex<Vec<UnclaimedWorker>> { UNCLAIMED.get_or_init(Mutex::default) }
impl Controller {
    pub fn start_owned() -> (Self, WorkerOwner) { Self::start_mode(false) }
    pub fn start_benchmark_owned() -> (Self, WorkerOwner) { Self::start_mode(true) }
    fn start_mode(benchmark_only: bool) -> (Self, WorkerOwner) {
        let (tx, rx) = mpsc::unbounded_channel();
        let view = Arc::new(Mutex::new(View::default()));
        let shutdown = CancellationToken::new();
        let local = Arc::new(LocalScope::default());
        let controller = Self { tx, view: view.clone(), shutdown: shutdown.clone() };
        let mut owner = WorkerOwner { thread: None, shutdown: shutdown.clone(), local: local.clone(), view: view.clone(), observed: None };
        if !unclaimed().lock().expect("unclaimed worker ownership").is_empty() {
            local.fail("video_processing_prior_worker_unjoined");
            shutdown.cancel();
            update(&view, |v| v.message = "A previous worker owner was lost; cleanup remains unverified. New work is blocked.".into());
            return (controller, owner);
        }
        owner.thread = Some(std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(_) => {shutdown.cancel();update(&view,|v|v.message="video_processing_runtime_unavailable".into());return WorkerExit { failure: Some("video_processing_runtime_unavailable"), ..WorkerExit::default() };},
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(async {
                if benchmark_only {
                    WorkerExit::benchmark(benchmark_scoped(&shutdown, &local, tokio::time::Instant::now() + Duration::from_secs(30)).await)
                } else {
                    control_owned(rx, view.clone(), shutdown.clone(), local.clone()).await
                }
            })));
            let result = match result {
                Ok(exit) => exit,
                Err(_) => {
                    local.fail("video_processing_worker_panicked");
                    shutdown.cancel();
                    WorkerExit::default()
                }
            };
            shutdown.cancel();
            runtime.block_on(local.settle());
            // Runtime destruction precedes native thread return. It is never
            // forced with shutdown_timeout to manufacture a joined report.
            drop(runtime);
            #[cfg(test)] observe_before_thread_return();
            result
        }));
        (controller, owner)
    }
    pub fn request_shutdown(&self) {
        if self.shutdown.is_cancelled() { return; }
        self.shutdown.cancel();
        if let Ok(mut view) = self.view.try_lock() {
            view.offered = false; view.passed = false; view.busy = true;
            view.message = "Stopping processing and verifying local cleanup…".into();
        }
    }
}
impl WorkerOwner {
    pub fn poll_shutdown(&mut self) -> ShutdownPoll {
        if let Some(observed) = &self.observed { return observed.clone(); }
        if let Some(reason) = self.local.fault() {
            if let Ok(mut view) = self.view.try_lock() { view.busy = false; view.message = format!("{reason}: local cleanup remains unverified; this window must stay open."); }
            return ShutdownPoll::LocalUncertain(reason);
        }
        if let Some(reason)=*self.local.waiting.lock().expect("local pending ownership") {
            return ShutdownPoll::LocalUncertain(reason);
        }
        let Some(thread) = self.thread.as_ref() else { return ShutdownPoll::LocalUncertain("video_processing_worker_missing"); };
        if thread.thread().id() == std::thread::current().id() { return ShutdownPoll::LocalUncertain("video_processing_self_join_refused"); }
        if !thread.is_finished() { return ShutdownPoll::Pending; }
        let thread = self.thread.take().expect("retained native join capability");
        let observed = match thread.join() {
            Ok(exit) if self.local.active.load(Ordering::SeqCst) == 0 && self.local.fault().is_none() => ShutdownPoll::Joined(JoinedReport { remote: exit.remote, failure: exit.failure, benchmark: exit.benchmark }),
            _ => ShutdownPoll::LocalUncertain("video_processing_worker_join_unverified"),
        };
        self.observed = Some(observed.clone());
        observed
    }
}
impl Drop for WorkerOwner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(thread) = self.thread.take() {
            self.local.fail("video_processing_shutdown_owner_dropped");
            if let Ok(mut view) = self.view.try_lock() { view.message = "Worker ownership was dropped before join; retained cleanup is unverified.".into(); }
            unclaimed().lock().expect("unclaimed worker ownership").push(UnclaimedWorker { _thread: thread, _local: self.local.clone() });
        }
    }
}

#[cfg(test)]
struct BenchmarkProbe { spawned: std::sync::mpsc::Sender<u32>, #[cfg(target_os="linux")] stop_owned: bool }
#[cfg(test)]
static BENCHMARK_PROBE: std::sync::OnceLock<BenchmarkProbe> = std::sync::OnceLock::new();
#[cfg(test)]
pub(super) fn observe_benchmark_spawned(pid: u32) {
    if let Some(probe)=BENCHMARK_PROBE.get(){
        #[cfg(target_os="linux")]
        if probe.stop_owned {
            unsafe extern "C" { fn kill(pid:std::ffi::c_int,signal:std::ffi::c_int)->std::ffi::c_int; }
            // SAFETY: observation is synchronous before wait, on this owned
            // unreaped child only. Linux SIGSTOP is19. No process is adopted.
            assert_eq!(unsafe{kill(pid as std::ffi::c_int,19)},0);
        }
        let _=probe.spawned.send(pid);
    }
}
#[cfg(test)]
struct BeforeReturnProbe { barrier: Arc<(Mutex<bool>,std::sync::Condvar)>, reached: std::sync::mpsc::Sender<()> }
#[cfg(test)]
static BEFORE_THREAD_RETURN: std::sync::OnceLock<BeforeReturnProbe>=std::sync::OnceLock::new();
#[cfg(test)]
fn observe_before_thread_return() {
    if let Some(probe)=SELF_JOIN_PROBE.get(){
        let mut owner=probe.owner.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        let refused=matches!(owner.poll_shutdown(),ShutdownPoll::LocalUncertain("video_processing_self_join_refused"))&&owner.thread.is_some();
        probe.returned.send((owner,refused)).unwrap_or_else(|_|panic!("selfjoin test owner return failed"));
    }
    if let Some(probe)=BEFORE_THREAD_RETURN.get(){
        let _=probe.reached.send(());
        let (lock,wake)=&*probe.barrier;let mut released=lock.lock().unwrap();
        while !*released { released=wake.wait(released).unwrap(); }
    }
}

#[cfg(test)]
struct SelfJoinProbe { owner: Mutex<std::sync::mpsc::Receiver<WorkerOwner>>, returned: std::sync::mpsc::Sender<(WorkerOwner,bool)> }
#[cfg(test)]
static SELF_JOIN_PROBE: std::sync::OnceLock<SelfJoinProbe>=std::sync::OnceLock::new();
#[cfg(test)]
static PROXY_RETURN_BARRIER: std::sync::OnceLock<Arc<tokio::sync::Notify>>=std::sync::OnceLock::new();
#[cfg(test)]
pub(super) async fn observe_proxy_return() {
    if let Some(barrier)=PROXY_RETURN_BARRIER.get(){barrier.notified().await;}
}
#[cfg(test)]
static STOP_ACK_PROBE: std::sync::OnceLock<std::sync::mpsc::Sender<bool>>=std::sync::OnceLock::new();
#[cfg(test)]
pub(super) fn observe_stop_ack(session:&Session,offered:bool,reply:&Heartbeat){
    if !offered && let Some(probe)=STOP_ACK_PROBE.get(){
        let matched=session.generation.as_deref().is_some_and(|original|stop_acknowledges(original,reply));
        let _=probe.send(matched);
    }
}

#[cfg(test)]
#[path = "owned_worker_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "owned_worker_phase_tests.rs"]
mod phase_tests;

#[cfg(test)]
#[path = "owned_worker_heartbeat_tests.rs"]
mod heartbeat_tests;
