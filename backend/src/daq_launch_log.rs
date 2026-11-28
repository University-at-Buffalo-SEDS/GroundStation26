//! DAQ's actual SD launch-journal state, never inferred from delivery ACKs.
use crate::types::DaqLaunchLogStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observation {
    session: u64,
    deadline_utc_ms: u64,
    revision: u64,
    flags: u32,
}
/// Transport delay belongs to this report, not to later ordinary traffic.
/// Before UTC synchronization, retain receipt-time compatibility.
pub(crate) fn source_age_ms(source: u64, network_now: u64) -> Option<u64> {
    const UTC_2020_MS: u64 = 1_577_836_800_000;
    if source < UTC_2020_MS || network_now < UTC_2020_MS {
        return Some(0);
    }
    if source > network_now {
        return (source - network_now <= 100).then_some(0);
    }
    Some(network_now - source)
}

impl Observation {
    pub(crate) fn decode(words: &[u32]) -> Option<Self> {
        if words.len() != 7 || words[6] & !0x7f != 0 {
            return None;
        }
        let pair = |low: usize| u64::from(words[low]) | (u64::from(words[low + 1]) << 32);
        Some(Self {
            session: pair(0),
            deadline_utc_ms: pair(2),
            revision: pair(4),
            flags: words[6],
        })
    }
    pub(crate) fn project(
        &self,
        received_age_ms: u64,
        expected: Option<([u64; 2], u64)>,
    ) -> DaqLaunchLogStatus {
        DaqLaunchLogStatus {
            session: self.session,
            deadline_utc_ms: self.deadline_utc_ms,
            revision: self.revision,
            flags: self.flags,
            received_age_ms,
            matches_expected: expected.map(|(pair, revision)| {
                pair == [self.session, self.deadline_utc_ms] && revision == self.revision
            }),
            expected_session: expected.map(|(pair, _)| pair[0]),
            expected_deadline_utc_ms: expected.map(|(pair, _)| pair[1]),
            expected_revision: expected.map(|(_, revision)| revision),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_integer_state_is_compared_to_pair_and_revision_including_reset() {
        let words = [
            u32::MAX - 1,
            0x12345678,
            99,
            0xabcd,
            u32::MAX,
            0xfedcba98,
            0,
        ];
        let observation = Observation::decode(&words).unwrap();
        let pair = [0x12345678_fffffffe, 0xabcd_00000063];
        let revision = 0xfedcba98_ffffffff;
        let unknown = observation.project(0, None);
        assert_eq!(unknown.matches_expected, None);
        assert_eq!(unknown.expected_session, None);
        let matched = observation.project(13_000, Some((pair, revision)));
        assert_eq!(matched.session, pair[0]);
        assert_eq!(matched.deadline_utc_ms, pair[1]);
        assert_eq!(matched.revision, revision);
        assert_eq!(matched.matches_expected, Some(true));
        assert_eq!(
            matched.received_age_ms, 13_000,
            "matching does not imply freshness"
        );
        assert_eq!(
            matched.flags, 0,
            "acceptance does not invent ready/open/flush flags"
        );
        for expected in [
            (pair, revision - 1),
            ([pair[0] + 1, pair[1]], revision),
            ([pair[0], pair[1] + 1], revision),
            ([0, 0], revision + 1),
        ] {
            assert_eq!(
                observation.project(0, Some(expected)).matches_expected,
                Some(false)
            );
        }
        let reset = Observation::decode(&[0, 0, 0, 0, 9, 0, 1]).unwrap();
        assert_eq!(
            reset.project(0, Some(([0, 0], 9))).matches_expected,
            Some(true)
        );
        assert_eq!(
            reset.project(0, Some(([0, 0], 10))).matches_expected,
            Some(false)
        );
    }
    #[test]
    fn transport_age_preserves_delays_and_rejects_material_future_captures() {
        let now = 1_798_000_000_000;
        assert_eq!(source_age_ms(now - 30_000, now), Some(30_000));
        assert_eq!(source_age_ms(now + 100, now), Some(0));
        assert_eq!(source_age_ms(now + 101, now), None);
        assert_eq!(source_age_ms(123, now), Some(0));
        assert_eq!(source_age_ms(now, 123), Some(0));
    }
    #[test]
    fn wire_contract_has_exact_length_and_rejects_reserved_flags() {
        assert!(Observation::decode(&[0; 6]).is_none());
        assert!(Observation::decode(&[0; 8]).is_none());
        for flags in [0, 1, 3, 0x7f] {
            let mut words = [0; 7];
            words[6] = flags;
            assert_eq!(Observation::decode(&words).unwrap().flags, flags);
        }
        let mut words = [0; 7];
        words[6] = 0x80;
        assert!(Observation::decode(&words).is_none());
    }
}
