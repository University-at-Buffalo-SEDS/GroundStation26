//! Offline terrain (DEM) for the 3D map and the 3D replay.
//!
//! The elevation source is the open, keyless AWS Terrain Tiles dataset in
//! Terrarium PNG encoding (`height_m = R*256 + G + B/256 - 32768`). The map
//! downloader prefetches it for the configured regions into
//! `backend/data/maps/<region>/terrain.sqlite3`, with the same schema as the
//! imagery bundle (`tiles.sqlite3`), so the station serves it offline.
//! Like the imagery tiles, the station never fetches terrain from the network
//! while serving: a tile that was not prefetched is simply absent.
//!
//! Routes (docs/backend/terrain.md and the UI's docs/backend-contract.md):
//! - `GET /terrain/{z}/{x}/{y}.png`: a raw cached Terrarium tile, 404 when absent;
//! - `GET /api/terrain/heightmap?lat=&lon=&extent_m=&size=`: a decoded square
//!   grid around a point, `{"available": false, …}` when no DEM covers it;
//! - `GET /api/terrain/status?lat=&lon=&radius_m=`: what is cached (DEM and
//!   imagery) and how much of the area around a point it covers.
use crate::{auth::Permission, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Where the downloader fetches the DEM from (never contacted by the station).
pub const DEM_SOURCE_URL: &str = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";
pub const DEM_SOURCE_NAME: &str = "AWS Terrain Tiles (Terrarium)";
pub const DEM_ATTRIBUTION: &str =
    "Terrain: Mapzen/Tilezen Terrain Tiles via AWS Open Data (SRTM, USGS 3DEP, GMTED2010, ETOPO1 and others)";
pub const DEM_LICENSE: &str = "Open data, attribution required: https://github.com/tilezen/joerd/blob/master/docs/attribution.md";
/// The imagery the map tab and the 3D view drape (the downloader's source).
pub const IMAGERY_SOURCE_NAME: &str = "Esri World Imagery";
pub const IMAGERY_ATTRIBUTION: &str = "Imagery: Esri, Maxar, Earthstar Geographics and the GIS User Community";
/// Terrarium's highest native zoom.
pub const DEM_MAX_ZOOM: u32 = 15;
/// Grid limits for `/api/terrain/heightmap`.
pub const MAX_GRID_SIZE: u32 = 257;
pub const DEFAULT_GRID_SIZE: u32 = 129;
pub const MIN_EXTENT_M: f64 = 200.0;
pub const MAX_EXTENT_M: f64 = 60_000.0;
pub const DEFAULT_EXTENT_M: f64 = 12_000.0;
/// At most this many distinct source tiles are decoded for one grid.
const MAX_GRID_TILES: usize = 64;
/// A missing tile is filled from an ancestor at most this many levels up.
const MAX_ANCESTOR_DEPTH: u32 = 6;
/// Tiles checked per zoom by a coverage check (strided above this).
const MAX_CHECK_TILES: usize = 256;
/// WGS84 mean radius; the UI's gpsToLocal uses the same equirectangular frame.
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/terrain/{z}/{x}/{y}", get(get_tile))
        .route("/api/terrain/heightmap", get(get_heightmap))
        .route("/api/terrain/status", get(get_status))
}

// ------------------------------------------------------------------ stores

/// A read-only cached tile pyramid: a bundle database (`tiles` with either
/// inline `image` or deduped `blob_id` -> `tile_blobs.image`) and the legacy
/// `<dir>/{z}/{x}/{y}.<ext>` layout.
#[derive(Clone, Debug)]
pub(crate) struct TileStore {
    pub sqlite: PathBuf,
    pub dir: PathBuf,
    pub ext: &'static str,
}

impl TileStore {
    pub(crate) fn dem() -> Self {
        let base = crate::map::map_region_base_dir(&crate::map::selected_map_region());
        Self { sqlite: base.join("terrain.sqlite3"), dir: base.join("terrain"), ext: "png" }
    }
    pub(crate) fn imagery() -> Self {
        let region = crate::map::selected_map_region();
        Self {
            sqlite: crate::map::tile_bundle_path(&region),
            dir: crate::map::map_region_base_dir(&region).join("tiles"),
            ext: "jpg",
        }
    }

    /// Successful opens only are cached, so a download made after startup is seen.
    async fn pool(&self) -> Option<sqlx::SqlitePool> {
        static POOLS: Mutex<Option<HashMap<PathBuf, sqlx::SqlitePool>>> = Mutex::new(None);
        if let Some(pool) = POOLS.lock().unwrap().as_ref().and_then(|m| m.get(&self.sqlite)) {
            return Some(pool.clone());
        }
        if !tokio::fs::try_exists(&self.sqlite).await.unwrap_or(false) {
            return None;
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(SqliteConnectOptions::new().filename(&self.sqlite).read_only(true))
            .await
            .map_err(|e| log::warn!("failed to open tile store {}: {e}", self.sqlite.display()))
            .ok()?;
        POOLS.lock().unwrap().get_or_insert_with(HashMap::new).insert(self.sqlite.clone(), pool.clone());
        Some(pool)
    }

    pub(crate) async fn read(&self, z: u32, x: u32, y: u32) -> Option<Vec<u8>> {
        if let Some(pool) = self.pool().await {
            for sql in [
                "SELECT b.image FROM tiles t JOIN tile_blobs b ON b.id = t.blob_id WHERE t.z = ? AND t.x = ? AND t.y = ? LIMIT 1",
                "SELECT image FROM tiles WHERE z = ? AND x = ? AND y = ? LIMIT 1",
            ] {
                match sqlx::query_scalar::<_, Vec<u8>>(sql).bind(i64::from(z)).bind(i64::from(x)).bind(i64::from(y)).fetch_optional(&pool).await {
                    Ok(Some(bytes)) => return Some(bytes),
                    Ok(None) => return None,
                    Err(_) => continue,
                }
            }
        }
        tokio::fs::read(self.dir.join(format!("{z}/{x}/{y}.{}", self.ext))).await.ok()
    }

    pub(crate) async fn has(&self, z: u32, x: u32, y: u32) -> bool {
        if let Some(pool) = self.pool().await
            && let Ok(Some(_)) = sqlx::query_scalar::<_, i64>("SELECT 1 FROM tiles WHERE z = ? AND x = ? AND y = ? LIMIT 1")
                .bind(i64::from(z))
                .bind(i64::from(x))
                .bind(i64::from(y))
                .fetch_optional(&pool)
                .await
        {
            return true;
        }
        tokio::fs::try_exists(self.dir.join(format!("{z}/{x}/{y}.{}", self.ext))).await.unwrap_or(false)
    }

    /// Tile counts per zoom (bundle first, then the legacy directory).
    pub(crate) async fn zoom_counts(&self) -> Vec<(u32, u64)> {
        if let Some(pool) = self.pool().await
            && let Ok(rows) = sqlx::query_as::<_, (i64, i64)>("SELECT z, COUNT(*) FROM tiles GROUP BY z ORDER BY z").fetch_all(&pool).await
            && !rows.is_empty()
        {
            return rows.into_iter().filter_map(|(z, n)| Some((u32::try_from(z).ok()?, u64::try_from(n).ok()?))).collect();
        }
        let mut out = Vec::new();
        let Ok(mut zooms) = tokio::fs::read_dir(&self.dir).await else { return out };
        while let Ok(Some(entry)) = zooms.next_entry().await {
            let Some(z) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let mut count = 0u64;
            if let Ok(mut xs) = tokio::fs::read_dir(entry.path()).await {
                while let Ok(Some(x)) = xs.next_entry().await {
                    if let Ok(mut ys) = tokio::fs::read_dir(x.path()).await {
                        while let Ok(Some(_)) = ys.next_entry().await {
                            count += 1;
                        }
                    }
                }
            }
            out.push((z, count));
        }
        out.sort_unstable();
        out
    }

    pub(crate) async fn max_zoom(&self) -> Option<u32> {
        if let Some(pool) = self.pool().await
            && let Ok(Some(z)) = sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(z) FROM tiles").fetch_one(&pool).await
        {
            return u32::try_from(z).ok();
        }
        self.zoom_counts().await.last().map(|(z, _)| *z)
    }
}

// ------------------------------------------------------------------ decoding

/// Terrarium RGB to metres.
pub fn terrarium_height(r: u8, g: u8, b: u8) -> f32 {
    (f32::from(r) * 256.0 + f32::from(g) + f32::from(b) / 256.0) - 32768.0
}

/// One decoded DEM tile (row-major metres, row 0 north).
#[derive(Clone, Debug)]
pub struct DemTile {
    pub width: u32,
    pub height: u32,
    pub metres: Vec<f32>,
}

impl DemTile {
    pub fn decode(png: &[u8]) -> Option<Self> {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.to_rgb8();
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 || width > 1024 || height > 1024 {
            return None;
        }
        let metres = image.pixels().map(|p| terrarium_height(p[0], p[1], p[2])).collect();
        Some(Self { width, height, metres })
    }
    /// Bilinear height at tile-relative fractional coordinates in [0, 1).
    fn sample(&self, u: f64, v: f64) -> f32 {
        let fx = (u * f64::from(self.width) - 0.5).clamp(0.0, f64::from(self.width - 1));
        let fy = (v * f64::from(self.height) - 0.5).clamp(0.0, f64::from(self.height - 1));
        let (x0, y0) = (fx.floor() as u32, fy.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (tx, ty) = ((fx - f64::from(x0)) as f32, (fy - f64::from(y0)) as f32);
        let at = |x: u32, y: u32| self.metres[(y * self.width + x) as usize];
        let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
        let bottom = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
        top * (1.0 - ty) + bottom * ty
    }
}

/// Encodes a synthetic Terrarium PNG (tests and fixtures).
#[cfg(test)]
pub fn encode_terrarium(width: u32, height: u32, metres: impl Fn(u32, u32) -> f32) -> Vec<u8> {
    let mut image = image::RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let v = f64::from(metres(x, y)) + 32768.0;
        let r = (v / 256.0).floor().clamp(0.0, 255.0);
        let g = (v - r * 256.0).floor().clamp(0.0, 255.0);
        let b = ((v - r * 256.0 - g) * 256.0).round().clamp(0.0, 255.0);
        *pixel = image::Rgb([r as u8, g as u8, b as u8]);
    }
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(image).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap_or_default();
    out
}

// ------------------------------------------------------------------ geometry

/// Fractional slippy-map tile coordinates of a point at zoom z.
pub fn tile_xy(lat: f64, lon: f64, z: u32) -> (f64, f64) {
    let n = f64::from(1u32 << z);
    let rad = lat.clamp(-85.05, 85.05).to_radians();
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x.clamp(0.0, n - 1e-9), y.clamp(0.0, n - 1e-9))
}

/// Ground metres per Terrarium pixel (256 px tiles) at a latitude and zoom.
pub fn metres_per_pixel(lat: f64, z: u32) -> f64 {
    2.0 * std::f64::consts::PI * EARTH_RADIUS_M * lat.to_radians().cos() / (256.0 * f64::from(1u32 << z))
}

/// Local east/north metres to lat/lon (the UI's equirectangular gpsToLocal).
pub fn local_to_gps(lat0: f64, lon0: f64, east_m: f64, north_m: f64) -> (f64, f64) {
    let lat = lat0 + (north_m / EARTH_RADIUS_M).to_degrees();
    let lon = lon0 + (east_m / (EARTH_RADIUS_M * lat0.to_radians().cos().max(1e-6))).to_degrees();
    (lat, lon)
}

/// The smallest zoom whose pixels are at least as fine as the grid spacing.
pub fn zoom_for_spacing(lat: f64, spacing_m: f64, max_zoom: u32) -> u32 {
    (0..=max_zoom).find(|&z| metres_per_pixel(lat, z) <= spacing_m).unwrap_or(max_zoom)
}

// ------------------------------------------------------------------ heightmap

#[derive(Debug, Deserialize)]
pub(crate) struct HeightmapQuery {
    pub lat: f64,
    pub lon: f64,
    pub extent_m: Option<f64>,
    pub size: Option<u32>,
}

/// A decoded square grid around (lat, lon). Row 0 is the north edge, column 0
/// the west edge; samples are evenly spaced over `[-extent/2, +extent/2]` m
/// east and north. Heights are `base_m + value * scale_m` for the
/// little-endian u16 `values` (base64).
#[derive(Debug, Serialize)]
pub struct Heightmap {
    pub available: bool,
    pub format: &'static str,
    pub version: u32,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub extent_m: f64,
    pub size: u32,
    pub spacing_m: f64,
    pub zoom: u32,
    pub min_m: f64,
    pub max_m: f64,
    pub encoding: &'static str,
    pub base_m: f64,
    pub scale_m: f64,
    pub values: String,
    /// Share of samples from tiles at the requested zoom (1 = complete);
    /// the rest came from ancestor tiles.
    pub exact_fraction: f64,
    /// Share of samples with any DEM data (the rest copy the nearest data).
    pub coverage: f64,
    pub source: &'static str,
    pub attribution: &'static str,
}

pub(crate) fn unavailable(reason: &str) -> Value {
    json!({
        "available": false,
        "format": "gs26.heightmap",
        "version": 1,
        "reason": reason,
        "source": DEM_SOURCE_NAME,
        "attribution": DEM_ATTRIBUTION,
    })
}

/// Builds the grid from a store. Never panics; `Err` is the unavailable reason.
pub async fn build_heightmap(store: &TileStore, lat: f64, lon: f64, extent_m: f64, size: u32) -> Result<Heightmap, String> {
    if !(lat.is_finite() && lon.is_finite() && lat.abs() <= 85.0 && lon.abs() <= 180.0) {
        return Err("invalid coordinates".into());
    }
    let extent_m = if extent_m.is_finite() { extent_m.clamp(MIN_EXTENT_M, MAX_EXTENT_M) } else { DEFAULT_EXTENT_M };
    let size = size.clamp(2, MAX_GRID_SIZE);
    let Some(stored_max) = store.max_zoom().await else {
        return Err("no DEM tiles are cached on this station".into());
    };
    let spacing = extent_m / f64::from(size - 1);
    let mut zoom = zoom_for_spacing(lat, spacing, stored_max.min(DEM_MAX_ZOOM));
    // Bound the decoded tile count: the grid's span in tiles at zoom z.
    let span_tiles = |z: u32| {
        let tiles_across = extent_m / (256.0 * metres_per_pixel(lat, z)) + 2.0;
        (tiles_across * tiles_across).ceil() as usize
    };
    while zoom > 0 && span_tiles(zoom) > MAX_GRID_TILES {
        zoom -= 1;
    }
    // Sample positions (fractional tile coordinates at `zoom`).
    let n = size as usize;
    let mut positions = Vec::with_capacity(n * n);
    for row in 0..n {
        let north = extent_m / 2.0 - row as f64 * spacing;
        for col in 0..n {
            let east = -extent_m / 2.0 + col as f64 * spacing;
            let (la, lo) = local_to_gps(lat, lon, east, north);
            positions.push(tile_xy(la, lo, zoom));
        }
    }
    // Load each needed tile once, falling back to its nearest cached ancestor.
    let mut tiles: HashMap<(u32, u32), Option<(u32, Arc<DemTile>)>> = HashMap::new();
    let mut ancestors: HashMap<(u32, u32, u32), Option<Arc<DemTile>>> = HashMap::new();
    for &(fx, fy) in &positions {
        let key = (fx as u32, fy as u32);
        if tiles.contains_key(&key) {
            continue;
        }
        let mut found = None;
        for depth in 0..=MAX_ANCESTOR_DEPTH.min(zoom) {
            let (az, ax, ay) = (zoom - depth, key.0 >> depth, key.1 >> depth);
            let tile = match ancestors.get(&(az, ax, ay)) {
                Some(tile) => tile.clone(),
                None => {
                    let tile = store.read(az, ax, ay).await.and_then(|bytes| DemTile::decode(&bytes)).map(Arc::new);
                    ancestors.insert((az, ax, ay), tile.clone());
                    tile
                }
            };
            if let Some(tile) = tile {
                found = Some((depth, tile));
                break;
            }
        }
        tiles.insert(key, found);
    }
    let mut heights: Vec<Option<f32>> = Vec::with_capacity(positions.len());
    let (mut exact, mut covered) = (0usize, 0usize);
    for &(fx, fy) in &positions {
        let key = (fx as u32, fy as u32);
        let value = tiles.get(&key).cloned().flatten().map(|(depth, tile)| {
            let scale = f64::from(1u32 << depth);
            let (ax, ay) = (fx / scale, fy / scale);
            if depth == 0 {
                exact += 1;
            }
            tile.sample(ax - ax.floor(), ay - ay.floor())
        });
        covered += usize::from(value.is_some());
        heights.push(value);
    }
    if covered == 0 {
        return Err("no DEM tiles are cached for this area".into());
    }
    fill_gaps(&mut heights, n);
    let values: Vec<f32> = heights.into_iter().map(|h| h.unwrap_or(0.0)).collect();
    let min = values.iter().copied().fold(f32::INFINITY, f32::min) as f64;
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
    let (base_m, scale_m, encoded) = encode_u16(&values, min, max);
    Ok(Heightmap {
        available: true,
        format: "gs26.heightmap",
        version: 1,
        lat_deg: lat,
        lon_deg: lon,
        extent_m,
        size,
        spacing_m: spacing,
        zoom,
        min_m: round2(min),
        max_m: round2(max),
        encoding: "u16le-base64",
        base_m,
        scale_m,
        values: encoded,
        exact_fraction: round4(exact as f64 / positions.len() as f64),
        coverage: round4(covered as f64 / positions.len() as f64),
        source: DEM_SOURCE_NAME,
        attribution: DEM_ATTRIBUTION,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// Fills samples with no data from their nearest filled neighbour (row-wise, then column-wise).
fn fill_gaps(heights: &mut [Option<f32>], n: usize) {
    if heights.iter().all(Option::is_some) {
        return;
    }
    let fallback = heights.iter().flatten().copied().next().unwrap_or(0.0);
    for _ in 0..2 {
        for row in 0..n {
            let line = &mut heights[row * n..(row + 1) * n];
            let mut last = None;
            for v in line.iter_mut() {
                if v.is_some() { last = *v } else if last.is_some() { *v = last }
            }
            let mut last = None;
            for v in line.iter_mut().rev() {
                if v.is_some() { last = *v } else if last.is_some() { *v = last }
            }
        }
        for col in 0..n {
            let mut last = None;
            for row in 0..n {
                let v = &mut heights[row * n + col];
                if v.is_some() { last = *v } else if last.is_some() { *v = last }
            }
            let mut last = None;
            for row in (0..n).rev() {
                let v = &mut heights[row * n + col];
                if v.is_some() { last = *v } else if last.is_some() { *v = last }
            }
        }
    }
    for v in heights.iter_mut() {
        v.get_or_insert(fallback);
    }
}

/// u16 quantisation: 0.1 m steps unless the relief needs coarser ones.
fn encode_u16(values: &[f32], min: f64, max: f64) -> (f64, f64, String) {
    let base = min.floor();
    let relief = (max - base).max(0.0);
    let scale = if relief / 0.1 <= 65_535.0 { 0.1 } else { (relief / 65_535.0 * 1000.0).ceil() / 1000.0 };
    let mut bytes = Vec::with_capacity(values.len() * 2);
    for &v in values {
        let q = ((f64::from(v) - base) / scale).round().clamp(0.0, 65_535.0) as u16;
        bytes.extend_from_slice(&q.to_le_bytes());
    }
    (base, scale, base64::engine::general_purpose::STANDARD.encode(bytes))
}

// ------------------------------------------------------------------ coverage

/// Cached tiles of a store around a point, per zoom: `{z, present, checked, total}`.
pub(crate) async fn coverage(store: &TileStore, lat: f64, lon: f64, radius_m: f64, zooms: &[u32]) -> Vec<Value> {
    let mut out = Vec::new();
    for &z in zooms {
        let (n_lat, _) = local_to_gps(lat, lon, 0.0, radius_m);
        let (s_lat, _) = local_to_gps(lat, lon, 0.0, -radius_m);
        let (_, w_lon) = local_to_gps(lat, lon, -radius_m, 0.0);
        let (_, e_lon) = local_to_gps(lat, lon, radius_m, 0.0);
        let (x0, y0) = tile_xy(n_lat, w_lon, z);
        let (x1, y1) = tile_xy(s_lat, e_lon, z);
        let (x0, y0, x1, y1) = (x0 as u32, y0 as u32, x1 as u32, y1 as u32);
        let total = (u64::from(x1 - x0) + 1) * (u64::from(y1 - y0) + 1);
        let stride = ((total as f64 / MAX_CHECK_TILES as f64).sqrt().ceil() as u32).max(1);
        let (mut present, mut checked) = (0u64, 0u64);
        let mut x = x0;
        while x <= x1 {
            let mut y = y0;
            while y <= y1 {
                checked += 1;
                present += u64::from(store.has(z, x, y).await);
                y += stride;
            }
            x += stride;
        }
        out.push(json!({
            "z": z,
            "present": present,
            "checked": checked,
            "total": total,
            "fraction": round4(present as f64 / checked.max(1) as f64),
        }));
    }
    out
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusQuery {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub radius_m: Option<f64>,
}

pub(crate) async fn status(dem: &TileStore, imagery: &TileStore, lat: f64, lon: f64, radius_m: f64) -> Value {
    let radius_m = if radius_m.is_finite() { radius_m.clamp(100.0, 50_000.0) } else { 6_000.0 };
    let counts = dem.zoom_counts().await;
    let dem_tiles: u64 = counts.iter().map(|(_, n)| n).sum();
    let dem_max = counts.last().map(|(z, _)| *z);
    let imagery_max = imagery.max_zoom().await;
    let dem_zooms: Vec<u32> = dem_max.map(|max| (max.saturating_sub(4).max(8)..=max).collect()).unwrap_or_default();
    let imagery_zooms: Vec<u32> = imagery_max
        .map(|max| {
            let mut zooms: Vec<u32> = [12, 14, 16].into_iter().filter(|z| *z < max).collect();
            zooms.push(max);
            zooms
        })
        .unwrap_or_default();
    let dem_coverage = coverage(dem, lat, lon, radius_m, &dem_zooms).await;
    let imagery_coverage = coverage(imagery, lat, lon, radius_m, &imagery_zooms).await;
    let complete = |cov: &[Value]| !cov.is_empty() && cov.iter().all(|c| c["present"] == c["checked"]);
    json!({
        "dem": {
            "available": dem_tiles > 0,
            "tiles": dem_tiles,
            "zooms": counts.iter().map(|(z, n)| json!({"z": z, "tiles": n})).collect::<Vec<_>>(),
            "max_zoom": dem_max,
            "source": DEM_SOURCE_NAME,
            "source_url": DEM_SOURCE_URL,
            "attribution": DEM_ATTRIBUTION,
            "license": DEM_LICENSE,
        },
        "imagery": {
            "available": imagery_max.is_some(),
            "max_zoom": imagery_max,
            "source": IMAGERY_SOURCE_NAME,
            "attribution": IMAGERY_ATTRIBUTION,
        },
        "check": {
            "lat_deg": lat,
            "lon_deg": lon,
            "radius_m": radius_m,
            "dem": dem_coverage,
            "imagery": imagery_coverage,
            "dem_complete": complete(&dem_coverage),
            "imagery_complete": complete(&imagery_coverage),
        },
        "download_hint": "python3 download_map.py downloads imagery and the DEM for the selected regions; --terrain-only refreshes just the DEM.",
    })
}

// ------------------------------------------------------------------ handlers

async fn get_tile(Path((z, x, y_raw)): Path<(u32, u32, String)>) -> Response {
    let Ok(y) = y_raw.trim_end_matches(".png").parse::<u32>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if z > DEM_MAX_ZOOM || u64::from(x) >= 1u64 << z || u64::from(y) >= 1u64 << z {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match TileStore::dem().read(z, x, y).await {
        Some(bytes) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=86400")], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn get_heightmap(State(state): State<Arc<AppState>>, headers: HeaderMap, Query(query): Query<HeightmapQuery>) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let body = match build_heightmap(
        &TileStore::dem(),
        query.lat,
        query.lon,
        query.extent_m.unwrap_or(DEFAULT_EXTENT_M),
        query.size.unwrap_or(DEFAULT_GRID_SIZE),
    )
    .await
    {
        Ok(map) => serde_json::to_value(map).unwrap_or_else(|_| unavailable("encoding failed")),
        Err(reason) => unavailable(&reason),
    };
    ([(header::CACHE_CONTROL, "private, max-age=300")], Json(body)).into_response()
}

async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap, Query(query): Query<StatusQuery>) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let site = crate::launch_site::configured(&state).unwrap_or(crate::launch_site::COMPILED_DEFAULT);
    let lat = query.lat.filter(|v| v.is_finite() && v.abs() <= 85.0).unwrap_or(site.lat_deg);
    let lon = query.lon.filter(|v| v.is_finite() && v.abs() <= 180.0).unwrap_or(site.lon_deg);
    let body = status(&TileStore::dem(), &TileStore::imagery(), lat, lon, query.radius_m.unwrap_or(6_000.0)).await;
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct TempStore {
        pub root: PathBuf,
        pub store: TileStore,
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A deduped bundle database holding `tiles` (synthetic, no network).
    pub(crate) async fn temp_store(name: &str, tiles: &[((u32, u32, u32), Vec<u8>)]) -> TempStore {
        let root = std::env::temp_dir().join(format!(
            "gs-terrain-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let sqlite = root.join("terrain.sqlite3");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&sqlite).create_if_missing(true))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE tile_blobs (id INTEGER PRIMARY KEY, hash BLOB, image BLOB)").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE tiles (z INTEGER, x INTEGER, y INTEGER, blob_id INTEGER, PRIMARY KEY (z, x, y))").execute(&pool).await.unwrap();
        for (i, ((z, x, y), bytes)) in tiles.iter().enumerate() {
            sqlx::query("INSERT INTO tile_blobs VALUES (?, NULL, ?)").bind(i as i64).bind(bytes).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO tiles VALUES (?, ?, ?, ?)").bind(*z as i64).bind(*x as i64).bind(*y as i64).bind(i as i64).execute(&pool).await.unwrap();
        }
        pool.close().await;
        let store = TileStore { sqlite, dir: root.join("terrain"), ext: "png" };
        TempStore { root, store }
    }

    fn decode_values(map: &Heightmap) -> Vec<f64> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(&map.values).unwrap();
        bytes.chunks(2).map(|c| map.base_m + f64::from(u16::from_le_bytes([c[0], c[1]])) * map.scale_m).collect()
    }

    #[test]
    fn terrarium_decodes_to_metres() {
        assert_eq!(terrarium_height(128, 0, 0), 0.0);
        assert_eq!(terrarium_height(131, 122, 0), 890.0);
        assert_eq!(terrarium_height(128, 0, 128), 0.5);
        assert_eq!(terrarium_height(0, 0, 0), -32768.0);
        let png = encode_terrarium(4, 4, |x, y| 890.0 + x as f32 * 10.0 - y as f32 * 0.25);
        let tile = DemTile::decode(&png).unwrap();
        assert_eq!((tile.width, tile.height), (4, 4));
        assert!((tile.metres[0] - 890.0).abs() < 0.01);
        assert!((tile.metres[3] - 920.0).abs() < 0.01);
        assert!((tile.metres[15] - 919.25).abs() < 0.01);
        assert!(DemTile::decode(b"not a png").is_none());
    }

    #[test]
    fn zoom_follows_grid_spacing() {
        let lat = 31.758;
        assert!(metres_per_pixel(lat, 12) > 30.0 && metres_per_pixel(lat, 12) < 35.0);
        assert_eq!(zoom_for_spacing(lat, 100.0, 15), 11);
        assert_eq!(zoom_for_spacing(lat, 1.0, 12), 12);
        let (la, lo) = local_to_gps(lat, -102.046, 1000.0, 1000.0);
        assert!((la - lat - 0.008_993).abs() < 1e-5 && (lo + 102.046 - 0.010_57).abs() < 1e-4);
    }

    /// The test site's z=10 tile filled with a known east-west ramp.
    async fn ramp_store(name: &str) -> (TempStore, (u32, u32, u32)) {
        let (fx, fy) = tile_xy(31.758, -102.046, 10);
        let key = (10, fx as u32, fy as u32);
        let png = encode_terrarium(256, 256, |x, _| 800.0 + x as f32);
        (temp_store(name, &[(key, png)]).await, key)
    }

    #[tokio::test]
    async fn heightmap_samples_the_cached_dem_and_falls_back_to_ancestors() {
        let (temp, key) = ramp_store("grid").await;
        assert!(temp.store.has(key.0, key.1, key.2).await);
        assert_eq!(temp.store.zoom_counts().await, vec![(10, 1)]);
        // A 2 km grid wants a finer zoom than 10: it is filled from the z=10 ancestor.
        let map = build_heightmap(&temp.store, 31.758, -102.046, 2000.0, 33).await.unwrap();
        assert!(map.available && map.size == 33 && map.zoom == 10);
        assert_eq!(map.exact_fraction, 1.0);
        assert_eq!(map.coverage, 1.0);
        let values = decode_values(&map);
        assert_eq!(values.len(), 33 * 33);
        // The ramp rises one metre per z10 pixel (~131 m) eastward and is flat north-south.
        let (west, east) = (values[16 * 33], values[16 * 33 + 32]);
        assert!(east > west + 10.0, "{west} {east}");
        assert!((values[16] - values[32 * 33 + 16]).abs() < 0.2);
        let (fx, _) = tile_xy(31.758, -102.046, 10);
        let centre = 800.0 + (fx - fx.floor()) * 256.0 - 0.5;
        assert!((values[16 * 33 + 16] - centre).abs() < 0.6, "{} {centre}", values[16 * 33 + 16]);
        assert!(map.min_m >= 800.0 && map.max_m <= 1056.0);
    }

    #[tokio::test]
    async fn heightmap_is_unavailable_without_dem_and_never_panics() {
        let missing = TileStore { sqlite: PathBuf::from("/nonexistent/terrain.sqlite3"), dir: PathBuf::from("/nonexistent/terrain"), ext: "png" };
        assert!(build_heightmap(&missing, 31.758, -102.046, 4000.0, 65).await.is_err());
        let (temp, _) = ramp_store("elsewhere").await;
        // Cached tiles elsewhere only: unavailable, not a crash.
        assert!(build_heightmap(&temp.store, 42.7, -77.19, 4000.0, 65).await.is_err());
        assert!(build_heightmap(&temp.store, f64::NAN, 0.0, 4000.0, 65).await.is_err());
        // Out-of-range sizes and extents are clamped.
        let map = build_heightmap(&temp.store, 31.758, -102.046, 1e9, 100_000).await.unwrap();
        assert_eq!((map.size, map.extent_m), (MAX_GRID_SIZE, MAX_EXTENT_M));
        assert!(map.coverage < 1.0 && map.coverage > 0.0);
        let v = unavailable("x");
        assert_eq!(v["available"], false);
    }

    #[tokio::test]
    async fn coverage_check_reports_cached_tiles_around_a_point() {
        let (temp, _) = ramp_store("coverage").await;
        let cov = coverage(&temp.store, 31.758, -102.046, 500.0, &[10, 11]).await;
        assert_eq!(cov[0]["z"], 10);
        assert_eq!(cov[0]["present"], cov[0]["checked"]);
        assert_eq!(cov[0]["fraction"], 1.0);
        assert_eq!(cov[1]["present"], 0);
        let missing = TileStore { sqlite: PathBuf::from("/nonexistent/x.sqlite3"), dir: PathBuf::from("/nonexistent/x"), ext: "jpg" };
        let s = status(&temp.store, &missing, 31.758, -102.046, 500.0).await;
        assert_eq!(s["dem"]["available"], true);
        assert_eq!(s["dem"]["tiles"], 1);
        assert_eq!(s["imagery"]["available"], false);
        assert_eq!(s["check"]["imagery_complete"], false);
        // Zooms 8..=10 are checked for the DEM; only z10 is cached.
        assert_eq!(s["check"]["dem_complete"], false);
        let big = coverage(&temp.store, 31.758, -102.046, 50_000.0, &[15]).await;
        assert!(big[0]["checked"].as_u64().unwrap() <= 2 * MAX_CHECK_TILES as u64);
    }
}
