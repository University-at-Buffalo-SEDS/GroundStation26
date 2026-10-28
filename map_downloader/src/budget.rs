//! RRLS-only storage admission. No compressed-size guesses or destructive cleanup.
//!
//! SQLite is limited to23GB of main pages. Cache spilling is disabled, so one
//! transaction emits at most one WAL frame per main page. Before EVERY writer
//! transaction a successful TRUNCATE checkpoint is required. Thus only one such
//! WAL can coexist with the main DB. A128MiB allowance covers WAL index/header
//! and filesystem rounding; all other dataset files are counted separately.
use anyhow::{Context, Result, ensure};
use sqlx::SqlitePool;
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};
pub(crate) const MAX_DATABASE_BYTES: u64 = 23_000_000_000;
pub(crate) const MAX_DATASET_BYTES: u64 = 50_000_000_000;
const AUXILIARY_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) struct DatasetBudget {
    root: PathBuf,
    database: PathBuf,
    limit: u64,
    max_database: u64,
    _lock: File,
}
impl Drop for DatasetBudget {
    fn drop(&mut self) {
        // Release ownership explicitly: a concurrent process spawn can retain
        // a duplicate descriptor until exec, delaying a close-only unlock.
        let _ = self._lock.unlock();
    }
}
impl DatasetBudget {
    pub(crate) fn open(database: &Path) -> Result<Arc<Self>> {
        Self::open_limits(database, MAX_DATASET_BYTES, MAX_DATABASE_BYTES)
    }
    fn open_limits(database: &Path, limit: u64, max_database: u64) -> Result<Arc<Self>> {
        let parent = database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let root = parent.canonicalize()?;
        let database = root.join(
            database
                .file_name()
                .context("RRLS bundle needs a filename")?,
        );
        ensure!(
            database.file_name().unwrap() != ".download.lock",
            "invalid RRLS bundle filename"
        );
        let lock_path = root.join(".download.lock");
        if lock_path.exists() {
            ensure!(
                !fs::symlink_metadata(&lock_path)?.file_type().is_symlink(),
                "RRLS dataset lock must not be a symlink"
            );
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        lock.try_lock()
            .context("RRLS dataset already has a downloader; stop it before starting another")?;
        let budget = Arc::new(Self {
            root,
            database,
            limit,
            max_database,
            _lock: lock,
        });
        // Use the smallest supported page size for conservative WAL frame overhead.
        budget.check(512)?;
        Ok(budget)
    }
    pub(crate) fn max_database_bytes(&self) -> u64 {
        self.max_database
    }
    fn sidecar(&self, suffix: &str) -> PathBuf {
        let mut name = self.database.as_os_str().to_os_string();
        name.push(suffix);
        name.into()
    }
    fn visit(&self, directory: &Path, other: &mut u64, total: &mut u64) -> Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "RRLS dataset must not contain symlinks: {}",
                path.display()
            );
            if metadata.is_dir() {
                self.visit(&path, other, total)?;
            } else {
                ensure!(
                    metadata.is_file(),
                    "RRLS dataset contains a non-file: {}",
                    path.display()
                );
                let mut bytes = metadata.len();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    bytes = bytes.max(metadata.blocks().saturating_mul(512));
                }
                // Round up so a small file's allocation also counts on non-Unix platforms.
                bytes = bytes.saturating_add(65535) / 65536 * 65536;
                *total = total.saturating_add(bytes);
                if path != self.database && path != self.sidecar("-wal") {
                    *other = other.saturating_add(bytes);
                }
                if path == self.database {
                    ensure!(
                        metadata.len() <= self.max_database,
                        "existing RRLS database exceeds its23GB main-file cap; nothing was deleted"
                    );
                }
            }
        }
        Ok(())
    }
    pub(crate) fn check(&self, page_size: u64) -> Result<()> {
        ensure!(
            (512..=65536).contains(&page_size) && page_size.is_power_of_two(),
            "invalid SQLite page size"
        );
        let (mut other, mut total) = (0u64, 0u64);
        self.visit(&self.root, &mut other, &mut total)?;
        // One full database worth of frames, including24-byte frame headers.
        let wal = (self.max_database / page_size)
            .saturating_mul(page_size + 24)
            .saturating_add(32);
        ensure!(
            total.saturating_add(AUXILIARY_BYTES) < self.limit
                && other
                    .saturating_add(self.max_database)
                    .saturating_add(wal)
                    .saturating_add(AUXILIARY_BYTES)
                    < self.limit,
            "RRLS dataset storage budget exhausted (cap{} bytes including DB/WAL/temp/other files); saved tiles retained, nothing deleted",
            self.limit
        );
        Ok(())
    }
    pub(crate) async fn checkpoint(&self, pool: &SqlitePool) -> Result<()> {
        self.check(512)?;
        let (busy, _, _): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(pool)
            .await?;
        ensure!(
            busy == 0,
            "RRLS WAL checkpoint is blocked by a reader; pause/retry after active map reads finish; saved tiles retained"
        );
        let size = fs::metadata(self.sidecar("-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        ensure!(
            size == 0,
            "RRLS WAL could not be truncated; saved tiles retained"
        );
        self.check(512)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn scratch(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rrls-budget-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
    #[test]
    fn counts_existing_files_wal_and_nested_temporary_files_without_deleting() {
        let root = scratch("files");
        fs::create_dir_all(root.join("tmp")).unwrap();
        let file = root.join("tmp/input");
        fs::write(&file, b"keep").unwrap();
        let budget = DatasetBudget::open_limits(
            &root.join("tiles.sqlite3"),
            AUXILIARY_BYTES + 1024 * 1024,
            1024,
        )
        .unwrap();
        assert!(budget.check(4096).is_ok());
        let mut large = File::create(root.join("tiles.sqlite3-wal")).unwrap();
        use std::io::Write;
        large.write_all(&vec![1; 1024 * 1024]).unwrap();
        assert!(budget.check(4096).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep");
        drop(budget);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn only_one_downloader_owns_a_dataset_and_lock_releases_on_drop() {
        let root = scratch("lock");
        let path = root.join("tiles.sqlite3");
        let first = DatasetBudget::open(&path).unwrap();
        assert!(DatasetBudget::open(&path).is_err());
        drop(first);
        drop(DatasetBudget::open(&path).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn last_dataset_owner_unlocks_even_while_a_duplicate_descriptor_is_alive() {
        let root = scratch("inherited-lock");
        let path = root.join("tiles.sqlite3");
        let first = DatasetBudget::open(&path).unwrap();
        // A concurrent Command spawn can briefly inherit this open file
        // description before exec applies close-on-exec.
        let inherited = first._lock.try_clone().unwrap();
        let owner = first.clone();
        drop(first);
        assert!(DatasetBudget::open(&path).is_err(), "live dataset owner still excludes another downloader");
        drop(owner);
        drop(DatasetBudget::open(&path).expect("last owner releases the lock without waiting for an unrelated child exec"));
        drop(inherited);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let root = scratch("symlink");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("outside")).unwrap();
        assert!(DatasetBudget::open(&root.join("tiles.sqlite3")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn sqlite_page_cap_stops_growth_and_retains_committed_data() {
        let root = scratch("sqlite");
        let path = root.join("tiles.sqlite3");
        let budget =
            DatasetBudget::open_limits(&path, AUXILIARY_BYTES + 1024 * 1024, 64 * 1024).unwrap();
        let pool = crate::open_bundle_database_with_budget(&path, Some(budget.clone()))
            .await
            .unwrap();
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE keep(data BLOB)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO keep VALUES('saved')")
            .execute(&pool)
            .await
            .unwrap();
        budget.checkpoint(&pool).await.unwrap();
        assert!(
            sqlx::query("INSERT INTO keep VALUES(zeroblob(131072))")
                .execute(&pool)
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM keep")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        pool.close().await;
        drop(budget);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn pinned_reader_stops_checkpoint_then_can_resume_without_losing_tiles() {
        let root = scratch("reader");
        let path = root.join("tiles.sqlite3");
        let budget =
            DatasetBudget::open_limits(&path, AUXILIARY_BYTES + 4 * 1024 * 1024, 1024 * 1024)
                .unwrap();
        let pool = crate::init_direct_bundle_pool_with_budget(&path, true, Some(budget.clone()))
            .await
            .unwrap();
        crate::upsert_tile_to_bundle(&pool, 17, 1, 1, b"existing")
            .await
            .unwrap();
        budget.checkpoint(&pool).await.unwrap();
        let reader = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .read_only(true),
            )
            .await
            .unwrap();
        let mut snapshot = reader.begin().await.unwrap();
        sqlx::query("SELECT * FROM tiles")
            .fetch_all(&mut *snapshot)
            .await
            .unwrap();
        crate::upsert_tile_to_bundle(&pool, 17, 2, 1, b"committed")
            .await
            .unwrap();
        sqlx::query("PRAGMA busy_timeout=1")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            crate::finalize_direct_bundle_pool(&pool, Some(&budget))
                .await
                .unwrap_err()
                .to_string()
                .contains("reader")
        );
        assert!(crate::bundle_has_tile(&pool, 17, 2, 1).await.unwrap());
        snapshot.commit().await.unwrap();
        crate::finalize_direct_bundle_pool(&pool, Some(&budget))
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sqlite_schema WHERE name='sqlite_stat1'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        reader.close().await;
        pool.close().await;
        drop(budget);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn writer_refuses_existing_file_growth_and_keeps_saved_tiles() {
        let root = scratch("writer");
        let path = root.join("tiles.sqlite3");
        let budget =
            DatasetBudget::open_limits(&path, AUXILIARY_BYTES + 2 * 1024 * 1024, 256 * 1024)
                .unwrap();
        let pool = crate::init_direct_bundle_pool_with_budget(&path, true, Some(budget.clone()))
            .await
            .unwrap();
        let (writer, task) =
            crate::storage::spawn_with_budget(pool.clone(), root.clone(), 0, Some(budget.clone()));
        writer.save(17, 1, 1, b"saved").await.unwrap();
        fs::write(root.join("unexpected.tmp"), vec![1; 2 * 1024 * 1024]).unwrap();
        assert!(writer.save(17, 2, 1, b"new").await.is_err());
        assert!(writer.failed());
        assert!(crate::bundle_has_tile(&pool, 17, 1, 1).await.unwrap());
        assert!(!crate::bundle_has_tile(&pool, 17, 2, 1).await.unwrap());
        assert!(
            crate::finalize_direct_bundle_pool(&pool, Some(&budget))
                .await
                .is_err()
        );
        drop(writer);
        task.await.unwrap();
        pool.close().await;
        drop(budget);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn startup_checkpoints_existing_wal_and_never_rebuilds_saved_data() {
        let root = scratch("startup");
        let path = root.join("tiles.sqlite3");
        let existing = crate::init_direct_bundle_pool(&path, true).await.unwrap();
        crate::upsert_tile_to_bundle(&existing, 17, 1, 1, b"saved in WAL")
            .await
            .unwrap();
        assert!(fs::metadata(root.join("tiles.sqlite3-wal")).unwrap().len() > 0);
        let budget =
            DatasetBudget::open_limits(&path, AUXILIARY_BYTES + 4 * 1024 * 1024, 1024 * 1024)
                .unwrap();
        assert!(
            crate::init_direct_bundle_pool_with_budget(&path, false, Some(budget.clone()))
                .await
                .is_err()
        );
        assert!(crate::bundle_has_tile(&existing, 17, 1, 1).await.unwrap());
        let resumed = crate::init_direct_bundle_pool_with_budget(&path, true, Some(budget.clone()))
            .await
            .unwrap();
        assert!(crate::bundle_has_tile(&resumed, 17, 1, 1).await.unwrap());
        assert_eq!(
            fs::metadata(root.join("tiles.sqlite3-wal")).unwrap().len(),
            0
        );
        resumed.close().await;
        existing.close().await;
        drop(budget);
        fs::remove_dir_all(root).unwrap();
    }
}
