//! Serial-driver completion for the linked emulator's pseudo-terminal.
//!
//! A physical UART's tcdrain waits for its configured 8N1 wire. A PTY reports
//! completion immediately, before Renode has serialized those bytes. Retain
//! subsequent frames in the priority queue until that virtual wire has room.
#[derive(Default)]
pub(super) struct WireDrain {
    ready_us: u64,
}

impl WireDrain {
    // The linked emulator publishes its shared clock every10ms. Permit at
    // most that much lookahead, rather than reducing throughput by rounding
    // every small frame up to a whole clock quantum. Idle time earns no credit.
    const CLOCK_QUANTUM_US: u64 = 10_000;

    pub(super) fn ready(&self, now_ms: u64) -> bool {
        self.ready_us
            <= now_ms
                .saturating_mul(1_000)
                .saturating_add(Self::CLOCK_QUANTUM_US)
    }

    /// Accumulated modeled stop bit, including previously accepted UART work.
    pub(super) fn stop_us(&self) -> u64 {
        self.ready_us
    }

    pub(super) fn committed(&mut self, now_ms: u64, bytes: usize, baud: u32) {
        let wire_us = (bytes as u64)
            .saturating_mul(10_000_000) // start +8data +stop bits, in microseconds
            .div_ceil(u64::from(baud.max(1)));
        self.ready_us = self
            .ready_us
            .max(now_ms.saturating_mul(1_000))
            .saturating_add(wire_us);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_virtual_clock_cannot_accept_an_unbounded_serial_burst() {
        let mut wire = WireDrain::default();
        let mut sent = 0;
        while wire.ready(0) {
            wire.committed(0, 192, 115_200);
            sent += 1;
        }
        assert_eq!(sent, 1);
        for _ in 0..100 {
            assert!(!wire.ready(0));
        }
        assert!(wire.ready(10));
    }

    #[test]
    fn small_frames_keep_uart_throughput_without_accumulating_idle_credit() {
        let mut wire = WireDrain::default();
        let mut bytes = 0;
        for now in (0..=1_000).step_by(10) {
            while wire.ready(now) {
                wire.committed(now, 47, 115_200);
                bytes += 47;
            }
        }
        // One-second UART service plus one quantized clock interval and frame.
        assert!(bytes >= 11_520, "UART was artificially slowed: {bytes}");
        assert!(bytes <= 11_520 + 116 + 47, "unbounded lookahead: {bytes}");
        let mut sent_after_idle = 0;
        while wire.ready(60_000) {
            wire.committed(60_000, 47, 115_200);
            sent_after_idle += 1;
        }
        assert_eq!(sent_after_idle, 3);
    }

    #[test]
    fn only_committed_bytes_consume_credit_and_clock_reversal_does_not_free_it() {
        let mut wire = WireDrain::default();
        assert!(wire.ready(100));
        assert!(wire.ready(100));
        wire.committed(100, 1_024, 115_200);
        assert!(!wire.ready(100));
        assert!(!wire.ready(0));
        assert!(wire.ready(180));
    }
}
