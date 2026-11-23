//! Arbitrary-table upload over DAQ_DAC_TABLE: BEGIN, paced DATA chunks,
//! repair of the chunks the DAQ's status bitmap shows missing, then COMMIT
//! and confirmation of the CRC-32 the DAQ computed. The state machine is pure
//! (time and status in, messages out) so it is tested against a lossy DAQ
//! model; `run` drives it on the network.
use super::wire::{
    CHUNK_SAMPLES, TABLE_BAD_LENGTH, TABLE_BAD_SAMPLE, TABLE_CRC_ERROR, TABLE_LOADING, TABLE_READY,
    TableMsg, TableOp, TableStatus,
};
use super::waveform;
use serde::Serialize;
use std::time::{Duration, Instant};

const BEGIN_REPEAT: Duration = Duration::from_millis(300);
const BEGIN_TIMEOUT: Duration = Duration::from_secs(5);
const REPAIR_WAIT: Duration = Duration::from_millis(400);
const COMMIT_REPEAT: Duration = Duration::from_millis(400);
const PHASE_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_REPAIR_ROUNDS: u32 = 8;
/// Gap between queued chunks, so a 32-chunk table takes about half a second
/// and never crowds the DAQ's queues.
pub const CHUNK_GAP: Duration = Duration::from_millis(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Begin,
    Data,
    Repair,
    Commit,
    Done,
    Failed,
}

pub struct Uploader {
    pub upload_id: u32,
    pub codes: Vec<i16>,
    pub crc: u32,
    pub phase: Phase,
    phase_since: Instant,
    last_tx: Option<Instant>,
    pub repair_rounds: u32,
    pub resent_chunks: usize,
    pub error: Option<String>,
    pub chunks_confirmed: usize,
}

impl Uploader {
    pub fn new(upload_id: u32, codes: Vec<i16>, now: Instant) -> Self {
        let crc = waveform::shape_crc(&codes);
        Self {
            upload_id,
            codes,
            crc,
            phase: Phase::Begin,
            phase_since: now,
            last_tx: None,
            repair_rounds: 0,
            resent_chunks: 0,
            error: None,
            chunks_confirmed: 0,
        }
    }
    pub fn len(&self) -> u16 {
        self.codes.len() as u16
    }
    pub fn chunk_count(&self) -> usize {
        self.codes.len().div_ceil(CHUNK_SAMPLES)
    }
    fn full_mask(&self) -> u64 {
        let n = self.chunk_count();
        if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
    }
    fn control(&self, op: TableOp) -> TableMsg {
        TableMsg {
            op,
            index: self.len(),
            upload_id: self.upload_id,
            value: self.crc,
            samples: Vec::new(),
        }
    }
    fn chunk(&self, k: usize) -> TableMsg {
        let at = k * CHUNK_SAMPLES;
        let end = (at + CHUNK_SAMPLES).min(self.codes.len());
        TableMsg {
            op: TableOp::Data,
            index: at as u16,
            upload_id: self.upload_id,
            value: (end - at) as u32,
            samples: self.codes[at..end].to_vec(),
        }
    }
    fn enter(&mut self, phase: Phase, now: Instant) {
        self.phase = phase;
        self.phase_since = now;
    }
    fn fail(&mut self, message: String) -> Vec<TableMsg> {
        self.error = Some(message);
        self.phase = Phase::Failed;
        Vec::new()
    }
    fn due(&self, now: Instant, every: Duration) -> bool {
        self.last_tx.is_none_or(|t| now.saturating_duration_since(t) >= every)
    }

    /// One step. `status` is the latest table block with the time it was
    /// received; only status received after our last transmission counts as
    /// an answer to it. Returns the messages to send now, in order.
    pub fn step(&mut self, now: Instant, status: Option<(TableStatus, Instant)>) -> Vec<TableMsg> {
        let fresh = status.filter(|(_, at)| self.last_tx.is_none_or(|tx| *at > tx));
        let ours = fresh.map(|(s, _)| s).filter(|s| s.upload_id == self.upload_id);
        let elapsed = now.saturating_duration_since(self.phase_since);
        let out = match self.phase {
            Phase::Done | Phase::Failed => return Vec::new(),
            Phase::Begin => {
                if let Some(s) = ours {
                    if s.state == TABLE_BAD_LENGTH {
                        return self.fail(format!("The DAQ refused a {}-sample table.", self.len()));
                    }
                    if s.state == TABLE_LOADING && s.len == self.len() && s.expected_crc == self.crc {
                        self.enter(Phase::Data, now);
                        let all: Vec<TableMsg> = (0..self.chunk_count()).map(|k| self.chunk(k)).collect();
                        self.enter(Phase::Repair, now);
                        self.last_tx = Some(now);
                        return all;
                    }
                }
                if elapsed > BEGIN_TIMEOUT {
                    return self.fail(format!(
                        "The DAQ did not acknowledge the table upload within {} s.",
                        BEGIN_TIMEOUT.as_secs()
                    ));
                }
                if self.due(now, BEGIN_REPEAT) {
                    vec![self.control(TableOp::Begin)]
                } else {
                    Vec::new()
                }
            }
            Phase::Data => Vec::new(),
            Phase::Repair => {
                if let Some(s) = ours {
                    if s.state == TABLE_BAD_SAMPLE {
                        return self.fail("The DAQ rejected a shape sample outside -1..1.".into());
                    }
                    if s.state != TABLE_LOADING {
                        return self.fail(format!(
                            "The DAQ left the upload (table {}).",
                            super::wire::table_state_name(s.state)
                        ));
                    }
                    self.chunks_confirmed = (s.chunks & self.full_mask()).count_ones() as usize;
                    let missing = self.full_mask() & !s.chunks;
                    if missing == 0 {
                        self.enter(Phase::Commit, now);
                        self.last_tx = Some(now);
                        return vec![self.control(TableOp::Commit)];
                    }
                    if now.saturating_duration_since(self.last_tx.unwrap_or(now)) >= REPAIR_WAIT {
                        self.repair_rounds += 1;
                        if self.repair_rounds > MAX_REPAIR_ROUNDS {
                            return self.fail(format!(
                                "{} chunks still missing after {MAX_REPAIR_ROUNDS} resends.",
                                missing.count_ones()
                            ));
                        }
                        let resend: Vec<TableMsg> = (0..self.chunk_count())
                            .filter(|k| missing & (1u64 << k) != 0)
                            .map(|k| self.chunk(k))
                            .collect();
                        self.resent_chunks += resend.len();
                        self.last_tx = Some(now);
                        self.phase_since = now;
                        return resend;
                    }
                }
                if elapsed > PHASE_TIMEOUT {
                    return self.fail("No DAQ table status during the upload.".into());
                }
                Vec::new()
            }
            Phase::Commit => {
                if let Some(s) = ours {
                    match s.state {
                        TABLE_READY if s.crc == self.crc && s.len == self.len() => {
                            self.enter(Phase::Done, now);
                            self.chunks_confirmed = self.chunk_count();
                            return Vec::new();
                        }
                        TABLE_CRC_ERROR => {
                            return self.fail(format!(
                                "CRC mismatch: the DAQ computed {:08X}, expected {:08X}.",
                                s.crc, self.crc
                            ));
                        }
                        // Incomplete (a chunk was lost after all): repair again.
                        TABLE_LOADING if self.full_mask() & !s.chunks != 0 => {
                            self.enter(Phase::Repair, now);
                            self.last_tx = Some(now - REPAIR_WAIT);
                            return self.step(now, status);
                        }
                        _ => {}
                    }
                }
                if elapsed > PHASE_TIMEOUT {
                    return self.fail("The DAQ did not confirm the table.".into());
                }
                if self.due(now, COMMIT_REPEAT) {
                    vec![self.control(TableOp::Commit)]
                } else {
                    Vec::new()
                }
            }
        };
        if !out.is_empty() {
            self.last_tx = Some(now);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::{TABLE_EMPTY, TABLE_VERIFYING};
    use super::*;

    /// The DAQ's table state machine (daq_dac_control.c), with optional loss
    /// and corruption.
    struct Daq {
        t: TableStatus,
        buf: Vec<i16>,
    }
    impl Daq {
        fn new() -> Self {
            Self {
                t: TableStatus::default(),
                buf: vec![0; 512],
            }
        }
        fn full(&self) -> u64 {
            let n = usize::from(self.t.len).div_ceil(32);
            if n >= 64 { u64::MAX } else { (1 << n) - 1 }
        }
        fn rx(&mut self, m: &TableMsg) {
            match m.op {
                TableOp::Begin => {
                    if self.t.state == TABLE_LOADING && self.t.upload_id == m.upload_id && self.t.len == m.index {
                        return;
                    }
                    let bad = m.index < 16 || m.index > 512 || m.index % 2 != 0;
                    self.t = TableStatus {
                        state: if bad { TABLE_BAD_LENGTH } else { TABLE_LOADING },
                        len: m.index,
                        upload_id: m.upload_id,
                        expected_crc: m.value,
                        crc: 0,
                        chunks: 0,
                    };
                }
                TableOp::Data => {
                    if self.t.state != TABLE_LOADING || m.upload_id != self.t.upload_id {
                        return;
                    }
                    if m.samples.iter().any(|s| *s == i16::MIN) {
                        self.t.state = TABLE_BAD_SAMPLE;
                        return;
                    }
                    let at = usize::from(m.index);
                    self.buf[at..at + m.samples.len()].copy_from_slice(&m.samples);
                    self.t.chunks |= 1 << (at / 32);
                }
                TableOp::Commit => {
                    if self.t.state == TABLE_LOADING && m.upload_id == self.t.upload_id && self.t.chunks == self.full() {
                        self.t.state = TABLE_VERIFYING;
                        self.t.crc = waveform::shape_crc(&self.buf[..usize::from(self.t.len)]);
                        self.t.state = if self.t.crc == self.t.expected_crc { TABLE_READY } else { TABLE_CRC_ERROR };
                    }
                }
                TableOp::Clear => self.t = TableStatus { state: TABLE_EMPTY, ..TableStatus::default() },
            }
        }
    }

    /// Runs an upload; `drop(i)` loses the i-th message, `corrupt` flips one
    /// stored code after the data phase.
    fn simulate(codes: Vec<i16>, mut drop: impl FnMut(usize, &TableMsg) -> bool, corrupt: bool) -> (Uploader, Daq, usize) {
        let start = Instant::now();
        let mut up = Uploader::new(0xABCD, codes, start);
        let mut daq = Daq::new();
        let mut now = start;
        let mut sent = 0;
        let mut status: Option<(TableStatus, Instant)> = None;
        for _ in 0..2000 {
            for m in up.step(now, status) {
                let lost = drop(sent, &m);
                sent += 1;
                if !lost {
                    daq.rx(&m);
                    if corrupt && m.op == TableOp::Data && m.index == 32 {
                        daq.buf[40] ^= 1;
                    }
                }
            }
            if matches!(up.phase, Phase::Done | Phase::Failed) {
                break;
            }
            now += Duration::from_millis(50);
            // The DAQ reports status every 50 ms service tick after a change.
            status = Some((daq.t, now));
        }
        (up, daq, sent)
    }

    fn codes(n: usize) -> Vec<i16> {
        (0..n).map(|i| (-32767 + ((i * 997) % 65535) as i32) as i16).collect()
    }

    #[test]
    fn clean_upload_sends_begin_chunks_commit() {
        let (up, daq, sent) = simulate(codes(500), |_, _| false, false);
        assert_eq!(up.phase, Phase::Done, "{:?}", up.error);
        assert_eq!(daq.t.state, TABLE_READY);
        assert_eq!(daq.t.crc, up.crc);
        assert_eq!(&daq.buf[..500], &up.codes[..]);
        // BEGIN + 16 chunks + COMMIT.
        assert_eq!(sent, 18);
        assert_eq!((up.repair_rounds, up.resent_chunks, up.chunks_confirmed), (0, 0, 16));
    }

    #[test]
    fn lost_chunks_and_control_messages_are_repaired() {
        // Lose the first BEGIN, chunk 3 twice, chunk 20 once and the first COMMIT.
        let mut losses = std::collections::HashMap::new();
        let (up, daq, _) = simulate(
            codes(512),
            |_, m| {
                let key = (m.op as u8, m.index);
                let count = losses.entry(key).or_insert(0u32);
                *count += 1;
                match (m.op, m.index) {
                    (TableOp::Begin, _) => *count == 1,
                    (TableOp::Data, 96) => *count <= 2,
                    (TableOp::Data, 320) => *count == 1,
                    (TableOp::Commit, _) => *count == 1,
                    _ => false,
                }
            },
            false,
        );
        assert_eq!(up.phase, Phase::Done, "{:?}", up.error);
        assert_eq!(daq.t.state, TABLE_READY);
        assert_eq!(up.resent_chunks, 3);
        assert_eq!(up.repair_rounds, 2);
    }

    #[test]
    fn heavy_random_loss_still_converges_or_fails_cleanly() {
        let mut seed = 12345u32;
        let (up, daq, _) = simulate(
            codes(512),
            |_, _| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed % 100 < 30
            },
            false,
        );
        assert_eq!(up.phase, Phase::Done, "{:?}", up.error);
        assert_eq!(daq.t.crc, up.crc);
    }

    #[test]
    fn corruption_is_reported_as_a_crc_mismatch() {
        let (up, daq, _) = simulate(codes(64), |_, _| false, true);
        assert_eq!(up.phase, Phase::Failed);
        assert_eq!(daq.t.state, TABLE_CRC_ERROR);
        assert!(up.error.unwrap().contains("CRC mismatch"));
    }

    #[test]
    fn total_loss_times_out_and_bad_tables_fail() {
        let (up, _, _) = simulate(codes(64), |_, _| true, false);
        assert_eq!(up.phase, Phase::Failed);
        assert!(up.error.unwrap().contains("did not acknowledge"));
        let (up, _, _) = simulate(codes(14), |_, _| false, false);
        assert!(up.error.unwrap().contains("refused a 14-sample"));
        let mut bad = codes(64);
        bad[10] = i16::MIN;
        let (up, _, _) = simulate(bad, |_, _| false, false);
        assert!(up.error.unwrap().contains("-1..1"));
        // Chunks never get through: bounded resends, then failure.
        let (up, _, _) = simulate(codes(64), |_, m| m.op == TableOp::Data && m.index == 32, false);
        assert_eq!(up.phase, Phase::Failed);
        assert!(up.error.unwrap().contains("still missing"));
    }

    #[test]
    fn stale_status_is_not_taken_as_an_answer() {
        let start = Instant::now();
        let mut up = Uploader::new(5, codes(64), start);
        let begin = up.step(start, None);
        assert_eq!(begin.len(), 1);
        // A LOADING status for our id received before BEGIN went out is ignored.
        let early = TableStatus { state: TABLE_LOADING, len: 64, upload_id: 5, expected_crc: up.crc, ..Default::default() };
        assert!(up.step(start + Duration::from_millis(10), Some((early, start - Duration::from_millis(1)))).is_empty());
        assert_eq!(up.phase, Phase::Begin);
        let data = up.step(start + Duration::from_millis(20), Some((early, start + Duration::from_millis(15))));
        assert_eq!(data.len(), 2);
        assert_eq!(up.phase, Phase::Repair);
    }
}
