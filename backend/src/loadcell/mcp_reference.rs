//! The DAQ's MCP3564R reference voltage, a per-station DAQ setting.
//!
//! The DAQ publishes every MCP3564R input as volts = code x VREF / 2^23 (P6:
//! divided again by its x16 gain), with VREF the value this station sends as
//! DAQ_MCP_REFERENCE (nominal 2.4 V; the internal reference of the current
//! board measures about 2.2 V). A load-cell calibration is a function of those
//! volts, so it holds only under the reference it was fitted under, kept with
//! it as `fitted_mcp_vref_v` (a calibration saved before the field existed was
//! fitted under the nominal 2.4 V).
//!
//! When the station's reference differs from a calibration's, the calibration
//! is converted, never reinterpreted: with r = current / fitted, the same ADC
//! code now reads r times the volts, so every volt value (zero, points,
//! thermal points, noise sigma, captures) is multiplied by r and every
//! coefficient moves so kg for that code is unchanged: a slope is divided by
//! r, a polynomial's x^i term by r^i, x0 is multiplied by r, the constant term
//! and the tare (kg) are unchanged, and a thermal V/degC slope is multiplied
//! by r (its reference temperature is unchanged). This is the DAQ's own
//! `daq_calibration_scale_input`, which converts the board's copy the same way
//! when the reference changes, so after the station re-sends they agree.
use super::*;

/// MCP3564R internal reference, nominal (DS20006391C: 2.4 V +/-2 %).
pub const NOMINAL_MCP_VREF_V: f32 = 2.4;
/// The DAQ refuses a DAQ_MCP_REFERENCE outside this range.
pub const MIN_MCP_VREF_V: f32 = 1.9;
pub const MAX_MCP_VREF_V: f32 = 2.6;
/// Longest operator note kept with the setting.
pub const MAX_NOTE_LEN: usize = 200;

pub(super) fn nominal() -> f32 {
    NOMINAL_MCP_VREF_V
}

/// A calibration saved before `fitted_mcp_vref_v` existed was fitted under the
/// nominal reference (the DAQ assumed it).
pub(super) fn legacy_fitted() -> Option<f32> {
    Some(NOMINAL_MCP_VREF_V)
}

pub fn validate(volts: f32) -> Result<(), String> {
    if volts.is_finite() && (MIN_MCP_VREF_V..=MAX_MCP_VREF_V).contains(&volts) {
        Ok(())
    } else {
        Err(format!(
            "MCP3564R reference must be {MIN_MCP_VREF_V}-{MAX_MCP_VREF_V} V (got {volts})"
        ))
    }
}

/// Full scale of the unity-gain MCP3564R inputs (KG1000, AMP2 KG50) in volts.
pub fn unity_full_scale_v(vref: f32) -> f32 {
    vref
}

/// Full scale of the P6 differential x16 scan in volts.
pub fn p6_full_scale_v(vref: f32) -> f32 {
    vref / 16.0
}

/// r = current / fitted for one calibration; `None` (not stamped yet) means it
/// was made under the current reference.
fn ratio(fitted: Option<f32>, current: f32) -> f64 {
    match fitted {
        Some(fitted) if fitted.is_finite() && fitted > 0.0 && fitted != current => {
            current as f64 / fitted as f64
        }
        _ => 1.0,
    }
}

fn convert_channel(
    label: &str,
    channel: Option<&mut GenericCalibrationChannel>,
    noise: Option<&mut NoiseCalibration>,
    captures: &mut [TemperatureCapture],
    current: f32,
) {
    let Some(channel) = channel else { return };
    let k = ratio(channel.fitted_mcp_vref_v, current);
    if k != 1.0 {
        log::info!(
            "converting the {label} calibration from MCP3564R reference {} V to {current} V",
            channel.fitted_mcp_vref_v.unwrap_or(current)
        );
        migrate_channel(channel, k);
        migrate_sensor_extras(None, noise, captures, k);
    }
    channel.fitted_mcp_vref_v = Some(current);
}

fn convert_thermal(label: &str, thermal: Option<&mut ThermalCalibration>, current: f32) {
    let Some(thermal) = thermal else { return };
    let k = ratio(thermal.fitted_mcp_vref_v, current);
    if k != 1.0 {
        log::info!(
            "converting the {label} thermal calibration from MCP3564R reference {} V to {current} V",
            thermal.fitted_mcp_vref_v.unwrap_or(current)
        );
        migrate_sensor_extras(Some(thermal), None, &mut [], k);
    }
    thermal.fitted_mcp_vref_v = Some(current);
}

/// Bring every MCP3564R calibration (KG1000, both KG50 input profiles, their
/// thermal, noise and captures) to the station's current reference and stamp
/// it. Other channels (IADC, pressure transducers) do not carry the field.
/// Runs from `normalize_calibration`, after the legacy channels are copied in.
pub(super) fn rereference(cfg: &mut LoadcellCalibrationFile) {
    if validate(cfg.mcp_vref_v).is_err() {
        log::warn!(
            "calibration file MCP3564R reference {} V is invalid; using the nominal {NOMINAL_MCP_VREF_V} V",
            cfg.mcp_vref_v
        );
        cfg.mcp_vref_v = NOMINAL_MCP_VREF_V;
    }
    let current = cfg.mcp_vref_v;
    for (key, channel) in cfg.extra_channels.iter_mut() {
        if !matches!(key.as_str(), "ch1" | "kg50") {
            channel.fitted_mcp_vref_v = None;
        }
    }
    for (channel, sensor) in [("ch1", "KG1000"), ("kg50", "KG50")] {
        convert_channel(
            sensor,
            cfg.extra_channels.get_mut(channel),
            cfg.noise.get_mut(sensor),
            cfg.temperature_captures
                .get_mut(sensor)
                .map(|v| v.as_mut_slice())
                .unwrap_or_default(),
            current,
        );
        convert_thermal(sensor, cfg.thermal.get_mut(sensor), current);
    }
    for (input, profile) in cfg.kg50_input_profiles.iter_mut() {
        let label = format!("KG50 {input}");
        convert_channel(
            &label,
            profile.channel.as_mut(),
            profile.noise.as_mut(),
            &mut profile.captures,
            current,
        );
        convert_thermal(&label, profile.thermal.as_mut(), current);
    }
}

/// Change the station's reference and convert every calibration to it.
pub fn set_reference(cfg: &mut LoadcellCalibrationFile, volts: f32) -> Result<(), String> {
    validate(volts)?;
    cfg.mcp_vref_v = volts;
    normalize_calibration(cfg);
    Ok(())
}

/// A calibration a peer sends must be expressed under this station's
/// reference: converting it here would change the document the peer revised.
pub(super) fn check_incoming(
    cfg: &LoadcellCalibrationFile,
    channel: Option<&GenericCalibrationChannel>,
    thermal: Option<&ThermalCalibration>,
) -> anyhow::Result<()> {
    let current = cfg.mcp_vref_v;
    for fitted in [
        channel.and_then(|c| c.fitted_mcp_vref_v),
        thermal.and_then(|t| t.fitted_mcp_vref_v),
    ]
    .into_iter()
    .flatten()
    {
        anyhow::ensure!(
            fitted == current,
            "the peer's calibration is for MCP3564R reference {fitted} V but this station's is {current} V; set the same DAQ reference on both stations"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published volts for an ADC code under `vref` (unity gain).
    fn volts(code: i32, vref: f32) -> f32 {
        (code as f64 * vref as f64 / 8_388_608.0) as f32
    }

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol * (1.0 + a.abs().max(b.abs()))
    }

    fn fitted_file() -> LoadcellCalibrationFile {
        let mut cfg = LoadcellCalibrationFile::default();
        normalize_calibration(&mut cfg);
        let ch1 = cfg.extra_channels.get_mut("ch1").unwrap();
        ch1.linear = ChannelLinear { m: Some(1234.5), b: Some(-12.0) };
        ch1.fit = Some(FitMeta { fit_type: Some("linear".into()), ..FitMeta::default() });
        ch1.zero_raw = Some(volts(150_000, 2.4));
        ch1.points = vec![CalibrationPoint { expected: 100.0, raw: volts(600_000, 2.4) }];
        ch1.fitted_mcp_vref_v = Some(2.4);
        let kg50 = cfg.extra_channels.get_mut("kg50").unwrap();
        kg50.fit = Some(FitMeta {
            fit_type: Some("poly4".into()),
            a: Some(3.0),
            b: Some(-7.0),
            c: Some(11.0),
            d: Some(40.0),
            e: Some(0.5),
            x0: Some(0.05),
        });
        kg50.zero_raw = Some(volts(40_000, 2.4));
        kg50.fitted_mcp_vref_v = Some(2.4);
        cfg.thermal.insert(
            "KG1000".into(),
            ThermalCalibration {
                reference_c: 25.0,
                raw_per_c: 1.5e-4,
                points: vec![[25.0, 0.04]],
                fitted_mcp_vref_v: Some(2.4),
            },
        );
        cfg.thermal.insert(
            "KG50".into(),
            ThermalCalibration {
                reference_c: 30.0,
                raw_per_c: -2.0e-5,
                points: vec![],
                fitted_mcp_vref_v: Some(2.4),
            },
        );
        normalize_calibration(&mut cfg);
        cfg
    }

    fn kg(cfg: &LoadcellCalibrationFile, sensor: &str, code: i32, temperature: f32) -> f32 {
        let raw = volts(code, cfg.mcp_vref_v);
        let corrected = temperature_corrected_raw(cfg, sensor, raw, Some(temperature)).unwrap();
        calibrated_weight_kg(cfg, sensor, corrected).unwrap()
    }

    #[test]
    fn a_reference_change_keeps_kg_for_the_same_code() {
        let before = fitted_file();
        let mut after = before.clone();
        set_reference(&mut after, 2.2).unwrap();
        assert_eq!(after.mcp_vref_v, 2.2);
        for sensor in ["KG1000", "KG50"] {
            for code in [-300_000, 0, 41_000, 650_000, 2_000_000] {
                for temperature in [10.0, 25.0, 41.0] {
                    let (a, b) = (kg(&before, sensor, code, temperature), kg(&after, sensor, code, temperature));
                    assert!(close(a, b, 2e-5), "{sensor} code {code} at {temperature} C: {a} vs {b}");
                }
            }
        }
        // Each calibration is stamped with the reference it is now expressed in.
        assert_eq!(after.extra_channels["ch1"].fitted_mcp_vref_v, Some(2.2));
        assert_eq!(after.extra_channels["kg50"].fitted_mcp_vref_v, Some(2.2));
        assert_eq!(after.thermal["KG1000"].fitted_mcp_vref_v, Some(2.2));
        assert_eq!(after.extra_channels["iadc"].fitted_mcp_vref_v, None);
        // The DAQ rule: slope / r, x^i / r^i, x0 * r, V/degC * r.
        let r = 2.2_f64 / 2.4;
        let m = after.extra_channels["ch1"].linear.m.unwrap() as f64;
        assert!((m - 1234.5 / r).abs() < 1e-3);
        assert_eq!(after.ch1.m, after.extra_channels["ch1"].linear.m, "legacy mirror follows");
        let fit = after.extra_channels["kg50"].fit.clone().unwrap();
        assert!((fit.a.unwrap() as f64 - 3.0 / r.powi(4)).abs() < 1e-5);
        assert!((fit.d.unwrap() as f64 - 40.0 / r).abs() < 1e-4);
        assert_eq!(fit.e, Some(0.5));
        assert!((fit.x0.unwrap() as f64 - 0.05 * r).abs() < 1e-8);
        let t = after.thermal["KG1000"].raw_per_c as f64;
        assert!((t - 1.5e-4 * r).abs() < 1e-10);
        assert_eq!(after.thermal["KG1000"].reference_c, 25.0);
        // And back: the original within float rounding.
        let mut back = after.clone();
        set_reference(&mut back, 2.4).unwrap();
        assert!(close(back.extra_channels["ch1"].linear.m.unwrap(), 1234.5, 1e-6));
        // The coefficients the DAQ is sent move the same way.
        let sent_before = kg50_daq_coefficients(&before).unwrap();
        let sent_after = kg50_daq_coefficients(&after).unwrap();
        assert!(close(sent_after[1], (sent_before[1] as f64 / r) as f32, 1e-6));
        assert!(close(sent_after[5], (sent_before[5] as f64 * r) as f32, 1e-6));
        assert!(close(sent_after[6], sent_before[6], 1e-5), "tare is kg");
    }

    #[test]
    fn the_p6_profile_converts_while_it_is_stored() {
        let mut cfg = fitted_file();
        switch_kg50_input(&mut cfg, Kg50Input::P6Differential);
        let p6 = cfg.extra_channels.entry("kg50".into()).or_default();
        p6.linear = ChannelLinear { m: Some(500.0), b: Some(0.0) };
        p6.fit = Some(FitMeta { fit_type: Some("linear".into()), ..FitMeta::default() });
        p6.fitted_mcp_vref_v = Some(2.4);
        switch_kg50_input(&mut cfg, Kg50Input::Amp2);
        set_reference(&mut cfg, 2.2).unwrap();
        switch_kg50_input(&mut cfg, Kg50Input::P6Differential);
        let p6 = &cfg.extra_channels["kg50"];
        assert_eq!(p6.fitted_mcp_vref_v, Some(2.2));
        assert!(close(p6.linear.m.unwrap(), (500.0 / (2.2_f64 / 2.4)) as f32, 1e-6));
    }

    #[test]
    fn a_file_without_the_fields_was_fitted_under_the_nominal_reference() {
        let json = r#"{"version":2,"mcp_vref_v":2.2,"extra_channels":{"ch1":{"linear":{"m":100.0,"b":0.0}}},
            "thermal":{"KG1000":{"reference_c":20.0,"raw_per_c":0.001}}}"#;
        let mut cfg: LoadcellCalibrationFile = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.extra_channels["ch1"].fitted_mcp_vref_v, Some(2.4));
        normalize_calibration(&mut cfg);
        assert!(close(cfg.extra_channels["ch1"].linear.m.unwrap(), (100.0 / (2.2_f64 / 2.4)) as f32, 1e-6));
        assert!(close(cfg.thermal["KG1000"].raw_per_c, (0.001 * (2.2_f64 / 2.4)) as f32, 1e-6));
        // Normalizing again changes nothing.
        let again = {
            let mut c = cfg.clone();
            normalize_calibration(&mut c);
            c
        };
        assert_eq!(again.extra_channels["ch1"].linear.m, cfg.extra_channels["ch1"].linear.m);
        let older: LoadcellCalibrationFile = serde_json::from_str(r#"{"version":2}"#).unwrap();
        assert_eq!(older.mcp_vref_v, NOMINAL_MCP_VREF_V);
    }

    #[test]
    fn the_setting_is_validated_and_persisted() {
        let mut cfg = fitted_file();
        for bad in [1.89, 2.61, f32::NAN, f32::INFINITY, 0.0] {
            assert!(set_reference(&mut cfg, bad).is_err(), "{bad}");
            assert_eq!(cfg.mcp_vref_v, NOMINAL_MCP_VREF_V);
        }
        set_reference(&mut cfg, 1.9).unwrap();
        set_reference(&mut cfg, 2.6).unwrap();
        set_reference(&mut cfg, 2.2).unwrap();
        cfg.mcp_vref_note = "measured approx.; re-measure after the December repair".into();
        let dir = std::env::temp_dir().join(format!("gs-mcp-reference-{}", rand::random::<u64>()));
        let path = dir.join("loadcell_calibration.json");
        save_at(&cfg, &path).unwrap();
        let loaded = load_or_default_at(&path, false);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.mcp_vref_v, 2.2);
        assert_eq!(loaded.mcp_vref_note, cfg.mcp_vref_note);
        assert_eq!(
            loaded.extra_channels["ch1"].linear.m,
            cfg.extra_channels["ch1"].linear.m,
            "loading a converted file does not convert it again"
        );
        // A hand-edited out-of-range value falls back to the nominal reference.
        let mut bad = cfg.clone();
        bad.mcp_vref_v = 3.3;
        normalize_calibration(&mut bad);
        assert_eq!(bad.mcp_vref_v, NOMINAL_MCP_VREF_V);
    }

    #[test]
    fn a_ui_save_is_stamped_with_the_reference_it_says_it_was_fitted_at() {
        let mut cfg = fitted_file();
        set_reference(&mut cfg, 2.2).unwrap();
        // A nested per-channel field is ignored (missing deserializes as the
        // nominal); the top-level reference the editor loaded is what counts.
        let mut incoming: LoadcellCalibrationFile = serde_json::from_value(serde_json::json!({
            "version": CALIBRATION_VERSION,
            "extra_channels": {"ch1": {"linear": {"m": 900.0, "b": 1.0}, "points": []}}
        }))
        .unwrap();
        incoming.kg50_input = cfg.kg50_input;
        let merged = merge_channel_update(&cfg, &CalibrationUpdate::based_on(&cfg, "ch1", incoming)).unwrap();
        assert_eq!(merged.extra_channels["ch1"].linear.m, Some(900.0), "not reinterpreted");
        assert_eq!(merged.extra_channels["ch1"].fitted_mcp_vref_v, Some(2.2));
    }

    #[test]
    fn rail_limits_follow_the_reference() {
        let mut cfg = LoadcellCalibrationFile::default();
        assert_eq!(loadcell_rail_limits_for(&cfg, "KG50"), Some((-2.4, 2.4)));
        cfg.mcp_vref_v = 2.2;
        assert_eq!(loadcell_rail_limits_for(&cfg, "KG1000").unwrap().1, 2.2);
        assert_eq!(loadcell_rail_limits_for(&cfg, "KG50"), Some((-2.2, 2.2)));
        cfg.kg50_input = Kg50Input::P6Differential;
        assert_eq!(loadcell_rail_limits_for(&cfg, "kg50"), Some((-2.2 / 16.0, 2.2 / 16.0)));
        assert!(loadcell_volts_at_rail_for(&cfg, "kg50", 0.14));
        assert!(!loadcell_volts_at_rail_for(&cfg, "kg50", 0.13));
    }
}
