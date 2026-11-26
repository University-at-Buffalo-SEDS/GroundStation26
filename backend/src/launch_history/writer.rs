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
enum PrefixState {
    Pending,
    Durable,
    Failed(String),
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
                        #[cfg(test)]
                        crate::web::clock_worker_tests::pause_journal_before_save(&path);
                        #[cfg(test)]
                        crate::disk_stall::wait(crate::disk_stall::Site::Journal);
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
    /// Inspect the actual queued/writing/durable prefix. No IO or callback.
    fn prefix_state(&self, ticket: u64) -> Result<PrefixState> {
        let state = self.inner.state.lock().unwrap();
        if state.next != ticket {
            return Err(anyhow!("Launch journal prefix changed"));
        }
        if state.writing || state.snapshot.is_some() {
            return Ok(PrefixState::Pending);
        }
        if state.written >= ticket {
            return Ok(PrefixState::Durable);
        }
        if let Some((failed, message)) = &state.error
            && *failed >= ticket
        {
            return Ok(PrefixState::Failed(message.clone()));
        }
        Err(anyhow!("Launch journal prefix is not durably complete"))
    }
    /// Final reset prefix inspection: RAM only, never waits for a save. It
    /// waits for the state lock, which every holder (the writer thread among
    /// them) keeps for a few field updates only, never across disk IO and
    /// never with another lock; a try-lock here refused a recovery whenever
    /// the writer thread was recording a finished save.
    pub(super) fn try_quiescent_through(&self, ticket: u64) -> Result<bool> {
        let state = self.inner.state.lock().map_err(|_| anyhow!("Launch writer busy"))?;
        if state.next != ticket { return Err(anyhow!("Launch journal prefix changed")); }
        if state.writing || state.snapshot.is_some() { return Ok(false); }
        if state.written >= ticket { return Ok(true); }
        Err(anyhow!("Launch journal prefix is not durably complete"))
    }
    pub(super) fn quiescent_through(&self, ticket: u64) -> Result<bool> {
        match self.prefix_state(ticket)? {
            PrefixState::Pending => Ok(false),
            PrefixState::Durable => Ok(true),
            PrefixState::Failed(message) => Err(anyhow!(message)),
        }
    }
    pub(super) fn failed_terminal_prefix(&self, ticket: u64) -> Result<()> {
        match self.prefix_state(ticket)? {
            PrefixState::Failed(_) => Ok(()),
            _ => Err(anyhow!(
                "Only a terminal failed prefix can be explicitly retried"
            )),
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
    /// A callback may inspect health, but must never wait for this writer.
    pub(super) fn try_healthy(&self) -> bool {
        self.inner
            .state
            .try_lock()
            .is_ok_and(|state| !state.stopped && state.error.is_none())
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

#[cfg(test)]
mod tests {
    use super::*;
    /// The prefix check meets the writer thread recording a save: it waits
    /// for those field updates instead of refusing as "busy".
    #[test]
    fn the_prefix_check_waits_for_the_writer_bookkeeping() {
        let path = std::env::temp_dir().join(format!("gs-writer-test-{:032x}.json", rand::random::<u128>()));
        let writer = Writer::start(path.clone(), false).unwrap();
        let (held, holding) = std::sync::mpsc::channel();
        let inner = writer.inner.clone();
        let holder = std::thread::spawn(move || {
            let state = inner.state.lock().unwrap();
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(state);
        });
        holding.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(writer.try_quiescent_through(0).unwrap(), "nothing pending: quiescent");
        holder.join().unwrap();
        drop(writer);
        let _ = std::fs::remove_file(path);
    }
}
