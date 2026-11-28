//! Launch admission and station mode changes exclude each other under one lock.
//!
//! A Launch holds mode changes from the moment it is admitted: while it waits
//! in the command queue, while the worker sends it, and after it is sent until
//! its countdown starts (or the T clock is reset, or the station aborts). A
//! committed mode change refuses new Launch presses until it ends. Both checks
//! happen under [`LaunchModeGate`]'s lock, so there is no instant at which a
//! Launch is admitted and a mode change is committed.
//!
//! The lock is a leaf: nothing else is locked while it is held.
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) const LAUNCH_PENDING: &str = "a Launch command is pending";
pub(crate) const MODE_CHANGE_IN_PROGRESS: &str =
    "a station mode change is in progress; send Launch after it finishes";

#[derive(Default)]
pub struct LaunchModeGate {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Launch presses admitted and not yet finished by the worker.
    admitted: usize,
    /// A Launch was handed to the network and its countdown has not started.
    sent: bool,
    /// A mode change is committed and not yet finished or cancelled.
    mode_change: bool,
}

impl LaunchModeGate {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Admits one Launch press unless a mode change is committed. The hold
    /// travels with the queued command; dropping it releases the admission.
    pub fn admit(self: &Arc<Self>) -> Result<LaunchHold, &'static str> {
        let mut inner = self.lock();
        if inner.mode_change {
            return Err(MODE_CHANGE_IN_PROGRESS);
        }
        inner.admitted += 1;
        Ok(LaunchHold(self.clone()))
    }

    /// The Launch is being handed to the network. Mode changes stay refused
    /// until its countdown starts, the T clock is reset or the station aborts.
    pub fn mark_sent(&self) {
        self.lock().sent = true;
    }

    /// The sent Launch no longer holds mode changes: its countdown started
    /// (which holds them itself), the T clock was reset or the station aborted.
    pub fn clear_sent(&self) {
        self.lock().sent = false;
    }

    /// Why a Launch refuses a mode change right now, if it does.
    pub fn launch_hold(&self) -> Option<&'static str> {
        let inner = self.lock();
        (inner.admitted > 0 || inner.sent).then_some(LAUNCH_PENDING)
    }

    /// Commits a mode change unless a Launch is admitted or sent. Until
    /// [`end_mode_change`](Self::end_mode_change), every Launch press is refused.
    pub fn begin_mode_change(&self) -> Result<(), &'static str> {
        let mut inner = self.lock();
        if inner.admitted > 0 || inner.sent {
            return Err(LAUNCH_PENDING);
        }
        if inner.mode_change {
            return Err("a mode change is already in progress");
        }
        inner.mode_change = true;
        Ok(())
    }

    /// The committed mode change finished or was cancelled.
    pub fn end_mode_change(&self) {
        self.lock().mode_change = false;
    }

    #[cfg(test)]
    pub(crate) fn mode_change_committed(&self) -> bool {
        self.lock().mode_change
    }
}

/// One admitted Launch press. It holds mode changes until dropped: when the
/// worker has finished with the press (sent or refused), or when the queued
/// command is discarded without being run.
pub struct LaunchHold(Arc<LaunchModeGate>);

impl Clone for LaunchHold {
    fn clone(&self) -> Self {
        self.0.lock().admitted += 1;
        Self(self.0.clone())
    }
}

impl Drop for LaunchHold {
    fn drop(&mut self) {
        let mut inner = self.0.lock();
        inner.admitted = inner.admitted.saturating_sub(1);
    }
}

impl std::fmt::Debug for LaunchHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaunchHold")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_and_mode_change_exclude_each_other() {
        let gate = Arc::new(LaunchModeGate::default());
        let hold = gate.admit().unwrap();
        assert_eq!(gate.launch_hold(), Some(LAUNCH_PENDING));
        assert_eq!(gate.begin_mode_change(), Err(LAUNCH_PENDING));
        let copy = hold.clone();
        drop(hold);
        assert_eq!(gate.begin_mode_change(), Err(LAUNCH_PENDING), "a clone still holds");
        gate.mark_sent();
        drop(copy);
        assert_eq!(gate.begin_mode_change(), Err(LAUNCH_PENDING), "a sent Launch holds");
        gate.clear_sent();
        assert_eq!(gate.launch_hold(), None);

        gate.begin_mode_change().unwrap();
        assert_eq!(gate.admit().unwrap_err(), MODE_CHANGE_IN_PROGRESS);
        assert!(gate.begin_mode_change().is_err());
        gate.end_mode_change();
        let _hold = gate.admit().unwrap();
    }

    /// Racing admissions and mode changes: never both at once.
    #[test]
    fn concurrent_admission_and_mode_change_never_both_succeed() {
        for _ in 0..200 {
            let gate = Arc::new(LaunchModeGate::default());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (g, b) = (gate.clone(), barrier.clone());
            let launch = std::thread::spawn(move || {
                b.wait();
                g.admit().ok()
            });
            barrier.wait();
            let switched = gate.begin_mode_change().is_ok();
            let admitted = launch.join().unwrap();
            assert!(switched != admitted.is_some(), "exactly one wins");
        }
    }
}
