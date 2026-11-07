//! Owner confirmation for the two cosmetic outputs (AV-bay underglow and the
//! FC buzzer). Admission to the radio queue is not delivery. The SDK retires a
//! reliable owner either on the owner's ACK or after its last retry, and both
//! look the same from outside, so a removal is treated as an ACK only when the
//! remaining retries could not yet have run out. Anything else is unconfirmed
//! and the desired value is published again: once straight away, then only
//! after the owner is heard from again.
//!
//! This deliberately covers only these two outputs. FLIGHT_STATE and the
//! fill-system commands keep their own paths.

use std::time::{Duration, Instant};

/// After the immediate republish also fails, wait at least this long before a
/// new owner report may trigger another one. A failed attempt keeps about 4
/// frames/s on air for 4.5 s; with this gap it uses under a third of that.
pub(super) const REPORT_REPUBLISH_GAP: Duration = Duration::from_secs(10);

/// One pending-row observation of the SDK's frozen reliable owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Seen {
    retries: u32,
    at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Inflight {
    packet_id: u64,
    value: bool,
    admitted: Instant,
    seen: Option<Seen>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Idle,
    Changed,
    Republish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Confirmation {
    /// Last value the owner acknowledged in this process. Unknown at start.
    confirmed: Option<bool>,
    inflight: Option<Inflight>,
    /// One immediate republish per operator request.
    immediate_left: bool,
    /// Unconfirmed and idle since this instant, with this many owner reports.
    waiting: Option<(Instant, u64)>,
}

impl Default for Confirmation {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Confirmation {
    pub(super) const DEFAULT: Self = Self {
        confirmed: None,
        inflight: None,
        immediate_left: false,
        waiting: None,
    };

    /// Confirmed value to show: only when it matches the request and nothing
    /// newer is still in flight.
    pub(super) fn shown(&self, requested: bool) -> Option<bool> {
        (self.inflight.is_none() && self.confirmed == Some(requested)).then_some(requested)
    }

    /// A toggle flips what the owner last confirmed, not the stored request.
    /// Before any confirmation the stored request is the best knowledge.
    pub(super) fn toggle_target(&self, requested: bool) -> bool {
        !self.confirmed.unwrap_or(requested)
    }

    /// True when the same value is already being delivered.
    pub(super) fn delivering(&self, value: bool) -> bool {
        self.inflight.is_some_and(|i| i.value == value)
    }

    pub(super) fn published(&mut self, packet_id: u64, value: bool, now: Instant, fresh: bool) {
        self.inflight = Some(Inflight {
            packet_id,
            value,
            admitted: now,
            seen: None,
        });
        if fresh {
            self.immediate_left = true;
        }
        self.waiting = None;
    }

    pub(super) fn publish_failed(&mut self, now: Instant, reports: u64) {
        self.inflight = None;
        self.waiting = Some((now, reports));
    }

    /// `row` is the SDK pending row for the in-flight packet, if still listed;
    /// `queued` is true while the SDK may not have created that row yet.
    pub(super) fn observe(
        &mut self,
        row: Option<u32>,
        queued: bool,
        now: Instant,
        reports: u64,
        retransmit: Duration,
        max_retries: u32,
    ) -> Step {
        let Some(mut inflight) = self.inflight else {
            return Step::Idle;
        };
        if let Some(retries) = row {
            inflight.seen = Some(Seen { retries, at: now });
            self.inflight = Some(inflight);
            return Step::Idle;
        }
        let horizon = retransmit
            .saturating_mul(max_retries.saturating_add(2))
            .saturating_add(Duration::from_secs(1));
        if inflight.seen.is_none() && queued && now.duration_since(inflight.admitted) < horizon {
            return Step::Idle;
        }
        // Exhaustion needs every remaining retry, each at least one retransmit
        // interval apart. If two or more were left at the last sighting and we
        // looked again within half an interval, this removal was an ACK.
        let acked = inflight.seen.is_some_and(|seen| {
            seen.retries.saturating_add(1) < max_retries
                && now.duration_since(seen.at) < retransmit / 2
        });
        self.inflight = None;
        if acked {
            self.confirmed = Some(inflight.value);
            self.waiting = None;
            return Step::Changed;
        }
        if self.immediate_left {
            self.immediate_left = false;
            return Step::Republish;
        }
        self.waiting = Some((now, reports));
        Step::Changed
    }

    /// Unconfirmed and idle: republish once the owner has been heard from
    /// again, no sooner than the gap after the last failed attempt.
    pub(super) fn owner_report(&self, now: Instant, reports: u64) -> Step {
        match self.waiting {
            Some((since, at_reports))
                if reports > at_reports && now.duration_since(since) >= REPORT_REPUBLISH_GAP =>
            {
                Step::Republish
            }
            _ => Step::Idle,
        }
    }

    pub(super) fn inflight_id(&self) -> Option<u64> {
        self.inflight.map(|i| i.packet_id)
    }

    pub(super) fn needs_attention(&self) -> bool {
        self.inflight.is_some() || self.waiting.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Duration = Duration::from_millis(250);

    fn sighting(c: &mut Confirmation, retries: u32, at: Instant) {
        assert_eq!(c.observe(Some(retries), false, at, 0, R, 17), Step::Idle);
    }

    #[test]
    fn removal_with_retries_left_is_an_ack() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.published(7, false, t, true);
        assert_eq!(c.shown(false), None);
        sighting(&mut c, 3, t);
        assert_eq!(c.observe(None, false, t + R / 4, 0, R, 17), Step::Changed);
        assert_eq!(c.shown(false), Some(false));
        assert!(!c.needs_attention());
    }

    #[test]
    fn removal_near_the_last_retry_is_never_called_an_ack() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.published(7, false, t, true);
        sighting(&mut c, 16, t);
        assert_eq!(
            c.observe(None, false, t + Duration::from_millis(1), 0, R, 17),
            Step::Republish
        );
        assert_eq!(c.shown(false), None);
        c.published(8, false, t, false);
        sighting(&mut c, 17, t);
        assert_eq!(c.observe(None, false, t, 0, R, 17), Step::Changed);
        assert_eq!(c.shown(false), None);
        assert!(c.needs_attention());
    }

    #[test]
    fn a_late_sighting_gap_is_unconfirmed() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.published(7, true, t, true);
        sighting(&mut c, 0, t);
        assert_eq!(c.observe(None, false, t + R, 0, R, 17), Step::Republish);
        assert_eq!(c.shown(true), None);
    }

    #[test]
    fn report_republish_waits_for_new_report_and_gap() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.publish_failed(t, 5);
        assert_eq!(c.owner_report(t + REPORT_REPUBLISH_GAP, 5), Step::Idle);
        assert_eq!(c.owner_report(t + Duration::from_secs(1), 6), Step::Idle);
        assert_eq!(c.owner_report(t + REPORT_REPUBLISH_GAP, 6), Step::Republish);
    }

    #[test]
    fn toggle_flips_confirmed_value_not_request() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        assert!(c.toggle_target(false));
        c.published(1, true, t, true);
        sighting(&mut c, 0, t);
        c.observe(None, false, t, 0, R, 17);
        // Off requested and lost: the board is still On, so a press asks Off.
        c.published(2, false, t, true);
        assert!(!c.toggle_target(false));
        assert!(c.delivering(false));
    }

    #[test]
    fn unseen_queued_owner_waits_then_counts_as_unconfirmed() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.published(1, true, t, true);
        assert_eq!(c.observe(None, true, t + R, 0, R, 17), Step::Idle);
        assert_eq!(c.observe(None, true, t + R * 30, 0, R, 17), Step::Republish);
    }
}
