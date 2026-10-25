//! Capacity backpressure before UART offload, independent of modem MAC/turns.
//! Retain the next frame in its priority queue until one bounded airtime share
//! has drained. No AUX, GRANT, receive windows, accumulated idle credit or sleep.
use std::time::{Duration, Instant};

pub(super) struct Capacity {
    next: Option<(Instant, Option<u64>)>,
    air_bps: u64,
    share_percent: u64,
    overhead_bytes: u64,
}
fn bounded_env(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (min..=max).contains(value))
        .unwrap_or(default)
}
impl Capacity {
    pub(super) fn from_env() -> Self {
        let recovery = matches!(
            std::env::var("GS_RRLS_ROLE").as_deref(),
            Ok("primary" | "rrls")
        ) || std::env::var("GS_RRLS_VOICE").as_deref() == Ok("1");
        Self::new(
            bounded_env("GS_RADIO_AIR_BIT_RATE_BPS", 64_000, 1_200, 1_000_000),
            bounded_env(
                "GS_RADIO_AIR_SHARE_PERCENT",
                if recovery { 25 } else { 40 },
                1,
                100,
            ),
            bounded_env("GS_RADIO_AIR_FRAME_OVERHEAD_BYTES", 16, 0, 256),
        )
    }
    fn new(air_bps: u64, share_percent: u64, overhead_bytes: u64) -> Self {
        Self {
            next: None,
            air_bps,
            share_percent,
            overhead_bytes,
        }
    }
    pub(super) fn ready(&self, wall: Instant, virtual_ms: Option<u64>) -> bool {
        self.next.is_none_or(
            |(next_wall, next_virtual)| match (virtual_ms, next_virtual) {
                (Some(now), Some(next)) => now >= next,
                _ => wall >= next_wall,
            },
        )
    }
    pub(super) fn committed(
        &mut self,
        wall: Instant,
        virtual_ms: Option<u64>,
        framed_bytes: usize,
    ) {
        let bps = (self.air_bps as u128 * self.share_percent as u128 / 100).max(1);
        let millis = ((framed_bytes as u128 + self.overhead_bytes as u128) * 8 * 1000)
            .div_ceil(bps)
            .min(u64::MAX as u128) as u64;
        let duration = Duration::from_millis(millis);
        self.next = Some((
            wall.checked_add(duration).unwrap_or(wall),
            virtual_ms.map(|now| now.saturating_add(millis)),
        ));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_virtual_time_and_idle_never_earn_extra_uart_burst_credit() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64000, 40, 16);
        assert!(cap.ready(wall, Some(0)));
        cap.committed(wall, Some(0), 196);
        assert!(!cap.ready(wall + Duration::from_secs(60), Some(66)));
        assert!(cap.ready(wall, Some(67)));
        cap.committed(wall, Some(10000), 196);
        assert!(!cap.ready(wall, Some(10000)));
        assert!(!cap.ready(wall, Some(10066)));
        assert!(cap.ready(wall, Some(10067)));
    }
    #[test]
    fn uart_only_floods_finite_modem_but_capacity_keeps_command_in_host_queue() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64000, 40, 16);
        // Shared-air receiver consumes2.8KiB/s of this sender's stream after
        // contention/overhead. UART-only115200 admission outruns that capacity.
        fn simulate(mut cap: Option<&mut Capacity>, wall: Instant) -> (usize, u64) {
            let mut modem = std::collections::VecDeque::<(u64, bool)>::new();
            let mut bulk = 1000usize;
            let mut command_pending = false;
            let mut command_sent = false;
            let mut peak = 0;
            let mut received = None;
            let mut uart_ready = 0;
            let mut air_ready = 0;
            for now in 0..2000u64 {
                if now == 500 {
                    command_pending = true;
                }
                if modem.front().is_some() && now >= air_ready {
                    let (_, command) = modem.pop_front().unwrap();
                    air_ready = now + 70;
                    if command && received.is_none() {
                        received = Some(now);
                    }
                }
                let ready = cap.as_ref().is_none_or(|c| c.ready(wall, Some(now)));
                if now >= uart_ready && ready && (bulk > 0 || command_pending) {
                    let command = command_pending && !command_sent;
                    modem.push_back((now, command));
                    if command {
                        command_pending = false;
                        command_sent = true;
                    } else {
                        bulk -= 1;
                    }
                    uart_ready = now + 18;
                    if let Some(cap) = cap.as_mut() {
                        cap.committed(wall, Some(now), 196);
                    }
                    peak = peak.max(modem.len() * 196);
                }
            }
            (peak, received.unwrap_or(2000).saturating_sub(500))
        }
        let flood = simulate(None, wall);
        let bounded = simulate(Some(&mut cap), wall);
        assert!(
            flood.0 > 4096 && flood.1 > 500,
            "negative control must reproduce modem backlog: {flood:?}"
        );
        assert!(
            bounded.0 <= 392 && bounded.1 <= 200,
            "keep commands before offload: {bounded:?}"
        );
    }
}
