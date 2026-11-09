//! Network confirmation for the two cosmetic network variables (AV-bay
//! underglow and the FC buzzer). Neither has an owner board: the GS owns
//! writing them, and their confirmed state comes from the variable itself,
//! the latest value seen published on the network, compared with the GS
//! request.
//!
//! A network value ("readback") is recorded from either source:
//! - the GS's own publication once the network has taken it. Admission to the
//!   radio queue is not delivery. The SDK retires a reliable frame either on
//!   the next hop's ACK or after its last retry, and both look the same from
//!   outside, so a removal counts only when the remaining retries could not
//!   yet have run out;
//! - an inbound network update of the variable from any sender.
//!
//! Freshness: a readback confirms the request only when it was seen at or
//! after the request was made, so a value from before a toggle never
//! confirms the toggle. These variables are not republished periodically
//! (the FC persists each new buzzer revision to flash), so there is no
//! wall-clock decay; a newer network value that differs replaces it.
//!
//! An unconfirmed request is published again: once straight away, then only
//! after the network has been heard from again, no sooner than a gap.
//!
//! FLIGHT_STATE and the fill-system commands keep their own paths.

use std::time::{Duration, Instant};

/// After the immediate republish also fails, wait at least this long before
/// new network traffic may trigger another one. A failed attempt keeps about
/// 4 frames/s on air for 4.5 s; with this gap it uses under a third of that.
pub(super) const REPUBLISH_GAP: Duration = Duration::from_secs(10);

/// One pending-row observation of the SDK's frozen reliable frame.
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

/// Latest value of the variable seen on the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Readback {
    value: bool,
    at: Instant,
}

/// Unconfirmed and idle since `since`. `reports` is the network traffic
/// count at that moment; None means the network was already heard from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    since: Instant,
    reports: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Idle,
    Changed,
    Republish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Confirmation {
    /// Latest network value in this process. None until one is seen.
    readback: Option<Readback>,
    /// When the current request was made (operator or startup publish).
    requested_at: Option<Instant>,
    inflight: Option<Inflight>,
    /// One immediate republish per operator request.
    immediate_left: bool,
    waiting: Option<Waiting>,
}

impl Default for Confirmation {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Confirmation {
    pub(super) const DEFAULT: Self = Self {
        readback: None,
        requested_at: None,
        inflight: None,
        immediate_left: false,
        waiting: None,
    };

    /// Confirmed value to show: the latest network value equals the request
    /// and was seen no earlier than the request.
    pub(super) fn shown(&self, requested: bool) -> Option<bool> {
        let readback = self.readback?;
        let fresh = self.requested_at.is_none_or(|at| readback.at >= at);
        (readback.value == requested && fresh).then_some(requested)
    }

    /// True once any network value for the variable has been seen.
    pub(super) fn readback_seen(&self) -> bool {
        self.readback.is_some()
    }

    /// A toggle flips the latest network value, not the stored request.
    /// Before any network value the stored request is the best knowledge.
    pub(super) fn toggle_target(&self, requested: bool) -> bool {
        !self.readback.map_or(requested, |r| r.value)
    }

    /// True when the same value is already being delivered.
    pub(super) fn delivering(&self, value: bool) -> bool {
        self.inflight.is_some_and(|i| i.value == value)
    }

    /// `fresh` marks a new request (operator or startup), which starts a new
    /// freshness epoch and earns one immediate republish on loss.
    pub(super) fn published(&mut self, packet_id: u64, value: bool, now: Instant, fresh: bool) {
        self.inflight = Some(Inflight {
            packet_id,
            value,
            admitted: now,
            seen: None,
        });
        if fresh {
            self.immediate_left = true;
            self.requested_at = Some(now);
        }
        self.waiting = None;
    }

    pub(super) fn publish_failed(&mut self, now: Instant, reports: u64, fresh: bool) {
        self.inflight = None;
        if fresh {
            self.requested_at = Some(now);
        }
        self.waiting = Some(Waiting {
            since: now,
            reports: Some(reports),
        });
    }

    /// An inbound network update of the variable, from any sender. A value
    /// that differs from the request while nothing is in flight waits for
    /// the republish gap and then publishes the request again.
    pub(super) fn network_value(&mut self, value: bool, requested: bool, now: Instant) -> Step {
        self.readback = Some(Readback { value, at: now });
        if value != requested && self.inflight.is_none() {
            self.waiting = Some(Waiting {
                since: self.waiting.map_or(now, |w| w.since),
                reports: None,
            });
        } else if value == requested && self.shown(requested).is_some() {
            self.waiting = None;
        }
        Step::Changed
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
            self.readback = Some(Readback {
                value: inflight.value,
                at: now,
            });
            self.waiting = None;
            return Step::Changed;
        }
        if self.immediate_left {
            self.immediate_left = false;
            return Step::Republish;
        }
        self.waiting = Some(Waiting {
            since: now,
            reports: Some(reports),
        });
        Step::Changed
    }

    /// Unconfirmed and idle: republish once the network has been heard from
    /// again, no sooner than the gap after the last failed attempt.
    pub(super) fn network_report(&self, now: Instant, reports: u64) -> Step {
        match self.waiting {
            Some(Waiting { since, reports: at })
                if at.is_none_or(|at| reports > at)
                    && now.duration_since(since) >= REPUBLISH_GAP =>
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
    fn removal_with_retries_left_is_a_network_readback() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        assert!(!c.readback_seen());
        c.published(7, false, t, true);
        assert_eq!(c.shown(false), None);
        sighting(&mut c, 3, t);
        assert_eq!(c.observe(None, false, t + R / 4, 0, R, 17), Step::Changed);
        assert_eq!(c.shown(false), Some(false));
        assert!(c.readback_seen());
        assert!(!c.needs_attention());
    }

    #[test]
    fn removal_near_the_last_retry_is_never_called_a_readback() {
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
        assert!(!c.readback_seen(), "no network value at all yet");
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
    fn republish_waits_for_network_traffic_and_gap() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.publish_failed(t, 5, true);
        assert_eq!(c.network_report(t + REPUBLISH_GAP, 5), Step::Idle);
        assert_eq!(c.network_report(t + Duration::from_secs(1), 6), Step::Idle);
        assert_eq!(c.network_report(t + REPUBLISH_GAP, 6), Step::Republish);
    }

    #[test]
    fn toggle_flips_network_value_not_request() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        assert!(c.toggle_target(false));
        c.published(1, true, t, true);
        sighting(&mut c, 0, t);
        c.observe(None, false, t, 0, R, 17);
        // Off requested and lost: the network still carries On, so a press asks Off.
        c.published(2, false, t, true);
        assert!(!c.toggle_target(false));
        assert!(c.delivering(false));
    }

    #[test]
    fn unseen_queued_frame_waits_then_counts_as_unconfirmed() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.published(1, true, t, true);
        assert_eq!(c.observe(None, true, t + R, 0, R, 17), Step::Idle);
        assert_eq!(c.observe(None, true, t + R * 30, 0, R, 17), Step::Republish);
    }

    #[test]
    fn any_sender_on_the_network_confirms_a_matching_request() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.publish_failed(t, 0, true);
        assert_eq!(c.shown(true), None);
        assert_eq!(c.network_value(true, true, t + R), Step::Changed);
        assert_eq!(c.shown(true), Some(true));
        assert!(!c.needs_attention());
    }

    #[test]
    fn a_readback_from_before_the_request_is_not_fresh() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.network_value(true, true, t);
        assert_eq!(c.shown(true), Some(true));
        // Off then On again: the old On predates the newest request.
        c.published(1, false, t + R, true);
        c.published(2, true, t + R * 2, true);
        assert_eq!(c.shown(true), None);
        assert!(c.readback_seen());
        c.network_value(true, true, t + R * 3);
        assert_eq!(c.shown(true), Some(true));
    }

    #[test]
    fn a_different_network_value_unconfirms_and_republishes_after_the_gap() {
        let t = Instant::now();
        let mut c = Confirmation::default();
        c.network_value(true, true, t);
        assert_eq!(c.network_value(false, true, t + R), Step::Changed);
        assert_eq!(c.shown(true), None);
        assert!(c.needs_attention());
        assert_eq!(c.network_report(t + R, 0), Step::Idle);
        assert_eq!(c.network_report(t + R + REPUBLISH_GAP, 0), Step::Republish);
    }
}
