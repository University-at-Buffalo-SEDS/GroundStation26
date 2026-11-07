//! Test-only adapter: reference radio scenarios must use the production stream
//! decoder rather than assume every modem burst is one application frame.
use super::*;

/// Use the maintained raw-UART builder for test-only data frames.
#[cfg(test)]
pub(crate) fn frame_data(bytes: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    build_raw_uart_frame(bytes)
}

#[derive(Default)]
pub(crate) struct Decoder {
    buffer: Vec<u8>,
    wait: RawUartFrameWait,
    pub(crate) rejected: usize,
    pub(crate) peak_bytes: usize,
}
impl Decoder {
    #[cfg(all(
        target_os = "linux",
        any(feature = "firmware_sim", feature = "hitl_mode")
    ))]
    pub(crate) fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }
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
    fn rr(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![b'R', b'R', 1, kind];
        body.extend_from_slice(payload);
        body.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        body
    }
    fn nested_revoke() -> Vec<u8> {
        use crate::rrls::budget_negotiation::{Context, Message, Nonce, encode};
        build_raw_uart_command_frame(
            encode(Message::Revoke(Context {
                session: 7,
                seq: 1,
                nonce: Nonce::new([8; 16]).unwrap(),
            }))
            .wire(),
        )
        .unwrap()
    }
    #[test]
    fn valid_rr_parent_never_exposes_nested_valid_protocol_frame() {
        let nested = nested_revoke();
        let parent = rr(10, &nested);
        for wire in [
            build_raw_uart_frame(&parent).unwrap(),
            build_raw_uart_command_frame(&parent).unwrap(),
        ] {
            let command = wire[..2] == [0xa6, 0x5b];
            let mut decoder = Decoder::default();
            let frames = decoder.push(&wire, 0);
            assert_eq!(frames, vec![(command, parent.clone())]);
            assert_eq!(decoder.rejected, 0);
            assert!(decoder.buffer.is_empty());
        }
    }
    #[test]
    fn unexpired_incomplete_rr_parent_never_scans_nested_protocol() {
        let parent = rr(10, &nested_revoke());
        let wire = build_raw_uart_command_frame(&parent).unwrap();
        let split = wire.len() - 2;
        let mut decoder = Decoder::default();
        assert!(decoder.push(&wire[..split], 0).is_empty());
        assert!(decoder.push(&[], 1).is_empty());
        assert_eq!(decoder.rejected, 0);
        assert_eq!(decoder.push(&wire[split..], 2), vec![(true, parent)]);
        assert!(decoder.buffer.is_empty());
    }
    #[test]
    fn rejected_rr_parent_preserves_complete_valid_rr_anchors_inside_failed_span() {
        let next = nested_revoke();
        let retry_body = rr(10, &[0x77; 80]);
        let retry = build_raw_uart_command_frame(&retry_body).unwrap();
        for parent in [build_raw_uart_frame(&retry_body).unwrap(), retry.clone()] {
            let wire = [parent[..7].to_vec(), next.clone(), retry.clone()].concat();
            let mut decoder = Decoder::default();
            let mut frames = Vec::new();
            for (i, part) in wire.chunks(7).enumerate() {
                frames.extend(decoder.push(part, i as u64 * 5));
            }
            let next_len = u16::from_le_bytes(next[2..4].try_into().unwrap()) as usize;
            assert_eq!(
                frames,
                vec![
                    (true, next[4..4 + next_len].to_vec()),
                    (true, retry_body.clone())
                ]
            );
            assert_eq!(decoder.rejected, 1);
            assert!(decoder.buffer.is_empty());
        }
    }
}
