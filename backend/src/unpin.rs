//! Waiting for a lock without holding a runtime worker.
//!
//! The station's state locks are held across disk writes (a Launch saves its
//! intent under the dispatch gate and the GSE lock). On a Pi the runtime has a
//! few workers; a handful of tasks waiting for such a lock used to hold all of
//! them, and then nothing ran: no HTTP request, WebSocket frame or board query
//! was served until the disk answered, an operator Abort among them.
//!
//! [`Mutex`] is `std::sync::Mutex` (same guards, ordering and poisoning). A
//! free lock costs the same. A held one is waited for inside
//! `block_in_place`, which hands this worker's other tasks to another thread.
use std::sync::{LockResult, MutexGuard, TryLockError, TryLockResult};

/// Run work that may wait (a disk write, a held lock) without holding a
/// runtime worker.
pub(crate) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    // block_in_place is refused on a current-thread runtime (unit tests and
    // the Abort threads), which has no worker to hand over.
    let worker = tokio::runtime::Handle::try_current()
        .is_ok_and(|runtime| runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if worker { tokio::task::block_in_place(work) } else { work() }
}

#[derive(Debug, Default)]
pub(crate) struct Mutex<T: ?Sized>(std::sync::Mutex<T>);

impl<T> Mutex<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }
}
impl<T: ?Sized> Mutex<T> {
    pub(crate) fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        match self.0.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => Err(poisoned),
            Err(TryLockError::WouldBlock) => blocking(|| self.0.lock()),
        }
    }
    pub(crate) fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        self.0.try_lock()
    }
    #[cfg(test)]
    pub(crate) fn is_poisoned(&self) -> bool {
        self.0.is_poisoned()
    }
}
impl<T> From<T> for Mutex<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    /// More waiters than workers, and the runtime still runs.
    #[test]
    fn waiters_do_not_hold_the_runtime() {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).build().unwrap();
        let lock = Arc::new(Mutex::new(0));
        let (held, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let holder = {
            let (lock, held, release) = (lock.clone(), held.clone(), release.clone());
            std::thread::spawn(move || {
                let _guard = lock.lock().unwrap();
                held.wait();
                release.wait();
            })
        };
        held.wait();
        let waiters = (0..8)
            .map(|_| {
                let lock = lock.clone();
                runtime.spawn(async move { *lock.lock().unwrap() += 1 })
            })
            .collect::<Vec<_>>();
        std::thread::sleep(Duration::from_millis(50));
        let (ran, answer) = std::sync::mpsc::channel();
        runtime.spawn(async move { ran.send(()).unwrap() });
        let ran = answer.recv_timeout(Duration::from_secs(5));
        release.wait();
        holder.join().unwrap();
        runtime.block_on(async {
            for waiter in waiters {
                waiter.await.unwrap();
            }
        });
        assert_eq!(*lock.lock().unwrap(), 8);
        ran.expect("a task runs while eight wait for the lock");
    }

    #[tokio::test]
    async fn a_current_thread_runtime_waits_in_place() {
        let lock = Arc::new(Mutex::new(()));
        let guard = lock.lock().unwrap();
        let waiter = {
            let lock = lock.clone();
            std::thread::spawn(move || drop(lock.lock().unwrap()))
        };
        assert!(lock.try_lock().is_err());
        drop(guard);
        waiter.join().unwrap();
        blocking(|| ());
    }
}
