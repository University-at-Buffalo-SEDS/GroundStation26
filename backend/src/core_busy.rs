//! One bounded retry for operator actions the SDK refused only because another
//! thread (usually the router worker mid-pass) held its core. Such a refusal
//! admitted nothing, so the same operator intent is offered again. Every
//! attempt re-runs the action's own checks, and callers hold no application
//! guard while waiting between attempts.
use std::time::Duration;

/// How long a never-admitted, core-busy operator offer is retried.
pub(crate) const CORE_BUSY_RETRY_WINDOW: Duration = Duration::from_millis(300);
/// Pause between attempts.
const CORE_BUSY_RETRY_PAUSE: Duration = Duration::from_millis(5);

/// Blocking form, for actions that already run on a blocking thread.
pub(crate) fn retry_blocking<T, E>(
    mut attempt: impl FnMut() -> Result<T, E>,
    busy: impl Fn(&E) -> bool,
) -> Result<T, E> {
    let deadline = std::time::Instant::now() + CORE_BUSY_RETRY_WINDOW;
    loop {
        match attempt() {
            Err(error) if busy(&error) && std::time::Instant::now() < deadline => {
                std::thread::sleep(
                    CORE_BUSY_RETRY_PAUSE.min(deadline.saturating_duration_since(std::time::Instant::now())),
                );
            }
            result => return result,
        }
    }
}

/// Async form, for actions run from a request handler or the command task.
pub(crate) async fn retry<T, E>(
    mut attempt: impl FnMut() -> Result<T, E>,
    busy: impl Fn(&E) -> bool,
) -> Result<T, E> {
    let deadline = tokio::time::Instant::now() + CORE_BUSY_RETRY_WINDOW;
    loop {
        match attempt() {
            Err(error) if busy(&error) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + CORE_BUSY_RETRY_PAUSE).min(deadline),
                )
                .await;
            }
            result => return result,
        }
    }
}

/// `try_tx_queue`'s refusal when another thread holds the SDK core.
pub(crate) fn is_core_busy(error: &sedsnet::TelemetryError) -> bool {
    matches!(error, sedsnet::TelemetryError::Io("router core busy"))
}

/// Test support: a router whose real SDK core a test can hold.
#[cfg(test)]
pub(crate) mod hold {
    use sedsnet::router::{Clock, Router, RouterConfig};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::{Duration, Instant};

    const HOLDER: &str = "test-sdk-core-holder";

    #[derive(Default)]
    struct Gate {
        released: Mutex<bool>,
        changed: Condvar,
        entered: Mutex<Option<mpsc::Sender<()>>>,
    }
    /// An ordinary monotonic clock, except that the holder thread parks inside
    /// it, and therefore inside the SDK core, until released.
    struct HoldClock {
        start: Instant,
        gate: Arc<Gate>,
    }
    impl Clock for HoldClock {
        fn now_ms(&self) -> u64 {
            if std::thread::current().name() == Some(HOLDER)
                && let Some(entered) = self.gate.entered.lock().unwrap().take()
            {
                let _ = entered.send(());
                let released = self.gate.released.lock().unwrap();
                let (released, _) = self
                    .gate
                    .changed
                    .wait_timeout_while(released, Duration::from_secs(5), |done| !*done)
                    .unwrap();
                assert!(*released, "test core holder was never released");
            }
            self.start.elapsed().as_millis() as u64
        }
    }
    pub(crate) fn router(config: RouterConfig) -> (Arc<Router>, Holdable) {
        let gate = Arc::new(Gate::default());
        let router = Arc::new(Router::new_with_clock(
            config,
            Box::new(HoldClock { start: Instant::now(), gate: gate.clone() }),
        ));
        (router.clone(), Holdable { router, gate })
    }
    #[derive(Clone)]
    pub(crate) struct Holdable {
        router: Arc<Router>,
        gate: Arc<Gate>,
    }
    /// The SDK core stays held until this is released or dropped.
    pub(crate) struct Held {
        gate: Arc<Gate>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Holdable {
        /// Takes the router's real SDK core on a holder thread and keeps it.
        pub(crate) fn hold(&self) -> Held {
            let (entered, reached) = mpsc::channel();
            *self.gate.released.lock().unwrap() = false;
            *self.gate.entered.lock().unwrap() = Some(entered);
            let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
            let _ = self.router.enable_managed_variable(ty);
            let seed = sedsnet::packet::Packet::new(
                ty,
                &[crate::telemetry_schema::endpoint("FLIGHT_STATE")],
                &self.router.sender(),
                1,
                Arc::from([0u8]),
            )
            .unwrap();
            let router = self.router.clone();
            let thread = std::thread::Builder::new()
                .name(HOLDER.into())
                .spawn(move || {
                    let _ = router.seed_managed_variable(seed);
                })
                .unwrap();
            reached
                .recv_timeout(Duration::from_secs(2))
                .expect("holder must enter the SDK core");
            assert!(
                matches!(
                    self.router
                        .try_has_pending_reliable_tx_type(crate::telemetry_schema::types::ABORT),
                    Err(sedsnet::router::TryPendingTypeError::CoreBusy)
                ),
                "the real SDK core must be held"
            );
            Held { gate: self.gate.clone(), thread: Some(thread) }
        }
    }
    impl Held {
        /// Ends the hold from another thread after `delay`, as a router pass ends.
        pub(crate) fn release_after(self, delay: Duration) -> std::thread::JoinHandle<()> {
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                drop(self);
            })
        }
    }
    impl Drop for Held {
        fn drop(&mut self) {
            *self.gate.released.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.gate.changed.notify_all();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retries_only_busy_and_only_inside_the_window() {
        let mut calls = 0;
        let result: Result<u8, &str> = retry_blocking(|| { calls += 1; if calls < 3 { Err("busy") } else { Ok(7) } }, |e| *e == "busy");
        assert_eq!((result, calls), (Ok(7), 3));
        let mut calls = 0;
        let result: Result<u8, &str> = retry_blocking(|| { calls += 1; Err("refused") }, |e| *e == "busy");
        assert_eq!((result, calls), (Err("refused"), 1), "a real refusal is never retried");
        let started = std::time::Instant::now();
        let result: Result<u8, &str> = retry_blocking(|| Err("busy"), |e| *e == "busy");
        assert_eq!(result, Err("busy"));
        assert!(started.elapsed() >= CORE_BUSY_RETRY_WINDOW && started.elapsed() < CORE_BUSY_RETRY_WINDOW * 3);
    }
}
