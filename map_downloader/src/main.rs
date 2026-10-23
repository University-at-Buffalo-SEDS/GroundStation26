mod dns;
mod limits;
mod network;
mod storage;

use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{Context, Result};
use blake3::Hasher;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use network::TileClient;
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
) -> TileOutcome {
    let tile_path = z_dir.join(format!("{x}/{y}.{TILE_EXT}"));
    let part_path = tile_path.with_extension(format!("{}.part", TILE_EXT));

    // Skip if final tile already exists
    if async_fs::try_exists(&tile_path).await.unwrap_or(false) {
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
        let failure = match client.send(&url).await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match network::tile_body(resp).await {
                        Ok(bytes) => {
                            if let Some(lim) = limiter {
                                lim.throttle_bytes(bytes.len()).await;
                            }
                            bytes_downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                            match write_tile_atomic_async(&tile_path, &bytes).await {
                                Ok(()) => return TileOutcome::Complete,
                                Err(error) => format!("saving tile: {error:#}"),
                            }
                        }
                        Err(error) => {
                            if error.is::<network::OversizedTile>() {
                                log_progress_error(
                                    pb,
                                    format!("tile z={z},x={x},y={y} exceeds512KiB; not retried"),
                                );
                                return TileOutcome::Permanent;
                            }
                            client.direct_failed();
                            format!("reading response body: {error:#}")
                        }
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
            Err(error) => format!("request: {:#}", anyhow::Error::new(error)),
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
        Err(_) => true,
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
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create map bundle directory {}", parent.display()))?;
    SqlitePoolOptions::new()
        .max_connections(1)
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
                let bytes = fs::read(&path)?;
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
                if pending_writes >= commit_every {
                    tx.commit().await?;
                    tx = pool.begin().await?;
                    pending_writes = 0;
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
    if !resume || !existing_bundle {
        sqlx::query("VACUUM;").execute(&pool).await?;
    } else {
        println!("bundle resume mode: skipping VACUUM for faster incremental updates");
    }
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

async fn init_direct_bundle_pool(bundle_path: &Path, resume: bool) -> Result<SqlitePool> {
    if !resume && bundle_path.exists() {
        fs::remove_file(bundle_path)?;
    }

    let pool = open_bundle_database(bundle_path).await?;

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
    Ok(pool)
}

async fn finalize_direct_bundle_pool(pool: &SqlitePool) -> Result<()> {
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
    let mut rows = sqlx::query("SELECT x,y FROM tiles WHERE z=?")
        .bind(i64::from(z))
        .fetch(pool);
    while let Some(row) = rows
        .try_next()
        .await
        .context("resume scan failed; saved tiles retained")?
    {
        use sqlx::Row;
        let x: i64 = row.try_get("x")?;
        let y: i64 = row.try_get("y")?;
        if let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) {
            pending.remove(&(x, y));
        }
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
    limits::prepare(max_concurrent())?;
    // Optional override for region, but default to north_america
    let region = env::var("MAP_REGION").unwrap_or_else(|_| REGION.to_string());
    let max_zoom = env::var("MAP_MAX_ZOOM")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_ZOOM);

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

    if direct_to_bundle {
        if !should_build_bundle() {
            eprintln!(
                "WARNING: MAP_DIRECT_TO_BUNDLE requested but MAP_BUILD_BUNDLE is disabled; nothing to do."
            );
        } else {
            let bundle_path = bundle_path_for(&data_dir);
            let resume = should_resume_bundle();
            let pool = init_direct_bundle_pool(&bundle_path, resume).await?;
            for z in MIN_ZOOM..=max_zoom {
                if z != MIN_ZOOM {
                    println!();
                    println!("----");
                }
                fetch_tiles_for_zoom_async_to_db(z, &pool, &client)
                    .await
                    .map_err(|error| anyhow::anyhow!("map zoom {z} failed: {error}"))?;
            }
            if let Err(e) = finalize_direct_bundle_pool(&pool).await {
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
            fetch_tiles_for_zoom_async(z, &tiles_root, &client)
                .await
                .map_err(|error| anyhow::anyhow!("map zoom {z} failed: {error}"))?;
        }

        if should_build_bundle() {
            let bundle_path = bundle_path_for(&data_dir);
            if let Err(e) = build_tile_bundle_sqlite(&tiles_root, &bundle_path).await {
                eprintln!(
                    "WARNING: failed building tile bundle {}: {e:#}",
                    bundle_path.display()
                );
            } else if !should_keep_tiles() {
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
/// - z=9..=MAX: Buffalo/Rochester + West Texas desert bounds
async fn fetch_tiles_for_zoom_async(
    z: u32,
    tiles_root: &Path,
    client: &TileClient,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bounds = if z <= BASE_COVERAGE_MAX_ZOOM {
        vec![NA_BOUNDS]
    } else {
        vec![BUFFALO_ROCHESTER_BOUNDS, TEXAS_DESERT_BOUNDS]
    };

    // Enumerate + de-duplicate all tile coordinates across selected bounds.
    let mut coord_set = HashSet::<(u32, u32)>::new();
    for bbox in &bounds {
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
        let failure = match client.send(&url).await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match network::tile_body(resp).await {
                        Ok(bytes) => {
                            if let Some(lim) = limiter {
                                lim.throttle_bytes(bytes.len()).await;
                            }
                            bytes_downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                            match writer.save(z, x, y, &bytes).await {
                                Ok(()) => return TileOutcome::Complete,
                                Err(_) => return TileOutcome::Exhausted,
                            }
                        }
                        Err(error) => {
                            if error.is::<network::OversizedTile>() {
                                log_progress_error(
                                    pb,
                                    format!("tile z={z},x={x},y={y} exceeds512KiB; not retried"),
                                );
                                return TileOutcome::Permanent;
                            }
                            client.direct_failed();
                            format!("reading response body: {error:#}")
                        }
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
            Err(error) => format!("request: {:#}", anyhow::Error::new(error)),
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bounds = if z <= BASE_COVERAGE_MAX_ZOOM {
        vec![NA_BOUNDS]
    } else {
        vec![BUFFALO_ROCHESTER_BOUNDS, TEXAS_DESERT_BOUNDS]
    };

    let mut coord_set = HashSet::<(u32, u32)>::new();
    for bbox in &bounds {
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

    let (writer, writer_task) = storage::spawn(pool.clone(), directory, reserve);
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

    const IMAGE: &[u8] = b"\xff\xd8\xfftile-data";
    fn server(failures: usize, success: bool) -> (String, std::thread::JoinHandle<usize>) {
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
                        let length = IMAGE.len() + if count < failures { 100 } else { 0 };
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").unwrap();
                        socket.write_all(IMAGE).unwrap();
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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::temp_dir().join(format!(
            "gs-tile-retry-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("0/0")).unwrap();
        let failures = if recover { 1 } else { 2 };
        let (base, thread) = server(failures, recover);
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
            let (writer, writer_task) = storage::spawn(pool.clone(), root.clone(), 0);
            let result = fetch_tile_with_retries_to_db(
                0, 0, 0, &client, &writer, &bytes, None, None, 2, &base,
            )
            .await;
            assert_eq!(result == TileOutcome::Complete, recover);
            assert_eq!(bundle_has_tile(&pool, 0, 0, 0).await.unwrap(), recover);
            if recover {
                let saved: Vec<u8> = sqlx::query_scalar("SELECT image FROM tile_blobs")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(saved, IMAGE);
            }
            drop(writer);
            writer_task.await.unwrap();
            pool.close().await;
        } else {
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
            )
            .await;
            assert_eq!(result == TileOutcome::Complete, recover);
            assert_eq!(root.join("0/0/0.jpg").exists(), recover);
            if recover {
                assert_eq!(fs::read(root.join("0/0/0.jpg")).unwrap(), IMAGE);
            }
        }
        assert_eq!(thread.join().unwrap(), 2);
        assert_eq!(
            bytes.load(Ordering::Relaxed),
            if recover { IMAGE.len() as u64 } else { 0 }
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
        upsert_tile_to_bundle(&pool, 17, 0, 0, b"existing")
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
            fetch_tile_with_retries(0, 0, 0, &root, &client, &bytes, None, None, 40, &url).await,
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
}
