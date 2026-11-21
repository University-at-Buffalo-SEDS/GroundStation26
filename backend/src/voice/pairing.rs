//! Echo avoidance between one person's devices on crew voice.
//!
//! A phone session and the watch whose token that session issued form a
//! *group* (`auth::voice_device_group`). Within a group:
//!
//! * audio is never relayed between members, so the watch never plays the
//!   phone's microphone (or the reverse) and no loop can form;
//! * while a watch member is *active* (it joined, or claimed the floor
//!   again after yielding), every other member is *held*: its microphone
//!   is ignored and it receives no audio, so the phone stays silent while
//!   the watch plays and talks. The latest active watch wins when there
//!   are several.
//!
//! Peers of different groups are unaffected, even on the same account (a
//! shared crew login on two people's phones still works).
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Member<'a> {
    pub id: u32,
    pub group: &'a str,
    pub watch: bool,
    /// Monotonic stamp of the latest floor claim; `None` when not claiming.
    pub active_since: Option<u64>,
}

/// The peers that are held: every member of a group other than its floor
/// holder, when the group has one.
pub(crate) fn held(members: &[Member<'_>]) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for m in members {
        if let Some(holder) = floor_holder(members, m.group)
            && holder != m.id
        {
            out.insert(m.id);
        }
    }
    out
}

/// The active watch of `group` with the latest claim.
pub(crate) fn floor_holder(members: &[Member<'_>], group: &str) -> Option<u32> {
    members
        .iter()
        .filter(|m| m.group == group && m.watch)
        .filter_map(|m| m.active_since.map(|since| (since, m.id)))
        .max()
        .map(|(_, id)| id)
}

/// Whether `listener` receives audio from `speaker`.
pub(crate) fn delivers(
    speaker: &Member<'_>,
    listener: &Member<'_>,
    held: &BTreeSet<u32>,
) -> bool {
    speaker.id != listener.id
        && speaker.group != listener.group
        && !held.contains(&speaker.id)
        && !held.contains(&listener.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u32, group: &'static str, watch: bool, active_since: Option<u64>) -> Member<'static> {
        Member {
            id,
            group,
            watch,
            active_since,
        }
    }

    /// Shared vectors: `voice.pairing` in the frontend `watch/test-vectors.json`
    /// carries the same cases for documentation and the watch cores.
    #[test]
    fn phone_is_held_while_its_watch_is_active_and_never_hears_it() {
        let phone = m(1, "ada-phone", false, None);
        let watch = m(2, "ada-phone", true, Some(5));
        let bob = m(3, "bob-laptop", false, None);
        let all = [phone.clone(), watch.clone(), bob.clone()];
        let held = held(&all);
        assert_eq!(held, BTreeSet::from([1]));
        assert!(!delivers(&watch, &phone, &held), "watch audio never reaches its phone");
        assert!(!delivers(&phone, &watch, &held), "phone audio never reaches its watch");
        assert!(!delivers(&bob, &phone, &held), "the held phone plays nothing");
        assert!(!delivers(&phone, &bob, &held), "the held phone's microphone is ignored");
        assert!(delivers(&bob, &watch, &held));
        assert!(delivers(&watch, &bob, &held));
    }

    #[test]
    fn a_yielded_watch_releases_the_phone_but_the_pair_still_never_hear_each_other() {
        let phone = m(1, "g", false, None);
        let watch = m(2, "g", true, None);
        let all = [phone.clone(), watch.clone()];
        let held = held(&all);
        assert!(held.is_empty());
        assert!(!delivers(&watch, &phone, &held));
        assert!(!delivers(&phone, &watch, &held));
    }

    #[test]
    fn latest_watch_claim_wins_and_other_groups_are_untouched() {
        let all = [
            m(1, "g", true, Some(3)),
            m(2, "g", true, Some(9)),
            m(3, "g", false, None),
            // A shared crew login on two other people's phones: two groups.
            m(4, "h1", false, None),
            m(5, "h2", false, None),
        ];
        let held = held(&all);
        assert_eq!(held, BTreeSet::from([1, 3]));
        assert_eq!(floor_holder(&all, "g"), Some(2));
        assert_eq!(floor_holder(&all, "h1"), None);
        assert!(delivers(&all[3], &all[4], &held), "other groups talk normally");
        assert!(delivers(&all[1], &all[4], &held));
        assert!(!delivers(&all[0], &all[4], &held), "the yielded older watch is held");
    }

    #[test]
    fn a_web_peer_never_takes_the_floor() {
        let all = [m(1, "g", false, Some(7)), m(2, "g", false, None)];
        assert!(held(&all).is_empty());
    }
}
