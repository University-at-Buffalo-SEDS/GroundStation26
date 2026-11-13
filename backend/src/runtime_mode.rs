//! Process-wide station operating mode, chosen at run time.
//!
//! Normal, HITL, test fire and testing used to be separate builds. One binary
//! now carries every mode and this module holds the one that is in force. The
//! mode and a switch epoch are packed in a single atomic, so any code can read
//! them without locking:
//!
//! - [`current`], [`is_hitl`], [`is_test_fire`], [`is_testing`], [`is_normal`]
//!   replace the old `cfg!(feature = "...")` checks one for one.
//! - [`token`] captures the mode and epoch together. An operation admitted
//!   under one mode keeps its token and calls [`ModeToken::is_current`] (or
//!   [`ModeToken::check`]) before it commits; after a switch the epoch differs
//!   and the operation is refused.
//! - [`switch_to`] changes the mode. It takes the admission gate exclusively,
//!   runs the caller's idle check while holding it, and only then publishes the
//!   new mode with a higher epoch. Code that starts mode-dependent activity holds
//!   [`admission`] (shared) across "check the token, register the activity", so
//!   the idle check and every admission are serialised.
//!
//! The boot mode comes from [`boot_mode`]: `GS_STATION_MODE`, else the saved
//! selection, else normal.
use crate::station_mode::Mode;
use std::sync::{
    RwLock, RwLockReadGuard,
    atomic::{AtomicU64, Ordering},
};

const UNSET: u64 = 0;
const MODE_BITS: u64 = 8;
const MODE_MASK: u64 = (1 << MODE_BITS) - 1;

/// Mode in the low byte (1-based so that zero means "not set yet"), epoch above.
static STATE: AtomicU64 = AtomicU64::new(UNSET);
fn changes() -> &'static tokio::sync::watch::Sender<ModeToken> {
    static CHANGES: std::sync::OnceLock<tokio::sync::watch::Sender<ModeToken>> =
        std::sync::OnceLock::new();
    CHANGES.get_or_init(|| tokio::sync::watch::Sender::new(token()))
}
/// Receives the new token after every switch (and the boot mode first).
/// Tasks that exist for one mode wait on this to start and stop.
pub fn subscribe() -> tokio::sync::watch::Receiver<ModeToken> {
    changes().subscribe()
}
fn publish() {
    let next = token();
    changes().send_if_modified(|current| {
        let changed = *current != next;
        *current = next;
        changed
    });
}

/// Shared by admissions, exclusive for a switch.
static GATE: RwLock<()> = RwLock::new(());

fn code(mode: Mode) -> u64 {
    match mode {
        Mode::Normal => 1,
        Mode::Hitl => 2,
        Mode::TestFire => 3,
        Mode::Testing => 4,
    }
}
fn decode(code: u64) -> Mode {
    match code & MODE_MASK {
        2 => Mode::Hitl,
        3 => Mode::TestFire,
        4 => Mode::Testing,
        _ => Mode::Normal,
    }
}
fn pack(mode: Mode, epoch: u64) -> u64 {
    (epoch << MODE_BITS) | code(mode)
}

/// Parse the ids used by the selection file, the API and `GS_STATION_MODE`.
pub fn parse(id: &str) -> Option<Mode> {
    match id.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "normal" => Some(Mode::Normal),
        "hitl" | "hitl_mode" => Some(Mode::Hitl),
        "test_fire" | "test_fire_mode" | "testfire" => Some(Mode::TestFire),
        "testing" => Some(Mode::Testing),
        _ => None,
    }
}

/// Mode used before anything was set explicitly.
fn default_mode() -> Mode {
    #[cfg(test)]
    {
        if let Some(mode) = std::env::var("GS_TEST_STATION_MODE").ok().as_deref().and_then(parse) {
            return mode;
        }
        Mode::Normal
    }
    #[cfg(not(test))]
    {
        boot_mode()
    }
}

/// `GS_STATION_MODE` (the launcher's or supervisor's explicit choice), else
/// the saved selection, else normal. Read once at start.
#[cfg(not(test))]
pub fn boot_mode() -> Mode {
    if let Ok(id) = std::env::var("GS_STATION_MODE") {
        match parse(&id) {
            Some(mode) => return mode,
            None => eprintln!("Ignoring unknown GS_STATION_MODE={id:?}"),
        }
    }
    crate::station_mode_switch::saved_mode().unwrap_or(Mode::Normal)
}

fn load() -> u64 {
    #[cfg(test)]
    if let Some(state) = test_support::override_state() {
        return state;
    }
    let state = STATE.load(Ordering::Acquire);
    if state != UNSET {
        return state;
    }
    let initial = pack(default_mode(), 1);
    match STATE.compare_exchange(UNSET, initial, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => initial,
        Err(existing) => existing,
    }
}

/// Fix the boot mode at start and log it.
pub fn init_boot() -> Mode {
    let mode = default_mode();
    init(mode);
    mode
}

/// Set the boot mode before the station starts. Later changes use [`switch_to`].
pub fn init(mode: Mode) {
    let _gate = GATE.write().unwrap_or_else(|e| e.into_inner());
    let epoch = match STATE.load(Ordering::Acquire) {
        UNSET => 1,
        state => (state >> MODE_BITS) + 1,
    };
    STATE.store(pack(mode, epoch), Ordering::Release);
    drop(_gate);
    publish();
}

/// The mode in force now.
#[inline]
pub fn current() -> Mode {
    decode(load())
}
/// Increases on every switch.
#[inline]
pub fn epoch() -> u64 {
    load() >> MODE_BITS
}
#[inline]
pub fn is_normal() -> bool {
    current() == Mode::Normal
}
#[inline]
pub fn is_hitl() -> bool {
    current() == Mode::Hitl
}
#[inline]
pub fn is_test_fire() -> bool {
    current() == Mode::TestFire
}
#[inline]
pub fn is_testing() -> bool {
    current() == Mode::Testing
}
/// HITL or test fire: the two modes that used to replace normal flight policy.
#[inline]
pub fn is_hitl_or_test_fire() -> bool {
    matches!(current(), Mode::Hitl | Mode::TestFire)
}

/// The mode and epoch an operation was admitted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeToken {
    pub mode: Mode,
    pub epoch: u64,
}
impl ModeToken {
    /// No switch happened since the token was taken.
    pub fn is_current(&self) -> bool {
        *self == token()
    }
    /// `Err` with an operator-facing reason when the mode changed since.
    #[cfg(test)]
    pub fn check(&self) -> Result<(), String> {
        if self.is_current() {
            Ok(())
        } else {
            Err(format!(
                "The station mode changed from {} to {} while this request was in flight; it was not carried out.",
                label(self.mode),
                label(current())
            ))
        }
    }
}
pub fn token() -> ModeToken {
    let state = load();
    ModeToken {
        mode: decode(state),
        epoch: state >> MODE_BITS,
    }
}

/// Short operator-facing name.
pub fn label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "Normal",
        Mode::Hitl => "HITL",
        Mode::TestFire => "Test fire",
        Mode::Testing => "Testing",
    }
}

/// Error text for something that exists only in other modes.
pub fn unavailable(what: &str) -> String {
    format!("{what} is unavailable in {} mode.", label(current()))
}

/// Held (shared) while code checks the mode and registers mode-dependent
/// activity, so that a switch cannot run between the two.
pub fn admission() -> RwLockReadGuard<'static, ()> {
    GATE.read().unwrap_or_else(|e| e.into_inner())
}

/// Switch to `target` if `admit` (run while every admission is excluded)
/// allows it, then run `apply` with the new token before admissions reopen
/// (reset mode-dependent state there). Returns the new token. Neither closure
/// may take [`admission`] or block on async work.
pub fn switch_to(
    target: Mode,
    admit: impl FnOnce() -> Result<(), String>,
    apply: impl FnOnce(ModeToken),
) -> Result<ModeToken, String> {
    #[cfg(test)]
    if test_support::override_state().is_some() {
        let token = test_support::switch_override(target, admit)?;
        apply(token);
        return Ok(token);
    }
    let _gate = GATE.write().unwrap_or_else(|e| e.into_inner());
    let before = load();
    if decode(before) == target {
        return Err(format!("{} mode is already active.", label(target)));
    }
    admit()?;
    let next = pack(target, (before >> MODE_BITS) + 1);
    STATE.store(next, Ordering::Release);
    let token = ModeToken {
        mode: target,
        epoch: next >> MODE_BITS,
    };
    apply(token);
    drop(_gate);
    publish();
    Ok(token)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A test can run with its own mode on its own thread (current-thread
    //! runtimes keep spawned tasks on that thread) without disturbing other
    //! tests, which read the process mode from `GS_TEST_STATION_MODE`.
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static OVERRIDE: Cell<Option<u64>> = const { Cell::new(None) };
    }
    pub(super) fn override_state() -> Option<u64> {
        OVERRIDE.with(Cell::get)
    }
    pub(super) fn switch_override(
        target: Mode,
        admit: impl FnOnce() -> Result<(), String>,
    ) -> Result<ModeToken, String> {
        let before = override_state().expect("override set");
        if decode(before) == target {
            return Err(format!("{} mode is already active.", label(target)));
        }
        admit()?;
        let next = pack(target, (before >> MODE_BITS) + 1);
        OVERRIDE.with(|cell| cell.set(Some(next)));
        Ok(ModeToken {
            mode: target,
            epoch: next >> MODE_BITS,
        })
    }
    /// Restores the previous override when dropped.
    pub(crate) struct ThreadMode(Option<u64>);
    impl Drop for ThreadMode {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.0));
        }
    }
    /// Run this thread in `mode` until the guard drops.
    pub(crate) fn thread_mode(mode: Mode) -> ThreadMode {
        let previous = override_state();
        OVERRIDE.with(|cell| cell.set(Some(pack(mode, 1))));
        ThreadMode(previous)
    }

    /// True when this test belongs to the suite of the mode in force, i.e.
    /// it was compiled only into that build before modes moved to run time.
    /// A test that returns early on `false` is reported as skipped in the
    /// file named by `GS_TEST_SKIP_LOG` (used to compare suites with the old
    /// per-build lists).
    #[track_caller]
    pub(crate) fn suite(allowed: &[Mode]) -> bool {
        let mode = current();
        let ok = allowed.contains(&mode);
        if !ok {
            record_skip();
        }
        ok
    }
    #[track_caller]
    fn record_skip() {
        let Some(path) = std::env::var_os("GS_TEST_SKIP_LOG") else {
            return;
        };
        let name = std::thread::current()
            .name()
            .map(str::to_owned)
            .unwrap_or_else(|| std::panic::Location::caller().to_string());
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            // One write per line: parallel tests append to the same file.
            let _ = file.write_all(format!("{name}\n").as_bytes());
        }
    }
}

/// Skip the rest of a test unless the station mode is one of the given modes.
/// Replaces `#[cfg(feature = "...")]` on tests that belonged to one build.
#[cfg(test)]
#[macro_export]
macro_rules! mode_suite {
    ($($mode:ident),+ $(,)?) => {
        if !$crate::runtime_mode::test_support::suite(&[$($crate::station_mode::Mode::$mode),+]) {
            return Default::default();
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_bumps_the_epoch_and_invalidates_old_tokens() {
        let _mode = test_support::thread_mode(Mode::Normal);
        let before = token();
        assert!(before.is_current());
        let after = switch_to(Mode::Hitl, || Ok(()), |_| {}).unwrap();
        assert_eq!(current(), Mode::Hitl);
        assert!(is_hitl());
        assert_eq!(after.epoch, before.epoch + 1);
        assert!(!before.is_current());
        assert!(before.check().unwrap_err().contains("changed from Normal to HITL"));
        assert!(after.is_current());
    }

    #[test]
    fn refused_admission_leaves_mode_and_epoch_unchanged() {
        let _mode = test_support::thread_mode(Mode::TestFire);
        let before = token();
        let error = switch_to(Mode::Normal, || Err("a GSE sequence is running".into()), |_| panic!("not applied")).unwrap_err();
        assert_eq!(error, "a GSE sequence is running");
        assert_eq!(token(), before);
        assert!(switch_to(Mode::TestFire, || Ok(()), |_| {}).unwrap_err().contains("already active"));
    }

    #[test]
    fn ids_parse() {
        assert_eq!(parse("test-fire"), Some(Mode::TestFire));
        assert_eq!(parse("hitl_mode"), Some(Mode::Hitl));
        assert_eq!(parse("Normal"), Some(Mode::Normal));
        assert_eq!(parse("flight"), None);
        for mode in [Mode::Normal, Mode::Hitl, Mode::TestFire, Mode::Testing] {
            assert_eq!(decode(pack(mode, 77)), mode);
        }
    }
}
