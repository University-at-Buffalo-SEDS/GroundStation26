//! Coverage and admission limits for the independent recovery-station dataset.
use crate::{DEFAULT_MAX_ZOOM, URRG_BOUNDS, coverage_bounds, tile_range_for_bounds};
use anyhow::{Context, Result, ensure};
use std::collections::HashSet;
pub(crate) type Bounds = (f64, f64, f64, f64);
// UB official visitor directions: north43.001,-78.790; south42.953,-78.818;
// downtown42.899,-78.866. https://www.buffalo.edu/home/visiting-ub.html
pub(crate) const UB_BOUNDS: Bounds = (-78.91, 42.87, -78.74, 43.04);
// Midland city and nearby airport (FAA MAF diagram:31°57'N,102°12'W).
// https://aeronav.faa.gov/afd/22JAN2026/SC_rear_22JAN2026.pdf
pub(crate) const MIDLAND_BOUNDS: Bounds = (-102.30, 31.85, -101.95, 32.10);
pub(crate) const RRLS_BOUNDS: &[Bounds] = &[UB_BOUNDS, URRG_BOUNDS, MIDLAND_BOUNDS];
pub(crate) const RRLS_REGION: &str = "rrls";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    GroundStation,
    Rrls,
}
impl Profile {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "groundstation" => Ok(Self::GroundStation),
            "rrls" => Ok(Self::Rrls),
            _ => anyhow::bail!("MAP_PROFILE must be groundstation or rrls"),
        }
    }
    pub(crate) fn configured() -> Result<Self> {
        Self::parse(&std::env::var("MAP_PROFILE").unwrap_or_else(|_| "groundstation".into()))
    }
    pub(crate) fn bounds(self, z: u32) -> &'static [Bounds] {
        match self {
            Self::GroundStation => coverage_bounds(z),
            Self::Rrls => RRLS_BOUNDS,
        }
    }
    pub(crate) fn default_region(self) -> &'static str {
        match self {
            Self::GroundStation => crate::REGION,
            Self::Rrls => RRLS_REGION,
        }
    }
    pub(crate) fn validate(self, z: u32, direct: bool, resume: bool, bundle: bool) -> Result<()> {
        if self == Self::Rrls {
            ensure!(
                z <= DEFAULT_MAX_ZOOM,
                "RRLS profile supports zoom0..=17; higher zoom exceeds the bounded dataset plan"
            );
            ensure!(
                direct && resume && bundle,
                "RRLS profile requires direct-to-DB, bundle and resume enabled; legacy/rebuild modes cannot preserve its storage bound"
            );
            let planned = planned_bytes(z)?;
            ensure!(
                planned <= crate::budget::MAX_DATABASE_BYTES,
                "RRLS worst-case tile plan exceeds its database budget"
            );
            println!(
                "RRLS worst-case database plan: {planned} bytes; dataset cap50,000,000,000 bytes including SQLite WAL and other files"
            );
        }
        Ok(())
    }
}
pub(crate) fn planned_bytes(max_zoom: u32) -> Result<u64> {
    let mut count = 0u64;
    for z in 0..=max_zoom {
        let mut coords = HashSet::new();
        for &b in RRLS_BOUNDS {
            let (a, c, d, e) = tile_range_for_bounds(b, z);
            for x in a..=c {
                for y in d..=e {
                    coords.insert((x, y));
                }
            }
        }
        count = count
            .checked_add(coords.len() as u64)
            .context("RRLS tile count overflow")?;
    }
    // Maximum admitted image body plus conservative per-tile page/index overhead.
    count
        .checked_mul((crate::network::MAX_TILE_BYTES + 64 * 1024) as u64)
        .and_then(|n| n.checked_add(64 * 1024 * 1024))
        .context("RRLS byte plan overflow")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rrls_never_selects_bulk_regions_and_contains_all_sites() {
        for z in 0..=17 {
            assert_eq!(Profile::Rrls.bounds(z), RRLS_BOUNDS);
        }
        for (lon, lat) in [
            (-78.790, 43.001),
            (-78.818, 42.953),
            (-78.866, 42.899),
            (-77.1941, 42.7004),
            (-102.20, 31.95),
            (-102.08, 32.0),
        ] {
            assert!(
                RRLS_BOUNDS
                    .iter()
                    .any(|b| lon >= b.0 && lon <= b.2 && lat >= b.1 && lat <= b.3)
            );
        }
        assert_eq!(
            Profile::GroundStation.bounds(crate::BASE_COVERAGE_MAX_ZOOM),
            &[crate::NA_BOUNDS]
        );
    }
    #[test]
    fn worst_case_plan_fits_but_unsupported_modes_are_rejected() {
        assert!(planned_bytes(17).unwrap() < crate::budget::MAX_DATABASE_BYTES);
        assert!(Profile::Rrls.validate(17, true, true, true).is_ok());
        for (z, d, r, b) in [
            (18, true, true, true),
            (17, false, true, true),
            (17, true, false, true),
            (17, true, true, false),
        ] {
            assert!(Profile::Rrls.validate(z, d, r, b).is_err());
        }
        assert!(Profile::parse("typo").is_err());
    }
}
