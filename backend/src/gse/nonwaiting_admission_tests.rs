//! Actual production enqueue controls; only the deterministic pauses are test-only.
use crate::state::AppState;
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::Duration;

struct Hook {
    owner: usize,
    prepared: mpsc::Sender<()>,
    proceed: Mutex<mpsc::Receiver<()>>,
    core_entered: mpsc::Sender<()>,
    busy: mpsc::Sender<()>,
    packet: Arc<Mutex<Option<sedsnet::packet::Packet>>>,
    core_release: Arc<(Mutex<bool>, Condvar)>,
}
static HOOK: OnceLock<Mutex<Option<Arc<Hook>>>> = OnceLock::new();
pub(crate) struct Pause {
    pub(crate) prepared: mpsc::Receiver<()>,
    pub(crate) proceed: mpsc::Sender<()>,
    pub(crate) core_entered: mpsc::Receiver<()>,
    pub(crate) busy:mpsc::Receiver<()>,
    pub(crate) packet:Arc<Mutex<Option<sedsnet::packet::Packet>>>,
    release: Arc<(Mutex<bool>, Condvar)>,
}
impl Pause {
    pub(crate) fn install(state: &AppState) -> Self {
        let (prepared, received) = mpsc::channel();
        let (proceed, continued) = mpsc::channel();
        let (core_entered, core_received) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (busy,busy_received)=mpsc::channel();
        let packet=Arc::new(Mutex::new(None));
        *HOOK.get_or_init(Default::default).lock().unwrap() = Some(Arc::new(Hook {
            owner: state as *const AppState as usize,
            prepared,
            proceed: Mutex::new(continued),
            core_entered,
            busy,
            packet:packet.clone(),
            core_release: release.clone(),
        }));
        Self {
            prepared: received,
            proceed,
            core_entered: core_received,
            busy:busy_received,
            packet,
            release,
        }
    }
    pub(crate) fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
        let _ = self.proceed.send(());
    }
}
impl Drop for Pause {
    fn drop(&mut self) {
        self.release();
        *HOOK.get().unwrap().lock().unwrap() = None;
    }
}
fn hook(owner: usize) -> Option<Arc<Hook>> {
    HOOK.get()?
        .lock()
        .unwrap()
        .as_ref()
        .filter(|h| h.owner == owner)
        .cloned()
}
pub(crate) fn after_prepared(state: &AppState) {
    if let Some(hook) = hook(state as *const AppState as usize) {
        hook.prepared.send(()).unwrap();
        hook.proceed
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }
}
pub(crate) fn clock_pause(owner: usize) {
    if std::thread::current().name() != Some("production-sdk-core-holder") {
        return;
    }
    if let Some(hook) = hook(owner) {
        let _ = hook.core_entered.send(());
        let released = hook.core_release.0.lock().unwrap();
        let (released, _) = hook
            .core_release
            .1
            .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
            .unwrap();
        assert!(
            *released,
            "RAII must release the actual public SDK core holder"
        );
    }
}

pub(crate) fn launch_ticket(state: &AppState, revision: u64) -> super::LaunchAdmission {
    super::LaunchAdmission::capture_for_launch(state, Some(revision)).unwrap()
}

pub(crate) fn core_busy_refused(state:&AppState) {
    if let Some(hook)=hook(state as *const AppState as usize){let _=hook.busy.send(());}
}
pub(crate) fn after_prepared_packet(state:&AppState,packet:&sedsnet::packet::Packet) {
    if let Some(hook)=hook(state as *const AppState as usize){*hook.packet.lock().unwrap()=Some(packet.clone());}
    after_prepared(state);
}
