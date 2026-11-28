//! A minimal ZIP writer (deflate, no ZIP64) for the export bundles, which
//! stay well under 4 GiB and 65 535 entries.
use std::io::Write;

pub(crate) struct Zip {
    out: Vec<u8>,
    central: Vec<u8>,
    entries: u16,
    dos_time: u16,
    dos_date: u16,
}

impl Zip {
    /// `now_ms`: the entries' modification time (UTC).
    pub(crate) fn new(now_ms: i64) -> Self {
        let at = time::OffsetDateTime::from_unix_timestamp(now_ms.div_euclid(1000)).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        let year = (at.year() - 1980).clamp(0, 127) as u16;
        Self {
            out: Vec::new(),
            central: Vec::new(),
            entries: 0,
            dos_time: ((at.hour() as u16) << 11) | ((at.minute() as u16) << 5) | (at.second() as u16 / 2),
            dos_date: (year << 9) | ((u8::from(at.month()) as u16) << 5) | at.day() as u16,
        }
    }

    pub(crate) fn add(&mut self, name: &str, data: &[u8]) {
        self.add_entry(name, data, true);
    }

    /// An entry stored without compression: media that is compressed
    /// already (MP4, Opus), where deflate only spends CPU.
    pub(crate) fn add_stored(&mut self, name: &str, data: &[u8]) {
        self.add_entry(name, data, false);
    }

    fn add_entry(&mut self, name: &str, data: &[u8], deflate: bool) {
        let compressed = if deflate {
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            let _ = encoder.write_all(data);
            encoder.finish().unwrap_or_default()
        } else {
            Vec::new()
        };
        let (method, body): (u16, &[u8]) = if deflate && compressed.len() < data.len() {
            (8, &compressed)
        } else {
            (0, data)
        };
        let crc = crc32fast::hash(data);
        let offset = self.out.len() as u32;
        let name = name.as_bytes();
        let header = |sig: u32, central: bool| {
            let mut h = Vec::with_capacity(46 + name.len());
            h.extend_from_slice(&sig.to_le_bytes());
            if central {
                h.extend_from_slice(&20u16.to_le_bytes()); // made by
            }
            h.extend_from_slice(&20u16.to_le_bytes()); // needed
            h.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
            h.extend_from_slice(&method.to_le_bytes());
            h.extend_from_slice(&self.dos_time.to_le_bytes());
            h.extend_from_slice(&self.dos_date.to_le_bytes());
            h.extend_from_slice(&crc.to_le_bytes());
            h.extend_from_slice(&(body.len() as u32).to_le_bytes());
            h.extend_from_slice(&(data.len() as u32).to_le_bytes());
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // extra
            if central {
                h.extend_from_slice(&0u16.to_le_bytes()); // comment
                h.extend_from_slice(&0u16.to_le_bytes()); // disk
                h.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
                h.extend_from_slice(&0u32.to_le_bytes()); // external attrs
                h.extend_from_slice(&offset.to_le_bytes());
            }
            h.extend_from_slice(name);
            h
        };
        let local = header(0x0403_4b50, false);
        let central = header(0x0201_4b50, true);
        self.out.extend_from_slice(&local);
        self.out.extend_from_slice(body);
        self.central.extend_from_slice(&central);
        self.entries += 1;
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        let offset = self.out.len() as u32;
        let size = self.central.len() as u32;
        self.out.append(&mut self.central);
        self.out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        self.out.extend_from_slice(&self.entries.to_le_bytes());
        self.out.extend_from_slice(&self.entries.to_le_bytes());
        self.out.extend_from_slice(&size.to_le_bytes());
        self.out.extend_from_slice(&offset.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        self.out
    }
}

/// Reads a bundle back: (name, contents) per entry. Tests only.
#[cfg(test)]
pub(crate) fn read(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    use std::io::Read;
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]) as usize;
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
    let end = bytes.len() - 22;
    assert_eq!(u32_at(end), 0x0605_4b50);
    let (count, mut at) = (u16_at(end + 10), u32_at(end + 16));
    let mut out = Vec::new();
    for _ in 0..count {
        assert_eq!(u32_at(at), 0x0201_4b50);
        let (method, crc, csize, name_len, offset) = (u16_at(at + 10), u32_at(at + 16), u32_at(at + 20), u16_at(at + 28), u32_at(at + 42));
        let name = String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).unwrap();
        let local_name = u16_at(offset + 26);
        let start = offset + 30 + local_name + u16_at(offset + 28);
        let body = &bytes[start..start + csize];
        let data = if method == 8 {
            let mut d = Vec::new();
            flate2::read::DeflateDecoder::new(body).read_to_end(&mut d).unwrap();
            d
        } else {
            body.to_vec()
        };
        assert_eq!(crc32fast::hash(&data) as usize, crc, "{name}");
        out.push((name, data));
        at += 46 + name_len;
    }
    out
}
