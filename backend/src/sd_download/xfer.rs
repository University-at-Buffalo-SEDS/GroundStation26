//! SD transfer protocol v2 (`docs/backend/sd-transfer.md`): the wire codec,
//! the GS receive state machine ([`Receiver`]) and a board-side engine
//! ([`Session`], [`serve`]) used by the RRLS outbox and by the tests' mock
//! FC and DAQ.
//!
//! The board cuts the requested range into independent blocks (16 KiB raw by
//! default), encodes each with SDX (and zstd where allowed), and streams the
//! block bodies in DATA messages with consecutive `seq`, paced to the rate
//! and window the GS sets in OPEN and every ACK. The GS acknowledges
//! selectively (`una` + a 32-bit bitmap); the board resends only gaps.
use super::sdx;
use sedsnet::router::{P2pStreamEventKind, P2pStreamId, Router};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const OP_LIST: u8 = 0x21;
pub(crate) const OP_CANCEL: u8 = 0x23;
pub(crate) const OP_CONFIRM: u8 = 0x24;
pub(crate) const OP_OPEN: u8 = 0x25;
pub(crate) const OP_ACK: u8 = 0x26;
pub(crate) const OP_HASH: u8 = 0x27;
pub(crate) const RESPONSE: u8 = 0x80;
pub(crate) const OP_DATA: u8 = OP_ACK | RESPONSE;

pub(crate) const STATUS_OK: u8 = 0;
pub(crate) const STATUS_BAD_MESSAGE: u8 = 1;
pub(crate) const STATUS_BUSY: u8 = 3;
pub(crate) const STATUS_NOT_FOUND: u8 = 4;
pub(crate) const STATUS_END: u8 = 6;
pub(crate) const STATUS_BAD_OFFSET: u8 = 7;

/// OPEN flags.
pub(crate) const OPEN_SDX: u8 = 0x01;
pub(crate) const OPEN_ZSTD: u8 = 0x02;
/// OPEN and HASH answer flag: the file is still open for writing.
pub(crate) const OPEN_GROWING: u8 = 0x04;

/// DATA flags.
pub(crate) const DATA_FIRST: u8 = 0x01;
pub(crate) const DATA_LAST: u8 = 0x02;
pub(crate) const DATA_SDX: u8 = 0x04;
pub(crate) const DATA_ZSTD: u8 = 0x08;
pub(crate) const DATA_END: u8 = 0x10;
pub(crate) const DATA_GROWING: u8 = 0x20;
/// DATA header without and with the LAST trailer (`raw_len`, `crc`). Boards
/// reserve the full 17 bytes, so a body is at most `msg - 17` bytes.
pub(crate) const DATA_HEADER: usize = 9;
pub(crate) const DATA_HEADER_MAX: usize = 17;
pub(crate) const OPEN_ANSWER_LEN: usize = 19;
pub(crate) const HASH_ANSWER_LEN: usize = 39;
pub(crate) const ACK_LEN: usize = 11;

/// The rate field counts DATA bytes per second in units of 16.
pub(crate) const RATE_UNIT: u64 = 16;
/// Largest raw block the GS accepts (64 KiB target plus one long item).
pub(crate) const MAX_RAW_BLOCK: usize = 128 * 1024;

// -------------------------------------------------------------------- codec

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Open {
    pub xfer: u8,
    pub dir: u8,
    pub flags: u8,
    pub offset: u32,
    pub length: u32,
    /// DATA bytes per second / 16.
    pub rate: u16,
    pub msg: u16,
    pub window: u8,
    pub block_kib: u8,
    pub name: String,
}

impl Open {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![OP_OPEN, self.xfer, self.dir, self.flags];
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
        out.extend_from_slice(&self.rate.to_le_bytes());
        out.extend_from_slice(&self.msg.to_le_bytes());
        out.extend_from_slice(&[self.window, self.block_kib, self.name.len() as u8]);
        out.extend_from_slice(self.name.as_bytes());
        out
    }
    pub(crate) fn decode(p: &[u8]) -> Option<Self> {
        if p.len() < 19 || p[0] != OP_OPEN || p.len() != 19 + usize::from(p[18]) {
            return None;
        }
        Some(Self {
            xfer: p[1],
            dir: p[2],
            flags: p[3],
            offset: u32_at(p, 4),
            length: u32_at(p, 8),
            rate: u16_at(p, 12),
            msg: u16_at(p, 14),
            window: p[16],
            block_kib: p[17],
            name: std::str::from_utf8(&p[19..]).ok()?.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct OpenAnswer {
    pub status: u8,
    pub xfer: u8,
    pub flags: u8,
    pub size: u32,
    pub offset: u32,
    pub length: u32,
    pub msg: u16,
    pub block_kib: u8,
}

impl OpenAnswer {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![OP_OPEN | RESPONSE, self.status, self.xfer, self.flags];
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
        out.extend_from_slice(&self.msg.to_le_bytes());
        out.push(self.block_kib);
        out
    }
    pub(crate) fn decode(p: &[u8]) -> Option<Self> {
        (p.len() == OPEN_ANSWER_LEN && p[0] == OP_OPEN | RESPONSE).then(|| Self {
            status: p[1],
            xfer: p[2],
            flags: p[3],
            size: u32_at(p, 4),
            offset: u32_at(p, 8),
            length: u32_at(p, 12),
            msg: u16_at(p, 16),
            block_kib: p[18],
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ack {
    pub xfer: u8,
    pub una: u16,
    pub bitmap: u32,
    pub rate: u16,
    pub window: u8,
}

impl Ack {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![OP_ACK, self.xfer];
        out.extend_from_slice(&self.una.to_le_bytes());
        out.extend_from_slice(&self.bitmap.to_le_bytes());
        out.extend_from_slice(&self.rate.to_le_bytes());
        out.push(self.window);
        out
    }
    pub(crate) fn decode(p: &[u8]) -> Option<Self> {
        (p.len() == ACK_LEN && p[0] == OP_ACK).then(|| Self {
            xfer: p[1],
            una: u16_at(p, 2),
            bitmap: u32_at(p, 4),
            rate: u16_at(p, 8),
            window: p[10],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Data<'a> {
    pub xfer: u8,
    pub flags: u8,
    pub seq: u16,
    pub block: u32,
    /// LAST only.
    pub raw_len: u32,
    pub crc: u32,
    pub body: &'a [u8],
}

impl Data<'_> {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DATA_HEADER_MAX + self.body.len());
        out.extend_from_slice(&[OP_DATA, self.xfer, self.flags]);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.block.to_le_bytes());
        if self.flags & DATA_LAST != 0 {
            out.extend_from_slice(&self.raw_len.to_le_bytes());
            out.extend_from_slice(&self.crc.to_le_bytes());
        }
        out.extend_from_slice(self.body);
        out
    }
    pub(crate) fn decode(p: &[u8]) -> Option<Data<'_>> {
        if p.len() < DATA_HEADER || p[0] != OP_DATA {
            return None;
        }
        let flags = p[2];
        let (raw_len, crc, body) = if flags & DATA_LAST != 0 {
            if p.len() < DATA_HEADER_MAX {
                return None;
            }
            (u32_at(p, 9), u32_at(p, 13), &p[DATA_HEADER_MAX..])
        } else {
            (0, 0, &p[DATA_HEADER..])
        };
        Some(Data { xfer: p[1], flags, seq: u16_at(p, 3), block: u32_at(p, 5), raw_len, crc, body })
    }
}

pub(crate) fn encode_hash(dir: u8, name: &str) -> Vec<u8> {
    let mut out = vec![OP_HASH, dir, name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HashAnswer {
    pub status: u8,
    pub flags: u8,
    pub size: u32,
    pub sha256: [u8; 32],
}

impl HashAnswer {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![OP_HASH | RESPONSE, self.status, self.flags];
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.sha256);
        out
    }
    pub(crate) fn decode(p: &[u8]) -> Option<Self> {
        (p.len() == HASH_ANSWER_LEN && p[0] == OP_HASH | RESPONSE).then(|| Self {
            status: p[1],
            flags: p[2],
            size: u32_at(p, 3),
            sha256: p[7..39].try_into().expect("32 bytes"),
        })
    }
}

fn u16_at(p: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([p[at], p[at + 1]])
}

fn u32_at(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().expect("four bytes"))
}

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().try_into().expect("32 bytes")
}

/// Rate in B/s -> the 16-byte wire unit (at least 1 unless 0).
pub(crate) fn rate_units(bytes_per_sec: u64) -> u16 {
    if bytes_per_sec == 0 { 0 } else { bytes_per_sec.div_ceil(RATE_UNIT).clamp(1, u64::from(u16::MAX)) as u16 }
}

// ---------------------------------------------------------- GS receiver

#[derive(Debug, Clone)]
struct Piece {
    flags: u8,
    block: u32,
    raw_len: u32,
    crc: u32,
    body: Vec<u8>,
}

/// What one DATA message did to the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Arrival {
    /// A message not seen before.
    pub fresh: bool,
    /// Seen before: the board resent it (our ACK may have been lost).
    pub duplicate: bool,
    /// It skipped over messages that have not arrived (a loss).
    pub gap: bool,
    /// A block completed but failed its length or CRC-32 (raw offset).
    pub bad_block: Option<u64>,
    /// A block completed and verified.
    pub block_done: bool,
}

/// GS side of one OPEN: reassembles blocks by `seq`, decodes and verifies
/// them, and hands out the verified bytes in file order.
pub(crate) struct Receiver {
    pub xfer: u8,
    /// Next raw offset the caller has not been given.
    pub committed: u64,
    pub range_end: u64,
    una: u64,
    received: BTreeSet<u64>,
    highest: Option<u64>,
    pieces: BTreeMap<u64, Piece>,
    /// Verified raw spans by offset, not yet handed out.
    spans: BTreeMap<u64, Vec<u8>>,
    held_bytes: usize,
}

/// Most messages the receiver keeps ahead of `una` (the window is <= 32).
const SEQ_AHEAD: u64 = 256;
/// Verified bytes held ahead of the committed offset.
const HELD_MAX: usize = 8 * 1024 * 1024;

impl Receiver {
    pub(crate) fn new(xfer: u8, start: u64, end: u64) -> Self {
        Self {
            xfer,
            committed: start,
            range_end: end,
            una: 0,
            received: BTreeSet::new(),
            highest: None,
            pieces: BTreeMap::new(),
            spans: BTreeMap::new(),
            held_bytes: 0,
        }
    }

    /// A new OPEN (after a stall or a bad block) on the same range: keep
    /// every verified span, forget the old transfer's sequence state.
    pub(crate) fn restart(&mut self, xfer: u8, end: u64) {
        self.xfer = xfer;
        self.range_end = end;
        self.una = 0;
        self.received.clear();
        self.highest = None;
        self.pieces.clear();
    }

    pub(crate) fn done(&self) -> bool {
        self.committed >= self.range_end
    }

    pub(crate) fn has_gap(&self) -> bool {
        !self.received.is_empty()
    }

    pub(crate) fn on_data(&mut self, d: &Data<'_>) -> Arrival {
        let mut out = Arrival::default();
        if d.xfer != self.xfer {
            return out;
        }
        let diff = u64::from(d.seq.wrapping_sub(self.una as u16));
        if diff >= 0x8000 {
            out.duplicate = true;
            return out;
        }
        if diff >= SEQ_AHEAD {
            return out;
        }
        let seq = self.una + diff;
        if seq < self.una || self.received.contains(&seq) {
            out.duplicate = true;
            return out;
        }
        out.fresh = true;
        if self.highest.is_some_and(|h| seq > h + 1) || (self.highest.is_none() && seq > self.una) {
            out.gap = true;
        }
        self.highest = Some(self.highest.map_or(seq, |h| h.max(seq)));
        self.received.insert(seq);
        while self.received.remove(&self.una) {
            self.una += 1;
        }
        let block = u64::from(d.block);
        if d.flags & DATA_LAST != 0 && block + u64::from(d.raw_len) <= self.committed {
            // Already handed out (a resent or overlapping block).
            return out;
        }
        self.pieces.insert(seq, Piece { flags: d.flags, block: d.block, raw_len: d.raw_len, crc: d.crc, body: d.body.to_vec() });
        if let Some(result) = self.try_block(seq) {
            match result {
                Ok(()) => out.block_done = true,
                Err(offset) => out.bad_block = Some(offset),
            }
        }
        out
    }

    /// Assembles the block holding `seq` once all its messages are here.
    fn try_block(&mut self, seq: u64) -> Option<Result<(), u64>> {
        let block = self.pieces.get(&seq)?.block;
        let mut first = seq;
        while self.pieces[&first].flags & DATA_FIRST == 0 {
            let prev = first.checked_sub(1)?;
            if self.pieces.get(&prev)?.block != block {
                return None;
            }
            first = prev;
        }
        let mut last = seq;
        while self.pieces[&last].flags & DATA_LAST == 0 {
            let next = last + 1;
            if self.pieces.get(&next)?.block != block || self.pieces[&next].flags & DATA_FIRST != 0 {
                return None;
            }
            last = next;
        }
        let parts: Vec<Piece> = (first..=last).filter_map(|s| self.pieces.remove(&s)).collect();
        let tail = parts.last().expect("one piece");
        let (flags, raw_len, crc) = (tail.flags, tail.raw_len as usize, tail.crc);
        let offset = u64::from(block);
        let body: Vec<u8> = parts.iter().flat_map(|p| p.body.iter().copied()).collect();
        let decoded = decode_body(&body, flags, raw_len);
        let ok = decoded.as_ref().is_some_and(|raw| raw.len() == raw_len && crc32(raw) == crc)
            && raw_len > 0
            && offset + raw_len as u64 <= self.range_end;
        if !ok {
            return Some(Err(offset));
        }
        let raw = decoded.expect("verified");
        if offset + raw.len() as u64 > self.committed && self.held_bytes + raw.len() <= HELD_MAX {
            self.held_bytes += raw.len();
            self.spans.insert(offset, raw);
        }
        Some(Ok(()))
    }

    /// The verified bytes that continue the committed offset, if any.
    pub(crate) fn take_ready(&mut self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let (start, end) = {
                let (start, raw) = self.spans.range(..=self.committed).next_back()?;
                (*start, *start + raw.len() as u64)
            };
            let raw = self.spans.remove(&start).expect("span");
            self.held_bytes -= raw.len();
            if end > self.committed {
                out.extend_from_slice(&raw[(self.committed - start) as usize..]);
                self.committed = end;
            }
            // Spans wholly behind the committed offset are dropped as we go.
            if self.spans.range(..=self.committed).next_back().is_none() {
                break;
            }
        }
        (!out.is_empty()).then_some(out)
    }

    pub(crate) fn ack(&self, rate: u16, window: u8) -> Ack {
        let mut bitmap = 0u32;
        for seq in self.received.range(self.una + 1..self.una + 33) {
            bitmap |= 1 << (seq - self.una - 1);
        }
        Ack { xfer: self.xfer, una: self.una as u16, bitmap, rate, window }
    }
}

/// zstd -> SDX -> raw; `None` when the body is not a valid encoding.
fn decode_body(body: &[u8], flags: u8, raw_len: usize) -> Option<Vec<u8>> {
    if raw_len > MAX_RAW_BLOCK {
        return None;
    }
    let unzipped;
    let body = if flags & DATA_ZSTD != 0 {
        unzipped = zstd::bulk::decompress(body, raw_len.saturating_mul(2) + 4096).ok()?;
        &unzipped[..]
    } else {
        body
    };
    if flags & DATA_SDX != 0 { sdx::decode_block(body, raw_len).ok() } else { Some(body.to_vec()) }
}

// ------------------------------------------------------------ board side

/// One file as a server sees it at OPEN or HASH time.
pub(crate) struct Snapshot {
    pub bytes: Arc<Vec<u8>>,
    pub growing: bool,
}

/// What a board (or the RRLS outbox) serves.
pub(crate) trait Catalog: Send + Sync + 'static {
    /// LIST entry `index` of `dir`: (name, size, open).
    fn list(&self, dir: u8, index: u16) -> Option<(String, u64, bool)>;
    fn file(&self, dir: u8, name: &str) -> Result<Snapshot, u8>;
    /// Size, SHA-256 and growing flag; by default hashed from [`Catalog::file`].
    fn hash(&self, dir: u8, name: &str) -> Result<(u64, [u8; 32], bool), u8> {
        let snap = self.file(dir, name)?;
        Ok((snap.bytes.len() as u64, sha256(&snap.bytes), snap.growing))
    }
    /// Requests beyond the common ones (RRLS CONFIRM); the answer, if any.
    fn other(&self, _payload: &[u8]) -> Option<Vec<u8>> {
        None
    }
    /// Test hooks: an OPEN arrived; corrupt the CRC-32 of the next block.
    fn on_open(&self, _open: &Open) {}
    fn corrupt_next_block(&self) -> bool {
        false
    }
    /// Test hook: raw bytes cut into new blocks.
    fn on_served(&self, _raw: usize) {}
    /// Test hook: an ACK for an open transfer arrived.
    fn on_ack(&self, _ack: &Ack) {}
    /// Test hook: a CANCEL arrived (a board abandons a SHA-256 it was computing).
    fn on_cancel(&self) {}
    /// Test hook: an incoming request is dropped (a lossy uplink in tests
    /// that cannot drop router frames).
    fn drop_request(&self, _payload: &[u8]) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ServeOptions {
    /// Largest DATA message this server sends (FC 768, DAQ 1024, RRLS 4096).
    pub max_msg: u16,
    pub zstd: bool,
    /// At least this long between DATA messages.
    pub min_gap: Duration,
    /// Session timer period.
    pub tick: Duration,
}

struct Sent {
    bytes: Vec<u8>,
    last_sent: Option<Instant>,
}

/// One OPEN on the board: cuts, encodes, paces and resends.
pub(crate) struct Session {
    xfer: u8,
    data: Arc<Vec<u8>>,
    end: usize,
    cursor: usize,
    msg: usize,
    window: u64,
    block_bytes: usize,
    sdx: bool,
    zstd: bool,
    growing: bool,
    corrupt: bool,
    next_seq: u64,
    una: u64,
    unacked: BTreeMap<u64, Sent>,
    resend: BTreeSet<u64>,
    rate: u64,
    tokens: f64,
    last_tick: Instant,
    last_send: Option<Instant>,
    last_ack: Instant,
    last_progress: Instant,
    min_gap: Duration,
    /// Raw bytes cut into blocks since the server last asked.
    fresh_raw: usize,
}

const RESEND_HOLD: Duration = Duration::from_secs(1);
const STALL_RESEND: Duration = Duration::from_secs(3);
const SESSION_TIMEOUT: Duration = Duration::from_secs(15);

impl Session {
    /// Answers an OPEN; a session when the range is non-empty.
    pub(crate) fn open(req: &Open, snap: Snapshot, opts: &ServeOptions, corrupt: bool, now: Instant) -> (OpenAnswer, Option<Self>) {
        let size = snap.bytes.len();
        let mut answer = OpenAnswer { xfer: req.xfer, size: size as u32, offset: req.offset, ..OpenAnswer::default() };
        let offset = req.offset as usize;
        if offset > size {
            answer.status = STATUS_BAD_OFFSET;
            return (answer, None);
        }
        if offset == size {
            answer.status = STATUS_END;
            return (answer, None);
        }
        if !(1..=32).contains(&req.window) || !(1..=64).contains(&req.block_kib) || usize::from(req.msg) <= DATA_HEADER_MAX {
            answer.status = STATUS_BAD_MESSAGE;
            return (answer, None);
        }
        let end = if req.length == 0 { size } else { size.min(offset + req.length as usize) };
        let msg = req.msg.min(opts.max_msg);
        let sdx = req.flags & OPEN_SDX != 0;
        let zstd = sdx && opts.zstd && req.flags & OPEN_ZSTD != 0;
        answer.flags = u8::from(sdx) * OPEN_SDX | u8::from(zstd) * OPEN_ZSTD | u8::from(snap.growing) * OPEN_GROWING;
        answer.length = (end - offset) as u32;
        answer.msg = msg;
        answer.block_kib = req.block_kib;
        let session = Self {
            xfer: req.xfer,
            data: snap.bytes,
            end,
            cursor: offset,
            msg: usize::from(msg),
            window: u64::from(req.window),
            block_bytes: usize::from(req.block_kib) * 1024,
            sdx,
            zstd,
            growing: snap.growing,
            corrupt,
            next_seq: 0,
            una: 0,
            unacked: BTreeMap::new(),
            resend: BTreeSet::new(),
            rate: u64::from(req.rate) * RATE_UNIT,
            tokens: 0.0,
            last_tick: now,
            last_send: None,
            last_ack: now,
            last_progress: now,
            min_gap: opts.min_gap,
            fresh_raw: 0,
        };
        (answer, Some(session))
    }

    pub(crate) fn on_ack(&mut self, ack: &Ack, now: Instant) {
        if ack.xfer != self.xfer {
            return;
        }
        self.last_ack = now;
        self.rate = u64::from(ack.rate) * RATE_UNIT;
        self.window = u64::from(ack.window.clamp(1, 32));
        let diff = u64::from(ack.una.wrapping_sub(self.una as u16));
        if diff <= self.next_seq - self.una {
            let una = self.una + diff;
            if una > self.una {
                self.last_progress = now;
            }
            self.una = una;
        }
        let una = self.una;
        self.unacked.retain(|seq, _| *seq >= una);
        self.resend.retain(|seq| *seq >= una);
        let mut highest = None;
        for i in 0..32 {
            if ack.bitmap & (1 << i) != 0 {
                let seq = una + 1 + i;
                self.unacked.remove(&seq);
                self.resend.remove(&seq);
                highest = Some(seq);
            }
        }
        // Gaps below a received message were lost.
        if let Some(h) = highest {
            let missing: Vec<u64> = self.unacked.range(..h).filter(|(_, s)| s.last_sent.is_some()).map(|(seq, _)| *seq).collect();
            self.resend.extend(missing);
        }
    }

    pub(crate) fn finished(&self, now: Instant) -> bool {
        (self.cursor >= self.end && self.unacked.is_empty()) || now.duration_since(self.last_ack) >= SESSION_TIMEOUT
    }

    /// DATA messages due now.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let dt = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        let mut out = Vec::new();
        if self.rate == 0 {
            self.tokens = 0.0;
            return out;
        }
        self.tokens = (self.tokens + dt * self.rate as f64).min(self.msg as f64);
        if !self.unacked.is_empty() && now.duration_since(self.last_progress) >= STALL_RESEND {
            let seqs: Vec<u64> = self.unacked.iter().filter(|(_, s)| s.last_sent.is_some()).map(|(seq, _)| *seq).collect();
            self.resend.extend(seqs);
            self.last_progress = now;
        }
        while self.tokens >= 0.0 {
            if self.last_send.is_some_and(|t| now.duration_since(t) < self.min_gap) {
                break;
            }
            let Some(seq) = self.next_to_send(now) else { break };
            let sent = self.unacked.get_mut(&seq).expect("message");
            sent.last_sent = Some(now);
            self.tokens -= sent.bytes.len() as f64;
            self.last_send = Some(now);
            out.push(sent.bytes.clone());
        }
        out
    }

    fn next_to_send(&mut self, now: Instant) -> Option<u64> {
        let due = self.resend.iter().copied().find(|seq| {
            self.unacked.get(seq).is_some_and(|s| s.last_sent.is_none_or(|t| now.duration_since(t) >= RESEND_HOLD))
        });
        if let Some(seq) = due {
            self.resend.remove(&seq);
            return Some(seq);
        }
        if let Some((seq, _)) = self.unacked.iter().find(|(_, s)| s.last_sent.is_none()) {
            return (*seq - self.una < self.window).then_some(*seq);
        }
        if self.next_seq - self.una >= self.window || self.cursor >= self.end {
            return None;
        }
        let first = self.next_seq;
        self.cut_block();
        (first < self.next_seq).then_some(first)
    }

    /// Encodes the next block into messages.
    fn cut_block(&mut self) {
        let start = self.cursor;
        let slice = &self.data[start..self.end];
        let (used, mut body) = if self.sdx { sdx::Encoder::encode_block(slice, self.block_bytes) } else {
            let used = slice.len().min(self.block_bytes);
            (used, slice[..used].to_vec())
        };
        if self.zstd {
            body = zstd::bulk::compress(&body, 5).expect("zstd");
        }
        let raw = &self.data[start..start + used];
        let mut crc = crc32(raw);
        if self.corrupt {
            self.corrupt = false;
            crc ^= 0x5a;
        }
        self.cursor = start + used;
        self.fresh_raw += used;
        let room = self.msg - DATA_HEADER_MAX;
        let chunks: Vec<&[u8]> = body.chunks(room).collect();
        let n = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut flags = if self.sdx { DATA_SDX } else { 0 };
            flags |= if self.zstd { DATA_ZSTD } else { 0 };
            flags |= if self.growing { DATA_GROWING } else { 0 };
            if i == 0 {
                flags |= DATA_FIRST;
            }
            if i + 1 == n {
                flags |= DATA_LAST;
                if self.cursor >= self.end {
                    flags |= DATA_END;
                }
            }
            let bytes = Data { xfer: self.xfer, flags, seq: self.next_seq as u16, block: start as u32, raw_len: used as u32, crc, body: chunk }.encode();
            self.unacked.insert(self.next_seq, Sent { bytes, last_sent: None });
            self.next_seq += 1;
        }
    }
}

/// Serves a [`Catalog`] on a P2P stream port: LIST, OPEN/ACK/DATA, HASH,
/// CANCEL and the catalog's own requests. `peer` restricts it to one host.
pub(crate) fn serve(router: &Arc<Router>, port: u16, catalog: Arc<dyn Catalog>, opts: ServeOptions, peer: Option<&'static str>) -> Result<(), String> {
    type Sessions = Arc<Mutex<HashMap<P2pStreamId, Session>>>;
    let sessions: Sessions = Arc::default();
    let reply = router.clone();
    let handler_sessions = sessions.clone();
    let handler_catalog = catalog.clone();
    router
        .bind_p2p_stream_port(port, move |event| {
            let mut sessions = handler_sessions.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(event.kind, P2pStreamEventKind::Closed | P2pStreamEventKind::Reset) {
                sessions.remove(&event.stream_id);
                return Ok(());
            }
            if event.kind != P2pStreamEventKind::Data || event.payload.is_empty() {
                return Ok(());
            }
            if let Some(peer) = peer
                && !event.peer_hostname.eq_ignore_ascii_case(peer)
            {
                return reply.reset_p2p_stream(event.stream_id).map(|_| ());
            }
            let catalog = &handler_catalog;
            let p = event.payload;
            if catalog.drop_request(p) {
                return Ok(());
            }
            let now = Instant::now();
            let answer = match p[0] {
                OP_LIST if p.len() == 4 => {
                    let index = u16::from_le_bytes([p[2], p[3]]);
                    let entry = catalog.list(p[1], index);
                    let mut f = vec![OP_LIST | RESPONSE, if entry.is_some() { STATUS_OK } else { STATUS_END }, p[1], p[2], p[3]];
                    let (name, size, open) = entry.unwrap_or_default();
                    f.extend_from_slice(&(size as u32).to_le_bytes());
                    f.extend_from_slice(&[u8::from(open), name.len() as u8, 0]);
                    f.extend_from_slice(name.as_bytes());
                    Some(f)
                }
                OP_OPEN => match Open::decode(p) {
                    Some(open) => {
                        catalog.on_open(&open);
                        sessions.remove(&event.stream_id);
                        match catalog.file(open.dir, &open.name) {
                            Ok(snap) => {
                                let (answer, session) = Session::open(&open, snap, &opts, catalog.corrupt_next_block(), now);
                                if let Some(session) = session {
                                    sessions.insert(event.stream_id, session);
                                }
                                Some(answer.encode())
                            }
                            Err(status) => Some(OpenAnswer { status, xfer: open.xfer, ..OpenAnswer::default() }.encode()),
                        }
                    }
                    None => Some(OpenAnswer { status: STATUS_BAD_MESSAGE, ..OpenAnswer::default() }.encode()),
                },
                OP_ACK => {
                    if let (Some(ack), Some(session)) = (Ack::decode(p), sessions.get_mut(&event.stream_id)) {
                        catalog.on_ack(&ack);
                        session.on_ack(&ack, now);
                    }
                    None
                }
                OP_HASH if p.len() >= 3 && p.len() == 3 + usize::from(p[2]) => {
                    let name = std::str::from_utf8(&p[3..]).unwrap_or("");
                    let answer = if sessions.contains_key(&event.stream_id) {
                        HashAnswer { status: STATUS_BUSY, flags: 0, size: 0, sha256: [0; 32] }
                    } else {
                        match catalog.hash(p[1], name) {
                            Ok((size, sha256, growing)) => HashAnswer { status: STATUS_OK, flags: if growing { OPEN_GROWING } else { 0 }, size: size as u32, sha256 },
                            Err(status) => HashAnswer { status, flags: 0, size: 0, sha256: [0; 32] },
                        }
                    };
                    Some(answer.encode())
                }
                OP_CANCEL => {
                    sessions.remove(&event.stream_id);
                    catalog.on_cancel();
                    Some(vec![OP_CANCEL | RESPONSE, STATUS_OK])
                }
                _ => catalog.other(p).or_else(|| Some(vec![p[0] | RESPONSE, STATUS_BAD_MESSAGE])),
            };
            drop(sessions);
            if let Some(answer) = answer {
                reply.send_p2p_stream(event.stream_id, &answer)?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    // Pacing: one timer for every session of this port. It ends when the
    // port is cleared (the handler, the other owner of `sessions`, is gone).
    let router = router.clone();
    tokio::spawn(async move {
        while Arc::strong_count(&sessions) > 1 {
            let mut raw = 0;
            let due: Vec<(P2pStreamId, Vec<Vec<u8>>)> = {
                let now = Instant::now();
                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                sessions.retain(|_, s| !s.finished(now));
                let due = sessions.iter_mut().map(|(id, s)| (*id, s.poll(now))).filter(|(_, m)| !m.is_empty()).collect();
                for s in sessions.values_mut() {
                    raw += std::mem::take(&mut s.fresh_raw);
                }
                due
            };
            if raw > 0 {
                catalog.on_served(raw);
            }
            for (id, messages) in due {
                for message in messages {
                    let _ = router.send_p2p_stream(id, &message);
                }
            }
            tokio::time::sleep(opts.tick).await;
        }
    });
    Ok(())
}
