//! Closing input ownership is separate from ordinary command permission/gates.
//! Existing manual handlers retain their authority while they finish; new input
//! cannot report acceptance after this manager becomes terminal.
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, watch};
tokio::task_local! { static CURRENT: usize; }
pub(crate) struct Registry {
    state: Mutex<State>,
    closing: watch::Sender<bool>,
    changed: Notify,
}
#[derive(Default)]
struct State {
    closed: bool,
    connections: usize,
    handlers: usize,
    poisoned: bool,
    busy_at_seal: bool,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            closing: watch::channel(false).0,
            changed: Notify::new(),
        }
    }
}
#[derive(Clone, Copy)]
enum Kind {
    Connection,
    Handler,
}
pub(crate) struct Lease {
    registry: Arc<Registry>,
    kind: Kind,
    completed: bool,
}
impl Registry {
    pub(crate) fn connection(self: &Arc<Self>) -> Option<Lease> {
        self.enter(Kind::Connection)
    }
    pub(crate) fn handler(self: &Arc<Self>) -> Option<Lease> {
        self.enter(Kind::Handler)
    }
    fn enter(self: &Arc<Self>, kind: Kind) -> Option<Lease> {
        let nested = matches!(kind, Kind::Handler)
            && CURRENT
                .try_with(|owner| *owner == Arc::as_ptr(self) as usize)
                .unwrap_or(false);
        let mut state = self.state.lock().unwrap();
        if state.closed && !nested {
            return None;
        }
        let count = match kind {
            Kind::Connection => &mut state.connections,
            Kind::Handler => &mut state.handlers,
        };
        if matches!(kind, Kind::Connection) && *count >= 128 {
            return None;
        }
        *count = count.checked_add(1)?;
        Some(Lease {
            registry: self.clone(),
            kind,
            completed: false,
        })
    }
    pub(crate) fn seal(&self) {
        {
            let mut state = self.state.lock().unwrap();
            if !state.closed {
                state.closed = true;
                state.busy_at_seal = state.handlers != 0;
            }
        }
        self.closing.send_replace(true);
        self.changed.notify_waiters();
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.closing.subscribe()
    }
    pub(crate) fn run_sync<T>(self: &Arc<Self>, work: impl FnOnce() -> T) -> Option<T> {
        let lease = self.handler()?;
        let result = work();
        lease.finish();
        Some(result)
    }
    pub(crate) async fn wait_terminal(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                if state.connections == 0 && state.handlers == 0 {
                    return state.closed && !state.poisoned && !state.busy_at_seal;
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return false;
            }
        }
    }
}
impl Lease {
    pub(crate) fn finish(mut self) {
        self.completed = true;
    }
    pub(crate) async fn run<T>(mut self, work: impl std::future::Future<Output = T>) -> T {
        let owner = Arc::as_ptr(&self.registry) as usize;
        CURRENT
            .scope(owner, async {
                let result = work.await;
                self.completed = true;
                result
            })
            .await
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.registry.state.lock().unwrap();
        if !self.completed {
            state.poisoned = true;
        }
        match self.kind {
            Kind::Connection => state.connections -= 1,
            Kind::Handler => state.handlers -= 1,
        }
        drop(state);
        self.registry.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test]
    async fn idle_connection_requires_terminal_receipt_but_does_not_force_unclean() {
        let registry = Arc::new(Registry::default());
        let connection = registry.connection().unwrap();
        registry.seal();
        assert!(registry.handler().is_none());
        assert!(registry.connection().is_none());
        assert!(!registry.wait_terminal(Duration::from_millis(1)).await);
        connection.finish();
        assert!(registry.wait_terminal(Duration::from_millis(1)).await);
    }
    #[tokio::test]
    async fn existing_handler_can_finish_nested_manual_work_but_new_task_cannot_enter() {
        let registry = Arc::new(Registry::default());
        let handler = registry.handler().unwrap();
        let work = registry.clone();
        handler
            .run(async move {
                work.seal();
                let nested = work
                    .handler()
                    .expect("closing rejected already owned handler");
                nested.finish();
                let unrelated = work.clone();
                assert!(
                    tokio::spawn(async move { unrelated.handler().is_none() })
                        .await
                        .unwrap()
                );
            })
            .await;
        assert!(
            !registry.wait_terminal(Duration::from_millis(1)).await,
            "active-at-seal work was silently called safe before SDK drain"
        );
    }
    #[tokio::test]
    async fn cancellation_and_panic_are_not_terminal_success_receipts() {
        let registry = Arc::new(Registry::default());
        let handler = registry.handler().unwrap();
        let task = tokio::spawn(handler.run(std::future::pending::<()>()));
        task.abort();
        assert!(task.await.is_err());
        registry.seal();
        assert!(!registry.wait_terminal(Duration::from_millis(1)).await);
        let registry = Arc::new(Registry::default());
        let handler = registry.handler().unwrap();
        assert!(
            tokio::spawn(handler.run(async { panic!("mock ingress panic") }))
                .await
                .is_err()
        );
        registry.seal();
        assert!(!registry.wait_terminal(Duration::from_millis(1)).await);
    }
    #[tokio::test]
    async fn gpio_lifetime_gate_preserves_operational_stop_without_ordinary_queue() {
        let registry = Arc::new(Registry::default());
        let intent = crate::manual_abort_intent::ManualAbortIntent::default();
        registry.run_sync(|| intent.request()).unwrap();
        let epoch = intent.epoch();
        assert_eq!(epoch, 1);
        registry.seal();
        assert!(registry.run_sync(|| intent.request()).is_none());
        assert_eq!(intent.epoch(), epoch);
        assert!(registry.wait_terminal(Duration::from_millis(1)).await);
    }
}
