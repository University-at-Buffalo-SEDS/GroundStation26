use super::{Disk, atomic_save};
use anyhow::{Result, anyhow};
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

struct Pending {
    snapshot: Option<(u64, Disk)>,
    next: u64,
    written: u64,
    writing: bool,
    error: Option<(u64, String)>,
    stopped: bool,
}
type Reporter = Arc<dyn Fn(String) + Send + Sync>;
struct Inner {
    state: Mutex<Pending>,
    changed: Condvar,
    reporter: Mutex<Option<Reporter>>,
}
pub struct Writer {
    inner: Arc<Inner>,
}
impl Writer {
    pub fn start(path: PathBuf, mut preserve_unreadable: bool) -> Result<Arc<Self>> {
        let inner = Arc::new(Inner {
            state: Mutex::new(Pending {
                snapshot: None,
                next: 0,
                written: 0,
                writing: false,
                error: None,
                stopped: false,
            }),
            changed: Condvar::new(),
            reporter: Mutex::new(None),
        });
        let work = inner.clone();
        std::thread::Builder::new()
            .name("launch-clock-journal".into())
            .spawn(move || {
                loop {
                    let (ticket, data) = {
                        let mut state = work.state.lock().unwrap();
                        loop {
                            if let Some(item) = state.snapshot.take() {
                                state.writing = true;
                                break item;
                            }
                            if state.stopped {
                                return;
                            }
                            state = work.changed.wait(state).unwrap();
                        }
                    };
                    let result = (|| -> Result<()> {
                        if preserve_unreadable && path.exists() {
                            let backup =
                                path.with_extension(format!("unreadable-{}", std::process::id()));
                            std::fs::copy(&path, &backup)?;
                            std::fs::File::open(&backup)?.sync_all()?;
                            preserve_unreadable = false;
                        }
                        atomic_save(&path, &data)
                    })();
                    let report = {
                        let mut state = work.state.lock().unwrap();
                        state.writing = false;
                        match result {
                            Ok(()) => {
                                state.written = state.written.max(ticket);
                                state.error = None;
                                None
                            }
                            Err(error) => {
                                let text = format!(
                                    "Launch clock/recovery history was not saved: {error:#}"
                                );
                                state.error = Some((ticket, text.clone()));
                                Some(text)
                            }
                        }
                    };
                    work.changed.notify_all();
                    if let Some(error) = report {
                        log::error!("{error}");
                        let report = work.reporter.lock().unwrap().clone();
                        if let Some(report) = report {
                            report(error);
                        }
                    }
                }
            })?;
        Ok(Arc::new(Self { inner }))
    }
    pub(super) fn enqueue(&self, snapshot: Disk) -> Result<u64> {
        let mut state = self.inner.state.lock().unwrap();
        if state.stopped {
            return Err(anyhow!("Launch clock writer stopped"));
        }
        state.next = state
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow!("Launch journal sequence exhausted"))?;
        let ticket = state.next;
        // New snapshots contain every previous recovery record and session.
        // Only the latest clock revision is needed; no history event is discarded.
        state.snapshot = Some((ticket, snapshot));
        drop(state);
        self.inner.changed.notify_one();
        Ok(ticket)
    }
    pub fn wait(&self, ticket: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if state.written >= ticket {
                return Ok(());
            }
            if !state.writing
                && state.snapshot.is_none()
                && let Some((failed, error)) = &state.error
                && *failed >= ticket
            {
                return Err(anyhow!(error.clone()));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(anyhow!("Timed out saving launch clock/recovery history"));
            };
            let (next, timeout) = self.inner.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if timeout.timed_out() && state.written < ticket {
                return Err(anyhow!("Timed out saving launch clock/recovery history"));
            }
        }
    }
    pub fn error(&self) -> Option<String> {
        self.inner
            .state
            .lock()
            .unwrap()
            .error
            .as_ref()
            .map(|(_, text)| text.clone())
    }
    pub fn set_reporter(&self, reporter: Arc<dyn Fn(String) + Send + Sync>) {
        *self.inner.reporter.lock().unwrap() = Some(reporter);
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().stopped = true;
        self.inner.changed.notify_one();
    }
}
