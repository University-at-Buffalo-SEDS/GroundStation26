//! I2C v2: one immediate packet per transaction, with a four-byte header.
//! A header-only read peeks; only a completed body read consumes the mailbox.
pub const MAGIC: u8 = 0xD2;
pub const HEADER_LEN: usize = 4;
pub const MAX_PAYLOAD: usize = 4092;
pub const IDLE: u8 = 0;
pub const DATA: u8 = 1;
pub const COMMAND: u8 = 2;
pub const BOOTSEL: u8 = 0x7E;
pub const ERROR: u8 = 0x7F;
pub const SELECT: [u8; 4] = [MAGIC, IDLE, 0, 0];

pub fn header(kind: u8, len: usize) -> Result<[u8; 4], &'static str> {
    if len > MAX_PAYLOAD
        || !matches!(kind, IDLE | DATA | COMMAND | BOOTSEL | ERROR)
        || (kind == IDLE && len != 0)
    {
        return Err("invalid I2C v2 header");
    }
    Ok([MAGIC, kind, len as u8, (len >> 8) as u8])
}
pub fn decode(raw: &[u8]) -> Result<(u8, usize), &'static str> {
    if raw.len() < HEADER_LEN || raw[0] != MAGIC {
        return Err("I2C v2 firmware required: unexpected packet header");
    }
    let len = u16::from_le_bytes([raw[2], raw[3]]) as usize;
    header(raw[1], len)?;
    Ok((raw[1], len))
}
pub fn packet(raw: &[u8]) -> Result<(u8, &[u8]), &'static str> {
    let (kind, len) = decode(raw)?;
    if raw.len() != HEADER_LEN + len {
        return Err("incomplete I2C v2 packet");
    }
    Ok((kind, &raw[HEADER_LEN..]))
}

pub struct Mailbox {
    bytes: [u8; MAX_PAYLOAD],
    len: usize,
    kind: u8,
}
impl Mailbox {
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_PAYLOAD],
            len: 0,
            kind: IDLE,
        }
    }
    pub fn pending(&self) -> bool {
        self.kind != IDLE
    }
    pub fn stage(&mut self, kind: u8, payload: &[u8]) -> Result<(), &'static str> {
        header(kind, payload.len())?;
        if kind != IDLE && payload.is_empty() {
            return Err("empty I2C mailbox packet");
        }
        if self.pending() {
            return Err("I2C mailbox occupied");
        }
        self.bytes[..payload.len()].copy_from_slice(payload);
        self.kind = kind;
        self.len = payload.len();
        Ok(())
    }
    pub fn header(&self) -> [u8; 4] {
        header(self.kind, self.len).unwrap()
    }
    pub fn payload(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
    pub fn complete_body(&mut self, complete: bool) {
        if complete {
            self.kind = IDLE;
            self.len = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_boundaries_and_versions() {
        for len in [0, 1, 14, 35, 256, MAX_PAYLOAD] {
            assert_eq!(decode(&header(DATA, len).unwrap()), Ok((DATA, len)));
        }
        assert!(header(DATA, MAX_PAYLOAD + 1).is_err());
        assert!(header(IDLE, 1).is_err());
        assert!(decode(&[0x49, 0x32, 1, 1]).is_err());
        assert!(decode(&[MAGIC, 3, 0, 0]).is_err());
        assert!(packet(&[MAGIC, DATA, 2, 0, 42]).is_err());
        assert!(packet(&[MAGIC, DATA, 0, 0, 42]).is_err());
    }
    #[test]
    fn peeks_and_aborted_reads_retain_exact_packet() {
        let mut m = Mailbox::new();
        assert_eq!(m.header(), SELECT);
        let payload = [0xA5; MAX_PAYLOAD];
        m.stage(DATA, &payload).unwrap();
        assert!(m.stage(DATA, b"next").is_err());
        for _ in 0..3 {
            assert_eq!(decode(&m.header()).unwrap(), (DATA, MAX_PAYLOAD));
            assert_eq!(m.payload(), payload);
            m.complete_body(false);
        }
        m.complete_body(true);
        assert!(!m.pending());
        m.stage(DATA, b"next").unwrap();
        assert_eq!(m.payload(), b"next");
    }
    #[test]
    fn short_message_uses_one_body_without_fragment_padding() {
        let payload = [9; 35];
        let mut frame = [0; HEADER_LEN + 35];
        frame[..4].copy_from_slice(&header(DATA, payload.len()).unwrap());
        frame[4..].copy_from_slice(&payload);
        assert_eq!(packet(&frame).unwrap(), (DATA, &payload[..]));
        // A peek plus the full frame is 43 bytes rather than three 32-byte slots.
        assert_eq!(HEADER_LEN + frame.len(), 43);
    }
}
