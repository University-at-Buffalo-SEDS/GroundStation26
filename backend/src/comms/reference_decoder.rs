//! Test-only adapter: reference radio scenarios must use the production stream
//! decoder rather than assume every modem burst is one application frame.
use super::*;
#[derive(Default)]
pub(crate) struct Decoder {
    buffer: Vec<u8>,
    wait: RawUartFrameWait,
    pub(crate) rejected: usize,
    pub(crate) peak_bytes: usize,
}
impl Decoder {
    pub(crate) fn push(&mut self, bytes: &[u8], now_ms: u64) -> Vec<(bool, Vec<u8>)> {
        self.buffer.extend_from_slice(bytes);
        self.peak_bytes = self.peak_bytes.max(self.buffer.len());
        let mut frames = Vec::new();
        // Each iteration must consume bytes, change recovery state, return a
        // frame, or stop. A broken fixture cannot spin forever in the harness.
        for _ in 0..=RAW_UART_MAX_FRAME_BYTES {
            let before = (self.buffer.len(), self.wait.failed_prefix);
            match take_raw_uart_framed_payload_with_clock(
                &mut self.buffer,
                &mut self.wait,
                Instant::now(),
                Some(now_ms),
            ) {
                Ok(Some((RawUartFrameKind::Data, bytes))) => frames.push((false, bytes)),
                Ok(Some((RawUartFrameKind::Command, bytes))) => frames.push((true, bytes)),
                Ok(Some((RawUartFrameKind::Ascii, _))) => {}
                Ok(None) => return frames,
                Err(_) => self.rejected += 1,
            }
            if before == (self.buffer.len(), self.wait.failed_prefix) {
                return frames;
            }
        }
        panic!("production stream decoder made no bounded progress");
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(value: u8) -> Vec<u8> {
        let mut body = b"SDT\x03".to_vec();
        body.extend_from_slice(&42u32.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[value; 3]);
        body.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        body
    }
    #[test]
    fn split_and_coalesced_modem_reads_preserve_two_frames() {
        let a = chunk(1);
        let b = chunk(2);
        let bytes = [
            build_raw_uart_frame(&a).unwrap(),
            build_raw_uart_frame(&b).unwrap(),
        ]
        .concat();
        for width in [1, 3, 7, bytes.len()] {
            let mut decoder = Decoder::default();
            let mut frames = Vec::new();
            for (i, part) in bytes.chunks(width).enumerate() {
                frames.extend(decoder.push(part, i as u64 * 10));
            }
            assert_eq!(frames, vec![(false, a.clone()), (false, b.clone())]);
            assert_eq!(decoder.rejected, 0);
        }
    }
    #[test]
    fn crc_rejected_modem_burst_does_not_consume_the_following_frame() {
        let mut bad = chunk(1);
        *bad.last_mut().unwrap() ^= 0x80;
        let good = chunk(2);
        let bytes = [
            build_raw_uart_frame(&bad).unwrap(),
            build_raw_uart_frame(&good).unwrap(),
        ]
        .concat();
        let mut decoder = Decoder::default();
        let frames = decoder.push(&bytes, 0);
        assert_eq!(frames, vec![(false, good)]);
        assert_eq!(decoder.rejected, 1);
    }
}
