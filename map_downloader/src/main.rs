use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Result;
use blake3::Hasher;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::Client;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
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

fn log_progress_error(pb: Option<&ProgressBar>, msg: String) {
    if let Some(pb) = pb {
        pb.println(msg);
    } else {
        eprintln!("{msg}");
    }
}

async fn fetch_tile_with_retries(
    z: u32,
    x: u32,
    y: u32,
    z_dir: &Path,
    client: &Client,
    bytes_downloaded: &Arc<AtomicU64>,
    pb: Option<&ProgressBar>,
    limiter: Option<&Arc<BandwidthLimiter>>,
    max_attempts: u32,
) -> bool {
    let tile_path = z_dir.join(format!("{x}/{y}.{TILE_EXT}"));
    let part_path = tile_path.with_extension(format!("{}.part", TILE_EXT));

    // Skip if final tile already exists
    if async_fs::try_exists(&tile_path).await.unwrap_or(false) {
        return true;
    }

    // Remove any leftover .part file
    if async_fs::try_exists(&part_path).await.unwrap_or(false) {
        let _ = async_fs::remove_file(&part_path).await;
    }

    let url = format!(
        "{base}/{z}/{y}/{x}",
        base = SATELLITE_BASE_URL,
        z = z,
        y = y,
        x = x,
    );

    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let mut should_retry = true;
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.bytes().await {
                        Ok(bytes) => {
                            if let Some(lim) = limiter {
                                lim.throttle_bytes(bytes.len()).await;
                            }
                            bytes_downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                            if write_tile_atomic_async(&tile_path, &bytes).await.is_ok() {
                                return true;
                            }
                        }
                        Err(e) => {
                            log_progress_error(
                                pb,
                                format!(
                                    "fetch_satellite_tiles_async: failed reading bytes for {} (attempt {attempts}/{max_attempts}): {e}",
                                    url
                                ),
                            );
                        }
                    }
                } else if status.as_u16() == 404 {
                    // Fine: no tile for this location (e.g. ocean).
                    return true;
                } else if status.as_u16() == 429 || status.is_server_error() {
                    should_retry = true;
                } else if status.is_client_error() {
                    // Non-retryable client errors (except 404 handled above).
                    log_progress_error(
                        pb,
                        format!(
                            "fetch_satellite_tiles_async: skipping non-retryable HTTP {} for tile z={z}, x={x}, y={y}",
                            status.as_u16()
                        ),
                    );
                    return true;
                }
            }
            Err(_e) => {
                should_retry = true;
            }
        }

        if attempts >= max_attempts {
            log_progress_error(
                pb,
                format!(
                    "fetch_satellite_tiles_async: giving up on tile z={z}, x={x}, y={y} after {} attempts",
                    attempts
                ),
            );
            return false;
        }
        if should_retry {
            let delay_ms = retry_backoff_ms(attempts, z, x, y);
            sleep(Duration::from_millis(delay_ms)).await;
        }
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

async fn build_tile_bundle_sqlite(tiles_root: &Path, bundle_path: &Path) -> Result<()> {
    let parent = bundle_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid bundle path: {}", bundle_path.display()))?;
    fs::create_dir_all(parent)?;

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

    let url = format!("sqlite://{}", working_path.to_string_lossy());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    sqlx::query("PRAGMA journal_mode = OFF;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA synchronous = OFF;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA locking_mode = EXCLUSIVE;")
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
    let parent = bundle_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid bundle path: {}", bundle_path.display()))?;
    fs::create_dir_all(parent)?;
    if !resume && bundle_path.exists() {
        fs::remove_file(bundle_path)?;
    }

    let url = format!("sqlite://{}", bundle_path.to_string_lossy());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;

    sqlx::query("PRAGMA journal_mode = OFF;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA synchronous = OFF;")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA locking_mode = EXCLUSIVE;")
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

async fn finalize_direct_bundle_pool(pool: &SqlitePool, run_vacuum: bool) -> Result<()> {
    sqlx::query("ANALYZE;").execute(pool).await?;
    sqlx::query("PRAGMA optimize;").execute(pool).await?;
    if run_vacuum {
        sqlx::query("VACUUM;").execute(pool).await?;
    }
    Ok(())
}

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
    let client = Client::builder()
        .user_agent("GroundStationOfflineTileFetcher/0.1")
        .build()?;

    if direct_to_bundle {
        if !should_build_bundle() {
            eprintln!(
                "WARNING: MAP_DIRECT_TO_BUNDLE requested but MAP_BUILD_BUNDLE is disabled; nothing to do."
            );
        } else {
            let bundle_path = bundle_path_for(&data_dir);
            let resume = should_resume_bundle();
            let existing_bundle = bundle_path.exists();
            let pool = init_direct_bundle_pool(&bundle_path, resume).await?;
            for z in MIN_ZOOM..=max_zoom {
                if z != MIN_ZOOM {
                    println!();
                    println!("----");
                }
                if fetch_tiles_for_zoom_async_to_db(z, &pool, &client)
                    .await
                    .is_err()
                {}
            }
            if let Err(e) = finalize_direct_bundle_pool(&pool, !resume || !existing_bundle).await {
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
            if fetch_tiles_for_zoom_async(z, &tiles_root, &client)
                .await
                .is_err()
            {}
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
    client: &Client,
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
            ProgressStyle::with_template(
                &format!(
                    "z={z} [{{bar:40.cyan/blue}}] {{percent:>3}}% ETA {{eta_precise}} {{msg}} ({{pos}}/{{len}})"
                ),
            )?
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
            let pct = if total == 0 {
                100
            } else {
                (done.saturating_mul(100)) / total
            };
            let bucket = (pct / PROGRESS_PERCENT_STEP) as i64;

            if let Some(pb) = &pb_for_rate {
                pb.set_position(done);
                pb.set_message(format!("{mib_per_s:.2} MiB/s"));
            } else if bucket != last_bucket || done == total {
                let eta_secs = if done == 0 {
                    0
                } else {
                    (((total - done) as f64) / ((done as f64) / elapsed_s)).max(0.0) as u64
                };
                let eta_m = eta_secs / 60;
                let eta_s = eta_secs % 60;
                println!(
                    "z={z} {pct}% ETA {eta_m}m{eta_s:02}s {mib_per_s:.2} MiB/s ({done}/{total})"
                );
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
                )
                .await;
                if !ok {
                    failed_coords.lock().await.push((x, y));
                }
                done_count.fetch_add(1, Ordering::Relaxed);
            }
        })
        .await;

    let retry_coords = {
        let mut failed = failed_coords.lock().await;
        std::mem::take(&mut *failed)
    };
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
                    )
                    .await;
                    if !ok {
                        retry_failures.lock().await.push((x, y));
                    }
                }
            })
            .await;

        let final_failed_count = retry_failures.lock().await.len();
        if final_failed_count > 0 {
            log_progress_error(
                pb.as_ref(),
                format!("z={z}: {final_failed_count} tiles still failed after second-pass retry"),
            );
        }
    }

    done_count.store(total, Ordering::Relaxed);
    stop_rate_updater.store(true, Ordering::Relaxed);
    let _ = rate_updater.await;
    let elapsed_s = start.elapsed().as_secs_f64().max(0.001);
    let bytes = bytes_downloaded.load(Ordering::Relaxed) as f64;
    let avg_mib_per_s = (bytes / elapsed_s) / (1024.0 * 1024.0);
    if let Some(pb) = pb {
        pb.finish_and_clear();
        println!(
            "z={z} done: {total} tiles in {:.1}s at {:.2} MiB/s",
            elapsed_s, avg_mib_per_s
        );
    }
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

async fn fetch_tile_with_retries_to_db(
    z: u32,
    x: u32,
    y: u32,
    client: &Client,
    pool: &SqlitePool,
    bytes_downloaded: &Arc<AtomicU64>,
    pb: Option<&ProgressBar>,
    limiter: Option<&Arc<BandwidthLimiter>>,
    max_attempts: u32,
) -> bool {
    match bundle_has_tile(pool, z, x, y).await {
        Ok(true) => return true,
        Ok(false) => {}
        Err(e) => {
            log_progress_error(
                pb,
                format!(
                    "fetch_satellite_tiles_async: bundle precheck failed for tile z={z}, x={x}, y={y}: {e}"
                ),
            );
        }
    }

    let url = format!(
        "{base}/{z}/{y}/{x}",
        base = SATELLITE_BASE_URL,
        z = z,
        y = y,
        x = x,
    );

    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let mut should_retry = true;
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.bytes().await {
                        Ok(bytes) => {
                            if let Some(lim) = limiter {
                                lim.throttle_bytes(bytes.len()).await;
                            }
                            bytes_downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                            if upsert_tile_to_bundle(pool, z, x, y, &bytes).await.is_ok() {
                                return true;
                            }
                        }
                        Err(e) => {
                            log_progress_error(
                                pb,
                                format!(
                                    "fetch_satellite_tiles_async: failed reading bytes for {} (attempt {attempts}/{max_attempts}): {e}",
                                    url
                                ),
                            );
                        }
                    }
                } else if status.as_u16() == 404 {
                    return true;
                } else if status.as_u16() == 429 || status.is_server_error() {
                    should_retry = true;
                } else if status.is_client_error() {
                    log_progress_error(
                        pb,
                        format!(
                            "fetch_satellite_tiles_async: skipping non-retryable HTTP {} for tile z={z}, x={x}, y={y}",
                            status.as_u16()
                        ),
                    );
                    return true;
                }
            }
            Err(_e) => {
                should_retry = true;
            }
        }

        if attempts >= max_attempts {
            log_progress_error(
                pb,
                format!(
                    "fetch_satellite_tiles_async: giving up on tile z={z}, x={x}, y={y} after {} attempts",
                    attempts
                ),
            );
            return false;
        }
        if should_retry {
            let delay_ms = retry_backoff_ms(attempts, z, x, y);
            sleep(Duration::from_millis(delay_ms)).await;
        }
    }
}

async fn fetch_tiles_for_zoom_async_to_db(
    z: u32,
    pool: &SqlitePool,
    client: &Client,
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
    let mut coords: Vec<(u32, u32)> = coord_set.into_iter().collect();
    coords.sort_unstable();
    let (x_start, x_end, y_start, y_end) = bounds_tile_extent(&coords);
    let total = coords.len() as u64;
    println!(
        "z={z}: fetching satellite tiles directly to bundle x=[{}..={}], y=[{}..={}], total={} tiles",
        x_start, x_end, y_start, y_end, total
    );
    let is_tty = std::io::stdout().is_terminal();

    let start = std::time::Instant::now();
    let done_count = Arc::new(AtomicU64::new(0));
    let bytes_downloaded = Arc::new(AtomicU64::new(0));
    let stop_rate_updater = Arc::new(AtomicBool::new(false));
    let pb = if is_tty {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(
                &format!(
                    "z={z} [{{bar:40.green/blue}}] {{percent:>3}}% ETA {{eta_precise}} {{msg}} ({{pos}}/{{len}})"
                ),
            )?
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
            let pct = if total == 0 {
                100
            } else {
                (done.saturating_mul(100)) / total
            };
            let bucket = (pct / PROGRESS_PERCENT_STEP) as i64;
            if let Some(pb) = &pb_for_rate {
                pb.set_position(done);
                pb.set_message(format!("{mib_per_s:.2} MiB/s"));
            } else if bucket != last_bucket || done == total {
                let eta_secs = if done == 0 {
                    0
                } else {
                    (((total - done) as f64) / ((done as f64) / elapsed_s)).max(0.0) as u64
                };
                let eta_m = eta_secs / 60;
                let eta_s = eta_secs % 60;
                println!(
                    "z={z} {pct}% ETA {eta_m}m{eta_s:02}s {mib_per_s:.2} MiB/s ({done}/{total})"
                );
                last_bucket = bucket;
            }
            sleep(Duration::from_millis(400)).await;
        }
    });

    let client_arc = client.clone();
    let pool_arc = pool.clone();
    let done_count_clone = done_count.clone();
    let bytes_downloaded_clone = bytes_downloaded.clone();
    let pb_for_workers = pb.clone();
    let limiter_for_workers = bandwidth_limiter.clone();
    let failed_coords = Arc::new(AsyncMutex::new(Vec::<(u32, u32)>::new()));
    let failed_coords_for_workers = failed_coords.clone();
    stream::iter(coords)
        .for_each_concurrent(max_concurrent, move |(x, y)| {
            let client = client_arc.clone();
            let pool = pool_arc.clone();
            let done_count = done_count_clone.clone();
            let bytes_downloaded = bytes_downloaded_clone.clone();
            let pb_for_worker = pb_for_workers.clone();
            let limiter_for_worker = limiter_for_workers.clone();
            let failed_coords = failed_coords_for_workers.clone();
            async move {
                let ok = fetch_tile_with_retries_to_db(
                    z,
                    x,
                    y,
                    &client,
                    &pool,
                    &bytes_downloaded,
                    pb_for_worker.as_ref(),
                    limiter_for_worker.as_ref(),
                    MAX_RETRY_ATTEMPTS,
                )
                .await;
                if !ok {
                    failed_coords.lock().await.push((x, y));
                }
                done_count.fetch_add(1, Ordering::Relaxed);
            }
        })
        .await;

    let retry_coords = {
        let mut failed = failed_coords.lock().await;
        std::mem::take(&mut *failed)
    };
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
        let client_retry = client.clone();
        let pool_retry = pool.clone();
        let bytes_retry = bytes_downloaded.clone();
        let pb_retry = pb.clone();
        let limiter_retry = bandwidth_limiter.clone();
        stream::iter(retry_coords)
            .for_each_concurrent(max_concurrent, move |(x, y)| {
                let client = client_retry.clone();
                let pool = pool_retry.clone();
                let bytes_downloaded = bytes_retry.clone();
                let pb = pb_retry.clone();
                let limiter = limiter_retry.clone();
                let retry_failures = retry_failures_workers.clone();
                async move {
                    let ok = fetch_tile_with_retries_to_db(
                        z,
                        x,
                        y,
                        &client,
                        &pool,
                        &bytes_downloaded,
                        pb.as_ref(),
                        limiter.as_ref(),
                        MAX_RETRY_ATTEMPTS,
                    )
                    .await;
                    if !ok {
                        retry_failures.lock().await.push((x, y));
                    }
                }
            })
            .await;
        let final_failed_count = retry_failures.lock().await.len();
        if final_failed_count > 0 {
            log_progress_error(
                pb.as_ref(),
                format!("z={z}: {final_failed_count} tiles still failed after second-pass retry"),
            );
        }
    }

    done_count.store(total, Ordering::Relaxed);
    stop_rate_updater.store(true, Ordering::Relaxed);
    let _ = rate_updater.await;
    let elapsed_s = start.elapsed().as_secs_f64().max(0.001);
    let bytes = bytes_downloaded.load(Ordering::Relaxed) as f64;
    let avg_mib_per_s = (bytes / elapsed_s) / (1024.0 * 1024.0);
    if let Some(pb) = pb {
        pb.finish_and_clear();
        println!(
            "z={z} done: {total} tiles in {:.1}s at {:.2} MiB/s",
            elapsed_s, avg_mib_per_s
        );
    }
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
