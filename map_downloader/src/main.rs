mod budget;
mod cache_probe;
mod connection_test;
mod dns;
mod limits;
mod network;
mod options;
mod profile;
mod raster;
mod repair;
mod storage;

use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{Context, Result};
use blake3::Hasher;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use network::TileClient;
use profile::Profile;
#[cfg(test)]
use reqwest::Client;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::collections::HashSet;
use std::io::IsTerminal;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::fs as async_fs;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{Duration, Instant, sleep};

/// Region name (used for directory layout)
const REGION: &str = "north_america";

/// ArcGIS World Imagery (satellite) XYZ tile endpoint.
const SATELLITE_BASE_URL: &str =
    "https://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile";

/// File extension for tiles from this layer.
const TILE_EXT: &str = "jpg";

/// Zoom levels we want to cache.
const MIN_ZOOM: u32 = 0;
const DEFAULT_MAX_ZOOM: u32 = 17;

/// Full North America coverage up to this zoom.
const BASE_COVERAGE_MAX_ZOOM: u32 = 8;

/// Approximate North America bounds in lon/lat (WGS84)
/// lon_min, lat_min, lon_max, lat_max
const NA_BOUNDS: (f64, f64, f64, f64) = (-170.0, 5.0, -50.0, 83.0);

/// Higher-detail region: Buffalo <-> Rochester corridor.
/// lon_min, lat_min, lon_max, lat_max
const BUFFALO_ROCHESTER_BOUNDS: (f64, f64, f64, f64) = (-79.30, 42.70, -77.25, 43.40);

/// Higher-detail region: West Texas desert / Trans-Pecos area.
/// lon_min, lat_min, lon_max, lat_max
const TEXAS_DESERT_BOUNDS: (f64, f64, f64, f64) = (-106.80, 29.00, -101.00, 32.60);

/// Higher-detail region: URRG launch field and recovery area near Potter, NY.
/// The official URRG field map is centered near 42.7004 N, 77.1941 W:
/// https://www.urrg.us/content.aspx (Field Map).
/// lon_min, lat_min, lon_max, lat_max; roughly 20 km east/west by 22 km north/south.
const URRG_BOUNDS: (f64, f64, f64, f64) = (-77.30, 42.60, -77.05, 42.80);

fn coverage_bounds(z: u32) -> &'static [(f64, f64, f64, f64)] {
    if z <= BASE_COVERAGE_MAX_ZOOM {
        &[NA_BOUNDS]
    } else {
        &[BUFFALO_ROCHESTER_BOUNDS, TEXAS_DESERT_BOUNDS, URRG_BOUNDS]
    }
}

/// Max concurrent HTTP fetches at a time.
/// Tune this: higher = faster but more load on remote tile service / network.
const DEFAULT_MAX_CONCURRENT: usize = 1024;
const PROGRESS_PERCENT_STEP: u64 = 10;
const DEFAULT_MAX_BANDWIDTH_MIBPS: f64 = 2.5;

const MAX_RETRY_ATTEMPTS: u32 = 40;
const DEFAULT_BUILD_BUNDLE: bool = true;

fn retry_backoff_ms(attempt: u32, z: u32, x: u32, y: u32) -> u64 {
    // Exponential backoff capped at 8s with deterministic per-tile jitter.
    let exp = attempt.saturating_sub(1).min(8);
    let base_ms = 100u64.saturating_mul(1u64 << exp);
    let seed = ((z as u64) << 42) ^ ((x as u64) << 21) ^ (y as u64) ^ (attempt as u64 * 1103515245);
    let jitter_ms = seed % 125;
    (base_ms + jitter_ms).min(8_000)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TileOutcome {
    Complete,
    Unavailable,
    Exhausted,
    Permanent,
}

fn sane_eta(done: u64, total: u64, elapsed_s: f64) -> String {
    let remaining = total.saturating_sub(done);
    if remaining == 0 {
        return "0s".to_owned();
    }
    if done == 0 || !elapsed_s.is_finite() || elapsed_s <= 0.0 {
        return "--".to_owned();
    }
    let seconds = remaining as f64 * (elapsed_s / done as f64);
    if !seconds.is_finite() || seconds < 0.0 || seconds > 366.0 * 86400.0 {
        return "--".to_owned();
    }
    let seconds = seconds.ceil() as u64;
    format!(
        "{}h{:02}m{:02}s",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

fn log_progress_error(pb: Option<&ProgressBar>, msg: String) {
    if let Some(pb) = pb {
        pb.println(msg);
    } else {
        eprintln!("{msg}");
    }
}

async fn fetch_valid_raster(client: &TileClient, url: &str) -> Result<network::TileResponse> {
    let mut response = client.fetch(url).await?;
    if response.status.is_success() {
        response.bytes = raster::validate_download(response.bytes)
            .await
            .context("invalid PNG/JPEG tile response")?;
    }
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_tile_with_retries(
    z: u32,
    x: u32,
    y: u32,
    z_dir: &Path,
    client: &TileClient,
    bytes_downloaded: &Arc<AtomicU64>,
    pb: Option<&ProgressBar>,
    limiter: Option<&Arc<BandwidthLimiter>>,
    max_attempts: u32,
    base_url: &str,
    force: bool,
) -> TileOutcome {
    let tile_path = z_dir.join(format!("{x}/{y}.{TILE_EXT}"));
    let part_path = tile_path.with_extension(format!("{}.part", TILE_EXT));

    // Skip if final tile already exists
    if !force
        && async_fs::try_exists(&tile_path).await.unwrap_or(false)
        && raster::cached_file_ok(&tile_path).unwrap_or(false)
    {
        return TileOutcome::Complete;
    }

    // Remove any leftover .part file
    if async_fs::try_exists(&part_path).await.unwrap_or(false) {
        let _ = async_fs::remove_file(&part_path).await;
    }

    let url = format!("{base}/{z}/{y}/{x}", base = base_url, z = z, y = y, x = x,);

    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        // Transient failures stay quiet. Keep the final cause for one useful
        // report only if this tile exhausts its bounded retry allowance.
        let failure = match fetch_valid_raster(client, &url).await {
            Ok(resp) => {
                let status = resp.status;
                if status.is_success() {
                    let bytes = resp.bytes;
                    if let Some(lim) = limiter {
                        lim.throttle_bytes(bytes.len()).await;
                    }
                    bytes_downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match write_tile_atomic_async(&tile_path, &bytes).await {
                        Ok(()) => return TileOutcome::Complete,
                        Err(error) => format!("saving tile: {error:#}"),
                    }
                } else if status.as_u16() == 404 {
                    return TileOutcome::Unavailable;
                } else if status.is_client_error() && status.as_u16() != 429 {
                    log_progress_error(
                        pb,
                        format!(
                            "fetch_satellite_tiles_async: skipping non-retryable HTTP {} for tile z={z}, x={x}, y={y}",
                            status.as_u16()
                        ),
                    );
                    return TileOutcome::Permanent;
                } else {
                    format!("HTTP {status}")
                }
            }
            Err(error) if error.is::<network::OversizedTile>() => {
                log_progress_error(
                    pb,
                    format!("tile z={z},x={x},y={y} exceeds512KiB; not retried"),
                );
                return TileOutcome::Permanent;
            }
            Err(error) => format!("request or response body: {error:#}"),
        };
        if attempts >= max_attempts {
            log_progress_error(
                pb,
                format!(
                    "fetch_satellite_tiles_async: giving up on tile z={z}, x={x}, y={y} after {attempts} attempts: {failure}"
                ),
            );
            return TileOutcome::Exhausted;
        }
        sleep(Duration::from_millis(retry_backoff_ms(attempts, z, x, y))).await;
    }
}

fn max_concurrent() -> usize {
    env::var("MAP_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, 16_384))
        .unwrap_or(DEFAULT_MAX_CONCURRENT)
}

fn max_bandwidth_mibps() -> Option<f64> {
    let v = env::var("MAP_MAX_BANDWIDTH_MIBPS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(DEFAULT_MAX_BANDWIDTH_MIBPS);
    if v <= 0.0 { None } else { Some(v) }
}

fn should_build_bundle() -> bool {
    match env::var("MAP_BUILD_BUNDLE") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"),
        Err(_) => DEFAULT_BUILD_BUNDLE,
    }
}

fn should_direct_to_bundle() -> bool {
    match env::var("MAP_DIRECT_TO_BUNDLE") {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => should_build_bundle(),
    }
}

fn should_keep_tiles() -> bool {
    match env::var("MAP_KEEP_TILES") {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

fn should_resume_bundle() -> bool {
    match env::var("MAP_BUNDLE_RESUME") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"),
        Err(_) => true,
    }
}

fn bundle_path_for(data_dir: &Path) -> PathBuf {
    match env::var("MAP_BUNDLE_PATH") {
        Ok(raw) if !raw.trim().is_empty() => PathBuf::from(raw),
        _ => data_dir.join("tiles.sqlite3"),
    }
}

async fn open_bundle_database(path: &Path) -> Result<SqlitePool> {
    open_bundle_database_with_budget(path, None).await
}

async fn open_bundle_database_with_budget(
    path: &Path,
    budget: Option<Arc<budget::DatasetBudget>>,
) -> Result<SqlitePool> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create map bundle directory {}", parent.display()))?;
    SqlitePoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _| {
            let budget = budget.clone();
            Box::pin(async move {
                if let Some(budget) = budget {
                    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
                        .fetch_one(&mut *connection)
                        .await?;
                    let pages = budget.max_database_bytes() / page_size as u64;
                    // PRAGMA assignments cannot bind parameters; `pages` is an unsigned integer.
                    let actual: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                        "PRAGMA max_page_count={pages}"
                    )))
                    .fetch_one(&mut *connection)
                    .await?;
                    if actual as u64 > pages {
                        return Err(sqlx::Error::Protocol(
                            "existing RRLS database exceeds its page cap".into(),
                        ));
                    }
                    sqlx::query("PRAGMA cache_spill=OFF")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("PRAGMA temp_store=MEMORY")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("PRAGMA wal_autocheckpoint=0")
                        .execute(&mut *connection)
                        .await?;
                }
                Ok(())
            })
        })
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
        .with_context(|| {
            format!(
                "open map bundle {} (check directory ownership and write access)",
                path.display()
            )
        })
}

async fn build_tile_bundle_sqlite(tiles_root: &Path, bundle_path: &Path) -> Result<()> {
    let resume = should_resume_bundle();
    let existing_bundle = bundle_path.exists();
    let tmp_path = bundle_path.with_extension("sqlite.tmp");
    let directory = bundle_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let reserve = storage::minimum_free_bytes()?;
    storage::ensure_space(directory, reserve.saturating_add(16 * 1024 * 1024))?;
    if !resume && tmp_path.exists() {
        fs::remove_file(&tmp_path)?;
    }

    let working_path = if resume {
        bundle_path.to_path_buf()
    } else {
        tmp_path.clone()
    };

    println!(
        "building tile bundle sqlite from {} -> {}",
        tiles_root.display(),
        bundle_path.display()
    );
    if resume {
        if existing_bundle {
            println!(
                "bundle resume enabled: appending into existing {}",
                bundle_path.display()
            );
        } else {
            println!(
                "bundle resume enabled: creating new {}",
                bundle_path.display()
            );
        }
    } else {
        println!("bundle resume disabled: rebuilding from scratch");
    }

    let pool = open_bundle_database(&working_path).await?;
    sqlx::query("PRAGMA journal_mode = WAL;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA synchronous = NORMAL;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA locking_mode = NORMAL;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA temp_store = MEMORY;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA cache_size = -262144;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA page_size = 8192;")
        .execute(&pool)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tile_blobs (
            id INTEGER PRIMARY KEY,
            hash BLOB NOT NULL UNIQUE,
            image BLOB NOT NULL
        );",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tiles (
            z INTEGER NOT NULL,
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            blob_id INTEGER NOT NULL,
            PRIMARY KEY (z, x, y)
        ) WITHOUT ROWID;",
    )
    .execute(&pool)
    .await?;

    let mut tx = pool.begin().await?;
    let commit_every: u64 = 10_000;
    let mut pending_writes: u64 = 0;
    let mut pending_bytes: u64 = 0;

    let mut inserted: u64 = 0;
    let mut skipped_existing: u64 = 0;
    let mut z_dirs: Vec<_> = fs::read_dir(tiles_root)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    z_dirs.sort_by_key(|e| e.file_name());

    for z_entry in z_dirs {
        let z_name = z_entry.file_name();
        let Some(z_str) = z_name.to_str() else {
            continue;
        };
        let Ok(z) = z_str.parse::<u32>() else {
            continue;
        };

        let mut x_dirs: Vec<_> = fs::read_dir(z_entry.path())?
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        x_dirs.sort_by_key(|e| e.file_name());

        for x_entry in x_dirs {
            let x_name = x_entry.file_name();
            let Some(x_str) = x_name.to_str() else {
                continue;
            };
            let Ok(x) = x_str.parse::<u32>() else {
                continue;
            };

            let mut y_files: Vec<_> = fs::read_dir(x_entry.path())?
                .flatten()
                .filter(|e| e.path().is_file())
                .collect();
            y_files.sort_by_key(|e| e.file_name());

            for y_entry in y_files {
                let path = y_entry.path();
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if ext != TILE_EXT {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(y) = stem.parse::<u32>() else {
                    continue;
                };
                if resume {
                    let present: Option<i64> = sqlx::query_scalar(
                        "SELECT blob_id FROM tiles WHERE z = ? AND x = ? AND y = ? LIMIT 1",
                    )
                    .bind(i64::from(z))
                    .bind(i64::from(x))
                    .bind(i64::from(y))
                    .fetch_optional(&mut *tx)
                    .await?;
                    if present.is_some() {
                        skipped_existing += 1;
                        if skipped_existing.is_multiple_of(50_000) {
                            println!(
                                "bundle resume progress: skipped existing {skipped_existing} tiles"
                            );
                        }
                        continue;
                    }
                }
                use std::io::Read;
                let mut bytes = Vec::new();
                fs::File::open(&path)?
                    .take((network::MAX_TILE_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)?;
                anyhow::ensure!(
                    bytes.len() <= network::MAX_TILE_BYTES,
                    "saved tile exceeds512KiB; input files retained"
                );
                storage::ensure_space(
                    directory,
                    reserve
                        .saturating_add(pending_bytes)
                        .saturating_add(bytes.len() as u64)
                        .saturating_add(16 * 1024 * 1024),
                )?;
                let mut hasher = Hasher::new();
                hasher.update(&bytes);
                let hash = hasher.finalize();
                let hash_bytes = hash.as_bytes().to_vec();
                let blob_id = sqlx::query_scalar::<_, i64>(
                    "INSERT INTO tile_blobs (hash, image)
                     VALUES (?, ?)
                     ON CONFLICT(hash) DO UPDATE SET hash = excluded.hash
                     RETURNING id",
                )
                .bind(&hash_bytes)
                .bind(&bytes)
                .fetch_one(&mut *tx)
                .await?;

                sqlx::query("INSERT OR REPLACE INTO tiles (z, x, y, blob_id) VALUES (?, ?, ?, ?)")
                    .bind(i64::from(z))
                    .bind(i64::from(x))
                    .bind(i64::from(y))
                    .bind(blob_id)
                    .execute(&mut *tx)
                    .await?;
                inserted += 1;
                pending_writes += 1;
                pending_bytes = pending_bytes.saturating_add(bytes.len() as u64);
                if pending_writes >= commit_every || pending_bytes >= 16 * 1024 * 1024 {
                    tx.commit().await?;
                    tx = pool.begin().await?;
                    pending_writes = 0;
                    pending_bytes = 0;
                }
                if inserted.is_multiple_of(50_000) {
                    println!("bundle progress: inserted {inserted} tiles");
                }
            }
        }
    }

    tx.commit().await?;
    sqlx::query("ANALYZE;").execute(&pool).await?;
    sqlx::query("PRAGMA optimize;").execute(&pool).await?;
    // Fresh/append-only bundles need no whole-database rewrite. VACUUM can
    // otherwise consume another database-sized temporary allocation after
    // the download already respected its disk reserve.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
        .execute(&pool)
        .await?;
    println!("Map bundle finalized without a full-database VACUUM rewrite.");
    let unique_blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tile_blobs")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    pool.close().await;

    if !resume {
        if bundle_path.exists() {
            fs::remove_file(bundle_path)?;
        }
        fs::rename(&tmp_path, bundle_path)?;
    }
    println!(
        "tile bundle ready: {} (inserted {}, skipped existing {}, {} unique blobs)",
        bundle_path.display(),
        inserted,
        skipped_existing,
        unique_blobs
    );
    Ok(())
}

#[cfg(test)]
async fn init_direct_bundle_pool(bundle_path: &Path, resume: bool) -> Result<SqlitePool> {
    init_direct_bundle_pool_with_budget(bundle_path, resume, None).await
}

async fn init_direct_bundle_pool_with_budget(
    bundle_path: &Path,
    resume: bool,
    budget: Option<Arc<budget::DatasetBudget>>,
) -> Result<SqlitePool> {
    anyhow::ensure!(
        budget.is_none() || resume,
        "RRLS dataset must resume; existing data will not be deleted"
    );
    if !resume && bundle_path.exists() {
        fs::remove_file(bundle_path)?;
    }

    let pool = open_bundle_database_with_budget(bundle_path, budget.clone()).await?;
    if let Some(budget) = &budget {
        budget.checkpoint(&pool).await?;
    }

    sqlx::query("PRAGMA journal_mode = WAL;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA synchronous = NORMAL;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA locking_mode = NORMAL;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA temp_store = MEMORY;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA cache_size = -262144;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA page_size = 8192;")
        .execute(&pool)
        .await?;
    if let Some(budget) = &budget {
        budget.checkpoint(&pool).await?;
    }
    // One schema transaction keeps initialization inside the same WAL bound.
    let mut schema = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tile_blobs (
            id INTEGER PRIMARY KEY,
            hash BLOB NOT NULL UNIQUE,
            image BLOB NOT NULL
        );",
    )
    .execute(&mut *schema)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tiles (
            z INTEGER NOT NULL,
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            blob_id INTEGER NOT NULL,
            PRIMARY KEY (z, x, y)
        ) WITHOUT ROWID;",
    )
    .execute(&mut *schema)
    .await?;
    schema.commit().await?;
    if let Some(budget) = &budget {
        budget.checkpoint(&pool).await?;
    }
    Ok(pool)
}

async fn finalize_direct_bundle_pool(
    pool: &SqlitePool,
    budget: Option<&budget::DatasetBudget>,
) -> Result<()> {
    if let Some(budget) = budget {
        // Primary-key lookups need no optional statistics writes. Each extra
        // ANALYZE/optimize transaction could otherwise append another full WAL.
        return budget.checkpoint(pool).await;
    }
    sqlx::query("ANALYZE;").execute(pool).await?;
    sqlx::query("PRAGMA optimize;").execute(pool).await?;
    // Append-only downloads need no full-size VACUUM copy. Release WAL space.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
        .execute(pool)
        .await?;
    Ok(())
}

async fn remove_saved_coords(
    pool: &SqlitePool,
    z: u32,
    pending: &mut HashSet<(u32, u32)>,
) -> Result<()> {
    use futures::TryStreamExt;
    let mut rows = sqlx::query("SELECT t.x,t.y,length(b.image) AS bytes,substr(b.image,1,32) AS head,substr(b.image,-12) AS tail FROM tiles t LEFT JOIN tile_blobs b ON b.id=t.blob_id WHERE t.z=?")
        .bind(i64::from(z))
        .fetch(pool);
    let mut invalid = 0u64;
    while let Some(row) = rows
        .try_next()
        .await
        .context("resume scan failed; saved tiles retained")?
    {
        use sqlx::Row;
        let x: i64 = row.try_get("x")?;
        let y: i64 = row.try_get("y")?;
        if let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) {
            let length: Option<i64> = row.try_get("bytes")?;
            let head: Option<Vec<u8>> = row.try_get("head")?;
            let tail: Option<Vec<u8>> = row.try_get("tail")?;
            if length
                .zip(head)
                .zip(tail)
                .is_some_and(|((length, head), tail)| {
                    length >= 0 && raster::cached_header_ok(&head, &tail, length as u64)
                })
            {
                pending.remove(&(x, y));
            } else if pending.contains(&(x, y)) {
                invalid += 1;
            }
        }
    }
    if invalid > 0 {
        println!(
            "z={z}: {invalid} invalid cached tile headers queued for replacement; other saved tiles retained"
        );
    }
    Ok(())
}

#[cfg(test)]
async fn bundle_has_tile(pool: &SqlitePool, z: u32, x: u32, y: u32) -> Result<bool> {
    let present: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM tiles WHERE z = ? AND x = ? AND y = ? LIMIT 1")
            .bind(i64::from(z))
            .bind(i64::from(x))
            .bind(i64::from(y))
            .fetch_optional(pool)
            .await?;
    Ok(present.is_some())
}

#[cfg(test)]
async fn upsert_tile_to_bundle(
    pool: &SqlitePool,
    z: u32,
    x: u32,
    y: u32,
    bytes: &[u8],
) -> Result<()> {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    let hash_bytes = hash.as_bytes().to_vec();
    let blob_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO tile_blobs (hash, image)
         VALUES (?, ?)
         ON CONFLICT(hash) DO UPDATE SET hash = excluded.hash
         RETURNING id",
    )
    .bind(&hash_bytes)
    .bind(bytes)
    .fetch_one(pool)
    .await?;
    sqlx::query("INSERT OR REPLACE INTO tiles (z, x, y, blob_id) VALUES (?, ?, ?, ?)")
        .bind(i64::from(z))
        .bind(i64::from(x))
        .bind(i64::from(y))
        .bind(blob_id)
        .execute(pool)
        .await?;
    Ok(())
}

struct BandwidthLimiter {
    bytes_per_sec: f64,
    next_slot: AsyncMutex<Instant>,
}

impl BandwidthLimiter {
    fn new(mib_per_sec: f64) -> Self {
        Self {
            bytes_per_sec: mib_per_sec * 1024.0 * 1024.0,
            next_slot: AsyncMutex::new(Instant::now()),
        }
    }

    async fn throttle_bytes(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let wait = {
            let mut next = self.next_slot.lock().await;
            let now = Instant::now();
            let start = if *next > now { *next } else { now };
            let slot = Duration::from_secs_f64(((bytes as f64) / self.bytes_per_sec).max(0.0));
            *next = start + slot;
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            sleep(wait).await;
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let profile = Profile::configured()?;
    let region = env::var("MAP_REGION").unwrap_or_else(|_| profile.default_region().to_string());
    let max_zoom = env::var("MAP_MAX_ZOOM")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_ZOOM);

    if max_zoom > 30 {
        anyhow::bail!("MAP_MAX_ZOOM must be between 0 and 30");
    }
    profile.validate(
        max_zoom,
        should_direct_to_bundle(),
        should_resume_bundle(),
        should_build_bundle(),
    )?;
    let options = options::Options::parse(env::args().skip(1))?;
    options.validate(
        profile,
        max_zoom,
        should_resume_bundle(),
        should_direct_to_bundle(),
        should_build_bundle(),
    )?;
    if options.test {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| env::current_dir().expect("failed to get current dir"));
        let data_dir = manifest_dir.join("../backend/data/maps").join(&region);
        let cache = cache_probe::check(
            &bundle_path_for(&data_dir),
            &data_dir.join("tiles"),
            &connection_test::sample_tiles_for_profile(max_zoom, profile),
        )
        .await;
        let client = TileClient::configured(&format!("{SATELLITE_BASE_URL}/0/0/0")).await?;
        let network = connection_test::run(
            &client,
            &region,
            profile,
            max_zoom,
            SATELLITE_BASE_URL,
            env::var_os("MAP_DIRECT_INTERFACE").is_some(),
        )
        .await;
        cache?;
        return network;
    }

    limits::prepare(if options.repairs.is_empty() {
        max_concurrent()
    } else {
        max_concurrent().min(options.repairs.len())
    })?;

    // Use CARGO_MANIFEST_DIR if present (when run via `cargo run`),
    // otherwise fall back to current directory.
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().expect("failed to get current dir"));

    // ./data/maps/<region>/tiles (adjusted to ../backend/data)
    let data_dir = manifest_dir
        .join("../backend/data")
        .join("maps")
        .join(&region);
    let tiles_root = data_dir.join("tiles");
    let direct_to_bundle = should_direct_to_bundle();

    if !direct_to_bundle {
        fs::create_dir_all(&tiles_root)?;
        println!(
            "fetch_satellite_tiles_async: populating satellite tiles for region '{}' into {} (z={MIN_ZOOM}..={max_zoom})",
            region,
            tiles_root.display()
        );
    } else {
        println!(
            "fetch_satellite_tiles_async: populating bundle DB directly for region '{}' (z={MIN_ZOOM}..={max_zoom})",
            region
        );
    }

    // Async HTTP client
    let client = TileClient::configured(&format!("{SATELLITE_BASE_URL}/0/0/0")).await?;

    if !options.repairs.is_empty() {
        return repair::run(
            &options.repairs,
            &bundle_path_for(&data_dir),
            &tiles_root,
            &client,
            profile,
            direct_to_bundle,
            MAX_RETRY_ATTEMPTS,
            SATELLITE_BASE_URL,
        )
        .await;
    }

    if direct_to_bundle {
        if !should_build_bundle() {
            eprintln!(
                "WARNING: MAP_DIRECT_TO_BUNDLE requested but MAP_BUILD_BUNDLE is disabled; nothing to do."
            );
        } else {
            let bundle_path = bundle_path_for(&data_dir);
            let resume = should_resume_bundle();
            let budget = if profile == Profile::Rrls {
                Some(budget::DatasetBudget::open(&bundle_path)?)
            } else {
                None
            };
            let pool =
                init_direct_bundle_pool_with_budget(&bundle_path, resume, budget.clone()).await?;
            for z in MIN_ZOOM..=max_zoom {
                if z != MIN_ZOOM {
                    println!();
                    println!("----");
                }
                fetch_tiles_for_zoom_async_to_db(z, &pool, &client, profile, budget.clone())
                    .await
                    .map_err(|error| anyhow::anyhow!("map zoom {z} failed: {error}"))?;
            }
            let finalized = finalize_direct_bundle_pool(&pool, budget.as_deref()).await;
            if budget.is_some() {
                finalized.context("RRLS finalization failed; saved tiles retained")?;
            } else if let Err(e) = finalized {
                eprintln!(
                    "WARNING: failed finalizing tile bundle {}: {e:#}",
                    bundle_path.display()
                );
            }
            let unique_blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tile_blobs")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            let total_tiles: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tiles")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            pool.close().await;
            println!(
                "tile bundle ready: {} ({} tiles, {} unique blobs)",
                bundle_path.display(),
                total_tiles,
                unique_blobs
            );
        }
    } else {
        for z in MIN_ZOOM..=max_zoom {
            if z != MIN_ZOOM {
                println!();
                println!("----");
            }
            fetch_tiles_for_zoom_async(z, &tiles_root, &client, profile)
                .await
                .map_err(|error| anyhow::anyhow!("map zoom {z} failed: {error}"))?;
        }

        if should_build_bundle() {
            let bundle_path = bundle_path_for(&data_dir);
            build_tile_bundle_sqlite(&tiles_root, &bundle_path)
                .await
                .with_context(|| {
                    format!(
                        "building map bundle {}; saved tiles retained",
                        bundle_path.display()
                    )
                })?;
            if !should_keep_tiles() {
                match fs::remove_dir_all(&tiles_root) {
                    Ok(_) => {
                        println!(
                            "Removed tile directory after successful bundle build (set MAP_KEEP_TILES=1 to keep): {}",
                            tiles_root.display()
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "WARNING: failed removing tile directory {}: {e}",
                            tiles_root.display()
                        );
                    }
                }
            }
        } else {
            println!("Skipping tile bundle generation (MAP_BUILD_BUNDLE disabled).");
        }
    }

    println!("fetch_satellite_tiles_async: done populating satellite tiles.");
    Ok(())
}

/// Download satellite tiles at zoom level `z` with tiered coverage:
/// - z=0..=8: full North America bounds
/// - z=9..=MAX: Buffalo/Rochester + West Texas desert + URRG bounds
async fn fetch_tiles_for_zoom_async(
    z: u32,
    tiles_root: &Path,
    client: &TileClient,
    profile: Profile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bounds = profile.bounds(z);

    // Enumerate + de-duplicate all tile coordinates across selected bounds.
    let mut coord_set = HashSet::<(u32, u32)>::new();
    for bbox in bounds {
        let (x_start, x_end, y_start, y_end) = tile_range_for_bounds(*bbox, z);
        for x in x_start..=x_end {
            for y in y_start..=y_end {
                coord_set.insert((x, y));
            }
        }
    }
    let mut coords: Vec<(u32, u32)> = coord_set.into_iter().collect();
    coords.sort_unstable();

    let (x_start, x_end, y_start, y_end) = bounds_tile_extent(&coords);

    let total = coords.len() as u64;
    println!(
        "z={z}: fetching satellite tiles x=[{}..={}], y=[{}..={}], total={} tiles",
        x_start, x_end, y_start, y_end, total
    );
    let is_tty = std::io::stdout().is_terminal();

    // Create the base z directory once (sync is fine here)
    let z_dir = tiles_root.join(format!("{z}"));
    fs::create_dir_all(&z_dir)?;

    // Pre-create all x directories once (avoid per-tile mkdir)
    let mut x_dirs = HashSet::new();
    for (x, _) in &coords {
        x_dirs.insert(*x);
    }
    for x in x_dirs {
        let x_dir = z_dir.join(format!("{x}"));
        if let Err(e) = fs::create_dir_all(&x_dir) {
            eprintln!(
                "fetch_satellite_tiles_async: failed to create directory {}: {e}",
                x_dir.display()
            );
        }
    }

    // Progress reporting: use a live bar on TTY, plain lines otherwise.
    let start = std::time::Instant::now();
    let done_count = Arc::new(AtomicU64::new(0));
    let bytes_downloaded = Arc::new(AtomicU64::new(0));
    let stop_rate_updater = Arc::new(AtomicBool::new(false));
    let pb = if is_tty {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(&format!(
                "z={z} [{{bar:40.cyan/blue}}] {{percent:>3}}% {{msg}} ({{pos}}/{{len}})"
            ))?
            .progress_chars("=> "),
        );
        pb.set_draw_target(ProgressDrawTarget::stdout_with_hz(10));
        pb.set_message("0.00 MiB/s");
        Some(pb)
    } else {
        None
    };
    let max_concurrent = max_concurrent();
    let max_bandwidth = max_bandwidth_mibps();
    let bandwidth_limiter = max_bandwidth.map(BandwidthLimiter::new).map(Arc::new);

    let done_for_rate = done_count.clone();
    let bytes_for_rate = bytes_downloaded.clone();
    let stop_for_rate = stop_rate_updater.clone();
    let pb_for_rate = pb.clone();
    let rate_updater = tokio::spawn(async move {
        let mut last_bucket: i64 = -1;
        loop {
            if stop_for_rate.load(Ordering::Relaxed) {
                break;
            }
            let done = done_for_rate.load(Ordering::Relaxed);
            let elapsed_s = start.elapsed().as_secs_f64().max(0.001);
            let bytes = bytes_for_rate.load(Ordering::Relaxed) as f64;
            let mib_per_s = (bytes / elapsed_s) / (1024.0 * 1024.0);
            let pct = done.saturating_mul(100).checked_div(total).unwrap_or(100);
            let bucket = (pct / PROGRESS_PERCENT_STEP) as i64;

            if let Some(pb) = &pb_for_rate {
                pb.set_position(done);
                pb.set_message(format!(
                    "ETA {} {mib_per_s:.2} MiB/s",
                    sane_eta(done, total, elapsed_s)
                ));
            } else if bucket != last_bucket || done == total {
                let eta = sane_eta(done, total, elapsed_s);
                println!("z={z} {pct}% ETA {eta} {mib_per_s:.2} MiB/s ({done}/{total})");
                last_bucket = bucket;
            }

            sleep(Duration::from_millis(400)).await;
        }
    });

    let z_dir_arc = z_dir.clone();
    let client_arc = client.clone(); // cheap clone
    let done_count_clone = done_count.clone();
    let bytes_downloaded_clone = bytes_downloaded.clone();
    let pb_for_workers = pb.clone();
    let limiter_for_workers = bandwidth_limiter.clone();
    let failed_coords = Arc::new(AsyncMutex::new(Vec::<(u32, u32)>::new()));
    let failed_coords_for_workers = failed_coords.clone();
    let permanent_failed = Arc::new(AtomicU64::new(0));
    let permanent_for_workers = permanent_failed.clone();
    let unavailable = Arc::new(AtomicU64::new(0));
    let unavailable_for_workers = unavailable.clone();
    // Build an async stream of all coordinate tasks
    stream::iter(coords)
        .for_each_concurrent(max_concurrent, move |(x, y)| {
            let z_dir = z_dir_arc.clone();
            let client = client_arc.clone();
            let done_count = done_count_clone.clone();
            let bytes_downloaded = bytes_downloaded_clone.clone();
            let pb_for_worker = pb_for_workers.clone();
            let limiter_for_worker = limiter_for_workers.clone();
            let failed_coords = failed_coords_for_workers.clone();
            let permanent_failed = permanent_for_workers.clone();
            let unavailable = unavailable_for_workers.clone();

            async move {
                let ok = fetch_tile_with_retries(
                    z,
                    x,
                    y,
                    &z_dir,
                    &client,
                    &bytes_downloaded,
                    pb_for_worker.as_ref(),
                    limiter_for_worker.as_ref(),
                    MAX_RETRY_ATTEMPTS,
                    SATELLITE_BASE_URL,
                    false,
                )
                .await;
                match ok {
                    TileOutcome::Complete => {
                        done_count.fetch_add(1, Ordering::Relaxed);
                    }
                    TileOutcome::Unavailable => {
                        unavailable.fetch_add(1, Ordering::Relaxed);
                        done_count.fetch_add(1, Ordering::Relaxed);
                    }
                    TileOutcome::Exhausted => failed_coords.lock().await.push((x, y)),
                    TileOutcome::Permanent => {
                        permanent_failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .await;

    let retry_coords = {
        let mut failed = failed_coords.lock().await;
        std::mem::take(&mut *failed)
    };
    let mut terminal_failed = 0;
    if !retry_coords.is_empty() {
        log_progress_error(
            pb.as_ref(),
            format!(
                "z={z}: second-pass retry for {} tiles that failed in main pass",
                retry_coords.len()
            ),
        );

        let retry_failures = Arc::new(AsyncMutex::new(Vec::<(u32, u32)>::new()));
        let retry_failures_workers = retry_failures.clone();
        let done_retry = done_count.clone();
        let permanent_retry = permanent_failed.clone();
        let unavailable_retry = unavailable.clone();
        let z_dir_retry = z_dir.clone();
        let client_retry = client.clone();
        let bytes_retry = bytes_downloaded.clone();
        let pb_retry = pb.clone();
        let limiter_retry = bandwidth_limiter.clone();
        stream::iter(retry_coords)
            .for_each_concurrent(max_concurrent, move |(x, y)| {
                let z_dir = z_dir_retry.clone();
                let client = client_retry.clone();
                let bytes_downloaded = bytes_retry.clone();
                let pb = pb_retry.clone();
                let limiter = limiter_retry.clone();
                let retry_failures = retry_failures_workers.clone();
                let done_count = done_retry.clone();
                let permanent_failed = permanent_retry.clone();
                let unavailable = unavailable_retry.clone();
                async move {
                    let ok = fetch_tile_with_retries(
                        z,
                        x,
                        y,
                        &z_dir,
                        &client,
                        &bytes_downloaded,
                        pb.as_ref(),
                        limiter.as_ref(),
                        MAX_RETRY_ATTEMPTS,
                        SATELLITE_BASE_URL,
                        false,
                    )
                    .await;
                    match ok {
                        TileOutcome::Complete => {
                            done_count.fetch_add(1, Ordering::Relaxed);
                        }
                        TileOutcome::Unavailable => {
                            unavailable.fetch_add(1, Ordering::Relaxed);
                            done_count.fetch_add(1, Ordering::Relaxed);
                        }
                        TileOutcome::Exhausted => retry_failures.lock().await.push((x, y)),
                        TileOutcome::Permanent => {
                            permanent_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
            .await;

        let final_failed_count = retry_failures.lock().await.len();
        terminal_failed = final_failed_count;
        if final_failed_count > 0 {
            log_progress_error(
                pb.as_ref(),
                format!("z={z}: {final_failed_count} tiles still failed after second-pass retry"),
            );
        }
    }

    terminal_failed += permanent_failed.load(Ordering::Relaxed) as usize;
    stop_rate_updater.store(true, Ordering::Relaxed);
    let _ = rate_updater.await;
    if terminal_failed != 0 {
        return Err(anyhow::anyhow!(
            "{terminal_failed} tiles unresolved (permanent responses or exhausted retries); rerun to resume"
        )
        .into());
    }
    let elapsed_s = start.elapsed().as_secs_f64().max(0.001);
    let bytes = bytes_downloaded.load(Ordering::Relaxed) as f64;
    let avg_mib_per_s = (bytes / elapsed_s) / (1024.0 * 1024.0);
    if let Some(pb) = pb {
        pb.finish_and_clear();
    }
    let unavailable = unavailable.load(Ordering::Relaxed);
    let available = done_count
        .load(Ordering::Relaxed)
        .saturating_sub(unavailable);
    println!(
        "z={z} complete: {available} available, {unavailable} unavailable in {elapsed_s:.1}s at {avg_mib_per_s:.2} MiB/s"
    );
    Ok(())
}

async fn write_tile_atomic_async(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_tile_atomic_with_reserve(
        path,
        bytes,
        storage::minimum_free_bytes().map_err(std::io::Error::other)?,
    )
    .await
}

async fn write_tile_atomic_with_reserve(
    path: &Path,
    bytes: &[u8],
    reserve: u64,
) -> std::io::Result<()> {
    if bytes.len() > network::MAX_TILE_BYTES {
        return Err(std::io::Error::other(
            "tile exceeds bounded atomic-write size",
        ));
    }
    const WRITE_SLOTS: usize = 32;
    const WRITE_HEADROOM: u64 =
        WRITE_SLOTS as u64 * network::MAX_TILE_BYTES as u64 + 16 * 1024 * 1024;
    static WRITERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let _permit = WRITERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(WRITE_SLOTS)))
        .acquire()
        .await
        .map_err(std::io::Error::other)?;
    let directory = path.parent().unwrap_or(Path::new("."));
    // Allow for every admitted atomic file write before consuming the disk
    // reserve. HTTP concurrency is independent of these bounded disk writes.
    storage::ensure_space(directory, reserve.saturating_add(WRITE_HEADROOM))
        .map_err(std::io::Error::other)?;
    let tmp_path = path.with_extension(format!(
        "{}.part",
        path.extension().unwrap().to_string_lossy()
    ));

    // Write into .part file
    async_fs::write(&tmp_path, bytes).await?;

    // Atomically replace final file
    async_fs::rename(&tmp_path, path).await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn fetch_tile_with_retries_to_db(
    z: u32,
    x: u32,
    y: u32,
    client: &TileClient,
    writer: &storage::BundleWriter,
    bytes_downloaded: &Arc<AtomicU64>,
    pb: Option<&ProgressBar>,
    limiter: Option<&Arc<BandwidthLimiter>>,
    max_attempts: u32,
    base_url: &str,
) -> TileOutcome {
    let url = format!("{base}/{z}/{y}/{x}", base = base_url, z = z, y = y, x = x,);

    let mut attempts: u32 = 0;
    loop {
        if writer.failed() {
            return TileOutcome::Exhausted;
        }
        attempts += 1;
        // Transient failures stay quiet. Keep the final cause for one useful
        // report only if this tile exhausts its bounded retry allowance.
        let failure = match fetch_valid_raster(client, &url).await {
            Ok(resp) => {
                let status = resp.status;
                if status.is_success() {
                    let bytes = resp.bytes;
                    if let Some(lim) = limiter {
                        lim.throttle_bytes(bytes.len()).await;
                    }
                    bytes_downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match writer.save(z, x, y, &bytes).await {
                        Ok(()) => return TileOutcome::Complete,
                        Err(_) => return TileOutcome::Exhausted,
                    }
                } else if status.as_u16() == 404 {
                    return TileOutcome::Unavailable;
                } else if status.is_client_error() && status.as_u16() != 429 {
                    log_progress_error(
                        pb,
                        format!(
                            "fetch_satellite_tiles_async: skipping non-retryable HTTP {} for tile z={z}, x={x}, y={y}",
                            status.as_u16()
                        ),
                    );
                    return TileOutcome::Permanent;
                } else {
                    format!("HTTP {status}")
                }
            }
            Err(error) if error.is::<network::OversizedTile>() => {
                log_progress_error(
                    pb,
                    format!("tile z={z},x={x},y={y} exceeds512KiB; not retried"),
                );
                return TileOutcome::Permanent;
            }
            Err(error) => format!("request or response body: {error:#}"),
        };
        if attempts >= max_attempts {
            log_progress_error(
                pb,
                format!(
                    "fetch_satellite_tiles_async: giving up on tile z={z}, x={x}, y={y} after {attempts} attempts: {failure}"
                ),
            );
            return TileOutcome::Exhausted;
        }
        sleep(Duration::from_millis(retry_backoff_ms(attempts, z, x, y))).await;
    }
}

async fn fetch_tiles_for_zoom_async_to_db(
    z: u32,
    pool: &SqlitePool,
    client: &TileClient,
    profile: Profile,
    budget: Option<Arc<budget::DatasetBudget>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bounds = profile.bounds(z);

    let mut coord_set = HashSet::<(u32, u32)>::new();
    for bbox in bounds {
        let (x_start, x_end, y_start, y_end) = tile_range_for_bounds(*bbox, z);
        for x in x_start..=x_end {
            for y in y_start..=y_end {
                coord_set.insert((x, y));
            }
        }
    }
    let requested = coord_set.len();
    remove_saved_coords(pool, z, &mut coord_set).await?;
    let skipped = requested - coord_set.len();
    println!(
        "z={z}: resumed {skipped} saved tiles; {} remaining",
        coord_set.len()
    );
    if coord_set.is_empty() {
        return Ok(());
    }
    let mut coords: Vec<(u32, u32)> = coord_set.into_iter().collect();
    coords.sort_unstable();
    let (x_start, x_end, y_start, y_end) = bounds_tile_extent(&coords);
    let total = coords.len() as u64;
    println!(
        "z={z}: fetching satellite tiles directly to bundle x=[{}..={}], y=[{}..={}], total={} tiles",
        x_start, x_end, y_start, y_end, total
    );
    let is_tty = std::io::stdout().is_terminal();

    let directory = pool
        .connect_options()
        .get_filename()
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let reserve = storage::minimum_free_bytes()?;
    storage::ensure_space(&directory, reserve.saturating_add(16 * 1024 * 1024))?;

    let start = std::time::Instant::now();
    let done_count = Arc::new(AtomicU64::new(0));
    let bytes_downloaded = Arc::new(AtomicU64::new(0));
    let stop_rate_updater = Arc::new(AtomicBool::new(false));
    let pb = if is_tty {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(&format!(
                "z={z} [{{bar:40.green/blue}}] {{percent:>3}}% {{msg}} ({{pos}}/{{len}})"
            ))?
            .progress_chars("=> "),
        );
        pb.set_draw_target(ProgressDrawTarget::stdout_with_hz(10));
        pb.set_message("0.00 MiB/s");
        Some(pb)
    } else {
        None
    };
    let max_concurrent = max_concurrent();
    let max_bandwidth = max_bandwidth_mibps();
    let bandwidth_limiter = max_bandwidth.map(BandwidthLimiter::new).map(Arc::new);

    let done_for_rate = done_count.clone();
    let bytes_for_rate = bytes_downloaded.clone();
    let stop_for_rate = stop_rate_updater.clone();
    let pb_for_rate = pb.clone();
    let rate_updater = tokio::spawn(async move {
        let mut last_bucket: i64 = -1;
        loop {
            if stop_for_rate.load(Ordering::Relaxed) {
                break;
            }
            let done = done_for_rate.load(Ordering::Relaxed);
            let elapsed_s = start.elapsed().as_secs_f64().max(0.001);
            let bytes = bytes_for_rate.load(Ordering::Relaxed) as f64;
            let mib_per_s = (bytes / elapsed_s) / (1024.0 * 1024.0);
            let pct = done.saturating_mul(100).checked_div(total).unwrap_or(100);
            let bucket = (pct / PROGRESS_PERCENT_STEP) as i64;
            if let Some(pb) = &pb_for_rate {
                pb.set_position(done);
                pb.set_message(format!(
                    "ETA {} {mib_per_s:.2} MiB/s",
                    sane_eta(done, total, elapsed_s)
                ));
            } else if bucket != last_bucket || done == total {
                let eta = sane_eta(done, total, elapsed_s);
                println!("z={z} {pct}% ETA {eta} {mib_per_s:.2} MiB/s ({done}/{total})");
                last_bucket = bucket;
            }
            sleep(Duration::from_millis(400)).await;
        }
    });

    let (writer, writer_task) =
        storage::spawn_with_budget(pool.clone(), directory, reserve, budget);
    let client_arc = client.clone();
    let writer_arc = writer.clone();
    let writer_for_admission = writer.clone();
    let done_count_clone = done_count.clone();
    let bytes_downloaded_clone = bytes_downloaded.clone();
    let pb_for_workers = pb.clone();
    let limiter_for_workers = bandwidth_limiter.clone();
    let failed_coords = Arc::new(AsyncMutex::new(Vec::<(u32, u32)>::new()));
    let failed_coords_for_workers = failed_coords.clone();
    let permanent_failed = Arc::new(AtomicU64::new(0));
    let permanent_for_workers = permanent_failed.clone();
    let unavailable = Arc::new(AtomicU64::new(0));
    let unavailable_for_workers = unavailable.clone();
    stream::iter(coords)
        .take_while(move |_| futures::future::ready(!writer_for_admission.failed()))
        .for_each_concurrent(max_concurrent, move |(x, y)| {
            let client = client_arc.clone();
            let writer = writer_arc.clone();
            let done_count = done_count_clone.clone();
            let bytes_downloaded = bytes_downloaded_clone.clone();
            let pb_for_worker = pb_for_workers.clone();
            let limiter_for_worker = limiter_for_workers.clone();
            let failed_coords = failed_coords_for_workers.clone();
            let permanent_failed = permanent_for_workers.clone();
            let unavailable = unavailable_for_workers.clone();
            async move {
                let ok = fetch_tile_with_retries_to_db(
                    z,
                    x,
                    y,
                    &client,
                    &writer,
                    &bytes_downloaded,
                    pb_for_worker.as_ref(),
                    limiter_for_worker.as_ref(),
                    MAX_RETRY_ATTEMPTS,
                    SATELLITE_BASE_URL,
                )
                .await;
                match ok {
                    TileOutcome::Complete => {
                        done_count.fetch_add(1, Ordering::Relaxed);
                    }
                    TileOutcome::Unavailable => {
                        unavailable.fetch_add(1, Ordering::Relaxed);
                        done_count.fetch_add(1, Ordering::Relaxed);
                    }
                    TileOutcome::Exhausted => failed_coords.lock().await.push((x, y)),
                    TileOutcome::Permanent => {
                        permanent_failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .await;

    let retry_coords = {
        let mut failed = failed_coords.lock().await;
        std::mem::take(&mut *failed)
    };
    let mut terminal_failed = 0;
    if !retry_coords.is_empty() && !writer.failed() {
        log_progress_error(
            pb.as_ref(),
            format!(
                "z={z}: second-pass retry for {} tiles that failed in main pass",
                retry_coords.len()
            ),
        );
        let retry_failures = Arc::new(AsyncMutex::new(Vec::<(u32, u32)>::new()));
        let retry_failures_workers = retry_failures.clone();
        let done_retry = done_count.clone();
        let permanent_retry = permanent_failed.clone();
        let unavailable_retry = unavailable.clone();
        let client_retry = client.clone();
        let writer_retry = writer.clone();
        let bytes_retry = bytes_downloaded.clone();
        let pb_retry = pb.clone();
        let limiter_retry = bandwidth_limiter.clone();
        stream::iter(retry_coords)
            .for_each_concurrent(max_concurrent, move |(x, y)| {
                let client = client_retry.clone();
                let writer = writer_retry.clone();
                let bytes_downloaded = bytes_retry.clone();
                let pb = pb_retry.clone();
                let limiter = limiter_retry.clone();
                let retry_failures = retry_failures_workers.clone();
                let done_count = done_retry.clone();
                let permanent_failed = permanent_retry.clone();
                let unavailable = unavailable_retry.clone();
                async move {
                    let ok = fetch_tile_with_retries_to_db(
                        z,
                        x,
                        y,
                        &client,
                        &writer,
                        &bytes_downloaded,
                        pb.as_ref(),
                        limiter.as_ref(),
                        MAX_RETRY_ATTEMPTS,
                        SATELLITE_BASE_URL,
                    )
                    .await;
                    match ok {
                        TileOutcome::Complete => {
                            done_count.fetch_add(1, Ordering::Relaxed);
                        }
                        TileOutcome::Unavailable => {
                            unavailable.fetch_add(1, Ordering::Relaxed);
                            done_count.fetch_add(1, Ordering::Relaxed);
                        }
                        TileOutcome::Exhausted => retry_failures.lock().await.push((x, y)),
                        TileOutcome::Permanent => {
                            permanent_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
            .await;
        let final_failed_count = retry_failures.lock().await.len();
        terminal_failed = final_failed_count;
        if final_failed_count > 0 {
            log_progress_error(
                pb.as_ref(),
                format!("z={z}: {final_failed_count} tiles still failed after second-pass retry"),
            );
        }
    }

    terminal_failed += permanent_failed.load(Ordering::Relaxed) as usize;
    stop_rate_updater.store(true, Ordering::Relaxed);
    let _ = rate_updater.await;
    let writer_failure = writer.failure();
    drop(writer);
    writer_task.await?;
    if let Some(error) = writer_failure {
        return Err(
            anyhow::anyhow!("map bundle writer failed; saved tiles retained: {error}").into(),
        );
    }
    if terminal_failed != 0 {
        return Err(anyhow::anyhow!(
            "{terminal_failed} tiles unresolved (permanent responses or exhausted retries); rerun to resume"
        )
        .into());
    }
    let elapsed_s = start.elapsed().as_secs_f64().max(0.001);
    let bytes = bytes_downloaded.load(Ordering::Relaxed) as f64;
    let avg_mib_per_s = (bytes / elapsed_s) / (1024.0 * 1024.0);
    if let Some(pb) = pb {
        pb.finish_and_clear();
    }
    let unavailable = unavailable.load(Ordering::Relaxed);
    let available = done_count
        .load(Ordering::Relaxed)
        .saturating_sub(unavailable);
    println!(
        "z={z} complete: {available} available, {unavailable} unavailable in {elapsed_s:.1}s at {avg_mib_per_s:.2} MiB/s"
    );
    Ok(())
}

/// Convert lon/lat (deg) to XYZ tile indices for Web Mercator at zoom `z`.
fn lonlat_to_tile(lon_deg: f64, lat_deg: f64, zoom: u32) -> (u32, u32) {
    let lat_rad = lat_deg.to_radians();
    let n = 2f64.powi(zoom as i32);

    let x = ((lon_deg + 180.0) / 360.0 * n).floor();
    let y = (1.0 - ((lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI)) / 2.0 * n;

    let max_idx = n - 1.0;
    let x = x.max(0.0).min(max_idx) as u32;
    let y = y.max(0.0).min(max_idx) as u32;

    (x, y)
}

fn tile_range_for_bounds(bounds: (f64, f64, f64, f64), z: u32) -> (u32, u32, u32, u32) {
    let (lon_min, lat_min, lon_max, lat_max) = bounds;
    let (x_min, y_max) = lonlat_to_tile(lon_min, lat_min, z);
    let (x_max, y_min) = lonlat_to_tile(lon_max, lat_max, z);
    (
        x_min.min(x_max),
        x_min.max(x_max),
        y_min.min(y_max),
        y_min.max(y_max),
    )
}

fn bounds_tile_extent(coords: &[(u32, u32)]) -> (u32, u32, u32, u32) {
    let mut x_min = u32::MAX;
    let mut x_max = 0u32;
    let mut y_min = u32::MAX;
    let mut y_max = 0u32;
    for (x, y) in coords {
        x_min = x_min.min(*x);
        x_max = x_max.max(*x);
        y_min = y_min.min(*y);
        y_max = y_max.max(*y);
    }
    (x_min, x_max, y_min, y_max)
}

#[cfg(test)]
mod bundle_database_tests {
    use super::*;

    #[test]
    fn coverage_keeps_north_america_at_low_zoom_and_launch_regions_at_high_zoom() {
        for z in MIN_ZOOM..=BASE_COVERAGE_MAX_ZOOM {
            assert_eq!(coverage_bounds(z), &[NA_BOUNDS]);
        }
        for z in BASE_COVERAGE_MAX_ZOOM + 1..=DEFAULT_MAX_ZOOM {
            assert_eq!(
                coverage_bounds(z),
                &[BUFFALO_ROCHESTER_BOUNDS, TEXAS_DESERT_BOUNDS, URRG_BOUNDS]
            );
            // URRG's field map center was outside the old Buffalo/Rochester bounds.
            let (x, y) = lonlat_to_tile(-77.1941, 42.7004, z);
            let (x_min, x_max, y_min, y_max) = tile_range_for_bounds(URRG_BOUNDS, z);
            assert!((x_min..=x_max).contains(&x));
            assert!((y_min..=y_max).contains(&y));
        }
        assert!(-77.1941 > BUFFALO_ROCHESTER_BOUNDS.2);
    }

    fn scratch(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "gs-map-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn first_download_creates_database_and_resume_keeps_tiles() {
        let root = scratch("create-resume");
        let path = root.join("new maps ?#%/tiles.sqlite3");
        assert!(!path.exists());
        let pool = init_direct_bundle_pool(&path, true).await.unwrap();
        upsert_tile_to_bundle(&pool, 0, 0, 0, b"tile-one")
            .await
            .unwrap();
        pool.close().await;
        assert!(path.is_file());
        let resumed = init_direct_bundle_pool(&path, true).await.unwrap();
        assert!(bundle_has_tile(&resumed, 0, 0, 0).await.unwrap());
        upsert_tile_to_bundle(&resumed, 1, 0, 0, b"tile-one")
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tile_blobs")
            .fetch_one(&resumed)
            .await
            .unwrap();
        assert_eq!(count, 1);
        resumed.close().await;
        let reset = init_direct_bundle_pool(&path, false).await.unwrap();
        assert!(!bundle_has_tile(&reset, 0, 0, 0).await.unwrap());
        reset.close().await;
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn station_can_read_committed_tiles_while_download_remains_open() {
        let root = scratch("concurrent-reader");
        let path = root.join("tiles.sqlite3");
        let writer = init_direct_bundle_pool(&path, true).await.unwrap();
        upsert_tile_to_bundle(&writer, 0, 0, 0, b"tile-one")
            .await
            .unwrap();
        let reader = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path).read_only(true))
            .await
            .unwrap();
        let mut transaction = writer.begin().await.unwrap();
        sqlx::query("INSERT INTO tiles(z,x,y,blob_id) SELECT 1,0,0,blob_id FROM tiles LIMIT 1")
            .execute(&mut *transaction)
            .await
            .unwrap();
        assert!(bundle_has_tile(&reader, 0, 0, 0).await.unwrap());
        assert!(!bundle_has_tile(&reader, 1, 0, 0).await.unwrap());
        transaction.commit().await.unwrap();
        assert!(bundle_has_tile(&reader, 1, 0, 0).await.unwrap());
        reader.close().await;
        writer.close().await;
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn legacy_bundling_creates_an_absent_output_database() {
        let root = scratch("legacy");
        let tiles = root.join("tiles");
        fs::create_dir_all(tiles.join("0/0")).unwrap();
        fs::write(tiles.join("0/0/0.jpg"), b"tile-one").unwrap();
        let path = root.join("output/tiles.sqlite3");
        build_tile_bundle_sqlite(&tiles, &path).await.unwrap();
        let pool = open_bundle_database(&path).await.unwrap();
        assert!(bundle_has_tile(&pool, 0, 0, 0).await.unwrap());
        pool.close().await;
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod tile_retry_tests {
    use super::*;
    use std::io::{Read, Write};

    fn image() -> Vec<u8> {
        raster::image()
    }
    fn server(
        failures: usize,
        success: bool,
        complete_bad_body: bool,
    ) -> (String, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            let expected = failures + usize::from(success);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut count = 0;
            while count < expected && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") && request.len() < 4096 {
                            let mut chunk = [0_u8; 512];
                            let count = socket.read(&mut chunk).unwrap();
                            assert!(count != 0, "client closed before request headers");
                            request.extend_from_slice(&chunk[..count]);
                        }
                        assert!(request.ends_with(b"\r\n\r\n"));
                        let bytes = if count < failures && complete_bad_body {
                            b"<html>upstream temporarily unavailable</html>".to_vec()
                        } else {
                            image()
                        };
                        let length = bytes.len()
                            + if count < failures && !complete_bad_body {
                                100
                            } else {
                                0
                            };
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").unwrap();
                        socket.write_all(&bytes).unwrap();
                        count += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("test tile listener: {e}"),
                }
            }
            count
        });
        (url, thread)
    }

    async fn check_retry(direct: bool, recover: bool) {
        check_retry_case(direct, recover, false, false).await;
    }
    async fn check_retry_case(
        direct: bool,
        recover: bool,
        complete_bad_body: bool,
        corrupt_cached: bool,
    ) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::temp_dir().join(format!(
            "gs-tile-retry-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("0/0")).unwrap();
        let failures = if recover { 1 } else { 2 };
        let (base, thread) = server(failures, recover, complete_bad_body);
        let client = TileClient::from(
            Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let bytes = Arc::new(AtomicU64::new(0));
        if direct {
            let pool = init_direct_bundle_pool(&root.join("tiles.sqlite3"), true)
                .await
                .unwrap();
            if corrupt_cached {
                upsert_tile_to_bundle(&pool, 0, 0, 0, b"<html>previous failed body</html>")
                    .await
                    .unwrap();
            }
            let (writer, writer_task) = storage::spawn(pool.clone(), root.clone(), 0);
            let result = fetch_tile_with_retries_to_db(
                0, 0, 0, &client, &writer, &bytes, None, None, 2, &base,
            )
            .await;
            assert_eq!(result == TileOutcome::Complete, recover);
            assert_eq!(bundle_has_tile(&pool, 0, 0, 0).await.unwrap(), recover);
            if recover {
                let saved: Vec<u8> = sqlx::query_scalar("SELECT b.image FROM tiles t JOIN tile_blobs b ON b.id=t.blob_id WHERE t.z=0 AND t.x=0 AND t.y=0")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(saved, image());
            }
            drop(writer);
            writer_task.await.unwrap();
            pool.close().await;
        } else {
            if corrupt_cached {
                fs::write(root.join("0/0/0.jpg"), b"<html>previous failed body</html>").unwrap();
            }
            let result = fetch_tile_with_retries(
                0,
                0,
                0,
                &root.join("0"),
                &client,
                &bytes,
                None,
                None,
                2,
                &base,
                false,
            )
            .await;
            assert_eq!(result == TileOutcome::Complete, recover);
            assert_eq!(root.join("0/0/0.jpg").exists(), recover);
            if recover {
                assert_eq!(fs::read(root.join("0/0/0.jpg")).unwrap(), image());
            }
        }
        assert_eq!(thread.join().unwrap(), 2);
        assert_eq!(
            bytes.load(Ordering::Relaxed),
            if recover { image().len() as u64 } else { 0 }
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn directory_retry_recovery_is_quiet() {
        check_retry(false, true).await;
    }
    #[tokio::test]
    async fn database_retry_recovery_is_quiet() {
        check_retry(true, true).await;
    }
    #[tokio::test]
    async fn directory_exhaustion_reports_final_cause_once() {
        check_retry(false, false).await;
    }
    #[tokio::test]
    async fn database_exhaustion_reports_final_cause_once() {
        check_retry(true, false).await;
    }
    #[tokio::test]
    async fn complete_http200_html_is_retried_and_never_saved_to_directory() {
        check_retry_case(false, false, true, false).await;
    }
    #[tokio::test]
    async fn complete_http200_html_is_retried_and_never_saved_to_database() {
        check_retry_case(true, false, true, false).await;
    }
    #[tokio::test]
    async fn invalid_http200_body_then_real_image_recovers_directory_quietly() {
        check_retry_case(false, true, true, false).await;
    }
    #[tokio::test]
    async fn invalid_http200_body_then_real_image_recovers_database_quietly() {
        check_retry_case(true, true, true, false).await;
    }
    #[tokio::test]
    async fn directory_resume_replaces_only_bad_cached_tile_after_valid_response() {
        check_retry_case(false, true, true, true).await;
    }
    #[tokio::test]
    async fn database_resume_replaces_only_bad_cached_tile_after_valid_response() {
        check_retry_case(true, true, true, true).await;
    }
}

#[cfg(test)]
mod map_pipeline_tests {
    use super::*;
    #[test]
    fn eta_handles_stalls_invalid_rates_and_counter_limits() {
        for (done, total, elapsed) in [
            (0, 100, 1.0),
            (1, u64::MAX, f64::MAX),
            (1, 100, f64::NAN),
            (1, 100, f64::INFINITY),
            (1, 100, -1.0),
        ] {
            assert_eq!(sane_eta(done, total, elapsed), "--");
        }
        assert_eq!(sane_eta(u64::MAX, 1, 1.0), "0s");
        assert_eq!(sane_eta(10, 100, 10.0), "0h01m30s");
    }
    #[tokio::test]
    async fn resume_scan_then_concurrent_writes_completes_with_one_pool_connection() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(100))
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
        upsert_tile_to_bundle(&pool, 17, 0, 0, &raster::image())
            .await
            .unwrap();
        upsert_tile_to_bundle(&pool, 16, 1, 0, b"other zoom")
            .await
            .unwrap();
        let mut pending: HashSet<_> = (0..257).map(|x| (x, 0)).collect();
        remove_saved_coords(&pool, 17, &mut pending).await.unwrap();
        assert_eq!(pending.len(), 256);
        assert!(!pending.contains(&(0, 0)));
        let (writer, task) = storage::spawn(pool.clone(), std::env::temp_dir(), 0);
        tokio::time::timeout(
            Duration::from_secs(5),
            futures::future::join_all(pending.into_iter().map(|(x, y)| {
                let writer = writer.clone();
                async move {
                    writer.save(17, x, y, b"new tile").await.unwrap();
                }
            })),
        )
        .await
        .unwrap();
        drop(writer);
        task.await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tiles WHERE z=17")
                .fetch_one(&pool)
                .await
                .unwrap(),
            257
        );
        let mut pending = HashSet::from([(999, 0)]);
        sqlx::query("DROP TABLE tiles")
            .execute(&pool)
            .await
            .unwrap();
        assert!(remove_saved_coords(&pool, 17, &mut pending).await.is_err());
        assert_eq!(
            pending,
            HashSet::from([(999, 0)]),
            "failed read must not be treated as a missing-tile scan"
        );
    }
    #[tokio::test]
    async fn legacy_low_space_refuses_before_touching_old_tile_or_partial_file() {
        let dir = std::env::temp_dir().join(format!("gs-legacy-reserve-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tile.jpg");
        fs::write(&path, b"old cached data").unwrap();
        assert!(
            write_tile_atomic_with_reserve(&path, &raster::image(), u64::MAX)
                .await
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"old cached data");
        assert!(!dir.join("tile.jpg.part").exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn oversized_tile_is_permanent_before_retry_budget_is_consumed() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                network::MAX_TILE_BYTES + 1
            )
            .unwrap();
        });
        let root = std::env::temp_dir().join(format!("gs-oversized-tile-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let client = TileClient::from(
            Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let bytes = Arc::new(AtomicU64::new(0));
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            fetch_tile_with_retries(
                0,
                0,
                0,
                &root,
                &client,
                &bytes,
                None,
                None,
                40,
                &format!("http://{addr}"),
                false,
            ),
        )
        .await
        .unwrap();
        assert_eq!(outcome, TileOutcome::Permanent);
        assert_eq!(bytes.load(Ordering::Relaxed), 0);
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn absent_imagery_is_unavailable_without_claiming_a_saved_tile() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
        });
        let root = std::env::temp_dir().join(format!("gs-unavailable-tile-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let client = TileClient::from(
            Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let bytes = Arc::new(AtomicU64::new(0));
        let url = format!("http://{addr}");
        assert_eq!(
            fetch_tile_with_retries(0, 0, 0, &root, &client, &bytes, None, None, 40, &url, false)
                .await,
            TileOutcome::Unavailable
        );
        assert!(!root.join("0/0.jpg").exists());
        let pool = init_direct_bundle_pool(&root.join("tiles.sqlite3"), true)
            .await
            .unwrap();
        let (writer, task) = storage::spawn(pool.clone(), root.clone(), 0);
        assert_eq!(
            fetch_tile_with_retries_to_db(0, 0, 0, &client, &writer, &bytes, None, None, 40, &url)
                .await,
            TileOutcome::Unavailable
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tiles")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        drop(writer);
        task.await.unwrap();
        pool.close().await;
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_pack_low_space_preserves_committed_bundle_and_input_tiles() {
        const CHILD: &str = "GS_MAP_PACK_RESERVE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","map_pipeline_tests::legacy_pack_low_space_preserves_committed_bundle_and_input_tiles","--test-threads=1"]).env(CHILD,"1").env("MAP_MIN_FREE_MIB","1000000000").status().unwrap();
            assert!(status.success());
            return;
        }
        let root = std::env::temp_dir().join(format!("gs-map-pack-reserve-{}", std::process::id()));
        fs::create_dir_all(root.join("tiles/0/0")).unwrap();
        let tile = root.join("tiles/0/0/0.jpg");
        fs::write(&tile, b"input tile").unwrap();
        let path = root.join("tiles.sqlite3");
        let pool = init_direct_bundle_pool(&path, true).await.unwrap();
        upsert_tile_to_bundle(&pool, 1, 0, 0, b"committed tile")
            .await
            .unwrap();
        pool.close().await;
        let error = build_tile_bundle_sqlite(&root.join("tiles"), &path)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disk space"));
        assert_eq!(fs::read(&tile).unwrap(), b"input tile");
        let pool = open_bundle_database(&path).await.unwrap();
        assert!(bundle_has_tile(&pool, 1, 0, 0).await.unwrap());
        assert!(!bundle_has_tile(&pool, 0, 0, 0).await.unwrap());
        pool.close().await;
        fs::remove_dir_all(root).unwrap();
    }
}
