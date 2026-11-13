use super::*;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const ID: [u8; 16] = [0x42; 16];
const CUTS: [Cut; 7] = [
    Cut::TempCreated,
    Cut::PartialWrite,
    Cut::Written,
    Cut::FileSynced,
    Cut::ReadBack,
    Cut::Renamed,
    Cut::DirectorySynced,
];
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "abort-history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> PathBuf {
        self.0.join("history")
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn nominal(revision: u64) -> Snapshot {
    Snapshot {
        revision,
        actual_abort: 0,
        unknown: 0,
        enrolled: 0,
        first_abort_ms: [0; 7],
    }
}
fn abort(revision: u64, bit: u8) -> Snapshot {
    Snapshot {
        revision,
        actual_abort: bit,
        unknown: 0,
        enrolled: bit,
        first_abort_ms: [123456; 7],
    }
}

#[test]
fn clean_restart_preserves_actual_abort_and_optional_absence() {
    let dir = Directory::new();
    let mut store = Store::provision(&dir.store(), ID, BOARD_BITS).unwrap();
    assert_eq!(
        store.begin_session(BOARD_BITS).unwrap().snapshot,
        nominal(0)
    );
    store.save(abort(1, 0x40)).unwrap();
    store
        .seal_clean(QuiescenceReceipt::checked(1, true, true, true).unwrap())
        .unwrap();
    drop(store);
    let mut reopened = Store::open(&dir.store(), ID).unwrap();
    let restored = reopened.begin_session(BOARD_BITS).unwrap();
    assert!(!restored.previous_session_unclean);
    assert_eq!(restored.snapshot.actual_abort, 0x40);
    assert_eq!(restored.snapshot.unknown, 0);
    assert_eq!(restored.snapshot.first_abort_ms[6], 123456);
}

#[test]
fn unclean_restart_distinguishes_unknown_from_actual_abort() {
    let dir = Directory::new();
    let mut store = Store::provision(&dir.store(), ID, 0x48).unwrap();
    store.begin_session(0x48).unwrap();
    store.save(abort(1, 0x08)).unwrap();
    drop(store);
    let mut reopened = Store::open(&dir.store(), ID).unwrap();
    let restored = reopened.begin_session(0x48).unwrap();
    assert!(restored.previous_session_unclean);
    assert_eq!(restored.snapshot.actual_abort, 0x08);
    assert_eq!(restored.snapshot.unknown, 0x48);
}

#[test]
fn new_optional_abort_not_yet_saved_is_covered_only_by_explicit_prospective_mask() {
    let dir = Directory::new();
    let mut store = Store::provision(&dir.store(), ID, 0x44).unwrap();
    store.begin_session(0x44).unwrap();
    // Runtime saw optional board0x40 Abort while writer was paused. No snapshot
    // save occurred; the durable pre-session prospective roster covers the cut.
    drop(store);
    let mut reopened = Store::open(&dir.store(), ID).unwrap();
    let restored = reopened.begin_session(0x44).unwrap();
    assert_eq!(restored.snapshot.actual_abort, 0);
    assert_eq!(restored.snapshot.unknown, 0x44);
    // Never claim protection for a bit omitted from the explicit roster.
    assert_eq!(restored.snapshot.unknown & 0x02, 0);
}

#[test]
fn snapshots_reject_stale_clear_and_silent_enrollment_removal() {
    let dir = Directory::new();
    let mut store = Store::provision(&dir.store(), ID, BOARD_BITS).unwrap();
    store.begin_session(BOARD_BITS).unwrap();
    store.save(abort(10, 0x20)).unwrap();
    assert!(store.save(nominal(9)).is_err());
    assert!(store.save(nominal(10)).is_err());
    assert!(store.save(nominal(11)).is_err());
    // Existing correlated register validation must authorize this clear. Store
    // serializes the accepted revision, never serializes/replays physical proof.
    store
        .save(Snapshot {
            revision: 11,
            enrolled: 0x20,
            ..nominal(11)
        })
        .unwrap();
    store.save(abort(12, 0x20)).unwrap();
    assert!(
        store
            .save(Snapshot {
                revision: 11,
                enrolled: 0x20,
                ..nominal(11)
            })
            .is_err()
    );
    assert_eq!(store.record.snapshot.actual_abort, 0x20);
}

#[test]
fn clean_seal_requires_every_actual_quiescence_result_and_matching_revision() {
    for missing in 0..3 {
        let mut success = [true; 3];
        success[missing] = false;
        assert!(QuiescenceReceipt::checked(0, success[0], success[1], success[2]).is_err());
    }
    let dir = Directory::new();
    let mut store = Store::provision(&dir.store(), ID, 1).unwrap();
    store.begin_session(1).unwrap();
    store.save(abort(3, 1)).unwrap();
    assert!(
        store
            .seal_clean(QuiescenceReceipt::checked(2, true, true, true).unwrap())
            .is_err()
    );
    drop(store);
    let mut reopened = Store::open(&dir.store(), ID).unwrap();
    assert!(reopened.begin_session(1).unwrap().previous_session_unclean);
}

#[test]
fn incomplete_missing_corrupt_truncated_oversized_and_replaced_stores_refuse() {
    let dir = Directory::new();
    assert!(Store::open(&dir.store(), ID).is_err());
    let store = Store::provision(&dir.store(), ID, 1).unwrap();
    drop(store);
    assert!(Store::provision(&dir.store(), ID, 1).is_err());
    assert!(Store::open(&dir.store(), [0x43; 16]).is_err());
    let original = fs::read(dir.store().join("state")).unwrap();
    for bytes in [original[..20].to_vec(), vec![0; STATE_SIZE], vec![0; 4097]] {
        fs::write(dir.store().join("state"), bytes).unwrap();
        assert!(Store::open(&dir.store(), ID).is_err());
    }
    fs::write(dir.store().join("state"), original).unwrap();
    fs::remove_file(dir.store().join("state")).unwrap();
    assert!(Store::open(&dir.store(), ID).is_err());
    assert!(Store::provision(&dir.store(), ID, 1).is_err());
}

#[test]
fn competing_store_owner_is_refused_and_kernel_releases_ownership() {
    let dir = Directory::new();
    let store = Store::provision(&dir.store(), ID, 1).unwrap();
    assert!(Store::open(&dir.store(), ID).is_err());
    drop(store);
    assert!(Store::open(&dir.store(), ID).is_ok());
}

#[test]
fn startup_failure_returns_no_receipt_and_poisons_owner() {
    for point in CUTS {
        let dir = Directory::new();
        let mut store = Store::provision(&dir.store(), ID, 0x40).unwrap();
        let mut cut = |stage| {
            if stage == point {
                Err(io::Error::other("injected I/O failure"))
            } else {
                Ok(())
            }
        };
        assert!(store.begin_with_cut(0x40, &mut cut).is_err());
        assert!(store.save(abort(1, 0x40)).is_err());
        assert!(store.begin_session(0x40).is_err());
        drop(store);
        // If active rename happened, reopening is conservative. Before rename,
        // no startup receipt was returned and no producer could have started.
        let mut reopened = Store::open(&dir.store(), ID).unwrap();
        let restored = reopened.begin_session(0x40).unwrap();
        if matches!(point, Cut::Renamed | Cut::DirectorySynced) {
            assert_eq!(restored.snapshot.unknown, 0x40);
        }
    }
}

#[test]
fn explicit_roster_validation_and_no_silent_shrink() {
    let dir = Directory::new();
    assert!(Store::provision(&dir.store(), ID, 0x80).is_err());
    let mut store = Store::provision(&dir.store(), ID, 0x40).unwrap();
    assert!(store.begin_session(0x01).is_err());
    store.begin_session(0x41).unwrap();
    assert!(store.save(abort(1, 0x02)).is_err());
}

#[test]
fn actual_producer_panic_and_timeout_cannot_create_clean_receipt() {
    use std::sync::mpsc;
    use std::time::Duration;
    for panic in [false, true] {
        let dir = Directory::new();
        let mut store = Store::provision(&dir.store(), ID, 0x40).unwrap();
        store.begin_session(0x40).unwrap();
        let (release, parked) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            parked.recv().unwrap();
            assert!(!panic, "injected actual producer panic");
            completed.send(()).unwrap();
        });
        let joined_successfully = if panic {
            release.send(()).unwrap();
            worker.join().is_ok()
        } else {
            // The producer is deterministically parked, not just slow. The
            // actual timeout must stay failed even if it eventually exits.
            let timed_out = completion.recv_timeout(Duration::from_millis(1)).is_err();
            assert!(timed_out);
            release.send(()).unwrap();
            worker.join().unwrap();
            false
        };
        assert!(QuiescenceReceipt::checked(0, true, joined_successfully, true).is_err());
        drop(store);
        let mut reopened = Store::open(&dir.store(), ID).unwrap();
        assert!(
            reopened
                .begin_session(0x40)
                .unwrap()
                .previous_session_unclean
        );
    }
}

#[test]
fn late_abort_during_actual_producer_drain_survives_clean_shutdown() {
    use std::sync::{Arc, Mutex, mpsc};
    let dir = Directory::new();
    let mut store = Store::provision(&dir.store(), ID, 0x40).unwrap();
    store.begin_session(0x40).unwrap();
    let current = Arc::new(Mutex::new(nominal(0)));
    let producer_register = current.clone();
    let (release, parked) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        parked.recv().unwrap();
        *producer_register.lock().unwrap() = abort(1, 0x40);
    });
    // Stop remains an independent RAM operation while drain is incomplete.
    let manual_stop = AtomicU64::new(0);
    manual_stop.fetch_add(1, Ordering::SeqCst);
    assert_eq!(manual_stop.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    producer.join().unwrap();
    let final_snapshot = *current.lock().unwrap();
    store.save(final_snapshot).unwrap();
    store
        .seal_clean(QuiescenceReceipt::checked(1, true, true, true).unwrap())
        .unwrap();
    drop(store);
    let mut reopened = Store::open(&dir.store(), ID).unwrap();
    let restored = reopened.begin_session(0x40).unwrap();
    assert!(!restored.previous_session_unclean);
    assert_eq!(restored.snapshot.actual_abort, 0x40);
    assert_eq!(restored.snapshot.first_abort_ms[6], 123456);
}

#[test]
fn final_writer_rejection_prevents_clean_seal_and_preserves_uncertainty() {
    let dir = Directory::new();
    let mut store = Store::provision(&dir.store(), ID, 0x40).unwrap();
    store.begin_session(0x40).unwrap();
    let mut reject = |stage| {
        if stage == Cut::PartialWrite {
            Err(io::Error::other("injected final writer rejection"))
        } else {
            Ok(())
        }
    };
    let drained = store.save_with_cut(abort(1, 0x40), &mut reject).is_ok();
    assert!(!drained);
    assert!(QuiescenceReceipt::checked(1, true, true, drained).is_err());
    assert!(
        store
            .seal_clean(QuiescenceReceipt::checked(0, true, true, true).unwrap())
            .is_err()
    );
    drop(store);
    let mut reopened = Store::open(&dir.store(), ID).unwrap();
    let restored = reopened.begin_session(0x40).unwrap();
    assert!(restored.previous_session_unclean);
    assert_eq!(restored.snapshot.unknown, 0x40);
}

// Run via subprocess only. process::exit intentionally bypasses Drop and clean
// shutdown. This validates real process-crash outcomes, not a power-loss device.
#[test]
#[ignore = "subprocess crashcut entrypoint"]
fn cut_child() {
    let path = PathBuf::from(std::env::var_os("ABORT_HISTORY_TEST_PATH").unwrap());
    let operation = std::env::var("ABORT_HISTORY_TEST_OPERATION").unwrap();
    let stage = std::env::var("ABORT_HISTORY_TEST_CUT").unwrap();
    let mut cut = |point: Cut| -> io::Result<()> {
        if format!("{point:?}") == stage {
            std::process::exit(77);
        }
        Ok(())
    };
    if operation.starts_with("provision") {
        let mut commits = 0;
        let mut provisioning_cut = |point| {
            // Provisioning uses two commits: identity anchor then initial state.
            if matches!(operation.as_str(), "provision_state" | "provision_uncertain_state") && commits == 0 {
                if point == Cut::DirectorySynced {
                    commits += 1;
                }
                Ok(())
            } else {
                cut(point)
            }
        };
        if operation == "provision_uncertain_state" {
            Store::provision_initial(&path,ID,0x40,0x40,&mut provisioning_cut).unwrap();
        } else { Store::provision_with_cut(&path, ID, 0x40, &mut provisioning_cut).unwrap(); }

    } else {
        let mut store = Store::open(&path, ID).unwrap();
        match operation.as_str() {
            "begin" => {
                store.begin_with_cut(0x40, &mut cut).unwrap();
            }
            "save" => {
                store.save_with_cut(abort(1, 0x40), &mut cut).unwrap();
            }
            "seal" => {
                store
                    .seal_with_cut(
                        QuiescenceReceipt::checked(0, true, true, true).unwrap(),
                        &mut cut,
                    )
                    .unwrap();
            }
            _ => panic!("unknown crashcut operation"),
        }
    }
    panic!("cut point was not reached");
}

#[test]
fn actual_process_crash_cuts_at_every_atomic_commit_stage() {
    for operation in ["provision", "provision_state", "provision_uncertain_state", "begin", "save", "seal"] {
        for point in CUTS {
            let dir = Directory::new();
            if !operation.starts_with("provision") {
                let mut store = Store::provision(&dir.store(), ID, 0x40).unwrap();
                if matches!(operation, "save" | "seal") {
                    store.begin_session(0x40).unwrap();
                }
            }
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!(
                        "{}::cut_child",
                        module_path!()
                            .split("::")
                            .skip(1)
                            .collect::<Vec<_>>()
                            .join("::")
                    ),
                    "--ignored",
                ])
                .env("ABORT_HISTORY_TEST_PATH", dir.store())
                .env("ABORT_HISTORY_TEST_OPERATION", operation)
                .env("ABORT_HISTORY_TEST_CUT", format!("{point:?}"))
                .output()
                .unwrap();
            assert_eq!(
                status.status.code(),
                Some(77),
                "{operation} {point:?}: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            if operation.starts_with("provision") {
                // All selected cuts strike the first atomic transaction, the
                // anchor. State was never initialized: cannot restart as fresh.
                if matches!(operation, "provision_state" | "provision_uncertain_state")
                    && matches!(point, Cut::Renamed | Cut::DirectorySynced)
                {
                    let mut reopened = Store::open(&dir.store(), ID).unwrap();
                    let restored = reopened.begin_session(0x40).unwrap();
                    assert!(!restored.previous_session_unclean);
                    assert_eq!(restored.snapshot.actual_abort, 0);
                    assert_eq!(restored.snapshot.unknown, if operation == "provision_uncertain_state" {0x40} else {0});
                } else {
                    assert!(Store::open(&dir.store(), ID).is_err());
                }
                assert!(Store::provision(&dir.store(), ID, 0x40).is_err());
                continue;
            }
            let mut reopened = Store::open(&dir.store(), ID).unwrap();
            let restored = reopened.begin_session(0x40).unwrap();
            match operation {
                "save" => {
                    assert_eq!(restored.snapshot.unknown, 0x40);
                    assert!(matches!(restored.snapshot.actual_abort, 0 | 0x40));
                }
                "begin" => {
                    if matches!(point, Cut::Renamed | Cut::DirectorySynced) {
                        assert_eq!(restored.snapshot.unknown, 0x40);
                    }
                }
                "seal" => {
                    // A committed clean seal is valid only because the child
                    // had a real model closure receipt before starting it.
                    if matches!(point, Cut::Renamed | Cut::DirectorySynced) {
                        assert_eq!(restored.snapshot.unknown, 0);
                    } else {
                        assert_eq!(restored.snapshot.unknown, 0x40);
                    }
                }
                _ => unreachable!(),
            }
            // Pending files remain bounded: anchor/state/lock + one temporary.
            assert!(fs::read_dir(dir.store()).unwrap().count() <= 4);
        }
    }
}
