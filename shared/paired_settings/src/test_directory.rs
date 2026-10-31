//! Test fixtures own a directory only after exclusive filesystem reservation.
//! A process counter avoids clock-resolution assumptions; create_dir also
//! protects against stale directories, PID reuse, and another process.
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) fn reserve(prefix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    reserve_in(&std::env::temp_dir(), prefix, &NEXT)
}

fn reserve_in(root: &Path, prefix: &str, next: &AtomicU64) -> PathBuf {
    loop {
        let sequence = next.load(Ordering::Relaxed);
        let following = sequence
            .checked_add(1)
            .expect("test directory counter exhausted");
        if next
            .compare_exchange_weak(sequence, following, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        let directory = root.join(format!("{prefix}-{}-{sequence}", std::process::id()));
        match std::fs::create_dir(&directory) {
            Ok(()) => return directory,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!(
                "cannot reserve test directory {}: {error}",
                directory.display()
            ),
        }
    }
}

#[test]
fn parallel_reservations_never_reuse_existing_paths_or_depend_on_clock_resolution() {
    let root = reserve("gs-test-directory-regression");
    let next = AtomicU64::new(0);
    let existing = root.join(format!("fixed-{}-0", std::process::id()));
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("sentinel"), b"preserve existing owner").unwrap();
    let first = reserve_in(&root, "fixed", &next);
    assert_ne!(first, existing);
    // No clock is read by the allocator, so even a constant/coarse wall clock
    // cannot collapse parallel requests into the same directory.
    let paths = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..32)
            .map(|_| {
                scope.spawn(|| {
                    (0..32)
                        .map(|_| reserve_in(&root, "fixed", &next))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1024
    );
    assert!(paths.iter().all(|path| path.is_dir() && *path != existing));
    assert_eq!(
        std::fs::read(existing.join("sentinel")).unwrap(),
        b"preserve existing owner"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "sqlite-journal")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn separately_reserved_journals_open_in_parallel_without_sharing_wal_files() {
    use crate::journal::{Journal, Limits};
    let root = reserve("gs-test-independent-journals");
    let next = AtomicU64::new(0);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(33));
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let path = reserve_in(&root, "journal", &next).join("journal.sqlite3");
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let journal = Journal::open(
                &path,
                "test".into(),
                vec!["gs".into(), "rrls".into()],
                Limits::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("independent fixture {}: {error:#}", path.display()));
            assert_eq!(journal.metadata().await.unwrap().documents, 0);
            journal.close().await;
        }));
    }
    barrier.wait().await;
    for task in tasks {
        task.await.unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}
