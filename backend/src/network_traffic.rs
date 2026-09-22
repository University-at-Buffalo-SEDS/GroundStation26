//! Samples SEDSnet counters using monotonic time, independently of browser delivery.
use crate::types::{NetworkTopologyStats, NetworkTrafficSide, NetworkTrafficSnapshot};
use sedsnet::diagnostics::RuntimeSideStats;
use std::time::Instant;

#[derive(Default)]
pub struct Sampler {
    previous_at: Option<Instant>,
    snapshot: NetworkTrafficSnapshot,
}

impl Sampler {
    pub fn sample(&mut self, now: Instant, sides: &[RuntimeSideStats]) -> NetworkTrafficSnapshot {
        self.sample_sides(
            now,
            sides
                .iter()
                .map(|side| NetworkTrafficSide {
                    side_id: side.side_id,
                    name: side.side_name.clone(),
                    ingress_enabled: side.ingress_enabled,
                    egress_enabled: side.egress_enabled,
                    totals: NetworkTopologyStats {
                        packets_received: side.rx_packets,
                        packets_sent: side.tx_packets,
                        bytes_received: side.rx_bytes,
                        bytes_sent: side.tx_bytes,
                    },
                    delta: None,
                })
                .collect(),
        )
    }

    fn sample_sides(
        &mut self,
        now: Instant,
        mut sides: Vec<NetworkTrafficSide>,
    ) -> NetworkTrafficSnapshot {
        let interval_ms = self
            .previous_at
            .map(|at| now.saturating_duration_since(at).as_millis() as u64)
            .unwrap_or(0);
        // Multiple WebSockets / HTTP requests share the same measurement window.
        if self.previous_at.is_some() && interval_ms < 1000 {
            return self.snapshot.clone();
        }
        for side in &mut sides {
            side.delta = self
                .snapshot
                .sides
                .iter()
                .find(|old| old.side_id == side.side_id && old.name == side.name)
                .and_then(|old| difference(side.totals, old.totals));
        }
        self.previous_at = Some(now);
        self.snapshot = NetworkTrafficSnapshot { interval_ms, sides };
        self.snapshot.clone()
    }
}

fn difference(
    now: NetworkTopologyStats,
    old: NetworkTopologyStats,
) -> Option<NetworkTopologyStats> {
    Some(NetworkTopologyStats {
        packets_received: now.packets_received.checked_sub(old.packets_received)?,
        packets_sent: now.packets_sent.checked_sub(old.packets_sent)?,
        bytes_received: now.bytes_received.checked_sub(old.bytes_received)?,
        bytes_sent: now.bytes_sent.checked_sub(old.bytes_sent)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn side(id: usize, rx: u64, tx: u64) -> NetworkTrafficSide {
        NetworkTrafficSide {
            side_id: id,
            name: format!("side_{id}"),
            ingress_enabled: true,
            egress_enabled: true,
            totals: NetworkTopologyStats {
                packets_received: rx,
                bytes_received: rx * 40,
                packets_sent: tx,
                bytes_sent: tx * 80,
            },
            delta: None,
        }
    }

    #[test]
    fn counts_1500_packets_independently_of_50_browser_batches_and_clients() {
        let mut sampler = Sampler::default();
        let start = Instant::now();
        assert!(
            sampler.sample_sides(start, vec![side(0, 0, 0)]).sides[0]
                .delta
                .is_none()
        );
        for batch in 1..50 {
            // Extra callers must not shorten the shared measurement interval.
            let sample = sampler.sample_sides(
                start + Duration::from_millis(batch * 20),
                vec![side(0, batch * 30, batch)],
            );
            assert_eq!(sample.interval_ms, 0);
        }
        let sample = sampler.sample_sides(start + Duration::from_secs(1), vec![side(0, 1500, 50)]);
        assert_eq!(sample.interval_ms, 1000);
        let delta = sample.sides[0].delta.unwrap();
        assert_eq!(delta.packets_received, 1500);
        assert_eq!(delta.bytes_received, 60_000);
        assert_eq!(delta.packets_sent, 50);
        assert_eq!(delta.bytes_sent, 4000);
        // Delayed samples use the actual elapsed duration, then idle becomes zero.
        let sample = sampler.sample_sides(start + Duration::from_secs(3), vec![side(0, 4500, 150)]);
        assert_eq!(sample.interval_ms, 2000);
        assert_eq!(sample.sides[0].delta.unwrap().packets_received, 3000);
        let idle = sampler.sample_sides(start + Duration::from_secs(4), vec![side(0, 4500, 150)]);
        assert_eq!(idle.sides[0].delta, Some(NetworkTopologyStats::default()));
    }

    #[test]
    fn handles_independent_sides_resets_and_removal() {
        let mut sampler = Sampler::default();
        let start = Instant::now();
        sampler.sample_sides(start, vec![side(0, 100, 10), side(1, 200, 20)]);
        let next = sampler.sample_sides(
            start + Duration::from_secs(1),
            vec![side(1, 250, 23), side(0, 1, 1), side(2, 1000, 0)],
        );
        assert_eq!(next.sides[0].delta.unwrap().packets_received, 50);
        assert!(next.sides[1].delta.is_none());
        assert!(next.sides[2].delta.is_none());
        let next = sampler.sample_sides(start + Duration::from_secs(2), vec![side(0, 11, 2)]);
        assert_eq!(next.sides.len(), 1);
        assert_eq!(next.sides[0].delta.unwrap().packets_received, 10);
    }
}
