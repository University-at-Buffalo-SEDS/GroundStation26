use super::*;
use crate::auto_zero::{Settings, SettingsUpdate};

fn config() -> loadcell::LoadcellCalibrationFile {
    let mut cfg = loadcell::LoadcellCalibrationFile::default();
    cfg.ch1.m = Some(1.0);
    cfg.ch1.b = Some(0.0);
    cfg.ch1_zero_raw = None;
    cfg
}

#[test]
fn sample_count_follows_measured_rate_with_a_100_sample_floor() {
    // DAQ-Board caps KG1000 at 10/25 of a 100 Hz aggregate: 40 Hz at most.
    assert_eq!(sample_count(40.0), Ok(200));
    assert_eq!(sample_count(39.86), Ok(199));
    assert_eq!(sample_count(30.0), Ok(150));
    assert_eq!(sample_count(20.0), Ok(100));
    // Below 20 Hz the floor takes longer than five seconds but still fits.
    assert_eq!(sample_count(12.0), Ok(100));
    // The capture history never holds more than the existing 200 samples.
    assert_eq!(sample_count(250.0), Ok(200));
    // 100 samples would need more than the window less its margin.
    assert!(sample_count(11.0).is_err());
    assert!(sample_count(0.0).is_err());
    assert!(sample_count(f64::NAN).is_err());
}

#[test]
fn rate_is_measured_from_recent_live_samples_only() {
    let base = CaptureClock::now();
    let samples: VecDeque<Sample> = (0..41)
        .map(|i| Sample { at: base.later(Duration::from_millis(25 * i)), raw: 0.0, packet_id: None })
        .collect();
    let now = base.later(Duration::from_millis(1_010));
    let rate = measured_rate_hz(&samples, now).unwrap();
    assert!((rate - 40.0).abs() < 0.01, "{rate}");
    // A stream that stopped is not live, whatever its earlier rate was.
    assert!(measured_rate_hz(&samples, base.later(Duration::from_millis(1_300))).is_err());
    assert!(measured_rate_hz(&VecDeque::new(), now).is_err());
    // Invalidated receipts never count.
    let mut invalid = samples.clone();
    for sample in &mut invalid {
        sample.at.invalidate();
    }
    assert!(measured_rate_hz(&invalid, now).is_err());
}

#[test]
fn hundred_sample_capture_keeps_the_same_stability_check() {
    assert!(stable_count(&[0.2; MIN_SAMPLES], &config(), SENSOR, MIN_SAMPLES).unwrap().1);
    let mut outliers = vec![0.2; MIN_SAMPLES];
    outliers[0] = 900.0;
    outliers[1] = -900.0;
    let (raw, _) = stable_count(&outliers, &config(), SENSOR, MIN_SAMPLES).unwrap();
    assert!((raw - 0.2).abs() < 0.001);
    let moving: Vec<f32> = (0..MIN_SAMPLES).map(|i| if i % 2 == 0 { -0.7 } else { 0.7 }).collect();
    assert!(stable_count(&moving, &config(), SENSOR, MIN_SAMPLES).is_err());
    assert!(stable_count(&[2.0; MIN_SAMPLES], &config(), SENSOR, MIN_SAMPLES).is_err());
    assert!(stable_count(&[0.0; MIN_SAMPLES - 1], &config(), SENSOR, MIN_SAMPLES).is_err());
}

#[tokio::test]
async fn nothing_held_means_no_status_and_cancel_is_refused() {
    let state = crate::state::tests::test_app_state().await;
    assert!(!held(&state));
    assert_eq!(status(&state), None);
    assert!(cancel(&state).is_err());
}

#[test]
fn launch_setting_is_opt_in_and_off_keeps_the_saved_wire() {
    assert!(!Settings::default().before_launch);
    let old: Settings = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
    assert!(old.enabled && !old.before_launch);
    assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"enabled":true}"#);
    let on = old.updated(SettingsUpdate { enabled: None, before_launch: Some(true) });
    assert!(on.enabled && on.before_launch);
    assert_eq!(
        serde_json::to_string(&on).unwrap(),
        r#"{"enabled":true,"before_launch":true}"#
    );
    // An older client that only knows the fill setting keeps the launch one.
    let fill_off = on.updated(SettingsUpdate { enabled: Some(false), before_launch: None });
    assert!(!fill_off.enabled && fill_off.before_launch);
}
