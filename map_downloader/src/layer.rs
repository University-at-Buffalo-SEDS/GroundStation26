//! The tile layer a run downloads: the satellite imagery (default) or, with
//! `--terrain`, the open AWS Terrain Tiles DEM in Terrarium PNG encoding
//! (`height_m = R*256 + G + B/256 - 32768`), which the 3D map and the 3D
//! replay drape the imagery over. Terrain goes to its own bundle,
//! `backend/data/maps/<region>/terrain.sqlite3`, with the imagery schema.
//!
//! Terrain source and licence: AWS Open Data "Terrain Tiles"
//! (https://registry.opendata.aws/terrain-tiles/), keyless; attribution per
//! https://github.com/tilezen/joerd/blob/master/docs/attribution.md
//! (SRTM, USGS 3DEP, GMTED2010, ETOPO1 and others).
use crate::profile::{Bounds, Profile};
use anyhow::{Result, ensure};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Imagery,
    Terrain,
}

static LAYER: OnceLock<Layer> = OnceLock::new();

pub(crate) const TERRAIN_BASE_URL: &str =
    "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";
/// Default DEM depth: ~32 m pixels at 31° N, plenty under 30-150 m mesh spacing.
pub(crate) const DEFAULT_TERRAIN_MAX_ZOOM: u32 = 12;
/// Terrarium's deepest native zoom.
pub(crate) const TERRAIN_MAX_NATIVE_ZOOM: u32 = 15;
/// The DEM covers North America only at these coarse zooms; deeper zooms
/// cover the profile's high-detail regions.
pub(crate) const TERRAIN_BASE_COVERAGE_MAX_ZOOM: u32 = 6;

/// Selects the layer once, before any download starts.
pub(crate) fn select(layer: Layer) {
    let _ = LAYER.set(layer);
}

pub(crate) fn current() -> Layer {
    LAYER.get().copied().unwrap_or(Layer::Imagery)
}

pub(crate) fn base_url() -> &'static str {
    match current() {
        Layer::Imagery => crate::SATELLITE_BASE_URL,
        Layer::Terrain => TERRAIN_BASE_URL,
    }
}

/// A tile URL: `{z}/{x}/{y}` templates are filled in, a plain base gets the
/// ArcGIS `/{z}/{y}/{x}` order.
pub(crate) fn tile_url(base: &str, z: u32, x: u32, y: u32) -> String {
    if base.contains("{z}") {
        base.replace("{z}", &z.to_string()).replace("{x}", &x.to_string()).replace("{y}", &y.to_string())
    } else {
        format!("{base}/{z}/{y}/{x}")
    }
}

pub(crate) fn label() -> &'static str {
    match current() {
        Layer::Imagery => "satellite",
        Layer::Terrain => "terrain (DEM)",
    }
}

pub(crate) fn bundle_file() -> &'static str {
    match current() {
        Layer::Imagery => "tiles.sqlite3",
        Layer::Terrain => "terrain.sqlite3",
    }
}

/// The override variable for this layer's bundle path.
pub(crate) fn bundle_path_env() -> &'static str {
    match current() {
        Layer::Imagery => "MAP_BUNDLE_PATH",
        Layer::Terrain => "MAP_TERRAIN_BUNDLE_PATH",
    }
}

/// The bounds a zoom covers for this layer.
pub(crate) fn bounds(profile: Profile, z: u32) -> &'static [Bounds] {
    match current() {
        Layer::Imagery => profile.bounds(z),
        Layer::Terrain => terrain_bounds(profile, z),
    }
}

pub(crate) fn terrain_bounds(profile: Profile, z: u32) -> &'static [Bounds] {
    match profile {
        Profile::GroundStation if z <= TERRAIN_BASE_COVERAGE_MAX_ZOOM => &[crate::NA_BOUNDS],
        // The detail regions (MAP_REGIONS-aware) at every deeper zoom.
        Profile::GroundStation => profile.bounds(crate::BASE_COVERAGE_MAX_ZOOM + 1),
        Profile::Rrls => profile.bounds(z),
    }
}

/// MAP_TERRAIN_MAX_ZOOM (default 12, at most 15).
pub(crate) fn terrain_max_zoom(value: Option<&str>) -> Result<u32> {
    let z = match value {
        Some(raw) => raw.trim().parse::<u32>().map_err(|_| anyhow::anyhow!("MAP_TERRAIN_MAX_ZOOM must be a number"))?,
        None => DEFAULT_TERRAIN_MAX_ZOOM,
    };
    ensure!(z <= TERRAIN_MAX_NATIVE_ZOOM, "MAP_TERRAIN_MAX_ZOOM must be 0..={TERRAIN_MAX_NATIVE_ZOOM} (Terrarium's deepest zoom)");
    Ok(z)
}

/// Distinct DEM tiles the plan requests (for the size estimate).
pub(crate) fn planned_terrain_tiles(profile: Profile, max_zoom: u32) -> u64 {
    let mut total = 0u64;
    for z in 0..=max_zoom {
        let mut coords = std::collections::HashSet::new();
        for &b in terrain_bounds(profile, z) {
            let (a, c, d, e) = crate::tile_range_for_bounds(b, z);
            for x in a..=c {
                for y in d..=e {
                    coords.insert((x, y));
                }
            }
        }
        total += coords.len() as u64;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_follow_each_source_order() {
        assert_eq!(tile_url(TERRAIN_BASE_URL, 12, 811, 1663), "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/12/811/1663.png");
        assert_eq!(tile_url("https://example.test/tile", 3, 1, 2), "https://example.test/tile/3/2/1");
    }

    #[test]
    fn terrain_covers_north_america_coarsely_and_detail_regions_deeply() {
        assert_eq!(terrain_bounds(Profile::GroundStation, 6), &[crate::NA_BOUNDS]);
        let deep = terrain_bounds(Profile::GroundStation, 7);
        assert!(!deep.contains(&crate::NA_BOUNDS));
        // The West Texas launch site is covered at the default depth.
        let (lon, lat) = (-102.046, 31.758);
        assert!(deep.iter().any(|b| lon >= b.0 && lon <= b.2 && lat >= b.1 && lat <= b.3));
        assert_eq!(terrain_bounds(Profile::Rrls, 12), crate::profile::RRLS_BOUNDS);
        // A bounded plan: well under the imagery's millions of tiles.
        let tiles = planned_terrain_tiles(Profile::GroundStation, DEFAULT_TERRAIN_MAX_ZOOM);
        assert!(tiles > 1_000 && tiles < 20_000, "{tiles}");
    }

    #[test]
    fn terrain_zoom_is_bounded_by_the_source() {
        assert_eq!(terrain_max_zoom(None).unwrap(), 12);
        assert_eq!(terrain_max_zoom(Some("15")).unwrap(), 15);
        assert!(terrain_max_zoom(Some("16")).is_err());
        assert!(terrain_max_zoom(Some("x")).is_err());
    }
}
