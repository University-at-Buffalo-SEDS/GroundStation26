//! The GSE runtime behind its lock, and what the automatic abort keeps beside
//! it. Other work holds the lock across disk writes (a completed test, a
//! Launch being saved), so the automatic abort never takes it: it is decided
//! and sent from the owners here, and joins the runtime the next time anyone
//! takes the lock.
//!
//! Lock order: dispatch gate, GSE lock, overpressure turn, overpressure
//! monitor, abort claims, published copies. The GSE lock and the gate are only
//! ever tried under the turn. Nothing is read from or written to a disk, and no
//! other lock is waited for, under the last three.
//!
//! A manual Abort's own outputs (supply close, dump and vent open) do not wait
//! for the lock either while it is held: they are decided from what was
//! published at its last release (`manual_stop_safing`).
use super::{automatic_abort::AbortOwner, overpressure, *};
use std::sync::{LockResult, MutexGuard, PoisonError, TryLockError, TryLockResult};

pub struct Owner {
    runtime: crate::unpin::Mutex<Runtime>,
    pub(crate) abort: Arc<AbortOwner>,
    pub(crate) overpressure: overpressure::Protection,
    /// An engine sequence owned the fill-side valves at the last release of
    /// the lock (`manual_stop_safing` reads it without the lock).
    sequence_owns_valves: std::sync::atomic::AtomicBool,
}
impl Owner {
    pub(crate) fn new(runtime: Runtime) -> Self {
        let owner = Self {
            abort: Default::default(),
            overpressure: runtime.overpressure.clone(),
            sequence_owns_valves: Default::default(),
            runtime: crate::unpin::Mutex::new(runtime),
        };
        drop(owner.lock());
        owner
    }
    /// The runtime, with any automatic abort claimed since already in it: a
    /// claim is in every snapshot saved after it, whoever saves.
    pub(crate) fn lock(&self) -> LockResult<Guard<'_>> {
        match self.runtime.lock() {
            Ok(runtime) => Ok(self.entered(runtime)),
            Err(poisoned) => Err(PoisonError::new(self.entered(poisoned.into_inner()))),
        }
    }
    pub(crate) fn try_lock(&self) -> TryLockResult<Guard<'_>> {
        match self.runtime.try_lock() {
            Ok(runtime) => Ok(self.entered(runtime)),
            Err(TryLockError::Poisoned(poisoned)) => {
                Err(TryLockError::Poisoned(PoisonError::new(self.entered(poisoned.into_inner()))))
            }
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
        }
    }
    /// Whether an engine sequence (not a ColdFlow session, which handles its
    /// own Abort) owned the fill-side valves when the lock was last released.
    pub(crate) fn sequence_owned_valves(&self) -> bool {
        self.sequence_owns_valves.load(std::sync::atomic::Ordering::Acquire)
    }
    #[cfg(test)]
    pub(crate) fn abort_watch_cycles_for_test(&self) -> usize {
        self.abort.cycles.load(std::sync::atomic::Ordering::Acquire)
    }
    fn entered<'a>(&'a self, mut runtime: MutexGuard<'a, Runtime>) -> Guard<'a> {
        self.overpressure.adopt(&mut runtime);
        self.abort.adopt(&mut runtime);
        Guard { runtime, owner: self }
    }
}
impl Default for Owner {
    fn default() -> Self {
        Self::new(Runtime::default())
    }
}

/// The held GSE lock. What the automatic abort reads without it is published
/// as it is released.
pub(crate) struct Guard<'a> {
    runtime: MutexGuard<'a, Runtime>,
    owner: &'a Owner,
}
impl std::ops::Deref for Guard<'_> {
    type Target = Runtime;
    fn deref(&self) -> &Runtime {
        &self.runtime
    }
}
impl std::ops::DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut Runtime {
        &mut self.runtime
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        // A replaced runtime keeps the station's one overpressure monitor.
        if !self.runtime.overpressure.same(&self.owner.overpressure) {
            self.runtime.overpressure = self.owner.overpressure.clone();
        }
        self.owner.abort.publish(&self.runtime);
        self.owner.overpressure.publish(&self.runtime);
        self.owner.sequence_owns_valves.store(
            self.runtime.engine.active() && !cold_flow::runtime_active(&self.runtime),
            std::sync::atomic::Ordering::Release,
        );
    }
}
