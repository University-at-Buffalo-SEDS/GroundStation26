//! Capacity backpressure before UART offload, independent of modem MAC/turns.
//! Retain the next frame in its priority queue until one bounded airtime share
//! has drained. Bounded write batches also leave a serial packet boundary.
//! No AUX, GRANT, receive windows, accumulated idle credit or blocking sleep.
use std::time::{Duration, Instant};

pub(crate) struct Capacity {
    next: Option<(Instant, Option<u64>)>,
    next_packet: Option<(Instant, Option<u64>)>,
    packet_started: Option<(Instant, Option<u64>)>,
    last_forced_boundary: Option<(Instant, Option<u64>)>,
    last_receipt_boundary: Option<(Instant, Option<u64>)>,
    promotion_contracts: [u64; 4],
    promotion_flags: u8,
    promotion_contract_count: u8,
    promotion_contract_cursor: u8,
    early_frame_outstanding: bool,
    late_command_id: u64,
    late_command_len: u8,
    late_command_virtual: bool,
    ordinary_service_credit_ms: u8,
    extra_close_owed: bool,
    packet_burst_ms: u16,
    uart_baud: u32,
    packet_gap_ms: u16,
    air_bps: u64,
    share_percent: u64,
    overhead_bytes: u64,
    default_share_percent: u64,
    fixed_share: bool,
}
fn bounded_env(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (min..=max).contains(value))
        .unwrap_or(default)
}
impl Capacity {
    pub(crate) fn ready_for(&self, wall: Instant, virtual_ms: Option<u64>, critical: bool) -> bool {
        if self.ready(wall, virtual_ms) {
            return true;
        }
        if !critical || self.early_frame_outstanding || !self.packet_ready(wall, virtual_ms) {
            return false;
        }
        let limit = u64::from(self.packet_burst_ms).saturating_add(u64::from(self.packet_gap_ms));
        self.next.is_some_and(
            |(next_wall, next_virtual)| match (virtual_ms, next_virtual) {
                (Some(now), Some(next)) => next.saturating_sub(now) <= limit,
                _ => next_wall.saturating_duration_since(wall) <= Duration::from_millis(limit),
            },
        )
    }
    /// A canonical unsequenced command may finish the existing bounded loan.
    /// Eligibility is validated from the wire contract, not bool critical.
    pub(crate) fn ready_for_command(
        &self,
        wall: Instant,
        clock: Option<u64>,
        id: u64,
        encoded_len: usize,
        canonical_unsequenced: bool,
    ) -> bool {
        encoded_len > 0
            && (self.ready_for(wall, clock, true)
                || self.late_sibling_ready(wall, clock, id, encoded_len, canonical_unsequenced))
    }
    fn late_sibling_ready(
        &self,
        wall: Instant,
        clock: Option<u64>,
        id: u64,
        encoded_len: usize,
        canonical_unsequenced: bool,
    ) -> bool {
        let first_id = self.late_command_id;
        let first_len = usize::from(self.late_command_len);
        let virtual_domain = self.late_command_virtual;
        if first_len == 0 {
            return false;
        }
        canonical_unsequenced
            && self.early_frame_outstanding
            && id != first_id
            && encoded_len > 0
            && first_len
                .checked_add(encoded_len)
                .is_some_and(|len| len <= 128)
            && virtual_domain == clock.is_some()
            && !Self::reached(self.next, wall, clock)
            && Self::reached(self.next_packet, wall, clock)
    }
    /// Charge every accepted byte. First partial/error cannot license another
    /// frame; any positive late-sibling prefix consumes the sole remaining slot.
    pub(crate) fn committed_command(
        &mut self,
        wall: Instant,
        clock: Option<u64>,
        command: (u64, usize),
        canonical_unsequenced: bool,
        accepted: usize,
        full_io_success: bool,
    ) {
        let (id, encoded_len) = command;
        if accepted == 0 {
            return;
        }
        let late = self.late_sibling_ready(wall, clock, id, encoded_len, canonical_unsequenced);
        let admissible = self.ready_for(wall, clock, true);
        self.committed(wall, clock, accepted);
        // Keep the existing first-loan semantics; historical IDs do not
        // create another slot in the current outstanding loan epoch.
        if !late
            && admissible
            && canonical_unsequenced
            && full_io_success
            && accepted == encoded_len
            && (1..=128).contains(&encoded_len)
            && self.early_frame_outstanding
        {
            self.late_command_id = id;
            self.late_command_len = encoded_len as u8;
            self.late_command_virtual = clock.is_some();
        }
    }
    /// Promote the first interactive completion per normal batch cycle. A
    /// retry/ACK burst must not turn all ordinary traffic into one-frame
    /// packets. Keep airtime debt and the stopbit/quiet deadline unchanged.
    pub(crate) fn close_packet_batch(&mut self, wall: Instant, virtual_ms: Option<u64>) {
        self.close_packet_batch_with_receipt(wall, virtual_ms, None);
    }
    pub(crate) fn close_packet_batch_with_receipt(
        &mut self,
        wall: Instant,
        virtual_ms: Option<u64>,
        receipt: Option<u64>,
    ) {
        self.close_packet_batch_for_contract(wall, virtual_ms, receipt.map(|id| (id, true)));
    }
    pub(crate) fn close_packet_batch_for_contract(
        &mut self,
        wall: Instant,
        virtual_ms: Option<u64>,
        contract: Option<(u64, bool)>,
    ) -> bool {
        let cycle_ms =
            u64::from(self.packet_burst_ms).saturating_add(u64::from(self.packet_gap_ms));
        let receipt = contract.is_some_and(|(_, receipt)| receipt);
        let slot = contract.and_then(|(id, _)| {
            self.promotion_contracts[..usize::from(self.promotion_contract_count)]
                .iter()
                .position(|stored| *stored == id)
        });
        if slot.is_some_and(|slot| {
            self.promotion_flags & (1 << (slot + if receipt { 4 } else { 0 })) != 0
        }) {
            return false;
        }
        // The first returning receipt may override a recent uplink close, but
        // updates the global cooldown so later commands yield. An extra unique
        // close requires a whole idle gap funded by completed ordinary service;
        // retries never get new credit.
        let previous = if receipt {
            self.last_receipt_boundary
        } else {
            self.last_forced_boundary
        };
        if previous.is_some_and(
            |(last_wall, last_virtual)| match (virtual_ms, last_virtual) {
                (Some(now), Some(last)) => now.saturating_sub(last) < cycle_ms,
                _ => wall.saturating_duration_since(last_wall) < Duration::from_millis(cycle_ms),
            },
        ) {
            if contract.is_none()
                || self.extra_close_owed
                || !(1..=55).contains(&self.packet_gap_ms)
                || u64::from(self.ordinary_service_credit_ms) < u64::from(self.packet_gap_ms)
            {
                return false;
            }
            self.ordinary_service_credit_ms = 0;
            self.extra_close_owed = true;
        }
        self.packet_started = None;
        self.last_forced_boundary = Some((wall, virtual_ms));
        if receipt {
            self.last_receipt_boundary = Some((wall, virtual_ms));
        }
        if let Some((id, _)) = contract {
            self.remember_promotion(id, receipt);
        }
        true
    }
    fn remember_promotion(&mut self, id: u64, receipt: bool) {
        let slot = self.promotion_contracts[..usize::from(self.promotion_contract_count)]
            .iter()
            .position(|stored| *stored == id)
            .unwrap_or_else(|| {
                let slot = usize::from(self.promotion_contract_cursor);
                self.promotion_contracts[slot] = id;
                self.promotion_flags &= !((1 << slot) | (1 << (slot + 4)));
                self.promotion_contract_cursor = (self.promotion_contract_cursor + 1) % 4;
                self.promotion_contract_count =
                    self.promotion_contract_count.saturating_add(1).min(4);
                slot
            });
        self.promotion_flags |= 1 << (slot + if receipt { 4 } else { 0 });
    }
    /// A single physical boundary serves both already-owned commands. Record
    /// both logical IDs only when a boundary was actually promoted.
    pub(crate) fn close_command_batch(
        &mut self,
        wall: Instant,
        clock: Option<u64>,
        ids: [u64; 2],
    ) -> bool {
        self.close_control_batch(wall, clock, ids.map(|id| (id, false)))
            .0
    }

    /// One physical close may serve two already-owned commands/receipts.
    /// Prefer an eligible receipt's existing permit, then ordinary commands.
    /// No new permit is created; mark both contracts only after actual close.
    pub(crate) fn close_control_batch(
        &mut self,
        wall: Instant,
        clock: Option<u64>,
        contracts: [(u64, bool); 2],
    ) -> (bool, bool) {
        for receipt in [true, false] {
            for contract in contracts
                .into_iter()
                .filter(|(_, is_receipt)| *is_receipt == receipt)
            {
                if self.close_packet_batch_for_contract(wall, clock, Some(contract)) {
                    for (id, receipt) in contracts {
                        self.remember_promotion(id, receipt);
                    }
                    return (true, receipt);
                }
            }
        }
        (false, false)
    }

    /// Charged ordinary bytes displaced by one modem idle gap. This is a
    /// scheduler-service allowance only, never UART or airtime readiness.
    pub(crate) fn packet_idle_service_bytes(&self) -> usize {
        (u128::from(self.air_bps) * u128::from(self.share_percent) * u128::from(self.packet_gap_ms)
            / 800_000)
            .min(usize::MAX as u128) as usize
    }

    /// Bank actual completed ordinary airtime, capped at one 55ms idle gap.
    /// This never changes readiness, air debt, or the early-frame loan.
    pub(crate) fn note_successful_ordinary_service(&mut self, framed_bytes: usize) {
        if framed_bytes == 0 {
            return;
        }
        let bps = (u128::from(self.air_bps) * u128::from(self.share_percent) / 100).max(1);
        let cost = ((framed_bytes as u128 + u128::from(self.overhead_bytes)) * 8000).div_ceil(bps);
        self.ordinary_service_credit_ms =
            (u128::from(self.ordinary_service_credit_ms) + cost).min(55) as u8;
        self.extra_close_owed = false;
    }
    #[cfg(test)]
    pub(crate) fn ordinary_service_credit_ms(&self) -> u8 {
        self.ordinary_service_credit_ms
    }
    #[cfg(test)]
    pub(crate) fn extra_close_owed(&self) -> bool {
        self.extra_close_owed
    }
    pub(crate) fn packetization(&self) -> (u64, u64) {
        (
            u64::from(self.packet_burst_ms),
            u64::from(self.packet_gap_ms),
        )
    }
    pub(crate) fn from_env(uart_baud: u32) -> Self {
        let recovery = matches!(
            std::env::var("GS_RRLS_ROLE").as_deref(),
            Ok("primary" | "rrls")
        ) || std::env::var("GS_RRLS_VOICE").as_deref() == Ok("1");
        let default_share = if recovery { 25 } else { 40 };
        let configured_share = std::env::var("GS_RADIO_AIR_SHARE_PERCENT")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| (1..=100).contains(value));
        let mut capacity = Self::new(
            bounded_env("GS_RADIO_AIR_BIT_RATE_BPS", 64_000, 1_200, 1_000_000),
            configured_share.unwrap_or(default_share),
            bounded_env("GS_RADIO_AIR_FRAME_OVERHEAD_BYTES", 16, 0, 256),
        );
        capacity.uart_baud = uart_baud.max(1);
        capacity.packet_burst_ms =
            u16::try_from(bounded_env("GS_RADIO_PACKET_BURST_MS", 100, 0, 500))
                .expect("bounded burst fits u16");
        capacity.packet_gap_ms = u16::try_from(bounded_env("GS_RADIO_PACKET_GAP_MS", 55, 0, 1_000))
            .expect("bounded gap fits u16");
        capacity.default_share_percent = default_share;
        capacity.fixed_share = configured_share.is_some();
        capacity
    }
    pub(crate) fn new(air_bps: u64, share_percent: u64, overhead_bytes: u64) -> Self {
        Self {
            next: None,
            next_packet: None,
            packet_started: None,
            last_forced_boundary: None,
            last_receipt_boundary: None,
            promotion_contracts: [0; 4],
            promotion_flags: 0,
            promotion_contract_count: 0,
            promotion_contract_cursor: 0,
            early_frame_outstanding: false,
            late_command_id: 0,
            late_command_len: 0,
            late_command_virtual: false,
            ordinary_service_credit_ms: 0,
            extra_close_owed: false,
            packet_burst_ms: 100,
            uart_baud: 115_200,
            packet_gap_ms: 55,
            air_bps,
            share_percent,
            overhead_bytes,
            default_share_percent: share_percent,
            fixed_share: false,
        }
    }
    pub(crate) fn apply_recovery_share(
        &mut self,
        requested: Option<u8>,
        wall: Instant,
        virtual_ms: Option<u64>,
    ) {
        if self.fixed_share {
            return;
        }
        let share = requested
            .map(u64::from)
            .unwrap_or(self.default_share_percent);
        assert!((1..=100).contains(&share));
        if share < self.share_percent
            && let Some((next_wall, next_virtual)) = self.next
        {
            // A reduction cannot forgive the existing reservation's remaining
            // airtime. An increase leaves it intact, preventing a transition burst.
            let scale = |value: u128| {
                value
                    .saturating_mul(self.share_percent as u128)
                    .div_ceil(share as u128)
                    .min(u64::MAX as u128) as u64
            };
            let remaining = scale(next_wall.saturating_duration_since(wall).as_nanos());
            let next_wall = wall
                .checked_add(Duration::from_nanos(remaining))
                .unwrap_or(wall);
            let next_virtual = match (next_virtual, virtual_ms) {
                (Some(next), Some(now)) => {
                    Some(now.saturating_add(scale(next.saturating_sub(now) as u128)))
                }
                (next, _) => next,
            };
            self.next = Some((next_wall, next_virtual));
        }
        if share != self.share_percent {
            self.late_command_len = 0;
        }
        self.share_percent = share;
    }
    pub(crate) fn ready(&self, wall: Instant, virtual_ms: Option<u64>) -> bool {
        Self::reached(self.next, wall, virtual_ms) && self.packet_ready(wall, virtual_ms)
    }
    fn reached(
        deadline: Option<(Instant, Option<u64>)>,
        wall: Instant,
        virtual_ms: Option<u64>,
    ) -> bool {
        deadline.is_none_or(
            |(next_wall, next_virtual)| match (virtual_ms, next_virtual) {
                (Some(now), Some(next)) => now >= next,
                _ => wall >= next_wall,
            },
        )
    }
    fn packet_ready(&self, wall: Instant, virtual_ms: Option<u64>) -> bool {
        let reached = |deadline: Option<(Instant, Option<u64>)>| {
            deadline.is_none_or(
                |(next_wall, next_virtual)| match (virtual_ms, next_virtual) {
                    (Some(now), Some(next)) => now >= next,
                    _ => wall >= next_wall,
                },
            )
        };
        let within_burst = self.packet_started.is_some_and(|(started, virtual_start)| {
            match (virtual_ms, virtual_start) {
                (Some(now), Some(start)) => {
                    now.saturating_sub(start) < u64::from(self.packet_burst_ms)
                }
                _ => {
                    wall.saturating_duration_since(started)
                        < Duration::from_millis(u64::from(self.packet_burst_ms))
                }
            }
        });
        within_burst || reached(self.next_packet)
    }
    /// Read-only diagnostic snapshot; never samples a new clock or retires state.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn next_packet_virtual_ms(&self) -> Option<u64> {
        self.next_packet.and_then(|(_, virtual_ms)| virtual_ms)
    }

    /// A physical tcdrain may complete later than its nominal wire duration.
    /// Anchor the quiet gap to actual completion; virtual time still follows
    /// the modeled 8N1 duration, independently of host scheduling delays.
    pub(crate) fn drained(&mut self, completed: Instant) {
        if let Some((deadline, _)) = self.next_packet.as_mut() {
            *deadline =
                (*deadline).max(completed + Duration::from_millis(u64::from(self.packet_gap_ms)));
        }
    }

    /// PTYs complete writes before their modeled UART drains. Preserve the
    /// same quiet interval as physical tcdrain, including accumulated lookahead.
    pub(crate) fn drained_virtual(&mut self, stop_us: u64) {
        let quiet_until = stop_us
            .div_ceil(1_000)
            .saturating_add(u64::from(self.packet_gap_ms));
        if let Some((_, Some(deadline))) = self.next_packet.as_mut() {
            *deadline = (*deadline).max(quiet_until);
        }
    }

    /// One loan admission still charges every original frame's overhead and
    /// individual rounding. A failed prefix charges only frames actually begun.
    pub(crate) fn committed_command_batch(
        &mut self,
        wall: Instant,
        clock: Option<u64>,
        lengths: [usize; 2],
        accepted: usize,
    ) {
        if accepted == 0 {
            return;
        }
        let accepted = accepted.min(lengths[0].saturating_add(lengths[1]));
        let cost = |bytes: usize| {
            if bytes == 0 {
                0
            } else {
                ((bytes as u128 + u128::from(self.overhead_bytes)) * 8 * 1000).div_ceil(
                    (u128::from(self.air_bps) * u128::from(self.share_percent) / 100).max(1),
                ) as u64
            }
        };
        let separate = cost(accepted.min(lengths[0])) + cost(accepted.saturating_sub(lengths[0]));
        let extra = separate.saturating_sub(cost(accepted));
        self.committed(wall, clock, accepted);
        if let Some((next_wall, next_virtual)) = self.next.as_mut() {
            *next_wall = next_wall
                .checked_add(Duration::from_millis(extra))
                .unwrap_or(*next_wall);
            *next_virtual = next_virtual.map(|next| next.saturating_add(extra));
        }
    }
    pub(crate) fn committed(
        &mut self,
        wall: Instant,
        virtual_ms: Option<u64>,
        framed_bytes: usize,
    ) {
        if framed_bytes > 0 {
            self.late_command_len = 0;
        }
        let bps = (self.air_bps as u128 * self.share_percent as u128 / 100).max(1);
        let millis = ((framed_bytes as u128 + self.overhead_bytes as u128) * 8 * 1000)
            .div_ceil(bps)
            .min(u64::MAX as u128) as u64;
        // Small writes can exhaust neither the airtime budget nor MAX_DATA.
        // Without an idle boundary they can still be held inside an async
        // modem for seconds. Keep this bound separate from air-share debt.
        let packet_ms = (framed_bytes as u64 * 10_000).div_ceil(u64::from(self.uart_baud))
            + u64::from(self.packet_gap_ms);
        let boundary_passed = self.next_packet.is_none_or(|(next_wall, next_virtual)| {
            match (virtual_ms, next_virtual) {
                (Some(now), Some(next)) => now >= next,
                _ => wall >= next_wall,
            }
        });
        if boundary_passed {
            self.packet_started = Some((wall, virtual_ms));
        }
        self.next_packet = Some((
            wall.checked_add(Duration::from_millis(packet_ms))
                .unwrap_or(wall),
            virtual_ms.map(|now| now.saturating_add(packet_ms)),
        ));
        let previous = self.next;
        self.early_frame_outstanding = !Self::reached(previous, wall, virtual_ms);
        let base_wall = previous.map_or(wall, |(next, _)| next.max(wall));
        let base_virtual = virtual_ms.map(|now| match previous {
            Some((_, Some(next))) => next.max(now),
            Some((next, None)) => now.saturating_add(
                next.saturating_duration_since(wall)
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .min(u64::MAX as u128) as u64,
            ),
            None => now,
        });
        let duration = Duration::from_millis(millis);
        self.next = Some((
            base_wall.checked_add(duration).unwrap_or(base_wall),
            base_virtual.map(|now| now.saturating_add(millis)),
        ));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paired_receipt_and_command_share_one_existing_permit_with_distinct_dedup_bits() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        assert!(cap.close_packet_batch_for_contract(wall, Some(0), Some((1, false))));
        assert_eq!(
            cap.close_control_batch(wall, Some(60), [(1, false), (1, true)]),
            (true, true)
        );
        assert_eq!(cap.last_forced_boundary.unwrap().1, Some(60));
        assert_eq!(cap.last_receipt_boundary.unwrap().1, Some(60));
        assert_eq!(
            cap.close_control_batch(wall, Some(100), [(2, true), (3, true)]),
            (false, false)
        );
        assert_eq!(
            cap.close_control_batch(wall, Some(216), [(1, false), (1, true)]),
            (false, false)
        );
        assert_eq!(
            cap.close_control_batch(wall, Some(216), [(2, true), (3, true)]),
            (true, true)
        );
        assert_eq!(
            cap.close_control_batch(wall, Some(400), [(2, true), (3, true)]),
            (false, false)
        );
    }

    #[test]
    fn diagnostic_packet_deadline_snapshot_keeps_exact_admission_state() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        assert_eq!(cap.next_packet_virtual_ms(), None);
        cap.committed(wall, Some(30080), 55);
        cap.drained_virtual(30_084_775);
        assert!(cap.close_packet_batch_for_contract(wall, Some(30080), Some((1, false))));
        assert_eq!(cap.next_packet_virtual_ms(), Some(30140));
        assert_eq!(cap.next_packet_virtual_ms(), Some(30140));
        assert!(!cap.ready(wall, Some(30139)));
        assert!(cap.ready(wall, Some(30140)));
        let mut physical = Capacity::new(64_000, 35, 16);
        physical.committed(wall, None, 55);
        assert_eq!(physical.next_packet_virtual_ms(), None);
    }

    #[test]
    fn already_queued_command_pair_uses_one_fully_charged_loan_and_one_close() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 15, 16);
        cap.committed(wall, Some(30000), 21);
        cap.drained_virtual(30_001_823);
        assert!(cap.ready_for(wall, Some(30001), true));
        cap.committed_command_batch(wall, Some(30001), [55, 61], 116);
        cap.drained_virtual(30_011_893);
        assert!(cap.close_command_batch(wall, Some(30001), [1, 2]));
        assert_eq!(cap.next.unwrap().1, Some(30156));
        assert!(cap.early_frame_outstanding);
        assert!(!cap.ready_for(wall, Some(30061), true));
        assert!(!cap.ready(wall, Some(30155)));
        assert!(cap.ready(wall, Some(30156)));
        assert!(!cap.close_packet_batch_for_contract(wall, Some(30200), Some((1, false))));
        assert!(!cap.close_packet_batch_for_contract(wall, Some(30200), Some((2, false))));
        assert!(
            cap.close_packet_batch_for_contract(wall, Some(30200), Some((2, true))),
            "receipt has independent first-return protection"
        );
    }

    #[test]
    fn partial_command_batch_keeps_per_frame_air_charges_without_rebating_overhead() {
        let wall = Instant::now();
        for accepted in [1, 54, 55, 56, 115, 116] {
            let mut batch = Capacity::new(64_000, 15, 16);
            batch.committed_command_batch(wall, Some(0), [55, 61], accepted);
            let mut original = Capacity::new(64_000, 15, 16);
            original.committed(wall, Some(0), accepted.min(55));
            if accepted > 55 {
                original.committed(wall, Some(0), accepted - 55);
            }
            assert_eq!(
                batch.next.unwrap().1,
                original.next.unwrap().1,
                "accepted={accepted}"
            );
        }
    }
    #[test]
    fn accumulated_uart_stop_preserves_quiet_after_captured_75_and_55_byte_close() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        let mut wire = super::super::virtual_uart::WireDrain::default();
        for bytes in [75, 55] {
            assert!(wire.ready(9080));
            assert!(cap.ready_for(wall, Some(9080), bytes == 55));
            wire.committed(9080, bytes, 115_200);
            cap.committed(wall, Some(9080), bytes);
            cap.drained_virtual(wire.stop_us());
        }
        assert!(cap.close_packet_batch_for_contract(wall, Some(9080), Some((1, false))));
        assert_eq!(wire.stop_us(), 9_091_286);
        assert!(
            !cap.ready(wall, Some(9140)),
            "48.714ms silence would merge the command with the next modem packet"
        );
        assert!(!cap.ready(wall, Some(9146)));
        assert!(cap.ready(wall, Some(9147)));
        assert!(9147_000 - wire.stop_us() >= 55_000);
    }

    #[test]
    fn several_queued_uart_frames_anchor_the_final_stop_without_idle_credit() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 100, 16);
        let mut wire = super::super::virtual_uart::WireDrain::default();
        for bytes in [50, 50, 55] {
            assert!(wire.ready(9080), "bounded10ms lookahead remains available");
            wire.committed(9080, bytes, 115_200);
            cap.committed(wall, Some(9080), bytes);
            cap.drained_virtual(wire.stop_us());
        }
        assert!(wire.stop_us() > 9_093_000);
        cap.close_packet_batch(wall, Some(9080));
        assert!(!cap.ready(wall, Some(9148)));
        assert!(cap.ready(wall, Some(9149)));
        wire.committed(60_000, 55, 115_200);
        cap.committed(wall, Some(60_000), 55);
        cap.drained_virtual(wire.stop_us());
        cap.close_packet_batch(wall, Some(60_000));
        assert!(!cap.ready(wall, Some(60_059)));
        assert!(
            cap.ready(wall, Some(60_060)),
            "idle time carries no queued wire debt"
        );
    }

    #[test]
    fn accumulated_uart_quiet_is_rounded_up_and_never_rewinds_or_forgives_debt() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 1, 16);
        cap.committed(wall, Some(100), 55);
        let debt = cap.next;
        cap.drained_virtual(120_001);
        cap.drained_virtual(110_000);
        assert_eq!(cap.next_packet.unwrap().1, Some(176));
        assert_eq!(cap.next, debt);
        assert!(
            !cap.ready(wall, Some(176)),
            "quiet does not forgive airtime debt"
        );
        let mut physical = Capacity::new(64_000, 35, 16);
        physical.committed(wall, None, 55);
        let before = physical.next_packet;
        physical.drained_virtual(1_000_000);
        assert_eq!(
            physical.next_packet, before,
            "physical tcdrain remains authoritative"
        );
    }

    #[test]
    fn logical_command_and_receipt_each_promote_once_while_retries_keep_batching() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        cap.committed(wall, Some(30120), 55);
        cap.close_packet_batch_for_contract(wall, Some(30120), Some((11, false)));
        let first = cap.last_forced_boundary;
        for now in [30420, 30690] {
            cap.committed(wall, Some(now), 55);
            cap.close_packet_batch_for_contract(wall, Some(now), Some((11, false)));
            assert_eq!(
                cap.last_forced_boundary, first,
                "retry retains normal batching"
            );
        }
        cap.committed(wall, Some(30750), 62);
        cap.close_packet_batch_for_contract(wall, Some(30750), Some((12, false)));
        assert_eq!(cap.last_forced_boundary.unwrap().1, Some(30750));
        cap.committed(wall, Some(30800), 63);
        cap.close_packet_batch_for_contract(wall, Some(30800), Some((11, true)));
        assert_eq!(
            cap.last_forced_boundary.unwrap().1,
            Some(30800),
            "command promotion must not consume the same contract's first receipt promotion"
        );
        let receipt = cap.last_forced_boundary;
        for now in [31370, 31660] {
            cap.committed(wall, Some(now), 63);
            cap.close_packet_batch_for_contract(wall, Some(now), Some((11, true)));
            assert_eq!(
                cap.last_forced_boundary, receipt,
                "a regenerated receipt nonce does not buy another logical promotion"
            );
        }
        assert_eq!(cap.promotion_contract_count, 2);
    }

    #[test]
    fn four_contract_history_is_bounded_and_denied_attempts_do_not_claim_it() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        cap.close_packet_batch_for_contract(wall, Some(0), Some((0, false)));
        cap.close_packet_batch_for_contract(wall, Some(1), Some((1, false)));
        assert_eq!(cap.promotion_contract_count, 1);
        for id in 1..=4 {
            cap.close_packet_batch_for_contract(wall, Some(id * 155), Some((id, false)));
        }
        assert_eq!(cap.promotion_contract_count, 4);
        assert!(!cap.promotion_contracts.contains(&0));
        cap.close_packet_batch_for_contract(wall, Some(775), Some((0, true)));
        assert_eq!(cap.last_receipt_boundary.unwrap().1, Some(775));
        assert_eq!(cap.promotion_contract_count, 4);
        assert!(
            std::mem::size_of::<Capacity>() <= 256,
            "promotion metadata is fixed host state, never a new frame queue"
        );
    }
    #[test]
    fn captured_return_receipt_gets_a_boundary_after_the_uplink_command() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        cap.committed(wall, Some(9290), 62);
        cap.close_packet_batch(wall, Some(9290));
        assert!(cap.ready(wall, Some(9360)));
        cap.committed(wall, Some(9360), 63);
        cap.close_packet_batch_with_receipt(wall, Some(9360), Some(1));
        assert!(
            !cap.ready(wall, Some(9410)),
            "captured ordinary9410/retry9440 must not extend the return receipt's packet"
        );
        assert!(!cap.ready(wall, Some(9420)));
        assert!(cap.ready(wall, Some(9421)));
        cap.committed(wall, Some(9421), 42);
        cap.close_packet_batch(wall, Some(9421));
        assert!(
            cap.ready(wall, Some(9442)),
            "the receipt updates the global cooldown, so later commands keep normal batching"
        );
    }

    #[test]
    fn receipt_overrides_are_bounded_and_preserve_ordinary_progress_and_airtime() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        let mut boundaries = Vec::new();
        let mut ordinary = 0;
        for now in 0..2000 {
            let receipt = now % 3 == 0;
            if !cap.ready_for(wall, Some(now), true) {
                continue;
            }
            cap.committed(wall, Some(now), 42);
            let before = cap.last_forced_boundary;
            if now % 5 == 0 {
                ordinary += 1;
            } else {
                cap.close_packet_batch_with_receipt(wall, Some(now), receipt.then_some(now));
            }
            if before != cap.last_forced_boundary {
                boundaries.push(now);
            }
        }
        assert!(
            ordinary >= 5,
            "ordinary frames must continue to make progress"
        );
        for start in &boundaries {
            assert!(
                boundaries
                    .iter()
                    .filter(|at| **at >= *start && **at < *start + 155)
                    .count()
                    <= 2,
                "at most a command and a receipt boundary per rolling155ms: {boundaries:?}"
            );
        }
        let mut low = Capacity::new(64_000, 1, 16);
        low.committed(wall, Some(0), 42);
        low.close_packet_batch_with_receipt(wall, Some(0), Some(1));
        assert!(!low.ready_for(wall, Some(569), true));
        assert!(low.ready_for(wall, Some(570), true));
        low.committed(wall, Some(570), 42);
        low.close_packet_batch_with_receipt(wall, Some(570), Some(2));
        assert!(!low.ready_for(wall, Some(1295), true));
        assert!(
            low.ready(wall, Some(1450)),
            "receipt closing cannot forgive an unpaid loan"
        );
    }

    #[test]
    fn receipt_cooldown_survives_virtual_wall_and_backward_clock_changes() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        cap.committed(wall, Some(0), 42);
        cap.close_packet_batch(wall, Some(0));
        cap.committed(wall + Duration::from_millis(62), Some(62), 42);
        cap.close_packet_batch_with_receipt(wall + Duration::from_millis(62), Some(62), Some(1));
        let receipt = cap.last_receipt_boundary;
        cap.close_packet_batch_with_receipt(wall + Duration::from_millis(120), None, Some(1));
        assert_eq!(cap.last_receipt_boundary, receipt);
        cap.close_packet_batch(wall + Duration::from_millis(220), None);
        cap.close_packet_batch_with_receipt(wall + Duration::from_millis(230), Some(230), Some(2));
        assert_eq!(cap.last_receipt_boundary.unwrap().1, Some(230));
        let receipt = cap.last_receipt_boundary;
        cap.close_packet_batch_with_receipt(wall + Duration::from_millis(240), Some(200), Some(3));
        assert_eq!(
            cap.last_receipt_boundary, receipt,
            "backward time cannot create new credit"
        );
    }
    #[test]
    fn critical_head_can_borrow_one_bounded_lead_but_must_repay_all_airtime() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 15, 16);
        cap.committed(wall, Some(8990), 204); //192-byteSDT +8RR +4UART =>184ms debt.
        assert!(!cap.ready_for(wall + Duration::from_millis(10), Some(9000), true));
        assert!(!cap.ready_for(wall + Duration::from_millis(28), Some(9018), true));
        assert!(
            cap.ready_for(wall + Duration::from_millis(29), Some(9019), true),
            "critical head may take155ms finite lead instead of waiting behind the184ms bulk debt"
        );
        assert!(!cap.ready_for(wall + Duration::from_millis(29), Some(9019), false));
        cap.committed(wall + Duration::from_millis(29), Some(9019), 42);
        cap.close_packet_batch(wall + Duration::from_millis(29), Some(9019));
        assert!(
            !cap.ready_for(wall + Duration::from_millis(80), Some(9070), true),
            "second early frame is refused until the entire loan is repaid"
        );
        assert!(!cap.ready_for(wall + Duration::from_millis(232), Some(9222), true));
        assert!(!cap.ready(wall + Duration::from_millis(232), None));
        assert!(cap.ready(wall + Duration::from_millis(233), None));
        assert!(
            cap.ready(wall, Some(9223)),
            "184+49ms airtime is paid, not overwritten by the early send"
        );
    }
    #[test]
    fn critical_loan_respects_low_share_wire_quiet_and_clock_changes() {
        let wall = Instant::now();
        let mut low = Capacity::new(64_000, 1, 16);
        low.committed(wall, Some(0), 42); //725ms debt, not an immediate critical credit.
        assert!(!low.ready_for(wall, Some(569), true));
        assert!(low.ready_for(wall, Some(570), true));
        low.committed(wall + Duration::from_millis(570), Some(570), 42);
        assert!(!low.ready_for(wall, Some(1295), true));
        assert!(low.ready(wall, Some(1450)));

        let mut quiet = Capacity::new(64_000, 15, 16);
        quiet.packet_burst_ms = 0;
        quiet.committed(wall, Some(0), 42);
        assert!(!quiet.ready_for(wall, Some(58), true));
        assert!(quiet.ready_for(wall, Some(59), true));
        quiet.drained(wall + Duration::from_millis(100));
        assert!(!quiet.ready_for(wall + Duration::from_millis(154), None, true));
        assert!(quiet.ready_for(wall + Duration::from_millis(155), None, true));

        let mut mixed = Capacity::new(64_000, 15, 16);
        mixed.committed(wall, None, 204);
        assert!(mixed.ready_for(wall + Duration::from_millis(29), Some(9000), true));
        mixed.committed(wall + Duration::from_millis(29), Some(9000), 42);
        assert!(!mixed.ready(wall + Duration::from_millis(232), None));
        assert!(mixed.ready(wall + Duration::from_millis(233), None));
        assert!(!mixed.ready(wall, Some(9203)));
        assert!(
            mixed.ready(wall, Some(9204)),
            "translate remaining155ms wall debt before adding49ms new airtime"
        );
    }

    #[test]
    fn interactive_completion_ends_only_this_batch_without_forgiving_airtime_debt() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 35, 16);
        for now in [0, 21, 42, 63] {
            assert!(cap.ready(wall, Some(now)));
            cap.committed(wall, Some(now), 42);
        }
        cap.close_packet_batch(wall, Some(63));
        assert!(
            !cap.ready(wall, Some(84)),
            "later bulk must leave the critical frame's stopbit +55ms quiet gap"
        );
        assert!(!cap.ready(wall, Some(121)));
        assert!(cap.ready(wall, Some(122)));
        cap.committed(wall, Some(122), 42);
        cap.close_packet_batch(wall, Some(122));
        assert!(
            cap.ready(wall, Some(143)),
            "ordinary traffic resumes unchanged100ms batches"
        );
        cap.committed(wall, Some(143), 42);
        cap.close_packet_batch(wall, Some(143));
        assert!(cap.ready(wall, Some(218)));
        cap.committed(wall, Some(218), 42);
        cap.close_packet_batch(wall, Some(218));
        assert!(
            !cap.ready(wall, Some(239)),
            "a new normal155ms cycle can promote its next critical completion"
        );
        assert!(cap.ready(wall, Some(277)));
        let mut low_share = Capacity::new(64_000, 1, 16);
        low_share.committed(wall, Some(0), 42);
        low_share.close_packet_batch(wall, Some(0));
        assert!(
            !low_share.ready(wall, Some(724)),
            "closing a batch cannot release existing airtime debt"
        );
        assert!(low_share.ready(wall, Some(725)));
    }
    #[test]
    fn bounded_batches_keep_small_frame_throughput_without_starving_packet_flush() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 15, 16);
        for now in [0, 49, 98] {
            assert!(cap.ready(wall, Some(now)));
            cap.committed(wall, Some(now), 42);
        }
        // The airtime deadline alone is 147ms. The batch must now leave enough
        // silence for the last frame to finish and the modem to flush.
        assert!(!cap.ready(wall, Some(147)));
        assert!(!cap.ready(wall, Some(156)));
        assert!(cap.ready(wall, Some(157)));
        cap.committed(wall, Some(157), 42);
        assert!(cap.ready(wall, Some(206)));
        cap.committed(wall, Some(206), 42);
        assert!(!cap.ready(wall + Duration::from_secs(60), Some(206)));
    }

    #[test]
    fn short_frames_leave_a_real_packet_boundary_after_uart_completion() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 15, 16);
        cap.packet_burst_ms = 0; // Exercise the boundary itself without batching.
        cap.committed(wall, Some(0), 42);
        // The old 49ms capacity-only interval never left 50ms of UART silence.
        assert!(!cap.ready(wall, Some(58)));
        assert!(cap.ready(wall, Some(59)));
        cap.drained(wall + Duration::from_millis(100));
        assert!(!cap.ready(wall + Duration::from_millis(154), None));
        assert!(cap.ready(wall + Duration::from_millis(155), None));
        // Host scheduling cannot move a virtual-time packet boundary.
        assert!(cap.ready(wall, Some(59)));
        cap.uart_baud = 9_600;
        cap.committed(wall, Some(1_000), 42);
        assert!(!cap.ready(wall, Some(1_098)));
        assert!(cap.ready(wall, Some(1_099)));
    }

    #[test]
    fn changing_air_share_cannot_shorten_the_serial_idle_boundary() {
        let wall = Instant::now();
        let mut cap = Capacity::new(64_000, 40, 16);
        cap.packet_burst_ms = 0;
        cap.committed(wall, Some(0), 42);
        cap.apply_recovery_share(Some(15), wall, Some(10));
        cap.apply_recovery_share(Some(35), wall, Some(20));
        assert!(!cap.ready(wall, Some(58)));
        assert!(cap.ready(wall, Some(59)));
    }

    #[test]
    fn active_relay_budget_changes_preserve_existing_debt_and_healthy_defaults() {
        let wall = Instant::now();
        let mut physical = Capacity::new(64_000, 25, 16);
        physical.committed(wall, None, 196);
        physical.apply_recovery_share(Some(15), wall + Duration::from_millis(10), None);
        assert!(!physical.ready(wall + Duration::from_millis(169), None));
        assert!(physical.ready(wall + Duration::from_millis(170), None));
        let mut primary = Capacity::new(64_000, 25, 16);
        primary.committed(wall, Some(0), 196);
        primary.apply_recovery_share(Some(15), wall + Duration::from_millis(10), Some(10));
        assert_eq!(primary.share_percent, 15);
        assert!(!primary.ready(wall + Duration::from_secs(60), Some(169)));
        assert!(primary.ready(wall, Some(170)));
        primary.apply_recovery_share(Some(35), wall + Duration::from_millis(20), Some(20));
        assert!(
            !primary.ready(wall, Some(169)),
            "an increase cannot erase a pending reservation"
        );
        primary.committed(wall, Some(170), 196);
        assert!(!primary.ready(wall, Some(245)));
        assert!(primary.ready(wall, Some(246)));
        primary.apply_recovery_share(None, wall, Some(200));
        assert_eq!(primary.share_percent, 25);
        assert!(!primary.ready(wall, Some(264)));
        assert!(primary.ready(wall, Some(265)));
        primary.apply_recovery_share(Some(35), wall, Some(10_000));
        primary.committed(wall, Some(10_000), 196);
        assert!(
            !primary.ready(wall, Some(10_075)),
            "idle accrues no offload burst"
        );
    }

    #[test]
    fn valid_explicit_user_share_remains_authoritative() {
        let wall = Instant::now();
        let mut configured = Capacity::new(64_000, 19, 16);
        configured.fixed_share = true;
        for requested in [Some(15), Some(35), None] {
            configured.apply_recovery_share(requested, wall, Some(0));
            assert_eq!(configured.share_percent, 19);
        }
        let mut ordinary = Capacity::new(64_000, 40, 16);
        ordinary.apply_recovery_share(None, wall, Some(0));
        assert_eq!(
            ordinary.share_percent, 40,
            "non-recovery healthy behavior is unchanged"
        );
    }

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
#[cfg(test)]
mod funded_extra_close_tests {
    use super::*;
    #[test]
    fn extra_close_requires_actual_service_and_repayment_without_forgiving_air_debt() {
        let wall = Instant::now();
        let mut c = Capacity::new(64_000, 35, 16);
        c.committed(wall, Some(0), 55);
        c.drained_virtual(4786);
        let debt = c.next.unwrap().1;
        let quiet = c.next_packet.unwrap().1;
        assert!(c.close_packet_batch_for_contract(wall, Some(0), Some((1, false))));
        assert!(
            !c.close_packet_batch_for_contract(wall, Some(60), Some((2, false))),
            "no idle credit"
        );
        c.note_successful_ordinary_service(75);
        assert!(
            !c.close_packet_batch_for_contract(wall, Some(60), Some((2, false))),
            "insufficient funded service"
        );
        c.note_successful_ordinary_service(48);
        assert!(c.close_packet_batch_for_contract(wall, Some(60), Some((2, false))));
        assert!(c.extra_close_owed());
        assert_eq!(c.next.unwrap().1, debt, "no airtime forgiveness");
        assert_eq!(c.next_packet.unwrap().1, quiet, "no new readiness");
        assert!(
            !c.close_packet_batch_for_contract(wall, Some(120), Some((3, false))),
            "one unpaid extra close only"
        );
        c.note_successful_ordinary_service(48);
        assert!(!c.extra_close_owed());
        assert!(
            !c.close_packet_batch_for_contract(wall, Some(120), Some((3, false))),
            "refill still bounded"
        );
        c.note_successful_ordinary_service(75);
        assert!(c.close_packet_batch_for_contract(wall, Some(120), Some((3, false))));
    }
}
#[cfg(test)]
mod funded_wall_and_partial_tests {
    use super::*;
    #[test]
    fn wall_clock_funding_and_failed_prefix_keep_actual_debt_and_credit_ownership() {
        let start = Instant::now();
        let mut c = Capacity::new(64_000, 35, 16);
        c.committed(start, None, 55);
        c.drained(start + Duration::from_millis(5));
        assert!(c.close_packet_batch_for_contract(start, None, Some((1, false))));
        assert!(!c.close_packet_batch_for_contract(
            start + Duration::from_millis(60),
            None,
            Some((2, false))
        ));
        c.note_successful_ordinary_service(176);
        assert_eq!(c.ordinary_service_credit_ms, 55);
        let debt = c.next.unwrap().0;
        let quiet = c.next_packet.unwrap().0;
        assert!(c.close_packet_batch_for_contract(
            start + Duration::from_millis(60),
            None,
            Some((2, false))
        ));
        assert_eq!(c.next.unwrap().0, debt);
        assert_eq!(c.next_packet.unwrap().0, quiet);
        assert_eq!(c.ordinary_service_credit_ms, 0);
        assert!(c.extra_close_owed());
        // Counted physical acceptance of a failed prefix is still charged. It is
        // not a successful logical ordinary frame and cannot fund/repay the bank.
        c.committed(start + Duration::from_millis(70), None, 5);
        c.note_successful_ordinary_service(0);
        assert_eq!(c.ordinary_service_credit_ms, 0);
        assert!(c.extra_close_owed());
        assert!(!c.close_packet_batch_for_contract(
            start + Duration::from_millis(80),
            None,
            Some((3, false))
        ));
        assert!(c.next.unwrap().0 > debt);
    }
}

#[cfg(test)]
mod funded_gap_scope_tests {
    use super::*;
    #[test]
    fn unsupported_idle_gaps_never_expand_the_literal_55ms_bank() {
        for gap in [0u16, 56, 1000] {
            let wall = Instant::now();
            let mut c = Capacity::new(64_000, 35, 16);
            c.packet_gap_ms = gap;
            assert!(c.close_packet_batch_for_contract(wall, Some(0), Some((1, false))));
            c.note_successful_ordinary_service(4096);
            assert_eq!(c.ordinary_service_credit_ms, 55);
            assert!(
                !c.close_packet_batch_for_contract(wall, Some(1), Some((2, false))),
                "gap={gap} must retain original close policy"
            );
            assert_eq!(c.ordinary_service_credit_ms, 55);
            assert!(!c.extra_close_owed());
        }
    }
}

#[cfg(test)]
mod late_sibling_prototype_tests {
    use super::*;
    fn prefix(first_len: usize, accepted: usize, success: bool) -> (Capacity, Instant) {
        let wall = Instant::now();
        let mut cap = Capacity::new(64000, 15, 16);
        cap.committed(wall, Some(30000), 21);
        cap.drained_virtual(30001927);
        assert!(cap.ready_for_command(wall, Some(30001), 1, first_len, true));
        cap.committed_command(wall, Some(30001), (1, first_len), true, accepted, success);
        cap.drained_virtual(30006712);
        cap.close_packet_batch_for_contract(wall, Some(30001), Some((1, false)));
        (cap, wall)
    }
    #[test]
    fn captured117_after_quiet_matches_queued_pair_debt() {
        let (mut cap, w) = prefix(55, 55, true);
        assert!(!cap.ready_for_command(w, Some(30061), 2, 62, true));
        assert!(cap.ready_for_command(w, Some(30062), 2, 62, true));
        assert!(!cap.ready_for(w, Some(30062), true));
        cap.committed_command(w, Some(30062), (2, 62), true, 62, true);
        assert_eq!(cap.next.unwrap().1, Some(30156));
        assert!(cap.late_command_len == 0);
        for now in [30062, 30100, 30155] {
            assert!(!cap.ready_for_command(w, Some(now), 3, 1, true));
        }
        assert!(cap.ready(w, Some(30156)));
    }
    #[test]
    fn same_epoch_retry_and_noncanonical_metadata_do_not_extend() {
        let (cap, w) = prefix(55, 55, true);
        assert!(!cap.ready_for_command(w, Some(30062), 1, 62, true));
        assert!(!cap.ready_for_command(w, Some(30062), 2, 62, false));
        let mut c = Capacity::new(64000, 15, 16);
        c.committed(w, Some(0), 21);
        c.committed_command(w, Some(1), (9, 55), false, 55, true);
        assert!(c.late_command_len == 0);
    }
    #[test]
    fn first_partial_or_flush_error_never_licenses() {
        for (n, ok) in [(1, false), (54, true), (55, false)] {
            let (c, w) = prefix(55, n, ok);
            assert!(c.late_command_len == 0);
            assert_eq!(
                c.ready_for_command(w, Some(30062), 2, 62, true),
                c.ready_for(w, Some(30062), true)
            );
        }
    }
    #[test]
    fn partial_second_consumes_slot_and_actual_debt() {
        for n in [1, 31, 61] {
            let (mut c, w) = prefix(55, 55, true);
            c.committed_command(w, Some(30062), (2, 62), true, n, false);
            assert!(c.late_command_len == 0);
            let next = c.next.unwrap().1.unwrap();
            assert_eq!(next, 30091 + ((n as u64 + 16) * 8000).div_ceil(9600));
            assert!(!c.ready_for_command(w, Some(next - 1), 3, 1, true));
        }
    }
    #[test]
    fn zero_prefix_error_keeps_epoch_without_sliding() {
        let (mut c, w) = prefix(55, 55, true);
        let debt = c.next.unwrap().1;
        c.committed_command(w, Some(30062), (2, 62), true, 0, false);
        assert_eq!(c.next.unwrap().1, debt);
        assert_eq!(c.late_command_id, 1);
        assert!(c.ready_for_command(w, Some(30062), 2, 62, true));
    }
    #[test]
    fn strict_total_wire_byte_and_frame_bounds() {
        let (c, w) = prefix(55, 55, true);
        assert!(c.ready_for_command(w, Some(30062), 2, 73, true));
        assert!(!c.ready_for_command(w, Some(30062), 2, 74, true));
        assert!(!c.ready_for_command(w, Some(30062), 2, 0, true));
        let (c, w) = prefix(129, 129, true);
        assert!(c.late_command_len == 0);
        assert!(!c.ready_for_command(w, Some(30062), 2, 1, true));
    }
    #[test]
    fn already_queued_pair_has_no_third_slot() {
        let w = Instant::now();
        let mut c = Capacity::new(64000, 15, 16);
        c.committed(w, Some(30000), 21);
        c.committed_command_batch(w, Some(30001), [55, 62], 117);
        assert!(c.late_command_len == 0);
        assert!(!c.ready_for_command(w, Some(30062), 3, 1, true));
    }
    #[test]
    fn noncommand_commit_share_or_clock_domain_change_cannot_extend() {
        let (mut c, w) = prefix(55, 55, true);
        c.committed(w, Some(30062), 1);
        assert!(c.late_command_len == 0);
        let (mut c, w) = prefix(55, 55, true);
        c.apply_recovery_share(Some(35), w, Some(30002));
        assert!(c.late_command_len == 0);
        let (c, w) = prefix(55, 55, true);
        assert!(!c.ready_for_command(w + Duration::from_millis(62), None, 2, 62, true));
    }
    #[test]
    fn wall_only_loan_respects_full_quiet() {
        let w = Instant::now();
        let mut c = Capacity::new(64000, 15, 16);
        c.committed(w, None, 21);
        assert!(c.ready_for_command(w + Duration::from_millis(1), None, 1, 55, true));
        c.committed_command(w + Duration::from_millis(1), None, (1, 55), true, 55, true);
        c.close_packet_batch_for_contract(w + Duration::from_millis(1), None, Some((1, false)));
        c.drained(w + Duration::from_millis(7));
        assert!(!c.ready_for_command(w + Duration::from_millis(61), None, 2, 62, true));
        assert!(c.ready_for_command(w + Duration::from_millis(62), None, 2, 62, true));
    }
}
