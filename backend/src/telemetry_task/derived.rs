use super::prelude::*;
use super::{get_current_timestamp_ms, queue_db_write};

pub(super) const FILL_SYSTEM_LOW_VOLTAGE_WARN_THRESHOLD: f32 = 13.0;
pub(super) const FILL_SYSTEM_LOW_VOLTAGE_RELATCH_THRESHOLD: f32 = 15.0;
pub(super) const FILL_SYSTEM_LOW_VOLTAGE_WARNING: &str =
    "Critical: Fill system battery voltage below 13V!";
pub(super) static FILL_SYSTEM_LOW_VOLTAGE_LATCHED: OnceLock<Mutex<bool>> = OnceLock::new();

pub(super) const PARSE_ERROR_REPORT_INTERVAL_MS: u64 = 60_000;

#[derive(Debug, Default)]
pub(super) struct ParseErrorReportState {
    last_emit_ms: Option<u64>,
    pending_count: u64,
}

pub(super) static PARSE_ERROR_REPORTS: OnceLock<Mutex<HashMap<String, ParseErrorReportState>>> =
    OnceLock::new();

pub(super) const GPS_SATELLITES_DATA_TYPE: &str = "GPS_SATELLITE_NUMBER";
pub(super) const VEHICLE_SPEED_DATA_TYPE: &str = "VEHICLE_SPEED";
pub(super) const GRAVITY_MPS2: f32 = 9.80665;

pub(super) static BATTERY_ESTIMATOR_STATE: OnceLock<Mutex<HashMap<String, BatteryEstimatorState>>> =
    OnceLock::new();
pub(super) static SPEED_ESTIMATOR_STATE: OnceLock<Mutex<SpeedEstimatorState>> = OnceLock::new();
pub(super) static BATTERY_LAYOUT_CFG: OnceLock<layout::BatteryLayoutConfig> = OnceLock::new();
pub(super) static NETWORK_TIME_ROUTER: OnceLock<Arc<Router>> = OnceLock::new();
pub(super) const BATTERY_VOLTAGE_EMA_ALPHA: f32 = 0.06;
pub(super) const BATTERY_DROP_RATE_EMA_ALPHA: f32 = 0.10;
pub(super) const BATTERY_MAX_VOLTAGE_SLEW_V_PER_SEC: f32 = 0.035;
pub(super) const BATTERY_MIN_VOLTAGE_DEFAULT: f32 = 6.3;
pub(super) const BATTERY_MAX_VOLTAGE_DEFAULT: f32 = 8.4;

#[derive(Clone, Default)]
pub(super) struct DbOverflow;

pub(super) fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

pub(super) fn should_persist_telemetry_sample(
    data_type: &str,
    sender_id: &str,
    ts_ms: i64,
) -> bool {
    let _ = (data_type, sender_id, ts_ms);
    true
}

#[derive(Default)]
pub(super) struct BatteryEstimatorState {
    samples: VecDeque<(i64, f32)>,
    ema_voltage: Option<f32>,
    ema_drop_rate_v_per_min: Option<f32>,
    ema_remaining_min: Option<f32>,
    last_ts_ms: Option<i64>,
    last_remaining_ts_ms: Option<i64>,
}

#[derive(Default)]
pub(super) struct SpeedEstimatorState {
    speed_mps: Option<f32>,
    last_update_ts_ms: Option<i64>,
    accel_mps2: Option<f32>,
    accel_ts_ms: Option<i64>,
    last_baro_alt_sample: Option<(i64, f32)>,
    baro_speed_mps: Option<f32>,
    baro_speed_ts_ms: Option<i64>,
    last_gps_alt_sample: Option<(i64, f32)>,
    gps_speed_mps: Option<f32>,
    gps_speed_ts_ms: Option<i64>,
}

pub(super) fn battery_layout_cfg() -> &'static layout::BatteryLayoutConfig {
    BATTERY_LAYOUT_CFG.get_or_init(|| match layout::load_layout() {
        Ok(cfg) => cfg.battery,
        Err(err) => {
            eprintln!("WARNING: failed to load battery layout config: {err}");
            layout::BatteryLayoutConfig::default()
        }
    })
}

pub(super) fn push_battery_sample_and_compute_drop_rate(
    source_id: &str,
    ts_ms: i64,
    voltage: f32,
    window_ms: i64,
) -> (f32, Option<f32>) {
    let by_source = BATTERY_ESTIMATOR_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = by_source.lock().unwrap();
    let state = map.entry(source_id.to_string()).or_default();

    let dt_s = state
        .last_remaining_ts_ms
        .map(|t0| (ts_ms.saturating_sub(t0) as f32 / 1000.0).clamp(0.0, 10.0))
        .unwrap_or(0.0);
    state.last_remaining_ts_ms = Some(ts_ms);
    state.last_ts_ms = Some(ts_ms);

    // Clamp abrupt jumps first, then apply low-alpha EMA for heavy smoothing.
    let slewed = if let Some(prev) = state.ema_voltage {
        let max_step = BATTERY_MAX_VOLTAGE_SLEW_V_PER_SEC * dt_s.max(0.02);
        voltage.clamp(prev - max_step, prev + max_step)
    } else {
        voltage
    };
    let smoothed_voltage = state
        .ema_voltage
        .map(|prev| prev + BATTERY_VOLTAGE_EMA_ALPHA * (slewed - prev))
        .unwrap_or(slewed);
    state.ema_voltage = Some(smoothed_voltage);

    state.samples.push_back((ts_ms, smoothed_voltage));
    while let Some((old_ts, _)) = state.samples.front().copied() {
        if ts_ms.saturating_sub(old_ts) <= window_ms {
            break;
        }
        state.samples.pop_front();
    }

    if state.samples.len() < 2 {
        return (smoothed_voltage, None);
    }

    let t0 = state.samples.front().map(|(t, _)| *t).unwrap_or(ts_ms);
    let n = state.samples.len() as f64;
    let mut sum_x = 0.0f64;
    let mut sum_y = 0.0f64;
    let mut sum_xy = 0.0f64;
    let mut sum_x2 = 0.0f64;

    for (t, v) in state.samples.iter() {
        let x = ((*t - t0) as f64) / 1000.0;
        let y = *v as f64;
        sum_x += x;
        sum_y += y;
        sum_xy += x * y;
        sum_x2 += x * x;
    }

    let denom = n * sum_x2 - (sum_x * sum_x);
    if denom.abs() < f64::EPSILON {
        return (smoothed_voltage, None);
    }

    let slope_v_per_sec = (n * sum_xy - (sum_x * sum_y)) / denom;
    if slope_v_per_sec >= 0.0 {
        let dr = state
            .ema_drop_rate_v_per_min
            .map(|prev| prev + BATTERY_DROP_RATE_EMA_ALPHA * (0.0 - prev))
            .unwrap_or(0.0);
        state.ema_drop_rate_v_per_min = Some(dr);
        return (smoothed_voltage, Some(dr));
    }

    let raw_drop = (-slope_v_per_sec * 60.0) as f32;
    let smoothed_drop = state
        .ema_drop_rate_v_per_min
        .map(|prev| prev + BATTERY_DROP_RATE_EMA_ALPHA * (raw_drop - prev))
        .unwrap_or(raw_drop);
    state.ema_drop_rate_v_per_min = Some(smoothed_drop);
    (smoothed_voltage, Some(smoothed_drop))
}

pub(super) fn battery_percent(voltage: f32, empty: f32, full: f32, exponent: f32) -> f32 {
    if full <= empty {
        return 0.0;
    }
    let linear = ((voltage - empty) / (full - empty)).clamp(0.0, 1.0);
    let exp = exponent.max(0.1);
    (linear.powf(exp) * 100.0).clamp(0.0, 100.0)
}

pub(super) fn battery_runtime_parts_data_types(base_data_type: &str) -> (String, String, String) {
    if let Some(prefix) = base_data_type.strip_suffix("_REMAINING_MINUTES") {
        return (
            format!("{prefix}_REMAINING_DAYS"),
            format!("{prefix}_REMAINING_HOURS"),
            format!("{prefix}_REMAINING_MINUTES_PART"),
        );
    }

    (
        format!("{base_data_type}_DAYS"),
        format!("{base_data_type}_HOURS"),
        format!("{base_data_type}_MINUTES_PART"),
    )
}

pub(super) fn battery_runtime_parts(
    remaining_min: Option<f32>,
) -> (Option<f32>, Option<f32>, Option<f32>) {
    let Some(total_minutes_f32) = remaining_min else {
        return (None, None, None);
    };

    let mut total_minutes = total_minutes_f32.max(0.0).round() as i64;
    let days = total_minutes / (24 * 60);
    total_minutes %= 24 * 60;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;

    (Some(days as f32), Some(hours as f32), Some(minutes as f32))
}

pub(super) fn update_speed_ema(prev: Option<f32>, sample: f32, alpha: f32) -> f32 {
    prev.map(|v| v + alpha * (sample - v)).unwrap_or(sample)
}

pub(super) fn ingest_altitude_velocity_sample(
    prev_sample: Option<(i64, f32)>,
    prev_speed: Option<f32>,
    ts_ms: i64,
    altitude_m: f32,
    min_dt_ms: i64,
    max_dt_ms: i64,
    alpha: f32,
) -> (Option<(i64, f32)>, Option<f32>, Option<i64>) {
    let next_sample = Some((ts_ms, altitude_m));
    let Some((prev_ts_ms, prev_altitude_m)) = prev_sample else {
        return (next_sample, prev_speed, None);
    };
    let dt_ms = ts_ms.saturating_sub(prev_ts_ms);
    if dt_ms < min_dt_ms || dt_ms > max_dt_ms {
        return (next_sample, prev_speed, None);
    }
    let dt_s = dt_ms as f32 / 1000.0;
    if dt_s <= 0.0 {
        return (next_sample, prev_speed, None);
    }
    let raw_speed_mps = ((altitude_m - prev_altitude_m) / dt_s).clamp(-800.0, 800.0);
    (
        next_sample,
        Some(update_speed_ema(prev_speed, raw_speed_mps, alpha)),
        Some(ts_ms),
    )
}

pub(super) fn fresh_sensor_value(
    sample: Option<f32>,
    sample_ts_ms: Option<i64>,
    now_ms: i64,
    max_age_ms: i64,
) -> Option<f32> {
    let value = sample?;
    let sample_ts_ms = sample_ts_ms?;
    (now_ms.saturating_sub(sample_ts_ms) <= max_age_ms).then_some(value)
}

pub(super) fn update_vehicle_speed_estimate(
    data_type: &str,
    ts_ms: i64,
    values: &[Option<f32>],
) -> Option<f32> {
    let state_cell =
        SPEED_ESTIMATOR_STATE.get_or_init(|| Mutex::new(SpeedEstimatorState::default()));
    let mut state = state_cell.lock().unwrap();

    match data_type {
        dt if dt == DataType::AccelData.as_str() || dt == DataType::IMUData.as_str() => {
            if let Some(accel_z_mps2) = values.get(2).copied().flatten()
                && accel_z_mps2.is_finite()
            {
                state.accel_mps2 = Some((accel_z_mps2 - GRAVITY_MPS2).clamp(-200.0, 200.0));
                state.accel_ts_ms = Some(ts_ms);
            }
        }
        dt if dt == DataType::BarometerData.as_str() => {
            if let Some(altitude_m) = values.get(2).copied().flatten()
                && altitude_m.is_finite()
            {
                (
                    state.last_baro_alt_sample,
                    state.baro_speed_mps,
                    state.baro_speed_ts_ms,
                ) = ingest_altitude_velocity_sample(
                    state.last_baro_alt_sample,
                    state.baro_speed_mps,
                    ts_ms,
                    altitude_m,
                    10,
                    2_000,
                    0.22,
                );
            }
        }
        dt if dt == DataType::GpsData.as_str() => {
            if let Some(altitude_m) = values.get(2).copied().flatten()
                && altitude_m.is_finite()
            {
                (
                    state.last_gps_alt_sample,
                    state.gps_speed_mps,
                    state.gps_speed_ts_ms,
                ) = ingest_altitude_velocity_sample(
                    state.last_gps_alt_sample,
                    state.gps_speed_mps,
                    ts_ms,
                    altitude_m,
                    100,
                    10_000,
                    0.15,
                );
            }
        }
        _ => return None,
    }

    let accel_mps2 = fresh_sensor_value(state.accel_mps2, state.accel_ts_ms, ts_ms, 600);
    let baro_speed_mps =
        fresh_sensor_value(state.baro_speed_mps, state.baro_speed_ts_ms, ts_ms, 1_500);
    let gps_speed_mps =
        fresh_sensor_value(state.gps_speed_mps, state.gps_speed_ts_ms, ts_ms, 4_500);

    let dt_s = state
        .last_update_ts_ms
        .map(|last_ts_ms| (ts_ms.saturating_sub(last_ts_ms) as f32 / 1000.0).clamp(0.0, 0.25))
        .unwrap_or(0.0);

    let mut fused_speed_mps = state.speed_mps.unwrap_or_else(|| {
        let mut seed = 0.0;
        let mut weight = 0.0;
        if let Some(v) = baro_speed_mps {
            seed += v * 0.75;
            weight += 0.75;
        }
        if let Some(v) = gps_speed_mps {
            seed += v * 0.25;
            weight += 0.25;
        }
        if weight > 0.0 { seed / weight } else { 0.0 }
    });

    if let Some(a) = accel_mps2
        && dt_s > 0.0
    {
        fused_speed_mps += a * dt_s;
    }

    let mut has_measurement = false;
    if let Some(v_baro) = baro_speed_mps {
        fused_speed_mps += 0.35 * (v_baro - fused_speed_mps);
        has_measurement = true;
    }
    if let Some(v_gps) = gps_speed_mps {
        fused_speed_mps += 0.18 * (v_gps - fused_speed_mps);
        has_measurement = true;
    }

    if !has_measurement && state.speed_mps.is_none() {
        return None;
    }

    if let Some(prev_speed_mps) = state.speed_mps {
        let smooth_alpha = if dt_s <= 0.0 {
            1.0
        } else {
            (dt_s / 0.12).clamp(0.15, 1.0)
        };
        fused_speed_mps = prev_speed_mps + smooth_alpha * (fused_speed_mps - prev_speed_mps);
    }

    if fused_speed_mps.abs() < 0.02 {
        fused_speed_mps = 0.0;
    }

    fused_speed_mps = fused_speed_mps.clamp(-800.0, 800.0);
    state.speed_mps = Some(fused_speed_mps);
    state.last_update_ts_ms = Some(ts_ms);
    Some(fused_speed_mps)
}

pub(super) fn battery_bounds_for_source(source: &layout::BatterySourceConfig) -> (f32, f32) {
    let mut empty = if source.empty_voltage.is_finite() {
        source.empty_voltage
    } else {
        BATTERY_MIN_VOLTAGE_DEFAULT
    };
    let mut full = if source.full_voltage.is_finite() {
        source.full_voltage
    } else {
        BATTERY_MAX_VOLTAGE_DEFAULT
    };

    if full <= empty {
        empty = BATTERY_MIN_VOLTAGE_DEFAULT;
        full = BATTERY_MAX_VOLTAGE_DEFAULT;
    }
    (empty, full)
}

pub(super) fn telemetry_values_json(values: &[Option<f32>]) -> Option<String> {
    serde_json::to_string(
        &values
            .iter()
            .map(|v| v.map(|n| n as f64))
            .collect::<Vec<_>>(),
    )
    .ok()
}

pub(super) fn maybe_take_parse_error_report(key: &str, now_ms: u64) -> Option<u64> {
    let reports = PARSE_ERROR_REPORTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut reports = reports.lock().unwrap();
    let state = reports.entry(key.to_string()).or_default();
    state.pending_count = state.pending_count.saturating_add(1);

    let should_emit = state
        .last_emit_ms
        .map(|last_emit_ms| now_ms.saturating_sub(last_emit_ms) >= PARSE_ERROR_REPORT_INTERVAL_MS)
        .unwrap_or(true);
    if !should_emit {
        return None;
    }

    let count = state.pending_count;
    state.pending_count = 0;
    state.last_emit_ms = Some(now_ms);
    Some(count)
}

pub(super) fn should_report_parse_error(data_type: &str) -> bool {
    !matches!(
        data_type,
        "HEARTBEAT"
            | "DISCOVERY_ANNOUNCE"
            | "DISCOVERY_TIMESYNC_SOURCES"
            | "TIME_SYNC_ANNOUNCE"
            | "TIME_SYNC_REQUEST"
            | "TIME_SYNC_RESPONSE"
            | "VALVE_COMMAND"
            | "FLIGHT_COMMAND"
            | "ACTUATOR_COMMAND"
            | "ABORT"
    )
}

pub(super) fn report_parse_error(
    state: &Arc<AppState>,
    sender_id: &str,
    data_type: &str,
    detail: &str,
) {
    if !should_report_parse_error(data_type) {
        return;
    }
    let now_ms = get_current_timestamp_ms();
    let key = format!("{sender_id}:{data_type}:{detail}");
    let Some(count) = maybe_take_parse_error_report(&key, now_ms) else {
        return;
    };
    let message = format!(
        "{sender_id}: {data_type} parse errors: {count} packet(s) in the last {}s ({detail})",
        PARSE_ERROR_REPORT_INTERVAL_MS / 1000
    );
    let _ = state.add_backend_message(message);
}

#[cfg(test)]
pub(super) fn reset_parse_error_reports() {
    if let Some(reports) = PARSE_ERROR_REPORTS.get() {
        reports.lock().unwrap().clear();
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn emit_derived_battery_rows(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    ts_ms: i64,
    source_timestamp_ms: Option<i64>,
    sender_id: &str,
    input_data_type: &str,
    voltage: f32,
    payload_json: &str,
) {
    let cfg = battery_layout_cfg().clone();
    if cfg.sources.is_empty() {
        return;
    }

    let window_ms = (cfg.estimator.window_seconds.max(30) as i64) * 1000;
    let min_drop_rate = cfg.estimator.min_drop_rate_v_per_min.max(0.000001);

    for source in cfg.sources.iter() {
        if source.sender_id != sender_id || source.input_data_type != input_data_type {
            continue;
        }

        let (_smoothed_voltage, drop_rate_v_per_min) =
            push_battery_sample_and_compute_drop_rate(&source.id, ts_ms, voltage, window_ms);

        let (empty_v, full_v) = battery_bounds_for_source(source);
        let pct = battery_percent(voltage, empty_v, full_v, source.curve_exponent);
        let raw_remaining_min = drop_rate_v_per_min.and_then(|rate| {
            if rate < min_drop_rate {
                return None;
            }
            let remaining_voltage = (voltage - empty_v).max(0.0);
            Some(remaining_voltage / rate)
        });
        let remaining_min = smooth_remaining_minutes(&source.id, ts_ms, raw_remaining_min);
        let (remaining_days, remaining_hours, remaining_minutes_part) =
            battery_runtime_parts(remaining_min);
        let (remaining_days_data_type, remaining_hours_data_type, remaining_minutes_part_data_type) =
            battery_runtime_parts_data_types(&source.remaining_minutes_data_type);

        let rows: Vec<(String, Vec<Option<f32>>)> = vec![
            (source.percent_data_type.clone(), vec![Some(pct)]),
            (
                source.drop_rate_data_type.clone(),
                vec![drop_rate_v_per_min],
            ),
            (
                source.remaining_minutes_data_type.clone(),
                vec![remaining_min],
            ),
            (remaining_days_data_type, vec![remaining_days]),
            (remaining_hours_data_type, vec![remaining_hours]),
            (
                remaining_minutes_part_data_type,
                vec![remaining_minutes_part],
            ),
        ];

        for (data_type, values) in rows {
            if should_persist_telemetry_sample(&data_type, sender_id, ts_ms) {
                queue_db_write(
                    state,
                    db_tx,
                    db_overflow,
                    DbWrite::Telemetry {
                        timestamp_ms: ts_ms,
                        source_timestamp_ms,
                        data_type: data_type.clone(),
                        sender_id: sender_id.to_string(),
                        values_json: telemetry_values_json(&values),
                        payload_json: payload_json.to_string(),
                    },
                )
                .await;
            }

            let row = TelemetryRow {
                timestamp_ms: ts_ms,
                data_type,
                sender_id: sender_id.to_string(),
                values,
            };
            state.cache_recent_telemetry(row.clone());
            let _ = state.ws_tx.send(row);
        }
    }
}

pub(super) struct DerivedLoadcellSample<'a> {
    pub(super) ts_ms: i64,
    pub(super) source_timestamp_ms: Option<i64>,
    pub(super) sender_id: &'a str,
    pub(super) sensor_id: &'a str,
    pub(super) raw_value: f32,
    pub(super) payload_json: &'a str,
}

pub(super) async fn emit_derived_loadcell_rows(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    sample: DerivedLoadcellSample<'_>,
) {
    let calibration_sensor_id = if sample.sensor_id == DataType::FuelTankPressure.as_str() {
        loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE
    } else {
        sample.sensor_id
    };
    let cfg = state.loadcell_calibration.lock().unwrap().clone();
    let Some(calibrated_value) =
        loadcell::calibrated_sensor_value(&cfg, calibration_sensor_id, sample.raw_value)
    else {
        match calibration_sensor_id {
            loadcell::RAW_LOADCELL_DATA_TYPE_1000KG => {
                let mut latest = state.latest_fill_mass_kg.lock().unwrap();
                *latest = None;
            }
            loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE => {
                let mut pressure = state.latest_fuel_tank_pressure.lock().unwrap();
                *pressure = None;
            }
            _ => {}
        }
        return;
    };
    let rows: Vec<(&str, Vec<Option<f32>>)> = match calibration_sensor_id {
        loadcell::RAW_LOADCELL_DATA_TYPE_1000KG => {
            let fill_targets = state.fill_targets_snapshot();
            let flight_state = *state.state.lock().unwrap();
            let target_mass_kg = loadcell::active_fill_target_mass_kg(&fill_targets, flight_state);
            let percent = loadcell::fill_percent(target_mass_kg, calibrated_value);
            {
                let mut latest = state.latest_fill_mass_kg.lock().unwrap();
                *latest = Some(calibrated_value);
            }
            vec![
                (
                    loadcell::DERIVED_WEIGHT_DATA_TYPE,
                    vec![Some(calibrated_value)],
                ),
                (
                    loadcell::DERIVED_FILL_PERCENT_DATA_TYPE,
                    vec![Some(percent)],
                ),
            ]
        }
        loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE => {
            {
                let mut pressure = state.latest_fuel_tank_pressure.lock().unwrap();
                *pressure = Some(calibrated_value);
            }
            vec![(
                loadcell::DERIVED_PRESSURE_TRANSDUCER_CALIBRATED_DATA_TYPE,
                vec![Some(calibrated_value)],
            )]
        }
        _ => Vec::new(),
    };

    for (data_type, values) in rows {
        if should_persist_telemetry_sample(data_type, sample.sender_id, sample.ts_ms) {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: sample.ts_ms,
                    source_timestamp_ms: sample.source_timestamp_ms,
                    data_type: data_type.to_string(),
                    sender_id: sample.sender_id.to_string(),
                    values_json: telemetry_values_json(&values),
                    payload_json: sample.payload_json.to_string(),
                },
            )
            .await;
        }

        let row = TelemetryRow {
            timestamp_ms: sample.ts_ms,
            data_type: data_type.to_string(),
            sender_id: sample.sender_id.to_string(),
            values,
        };
        state.cache_recent_telemetry(row.clone());
        let _ = state.ws_tx.send(row);
    }
}

pub(super) async fn emit_derived_vehicle_speed_row(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    ts_ms: i64,
    source_timestamp_ms: Option<i64>,
    speed_mps: f32,
    payload_json: &str,
) {
    let values = vec![Some(speed_mps)];
    if should_persist_telemetry_sample(VEHICLE_SPEED_DATA_TYPE, "", ts_ms) {
        queue_db_write(
            state,
            db_tx,
            db_overflow,
            DbWrite::Telemetry {
                timestamp_ms: ts_ms,
                source_timestamp_ms,
                data_type: VEHICLE_SPEED_DATA_TYPE.to_string(),
                sender_id: String::new(),
                values_json: telemetry_values_json(&values),
                payload_json: payload_json.to_string(),
            },
        )
        .await;
    }

    let row = TelemetryRow {
        timestamp_ms: ts_ms,
        data_type: VEHICLE_SPEED_DATA_TYPE.to_string(),
        sender_id: String::new(),
        values,
    };
    state.cache_recent_telemetry(row.clone());
    let _ = state.ws_tx.send(row);
}

pub(super) fn normalized_gps_values(
    state: &Arc<AppState>,
    sender_id: &str,
    raw_values: &[Option<f32>],
) -> Vec<Option<f32>> {
    let lat = raw_values.first().copied().flatten();
    let lon = raw_values.get(1).copied().flatten();
    let alt = raw_values.get(2).copied().flatten();

    {
        let mut fixes = state.latest_gps_fix_by_sender.lock().unwrap();
        fixes.insert(sender_id.to_string(), vec![lat, lon, alt]);
    }

    vec![lat, lon, alt]
}

pub(super) fn f32_values_from_payload_bytes(bytes: &[u8]) -> Option<Vec<Option<f32>>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(size_of::<f32>()) {
        return None;
    }

    Some(
        bytes
            .chunks_exact(size_of::<f32>())
            .map(|chunk| Some(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])))
            .collect(),
    )
}

pub(super) fn telemetry_f32_values(pkt: &Packet) -> Option<Vec<Option<f32>>> {
    match pkt.data_as_f32() {
        Ok(values) => Some(values.into_iter().map(Some).collect()),
        Err(_) if pkt.data_type() == DataType::GpsData => {
            f32_values_from_payload_bytes(pkt.payload())
        }
        Err(_) => None,
    }
}

type TelemetryValues = Vec<Option<f32>>;

pub(super) fn split_imu_values(
    values: &[Option<f32>],
) -> Option<(TelemetryValues, TelemetryValues)> {
    if values.len() < 6 {
        return None;
    }

    Some((values[..3].to_vec(), values[3..6].to_vec()))
}

pub(super) fn telemetry_rows_from_packet_values(
    state: &Arc<AppState>,
    pkt: &Packet,
    sender_id: &str,
    mut values: Vec<Option<f32>>,
) -> Vec<(String, Vec<Option<f32>>)> {
    match pkt.data_type() {
        DataType::GpsData => {
            values = normalized_gps_values(state, sender_id, &values);
            vec![(DataType::GpsData.as_str().to_string(), values)]
        }
        DataType::IMUData => split_imu_values(&values)
            .map(|(accel, gyro)| {
                vec![
                    (DataType::AccelData.as_str().to_string(), accel),
                    (DataType::GyroData.as_str().to_string(), gyro),
                ]
            })
            .unwrap_or_else(|| vec![(DataType::IMUData.as_str().to_string(), values)]),
        _ => vec![(pkt.data_type().as_str().to_string(), values)],
    }
}

pub(super) fn is_fill_system_battery_sender(sender_id: &str) -> bool {
    matches!(
        canonical_sender_id(sender_id),
        sender if sender == Board::GatewayBoard.sender_id()
    )
}

pub(super) fn update_fill_system_low_voltage_latch(voltage: f32) -> bool {
    if !voltage.is_finite() {
        return false;
    }

    let mut latched = FILL_SYSTEM_LOW_VOLTAGE_LATCHED
        .get_or_init(|| Mutex::new(false))
        .lock()
        .unwrap();
    if voltage > FILL_SYSTEM_LOW_VOLTAGE_RELATCH_THRESHOLD {
        *latched = false;
        return false;
    }

    if voltage <= FILL_SYSTEM_LOW_VOLTAGE_WARN_THRESHOLD && !*latched {
        *latched = true;
        return true;
    }

    false
}

#[cfg(test)]
pub(super) fn reset_fill_system_low_voltage_latch_for_tests() {
    if let Some(latched) = FILL_SYSTEM_LOW_VOLTAGE_LATCHED.get() {
        *latched.lock().unwrap() = false;
    }
}

pub(super) async fn handle_gps_satellite_count_packet(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    pkt: &Packet,
    payload_json: &str,
) -> Option<TelemetryRow> {
    let count = pkt.data_as_u8().ok().and_then(|v| v.first().copied())?;
    let ts_ms = get_current_timestamp_ms() as i64;
    let sender_id = pkt.sender().to_string();

    {
        let mut sats = state.latest_gps_satellites_by_sender.lock().unwrap();
        sats.insert(sender_id.clone(), count);
    }

    let values = vec![Some(count as f32)];
    if should_persist_telemetry_sample(GPS_SATELLITES_DATA_TYPE, &sender_id, ts_ms) {
        queue_db_write(
            state,
            db_tx,
            db_overflow,
            DbWrite::Telemetry {
                timestamp_ms: ts_ms,
                source_timestamp_ms: Some(pkt.timestamp() as i64),
                data_type: GPS_SATELLITES_DATA_TYPE.to_string(),
                sender_id: sender_id.clone(),
                values_json: telemetry_values_json(&values),
                payload_json: payload_json.to_string(),
            },
        )
        .await;
    }

    Some(TelemetryRow {
        timestamp_ms: ts_ms,
        data_type: GPS_SATELLITES_DATA_TYPE.to_string(),
        sender_id,
        values,
    })
}

pub(super) fn smooth_remaining_minutes(
    source_id: &str,
    ts_ms: i64,
    raw: Option<f32>,
) -> Option<f32> {
    const REMAINING_EMA_ALPHA: f32 = 0.05;
    const REMAINING_MAX_STEP_MIN_PER_SEC: f32 = 0.08;

    let by_source = BATTERY_ESTIMATOR_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = by_source.lock().unwrap();
    let state = map.entry(source_id.to_string()).or_default();
    let Some(raw_val) = raw else {
        state.ema_remaining_min = None;
        return None;
    };

    let dt_s = state
        .last_ts_ms
        .map(|t0| (ts_ms.saturating_sub(t0) as f32 / 1000.0).clamp(0.0, 10.0))
        .unwrap_or(0.0);
    let prev = state.ema_remaining_min.unwrap_or(raw_val);
    let max_step = REMAINING_MAX_STEP_MIN_PER_SEC * dt_s.max(0.02);
    let slewed = raw_val.clamp(prev - max_step, prev + max_step);
    let smoothed = prev + REMAINING_EMA_ALPHA * (slewed - prev);
    state.ema_remaining_min = Some(smoothed.max(0.0));
    state.ema_remaining_min
}
