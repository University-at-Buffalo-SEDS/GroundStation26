//! App-owned reset admission barrier. No lock is held over command execution.
use anyhow::{Result, ensure};
use std::sync::{Arc, Mutex, OnceLock, Weak};
#[derive(Debug)]
struct State {
    boot: [u8; 16],
    closed: bool,
    active: usize,
}
#[derive(Debug)]
pub(super) struct Admission(Mutex<State>);
impl Admission {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(State {
            boot: super::nonce(),
            closed: false,
            active: 0,
        })))
    }
    pub(super) fn boot(&self) -> [u8; 16] {
        self.0.lock().unwrap().boot
    }
    pub(super) fn request_allowed(&self, boot: [u8; 16], manual_stop: bool) -> bool {
        let s = self.0.lock().unwrap();
        s.boot == boot && (!s.closed || manual_stop)
    }
    pub(super) fn admit(self: &Arc<Self>, boot: [u8; 16], manual_stop: bool) -> Result<Lease> {
        let mut s = self.0.lock().unwrap();
        ensure!(s.boot == boot, "Proxy capability was revoked");
        ensure!(!s.closed || manual_stop, "Proxy recovery barrier is closed");
        // Stop never waits behind ordinary work. Its own short dispatch still
        // counts until intent retention, so reset cannot pass that interval.
        s.active = s
            .active
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Proxy admission exhausted"))?;
        Ok(Lease {
            admission: self.clone(),
        })
    }
    pub(super) fn rotate_transport(&self) {
        let boot = super::nonce();
        self.0.lock().unwrap().boot = boot;
    }
    pub(super) fn begin(self: &Arc<Self>) -> Result<Barrier> {
        let next = super::nonce();
        let mut s = self.0.lock().unwrap();
        // An explicit new coordinator attempt may replace a failed barrier;
        // the old token can never reopen this one. Caller serializes operations.
        ensure!(next != s.boot, "Proxy nonce collision");
        s.closed = true;
        s.boot = next;
        Ok(Barrier {
            admission: self.clone(),
            boot: next,
        })
    }
}
pub(super) struct Lease {
    admission: Arc<Admission>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.admission.0.lock().unwrap();
        s.active = s.active.checked_sub(1).expect("owned proxy lease");
    }
}
/// Dropping a barrier leaves admission CLOSED. A caller must first quiesce,
/// purge its ordinary queues and verify owner reset/readbacks before reopening.
/// This object does not itself authorize reset, purge SDK/physical bytes, or
/// prove board completion. New manual Stop remains an independent retained lane.
pub(crate) struct Barrier {
    admission: Arc<Admission>,
    boot: [u8; 16],
}
impl Barrier {
    pub(crate) fn is_quiescent(&self) -> bool {
        let s = self.admission.0.lock().unwrap();
        s.closed && s.boot == self.boot && s.active == 0
    }
    pub(crate) fn reopen_after_verified_recovery(self) -> Result<()> {
        let next = super::nonce();
        let mut s = self.admission.0.lock().unwrap();
        ensure!(
            s.closed && s.boot == self.boot && s.active == 0,
            "Proxy dispatch has not quiesced"
        );
        ensure!(next != s.boot, "Proxy nonce collision");
        // Revoke capabilities observed while CLOSED as well as those before
        // the barrier; delayed requests must not spring alive after reopening.
        s.boot = next;
        s.closed = false;
        Ok(())
    }
}
static ACTIVE: OnceLock<Mutex<Weak<Admission>>> = OnceLock::new();
pub(super) fn register(admission: &Arc<Admission>) -> Result<()> {
    let mut active = ACTIVE
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap();
    ensure!(
        active
            .upgrade()
            .is_none_or(|previous| Arc::ptr_eq(&previous, admission)),
        "A primary proxy is already active"
    );
    *active = Arc::downgrade(admission);
    Ok(())
}
/// None means no active paired primary proxy exists; it says nothing about
/// unrelated GS/RRLS command queues. Caller must hold its own recovery fence.
pub(crate) fn begin_primary_recovery() -> Result<Option<Barrier>> {
    ACTIVE
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap()
        .upgrade()
        .map(|a| a.begin())
        .transpose()
}/// Fixture access to the exact production admission/lease, never a replacement
/// capability or success result. The coordinator must actually rotate and wait.
#[cfg(test)]
pub(crate) struct TestPrimaryAdmission(Arc<Admission>);
#[cfg(test)]
pub(crate) struct TestPrimaryLease {
    _lease: Lease,
}
#[cfg(test)]
impl TestPrimaryAdmission {
    pub(crate) fn register() -> Self {
        let admission = Admission::new();
        register(&admission).unwrap();
        Self(admission)
    }
    pub(crate) fn boot(&self) -> [u8; 16] {
        self.0.boot()
    }
    pub(crate) fn closed(&self) -> bool {
        self.0.0.lock().unwrap().closed
    }
    pub(crate) fn admit(&self, boot: [u8; 16]) -> Result<TestPrimaryLease> {
        self.0
            .admit(boot, false)
            .map(|lease| TestPrimaryLease { _lease: lease })
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_revokes_paused_validation_and_waits_admitted_dispatch_drop() {
        let a = Admission::new();
        let old = a.boot();
        let lease = a.admit(old, false).unwrap();
        let b = a.begin().unwrap();
        assert!(!b.is_quiescent());
        assert!(a.admit(old, false).is_err());
        assert!(a.admit(a.boot(), false).is_err());
        let stop = a.admit(a.boot(), true).unwrap();
        drop(stop);
        drop(lease);
        assert!(b.is_quiescent());
        b.reopen_after_verified_recovery().unwrap();
        assert!(a.admit(old, false).is_err());
        assert!(a.admit(a.boot(), false).is_ok());
    }
    #[test]
    fn cancelled_barrier_stays_closed_and_no_lock_crosses_dispatch_or_unwind() {
        let a = Admission::new();
        let old = a.boot();
        let lease = a.admit(old, false).unwrap();
        let b = a.begin().unwrap();
        drop(b);
        assert!(a.admit(a.boot(), false).is_err());
        drop(lease);
        assert!(a.0.try_lock().is_ok());
        let repair = a.begin().unwrap();
        assert!(repair.is_quiescent());
        repair.reopen_after_verified_recovery().unwrap();
        assert!(a.admit(a.boot(), false).is_ok());
        // An unwind drops the lease rather than leaving recovery permanently busy.
        let a = Admission::new();
        let copy = a.clone();
        let _ = std::panic::catch_unwind(move || {
            let _lease = copy.admit(copy.boot(), false).unwrap();
            panic!("dispatch fixture");
        });
        let b = a.begin().unwrap();
        assert!(b.is_quiescent());
    }
}
