use std::f64::consts::PI;
use std::{env, fs, io::Write, path::Path, path::PathBuf};

use rayon::prelude::*;
use reqwest::blocking::Client;

/// Region name (used for directory layout)
const REGION: &str = "north_america";

/// GIBS layer + WMTS config.
/// This uses a non-time-varying, global satellite-ish basemap.
const GIBS_LAYER: &str = "BlueMarble_ShadedRelief";
const GIBS_TILE_MATRIX_SET: &str = "GoogleMapsCompatible_Level8";
const GIBS_BASE_URL: &str = "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best";

/// File extension for tiles from this layer.
const TILE_EXT: &str = "jpeg";

/// Zoom levels we want to cache.
/// Be careful: tiles grow as 4^z.
/// 0..6 is a reasonable compromise for a regional basemap.
const MIN_ZOOM: u32 = 0;
const MAX_ZOOM: u32 = 8;

/// Approximate North America bounds in lon/lat (WGS84)
/// lon_min, lat_min, lon_max, lat_max
const NA_BOUNDS: (f64, f64, f64, f64) = (-170.0, 5.0, -50.0, 83.0);

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MAP_REGION");

    // Optional override, but default to north_america
    let region = env::var("MAP_REGION").unwrap_or_else(|_| REGION.to_string());

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // ./data/maps/<region>/tiles
    let data_dir = manifest_dir.join("data").join("maps").join(&region);
    let tiles_root = data_dir.join("tiles");

    // If any tiles already exist, skip download
    if tiles_exist(&tiles_root) {
        println!(
            "build.rs: tiles already present in {}, skipping GIBS download",
            tiles_root.display()
        );
        return;
    }

    if let Err(e) = fs::create_dir_all(&tiles_root) {
        eprintln!(
            "build.rs: failed to create tiles dir {}: {e}",
            tiles_root.display()
        );
        return;
    }

    // Configure rayon pool with limited parallelism to avoid hammering GIBS
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(8) // tune as desired
        .build_global();

    // Single reusable client (connection pooling, keep-alive)
    let client = match Client::builder()
        .user_agent("GroundStationOfflineTileFetcher/0.1")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("build.rs: failed to create reqwest client: {e}");
            return;
        }
    };

    println!(
        "build.rs: populating GIBS tiles for region '{}' into {} (z={MIN_ZOOM}..={MAX_ZOOM})",
        region,
        tiles_root.display()
    );

    for z in MIN_ZOOM..=MAX_ZOOM {
        if let Err(e) = fetch_tiles_for_zoom(z, &tiles_root, &client) {
            eprintln!("build.rs: WARNING: failed to fetch tiles for z={z}: {e}");
        }
    }
}

/// Check whether tiles directory is non-empty.
fn tiles_exist(tiles_root: &Path) -> bool {
    if !tiles_root.exists() {
        return false;
    }
    match fs::read_dir(tiles_root) {
        Ok(mut it) => it.next().is_some(),
        Err(_) => false,
    }
}

/// Download tiles for North America at zoom level `z`, in parallel.
fn fetch_tiles_for_zoom(
    z: u32,
    tiles_root: &Path,
    client: &Client,
) -> Result<(), Box<dyn std::error::Error>> {
    let (lon_min, lat_min, lon_max, lat_max) = NA_BOUNDS;

    // Convert bounding box to tile index ranges at this zoom level
    let (x_min, y_max) = lonlat_to_tile(lon_min, lat_min, z);
    let (x_max, y_min) = lonlat_to_tile(lon_max, lat_max, z);

    let x_start = x_min.min(x_max);
    let x_end = x_min.max(x_max);
    let y_start = y_min.min(y_max);
    let y_end = y_min.max(y_max);

    println!(
        "build.rs: zoom {z}: fetching tiles x=[{}..={}], y=[{}..={}]",
        x_start, x_end, y_start, y_end
    );

    // Enumerate all tile coordinates we want at this zoom
    let mut coords = Vec::new();
    for x in x_start..=x_end {
        for y in y_start..=y_end {
            coords.push((x, y));
        }
    }

    // Create the base z directory once
    let z_dir = tiles_root.join(format!("{z}"));
    fs::create_dir_all(&z_dir)?;

    // Parallel download of (x, y) tiles with rayon
    coords.par_iter().for_each(|&(x, y)| {
        let x_dir = z_dir.join(format!("{x}"));
        if let Err(e) = fs::create_dir_all(&x_dir) {
            eprintln!(
                "build.rs: failed to create directory {}: {e}",
                x_dir.display()
            );
            return;
        }

        let tile_path = x_dir.join(format!("{y}.{TILE_EXT}"));
        if tile_path.exists() {
            // Already cached; skip
            return;
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

        // Don’t spam stdout for every tile; only log errors
        match client.get(&url).send() {
            Ok(resp) => {
                if !resp.status().is_success() {
                    eprintln!(
                        "build.rs: HTTP {} for tile z={z}, x={x}, y={y}",
                        resp.status()
                    );
                    return;
                }
                match resp.bytes() {
                    Ok(bytes) => {
                        if let Err(e) = write_tile(&tile_path, &bytes) {
                            eprintln!(
                                "build.rs: failed to write tile {}: {e}",
                                tile_path.display()
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "build.rs: failed to read body for tile z={z}, x={x}, y={y}: {e}"
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "build.rs: ERROR fetching tile z={z}, x={x}, y={y} from {url}: {e}"
                );
            }
        }
    });

    Ok(())
}

fn write_tile(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// Convert lon/lat (deg) to XYZ tile indices for Web Mercator at zoom `z`.
fn lonlat_to_tile(lon_deg: f64, lat_deg: f64, zoom: u32) -> (u32, u32) {
    let lat_rad = lat_deg.to_radians();
    let n = 2f64.powi(zoom as i32);

    let x = ((lon_deg + 180.0) / 360.0 * n).floor();
    let y = (
        1.0 - ((lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI)
    ) / 2.0 * n;

    let max_idx = n - 1.0;
    let x = x.max(0.0).min(max_idx) as u32;
    let y = y.max(0.0).min(max_idx) as u32;

    (x, y)
}
