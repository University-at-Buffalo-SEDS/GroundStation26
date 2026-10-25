//! Bounded position/optional signal-strength snapshots for antenna alignment.
//! Reads GPSD only when explicitly configured; never queries radio AT mode.
use crate::types::TelemetryRow;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[path = "antenna_positions.rs"]
pub(crate) mod positions;

const FIX_FRESH_MS: u64 = 15_000;
const UTC_FLOOR_MS: u64 = 946_684_800_000; // 2000-01-01 UTC
const RSSI_FRESH_MS: u64 = 3_000;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AltitudeReference {
    MeanSeaLevel,
    Ellipsoid,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fix {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: Option<f64>,
    pub altitude_reference: AltitudeReference,
    pub accuracy_m: Option<f64>,
    pub geoid_separation_m: Option<f64>,
    pub age_ms: u64,
    pub stale: bool,
    pub source: String,
    /// Static administrator-saved fallback, not a live GPS solution.
    #[serde(default)]
    pub manual: bool,
}
impl Fix {
    fn valid(&self) -> bool {
        self.latitude.is_finite()
            && (-90.0..=90.0).contains(&self.latitude)
            && self.longitude.is_finite()
            && (-180.0..=180.0).contains(&self.longitude)
            && self.altitude_m.is_none_or(f64::is_finite)
            && self.accuracy_m.is_none_or(|v| v.is_finite() && v >= 0.0)
            && self.geoid_separation_m.is_none_or(f64::is_finite)
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct SignalStrength {
    pub value: f64,
    pub unit: &'static str,
    pub source: String,
    pub age_ms: u64,
}
#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub rocket: Option<Fix>,
    pub ground_station: Option<Fix>,
    pub rrls: Option<Fix>,
    pub rssi: Option<SignalStrength>,
    pub gpsd_configured: bool,
}
struct TimedFix {
    fix: Fix,
    received: Instant,
    received_model_ms: Option<u64>,
    publication_ms: Option<u64>,
}
impl TimedFix {
    fn snapshot(&self) -> Fix {
        self.snapshot_at(Instant::now(), crate::simulation_clock_ms())
    }
    fn snapshot_at(&self, wall: Instant, model_ms: Option<u64>) -> Fix {
        let elapsed = match (self.received_model_ms, model_ms) {
            (Some(received), Some(now)) if now >= received => now - received,
            // A model reset/lost clock cannot make an old sample young again.
            (Some(_), _) => u64::MAX,
            (None, _) => wall
                .saturating_duration_since(self.received)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        };
        let mut value = self.fix.clone();
        value.age_ms = value.age_ms.saturating_add(elapsed);
        value.stale = value.age_ms > FIX_FRESH_MS;
        value
    }
}
#[derive(Clone, Copy)]
struct NoFix {
    publication_ms: u64,
    network_utc_ms: Option<u64>,
}
fn comparable_publications(left: u64, right: u64) -> bool {
    (left >= UTC_FLOOR_MS) == (right >= UTC_FLOOR_MS)
}
impl NoFix {
    fn precedes(self, publication_ms: u64) -> bool {
        if comparable_publications(self.publication_ms, publication_ms) {
            return publication_ms > self.publication_ms;
        }
        // RF can acquire GPS UTC after starting with a boot-relative clock.
        // Order that transition only against a known UTC receipt bound; never
        // subtract unlike publication domains or accept a delayed pre-loss fix.
        publication_ms >= UTC_FLOOR_MS
            && self
                .network_utc_ms
                .is_some_and(|now| now >= UTC_FLOOR_MS && publication_ms > now)
    }
}
#[derive(Default)]
struct Cache {
    rocket: [Option<TimedFix>; 2],
    rf_no_fix: Option<NoFix>,
    local: Option<TimedFix>,
    remote: Option<TimedFix>,
    signal: Option<(SignalStrength, Instant)>,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// Publication age is comparable only when both timestamps are Unix UTC.
/// Boot-relative/unset source clocks retain receipt-age behavior; never subtract
/// them from the synchronized network epoch.
fn source_fix_age_ms(source_ms: Option<u64>, network_utc_ms: Option<u64>) -> Option<u64> {
    const FUTURE_SKEW_MS: u64 = 5_000;
    match (source_ms, network_utc_ms) {
        (Some(source), Some(now)) if source >= UTC_FLOOR_MS && now >= UTC_FLOOR_MS => {
            if source > now.saturating_add(FUTURE_SKEW_MS) {
                return None;
            }
            let age = now.saturating_sub(source);
            (age <= FIX_FRESH_MS).then_some(age)
        }
        _ => Some(0),
    }
}

pub(crate) fn observe_source_gps(row: &TelemetryRow, source_ms: u64) {
    let network_utc_ms =
        crate::telemetry_task::recording_network_utc_ms().and_then(|value| value.try_into().ok());
    observe_at(row, Some(source_ms), network_utc_ms);
}

/// A positive satellite count alone never establishes a receiver lock.
/// RF publishes zero when its satellite solution is absent or stale.
pub(crate) fn observe_source_satellites(sender: &str, count: u8, source_ms: u64) {
    let network_utc_ms =
        crate::telemetry_task::recording_network_utc_ms().and_then(|value| value.try_into().ok());
    observe_satellites_at(sender, count, source_ms, network_utc_ms);
}
fn observe_satellites_at(sender: &str, count: u8, source_ms: u64, network_utc_ms: Option<u64>) {
    if sender != "RF" || count != 0 || source_fix_age_ms(Some(source_ms), network_utc_ms).is_none()
    {
        return;
    }
    let mut cache = cache().lock().unwrap();
    let older_than =
        |previous| comparable_publications(source_ms, previous) && source_ms < previous;
    if cache.rocket[0]
        .as_ref()
        .and_then(|fix| fix.publication_ms)
        .is_some_and(older_than)
        || cache
            .rf_no_fix
            .is_some_and(|lost| older_than(lost.publication_ms))
    {
        return;
    }
    cache.rocket[0] = None;
    cache.rf_no_fix = Some(NoFix {
        publication_ms: source_ms,
        network_utc_ms,
    });
}

pub fn observe(row: &TelemetryRow) {
    observe_at(row, None, None);
}

fn observe_at(row: &TelemetryRow, source_ms: Option<u64>, network_utc_ms: Option<u64>) {
    if row.data_type == "GPS_DATA" {
        let index = match row.sender_id.as_str() {
            "RF" => 0,
            "FC" => 1,
            _ => return,
        };
        let Some(age_ms) = source_fix_age_ms(source_ms, network_utc_ms) else {
            return;
        };
        let value = |i: usize| row.values.get(i).copied().flatten().map(f64::from);
        if let (Some(latitude), Some(longitude)) = (value(0), value(1)) {
            let fix = Fix {
                latitude,
                longitude,
                altitude_m: value(2),
                altitude_reference: AltitudeReference::MeanSeaLevel,
                accuracy_m: None,
                geoid_separation_m: None,
                age_ms,
                stale: false,
                source: row.sender_id.clone(),
                manual: false,
            };
            if fix.valid() {
                let mut cache = cache().lock().unwrap();
                if index == 0
                    && let Some(mut lost) = cache.rf_no_fix
                {
                    // Anchor a newly available UTC domain once. Repeated
                    // arrivals cannot move the fence and chase recovery forever.
                    if lost.network_utc_ms.is_none()
                        && source_ms.is_some_and(|source| {
                            source >= UTC_FLOOR_MS
                                && !comparable_publications(source, lost.publication_ms)
                        })
                        && network_utc_ms.is_some_and(|now| now >= UTC_FLOOR_MS)
                    {
                        lost.network_utc_ms = network_utc_ms;
                        cache.rf_no_fix = Some(lost);
                    }
                    if !source_ms.is_some_and(|source| lost.precedes(source)) {
                        return;
                    }
                }
                if index == 0
                    && source_ms.is_some_and(|source| {
                        cache.rocket[0]
                            .as_ref()
                            .and_then(|previous| previous.publication_ms)
                            .is_some_and(|previous| {
                                comparable_publications(source, previous) && source < previous
                            })
                    })
                {
                    return;
                }
                cache.rocket[index] = Some(TimedFix {
                    fix,
                    received: Instant::now(),
                    received_model_ms: crate::simulation_clock_ms(),
                    publication_ms: source_ms,
                });
            }
        }
    } else if matches!(
        row.data_type.as_str(),
        "RSSI_DBM" | "RFD_RSSI_DBM" | "RSSI" | "RFD_RSSI"
    ) {
        if let Some(value) = row
            .values
            .first()
            .copied()
            .flatten()
            .map(f64::from)
            .filter(|v| v.is_finite())
        {
            let unit = if row.data_type.ends_with("_DBM") {
                "dBm"
            } else {
                "raw"
            };
            cache().lock().unwrap().signal = Some((
                SignalStrength {
                    value,
                    unit,
                    source: row.sender_id.clone(),
                    age_ms: 0,
                },
                Instant::now(),
            ));
        }
    }
}
pub fn local_fix() -> Option<Fix> {
    cache()
        .lock()
        .unwrap()
        .local
        .as_ref()
        .map(TimedFix::snapshot)
}
pub fn accept_remote_fix(mut fix: Fix) {
    if !fix.valid() || fix.age_ms > FIX_FRESH_MS {
        return;
    }
    fix.source = if crate::rrls::is_rrls() {
        "GroundStation GPS"
    } else {
        "RRLS GPS"
    }
    .to_string();
    cache().lock().unwrap().remote = Some(TimedFix {
        fix,
        received: Instant::now(),
        received_model_ms: None,
        publication_ms: None,
    });
}
pub fn snapshot() -> Snapshot {
    let cache = cache().lock().unwrap();
    let rocket = cache
        .rocket
        .iter()
        .flatten()
        .map(TimedFix::snapshot)
        .min_by_key(|fix| fix.age_ms);
    let local = cache.local.as_ref().map(TimedFix::snapshot);
    let remote = cache.remote.as_ref().map(TimedFix::snapshot);
    let (ground_station, rrls) = if crate::rrls::is_rrls() {
        (remote, local)
    } else {
        (local, remote)
    };
    let rssi = cache.signal.as_ref().and_then(|(value, received)| {
        let age = received.elapsed().as_millis() as u64;
        (age <= RSSI_FRESH_MS).then(|| {
            let mut v = value.clone();
            v.age_ms = age;
            v
        })
    });
    drop(cache);
    let (saved_gs, saved_rrls) = positions::fallbacks();
    let choose = |live: Option<Fix>, saved: Option<Fix>| {
        live.as_ref()
            .filter(|f| !f.stale)
            .cloned()
            .or(saved)
            .or(live)
    };
    Snapshot {
        rocket,
        ground_station: choose(ground_station, saved_gs),
        rrls: choose(rrls, saved_rrls),
        rssi,
        gpsd_configured: std::env::var_os("GS_GPSD_ADDR").is_some(),
    }
}

fn gpsd_fix(line: &[u8]) -> Option<Fix> {
    gpsd_fix_at(line, time::OffsetDateTime::now_utc())
}

fn gpsd_fix_at(line: &[u8], now: time::OffsetDateTime) -> Option<Fix> {
    let data: serde_json::Value = serde_json::from_slice(line).ok()?;
    if data.get("class")?.as_str()? != "TPV" || data.get("mode")?.as_u64()? < 2 {
        return None;
    }
    let number = |name| {
        data.get(name)
            .and_then(serde_json::Value::as_f64)
            .filter(|v| v.is_finite())
    };
    // A 2-D solution cannot establish antenna elevation, even if GPSD includes
    // an altitude retained from an earlier 3-D solution.
    let has_altitude = data.get("mode")?.as_u64()? >= 3;
    let msl = has_altitude.then(|| number("altMSL")).flatten();
    let hae = has_altitude.then(|| number("altHAE")).flatten();
    let age_ms = match data.get("time") {
        Some(value) => {
            let recorded = time::OffsetDateTime::parse(
                value.as_str()?,
                &time::format_description::well_known::Rfc3339,
            )
            .ok()?;
            let elapsed = (now - recorded).whole_milliseconds();
            // Reject old fixes and a clock mismatch; do not make them fresh
            // merely because GPSD just sent them again.
            if elapsed < -5_000 || elapsed > i128::from(FIX_FRESH_MS) {
                return None;
            }
            elapsed.max(0) as u64
        }
        None => 0,
    };
    let fix = Fix {
        latitude: number("lat")?,
        longitude: number("lon")?,
        altitude_m: msl.or(hae),
        altitude_reference: if msl.is_some() {
            AltitudeReference::MeanSeaLevel
        } else if hae.is_some() {
            AltitudeReference::Ellipsoid
        } else {
            AltitudeReference::Unknown
        },
        accuracy_m: match (number("epx"), number("epy")) {
            (Some(x), Some(y)) if x >= 0.0 && y >= 0.0 => Some(x.hypot(y)),
            _ => None,
        },
        geoid_separation_m: msl.zip(hae).map(|(m, h)| h - m),
        age_ms,
        stale: false,
        source: "Attached GPS".into(),
        manual: false,
    };
    fix.valid().then_some(fix)
}

/// GPSD is opt-in so an absent GPS cannot delay startup or existing comms.
pub fn start_gpsd(
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    let address = std::env::var("GS_GPSD_ADDR").ok()?;
    Some(tokio::spawn(async move {
        loop {
            let session = async {
                let mut socket = tokio::time::timeout(
                    Duration::from_secs(3),
                    tokio::net::TcpStream::connect(&address),
                )
                .await??;
                socket
                    .write_all(b"?WATCH={\"enable\":true,\"json\":true};\n")
                    .await?;
                let mut buffer = [0u8; 2048];
                let mut line = Vec::with_capacity(4096);
                loop {
                    let count =
                        tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buffer))
                            .await??;
                    if count == 0 {
                        return Err::<(), anyhow::Error>(anyhow::anyhow!("GPSD disconnected"));
                    }
                    for &byte in &buffer[..count] {
                        if byte == b'\n' {
                            if let Some(fix) = gpsd_fix(&line) {
                                cache().lock().unwrap().local = Some(TimedFix {
                                    fix,
                                    received: Instant::now(),
                                    received_model_ms: None,
                                    publication_ms: None,
                                });
                            }
                            line.clear();
                        } else {
                            if line.len() >= 4096 {
                                anyhow::bail!("GPSD line exceeds bounded input buffer");
                            }
                            line.push(byte);
                        }
                    }
                }
            };
            tokio::select! {
                _=shutdown.recv()=>break,
                result=session=>{if let Err(error)=result {log::debug!("antenna GPSD session: {error}");}}
            }
            tokio::select! {_=shutdown.recv()=>break,_=tokio::time::sleep(Duration::from_secs(3))=>{}}
        }
    }))
}

// Fixed-size application-level RRLS position exchange. Age is relative, so
// different host UTC offsets cannot silently turn an old fix into a fresh one.
pub fn encode_position(fix: &Fix) -> Option<[u8; 38]> {
    if !fix.valid() || fix.manual || fix.age_ms > FIX_FRESH_MS {
        return None;
    }
    let mut out = [0; 38];
    out[0] = 1;
    out[1..9].copy_from_slice(&fix.latitude.to_le_bytes());
    out[9..17].copy_from_slice(&fix.longitude.to_le_bytes());
    out[17..25].copy_from_slice(&fix.altitude_m.unwrap_or(f64::NAN).to_le_bytes());
    out[25] = match fix.altitude_reference {
        AltitudeReference::Unknown => 0,
        AltitudeReference::MeanSeaLevel => 1,
        AltitudeReference::Ellipsoid => 2,
    };
    out[26..30].copy_from_slice(&(fix.accuracy_m.unwrap_or(f64::NAN) as f32).to_le_bytes());
    out[30..34].copy_from_slice(&(fix.geoid_separation_m.unwrap_or(f64::NAN) as f32).to_le_bytes());
    out[34..38].copy_from_slice(&(fix.age_ms as u32).to_le_bytes());
    Some(out)
}
pub fn decode_position(data: &[u8]) -> Option<Fix> {
    if data.len() != 38 || data[0] != 1 {
        return None;
    }
    let f64_at = |i| Some(f64::from_le_bytes(data.get(i..i + 8)?.try_into().ok()?));
    let f32_at = |i| Some(f32::from_le_bytes(data.get(i..i + 4)?.try_into().ok()?) as f64);
    let fix = Fix {
        latitude: f64_at(1)?,
        longitude: f64_at(9)?,
        altitude_m: f64_at(17).filter(|v| v.is_finite()),
        altitude_reference: match data[25] {
            0 => AltitudeReference::Unknown,
            1 => AltitudeReference::MeanSeaLevel,
            2 => AltitudeReference::Ellipsoid,
            _ => return None,
        },
        accuracy_m: f32_at(26).filter(|v| v.is_finite()),
        geoid_separation_m: f32_at(30).filter(|v| v.is_finite()),
        age_ms: u32::from_le_bytes(data[34..38].try_into().ok()?) as u64,
        stale: false,
        source: "Radio GPS".into(),
        manual: false,
    };
    (fix.valid() && fix.age_ms <= FIX_FRESH_MS).then_some(fix)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gpsd_rejects_missing_invalid_and_no_fix_and_preserves_altitude_reference() {
        assert!(gpsd_fix(br#"{"class":"TPV","mode":1,"lat":0,"lon":0}"#).is_none());
        assert!(gpsd_fix(br#"{"class":"TPV","mode":3,"lat":91,"lon":0}"#).is_none());
        let fix=gpsd_fix(br#"{"class":"TPV","mode":3,"lat":43,"lon":-78,"altMSL":150,"altHAE":180,"epx":3,"epy":4}"#).unwrap();
        assert_eq!(fix.altitude_m, Some(150.0));
        assert_eq!(fix.geoid_separation_m, Some(30.0));
        assert_eq!(fix.accuracy_m, Some(5.0));
        let fix = gpsd_fix(br#"{"class":"TPV","mode":2,"lat":0,"lon":0}"#).unwrap();
        assert_eq!(fix.altitude_m, None);
        assert_eq!(fix.altitude_reference, AltitudeReference::Unknown);
    }
    #[test]
    fn gpsd_preserves_fix_age_and_ignores_altitude_without_a_3d_solution() {
        let now = time::macros::datetime!(2026-10-15 12:00 UTC);
        let sample = br#"{"class":"TPV","mode":2,"lat":43,"lon":-78,"altMSL":150,"altHAE":180,"time":"2026-10-15T11:59:55Z"}"#;
        let fix = gpsd_fix_at(sample, now).unwrap();
        assert_eq!(fix.age_ms, 5_000);
        assert_eq!(fix.altitude_m, None);
        assert_eq!(fix.geoid_separation_m, None);
        assert_eq!(fix.altitude_reference, AltitudeReference::Unknown);
        assert!(gpsd_fix_at(sample, now + time::Duration::seconds(11)).is_none());
        assert!(gpsd_fix_at(sample, now - time::Duration::seconds(11)).is_none());
        let timed = TimedFix {
            fix,
            received: Instant::now() - Duration::from_secs(11),
            received_model_ms: None,
            publication_ms: None,
        };
        assert!(timed.snapshot().stale);
    }
    #[test]
    fn network_gps_publication_age_requires_comparable_utc_and_preserves_remaining_life() {
        let now = 1_767_225_600_000;
        assert_eq!(source_fix_age_ms(Some(now - 15_001), Some(now)), None);
        assert_eq!(
            source_fix_age_ms(Some(now - 15_000), Some(now)),
            Some(15_000)
        );
        assert_eq!(source_fix_age_ms(Some(now - 1_200), Some(now)), Some(1_200));
        assert_eq!(source_fix_age_ms(Some(now + 5_001), Some(now)), None);
        assert_eq!(source_fix_age_ms(Some(now + 5_000), Some(now)), Some(0));
        assert_eq!(source_fix_age_ms(Some(123), Some(now)), Some(0));
        assert_eq!(source_fix_age_ms(Some(now), None), Some(0));
        assert_eq!(source_fix_age_ms(None, Some(now)), Some(0));
    }
    #[test]
    fn rocket_fix_ages_in_model_time_without_wall_acceleration_or_reset_revival() {
        let wall = Instant::now();
        let fix = gpsd_fix(br#"{"class":"TPV","mode":3,"lat":43,"lon":-78,"altMSL":150}"#).unwrap();
        let mut timed = TimedFix {
            fix,
            received: wall,
            received_model_ms: Some(1000),
            publication_ms: None,
        };
        timed.fix.age_ms = 1200;
        assert_eq!(
            timed
                .snapshot_at(wall + Duration::from_secs(600), Some(1000))
                .age_ms,
            1200
        );
        assert!(
            !timed
                .snapshot_at(wall + Duration::from_secs(600), Some(14_800))
                .stale
        );
        assert!(timed.snapshot_at(wall, Some(14_801)).stale);
        assert!(timed.snapshot_at(wall, Some(999)).stale);
        assert!(timed.snapshot_at(wall, None).stale);
        timed.received_model_ms = None;
        assert_eq!(
            timed
                .snapshot_at(wall + Duration::from_millis(500), Some(99_000))
                .age_ms,
            1700
        );
    }
    #[test]
    fn delayed_gps_does_not_establish_or_replace_live_receiver_lock() {
        const CHILD: &str = "GS_ANTENNA_SOURCE_AGE_TEST_CHILD";
        const NAME: &str =
            "antenna::tests::delayed_gps_does_not_establish_or_replace_live_receiver_lock";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env_remove("GS_SIM_CLOCK_FILE")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        clear_rocket_fix_for_test();
        let now = 1_767_225_600_000;
        let mut row = TelemetryRow {
            timestamp_ms: now as i64,
            data_type: "GPS_DATA".into(),
            sender_id: "RF".into(),
            values: vec![Some(43.0), Some(-78.0), Some(150.0)],
        };
        observe_at(&row, Some(now - 15_001), Some(now));
        assert!(cache().lock().unwrap().rocket[0].is_none());
        observe_at(&row, Some(now - 1_200), Some(now));
        row.values[0] = Some(44.0);
        observe_at(&row, Some(now - 15_001), Some(now));
        let locked = cache().lock().unwrap();
        let value = locked.rocket[0].as_ref().unwrap().snapshot();
        assert_eq!(value.latitude, 43.0);
        assert!(value.age_ms >= 1200 && value.age_ms < 2000);
        drop(locked);
        clear_rocket_fix_for_test();
        row.sender_id = "FC".into();
        observe_at(&row, Some(now), Some(now));
        assert!(cache().lock().unwrap().rocket[0].is_none());
    }
    #[test]
    fn rf_zero_satellites_revokes_lock_and_source_order_controls_recovery() {
        const CHILD: &str = "GS_ANTENNA_NO_FIX_TEST_CHILD";
        const NAME: &str =
            "antenna::tests::rf_zero_satellites_revokes_lock_and_source_order_controls_recovery";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env_remove("GS_SIM_CLOCK_FILE")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let now = 1_767_225_600_000;
        let row = TelemetryRow {
            timestamp_ms: now as i64,
            data_type: "GPS_DATA".into(),
            sender_id: "RF".into(),
            values: vec![Some(43.0), Some(-78.0), Some(150.0)],
        };
        let locked = || cache().lock().unwrap().rocket[0].is_some();
        clear_rocket_fix_for_test();
        observe_satellites_at("RF", 12, now, Some(now));
        assert!(
            !locked(),
            "satellite count alone cannot prove a position lock"
        );
        observe_at(&row, Some(now), Some(now));
        assert!(locked());
        observe_satellites_at("FC", 0, now + 1, Some(now));
        assert!(locked(), "other boards cannot revoke RF lock");
        observe_satellites_at("RF", 0, now - 1, Some(now));
        assert!(locked(), "delayed no-fix cannot revoke newer RF GPS");
        observe_satellites_at("RF", 0, now - 15_001, Some(now));
        assert!(locked(), "stale zero must not replace current state");
        observe_satellites_at("RF", 0, now + 1, Some(now));
        assert!(
            !locked(),
            "current RF zero revokes the cached fix immediately"
        );
        for source in [now - 1, now, now + 1] {
            observe_at(&row, Some(source), Some(now));
            assert!(!locked(), "old/equal GPS publication cannot revive no-fix");
        }
        observe(&row);
        assert!(
            !locked(),
            "receipt-only cache cannot bypass publication watermark"
        );
        observe_at(&row, Some(now + 2), Some(now));
        assert!(locked(), "newer valid RF GPS recovers normally");
        observe_satellites_at("RF", 0, now + 1, Some(now));
        assert!(locked());

        clear_rocket_fix_for_test();
        observe_at(&row, Some(200), Some(now));
        observe_satellites_at("RF", 0, 210, Some(now));
        observe_at(&row, Some(209), Some(now));
        assert!(!locked());
        observe_at(&row, Some(211), Some(now));
        assert!(
            locked(),
            "boot-relative publications are compared only with each other"
        );

        clear_rocket_fix_for_test();
        observe_satellites_at("RF", 0, 210, Some(now));
        observe_at(&row, Some(now - 1), Some(now));
        assert!(
            !locked(),
            "cross-domain old UTC cannot bypass the synchronized loss bound"
        );
        observe_at(&row, Some(now + 1), Some(now));
        assert!(
            locked(),
            "fresh UTC acquisition can follow boot-relative no-fix"
        );
        clear_rocket_fix_for_test();
        observe_satellites_at("RF", 0, 210, None);
        observe_at(&row, Some(now - 1), None);
        assert!(
            !locked(),
            "unorderable domain transition is not proof of recovery"
        );
        observe_at(&row, Some(now - 1), Some(now));
        assert!(
            !locked(),
            "old UTC cannot cross the newly established UTC fence"
        );
        observe_at(&row, Some(now + 1), Some(now + 2));
        assert!(
            locked(),
            "a fixed UTC fence permits later valid recovery without another zero report"
        );
    }

    #[test]
    fn mesh_position_roundtrip_rejects_bad_version_coordinates_age_and_datum() {
        let fix = gpsd_fix(br#"{"class":"TPV","mode":3,"lat":43,"lon":-78,"altMSL":150}"#).unwrap();
        let encoded = encode_position(&fix).unwrap();
        let decoded = decode_position(&encoded).unwrap();
        assert_eq!(decoded.latitude, 43.0);
        assert_eq!(decoded.altitude_reference, AltitudeReference::MeanSeaLevel);
        let mut bad = encoded;
        bad[0] = 2;
        assert!(decode_position(&bad).is_none());
        bad = encoded;
        bad[25] = 3;
        assert!(decode_position(&bad).is_none());
        bad = encoded;
        bad[1..9].copy_from_slice(&f64::NAN.to_le_bytes());
        assert!(decode_position(&bad).is_none());
        bad = encoded;
        bad[34..38].copy_from_slice(&15_001u32.to_le_bytes());
        assert!(decode_position(&bad).is_none());
    }
}

/// RF only emits GPS_DATA when gps_has_fix() is true. FC's GPS_DATA contains
/// filtered/relative coordinates and cannot prove a current receiver lock.
#[cfg(not(feature = "hitl_mode"))]
pub fn rocket_launch_fix_locked() -> bool {
    cache().lock().unwrap().rocket[0]
        .as_ref()
        .is_some_and(|value| {
            let fix = value.snapshot();
            fix.valid() && !fix.stale && fix.altitude_m.is_some()
        })
}

#[cfg(test)]
pub(crate) fn clear_rocket_fix_for_test() {
    let mut cache = cache().lock().unwrap();
    cache.rocket[0] = None;
    cache.rf_no_fix = None;
}
