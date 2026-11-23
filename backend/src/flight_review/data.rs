//! The rows a motor or flight review reads, from a saved recording (any
//! replay source) or from the station's live cache, in one shape.
use crate::types::TelemetryRow;
use sqlx::{Row as _, SqlitePool};

/// One telemetry row on network time (`source_timestamp_ms` when the row has
/// one, as the burn report reads it).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Row {
    pub t_ms: i64,
    pub data_type: String,
    pub sender: String,
    pub values: Vec<Option<f64>>,
    pub payload: Option<String>,
}

impl Row {
    pub(crate) fn v(&self, i: usize) -> Option<f64> {
        self.values.get(i).copied().flatten().filter(|v| v.is_finite())
    }
    pub(crate) fn from_fc(&self) -> bool {
        crate::types::canonical_sender_id(self.sender.trim()) == "FC"
    }
}

/// Everything one review reads: telemetry rows (time order), the message log
/// (FC messages and the station's deployment notices) and the station's own
/// flight-state changes.
#[derive(Clone, Debug, Default)]
pub(crate) struct Data {
    pub rows: Vec<Row>,
    pub messages: Vec<(i64, String)>,
    pub station_states: Vec<(i64, u8)>,
}

impl Data {
    pub(crate) fn of_type<'a>(&'a self, data_type: &'a str) -> impl Iterator<Item = &'a Row> + 'a {
        self.rows.iter().filter(move |r| r.data_type == data_type)
    }
    pub(crate) fn sort(&mut self) {
        self.rows.sort_by_key(|r| r.t_ms);
        self.messages.sort_by_key(|m| m.0);
        self.station_states.sort_by_key(|s| s.0);
    }
    pub(crate) fn first_ms(&self) -> Option<i64> {
        self.rows.first().map(|r| r.t_ms)
    }
    pub(crate) fn last_ms(&self) -> Option<i64> {
        self.rows.last().map(|r| r.t_ms)
    }
    /// The recording's context: the first GS_STATION_MODE row.
    pub(crate) fn station_mode(&self) -> Option<serde_json::Value> {
        self.of_type("GS_STATION_MODE")
            .find_map(|r| r.payload.as_deref().and_then(|p| serde_json::from_str(p).ok()))
    }
    /// Keeps only what falls in `[from, to]`.
    pub(crate) fn clip(&mut self, from_ms: Option<i64>, to_ms: Option<i64>) {
        let keep = |t: i64| from_ms.is_none_or(|f| t >= f) && to_ms.is_none_or(|e| t <= e);
        // The station-mode context rows are kept whatever their time.
        self.rows.retain(|r| keep(r.t_ms) || r.data_type == "GS_STATION_MODE");
        self.messages.retain(|m| keep(m.0));
        self.station_states.retain(|s| keep(s.0));
    }
}

/// Telemetry types the motor review reads (besides every `*TEMP*` type and
/// every DAQ SD row).
pub(crate) const MOTOR_TYPES: &[&str] = &[
    "LOADCELL_WEIGHT_KG",
    "LOADCELL_50_WEIGHT_KG",
    "KG1000",
    "KG50",
    "DAQ_KG50_SELECTED",
    "DAQ_LOADCELL_CHAMBER",
    "PRESSURE_TRANSDUCER_CALIBRATED",
    "OX_TANK_PRESSURE",
    "FUEL_TANK_PRESSURE",
    "COMBUSTION_CHAMBER_PRESSURE",
    "COMBUSTION_CHAMBER_VOLTAGE",
    "VALVE_STATE",
    crate::recording_replay::sources::THRUST_TYPE,
];

/// Telemetry types the flight review reads (besides every `SIM_*` type).
pub(crate) const FLIGHT_TYPES: &[&str] = &[
    "FLIGHT_STATE",
    "ASCENT_STATE",
    "DESCENT_STATE",
    "BAROMETER_DATA",
    "GPS_DATA",
    "ACCEL_DATA",
    "IMU_DATA",
    "VEHICLE_SPEED",
    "LAUNCH_SITE",
    "GS_FC_PHASE",
    "GS_FLIGHT_MAXIMA",
];

const CONTEXT_TYPES: &[&str] = &["GS_STATION_MODE", "GS_IGT"];

/// Rows read from one source at most (a long rolling range is cut here).
const MAX_ROWS: i64 = 4_000_000;

fn wanted(data_type: &str, sender: &str) -> bool {
    MOTOR_TYPES.contains(&data_type)
        || FLIGHT_TYPES.contains(&data_type)
        || CONTEXT_TYPES.contains(&data_type)
        || data_type.starts_with("SIM_")
        || data_type.contains("TEMP")
        || data_type.starts_with("DAQ_SD_")
        || sender == "DAQ_SD"
}

fn parse_values(json: Option<&str>) -> Vec<Option<f64>> {
    json.and_then(|j| serde_json::from_str::<Vec<Option<f64>>>(j).ok())
        .unwrap_or_default()
}

/// Reads a review's rows from a replay source database, between `from_ms`
/// and `to_ms` (network time) when given. The station-mode context rows are
/// read whatever their time.
pub(crate) async fn read_pool(db: &SqlitePool, from_ms: Option<i64>, to_ms: Option<i64>) -> anyhow::Result<Data> {
    let mut types: Vec<String> = MOTOR_TYPES
        .iter()
        .chain(FLIGHT_TYPES)
        .chain(CONTEXT_TYPES)
        .map(|s| s.to_string())
        .collect();
    types = crate::telemetry_schema::expand_type_filter(&types);
    let list = types.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(",");
    // Receipt time bounds the indexed scan; the source time is filtered after.
    let slack = 120_000;
    let lo = from_ms.map_or(i64::MIN, |f| f.saturating_sub(slack));
    let hi = to_ms.map_or(i64::MAX, |t| t.saturating_add(slack));
    let sql = format!(
        "SELECT COALESCE(source_timestamp_ms, timestamp_ms) AS t, data_type, COALESCE(sender_id,'') AS sender, values_json, payload_json \
         FROM telemetry WHERE (data_type IN ({list}) OR data_type LIKE 'SIM\\_%' ESCAPE '\\' OR data_type LIKE '%TEMP%' \
         OR data_type LIKE 'DAQ\\_SD\\_%' ESCAPE '\\' OR sender_id = 'DAQ_SD') \
         AND (timestamp_ms BETWEEN ? AND ? OR data_type = 'GS_STATION_MODE') ORDER BY t LIMIT ?"
    );
    // Only the fixed type list above is formatted in; the bounds are bound.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(lo)
        .bind(hi)
        .bind(MAX_ROWS)
        .fetch_all(db)
        .await?;
    let mut data = Data::default();
    for row in rows {
        let data_type: String = row.try_get("data_type")?;
        let sender: String = row.try_get("sender")?;
        if !wanted(&data_type, &sender) {
            continue;
        }
        let values_json: Option<String> = row.try_get("values_json").ok().flatten();
        data.rows.push(Row {
            t_ms: row.try_get("t")?,
            data_type: crate::telemetry_schema::canonical_type_name(&data_type).to_owned(),
            sender,
            values: parse_values(values_json.as_deref()),
            payload: row.try_get("payload_json").ok().flatten(),
        });
    }
    let (mlo, mhi) = (from_ms.unwrap_or(i64::MIN), to_ms.unwrap_or(i64::MAX));
    if let Ok(messages) = sqlx::query("SELECT timestamp_ms, message FROM messages WHERE timestamp_ms BETWEEN ? AND ? ORDER BY timestamp_ms")
        .bind(mlo.saturating_sub(slack))
        .bind(mhi.saturating_add(slack))
        .fetch_all(db)
        .await
    {
        for row in messages {
            data.messages.push((row.try_get("timestamp_ms")?, row.try_get("message")?));
        }
    }
    if let Ok(states) =
        sqlx::query("SELECT timestamp_ms, f_state FROM flight_state WHERE timestamp_ms BETWEEN ? AND ? ORDER BY timestamp_ms")
            .bind(mlo.saturating_sub(slack))
            .bind(mhi.saturating_add(slack))
            .fetch_all(db)
            .await
    {
        for row in states {
            let code: i64 = row.try_get("f_state")?;
            if let Ok(code) = u8::try_from(code) {
                data.station_states.push((row.try_get("timestamp_ms")?, code));
            }
        }
    }
    data.sort();
    data.clip(from_ms, to_ms);
    Ok(data)
}

/// A review's rows from the live cache (rows at or after `from_ms`).
pub(crate) fn from_cache<'a>(rows: impl IntoIterator<Item = &'a TelemetryRow>, from_ms: i64) -> Data {
    let mut data = Data::default();
    for row in rows.into_iter().filter(|r| r.timestamp_ms >= from_ms) {
        let data_type = crate::telemetry_schema::canonical_type_name(&row.data_type);
        if !wanted(data_type, &row.sender_id) {
            continue;
        }
        data.rows.push(Row {
            t_ms: row.timestamp_ms,
            data_type: data_type.to_owned(),
            sender: row.sender_id.clone(),
            values: row.values.iter().map(|v| v.map(f64::from)).collect(),
            payload: None,
        });
    }
    data.sort();
    data
}

// ------------------------------------------------------------------ live log

/// Flight events the live cache does not hold: FC messages, the station's
/// deployment notices, the FC's detailed phase and the station's flight
/// state. RAM only, bounded; saved recordings hold the same in their
/// `messages` / `flight_state` tables and `GS_FC_PHASE` rows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LiveEntry {
    Message(String),
    FcPhase(u8),
    Station(u8),
}

static LIVE_LOG: std::sync::Mutex<std::collections::VecDeque<(i64, LiveEntry)>> = std::sync::Mutex::new(std::collections::VecDeque::new());
const LIVE_LOG_MAX: usize = 1024;

pub(crate) fn note(t_ms: i64, entry: LiveEntry) {
    let mut log = LIVE_LOG.lock().unwrap();
    // States are logged on change only.
    let same_kind = |e: &LiveEntry| std::mem::discriminant(e) == std::mem::discriminant(&entry);
    if !matches!(entry, LiveEntry::Message(_)) && log.iter().rev().find(|(_, e)| same_kind(e)).is_some_and(|(_, e)| *e == entry) {
        return;
    }
    log.push_back((t_ms, entry));
    while log.len() > LIVE_LOG_MAX {
        log.pop_front();
    }
}

/// Adds the live log entries at or after `from_ms` to `data`.
pub(crate) fn add_live_log(data: &mut Data, from_ms: i64) {
    let log = LIVE_LOG.lock().unwrap();
    for (t, entry) in log.iter().filter(|(t, _)| *t >= from_ms) {
        match entry {
            LiveEntry::Message(text) => data.messages.push((*t, text.clone())),
            LiveEntry::FcPhase(code) => data.rows.push(Row {
                t_ms: *t,
                data_type: "GS_FC_PHASE".into(),
                sender: "GS".into(),
                values: vec![Some(f64::from(*code))],
                payload: None,
            }),
            LiveEntry::Station(code) => data.station_states.push((*t, *code)),
        }
    }
    drop(log);
    data.sort();
}
