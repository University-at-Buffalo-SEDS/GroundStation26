//! Retained manual stop intent, independent of ordinary command/recording queues.
//! Acceptance transfers ownership to the SDK, not proof of physical actuation.
//! The reset supervisor must also wait for SDK and transport ownership to retire.
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};

#[derive(Default)]
struct Pending {
    epoch: u64,
    pending: bool,
    publishing: Option<u64>,
    renewed_during_claim: bool,
}
#[derive(Default)]
pub(crate) struct ManualAbortIntent {
    state: Mutex<Pending>,
    wake: Notify,
}
impl ManualAbortIntent {
    /// Caller has already authorized this operator. Even epoch exhaustion must
    /// retain Stop; a supervisor must refuse reset when epoch == u64::MAX.
    pub(crate) fn request(&self) -> u64 {
        let epoch = {
            let mut state = self.state.lock().unwrap();
            state.epoch = state.epoch.saturating_add(1);
            state.pending = true;
            state.renewed_during_claim |= state.publishing.is_some();
            state.epoch
        };
        self.wake.notify_one();
        epoch
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.state.lock().unwrap().epoch
    }
    pub(crate) fn pending_epoch(&self) -> Option<u64> {
        let state = self.state.lock().unwrap();
        state.pending.then_some(state.epoch)
    }
    /// Final launch linearization only. `admit` may enqueue prebuilt SDK
    /// packets; it must never flush, await, log, or take GSE/journal/DB locks.
    pub(crate) fn admit_if_unchanged<T>(&self, epoch: u64, admit: impl FnOnce() -> T) -> Option<T> {
        let state = self.state.lock().unwrap();
        if epoch == u64::MAX
            || state.epoch == u64::MAX
            || state.epoch != epoch
            || state.pending
            || state.publishing.is_some()
        {
            return None;
        }
        Some(admit())
    }
    #[cfg(test)]
    pub(crate) fn exhaust_epoch_for_test(&self) {
        let mut state = self.state.lock().unwrap();
        assert!(!state.pending && state.publishing.is_none());
        state.epoch = u64::MAX;
    }
    pub(crate) fn try_claim(self: &Arc<Self>) -> Option<Claim> {
        let mut state = self.state.lock().unwrap();
        if !state.pending || state.publishing.is_some() {
            return None;
        }
        let epoch = state.epoch;
        state.publishing = Some(epoch);
        state.renewed_during_claim = false;
        Some(Claim {
            owner: self.clone(),
            epoch,
            accepted: false,
        })
    }
}

pub(crate) struct Claim {
    owner: Arc<ManualAbortIntent>,
    epoch: u64,
    accepted: bool,
}
impl Claim {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
    /// Call only after SDK admission succeeds. A newer request remains pending.
    pub(crate) fn accepted(mut self) {
        self.accepted = true;
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        {
            let mut state = self.owner.state.lock().unwrap();
            debug_assert_eq!(state.publishing, Some(self.epoch));
            state.publishing = None;
            if self.accepted && state.epoch == self.epoch && !state.renewed_during_claim {
                state.pending = false;
            }
        }
        self.owner.wake.notify_one();
    }
}

/// Spawn separately from the ordinary command worker. `admit` must transfer
/// to SDK ownership before optional UI/database/GSE bookkeeping; it executes
/// without an intent mutex held. False means no ownership transfer occurred.
pub(crate) async fn service(
    intent: Arc<ManualAbortIntent>,
    mut shutdown: broadcast::Receiver<()>,
    mut admit: impl FnMut(u64) -> bool,
) {
    loop {
        match shutdown.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            _ => return,
        }
        if let Some(claim) = intent.try_claim() {
            if admit(claim.epoch()) {
                claim.accepted();
            } else {
                drop(claim);
                // A refused queue cannot erase Stop or create a busy retry loop.
                tokio::select! {
                    biased;
                    _ = shutdown.recv() => return,
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = shutdown.recv() => return,
                _ = intent.wake.notified() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn admission_failure_and_newer_request_are_not_erased() {
        let intent = Arc::new(ManualAbortIntent::default());
        assert_eq!(intent.request(), 1);
        let first = intent.try_claim().unwrap();
        assert!(intent.try_claim().is_none());
        drop(first);
        assert_eq!(intent.pending_epoch(), Some(1));
        let retry = intent.try_claim().unwrap();
        assert_eq!(intent.request(), 2);
        retry.accepted();
        assert_eq!(intent.pending_epoch(), Some(2));
        intent.try_claim().unwrap().accepted();
        assert_eq!(intent.pending_epoch(), None);
        assert_eq!(intent.epoch(), 2);
    }

    #[test]
    fn exhausted_epoch_preserves_stop_and_remains_unresettable() {
        let intent = Arc::new(ManualAbortIntent::default());
        intent.state.lock().unwrap().epoch = u64::MAX;
        assert_eq!(intent.request(), u64::MAX);
        assert_eq!(intent.pending_epoch(), Some(u64::MAX));
        let first = intent.try_claim().unwrap();
        intent.request();
        first.accepted();
        assert_eq!(
            intent.pending_epoch(),
            Some(u64::MAX),
            "saturation cannot hide a new Stop"
        );
        intent.try_claim().unwrap().accepted();
        assert_eq!(intent.pending_epoch(), None);
        assert_eq!(intent.epoch(), u64::MAX);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn independent_publisher_retries_without_holding_state_or_ordinary_queue() {
        let intent = Arc::new(ManualAbortIntent::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (stop, shutdown) = broadcast::channel(1);
        let (accepted, got) = tokio::sync::oneshot::channel();
        let task_intent = intent.clone();
        let inspect = intent.clone();
        let observed = calls.clone();
        // An unrelated full command channel is never read by this publisher.
        let (ordinary, mut rx) = tokio::sync::mpsc::channel(1);
        ordinary.send("blocked ordinary command").await.unwrap();
        let task = tokio::spawn(async move {
            let mut accepted = Some(accepted);
            service(task_intent, shutdown, move |epoch| {
                assert!(inspect.state.try_lock().is_ok());
                assert_eq!(epoch, 1);
                if observed.fetch_add(1, Ordering::SeqCst) < 2 {
                    return false;
                }
                let _ = accepted.take().unwrap().send(());
                true
            })
            .await;
        });
        intent.request();
        tokio::time::timeout(Duration::from_secs(1), got)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..100 {
            if intent.pending_epoch().is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(intent.pending_epoch(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(rx.try_recv().unwrap(), "blocked ordinary command");
        stop.send(()).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_does_not_drop_an_unaccepted_stop() {
        let intent = Arc::new(ManualAbortIntent::default());
        intent.request();
        let (stop, shutdown) = broadcast::channel(1);
        stop.send(()).unwrap();
        service(intent.clone(), shutdown, |_| panic!("already stopped")).await;
        assert_eq!(intent.pending_epoch(), Some(1));
    }
}
