use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Result;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::Client;
use tokio::fs as async_fs;
use tokio::time::{sleep, Duration};

/// Region name (used for directory layout)
const REGION: &str = "north_america";

/// GIBS layer + WMTS config.
const GIBS_LAYER: &str = "ASTER_GDEM_Color_Shaded_Relief";
const GIBS_TILE_MATRIX_SET: &str = "GoogleMapsCompatible_Level12";
const GIBS_BASE_URL: &str = "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best";

/// File extension for tiles from this layer.
const TILE_EXT: &str = "jpg";

/// Zoom levels we want to cache.
const MIN_ZOOM: u32 = 0;
const MAX_ZOOM: u32 = 12;

/// Approximate North America bounds in lon/lat (WGS84)
/// lon_min, lat_min, lon_max, lat_max
const NA_BOUNDS: (f64, f64, f64, f64) = (-170.0, 5.0, -50.0, 83.0);

/// Max concurrent HTTP fetches at a time.
/// Tune this: higher = faster but more load on GIBS / your network.
const MAX_CONCURRENT: usize = 256;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Optional override for region, but default to north_america
    let region = env::var("MAP_REGION").unwrap_or_else(|_| REGION.to_string());

    // Use CARGO_MANIFEST_DIR if present (when run via `cargo run`),
    // otherwise fall back to current directory.
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().expect("failed to get current dir"));

    // ./data/maps/<region>/tiles (adjusted to your ../backend/data)
    let data_dir = manifest_dir
        .join("../backend/data")
        .join("maps")
        .join(&region);
    let tiles_root = data_dir.join("tiles");

    fs::create_dir_all(&tiles_root)?;
    println!(
        "fetch_gibs_tiles_async: populating GIBS tiles for region '{}' into {} (z={MIN_ZOOM}..={MAX_ZOOM})",
        region,
        tiles_root.display()
    );

    // Async HTTP client
    let client = Client::builder()
        .user_agent("GroundStationOfflineTileFetcher/0.1")
        .build()?;

    for z in MIN_ZOOM..=MAX_ZOOM {
        if let Err(e) = fetch_tiles_for_zoom_async(z, &tiles_root, &client).await {
            eprintln!(
                "fetch_gibs_tiles_async: WARNING: failed to fetch tiles for z={z}: {e}"
            );
        }
    }

    println!("fetch_gibs_tiles_async: done populating GIBS tiles.");
    Ok(())
}

/// Download tiles for North America at zoom level `z`, in parallel with Tokio.
async fn fetch_tiles_for_zoom_async(
    z: u32,
    tiles_root: &Path,
    client: &Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (lon_min, lat_min, lon_max, lat_max) = NA_BOUNDS;

    // Convert bounding box to tile index ranges at this zoom level
    let (x_min, y_max) = lonlat_to_tile(lon_min, lat_min, z);
    let (x_max, y_min) = lonlat_to_tile(lon_max, lat_max, z);

    let x_start = x_min.min(x_max);
    let x_end = x_min.max(x_max);
    let y_start = y_min.min(y_max);
    let y_end = y_min.max(y_max);

    // Enumerate all tile coordinates we want at this zoom
    let mut coords = Vec::new();
    for x in x_start..=x_end {
        for y in y_start..=y_end {
            coords.push((x, y));
        }
    }

    let total = coords.len() as u64;
    println!(
        "z={z}: fetching tiles x=[{}..={}], y=[{}..={}], total={} tiles",
        x_start, x_end, y_start, y_end, total
    );

    // Create the base z directory once (sync is fine here)
    let z_dir = tiles_root.join(format!("{z}"));
    fs::create_dir_all(&z_dir)?;

    // Pre-create all x directories once (avoid per-tile mkdir)
    for x in x_start..=x_end {
        let x_dir = z_dir.join(format!("{x}"));
        if let Err(e) = fs::create_dir_all(&x_dir) {
            eprintln!(
                "fetch_gibs_tiles_async: failed to create directory {}: {e}",
                x_dir.display()
            );
        }
    }

    // Progress bar for this zoom level
    let pb = ProgressBar::new(total);
    pb.set_prefix(format!("z={z}"));
    pb.set_style(
        ProgressStyle::with_template(
            "{prefix} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) ETA {eta}",
        )?
            .progress_chars("##-"),
    );
    pb.set_draw_target(ProgressDrawTarget::stdout_with_hz(10));

    let z_dir_arc = z_dir.clone();
    let client_arc = client.clone(); // cheap clone
    let pb_clone = pb.clone();
    // Build an async stream of all coordinate tasks
    stream::iter(coords)
        .for_each_concurrent(MAX_CONCURRENT, move |(x, y)| {
            let z_dir = z_dir_arc.clone();
            let client = client_arc.clone();
            let pb = pb_clone.clone();

            async move {
                let tile_path = z_dir.join(format!("{x}/{y}.{TILE_EXT}"));
                let part_path = tile_path.with_extension(format!("{}.part", TILE_EXT));

                // Skip if final tile already exists
                if async_fs::try_exists(&tile_path).await.unwrap_or(false) {
                    pb.inc(1);
                    return;
                }

                // Remove any leftover .part file
                if async_fs::try_exists(&part_path).await.unwrap_or(false) {
                    let _ = async_fs::remove_file(&part_path).await;
                }

                let url = format!(
                    "{base}/{layer}/default/{matrix_set}/{z}/{y}/{x}.{ext}",
                    base = GIBS_BASE_URL,
                    layer = GIBS_LAYER,
                    matrix_set = GIBS_TILE_MATRIX_SET,
                    z = z,
                    y = y,
                    x = x,
                    ext = TILE_EXT,
                );

                let mut attempts: u32 = 0;
                const MAX_ATTEMPTS: u32 = 3;

                loop {
                    attempts += 1;

                    match client.get(&url).send().await {
                        Ok(resp) => {
                            let status = resp.status();

                            if status.is_success() {
                                match resp.bytes().await {
                                    Ok(bytes) => {
                                        if let Err(e) =
                                            write_tile_atomic_async(&tile_path, &bytes).await
                                        {
                                            eprintln!(
                                                "fetch_gibs_tiles_async: failed to write tile {}: {e}",
                                                tile_path.display()
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "fetch_gibs_tiles_async: failed reading bytes for {}: {e}",
                                            url
                                        );
                                    }
                                }
                                break; // success
                            }

                            // 404: tile doesn't exist, never retry
                            if status.as_u16() == 404 {
                                // fine: no tile for this location (e.g. ocean)
                                break;
                            }
                        }

                        Err(_e) => {}
                    }

                    if attempts >= MAX_ATTEMPTS {
                        eprintln!(
                            "fetch_gibs_tiles_async: giving up on tile z={z}, x={x}, y={y} after {} attempts",
                            attempts
                        );
                        break;
                    }

                    // Backoff — can tweak lower/higher depending on how aggressive you want to be
                    sleep(Duration::from_millis(5)).await;
                }

                pb.inc(1);
            }
        })
        .await;

    pb.finish_with_message(format!("z={z} done"));
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
