//! Merging the DAQ SD launch file with the station's live telemetry.
//!
//! Both are on network time (the DAQ's rows carry `time_source = network`,
//! the station's rows their source timestamp). Per channel, the preferred
//! source's samples are kept as they are and the other source fills only
//! the stretches the preferred one does not cover. Every sample keeps its
//! provenance.
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    DaqSd,
    GsTelemetry,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub t_ms: i64,
    pub value: f64,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Channel {
    Kg1000,
    Kg50,
    Tank,
    Chamber,
}

impl Channel {
    pub(crate) const ALL: [Channel; 4] = [
        Channel::Kg1000,
        Channel::Kg50,
        Channel::Tank,
        Channel::Chamber,
    ];
    /// GS telemetry data type (and sender) of each channel.
    pub(crate) fn gs_type(self) -> (&'static str, &'static str) {
        match self {
            Self::Kg1000 => ("LOADCELL_WEIGHT_KG", "DAQ"),
            Self::Kg50 => ("LOADCELL_50_WEIGHT_KG", "DAQ"),
            Self::Tank => ("PRESSURE_TRANSDUCER_CALIBRATED", "VB"),
            Self::Chamber => ("COMBUSTION_CHAMBER_PRESSURE", ""),
        }
    }
    /// DAQ SD sensor label (DAQ-Board `Core/Src/sd_card.c` scalar sensors).
    pub(crate) fn daq_sensor(self) -> &'static str {
        match self {
            Self::Kg1000 => "kg1000_network",
            Self::Kg50 => "kg50_network",
            Self::Tank => "valve_pressure_psi",
            Self::Chamber => "chamber_pressure_raw_volts_psi",
        }
    }
    /// The DAQ samples its load cells and the chamber PT itself; the tank PT
    /// is the valve board's and only relayed in the DAQ file.
    pub(crate) fn preferred(self) -> Source {
        match self {
            Self::Tank => Source::GsTelemetry,
            _ => Source::DaqSd,
        }
    }
    pub(crate) fn from_gs(data_type: &str, sender: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| {
            let (kind, who) = c.gs_type();
            kind == data_type && (who.is_empty() || who == sender)
        })
    }
}

/// Samples of the preferred source are covered within this distance of
/// their neighbours; longer holes are filled from the other source.
pub(crate) const COVER_GAP_MS: i64 = 250;

pub(crate) fn merge(preferred: &[Sample], other: &[Sample]) -> Vec<Sample> {
    let mut p: Vec<Sample> = preferred.to_vec();
    p.sort_by_key(|s| s.t_ms);
    let covered = |t: i64| -> bool {
        let i = p.partition_point(|s| s.t_ms < t);
        let next = p.get(i).map(|s| s.t_ms - t);
        let prev = i.checked_sub(1).map(|j| t - p[j].t_ms);
        match (prev, next) {
            (Some(a), Some(b)) => a + b <= COVER_GAP_MS || a.min(b) <= COVER_GAP_MS / 5,
            (Some(a), None) | (None, Some(a)) => a <= COVER_GAP_MS / 5,
            (None, None) => false,
        }
    };
    let mut out = p.clone();
    out.extend(other.iter().copied().filter(|s| !covered(s.t_ms)));
    out.sort_by_key(|s| s.t_ms);
    out
}

/// Network-UTC rows of the four channels from a DAQ SD CSV (launch file or
/// its lead-up). Rows on the local clock are skipped: they cannot be aligned.
pub(crate) fn read_daq_csv(path: &Path) -> std::io::Result<Vec<(Channel, Sample)>> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() || line.starts_with('#') || line.starts_with("timestamp_ms") {
            continue;
        }
        if let Some(row) = parse_daq_line(&line) {
            out.push(row);
        }
    }
    Ok(out)
}

pub(crate) fn parse_daq_line(line: &str) -> Option<(Channel, Sample)> {
    let fields = csv_fields(line);
    if fields.len() < 8 || fields[7].trim() != "network" {
        return None;
    }
    let sensor = fields[2].trim();
    let channel = Channel::ALL
        .into_iter()
        .find(|c| c.daq_sensor() == sensor)?;
    let t_ms = fields[0].trim().parse::<i64>().ok()?;
    let value = [3usize, 6]
        .iter()
        .filter_map(|i| fields.get(*i).and_then(|f| f.trim().parse::<f64>().ok()))
        .find(|v| v.is_finite())?;
    Some((
        channel,
        Sample {
            t_ms,
            value,
            source: Source::DaqSd,
        },
    ))
}

pub(crate) fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}
