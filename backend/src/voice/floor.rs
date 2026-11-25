//! Talk floor for crew voice: who is on air.
//!
//! Any number of participants (up to the channel cap) listen, but at most
//! `max_talkers` are on air at once. Only on-air talkers are forwarded, so
//! the station's egress is bounded by `max_talkers × listeners`, not by
//! `participants²`.
//!
//! A participant asks for the floor with `transmitting: true` (push to talk
//! pressed, or open mic). The rules, applied by [`select`]:
//!
//! 1. An on-air talker keeps the floor while it wants it.
//! 2. Waiting requests are served priority first, then oldest first.
//! 3. When the floor is full, a waiter may take the slot of the on-air
//!    talker that has been quiet longest, once that talker has been quiet
//!    for `quiet_yield_ms` (an open mic left on does not block the channel).
//! 4. A priority waiter (an application administrator, e.g. the range
//!    safety officer) takes the slot of the non-priority talker that has
//!    been on air longest when no slot is quiet. Priority talkers are never
//!    displaced except by rule 3.
//!
//! A displaced talker keeps its request and waits at the back of the queue;
//! it goes back on air automatically when a slot frees.
use std::collections::BTreeSet;

/// Default and bounds for `GS_VOICE_MAX_PARTICIPANTS`.
pub(crate) const DEFAULT_MAX_PARTICIPANTS: usize = 32;
pub(crate) const MAX_PARTICIPANTS_RANGE: (usize, usize) = (2, 128);
/// Default and bounds for `GS_VOICE_MAX_TALKERS`.
pub(crate) const DEFAULT_MAX_TALKERS: usize = 4;
pub(crate) const MAX_TALKERS_RANGE: (usize, usize) = (1, 8);
/// An on-air talker quiet this long yields to a waiter (rule 3).
pub(crate) const QUIET_YIELD_MS: u64 = 1_500;
/// A frame whose RMS is below this (about −40 dBFS) counts as quiet.
pub(crate) const QUIET_RMS: f64 = 330.0;

/// Channel limits, from the environment at start-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub max_participants: usize,
    pub max_talkers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_participants: DEFAULT_MAX_PARTICIPANTS,
            max_talkers: DEFAULT_MAX_TALKERS,
        }
    }
}

impl Limits {
    /// `GS_VOICE_MAX_PARTICIPANTS` / `GS_VOICE_MAX_TALKERS`; unset, empty or
    /// unparsable values use the defaults, and values are clamped to the
    /// supported ranges. The talker cap never exceeds the participant cap.
    pub(crate) fn from_env() -> Self {
        Self::parse(
            std::env::var("GS_VOICE_MAX_PARTICIPANTS").ok().as_deref(),
            std::env::var("GS_VOICE_MAX_TALKERS").ok().as_deref(),
        )
    }

    pub(crate) fn parse(participants: Option<&str>, talkers: Option<&str>) -> Self {
        let read = |raw: Option<&str>, default: usize, (low, high): (usize, usize)| {
            raw.and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(default)
                .clamp(low, high)
        };
        let max_participants = read(participants, DEFAULT_MAX_PARTICIPANTS, MAX_PARTICIPANTS_RANGE);
        let max_talkers =
            read(talkers, DEFAULT_MAX_TALKERS, MAX_TALKERS_RANGE).min(max_participants);
        Self {
            max_participants,
            max_talkers,
        }
    }
}

/// One participant as the floor sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub id: u32,
    /// Stamp of the current request; `None` when not asking to talk.
    pub wants_since: Option<u64>,
    /// Stamp of the grant when on air.
    pub on_air_since: Option<u64>,
    pub priority: bool,
    /// Milliseconds since this participant last sent a voiced frame (or
    /// was put on air).
    pub quiet_ms: u64,
}

/// Outcome of [`select`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Floor {
    /// Participants on air after this decision.
    pub on_air: BTreeSet<u32>,
    /// Talkers that lost the floor to a waiter while still asking for it.
    pub displaced: BTreeSet<u32>,
}

/// Applies the floor rules (module docs) to `candidates`.
pub(crate) fn select(candidates: &[Candidate], max_talkers: usize, quiet_yield_ms: u64) -> Floor {
    // Rule 1: current talkers that still want the floor keep it.
    let mut on_air: Vec<Candidate> = candidates
        .iter()
        .filter(|c| c.wants_since.is_some() && c.on_air_since.is_some())
        .cloned()
        .collect();
    let mut displaced = BTreeSet::new();
    // A lowered cap (never at run time today) trims the newest grants.
    on_air.sort_by_key(|c| (!c.priority, c.on_air_since, c.id));
    while on_air.len() > max_talkers {
        if let Some(c) = on_air.pop() {
            displaced.insert(c.id);
        }
    }
    // Rule 2: priority first, then oldest request.
    let mut waiting: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.wants_since.is_some() && c.on_air_since.is_none())
        .collect();
    waiting.sort_by_key(|c| (!c.priority, c.wants_since, c.id));
    for waiter in waiting {
        // Granted in this pass: not quiet, and the newest grant.
        let granted = Candidate {
            quiet_ms: 0,
            on_air_since: Some(u64::MAX),
            ..waiter.clone()
        };
        if on_air.len() < max_talkers {
            on_air.push(granted);
            continue;
        }
        // Rule 3: the quietest talker past the yield time.
        let quiet = on_air
            .iter()
            .enumerate()
            .filter(|(_, c)| c.quiet_ms >= quiet_yield_ms)
            .max_by_key(|(_, c)| (c.quiet_ms, std::cmp::Reverse(c.id)))
            .map(|(i, _)| i);
        // Rule 4: a priority waiter displaces the longest non-priority talker.
        let victim = quiet.or_else(|| {
            waiter.priority.then(|| {
                on_air
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| !c.priority)
                    .min_by_key(|(_, c)| (c.on_air_since, c.id))
                    .map(|(i, _)| i)
            })?
        });
        let Some(index) = victim else {
            // Later waiters rank no higher, so none of them can get in.
            break;
        };
        displaced.insert(on_air[index].id);
        on_air[index] = granted;
    }
    Floor {
        on_air: on_air.into_iter().map(|c| c.id).collect(),
        displaced,
    }
}

/// RMS of one PCM16 little-endian frame.
pub(crate) fn rms(pcm: &[u8]) -> f64 {
    let n = pcm.len() / 2;
    if n == 0 {
        return 0.0;
    }
    let sum: f64 = pcm
        .chunks_exact(2)
        .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])).powi(2))
        .sum();
    (sum / n as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u32, wants: Option<u64>, on_air: Option<u64>, priority: bool, quiet_ms: u64) -> Candidate {
        Candidate {
            id,
            wants_since: wants,
            on_air_since: on_air,
            priority,
            quiet_ms,
        }
    }
    fn ids(set: &BTreeSet<u32>) -> Vec<u32> {
        set.iter().copied().collect()
    }

    #[test]
    fn limits_default_clamp_and_parse() {
        assert_eq!(Limits::parse(None, None), Limits { max_participants: 32, max_talkers: 4 });
        assert_eq!(Limits::default(), Limits::parse(Some(""), Some("x")));
        assert_eq!(Limits::parse(Some(" 48 "), Some("3")), Limits { max_participants: 48, max_talkers: 3 });
        assert_eq!(Limits::parse(Some("1"), Some("0")), Limits { max_participants: 2, max_talkers: 1 });
        assert_eq!(Limits::parse(Some("100000"), Some("99")), Limits { max_participants: 128, max_talkers: 8 });
        // The talker cap never exceeds the participant cap.
        assert_eq!(Limits::parse(Some("2"), Some("8")), Limits { max_participants: 2, max_talkers: 2 });
    }

    #[test]
    fn requests_are_granted_oldest_first_up_to_the_cap() {
        let peers = [
            c(1, Some(5), None, false, 0),
            c(2, Some(1), None, false, 0),
            c(3, None, None, false, 0),
            c(4, Some(3), None, false, 0),
            c(5, Some(2), None, false, 0),
            c(6, Some(4), None, false, 0),
        ];
        let floor = select(&peers, 3, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![2, 4, 5]);
        assert!(floor.displaced.is_empty());
        // Nobody wants the floor: nobody is on air.
        assert!(select(&[c(1, None, Some(1), false, 0)], 3, QUIET_YIELD_MS).on_air.is_empty());
    }

    #[test]
    fn talkers_keep_the_floor_and_a_freed_slot_goes_to_the_oldest_waiter() {
        let mut peers = vec![
            c(1, Some(1), Some(1), false, 0),
            c(2, Some(2), Some(2), false, 0),
            c(3, Some(3), None, false, 0),
            c(4, Some(4), None, false, 0),
        ];
        // A newer request never bumps a talking speaker.
        assert_eq!(ids(&select(&peers, 2, QUIET_YIELD_MS).on_air), vec![1, 2]);
        // Talker 1 releases push-to-talk: the oldest waiter (3) goes on air.
        peers[0].wants_since = None;
        let floor = select(&peers, 2, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![2, 3]);
        assert!(floor.displaced.is_empty());
    }

    #[test]
    fn a_quiet_open_mic_yields_to_a_waiter_and_rejoins_the_queue() {
        let peers = [
            c(1, Some(1), Some(1), false, 200),
            c(2, Some(2), Some(2), false, 4_000), // open mic, silent
            c(3, Some(3), Some(3), false, 1_600), // quiet, but less so
            c(4, Some(9), None, false, 0),
        ];
        let floor = select(&peers, 3, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![1, 3, 4]);
        assert_eq!(ids(&floor.displaced), vec![2]);
        // Two waiters take both quiet slots, quietest first.
        let mut more = peers.to_vec();
        more.push(c(5, Some(10), None, false, 0));
        let floor = select(&more, 3, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![1, 4, 5]);
        assert_eq!(ids(&floor.displaced), vec![2, 3]);
        // A waiter granted in this pass is never treated as quiet, however
        // long it waited silently.
        let fresh = [
            c(1, Some(1), Some(1), false, 0),
            c(2, Some(2), None, false, 60_000),
            c(3, Some(3), None, false, 0),
        ];
        let floor = select(&fresh, 2, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![1, 2]);
        assert!(floor.displaced.is_empty());
        // Nobody quiet long enough: the waiter waits.
        let busy = [
            c(1, Some(1), Some(1), false, 0),
            c(2, Some(2), Some(2), false, QUIET_YIELD_MS - 1),
            c(3, Some(3), None, false, 0),
        ];
        let floor = select(&busy, 2, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![1, 2]);
        assert!(floor.displaced.is_empty());
    }

    #[test]
    fn a_priority_request_overrides_the_longest_ordinary_talker() {
        let peers = [
            c(1, Some(1), Some(10), false, 0),
            c(2, Some(2), Some(5), false, 0), // on air longest
            c(3, Some(3), Some(7), true, 0),  // priority talker
            c(4, Some(20), None, true, 0),    // admin presses talk
            c(5, Some(4), None, false, 0),    // older ordinary waiter
        ];
        let floor = select(&peers, 3, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![1, 3, 4]);
        assert_eq!(ids(&floor.displaced), vec![2]);
        // Priority waiters are served before older ordinary waiters.
        let open = [c(5, Some(1), None, false, 0), c(4, Some(9), None, true, 0)];
        assert_eq!(ids(&select(&open, 1, QUIET_YIELD_MS).on_air), vec![4]);
        // A floor full of priority talkers is not overridden.
        let full = [
            c(1, Some(1), Some(1), true, 0),
            c(2, Some(2), Some(2), true, 0),
            c(3, Some(3), None, true, 0),
        ];
        let floor = select(&full, 2, QUIET_YIELD_MS);
        assert_eq!(ids(&floor.on_air), vec![1, 2]);
        assert!(floor.displaced.is_empty());
        // An ordinary waiter never overrides.
        let ordinary = [c(1, Some(1), Some(1), false, 0), c(2, Some(2), None, false, 0)];
        assert_eq!(ids(&select(&ordinary, 1, QUIET_YIELD_MS).on_air), vec![1]);
    }

    #[test]
    fn the_cap_holds_for_any_mix_of_requests() {
        // 40 participants, all asking, some on air, some quiet, some priority.
        for max in 1..=8 {
            let peers: Vec<_> = (0..40u32)
                .map(|i| {
                    c(
                        i,
                        Some(u64::from(i * 7 % 40)),
                        (i % 3 == 0).then_some(u64::from(i)),
                        i % 11 == 0,
                        u64::from(i) * 100,
                    )
                })
                .collect();
            let floor = select(&peers, max, QUIET_YIELD_MS);
            assert_eq!(floor.on_air.len(), max);
            assert!(floor.displaced.is_disjoint(&floor.on_air));
        }
    }

    #[test]
    fn rms_of_silence_and_tone() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0; 640]), 0.0);
        let tone: Vec<u8> = (0..320)
            .flat_map(|i| (((i as f64 / 16.0).sin() * 9000.0) as i16).to_le_bytes())
            .collect();
        assert!(rms(&tone) > QUIET_RMS * 10.0);
        let hiss: Vec<u8> = (0..320).flat_map(|i| (((i % 7) as i16 - 3) * 20).to_le_bytes()).collect();
        assert!(rms(&hiss) < QUIET_RMS);
    }
}
