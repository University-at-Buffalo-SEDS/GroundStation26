//! Explicit small repair requests never expand into the normal region download.
use anyhow::{Result, bail, ensure};
use std::collections::BTreeSet;
#[derive(Default)]
pub(crate) struct Options {
    pub test: bool,
    pub repairs: BTreeSet<(u32, u32, u32)>,
}
impl Options {
    pub(crate) fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut args = args.into_iter();
        let mut out = Self::default();
        while let Some(arg) = args.next() {
            if arg == "--test" {
                out.test = true;
            } else if arg == "--repair-tile" {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--repair-tile requires Z/X/Y"))?;
                out.repairs.insert(tile(&value)?);
            } else if let Some(value) = arg.strip_prefix("--repair-tile=") {
                out.repairs.insert(tile(value)?);
            } else {
                bail!("unknown map downloader argument: {arg}");
            }
            ensure!(
                out.repairs.len() <= 32,
                "repair is limited to 32 distinct tiles per run"
            );
        }
        ensure!(
            !out.test || out.repairs.is_empty(),
            "--test and --repair-tile cannot be combined; --test never writes map data"
        );
        Ok(out)
    }
    pub(crate) fn validate(
        &self,
        profile: crate::profile::Profile,
        max_zoom: u32,
        resume: bool,
        direct: bool,
        bundle: bool,
    ) -> Result<()> {
        if self.repairs.is_empty() {
            return Ok(());
        }
        ensure!(
            (direct && bundle) || (!direct && !bundle),
            "targeted repair requires direct-to-DB or unbundled legacy files; it cannot rebuild an entire bundle via --via-tiles"
        );
        ensure!(
            resume,
            "targeted repair requires resume enabled; it must not rebuild/delete an existing dataset"
        );
        for &(z, x, y) in &self.repairs {
            ensure!(z <= max_zoom, "repair zoom exceeds MAP_MAX_ZOOM");
            ensure!(
                profile.bounds(z).iter().any(|bounds| {
                    let (a, b, c, d) = crate::tile_range_for_bounds(*bounds, z);
                    x >= a && x <= b && y >= c && y <= d
                }),
                "repair coordinate {z}/{x}/{y} lies outside the selected map profile"
            );
        }
        Ok(())
    }
}
fn tile(value: &str) -> Result<(u32, u32, u32)> {
    let values = value
        .split('/')
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        values.len() == 3 && values[0] <= 30,
        "repair coordinate must be Z/X/Y with zoom 0..=30"
    );
    let (z, x, y) = (values[0], values[1], values[2]);
    ensure!(
        u64::from(x) < (1u64 << z) && u64::from(y) < (1u64 << z),
        "repair coordinates exceed zoom bounds"
    );
    Ok((z, x, y))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(args.iter().map(|s| s.to_string()))
    }
    #[test]
    fn repair_parser_is_bounded_deduplicated_and_not_a_probe() {
        let p = parse(&["--repair-tile", "0/0/0", "--repair-tile=0/0/0"]).unwrap();
        assert_eq!(p.repairs.len(), 1);
        for args in [
            vec!["--test", "--repair-tile", "0/0/0"],
            vec!["--repair-tile", "31/0/0"],
            vec!["--repair-tile", "1/2/0"],
            vec!["--repair-tile", "../../bad"],
            vec!["--repair-tile"],
            vec!["--unknown"],
        ] {
            assert!(parse(&args).is_err());
        }
        assert!(Options::parse((0..33).map(|x| format!("--repair-tile=6/{x}/0"))).is_err());
    }
    #[test]
    fn repair_keeps_profile_zoom_and_resume_interlocks() {
        let p = parse(&["--repair-tile", "0/0/0"]).unwrap();
        assert!(
            p.validate(crate::profile::Profile::Rrls, 17, true, true, true)
                .is_ok()
        );
        assert!(
            p.validate(crate::profile::Profile::Rrls, 17, false, true, true)
                .is_err()
        );
        assert!(
            p.validate(
                crate::profile::Profile::GroundStation,
                17,
                true,
                false,
                true
            )
            .is_err()
        );
        assert!(
            p.validate(
                crate::profile::Profile::GroundStation,
                17,
                true,
                true,
                false
            )
            .is_err()
        );
        assert!(
            p.validate(
                crate::profile::Profile::GroundStation,
                17,
                true,
                false,
                false
            )
            .is_ok()
        );
        let p = parse(&["--repair-tile", "17/0/0"]).unwrap();
        assert!(
            p.validate(crate::profile::Profile::Rrls, 17, true, true, true)
                .is_err()
        );
        assert!(
            p.validate(crate::profile::Profile::GroundStation, 16, true, true, true)
                .is_err()
        );
    }
}
