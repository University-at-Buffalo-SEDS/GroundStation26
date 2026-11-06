//! Test-only pause after the real SDK topology export returns, outside its lock.
use super::*;
struct Gate {
    entered: mpsc::Sender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
    expire_request: AtomicBool,
}
struct Installed {
    key: usize,
    gate: Arc<Gate>,
}
static GATE: OnceLock<Mutex<Option<Installed>>> = OnceLock::new();
pub(super) struct Pause {
    pub(super) entered: mpsc::Receiver<()>,
    pub(super) resume: mpsc::Sender<()>,
    key: usize,
}
impl Pause {
    pub(super) fn install(owner: &Arc<Observer>) -> Self {
        let (entered, received) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let key = Arc::as_ptr(owner) as usize;
        *GATE.get_or_init(Default::default).lock().unwrap() = Some(Installed {
            key,
            gate: Arc::new(Gate {
                entered,
                resume: Mutex::new(resumed),
                expire_request: AtomicBool::new(false),
            }),
        });
        Self {
            entered: received,
            resume,
            key,
        }
    }
    pub(super) fn expire_original_request(&self) {
        let slot = GATE.get().unwrap().lock().unwrap();
        let installed = slot.as_ref().unwrap();
        assert_eq!(installed.key, self.key);
        installed.gate.expire_request.store(true, Ordering::Release);
    }

}
impl Drop for Pause {
    fn drop(&mut self) {
        let _ = self.resume.send(());
        if let Some(slot) = GATE.get() {
            let mut slot = slot.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|installed| installed.key == self.key)
            {
                *slot = None;
            }
        }
    }
}
pub(super) fn after_topology(owner: &Arc<Observer>, requested: Instant) -> Instant {
    let gate = GATE.get().and_then(|slot| {
        slot.lock()
            .unwrap()
            .as_ref()
            .filter(|installed| installed.key == Arc::as_ptr(owner) as usize)
            .map(|installed| installed.gate.clone())
    });
    if let Some(gate) = gate {
        gate.entered.send(()).unwrap();
        gate.resume
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        if gate.expire_request.load(Ordering::Acquire) {
            return Instant::now() - MAX_AGE - Duration::from_millis(10);
        }
    }
    requested
}
