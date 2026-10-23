//! One shared, paced allowance for mission settings across all paired peers.
//! The backend must schedule this below commands/GPS and validate completed
//! documents through its typed adapter before merging or applying them.
use crate::{Document, Store, auth, transport::Protocol};
use ring::signature::Ed25519KeyPair;

pub const BYTES_PER_SECOND: u64 = 256;
const MAX_PEERS: usize = 8;
struct Peer {
    id: String,
    protocol: Protocol,
    pending: Option<Vec<u8>>,
}
pub struct RadioSync {
    installation: String,
    station: String,
    peers: Vec<Peer>,
    cursor: usize,
    next_at: Option<u64>,
    clock_exhausted: bool,
}
#[derive(Debug, PartialEq)]
pub struct Attempt {
    pub recipient: String,
    pub bytes: usize,
    pub accepted: bool,
}
impl RadioSync {
    pub fn new(installation: String, station: String, peers: Vec<String>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            crate::identity(&installation)
                && crate::identity(&station)
                && !peers.is_empty()
                && peers.len() <= MAX_PEERS,
            "invalid mission radio identity/peer count"
        );
        let mut seen = std::collections::BTreeSet::new();
        for peer in &peers {
            anyhow::ensure!(
                crate::identity(peer) && peer != &station && seen.insert(peer),
                "invalid or duplicate mission radio peer"
            );
        }
        Ok(Self {
            installation,
            station,
            peers: peers
                .into_iter()
                .map(|id| Peer {
                    id,
                    protocol: Protocol::default(),
                    pending: None,
                })
                .collect(),
            cursor: 0,
            next_at: None,
            clock_exhausted: false,
        })
    }
    /// Attempt at most one signed frame. Charge its actual wire length even on
    /// backpressure, retain the exact frame for retry, then give the next peer
    /// a turn. Idle time cannot accumulate an unbounded transmission burst.
    /// `now` is the monotonic clock in milliseconds, not wall/network time.
    pub fn poll(
        &mut self,
        store: &Store,
        now: u64,
        signer: &Ed25519KeyPair,
        mut send: impl FnMut(&str, &[u8]) -> bool,
    ) -> anyhow::Result<Option<Attempt>> {
        if self.clock_exhausted || self.next_at.is_some_and(|at| now < at) {
            return Ok(None);
        }
        for _ in 0..self.peers.len() {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.peers.len();
            let peer = &mut self.peers[index];
            if peer.pending.is_none()
                && let Some(message) = peer.protocol.poll(store, now)
            {
                peer.pending = Some(auth::seal(
                    signer,
                    &self.installation,
                    &self.station,
                    &peer.id,
                    message,
                )?);
            }
            let Some(wire) = peer.pending.as_ref() else {
                continue;
            };
            let bytes = wire.len();
            let delay = (bytes as u64 * 1000).div_ceil(BYTES_PER_SECOND);
            self.next_at = now.checked_add(delay);
            self.clock_exhausted = self.next_at.is_none();
            let accepted = send(&peer.id, wire);
            if accepted {
                peer.pending = None;
            }
            return Ok(Some(Attempt {
                recipient: peer.id.clone(),
                bytes,
                accepted,
            }));
        }
        Ok(None)
    }
    /// Authentication precedes parsing by the per-peer reconciliation protocol.
    /// Returning a document does not authorize applying it to runtime settings.
    pub fn receive(
        &mut self,
        store: &Store,
        wire: &[u8],
        now: u64,
        verify_key: &[u8],
    ) -> anyhow::Result<Option<(String, Document)>> {
        let issuers = self.peers.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        let verified = auth::open(
            wire,
            &self.installation,
            &self.station,
            &issuers,
            verify_key,
        )?;
        let peer = self
            .peers
            .iter_mut()
            .find(|p| p.id == verified.issuer)
            .ok_or_else(|| anyhow::anyhow!("unpaired mission radio peer"))?;
        peer.protocol.receive(store, verified.message, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;
    fn key() -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&[31; 32]).unwrap()
    }
    fn scheduler(peers: &[&str]) -> RadioSync {
        RadioSync::new(
            "launch".into(),
            "gs".into(),
            peers.iter().map(|s| (*s).into()).collect(),
        )
        .unwrap()
    }
    #[test]
    fn actual_signed_bytes_share_one_budget_and_failed_peer_cannot_starve_others() {
        let mut radio = scheduler(&["rrls-a", "rrls-b", "rrls-c"]);
        let store = Store::new("launch".into()).unwrap();
        let signer = key();
        let mut events = Vec::new();
        for now in 0..60_000 {
            radio
                .poll(&store, now, &signer, |peer, wire| {
                    events.push((now, peer.to_owned(), wire.len()));
                    peer != "rrls-a"
                })
                .unwrap();
        }
        assert!(events.len() >= 9);
        assert_eq!(
            events
                .iter()
                .take(3)
                .map(|e| e.1.as_str())
                .collect::<Vec<_>>(),
            ["rrls-a", "rrls-b", "rrls-c"]
        );
        for pair in events.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= (pair[0].2 as u64 * 1000).div_ceil(BYTES_PER_SECOND));
        }
        let bytes: usize = events.iter().map(|e| e.2).sum();
        assert!(bytes <= 60 * BYTES_PER_SECOND as usize + events.last().unwrap().2);
        for peer in ["rrls-a", "rrls-b", "rrls-c"] {
            assert!(events.iter().filter(|e| e.1 == peer).count() >= 3);
        }
    }
    #[test]
    fn backpressure_retries_exact_envelope_without_busy_loop_or_clock_burst() {
        let mut radio = scheduler(&["rrls"]);
        let store = Store::new("launch".into()).unwrap();
        let signer = key();
        let mut first = Vec::new();
        let attempt = radio
            .poll(&store, 500, &signer, |_, wire| {
                first = wire.to_vec();
                false
            })
            .unwrap()
            .unwrap();
        assert!(!attempt.accepted);
        let next = 500 + (first.len() as u64 * 1000).div_ceil(BYTES_PER_SECOND);
        for now in [0, 500, next - 1] {
            assert!(
                radio
                    .poll(&store, now, &signer, |_, _| panic!("unpaced retry"))
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            radio
                .poll(&store, next, &signer, |_, wire| {
                    assert_eq!(wire, first);
                    true
                })
                .unwrap()
                .unwrap()
                .accepted
        );
        // A long idle period permits only one frame, not accumulated credit.
        assert!(
            radio
                .poll(&store, 1_000_000, &signer, |_, _| true)
                .unwrap()
                .is_some()
        );
        assert!(
            radio
                .poll(&store, 1_000_000, &signer, |_, _| panic!("idle burst"))
                .unwrap()
                .is_none()
        );
        assert!(
            radio
                .poll(&store, u64::MAX, &signer, |_, _| true)
                .unwrap()
                .is_some()
        );
        assert!(
            radio
                .poll(&store, u64::MAX, &signer, |_, _| panic!(
                    "clock overflow retry"
                ))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn receive_rejects_unpaired_frames_and_peer_catalog_is_bounded() {
        let mut radio = scheduler(&["rrls"]);
        let store = Store::new("launch".into()).unwrap();
        let signer = key();
        let message = || crate::transport::Message::Inventory {
            root: "x".repeat(43),
        };
        for issuer in ["intruder", "gs"] {
            let wire = auth::seal(&signer, "launch", issuer, "gs", message()).unwrap();
            assert!(
                radio
                    .receive(&store, &wire, 0, signer.public_key().as_ref())
                    .is_err()
            );
        }
        for peers in [
            vec![],
            vec!["gs".into()],
            vec!["rrls".into(), "rrls".into()],
            (0..9).map(|n| format!("peer-{n}")).collect(),
        ] {
            assert!(RadioSync::new("launch".into(), "gs".into(), peers).is_err());
        }
    }
}
