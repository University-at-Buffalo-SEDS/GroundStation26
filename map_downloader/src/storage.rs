//! One bounded SQLite writer. HTTP workers never queue pool acquisitions.
use anyhow::{Context, Result};
use sqlx::SqlitePool;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_CAPACITY: usize = 32;
const BATCH_SIZE: usize = 32;
use crate::network::MAX_TILE_BYTES;
pub(crate) const DEFAULT_MIN_FREE_MIB: u64 = 2048;

struct Tile {
    z: u32,
    x: u32,
    y: u32,
    bytes: Vec<u8>,
    done: oneshot::Sender<std::result::Result<(), String>>,
}
#[derive(Clone)]
pub(crate) struct BundleWriter {
    tx: mpsc::Sender<Tile>,
    failed: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
}
impl BundleWriter {
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
    pub(crate) fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
    fn stopped(&self) -> anyhow::Error {
        self.failed.store(true, Ordering::Relaxed);
        let mut failure = self.failure.lock().unwrap();
        if failure.is_none() {
            *failure = Some("map bundle writer stopped".to_owned());
        }
        anyhow::anyhow!("map bundle writer stopped")
    }
    pub(crate) async fn save(&self, z: u32, x: u32, y: u32, bytes: &[u8]) -> Result<()> {
        self.save_with(z, x, y, bytes.len(), || bytes.to_vec())
            .await
    }
    async fn save_with(
        &self,
        z: u32,
        x: u32,
        y: u32,
        length: usize,
        copy: impl FnOnce() -> Vec<u8>,
    ) -> Result<()> {
        anyhow::ensure!(!self.failed(), "map bundle writer stopped");
        anyhow::ensure!(
            length <= MAX_TILE_BYTES,
            "map tile exceeds 512KiB storage limit"
        );
        // Reserve a bounded queue slot BEFORE making any payload copy. Waiting
        // HTTP workers retain their response body only, never another queued copy.
        let permit = self.tx.reserve().await.map_err(|_| self.stopped())?;
        anyhow::ensure!(!self.failed(), "map bundle writer stopped");
        let (done, receive) = oneshot::channel();
        permit.send(Tile {
            z,
            x,
            y,
            bytes: copy(),
            done,
        });
        receive
            .await
            .map_err(|_| self.stopped())?
            .map_err(anyhow::Error::msg)
    }
}

pub(crate) fn minimum_free_bytes() -> Result<u64> {
    let value = std::env::var("MAP_MIN_FREE_MIB")
        .ok()
        .map(|v| v.parse::<u64>())
        .transpose()
        .context("invalid MAP_MIN_FREE_MIB")?
        .unwrap_or(DEFAULT_MIN_FREE_MIB);
    value
        .checked_mul(1024 * 1024)
        .context("MAP_MIN_FREE_MIB is too large")
}

#[cfg(test)]
pub(crate) fn spawn(
    pool: SqlitePool,
    directory: PathBuf,
    reserve: u64,
) -> (BundleWriter, tokio::task::JoinHandle<()>) {
    spawn_with_budget(pool, directory, reserve, None)
}

pub(crate) fn spawn_with_budget(
    pool: SqlitePool,
    directory: PathBuf,
    reserve: u64,
    budget: Option<Arc<crate::budget::DatasetBudget>>,
) -> (BundleWriter, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Tile>(QUEUE_CAPACITY);
    let failed = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None));
    let writer = BundleWriter {
        tx,
        failed: failed.clone(),
        failure: failure.clone(),
    };
    let task = tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while batch.len() < BATCH_SIZE {
                match rx.try_recv() {
                    Ok(tile) => batch.push(tile),
                    Err(_) => break,
                }
            }
            let result = write_batch(&pool, &directory, reserve, &batch, budget.as_deref()).await;
            let message = result.as_ref().err().map(|error| format!("{error:#}"));
            if let Some(message) = &message {
                *failure.lock().unwrap() = Some(message.clone());
                failed.store(true, Ordering::Relaxed);
            }
            for tile in batch {
                let _ = tile.done.send(match &message {
                    Some(message) => Err(message.clone()),
                    None => Ok(()),
                });
            }
            if message.is_some() {
                break;
            }
        }
    });
    (writer, task)
}

async fn write_batch(
    pool: &SqlitePool,
    directory: &Path,
    reserve: u64,
    batch: &[Tile],
    budget: Option<&crate::budget::DatasetBudget>,
) -> Result<()> {
    if let Some(budget) = budget {
        budget.checkpoint(pool).await?;
    }
    let incoming = batch.iter().fold(0u64, |sum, tile| {
        sum.saturating_add(tile.bytes.len() as u64)
    });
    ensure_space(
        directory,
        reserve
            .saturating_add(incoming)
            .saturating_add(16 * 1024 * 1024),
    )?;
    // Acquire once per batch, release on commit BEFORE waiting for more work.
    let mut tx = pool.begin().await?;
    for tile in batch {
        let hash = blake3::hash(&tile.bytes);
        let blob_id: i64 = sqlx::query_scalar("INSERT INTO tile_blobs(hash,image) VALUES(?,?) ON CONFLICT(hash) DO UPDATE SET hash=excluded.hash RETURNING id")
            .bind(hash.as_bytes().as_slice()).bind(&tile.bytes).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT OR REPLACE INTO tiles(z,x,y,blob_id) VALUES(?,?,?,?)")
            .bind(i64::from(tile.z))
            .bind(i64::from(tile.x))
            .bind(i64::from(tile.y))
            .bind(blob_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    if let Some(budget) = budget {
        budget.checkpoint(pool).await?;
    }
    Ok(())
}

pub(crate) fn ensure_space(directory: &Path, required: u64) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let path = CString::new(directory.as_os_str().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: path is NUL-terminated; stat points to writable storage.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("check map disk free space");
        }
        // SAFETY: successful statvfs initialized stat.
        let stat = unsafe { stat.assume_init() };
        // statvfs field widths vary across supported Unix targets, including
        // 32-bit Raspberry Pi builds. Widen before multiplying on those targets.
        #[allow(clippy::unnecessary_cast)]
        let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
        anyhow::ensure!(
            free >= required,
            "map download paused: insufficient disk space to preserve the configured free-space reserve; saved tiles are retained"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, required);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn db() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(100))
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE tile_blobs(id INTEGER PRIMARY KEY,hash BLOB UNIQUE,image BLOB)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE tiles(z INTEGER,x INTEGER,y INTEGER,blob_id INTEGER,PRIMARY KEY(z,x,y))",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }
    #[tokio::test]
    async fn one_connection_batched_writer_handles_concurrent_tiles_and_releases_idle_connection() {
        let pool = db().await;
        let (writer, task) = spawn(pool.clone(), std::env::temp_dir(), 0);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures::future::join_all((0..256).map(|x| {
                let writer = writer.clone();
                async move {
                    writer.save(17, x, 0, b"same tile").await.unwrap();
                }
            })),
        )
        .await
        .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tiles")
                .fetch_one(&pool)
                .await
                .unwrap(),
            256
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tile_blobs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert!(!writer.failed());
        drop(writer);
        task.await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn disk_reserve_failure_is_terminal_and_keeps_existing_rows() {
        let pool = db().await;
        let (initial, initial_task) = spawn(pool.clone(), std::env::temp_dir(), 0);
        initial.save(17, 0, 0, b"existing tile").await.unwrap();
        drop(initial);
        initial_task.await.unwrap();
        let (writer, task) = spawn(pool.clone(), std::env::temp_dir(), u64::MAX);
        assert!(writer.save(17, 1, 0, b"tile").await.is_err());
        assert!(writer.failed());
        assert!(writer.failure().unwrap().contains("disk space"));
        assert!(writer.save(17, 2, 0, b"tile").await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tiles")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        drop(writer);
        task.await.unwrap();
    }
    #[tokio::test]
    async fn failed_commit_is_terminal_and_rolls_back_unmapped_blob() {
        let pool = db().await;
        let (writer, task) = spawn(pool.clone(), std::env::temp_dir(), 0);
        writer.save(17, 0, 0, b"existing").await.unwrap();
        sqlx::query("CREATE TRIGGER fail_tile BEFORE INSERT ON tiles WHEN NEW.x=1 BEGIN SELECT RAISE(FAIL,'fixture write failure'); END").execute(&pool).await.unwrap();
        assert!(writer.save(17, 1, 0, b"should roll back").await.is_err());
        assert!(writer.failed());
        assert!(writer.failure().unwrap().contains("fixture write failure"));
        assert!(writer.save(17, 2, 0, b"not admitted").await.is_err());
        drop(writer);
        task.await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tiles")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tile_blobs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }
    #[tokio::test]
    async fn blocked_producer_reserves_capacity_before_copying_payload() {
        let (tx, mut rx) = mpsc::channel(1);
        let (done, _) = oneshot::channel();
        tx.send(Tile {
            z: 0,
            x: 0,
            y: 0,
            bytes: vec![],
            done,
        })
        .await
        .unwrap();
        let writer = BundleWriter {
            tx,
            failed: Arc::new(AtomicBool::new(false)),
            failure: Arc::new(Mutex::new(None)),
        };
        let copied = Arc::new(AtomicBool::new(false));
        let marker = copied.clone();
        let waiting = writer.save_with(0, 1, 0, 1, move || {
            marker.store(true, Ordering::Relaxed);
            vec![1]
        });
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), &mut waiting)
                .await
                .is_err()
        );
        assert!(!copied.load(Ordering::Relaxed));
        drop(rx.recv().await.unwrap());
        let (result, ()) = tokio::join!(&mut waiting, async {
            let tile = rx.recv().await.unwrap();
            tile.done.send(Ok(())).unwrap();
        });
        result.unwrap();
        assert!(copied.load(Ordering::Relaxed));
    }
}
