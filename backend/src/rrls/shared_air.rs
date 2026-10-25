//! Bounded shared-medium reference model for RFD qualification.
//!
//! This models byte capacity, UART delivery, finite buffers and link outages.
//! Round-robin contention is an explicit deterministic approximation, not the
//! proprietary RFD MAC, its collision backoff, FEC or retransmission algorithm.
//! FIFO sizes, framing overhead and turn time are required scenario inputs.
use std::collections::VecDeque;

#[derive(Clone, Copy)]
pub struct Config {
    pub air_bits_per_second: u32,
    pub uart_baud: u32,
    pub air_overhead_bits: u32,
    pub turn_us: u64,
    pub tx_fifo_bytes: usize,
    pub rx_fifo_bytes: usize,
    pub max_burst_bytes: usize,
}

struct Receive {
    ready_us: u64,
    bytes: Vec<u8>,
}
#[derive(Default)]
struct Node {
    tx: VecDeque<Vec<u8>>,
    tx_bytes: usize,
    rx: VecDeque<Receive>,
    rx_bytes: usize,
    uart_ready_us: u64,
    peak_tx: usize,
    peak_rx: usize,
    refused_tx: u64,
    dropped_rx: u64,
    lost_link: u64,
}
struct Active {
    sender: usize,
    end_us: u64,
    air_bits: u64,
    bytes: Vec<u8>,
}
pub struct SharedAir {
    config: Config,
    nodes: Vec<Node>,
    links: Vec<Vec<bool>>,
    active: Option<Active>,
    cursor: usize,
    now_us: u64,
    available_us: u64,
    pub completed_air_bits: u64,
}
impl SharedAir {
    pub fn new(ids: &[u16], config: Config) -> Result<Self, &'static str> {
        if ids.len() < 2 || ids.len() > 32 {
            return Err("a shared radio scenario needs2..32nodes");
        }
        if ids.iter().any(|&id| id == 0 || id > 32767)
            || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != ids.len()
        {
            return Err("RFD nodes need distinct unicast IDs in1..32767");
        }
        if config.air_bits_per_second == 0
            || config.uart_baud == 0
            || config.max_burst_bytes == 0
            || config.tx_fifo_bytes < config.max_burst_bytes
            || config.rx_fifo_bytes < config.max_burst_bytes
        {
            return Err("scenario rates and finite FIFO limits must be valid");
        }
        Ok(Self {
            config,
            nodes: (0..ids.len()).map(|_| Node::default()).collect(),
            links: vec![vec![true; ids.len()]; ids.len()],
            active: None,
            cursor: 0,
            now_us: 0,
            available_us: 0,
            completed_air_bits: 0,
        })
    }
    pub fn set_link(&mut self, sender: usize, receiver: usize, available: bool) {
        self.links[sender][receiver] = available;
    }
    /// On refusal the caller retains ownership, matching a failed serial/SDK
    /// admission. Accepted bytes, including the current air transfer, are bounded.
    pub fn submit(&mut self, sender: usize, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        let node = &mut self.nodes[sender];
        if bytes.is_empty()
            || bytes.len() > self.config.max_burst_bytes
            || bytes.len() > self.config.tx_fifo_bytes.saturating_sub(node.tx_bytes)
        {
            node.refused_tx += 1;
            return Err(bytes);
        }
        node.tx_bytes += bytes.len();
        node.peak_tx = node.peak_tx.max(node.tx_bytes);
        node.tx.push_back(bytes);
        Ok(())
    }
    pub fn advance_to(&mut self, target_us: u64) -> Result<(), &'static str> {
        if target_us < self.now_us {
            return Err("shared radio time cannot run backwards");
        }
        loop {
            if let Some(active) = self.active.take() {
                if active.end_us > target_us {
                    self.active = Some(active);
                    break;
                }
                self.now_us = active.end_us;
                self.nodes[active.sender].tx_bytes -= active.bytes.len();
                self.completed_air_bits += active.air_bits;
                for receiver in 0..self.nodes.len() {
                    if receiver == active.sender {
                        continue;
                    }
                    let node = &mut self.nodes[receiver];
                    if !self.links[active.sender][receiver] {
                        node.lost_link += 1;
                        continue;
                    }
                    if active.bytes.len() > self.config.rx_fifo_bytes.saturating_sub(node.rx_bytes)
                    {
                        node.dropped_rx += 1;
                        continue;
                    }
                    let wire_us = (active.bytes.len() as u64 * 10_000_000)
                        .div_ceil(u64::from(self.config.uart_baud));
                    node.uart_ready_us = node.uart_ready_us.max(active.end_us) + wire_us;
                    node.rx_bytes += active.bytes.len();
                    node.peak_rx = node.peak_rx.max(node.rx_bytes);
                    node.rx.push_back(Receive {
                        ready_us: node.uart_ready_us,
                        bytes: active.bytes.clone(),
                    });
                }
                self.available_us = active.end_us.saturating_add(self.config.turn_us);
            }
            let start_us = self.available_us.max(self.now_us);
            if start_us > target_us {
                break;
            }
            let sender = (0..self.nodes.len())
                .map(|offset| (self.cursor + offset) % self.nodes.len())
                .find(|&sender| !self.nodes[sender].tx.is_empty());
            let Some(sender) = sender else {
                break;
            };
            let bytes = self.nodes[sender].tx.pop_front().unwrap();
            let air_bits = bytes.len() as u64 * 8 + u64::from(self.config.air_overhead_bits);
            let end_us = start_us
                + (air_bits * 1_000_000).div_ceil(u64::from(self.config.air_bits_per_second));
            self.active = Some(Active {
                sender,
                end_us,
                air_bits,
                bytes,
            });
            self.cursor = (sender + 1) % self.nodes.len();
        }
        self.now_us = target_us;
        Ok(())
    }
    /// A burst becomes visible only after its final UART stop bit. This is a
    /// conservative burst-level delivery bound, not per-byte UART timing.
    pub fn receive(&mut self, receiver: usize) -> Option<Vec<u8>> {
        let node = &mut self.nodes[receiver];
        if node.rx.front()?.ready_us > self.now_us {
            return None;
        }
        let packet = node.rx.pop_front().unwrap();
        node.rx_bytes -= packet.bytes.len();
        Some(packet.bytes)
    }
    pub fn counters(&self, node: usize) -> (usize, usize, u64, u64, u64) {
        let node = &self.nodes[node];
        (
            node.peak_tx,
            node.peak_rx,
            node.refused_tx,
            node.dropped_rx,
            node.lost_link,
        )
    }
    pub fn queued_bytes(&self, node: usize) -> (usize, usize) {
        (self.nodes[node].tx_bytes, self.nodes[node].rx_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config {
            air_bits_per_second: 64_000,
            uart_baud: 115_200,
            air_overhead_bits: 0,
            turn_us: 0,
            tx_fifo_bytes: 4096,
            rx_fifo_bytes: 4096,
            max_burst_bytes: 128,
        }
    }
    #[test]
    fn rejects_ambiguous_node_identity_and_invalid_limits() {
        assert!(SharedAir::new(&[4, 4, 5], config()).is_err());
        assert!(SharedAir::new(&[0, 1], config()).is_err());
        let mut c = config();
        c.tx_fifo_bytes = 127;
        assert!(SharedAir::new(&[1, 2], c).is_err());
    }
    #[test]
    fn a_broadcast_uses_air_once_and_reaches_both_other_modems() {
        let mut air = SharedAir::new(&[1, 2, 3], config()).unwrap();
        air.submit(1, vec![7; 80]).unwrap(); //10ms of64kbps, then6.945ms ofUART
        air.advance_to(10_000).unwrap();
        assert!(air.receive(0).is_none());
        air.advance_to(16_945).unwrap();
        assert_eq!(air.receive(0), Some(vec![7; 80]));
        assert_eq!(air.receive(2), Some(vec![7; 80]));
        assert!(air.receive(1).is_none());
        assert_eq!(air.completed_air_bits, 640);
    }
    #[test]
    fn simultaneous_three_node_bursts_share_capacity_without_starvation() {
        let mut air = SharedAir::new(&[1, 2, 3], config()).unwrap();
        for node in 0..3 {
            for _ in 0..16 {
                air.submit(node, vec![node as u8; 80]).unwrap();
            }
        }
        air.advance_to(30_000).unwrap();
        assert_eq!(air.completed_air_bits, 1920);
        for node in &air.nodes {
            assert_eq!(node.tx_bytes, 15 * 80);
        }
        air.advance_to(100_000).unwrap();
        assert!(
            air.completed_air_bits <= 6400,
            "broadcast recipients cannot multiply air capacity"
        );
    }
    #[test]
    fn finite_tx_fifo_keeps_the_refused_payload_owned_by_the_caller() {
        let mut c = config();
        c.tx_fifo_bytes = 256;
        let mut air = SharedAir::new(&[1, 2], c).unwrap();
        air.submit(0, vec![1; 128]).unwrap();
        air.submit(0, vec![2; 128]).unwrap();
        let packet = vec![3; 128];
        let pointer = packet.as_ptr();
        let refused = air.submit(0, packet).unwrap_err();
        assert_eq!(refused.as_ptr(), pointer);
        air.advance_to(1).unwrap();
        assert_eq!(air.queued_bytes(0).0, 256);
        air.advance_to(16_000).unwrap();
        assert_eq!(air.queued_bytes(0).0, 128);
        air.submit(0, refused).unwrap();
        assert_eq!(air.nodes[0].peak_tx, 256);
    }
    #[test]
    fn receiver_backpressure_is_finite_and_visible_in_counters() {
        let mut c = config();
        c.rx_fifo_bytes = 128;
        let mut air = SharedAir::new(&[1, 2], c).unwrap();
        air.submit(0, vec![1; 128]).unwrap();
        air.submit(0, vec![2; 128]).unwrap();
        air.advance_to(50_000).unwrap();
        assert_eq!(air.nodes[1].dropped_rx, 1);
        assert_eq!(air.nodes[1].peak_rx, 128);
        assert_eq!(air.receive(1), Some(vec![1; 128]));
        assert!(air.receive(1).is_none());
        air.submit(0, vec![3; 128]).unwrap();
        air.advance_to(100_000).unwrap();
        assert_eq!(air.receive(1), Some(vec![3; 128]));
    }
    #[test]
    fn direct_outage_requires_an_explicit_application_relay_and_recovers() {
        let mut air = SharedAir::new(&[1, 2, 3], config()).unwrap();
        air.set_link(1, 0, false);
        air.submit(1, vec![9; 80]).unwrap();
        air.advance_to(20_000).unwrap();
        assert!(air.receive(0).is_none());
        let relay = air.receive(2).unwrap();
        assert_eq!(air.nodes[0].lost_link, 1);
        air.submit(2, relay).unwrap();
        air.advance_to(40_000).unwrap();
        assert_eq!(air.receive(0), Some(vec![9; 80]));
        assert_eq!(
            air.completed_air_bits, 1280,
            "two-hop application relay costs two air transfers"
        );
        air.set_link(1, 0, true);
        air.submit(1, vec![8; 80]).unwrap();
        air.advance_to(60_000).unwrap();
        assert_eq!(air.receive(0), Some(vec![8; 80]));
    }
    #[test]
    fn frozen_clock_cannot_drain_air_credit_and_idle_time_cannot_bank_it() {
        let mut air = SharedAir::new(&[1, 2], config()).unwrap();
        air.advance_to(1_000_000).unwrap();
        air.submit(0, vec![1; 80]).unwrap();
        for _ in 0..100 {
            air.advance_to(1_000_000).unwrap();
        }
        assert_eq!(air.completed_air_bits, 0);
        assert!(air.advance_to(999_999).is_err());
        air.advance_to(1_010_000).unwrap();
        assert_eq!(air.completed_air_bits, 640);
    }
    #[test]
    fn overhead_and_turnaround_reduce_service_instead_of_creating_capacity() {
        let mut c = config();
        c.air_overhead_bits = 160;
        c.turn_us = 2500;
        let mut air = SharedAir::new(&[1, 2], c).unwrap();
        air.submit(0, vec![1; 80]).unwrap();
        air.submit(0, vec![2; 80]).unwrap();
        air.advance_to(12_499).unwrap();
        assert_eq!(air.completed_air_bits, 0);
        air.advance_to(12_500).unwrap();
        assert_eq!(air.completed_air_bits, 800);
        air.advance_to(27_499).unwrap();
        assert_eq!(air.completed_air_bits, 800);
        air.advance_to(27_500).unwrap();
        assert_eq!(air.completed_air_bits, 1600);
    }
}
