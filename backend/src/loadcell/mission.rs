//! Channel-scoped mission documents. Runtime adapters must hold the calibration
//! lock across compare, durable save and replacement, and enforce an idle boundary.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Channel {
    calibration: GenericCalibrationChannel,
    thermal: Option<ThermalCalibration>,
    noise: Option<NoiseCalibration>,
    captures: Vec<TemperatureCapture>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Kg50 {
    input: Kg50Input,
    profiles: BTreeMap<String, Kg50InputProfile>,
    active: Channel,
}
fn read_channel(cfg: &LoadcellCalibrationFile, channel: &str, sensor: &str) -> Channel {
    Channel {
        calibration: cfg.extra_channels.get(channel).cloned().unwrap_or_default(),
        thermal: cfg.thermal.get(sensor).cloned(),
        noise: cfg.noise.get(sensor).cloned(),
        captures: cfg
            .temperature_captures
            .get(sensor)
            .cloned()
            .unwrap_or_default(),
    }
}
fn check_channel(value: &Channel) -> anyhow::Result<()> {
    let c = &value.calibration;
    anyhow::ensure!(
        c.linear
            .m
            .into_iter()
            .chain(c.linear.b)
            .chain(c.zero_raw)
            .all(f32::is_finite),
        "nonfinite calibration"
    );
    anyhow::ensure!(
        c.points
            .iter()
            .all(|p| p.expected.is_finite() && p.raw.is_finite()),
        "nonfinite calibration point"
    );
    if let Some(f) = &c.fit {
        anyhow::ensure!(
            [f.a, f.b, f.c, f.d, f.e, f.x0]
                .into_iter()
                .flatten()
                .all(f32::is_finite),
            "nonfinite calibration fit"
        );
        anyhow::ensure!(
            f.fit_type.as_ref().is_none_or(|name| name.len() <= 64),
            "invalid calibration fit name"
        );
    }
    anyhow::ensure!(
        value.captures.iter().all(|c| c.expected.is_finite()
            && c.raw.is_finite()
            && c.temperature_c
                .is_none_or(|t| t.is_finite() && (-40.0..=125.0).contains(&t))),
        "invalid calibration capture"
    );
    Ok(())
}
fn install(
    cfg: &mut LoadcellCalibrationFile,
    channel: &str,
    sensor: &str,
    value: Channel,
) -> anyhow::Result<()> {
    check_channel(&value)?;
    cfg.extra_channels.insert(channel.into(), value.calibration);
    cfg.thermal.remove(sensor);
    cfg.noise.remove(sensor);
    cfg.temperature_captures.remove(sensor);
    if let Some(v) = value.thermal {
        cfg.thermal.insert(sensor.into(), v);
    }
    if let Some(v) = value.noise {
        cfg.noise.insert(sensor.into(), v);
    }
    if !value.captures.is_empty() {
        cfg.temperature_captures
            .insert(sensor.into(), value.captures);
    }
    Ok(())
}
pub fn snapshot(cfg: &LoadcellCalibrationFile) -> anyhow::Result<BTreeMap<String, Value>> {
    let mut cfg = cfg.clone();
    normalize_calibration(&mut cfg);
    validate_thermal(&cfg).map_err(anyhow::Error::msg)?;
    let mut docs = BTreeMap::new();
    for (channel, sensor) in [("ch1", "KG1000"), ("iadc", "IADC")] {
        let value = read_channel(&cfg, channel, sensor);
        check_channel(&value)?;
        docs.insert(
            format!("calibration/{channel}"),
            serde_json::to_value(value)?,
        );
    }
    let active = read_channel(&cfg, "kg50", "KG50");
    check_channel(&active)?;
    for (name, profile) in &cfg.kg50_input_profiles {
        anyhow::ensure!(
            matches!(name.as_str(), "amp2" | "p6_differential"),
            "unknown 50kg input profile"
        );
        let mut check = LoadcellCalibrationFile::default();
        install(
            &mut check,
            "kg50",
            "KG50",
            Channel {
                calibration: profile.channel.clone().unwrap_or_default(),
                thermal: profile.thermal.clone(),
                noise: profile.noise.clone(),
                captures: profile.captures.clone(),
            },
        )?;
        validate_thermal(&check).map_err(anyhow::Error::msg)?;
    }
    docs.insert(
        "calibration/kg50".into(),
        serde_json::to_value(Kg50 {
            input: cfg.kg50_input,
            profiles: cfg.kg50_input_profiles,
            active,
        })?,
    );
    anyhow::ensure!(
        cfg.full_mass_kg.is_none_or(f32::is_finite),
        "nonfinite full mass"
    );
    docs.insert(
        "calibration/full_mass_kg".into(),
        serde_json::to_value(cfg.full_mass_kg)?,
    );
    Ok(docs)
}
/// Pure compare-and-replace. No files, hardware, live samples or other channels
/// are changed. Unknown or noncanonical document fields are rejected.
pub fn replace(
    cfg: &LoadcellCalibrationFile,
    key: &str,
    expected: &Value,
    value: &Value,
) -> anyhow::Result<LoadcellCalibrationFile> {
    let before = snapshot(cfg)?;
    anyhow::ensure!(
        before.get(key) == Some(expected),
        "calibration changed while applying mission settings"
    );
    let mut next = cfg.clone();
    normalize_calibration(&mut next);
    match key {
        "calibration/ch1" => install(
            &mut next,
            "ch1",
            "KG1000",
            serde_json::from_value(value.clone())?,
        )?,
        "calibration/iadc" => install(
            &mut next,
            "iadc",
            "IADC",
            serde_json::from_value(value.clone())?,
        )?,
        "calibration/kg50" => {
            let incoming: Kg50 = serde_json::from_value(value.clone())?;
            install(&mut next, "kg50", "KG50", incoming.active)?;
            next.kg50_input = incoming.input;
            next.kg50_input_profiles = incoming.profiles;
        }
        "calibration/full_mass_kg" => next.full_mass_kg = serde_json::from_value(value.clone())?,
        _ => anyhow::bail!("unknown mission calibration document"),
    }
    sync_extra_channels_into_legacy(&mut next);
    normalize_calibration(&mut next);
    let after = snapshot(&next)?;
    anyhow::ensure!(
        after.get(key) == Some(value),
        "invalid or noncanonical mission calibration document"
    );
    Ok(next)
}
/// The runtime holds the calibration lock across compare, persistence and
/// replacement, after checking the GSE safe boundary. Failed persistence leaves
/// the in-memory configuration unchanged; the coordinator is acknowledged later.
pub fn apply_and_save(
    cfg: &mut LoadcellCalibrationFile,
    pending: &paired_settings::coordinator::Pending,
    save: impl FnOnce(&LoadcellCalibrationFile) -> Result<(), String>,
) -> anyhow::Result<()> {
    let next = replace(
        cfg,
        &pending.key,
        &pending.expected_local,
        &pending.revision.value,
    )?;
    save(&next).map_err(anyhow::Error::msg)?;
    *cfg = next;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_channels_merge_and_explicit_reset_cannot_resurrect_legacy_fit() {
        let mut a = LoadcellCalibrationFile::default();
        normalize_calibration(&mut a);
        a.extra_channels.entry("ch1".into()).or_default().linear.m = Some(11.);
        sync_extra_channels_into_legacy(&mut a);
        let old = snapshot(&a).unwrap();
        let mut b = a.clone();
        b.extra_channels.entry("iadc".into()).or_default().linear.m = Some(22.);
        sync_extra_channels_into_legacy(&mut b);
        let incoming = snapshot(&b).unwrap();
        let merged = replace(
            &a,
            "calibration/iadc",
            &old["calibration/iadc"],
            &incoming["calibration/iadc"],
        )
        .unwrap();
        assert_eq!(merged.ch1.m, Some(11.));
        assert_eq!(merged.iadc.m, Some(22.));
        let mut empty = a.clone();
        empty
            .extra_channels
            .insert("ch1".into(), GenericCalibrationChannel::default());
        sync_extra_channels_into_legacy(&mut empty);
        let cleared = snapshot(&empty).unwrap();
        let reset = replace(
            &merged,
            "calibration/ch1",
            &old["calibration/ch1"],
            &cleared["calibration/ch1"],
        )
        .unwrap();
        assert_eq!(reset.ch1.m, None);
        assert_eq!(reset.iadc.m, Some(22.));
    }
    #[test]
    fn physical_input_and_both_profiles_travel_as_one_document() {
        let mut a = LoadcellCalibrationFile::default();
        normalize_calibration(&mut a);
        a.extra_channels.get_mut("kg50").unwrap().linear.m = Some(10.);
        let old = snapshot(&a).unwrap();
        let mut b = a.clone();
        switch_kg50_input(&mut b, Kg50Input::P6Differential);
        b.extra_channels.entry("kg50".into()).or_default().linear.m = Some(20.);
        let remote = snapshot(&b).unwrap();
        let mut merged = replace(
            &a,
            "calibration/kg50",
            &old["calibration/kg50"],
            &remote["calibration/kg50"],
        )
        .unwrap();
        assert_eq!(merged.kg50_input, Kg50Input::P6Differential);
        assert_eq!(merged.extra_channels["kg50"].linear.m, Some(20.));
        switch_kg50_input(&mut merged, Kg50Input::Amp2);
        assert_eq!(merged.extra_channels["kg50"].linear.m, Some(10.));
    }
    #[test]
    fn stale_compare_unknown_fields_and_malformed_values_are_rejected_without_mutation() {
        let a = LoadcellCalibrationFile::default();
        let docs = snapshot(&a).unwrap();
        let mut edit = docs["calibration/ch1"].clone();
        edit["calibration"]["linear"]["m"] = serde_json::json!(7.);
        let changed = replace(&a, "calibration/ch1", &docs["calibration/ch1"], &edit).unwrap();
        assert!(replace(&changed, "calibration/ch1", &docs["calibration/ch1"], &edit).is_err());
        edit["password"] = serde_json::json!("excluded");
        assert!(replace(&a, "calibration/ch1", &docs["calibration/ch1"], &edit).is_err());
        assert!(replace(&a, "hardware/port", &Value::Null, &Value::Null).is_err());
        assert_eq!(snapshot(&a).unwrap(), docs);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use paired_settings::{Revision, coordinator::Pending};
    use serde_json::json;
    fn pending(cfg: &LoadcellCalibrationFile) -> Pending {
        Pending {
            key: "calibration/full_mass_kg".into(),
            expected_local: snapshot(cfg).unwrap()["calibration/full_mass_kg"].clone(),
            revision: Revision {
                clock: BTreeMap::from([("rrls".into(), 1)]),
                value: json!(17.0),
            },
        }
    }
    #[test]
    fn failed_durable_save_cannot_change_live_calibration() {
        let mut cfg = LoadcellCalibrationFile::default();
        let before = serde_json::to_value(&cfg).unwrap();
        let incoming = pending(&cfg);
        assert!(apply_and_save(&mut cfg, &incoming, |_| Err("disk unavailable".into())).is_err());
        assert_eq!(serde_json::to_value(&cfg).unwrap(), before);
    }
    #[test]
    fn stale_compare_is_rejected_before_persistence_and_success_changes_only_one_document() {
        let mut cfg = LoadcellCalibrationFile::default();
        let incoming = pending(&cfg);
        cfg.full_mass_kg = Some(11.0);
        assert!(apply_and_save(&mut cfg, &incoming, |_| panic!("stale value persisted")).is_err());
        assert_eq!(cfg.full_mass_kg, Some(11.0));
        let incoming = pending(&cfg);
        let before = snapshot(&cfg).unwrap();
        apply_and_save(&mut cfg, &incoming, |next| {
            assert_eq!(next.full_mass_kg, Some(17.0));
            Ok(())
        })
        .unwrap();
        let after = snapshot(&cfg).unwrap();
        for (key, value) in before {
            if key != incoming.key {
                assert_eq!(after[&key], value);
            }
        }
        assert_eq!(cfg.full_mass_kg, Some(17.0));
    }
}
