//! Test frames in the SEDSnet 7 side-transport layout, built from the fields
//! older tests wrote as SEDSnet 6 `"SDT" kind body CRC` frames: kind 1 (full
//! template: ULEB128 template id, then a canonical frame), kinds 2/4/5
//! (compact: a flags byte, ULEB128 template id, then the rest) and kind 3
//! (chunk: transfer u32, index u16, total u16, then the fragment).

fn uleb(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_uleb(bytes: &[u8], off: &mut usize) -> u64 {
    let (mut value, mut shift) = (0u64, 0);
    while let Some(&byte) = bytes.get(*off) {
        *off += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    value
}

/// A SEDSnet 7 side frame (CRC32 included) for a SEDSnet 6-style `kind` and
/// `body` (without magic, kind or CRC).
pub(crate) fn side_frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match kind {
        1 => {
            let mut off = 0;
            let id = read_uleb(body, &mut off);
            out.push(0x60);
            uleb((id & 0x0fff_ffff) << 3, &mut out);
            out.push(0); // base delta
            let rest = &body[off..];
            // A whole canonical frame drops its own CRC (the FULL frame's
            // CRC32 covers it); opaque test bytes are kept as they are.
            let canonical = rest.len() > 4
                && crc32fast::hash(&rest[..rest.len() - 4]).to_le_bytes() == rest[rest.len() - 4..];
            out.extend_from_slice(if canonical { &rest[..rest.len() - 4] } else { rest });
        }
        3 => {
            let transfer = body.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            let index = body.get(4..6).map_or(0, |b| u16::from_le_bytes(b.try_into().unwrap()));
            let total = body.get(6..8).map_or(1, |b| u16::from_le_bytes(b.try_into().unwrap())).max(1);
            out.push(0x61);
            out.extend_from_slice(&((transfer as u16).max(1)).to_le_bytes());
            uleb(u64::from(total) * (u64::from(total) - 1) / 2 + u64::from(index), &mut out);
            out.extend_from_slice(body.get(8..).unwrap_or(&[]));
        }
        _ => {
            let mut off = 1.min(body.len());
            let id = read_uleb(body, &mut off);
            out.push(0x70);
            uleb((id & 0x0fff_ffff) << 3, &mut out);
            out.extend_from_slice(body.get(off..).unwrap_or(&[]));
        }
    }
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// A SEDSnet 7 chunk frame of exactly `len` bytes (CRC32 included) for
/// fragment `index` of `total` in transfer `transfer`, zero-filled.
pub(crate) fn chunk_of_len(transfer: u32, index: u16, total: u16, len: usize) -> Vec<u8> {
    let mut out = vec![0x61];
    out.extend_from_slice(&((transfer % 0xffff) as u16 + 1).to_le_bytes());
    let total = u64::from(total.max(1));
    uleb(total * (total - 1) / 2 + u64::from(index), &mut out);
    assert!(out.len() + 4 <= len, "chunk too short for its header");
    out.resize(len - 4, 0);
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// The fragment index of a chunk frame (CRC not checked).
pub(crate) fn chunk_index(frame: &[u8]) -> Option<u16> {
    sedsnet::side_peek::side_chunk_unchecked(frame).map(|chunk| chunk.index)
}
