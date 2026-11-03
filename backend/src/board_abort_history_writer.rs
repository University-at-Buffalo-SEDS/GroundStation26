//! One bounded coalescing writer. Application callbacks publish RAM only.
use crate::board_abort_history::{QuiescenceReceipt, Restoration, Snapshot, Store};
use std::{
    io,
    path::Path,
    sync::{Arc, Condvar, Mutex},
    thread,
};
use tokio::sync::Notify;

#[derive(Clone)]
pub(crate) struct Publisher(Arc<Shared>);
struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    changed: Arc<Notify>,
}
#[derive(Default)]
struct Mailbox {
    latest: Option<Snapshot>,
    finishing: Option<(Snapshot, bool)>,
    failure: Option<String>,
    closed: bool,
}
pub(crate) struct Owner {
    publisher: Publisher,
    task: Option<thread::JoinHandle<io::Result<()>>>,
}

impl Publisher {
    pub(crate) fn publish(&self, snapshot: Snapshot) {
        let mut mailbox = self.0.mailbox.lock().unwrap();
        if mailbox.closed {
            return;
        }
        if mailbox
            .latest
            .is_none_or(|old| snapshot.revision > old.revision)
        {
            mailbox.latest = Some(snapshot);
        }
        self.0.wake.notify_one();
    }
    pub(crate) fn failure(&self) -> Option<String> {
        self.0.mailbox.lock().unwrap().failure.clone()
    }
}

impl Owner {
    pub(crate) fn open(
        directory: &Path,
        identity: [u8; 16],
        changed: Arc<Notify>,
    ) -> io::Result<(Restoration, Publisher, Self)> {
        let mut store = if directory.exists() {
            Store::open(directory, identity)?
        } else {
            Store::provision_uncertain(
                directory,
                identity,
                crate::history_policy::PROSPECTIVE_ALL_BOARDS,
            )?
        };
        // This synchronous bootstrap is before producers and outside app locks.
        let restoration = store.begin_session(crate::history_policy::PROSPECTIVE_ALL_BOARDS)?;
        let publisher = Publisher(Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            changed,
        }));
        let worker = publisher.clone();
        let task = thread::Builder::new()
            .name("board_abort_history".into())
            .spawn(move || {
                let mut saved_revision = restoration.snapshot.revision;
                let result = loop {
                    let (latest, finishing) = {
                        let mut mailbox = worker.0.mailbox.lock().unwrap();
                        while mailbox.latest.is_none()
                            && mailbox.finishing.is_none()
                            && !mailbox.closed
                        {
                            mailbox = worker.0.wake.wait(mailbox).unwrap();
                        }
                        if mailbox.closed && mailbox.finishing.is_none() {
                            break Ok(());
                        }
                        (mailbox.latest.take(), mailbox.finishing.take())
                    };
                    // No shared mailbox/register/Stop mutex is held during IO.
                    if let Some(snapshot) = latest.filter(|s| s.revision > saved_revision) {
                        if let Err(error) = store.save(snapshot) {
                            break Err(error);
                        }
                        saved_revision = snapshot.revision;
                    }
                    if let Some((snapshot, quiesced)) = finishing {
                        if snapshot.revision > saved_revision {
                            if let Err(error) = store.save(snapshot) {
                                break Err(error);
                            }
                            saved_revision = snapshot.revision;
                        }
                        if !quiesced {
                            break Ok(());
                        } // Keep Active on disk.
                        if saved_revision != snapshot.revision {
                            break Err(io::Error::other("final snapshot superseded"));
                        }
                        let receipt =
                            match QuiescenceReceipt::checked(snapshot.revision, true, true, true) {
                                Ok(receipt) => receipt,
                                Err(error) => break Err(error),
                            };
                        break store.seal_clean(receipt);
                    }
                };
                let mut mailbox = worker.0.mailbox.lock().unwrap();
                mailbox.closed = true;
                if let Err(error) = &result {
                    mailbox.failure = Some(error.to_string());
                }
                drop(mailbox);
                worker.0.changed.notify_one();
                result
            })?;
        Ok((
            restoration,
            publisher.clone(),
            Self {
                publisher,
                task: Some(task),
            },
        ))
    }
    /// Main runs this through spawn_blocking after actual producer joins.
    pub(crate) fn finish(mut self, snapshot: Snapshot, quiesced: bool) -> io::Result<()> {
        {
            let mut mailbox = self.publisher.0.mailbox.lock().unwrap();
            mailbox.finishing = Some((snapshot, quiesced));
            self.publisher.0.wake.notify_one();
        }
        self.task
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("history writer panicked"))?
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        // Never block Drop or create a fabricated clean receipt on early exit.
        if self.task.is_some() {
            let mut mailbox = self.publisher.0.mailbox.lock().unwrap();
            mailbox.closed = true;
            self.publisher.0.wake.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{board_abort::Runtime, types::Board};
    use std::time::{Duration, Instant};
    fn directory() -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("gs-abort-owner-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&directory).unwrap();
        directory.join("history")
    }
    fn open(path: &Path) -> (Arc<Runtime>, Owner) {
        let changed = Arc::new(Notify::new());
        let (restored, publisher, owner) = Owner::open(path, [42; 16], changed.clone()).unwrap();
        let runtime = Arc::new(Runtime::restored(restored, publisher, changed));
        runtime.publish_initial_history();
        (runtime, owner)
    }
    fn cleanup(path: &Path) {
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn durable_clean_restart_does_not_require_unseen_optional_boards() {
        let path = directory();
        let (runtime, owner) = open(&path);
        assert_eq!(runtime.history_snapshot().unknown,0x7f,"unproven first installation must be uncertain");
        for board in crate::board_abort::BOARDS {
            let after=Instant::now();
            runtime.observe_status(board,42,3,11,Some(1),false,true,after,runtime.query_revision(board),Instant::now(),123456);
        }
        assert_eq!(runtime.history_snapshot().unknown, 0);
        owner
            .finish(runtime.freeze_history_after_quiescence(), true)
            .unwrap();
        drop(runtime);
        let (runtime, owner) = open(&path);
        assert_eq!(runtime.history_snapshot().unknown, 0);
        assert_eq!(runtime.history_snapshot().actual_abort, 0);
        owner
            .finish(runtime.freeze_history_after_quiescence(), true)
            .unwrap();
        cleanup(&path);
    }
    #[test]
    fn durable_unclean_restart_preserves_actual_abort_and_unknown_separately() {
        let path = directory();
        let (runtime, owner) = open(&path);
        runtime.observe_abort(Board::DaqBoard, Instant::now(), 123456);
        owner.finish(runtime.history_snapshot(), false).unwrap();
        let (runtime, owner) = open(&path);
        assert_eq!(runtime.history_snapshot().unknown, 0x7f);
        assert_eq!(runtime.history_snapshot().actual_abort, 0x40);
        assert_eq!(runtime.snapshot(0).active_errors[0].timestamp_ms, 123456);
        // Old/weak proof cannot clear Unknown or the retained actual Abort.
        let old = Instant::now() - Duration::from_secs(1);
        runtime.observe_status(
            Board::DaqBoard,
            42,
            3,
            10,
            Some(1),
            false,
            true,
            old,
            runtime.query_revision(Board::DaqBoard),
            Instant::now(),
            999999,
        );
        assert_eq!(runtime.history_snapshot().unknown, 0x7f);
        assert_eq!(runtime.history_snapshot().actual_abort, 0x40);
        let fresh = Instant::now();
        runtime.observe_status(
            Board::DaqBoard,
            42,
            3,
            11,
            Some(1),
            false,
            false,
            fresh,
            runtime.query_revision(Board::DaqBoard),
            Instant::now(),
            999999,
        );
        assert_eq!(runtime.history_snapshot().unknown, 0x7f);
        runtime.observe_status(
            Board::DaqBoard,
            42,
            3,
            12,
            Some(1),
            false,
            true,
            fresh,
            runtime.query_revision(Board::DaqBoard),
            Instant::now(),
            999999,
        );
        assert_eq!(runtime.history_snapshot().unknown, 0x3f);
        assert_eq!(runtime.history_snapshot().actual_abort, 0);
        // Clean restart must retain all still-unresolved history, not reset it.
        owner
            .finish(runtime.freeze_history_after_quiescence(), true)
            .unwrap();
        let (runtime, owner) = open(&path);
        assert_eq!(runtime.history_snapshot().unknown, 0x3f);
        owner
            .finish(runtime.freeze_history_after_quiescence(), true)
            .unwrap();
        cleanup(&path);
    }
    #[test]
    fn writer_failure_is_visible_and_cannot_clean_seal() {
        let path = directory();
        let (runtime, owner) = open(&path);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let bytes = std::fs::read(path.join("state")).unwrap();
            if bytes.len() == 112 && u64::from_le_bytes(bytes[32..40].try_into().unwrap()) >= 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "initial owned writer did not persist"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // Deterministic real I/O refusal: atomic temp path is not a regular file.
        std::fs::create_dir(path.join(".state.pending")).unwrap();
        runtime.observe_abort(Board::DaqBoard, Instant::now(), 123456);
        let result = owner.finish(runtime.freeze_history_after_quiescence(), true);
        assert!(result.is_err());
        assert!(runtime.snapshot(0).history_error.is_some());
        std::fs::remove_dir(path.join(".state.pending")).unwrap();
        let (runtime, owner) = open(&path);
        assert_eq!(runtime.history_snapshot().unknown, 0x7f);
        owner
            .finish(runtime.freeze_history_after_quiescence(), true)
            .unwrap();
        cleanup(&path);
    }
    #[tokio::test]
    async fn unresolved_stop_or_missing_router_cannot_earn_clean_shutdown() {
        let state=crate::state::tests::test_app_state().await;
        assert!(!crate::board_abort::shutdown_stop_quiesced(&state),"unknown SDK owner was treated as empty");
        assert!(state.topology_router.set(Arc::new(sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([])))).is_ok());
        assert!(crate::board_abort::shutdown_stop_quiesced(&state));
        state.manual_abort_intent.request();
        assert!(!crate::board_abort::shutdown_stop_quiesced(&state));
    }
    #[tokio::test]
    async fn late_http_origin_and_dispatch_cannot_reopen_shutdown() {
        let state = crate::state::tests::test_app_state().await;
        let original = crate::gse::capture_operator_origin(&state).unwrap();
        state.request_shutdown();
        assert!(crate::gse::capture_operator_origin(&state).is_none());
        assert!(crate::gse::claim_operator_dispatch(&state, original).is_none());
        assert_eq!(
            state
                .operator_dispatch_inflight
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        let before = state.manual_abort_intent.epoch();
        state.manual_abort_intent.request();
        assert!(
            state.manual_abort_intent.epoch() > before,
            "Stop remains accessible without journal/dispatch locks"
        );
    }
    #[test]
    fn dropped_live_owner_never_fabricates_clean_exit() {
        let path = directory();
        let (runtime, owner) = open(&path);
        runtime.observe_abort(Board::DaqBoard, Instant::now(), 123456);
        drop(owner); // No flush/seal receipt, regardless of writer scheduling.
        let deadline = Instant::now() + Duration::from_secs(2);
        let changed = Arc::new(Notify::new());
        let (restored, _, owner) = loop {
            match Owner::open(&path, [42; 16], changed.clone()) {
                Ok(value) => break value,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("abandoned owner did not relinquish ownership: {error}"),
            }
        };
        assert!(restored.previous_session_unclean);
        assert_eq!(restored.snapshot.unknown, 0x7f);
        assert!(matches!(restored.snapshot.actual_abort, 0 | 0x40));
        owner.finish(restored.snapshot, false).unwrap();
        cleanup(&path);
    }
    #[test]
    fn missing_whole_directory_after_checked_clean_never_becomes_clean_first_install() {
        let path=directory();
        let (runtime,owner)=open(&path);
        for board in crate::board_abort::BOARDS {
            let after=Instant::now();
            runtime.observe_status(board,42,3,11,Some(1),false,true,after,runtime.query_revision(board),Instant::now(),123456);
        }
        assert_eq!(runtime.history_snapshot().unknown,0);
        owner.finish(runtime.freeze_history_after_quiescence(),true).unwrap();
        std::fs::remove_dir_all(&path).unwrap(); // Simulated lost/unmounted history.
        let (runtime,owner)=open(&path);
        assert_eq!(runtime.history_snapshot().unknown,0x7f);
        assert_eq!(runtime.history_snapshot().actual_abort,0,"absence cannot fabricate a physical Abort");
        owner.finish(runtime.history_snapshot(),false).unwrap();
        cleanup(&path);
    }
    #[tokio::test]
    async fn effective_unknown_scope_is_separate_from_retained_truth() {
        let state = crate::state::tests::test_app_state().await;
        #[cfg(feature = "hitl_mode")]
        {
            assert!(!state.board_state_proof_enforced());
            assert_eq!(crate::board_abort::required_unknown_mask(&state, 0x7f), 0);
            state.hitl_safety_interlocks[3].store(true, std::sync::atomic::Ordering::Release);
        }
        assert_eq!(
            crate::board_abort::required_unknown_mask(&state, 0x7f),
            if cfg!(feature = "test_fire_mode") {
                0x78
            } else {
                0x7f
            }
        );
        // Explicit paired demand scans the approved scope even when this
        // primary's own journal was clean and its optional rows are unseen.
        assert_eq!(state.board_abort.history_snapshot().unknown, 0);
        let _ = crate::board_abort::request_paired_readiness_queries(&state);
        assert_eq!(
            state.board_abort.take_queries(),
            if cfg!(feature = "test_fire_mode") {
                0x78
            } else {
                0x7f
            }
        );
        state
            .board_abort
            .observe_abort(Board::RFBoard, Instant::now(), 123456);
        assert!(
            crate::board_abort::blocked_message(&state)
                .unwrap()
                .contains("report Abort"),
            "actual AV-bay Abort was incorrectly scoped away"
        );
        assert_eq!(state.board_abort.history_snapshot().actual_abort, 1);
    }
}
