//! Test seam: a disk that stops answering. Each durable write a Launch makes
//! calls `wait` where its disk wait would be; production builds contain none
//! of it. A stall is process-wide, so tests that arm one run in their own
//! process (`operator_queue::isolated_native_case`).
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    /// The GSE qualification record (`operational::Saved::save`).
    Qualification,
    /// The launch clock/recovery journal writer thread.
    Journal,
    /// The checklist document.
    Checklists,
    /// The network-variable cache.
    Variables,
    /// Every write to the log files.
    Log,
}
#[derive(Default)]
struct Held {
    site: Option<Site>,
    entered: usize,
}
static HELD: Mutex<Held> = Mutex::new(Held { site: None, entered: 0 });
static CHANGED: Condvar = Condvar::new();
/// No stall outlives this, so a broken test fails instead of hanging.
const LIMIT: Duration = Duration::from_secs(30);

pub(crate) fn wait(site: Site) {
    let mut held = HELD.lock().unwrap();
    if held.site != Some(site) {
        return;
    }
    held.entered += 1;
    CHANGED.notify_all();
    let deadline = Instant::now() + LIMIT;
    while held.site == Some(site) {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { return };
        held = CHANGED.wait_timeout(held, left).unwrap().0;
    }
}
/// The disk behind `site` answers nothing until this is dropped.
pub(crate) struct Stall;
impl Stall {
    pub(crate) fn begin(site: Site) -> Self {
        *HELD.lock().unwrap() = Held { site: Some(site), entered: 0 };
        Self
    }
    /// Wait until a writer is actually held by the stall.
    pub(crate) fn entered(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut held = HELD.lock().unwrap();
        while held.entered == 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return false };
            held = CHANGED.wait_timeout(held, left).unwrap().0;
        }
        true
    }
}
impl Drop for Stall {
    fn drop(&mut self) {
        HELD.lock().unwrap().site = None;
        CHANGED.notify_all();
    }
}
