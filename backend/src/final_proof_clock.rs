//! Final recovery-proof observation only. This never replaces the SDK clock,
//! its returned monotonic clamp, or its error policy. No source I/O under locks.
use std::sync::{Mutex, TryLockError, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error { Busy, Invalid, Exhausted, RetryExpired }
impl Error {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Busy => "Final clock observation is busy; original recovery remains pending",
            Self::Invalid => "Final clock observation is invalid or changed domain; a fresh explicit recovery intent is required",
            Self::Exhausted => "Final clock observation sequence is exhausted; recovery cannot publish",
            Self::RetryExpired => "Final clock remained busy through its 100ms retry budget; recovery stays closed and requires a fresh explicit intent",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum Now { Wall(Instant), Model(u64) }
#[derive(Clone, Copy, Debug)]
pub(crate) enum Prepared {
    Wall { failure: FailureFence },
    Model { floor: u64, ticket: u64, failure: FailureFence },
}
/// RAM-only original-intent fence, captured before CONFIRM clock sampling.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FailureFence { revision: u64 }
impl FailureFence {
    pub(crate) fn capture() -> Self { Self { revision: MODEL.failures.load(Ordering::SeqCst) } }
    fn current_with(self, observer: &Observer) -> Result<(), Error> {
        if observer.exhausted.load(Ordering::SeqCst) { return Err(Error::Exhausted); }
        if observer.poisoned.load(Ordering::SeqCst)
            || self.revision != observer.failures.load(Ordering::SeqCst) { return Err(Error::Invalid); }
        Ok(())
    }
    pub(crate) fn current(self) -> Result<(), Error> { self.current_with(&MODEL) }
}
#[derive(Clone, Copy, Default)]
struct Completed { ticket: u64, tick: u64, valid: bool }
struct Observer {
    issued: AtomicU64,
    failures: AtomicU64,
    inflight: AtomicUsize,
    exhausted: AtomicBool,
    poisoned: AtomicBool,
    completed: Mutex<Completed>,
}
impl Observer {
    const fn new() -> Self {
        Self { issued: AtomicU64::new(0), failures: AtomicU64::new(0), inflight: AtomicUsize::new(0),
            exhausted: AtomicBool::new(false), poisoned: AtomicBool::new(false),
            completed: Mutex::new(Completed { ticket: 0, tick: 0, valid: false }) }
    }
    fn begin(&self) -> Attempt<'_> {
        let counted = self.inflight.try_update(Ordering::SeqCst, Ordering::SeqCst,
            |n| n.checked_add(1).filter(|n| *n != usize::MAX)).is_ok();
        if !counted { self.exhausted.store(true, Ordering::SeqCst); }
        let ticket = if counted {
            self.issued.try_update(Ordering::SeqCst, Ordering::SeqCst,
                |n| n.checked_add(1).filter(|n| *n != u64::MAX)).ok().map(|old| old + 1)
        } else { None };
        if ticket.is_none() { self.exhausted.store(true, Ordering::SeqCst); }
        Attempt { observer: self, ticket, counted, terminal: false }
    }
    fn invalidate(&self) {
        if self.failures.try_update(Ordering::SeqCst, Ordering::SeqCst,
            |n| n.checked_add(1).filter(|n| *n != u64::MAX)).is_err() {
            self.exhausted.store(true, Ordering::SeqCst);
        }
    }
    fn snapshot(&self) -> Result<Completed, Error> {
        if self.exhausted.load(Ordering::SeqCst) { return Err(Error::Exhausted); }
        if self.poisoned.load(Ordering::SeqCst) { return Err(Error::Invalid); }
        if self.inflight.load(Ordering::SeqCst) != 0 { return Err(Error::Busy); }
        let copy = match self.completed.try_lock() {
            Ok(record) => *record,
            Err(TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(Error::Invalid),
        }; // The mutex is dropped HERE, before any callback/publication/other lock.
        if self.exhausted.load(Ordering::SeqCst) { return Err(Error::Exhausted); }
        if self.poisoned.load(Ordering::SeqCst) { return Err(Error::Invalid); }
        if self.inflight.load(Ordering::SeqCst) != 0
            || self.issued.load(Ordering::SeqCst) != copy.ticket { return Err(Error::Busy); }
        if !copy.valid || copy.ticket == 0 { return Err(Error::Invalid); }
        Ok(copy)
    }
}
pub(crate) struct Attempt<'a> {
    observer: &'a Observer,
    ticket: Option<u64>,
    counted: bool,
    terminal: bool,
}
impl Attempt<'_> {
    /// Called only after actual source read and all old SDK clamp/cache updates.
    /// The mutex covers only bounded completed-record publication.
    pub(crate) fn finish(mut self, returned_clamp: u64, raw: Option<u64>) {
        if let Some(ticket) = self.ticket {
            if let Ok(mut record) = self.observer.completed.lock() {
                let valid = raw == Some(returned_clamp) && returned_clamp != u64::MAX;
                if ticket > record.ticket {
                    let monotonic = returned_clamp >= record.tick;
                    record.ticket = ticket;
                    record.tick = record.tick.max(returned_clamp);
                    record.valid = valid && monotonic;
                } else {
                    // An older completion can NEVER restore validity after a
                    // newer invalid source observation. Disagreement poisons
                    // this completed receipt until a genuinely later sample.
                    record.valid &= valid && returned_clamp == record.tick;
                    record.tick = record.tick.max(returned_clamp);
                }
                if !record.valid { self.observer.invalidate(); }
            } else { self.observer.poisoned.store(true, Ordering::SeqCst); }
        }
        self.terminal = true;
    }
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.terminal { self.observer.poisoned.store(true, Ordering::SeqCst); }
        if self.counted { self.observer.inflight.fetch_sub(1, Ordering::SeqCst); }
    }
}
static MODEL: Observer = Observer::new();
pub(crate) fn begin_model_observation() -> Attempt<'static> { MODEL.begin() }
impl Prepared {
    /// Actual source read occurs once, outside every application admission lock.
    /// This is not a new query/confirmation/command timestamp or owner.
    pub(crate) fn capture(failure: FailureFence) -> Result<Self, Error> {
        failure.current()?;
        let sampled = crate::simulation_clock_ms();
        let result = match (crate::SIMULATION_CLOCK_PATH.get(), sampled) {
            (Some(None), None) => Ok(Self::Wall { failure }),
            (Some(Some(_)), Some(floor)) => {
                // Own actual read has completed. Other in-flight readers do
                // not block preparation; the final RAM snapshot refuses them.
                let ticket = MODEL.issued.load(Ordering::SeqCst);
                if ticket == 0 { return Err(Error::Invalid); }
                Ok(Self::Model { floor, ticket, failure })
            }
            _ => Err(Error::Invalid),
        };
        failure.current()?;
        result
    }
    /// RAM-only Copy snapshot. Never holds its mutex while caller validates,
    /// acquires another application lock, or publishes state. Observed model
    /// time is NOT authority over unobserved external writes to the clock file.
    fn current_model(&self, observer: &Observer) -> Result<Now, Error> {
        let Self::Model { floor, ticket, failure } = self else { return Err(Error::Invalid) };
        failure.current_with(observer)?;
        let record = observer.snapshot()?;
        if record.tick < *floor || record.ticket < *ticket { return Err(Error::Invalid); }
        failure.current_with(observer)?;
        Ok(Now::Model(record.tick))
    }
    pub(crate) fn current(&self) -> Result<Now, Error> {
        match (self, crate::SIMULATION_CLOCK_PATH.get()) {
            (Self::Wall { failure }, Some(None)) => {
                failure.current()?;
                Ok(Now::Wall(Instant::now()))
            }
            (Self::Model { .. }, Some(Some(_))) => self.current_model(&MODEL),
            _ => Err(Error::Invalid),
        }
    }
}

/// New contention-only latency budget, not a new human-intent lifetime or
/// model-clock freshness TTL. The uncontended first attempt is not age-capped.
const CLOCK_BUSY_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);
pub(crate) struct Admission {
    clock: Prepared,
    started: Instant,
    contended: bool,
}
impl Admission {
    pub(crate) fn new(clock: Prepared) -> Self {
        Self { clock, started: Instant::now(), contended: false }
    }
    pub(crate) fn note_busy(&mut self) -> Result<(), Error> {
        self.contended = true;
        if self.started.elapsed() >= CLOCK_BUSY_RETRY_BUDGET { Err(Error::RetryExpired) } else { Ok(()) }
    }
    pub(crate) fn current(&self) -> Result<Now, Error> {
        if self.contended && self.started.elapsed() >= CLOCK_BUSY_RETRY_BUDGET { return Err(Error::RetryExpired); }
        self.clock.current()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[cfg(any(feature="hitl_mode", feature="firmware_sim"))]
    struct ReadPause {
        path: std::path::PathBuf,
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        used: AtomicBool,
    }
    #[cfg(any(feature="hitl_mode", feature="firmware_sim"))]
    static READ_PAUSE: Mutex<Option<std::sync::Arc<ReadPause>>> = Mutex::new(None);
    #[cfg(any(feature="hitl_mode", feature="firmware_sim"))]
    pub(crate) fn before_source_read(path: &std::path::Path) {
        let pause = READ_PAUSE.lock().unwrap().as_ref().filter(|pause|pause.path==path).cloned();
        if let Some(pause) = pause
            && !pause.used.swap(true,Ordering::SeqCst) {
                pause.entered.send(()).unwrap();
                pause.release.lock().unwrap().recv().unwrap();
            }
    }
    #[cfg(any(feature="hitl_mode", feature="firmware_sim"))]
    #[tokio::test]
    async fn actual_held_model_source_read_refuses_without_holding_stop_or_dispatch() {
        let Some(path) = model_child("actual_held_model_source_read_refuses_without_holding_stop_or_dispatch") else {return};
        let state = crate::state::tests::test_app_state().await;
        let original = Prepared::capture(FailureFence::capture()).unwrap();
        let (entered,received)=std::sync::mpsc::channel();
        let (release,released)=std::sync::mpsc::channel();
        struct Release(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for Release {fn drop(&mut self){if let Some(sender)=self.0.take(){let _=sender.send(());}}}
        let release = Release(Some(release));
        *READ_PAUSE.lock().unwrap()=Some(std::sync::Arc::new(ReadPause {
            path,entered,release:Mutex::new(released),used:AtomicBool::new(false),
        }));
        let reader=std::thread::spawn(crate::simulation_clock_ms);
        received.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let started=Instant::now();
        assert!(matches!(original.current(),Err(Error::Busy)));
        let before=state.manual_abort_intent.epoch();
        state.manual_abort_intent.request();
        assert!(state.manual_abort_intent.epoch()>before);
        let _=crate::gse::capture_operator_origin(&state); // actual dispatch gate remains obtainable
        assert!(started.elapsed()<std::time::Duration::from_millis(100),"RAM final clock/Stop/dispatch waited behind source IO");
        assert!(MODEL.completed.try_lock().is_ok(),"source IO held completed-record mutex");
        drop(release);
        assert_eq!(reader.join().unwrap(),Some(1000));
        *READ_PAUSE.lock().unwrap()=None;
        assert!(matches!(original.current(),Ok(Now::Model(1000))));
        // Clock readiness is deliberately NOT command permission: the actual
        // original Stop token was retired above and ordinary guards still apply.
    }
    #[test]
    fn original_intent_never_revives_after_busy_invalid_then_good() {
        let clock = Observer::new();
        clock.begin().finish(100, Some(100));
        let failure = FailureFence { revision: clock.failures.load(Ordering::SeqCst) };
        let original = Prepared::Model { floor: 100, ticket: 1, failure };
        let held = clock.begin();
        assert!(matches!(original.current_model(&clock), Err(Error::Busy)));
        clock.begin().finish(100, None);
        clock.begin().finish(100, Some(100));
        held.finish(100, Some(100));
        assert!(clock.snapshot().is_ok(), "latest-valid shortcut would revive the old intent");
        assert!(matches!(original.current_model(&clock), Err(Error::Invalid)));
        let fresh = Prepared::Model { floor: 100, ticket: clock.issued.load(Ordering::SeqCst),
            failure: FailureFence { revision: clock.failures.load(Ordering::SeqCst) } };
        assert!(matches!(fresh.current_model(&clock), Ok(Now::Model(100))));
        clock.failures.store(u64::MAX - 1, Ordering::SeqCst);
        clock.begin().finish(100, None);
        assert!(matches!(clock.snapshot(), Err(Error::Exhausted)));
    }
    #[cfg(any(feature="hitl_mode", feature="firmware_sim"))]
    fn model_child(name: &str) -> Option<std::path::PathBuf> {
        if std::env::var("GS_FINAL_CLOCK_CHILD").as_deref() == Ok(name) {
            return Some(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap().into());
        }
        let root = std::env::temp_dir().join(format!("gs-final-clock-{}-{:016x}",
            std::process::id(), rand::random::<u64>()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("clock"); std::fs::write(&path,"1000").unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("final_proof_clock::tests::{name}"), "--nocapture"])
            .env("GS_FINAL_CLOCK_CHILD",name).env("GS_SIM_CLOCK_FILE",&path)
            .output().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.status.success(), "actual model child failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),String::from_utf8_lossy(&result.stderr));
        None
    }
    #[cfg(any(feature="hitl_mode", feature="firmware_sim"))]
    #[test]
    fn actual_model_file_invalid_then_good_cannot_revive_original_confirmation() {
        let Some(path) = model_child("actual_model_file_invalid_then_good_cannot_revive_original_confirmation") else {return};
        let original = Prepared::capture(FailureFence::capture()).unwrap();
        let held = begin_model_observation();
        assert!(matches!(original.current(), Err(Error::Busy)));
        std::fs::write(&path,"temporarily truncated invalid clock").unwrap();
        assert_eq!(crate::simulation_clock_ms(),Some(1000),"SDK returned clamp must stay unchanged");
        std::fs::write(&path,"1000").unwrap();
        assert_eq!(crate::simulation_clock_ms(),Some(1000));
        held.finish(1000,Some(1000));
        assert!(MODEL.snapshot().is_ok());
        assert!(matches!(original.current(),Err(Error::Invalid)),"old confirmation revived after good source returned");
        assert!(Prepared::capture(FailureFence::capture()).is_ok(),"new explicit intent with actual valid read remains possible");
    }
    #[cfg(any(feature="hitl_mode", feature="firmware_sim"))]
    #[test]
    fn actual_model_faults_preserve_sdk_clamp_and_new_final_receipt_refuses() {
        let Some(path) = model_child("actual_model_faults_preserve_sdk_clamp_and_new_final_receipt_refuses") else {return};
        for raw in [Some(""),Some("bad"),Some("999"),None] {
            std::fs::write(&path,"1000").unwrap();
            let original = Prepared::capture(FailureFence::capture()).unwrap();
            match raw {Some(raw)=>std::fs::write(&path,raw).unwrap(),None=>std::fs::remove_file(&path).unwrap()};
            assert_eq!(crate::simulation_clock_ms(),Some(1000));
            assert!(matches!(original.current(),Err(Error::Invalid)));
            std::fs::write(&path,"1000").unwrap();
            assert_eq!(crate::simulation_clock_ms(),Some(1000));
            assert!(matches!(original.current(),Err(Error::Invalid)));
            assert!(Prepared::capture(FailureFence::capture()).is_ok());
        }
        let original = Prepared::capture(FailureFence::capture()).unwrap();
        std::fs::write(&path,u64::MAX.to_string()).unwrap();
        assert_eq!(crate::simulation_clock_ms(),Some(u64::MAX),"global SDK policy is unchanged even for sentinel");
        assert!(matches!(original.current(),Err(Error::Invalid)));
    }
    #[test]
    fn inflight_and_busy_copy_never_wait_or_manufacture_a_completed_sample() {
        let clock = Observer::new();
        let a = clock.begin();
        assert!(matches!(clock.snapshot(), Err(Error::Busy)));
        a.finish(100, Some(100));
        let held = clock.completed.lock().unwrap();
        assert!(matches!(clock.snapshot(), Err(Error::Busy)));
        drop(held);
        assert_eq!(clock.snapshot().unwrap().tick, 100);
    }
    #[test]
    fn older_valid_completion_cannot_clear_newer_invalid_or_move_time_backward() {
        let clock = Observer::new();
        let older = clock.begin(); let newer = clock.begin();
        newer.finish(100, None);
        older.finish(100, Some(100));
        assert!(matches!(clock.snapshot(), Err(Error::Invalid)));
        clock.begin().finish(100, Some(100));
        assert_eq!(clock.snapshot().unwrap().tick, 100);
        clock.begin().finish(100, Some(90));
        assert!(matches!(clock.snapshot(), Err(Error::Invalid)));
    }
    #[test]
    fn paused_model_keeps_tick_and_exhaustion_or_dropped_read_never_revives() {
        let clock = Observer::new();
        clock.begin().finish(100, Some(100));
        clock.begin().finish(100, Some(100));
        assert_eq!(clock.snapshot().unwrap().tick, 100);
        clock.issued.store(u64::MAX - 1, Ordering::SeqCst);
        clock.begin().finish(100, Some(100));
        assert!(matches!(clock.snapshot(), Err(Error::Exhausted)));
        let clock = Observer::new();
        drop(clock.begin());
        clock.begin().finish(100, Some(100));
        assert!(matches!(clock.snapshot(), Err(Error::Invalid)));
    }
}
