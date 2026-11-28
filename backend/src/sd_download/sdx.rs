//! SDX: the lossless binary encoding of SD CSV rows on the SD transfer wire
//! (protocol v2, `docs/backend/sd-transfer.md`). Files on the card and on the
//! station stay CSV; SDX exists only between the board's encoder
//! (FlightComputer26/DAQ-Board `Core/Src/sd_xfer_codec.c`, or [`Encoder`] on
//! the RRLS outbox) and [`decode_block`].
//!
//! The decoder is the reference: it only expands what it is sent, rejects
//! anything malformed with an [`Error`] (never panics) and never produces more
//! than the caller's `max_out` bytes, so a hostile or damaged block costs at
//! most that much memory. The encoder replaces only text the decoder rebuilds
//! byte for byte (every number is formatted back and compared before use) and
//! carries everything else verbatim.
use std::collections::HashMap;

const TAG_RAW: u8 = 0x00;
const TAG_LABEL: u8 = 0x01;
const TAG_ROW: u8 = 0x02;
const OP_EMPTY: u8 = 0;
const OP_SAME: u8 = 1;
const OP_DELTA: u8 = 2;
const OP_NUM: u8 = 3;
const OP_TEXT: u8 = 4;
const OP_QTEXT: u8 = 5;
const OP_COPY: u8 = 6;
const OP_QVEC: u8 = 7;
const FIELDS: usize = 13;
/// Label ids 0..126 fit the row tag byte (`0x02 + 2 * id + s`).
pub const MAX_LABELS: usize = 127;
/// The longest label the Rust encoder defines (the firmware stops at 23).
const ENCODER_LABEL_MAX: usize = 63;
/// Most op nibbles a row can carry: 12 fields of QVEC 15 (1 + 1 + 15 each).
const MAX_OPS: usize = (FIELDS - 1) * 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SDX: {}", self.0)
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------- numbers

/// A number token: shape bytes, signed exponent and the signed integer of all
/// its digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Num {
    b0: u8,
    b1: u8,
    exp: i64,
    v: i64,
}

const POW10: [i128; 4] = [1, 10, 100, 1000];

fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzz(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

/// Expansion of a number (spec "Numbers"); `None` when the shape cannot hold
/// the value or the exponent.
fn fmt_num(n: &Num, out: &mut Vec<u8>) -> Option<()> {
    let nint = usize::from(n.b1 & 15);
    let nfrac = usize::from(n.b1 >> 4);
    if nint == 0 || nint + nfrac > 18 || n.b0 & !0x1f != 0 {
        return None;
    }
    let digits = n.v.unsigned_abs().to_string();
    if digits.len() > nint + nfrac {
        return None;
    }
    if n.v < 0 {
        out.push(b'-');
    }
    let pad = nint + nfrac - digits.len();
    let mut all = Vec::with_capacity(nint + nfrac);
    all.resize(pad, b'0');
    all.extend_from_slice(digits.as_bytes());
    out.extend_from_slice(&all[..nint]);
    if nfrac > 0 {
        out.push(b'.');
        out.extend_from_slice(&all[nint..]);
    }
    if n.b0 & 1 != 0 {
        let x = usize::from((n.b0 >> 3) & 3);
        let e = n.exp.unsigned_abs().to_string();
        if x == 0 || e.len() > x {
            return None;
        }
        out.push(if n.b0 & 2 != 0 { b'E' } else { b'e' });
        if n.exp < 0 {
            out.push(b'-');
        } else if n.b0 & 4 != 0 {
            out.push(b'+');
        }
        out.resize(out.len() + x - e.len(), b'0');
        out.extend_from_slice(e.as_bytes());
    } else if n.exp != 0 || n.b0 != 0 {
        return None;
    }
    Some(())
}

/// `[-]D{1..15}[.D{1..15}][(e|E)[+|-]D{1..3}]`, at most 18 digits, and only
/// when [`fmt_num`] rebuilds exactly this text.
fn parse_num(t: &[u8]) -> Option<Num> {
    let n = t.len();
    let mut i = 0;
    let neg = n > 0 && t[0] == b'-';
    if neg {
        i = 1;
    }
    let d0 = i;
    while i < n && t[i].is_ascii_digit() {
        i += 1;
    }
    let nint = i - d0;
    if nint == 0 || nint > 15 {
        return None;
    }
    let mut nfrac = 0;
    let f0;
    if i < n && t[i] == b'.' {
        i += 1;
        f0 = i;
        while i < n && t[i].is_ascii_digit() {
            i += 1;
        }
        nfrac = i - f0;
        if nfrac == 0 || nfrac > 15 || nint + nfrac > 18 {
            return None;
        }
    } else {
        f0 = i;
    }
    let mut v: i64 = 0;
    for &c in t[d0..d0 + nint].iter().chain(&t[f0..f0 + nfrac]) {
        v = v * 10 + i64::from(c - b'0');
    }
    let mut b0 = 0u8;
    let mut exp = 0i64;
    if i < n && (t[i] == b'e' || t[i] == b'E') {
        b0 = 1 | if t[i] == b'E' { 2 } else { 0 };
        i += 1;
        let mut eneg = false;
        if i < n && (t[i] == b'+' || t[i] == b'-') {
            if t[i] == b'+' {
                b0 |= 4;
            } else {
                eneg = true;
            }
            i += 1;
        }
        let e0 = i;
        while i < n && t[i].is_ascii_digit() {
            i += 1;
        }
        let x = i - e0;
        if x == 0 || x > 3 {
            return None;
        }
        b0 |= (x as u8) << 3;
        exp = t[e0..i].iter().fold(0i64, |a, c| a * 10 + i64::from(c - b'0'));
        if eneg {
            exp = -exp;
        }
    }
    if i != n {
        return None;
    }
    let num = Num { b0, b1: (nint | nfrac << 4) as u8, exp, v: if neg { -v } else { v } };
    let mut back = Vec::with_capacity(n);
    (fmt_num(&num, &mut back).is_some() && back == t).then_some(num)
}

/// DELTA payload applied to the previous number of the context.
fn apply_delta(prev: &Num, u: u64) -> Result<Num> {
    let mut next = *prev;
    if prev.b0 & 1 == 0 {
        next.v = prev.v.checked_add(unzz(u)).ok_or(Error("delta overflow"))?;
        return Ok(next);
    }
    let de = (u & 7) as i64 - 3;
    if de > 3 {
        return Err(Error("bad exponent delta"));
    }
    let e = prev.exp + de;
    let m = e.min(prev.exp);
    let scaled = i128::from(prev.v) * POW10[(prev.exp - m) as usize] + i128::from(unzz(u >> 3));
    let div = POW10[(e - m) as usize];
    if scaled % div != 0 {
        return Err(Error("delta not exact"));
    }
    next.v = i64::try_from(scaled / div).map_err(|_| Error("delta overflow"))?;
    next.exp = e;
    Ok(next)
}

/// The DELTA payload that takes `prev` to `num` (same shape), if any.
fn delta(prev: &Num, num: &Num) -> Option<u64> {
    if prev.b0 != num.b0 || prev.b1 != num.b1 {
        return None;
    }
    let u = if num.b0 & 1 == 0 {
        let d = num.v.checked_sub(prev.v)?;
        if d.unsigned_abs() >= 1 << 62 {
            return None;
        }
        zz(d)
    } else {
        let de = num.exp - prev.exp;
        if !(-3..=3).contains(&de) {
            return None;
        }
        let m = num.exp.min(prev.exp);
        let d = i128::from(num.v) * POW10[(num.exp - m) as usize] - i128::from(prev.v) * POW10[(prev.exp - m) as usize];
        if d.unsigned_abs() >= 1 << 59 {
            return None;
        }
        (zz(d as i64) << 3) | (de + 3) as u64
    };
    (apply_delta(prev, u).ok()? == *num).then_some(u)
}

// ------------------------------------------------------------------ varints

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or(Error("truncated"))?;
        self.pos += 1;
        Ok(b)
    }
    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return Err(Error("varint overflow"));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(Error("varint too long"))
    }
    fn bytes(&mut self, n: u64) -> Result<&'a [u8]> {
        let n = usize::try_from(n).map_err(|_| Error("length"))?;
        let end = self.pos.checked_add(n).filter(|e| *e <= self.buf.len()).ok_or(Error("truncated"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
}

// ------------------------------------------------------------------ decoder

#[derive(Clone)]
struct Ctx {
    text: Vec<u8>,
    num: Option<Num>,
}

type Key = (u8, u8, u8);

/// Decodes one block body into at most `max_out` bytes.
pub fn decode_block(body: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(max_out.min(body.len().saturating_mul(8)).min(1 << 20));
    let mut labels: Vec<&[u8]> = Vec::new();
    let mut ops_last: Vec<Option<Vec<u8>>> = Vec::new();
    let mut ctx: HashMap<Key, Ctx> = HashMap::new();
    let mut r = Reader { buf: body, pos: 0 };
    let room = |out: &Vec<u8>, n: usize| {
        if out.len().saturating_add(n) > max_out { Err(Error("block longer than its raw length")) } else { Ok(()) }
    };
    while r.pos < body.len() {
        let tag = r.byte()?;
        match tag {
            TAG_RAW => {
                let n = r.varint()?;
                let bytes = r.bytes(n)?;
                room(&out, bytes.len())?;
                out.extend_from_slice(bytes);
            }
            TAG_LABEL => {
                let n = r.varint()?;
                let name = r.bytes(n)?;
                if labels.len() >= MAX_LABELS {
                    return Err(Error("too many labels"));
                }
                labels.push(name);
                ops_last.push(None);
            }
            _ => {
                let lid = usize::from((tag - TAG_ROW) >> 1);
                let label = *labels.get(lid).ok_or(Error("undefined label"))?;
                let ops = if tag & 1 != 0 {
                    ops_last[lid].clone().ok_or(Error("no previous op list"))?
                } else {
                    let count = usize::try_from(r.varint()?).map_err(|_| Error("op count"))?;
                    if count == 0 || count > MAX_OPS {
                        return Err(Error("op count"));
                    }
                    let packed = r.bytes(count.div_ceil(2) as u64)?;
                    let ops: Vec<u8> = packed.iter().flat_map(|b| [b & 15, b >> 4]).take(count).collect();
                    ops_last[lid] = Some(ops.clone());
                    ops
                };
                decode_row(&mut r, &mut out, max_out, lid as u8, label, &ops, &mut ctx)?;
            }
        }
    }
    Ok(out)
}

fn decode_row(
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
    max_out: usize,
    lid: u8,
    label: &[u8],
    ops: &[u8],
    ctx: &mut HashMap<Key, Ctx>,
) -> Result<()> {
    let mut spans = [(0usize, 0usize); FIELDS];
    let mut k = 0;
    let next_op = |k: &mut usize| -> Result<u8> {
        let op = *ops.get(*k).ok_or(Error("op list too short"))?;
        *k += 1;
        Ok(op)
    };
    for f in 0..FIELDS {
        if f > 0 {
            out.push(b',');
        }
        let start = out.len();
        if f == 2 {
            out.extend_from_slice(label);
        } else {
            match next_op(&mut k)? {
                OP_EMPTY => {}
                OP_COPY => {
                    let g = usize::from(next_op(&mut k)?);
                    if g >= f {
                        return Err(Error("copy of a later field"));
                    }
                    let (a, b) = spans[g];
                    out.extend_from_within(a..b);
                }
                OP_QTEXT => {
                    let n = r.varint()?;
                    let inner = r.bytes(n)?;
                    out.push(b'"');
                    out.extend_from_slice(inner);
                    out.push(b'"');
                }
                OP_QVEC => {
                    let c = next_op(&mut k)?;
                    if c == 0 {
                        return Err(Error("empty vector"));
                    }
                    out.push(b'"');
                    for e in 0..c {
                        if e > 0 {
                            out.push(b' ');
                        }
                        let op = next_op(&mut k)?;
                        decode_token(r, out, ctx, (lid, f as u8, e), op)?;
                        if out.len() > max_out {
                            return Err(Error("block longer than its raw length"));
                        }
                    }
                    out.push(b'"');
                }
                op => decode_token(r, out, ctx, (lid, f as u8, 0), op)?,
            }
        }
        spans[f] = (start, out.len());
        if out.len() > max_out {
            return Err(Error("block longer than its raw length"));
        }
    }
    if k != ops.len() {
        return Err(Error("op list too long"));
    }
    out.extend_from_slice(b"\r\n");
    if out.len() > max_out {
        return Err(Error("block longer than its raw length"));
    }
    Ok(())
}

fn decode_token(r: &mut Reader<'_>, out: &mut Vec<u8>, ctx: &mut HashMap<Key, Ctx>, key: Key, op: u8) -> Result<()> {
    let num = match op {
        OP_SAME => {
            out.extend_from_slice(&ctx.get(&key).ok_or(Error("SAME without context"))?.text);
            return Ok(());
        }
        OP_DELTA => {
            let prev = ctx.get(&key).and_then(|c| c.num).ok_or(Error("DELTA without a number"))?;
            apply_delta(&prev, r.varint()?)?
        }
        OP_NUM => {
            let (b0, b1) = (r.byte()?, r.byte()?);
            let exp = if b0 & 1 != 0 { unzz(r.varint()?) } else { 0 };
            Num { b0, b1, exp, v: unzz(r.varint()?) }
        }
        OP_TEXT => {
            let n = r.varint()?;
            let text = r.bytes(n)?;
            out.extend_from_slice(text);
            ctx.insert(key, Ctx { text: text.to_vec(), num: None });
            return Ok(());
        }
        _ => return Err(Error("bad op")),
    };
    let start = out.len();
    fmt_num(&num, out).ok_or(Error("bad number"))?;
    ctx.insert(key, Ctx { text: out[start..].to_vec(), num: Some(num) });
    Ok(())
}

// ------------------------------------------------------------------ encoder

/// One CSV field: byte range (inside the quotes when quoted).
#[derive(Clone, Copy)]
struct Field {
    start: usize,
    end: usize,
    quoted: bool,
}

/// A 13-field record ending in CRLF at the start of `d`, as the firmware's
/// `split_row`: quoted fields with `""` escapes, unquoted fields without
/// quotes or line breaks. Returns the fields and the bytes consumed.
fn split_row(d: &[u8]) -> Option<([Field; FIELDS], usize)> {
    let len = d.len();
    let mut f = [Field { start: 0, end: 0, quoted: false }; FIELDS];
    let mut i = 0;
    for (k, field) in f.iter_mut().enumerate() {
        let start = i;
        if i < len && d[i] == b'"' {
            i += 1;
            loop {
                while i < len && d[i] != b'"' {
                    i += 1;
                }
                if i >= len {
                    return None;
                }
                if i + 1 < len && d[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                break;
            }
            *field = Field { start: start + 1, end: i, quoted: true };
            i += 1;
        } else {
            while i < len && !matches!(d[i], b',' | b'\r' | b'\n' | b'"') {
                i += 1;
            }
            *field = Field { start, end: i, quoted: false };
        }
        if i >= len {
            return None;
        }
        match d[i] {
            b',' if k + 1 < FIELDS => i += 1,
            b'\r' if k + 1 == FIELDS && i + 1 < len && d[i + 1] == b'\n' => return Some((f, i + 2)),
            _ => return None,
        }
    }
    None
}

/// The block encoder (RRLS outbox, tests). State starts empty per block.
#[derive(Default)]
pub struct Encoder {
    labels: Vec<Vec<u8>>,
    ops_last: Vec<Option<Vec<u8>>>,
    ctx: HashMap<Key, Ctx>,
}

impl Encoder {
    /// Encodes the block that starts at `data[0]`: items until at least
    /// `target` raw bytes are consumed or `data` ends (a trailing partial line
    /// is carried RAW). Returns (raw bytes consumed, SDX body).
    pub fn encode_block(data: &[u8], target: usize) -> (usize, Vec<u8>) {
        let mut enc = Self::default();
        let mut body = Vec::with_capacity(target / 4);
        let mut pos = 0;
        let mut raw_from = None::<usize>;
        let flush_raw = |body: &mut Vec<u8>, raw_from: &mut Option<usize>, to: usize| {
            if let Some(from) = raw_from.take() {
                body.push(TAG_RAW);
                put_varint(body, (to - from) as u64);
                body.extend_from_slice(&data[from..to]);
            }
        };
        while pos < data.len() && pos < target {
            if let Some((fields, used)) = split_row(&data[pos..])
                && let Some(row) = enc.row(&data[pos..], &fields)
            {
                flush_raw(&mut body, &mut raw_from, pos);
                body.extend_from_slice(&row);
                pos += used;
                continue;
            }
            let line = data[pos..].iter().position(|b| *b == b'\n').map_or(data.len(), |i| pos + i + 1);
            raw_from.get_or_insert(pos);
            pos = line;
        }
        flush_raw(&mut body, &mut raw_from, pos);
        (pos, body)
    }

    /// The items of one row (a LABEL first when it is new), or `None` when
    /// the row cannot be carried as a ROW (label quoted, empty, too long, or
    /// no label id left).
    fn row(&mut self, d: &[u8], f: &[Field; FIELDS]) -> Option<Vec<u8>> {
        let label = &d[f[2].start..f[2].end];
        if f[2].quoted || label.is_empty() || label.len() > ENCODER_LABEL_MAX {
            return None;
        }
        let mut out = Vec::new();
        let lid = match self.labels.iter().position(|l| l == label) {
            Some(lid) => lid,
            None if self.labels.len() < MAX_LABELS => {
                out.push(TAG_LABEL);
                put_varint(&mut out, label.len() as u64);
                out.extend_from_slice(label);
                self.labels.push(label.to_vec());
                self.ops_last.push(None);
                self.labels.len() - 1
            }
            None => return None,
        };
        let mut ops = Vec::with_capacity(16);
        let mut pay = Vec::new();
        let bytes = |x: &Field| &d[x.start..x.end];
        for k in 0..FIELDS {
            if k == 2 {
                continue;
            }
            let x = &f[k];
            let text = bytes(x);
            if (!text.is_empty() || x.quoted)
                && let Some(g) = (0..k).find(|g| f[*g].quoted == x.quoted && bytes(&f[*g]) == text)
            {
                ops.extend_from_slice(&[OP_COPY, g as u8]);
                continue;
            }
            if !x.quoted {
                ops.push(if text.is_empty() { OP_EMPTY } else { self.token((lid as u8, k as u8, 0), text, &mut pay) });
                continue;
            }
            let vec = !text.is_empty()
                && text[0] != b' '
                && text[text.len() - 1] != b' '
                && !text.contains(&b'"')
                && !text.windows(2).any(|w| w == b"  ");
            let count = text.iter().filter(|b| **b == b' ').count() + 1;
            if vec && count <= 15 {
                ops.extend_from_slice(&[OP_QVEC, count as u8]);
                for (e, tok) in text.split(|b| *b == b' ').enumerate() {
                    let op = self.token((lid as u8, k as u8, e as u8), tok, &mut pay);
                    ops.push(op);
                }
                continue;
            }
            ops.push(OP_QTEXT);
            put_varint(&mut pay, text.len() as u64);
            pay.extend_from_slice(text);
        }
        let same = self.ops_last[lid].as_deref() == Some(ops.as_slice());
        out.push(TAG_ROW + 2 * lid as u8 + u8::from(same));
        if !same {
            put_varint(&mut out, ops.len() as u64);
            out.extend(ops.chunks(2).map(|c| c[0] | c.get(1).copied().unwrap_or(0) << 4));
            self.ops_last[lid] = Some(ops);
        }
        out.extend_from_slice(&pay);
        Some(out)
    }

    /// One context-using token; mirrors the decoder's context updates.
    fn token(&mut self, key: Key, t: &[u8], pay: &mut Vec<u8>) -> u8 {
        let prev = self.ctx.get(&key);
        if prev.is_some_and(|c| c.text == t) {
            return OP_SAME;
        }
        let Some(num) = parse_num(t) else {
            put_varint(pay, t.len() as u64);
            pay.extend_from_slice(t);
            self.ctx.insert(key, Ctx { text: t.to_vec(), num: None });
            return OP_TEXT;
        };
        let op = match prev.and_then(|c| c.num).and_then(|p| delta(&p, &num)) {
            Some(u) => {
                put_varint(pay, u);
                OP_DELTA
            }
            None => {
                pay.extend_from_slice(&[num.b0, num.b1]);
                if num.b0 & 1 != 0 {
                    put_varint(pay, zz(num.exp));
                }
                put_varint(pay, zz(num.v));
                OP_NUM
            }
        };
        self.ctx.insert(key, Ctx { text: t.to_vec(), num: Some(num) });
        op
    }
}

#[cfg(test)]
#[path = "sdx_tests.rs"]
mod tests;
