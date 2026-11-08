//! Current-flight maxima computed on the GS from received telemetry: maximum
//! altitude, speed and acceleration, each with its T+ time and source.
//!
//! - Tracked only while the T clock counts up (T+), so pad noise is excluded.
//! - One extreme per source; the UI shows the highest-precedence source first
//!   (FC Kalman over barometer over GPS for altitude, FC Kalman velocity over
//!   the GS-derived vehicle speed for speed, raw accelerometer magnitude for
//!   acceleration).
//! - The FC's own maxima ("EV apogee guess", "EV maxvel guess" messages) are
//!   kept alongside as the authoritative values once received.
//! - Persisted beside the launch-clock session (survives a GS restart for the
//!   same T+ anchor) and written to the recording as `GS_FLIGHT_MAXIMA` rows.
//! - Cleared by a T-clock reset, which Reset all boards also performs, and by a
//!   new T+ anchor (a new flight).
use crate::{state::AppState, telemetry_db::LaunchClockKind, types::TelemetryRow};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{LazyLock, Mutex},
};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extreme {
    pub value: f64,
    pub source: String,
    /// Lower is preferred when several sources report the same quantity.
    pub precedence: u8,
    pub t_plus_ms: Option<i64>,
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FlightMaxima {
    /// T+ anchor of the flight these maxima belong to.
    pub flight_anchor_ms: Option<i64>,
    pub tracking: bool,
    /// Metres, per source, preferred first.
    pub altitude: Vec<Extreme>,
    /// Metres per second, per source, preferred first.
    pub speed: Vec<Extreme>,
    /// Metres per second squared (magnitude), per source, preferred first.
    pub acceleration: Vec<Extreme>,
    /// FC-reported maxima; authoritative once present.
    pub fc_altitude: Option<Extreme>,
    pub fc_speed: Option<Extreme>,
    pub fc_acceleration: Option<Extreme>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantity {
    Altitude,
    Speed,
    Acceleration,
}

/// (quantity, value, source, precedence) samples carried by one row.
pub fn samples(row: &TelemetryRow) -> Vec<(Quantity, f64, &'static str, u8)> {
    let v = |i: usize| {
        row.values
            .get(i)
            .copied()
            .flatten()
            .map(f64::from)
            .filter(|v| v.is_finite())
    };
    let mut out = Vec::new();
    match row.data_type.as_str() {
        "ASCENT_STATE" => {
            if let Some(alt) = v(4) {
                out.push((Quantity::Altitude, alt, "FC Kalman altitude", 0));
            }
            if let Some(vel) = v(5) {
                out.push((Quantity::Speed, vel.abs(), "FC Kalman velocity", 0));
            }
        }
        "DESCENT_STATE" => {
            if let Some(alt) = v(2) {
                out.push((Quantity::Altitude, alt, "FC Kalman altitude", 0));
            }
            if let Some(vel) = v(3) {
                out.push((Quantity::Speed, vel.abs(), "FC Kalman velocity", 0));
            }
        }
        "BAROMETER_DATA" => {
            if let Some(alt) = v(2) {
                out.push((Quantity::Altitude, alt, "Barometer altitude", 1));
            }
        }
        "GPS_DATA" => {
            if let Some(alt) = v(2) {
                out.push((Quantity::Altitude, alt, "GPS altitude", 2));
            }
        }
        "VEHICLE_SPEED" => {
            if let Some(speed) = v(0) {
                out.push((
                    Quantity::Speed,
                    speed.abs(),
                    "GS vehicle speed (baro/GPS)",
                    1,
                ));
            }
        }
        "ACCEL_DATA" => {
            if let (Some(x), Some(y), Some(z)) = (v(0), v(1), v(2)) {
                out.push((
                    Quantity::Acceleration,
                    (x * x + y * y + z * z).sqrt(),
                    "Raw accelerometer",
                    0,
                ));
            }
        }
        _ => {}
    }
    out
}

/// FC text reports of its own maxima: "EV apogee guess: 2810", "EV maxvel
/// guess: 251". Accepts the GS "FC: " prefix too.
pub fn parse_fc_message(text: &str) -> Option<(Quantity, f64)> {
    let text = text.trim();
    let text = text.strip_prefix("FC:").map(str::trim).unwrap_or(text);
    let (quantity, rest) = if let Some(rest) = text.strip_prefix("EV apogee guess") {
        (Quantity::Altitude, rest)
    } else {
        (Quantity::Speed, text.strip_prefix("EV maxvel guess")?)
    };
    let value: f64 = rest.trim_start_matches([':', ' ']).trim().parse().ok()?;
    value.is_finite().then_some((quantity, value.abs()))
}

impl FlightMaxima {
    fn list(&mut self, quantity: Quantity) -> &mut Vec<Extreme> {
        match quantity {
            Quantity::Altitude => &mut self.altitude,
            Quantity::Speed => &mut self.speed,
            Quantity::Acceleration => &mut self.acceleration,
        }
    }

    /// Start (or restart) tracking for the flight anchored at `anchor`.
    /// Returns true when this cleared a previous flight.
    pub fn align(&mut self, kind: LaunchClockKind, anchor: Option<i64>) -> bool {
        let tracking = kind == LaunchClockKind::TPlus && anchor.is_some();
        let mut cleared = false;
        if tracking && self.flight_anchor_ms != anchor {
            cleared = self.flight_anchor_ms.is_some() || !self.is_empty();
            *self = FlightMaxima {
                flight_anchor_ms: anchor,
                ..FlightMaxima::default()
            };
        }
        self.tracking = tracking;
        cleared
    }

    pub fn is_empty(&self) -> bool {
        self.altitude.is_empty()
            && self.speed.is_empty()
            && self.acceleration.is_empty()
            && self.fc_altitude.is_none()
            && self.fc_speed.is_none()
            && self.fc_acceleration.is_none()
    }

    /// Fold one sample in. Returns true when a maximum changed.
    pub fn observe(
        &mut self,
        quantity: Quantity,
        value: f64,
        source: &str,
        precedence: u8,
        t_plus_ms: Option<i64>,
        timestamp_ms: i64,
    ) -> bool {
        if !self.tracking || !value.is_finite() {
            return false;
        }
        let list = self.list(quantity);
        match list.iter_mut().find(|e| e.source == source) {
            Some(existing) if existing.value >= value => false,
            Some(existing) => {
                existing.value = value;
                existing.t_plus_ms = t_plus_ms;
                existing.timestamp_ms = timestamp_ms;
                true
            }
            None => {
                list.push(Extreme {
                    value,
                    source: source.to_string(),
                    precedence,
                    t_plus_ms,
                    timestamp_ms,
                });
                list.sort_by_key(|e| e.precedence);
                true
            }
        }
    }

    pub fn observe_fc(
        &mut self,
        quantity: Quantity,
        value: f64,
        t_plus_ms: Option<i64>,
        timestamp_ms: i64,
    ) -> bool {
        let (slot, source) = match quantity {
            Quantity::Altitude => (&mut self.fc_altitude, "FC apogee (FC log)"),
            Quantity::Speed => (&mut self.fc_speed, "FC max velocity (FC log)"),
            Quantity::Acceleration => (&mut self.fc_acceleration, "FC max acceleration (FC log)"),
        };
        if slot.as_ref().is_some_and(|e| e.value == value) {
            return false;
        }
        *slot = Some(Extreme {
            value,
            source: source.into(),
            precedence: 0,
            t_plus_ms,
            timestamp_ms,
        });
        true
    }

    /// Preferred (highest-precedence) GS value per quantity, for recordings.
    pub fn preferred(&self) -> [Option<f64>; 3] {
        [
            self.altitude.first().map(|e| e.value),
            self.speed.first().map(|e| e.value),
            self.acceleration.first().map(|e| e.value),
        ]
    }
}

struct Tracker {
    maxima: FlightMaxima,
    dirty: bool,
    restored: bool,
    last_recorded_ms: i64,
    latest_altitude_m: Option<f64>,
}

static TRACKER: Mutex<Tracker> = Mutex::new(Tracker {
    maxima: FlightMaxima {
        flight_anchor_ms: None,
        tracking: false,
        altitude: Vec::new(),
        speed: Vec::new(),
        acceleration: Vec::new(),
        fc_altitude: None,
        fc_speed: None,
        fc_acceleration: None,
    },
    dirty: false,
    restored: false,
    last_recorded_ms: 0,
    latest_altitude_m: None,
});

static CHANNEL: LazyLock<watch::Sender<FlightMaxima>> =
    LazyLock::new(|| watch::channel(FlightMaxima::default()).0);

pub fn subscribe() -> watch::Receiver<FlightMaxima> {
    CHANNEL.subscribe()
}

pub fn snapshot() -> FlightMaxima {
    TRACKER.lock().unwrap().maxima.clone()
}

/// Latest altitude received from any vehicle source (any phase).
pub fn latest_altitude_m() -> Option<f64> {
    TRACKER.lock().unwrap().latest_altitude_m
}

fn state_path() -> PathBuf {
    std::env::var_os("GS_FLIGHT_MAXIMA_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("backend/data/flight-maxima.json"))
}

fn clock_context(state: &AppState) -> (LaunchClockKind, Option<i64>, Option<i64>) {
    let clock = state.launch_clock_snapshot();
    let t_plus = (clock.kind == LaunchClockKind::TPlus)
        .then(|| {
            clock.anchor_timestamp_ms.map(|anchor| {
                crate::telemetry_task::procedure_clock::timestamp_ms().saturating_sub(anchor)
            })
        })
        .flatten();
    (clock.kind, clock.anchor_timestamp_ms, t_plus)
}

/// Telemetry ingress hook (every cached row). Cheap for unrelated types.
pub fn observe_row(state: &AppState, row: &TelemetryRow) {
    let found = samples(row);
    if found.is_empty() {
        return;
    }
    let (kind, anchor, t_plus) = clock_context(state);
    let mut tracker = TRACKER.lock().unwrap();
    tracker.maxima.align(kind, anchor);
    for (quantity, value, source, precedence) in found {
        if quantity == Quantity::Altitude {
            tracker.latest_altitude_m = Some(value);
        }
        if tracker.maxima.observe(
            quantity,
            value,
            source,
            precedence,
            t_plus,
            row.timestamp_ms,
        ) {
            tracker.dirty = true;
        }
    }
}

/// FC message ingress hook.
pub fn observe_fc_message(state: &AppState, text: &str, timestamp_ms: i64) {
    let Some((quantity, value)) = parse_fc_message(text) else {
        return;
    };
    let (kind, anchor, t_plus) = clock_context(state);
    let mut tracker = TRACKER.lock().unwrap();
    tracker.maxima.align(kind, anchor);
    if tracker
        .maxima
        .observe_fc(quantity, value, t_plus, timestamp_ms)
    {
        tracker.dirty = true;
    }
}

/// T-clock reset (also run by Reset all boards): start a new flight.
pub fn reset() {
    let mut tracker = TRACKER.lock().unwrap();
    tracker.maxima = FlightMaxima::default();
    tracker.dirty = true;
    tracker.restored = true;
    drop(tracker);
    let _ = std::fs::remove_file(state_path());
}

/// Periodic publication: WS snapshot, persistence and the recording row.
pub fn tick(state: &AppState) {
    let (kind, anchor, _) = clock_context(state);
    let mut tracker = TRACKER.lock().unwrap();
    if !tracker.restored {
        tracker.restored = true;
        if let Ok(bytes) = std::fs::read(state_path())
            && let Ok(saved) = serde_json::from_slice::<FlightMaxima>(&bytes)
            && saved.flight_anchor_ms.is_some()
            && saved.flight_anchor_ms == anchor
        {
            tracker.maxima = saved;
            tracker.dirty = true;
        }
    }
    if tracker.maxima.align(kind, anchor) {
        tracker.dirty = true;
    }
    if !tracker.dirty {
        return;
    }
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    if now.saturating_sub(tracker.last_recorded_ms) < 1_000 {
        // Publish live at the tick rate; persist and record at most 1 Hz.
        CHANNEL.send_if_modified(|current| {
            let changed = *current != tracker.maxima;
            if changed {
                *current = tracker.maxima.clone();
            }
            changed
        });
        return;
    }
    tracker.dirty = false;
    tracker.last_recorded_ms = now;
    let maxima = tracker.maxima.clone();
    drop(tracker);
    CHANNEL.send_replace(maxima.clone());
    let path = state_path();
    if maxima.is_empty() && maxima.flight_anchor_ms.is_none() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Ok(json) = serde_json::to_vec_pretty(&maxima) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    let values = maxima.preferred();
    if values.iter().any(Option::is_some) {
        let values_json = serde_json::to_string(&values).ok();
        let _ = state
            .db_queue_tx
            .try_send(crate::telemetry_db::DbQueueItem::Write(
                crate::telemetry_db::DbWrite::Telemetry {
                    timestamp_ms: now,
                    source_timestamp_ms: None,
                    data_type: "GS_FLIGHT_MAXIMA".into(),
                    sender_id: "GS".into(),
                    values_json,
                    payload_json: serde_json::to_string(&maxima).unwrap_or_default(),
                },
            ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(data_type: &str, values: &[f32]) -> TelemetryRow {
        TelemetryRow {
            timestamp_ms: 1,
            data_type: data_type.into(),
            sender_id: "FC".into(),
            values: values.iter().copied().map(Some).collect(),
        }
    }

    #[test]
    fn rows_map_to_sources() {
        let s = samples(&row("ASCENT_STATE", &[1.0, 0.0, 0.0, 0.0, 812.0, -250.0]));
        assert_eq!(
            s,
            vec![
                (Quantity::Altitude, 812.0, "FC Kalman altitude", 0),
                (Quantity::Speed, 250.0, "FC Kalman velocity", 0)
            ]
        );
        let s = samples(&row("ACCEL_DATA", &[3.0, 4.0, 12.0]));
        assert_eq!(
            s,
            vec![(Quantity::Acceleration, 13.0, "Raw accelerometer", 0)]
        );
        assert_eq!(
            samples(&row("GPS_DATA", &[1.0, 2.0, 99.0]))[0].2,
            "GPS altitude"
        );
        assert!(samples(&row("GYRO_DATA", &[1.0, 2.0, 3.0])).is_empty());
    }

    #[test]
    fn tracks_only_during_t_plus_and_keeps_time_of_maximum() {
        let mut m = FlightMaxima::default();
        m.align(LaunchClockKind::TMinus, Some(100));
        assert!(!m.observe(Quantity::Altitude, 5.0, "Barometer altitude", 1, None, 1));
        m.align(LaunchClockKind::TPlus, Some(100));
        assert!(m.observe(
            Quantity::Altitude,
            500.0,
            "Barometer altitude",
            1,
            Some(10_000),
            2
        ));
        assert!(m.observe(
            Quantity::Altitude,
            900.0,
            "FC Kalman altitude",
            0,
            Some(20_000),
            3
        ));
        assert!(!m.observe(
            Quantity::Altitude,
            400.0,
            "Barometer altitude",
            1,
            Some(30_000),
            4
        ));
        assert_eq!(m.altitude[0].source, "FC Kalman altitude");
        assert_eq!(m.altitude[1].value, 500.0);
        assert_eq!(m.altitude[1].t_plus_ms, Some(10_000));
        // A new flight anchor clears the previous flight.
        assert!(m.align(LaunchClockKind::TPlus, Some(200)));
        assert!(m.altitude.is_empty());
    }

    #[test]
    fn parses_fc_maxima_messages() {
        assert_eq!(
            parse_fc_message("FC: EV apogee guess: 2810"),
            Some((Quantity::Altitude, 2810.0))
        );
        assert_eq!(
            parse_fc_message("EV maxvel guess: 251"),
            Some((Quantity::Speed, 251.0))
        );
        assert_eq!(parse_fc_message("EV new flight state: 7"), None);
        let mut m = FlightMaxima::default();
        assert!(m.observe_fc(Quantity::Altitude, 2810.0, Some(25_000), 9));
        assert!(!m.observe_fc(Quantity::Altitude, 2810.0, Some(25_000), 9));
        assert_eq!(m.fc_altitude.as_ref().unwrap().source, "FC apogee (FC log)");
    }

    #[test]
    fn persisted_maxima_round_trip() {
        let mut m = FlightMaxima::default();
        m.align(LaunchClockKind::TPlus, Some(42));
        m.observe(
            Quantity::Speed,
            250.0,
            "FC Kalman velocity",
            0,
            Some(4_000),
            5,
        );
        let json = serde_json::to_vec(&m).unwrap();
        let back: FlightMaxima = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.preferred(), [None, Some(250.0), None]);
    }
}
