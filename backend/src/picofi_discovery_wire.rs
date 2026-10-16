//! PICOFI1 LAN discovery. No application packets or commands enter this codec.
//! This file is mirrored in GroundStation; fixed vectors define the wire contract.
pub const PORT: u16 = 49361;
pub const SIZE: usize = 32;
const MAGIC: &[u8; 8] = b"PCFIND01";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Party {
    GroundStation = 1,
    Pico = 2,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Query = 1,
    Offer = 2,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: Kind,
    pub party: Party,
    pub port: u16,
    pub nonce: u64,
    pub identity: [u8; 6],
}
impl Message {
    pub fn encode(self) -> [u8; SIZE] {
        let mut bytes = [0; SIZE];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8] = self.kind as u8;
        bytes[9] = self.party as u8;
        bytes[10..12].copy_from_slice(&self.port.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.nonce.to_le_bytes());
        bytes[20..26].copy_from_slice(&self.identity);
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != SIZE || &bytes[..8] != MAGIC || bytes[26..].iter().any(|b| *b != 0) {
            return None;
        }
        let kind = match bytes[8] {
            1 => Kind::Query,
            2 => Kind::Offer,
            _ => return None,
        };
        let party = match bytes[9] {
            1 => Party::GroundStation,
            2 => Party::Pico,
            _ => return None,
        };
        let port = u16::from_le_bytes(bytes[10..12].try_into().ok()?);
        if (kind == Kind::Query && port != 0) || (kind == Kind::Offer && port == 0) {
            return None;
        }
        let nonce = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
        if nonce == 0 {
            return None;
        }
        Some(Self {
            kind,
            party,
            port,
            nonce,
            identity: bytes[20..26].try_into().ok()?,
        })
    }
    pub fn compatible_offer(self, query: Self) -> bool {
        self.kind == Kind::Offer
            && query.kind == Kind::Query
            && self.party != query.party
            && self.nonce == query.nonce
            && self.identity != query.identity
            && self.port != 0
    }
}

/// Locally administered unicast MAC from the physical flash UID, not a shared UF2.
pub fn hardware_mac(uid: &[u8]) -> [u8; 6] {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in uid {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    let h = hash.to_le_bytes();
    [0x02, h[0], h[1], h[2], h[3], h[4]]
}
#[cfg(test)]
mod tests {
    use super::*;
    fn query() -> Message {
        Message {
            kind: Kind::Query,
            party: Party::Pico,
            port: 0,
            nonce: 0x0807060504030201,
            identity: [2, 3, 4, 5, 6, 7],
        }
    }
    #[test]
    fn fixed_vector_roundtrip_and_strict_shape() {
        let q = query();
        let b = q.encode();
        assert_eq!(&b[..12], b"PCFIND01\x01\x02\x00\x00");
        assert_eq!(&b[12..20], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(Message::decode(&b), Some(q));
        for length in 0..SIZE {
            assert!(Message::decode(&b[..length]).is_none());
        }
        let mut extra = b.to_vec();
        extra.push(0);
        assert!(Message::decode(&extra).is_none());
        for index in [0, 7, 8, 9, 10, 26, 31] {
            let mut bad = b;
            bad[index] = 0xff;
            assert!(Message::decode(&bad).is_none());
        }
        let mut zero = q;
        zero.nonce = 0;
        assert!(Message::decode(&zero.encode()).is_none());
    }
    #[test]
    fn responses_require_opposite_party_and_current_nonce() {
        let q = query();
        let mut offer = Message {
            kind: Kind::Offer,
            party: Party::GroundStation,
            port: 9000,
            nonce: q.nonce,
            identity: [2, 8, 9, 10, 11, 12],
        };
        assert!(offer.compatible_offer(q));
        offer.nonce += 1;
        assert!(!offer.compatible_offer(q));
        offer.nonce = q.nonce;
        offer.party = q.party;
        assert!(!offer.compatible_offer(q));
        offer.party = Party::GroundStation;
        offer.identity = q.identity;
        assert!(!offer.compatible_offer(q));
        offer.identity = [1; 6];
        offer.port = 0;
        assert!(Message::decode(&offer.encode()).is_none());
    }
}
