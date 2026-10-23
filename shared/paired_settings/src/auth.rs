//! Application signatures are independent of HTTP, proxies and TLS certificates.
use crate::transport::Message;
use ring::signature;
use serde::{Deserialize, Serialize};
const DOMAIN: &[u8] = b"gs-mission-settings-v1\0";
const MAGIC: &[u8] = b"MS\x01";
const MAX_BODY: usize = 2048;
const SIGNATURE_BYTES: usize = 64;
const HEADER_BYTES: usize = 5;
pub const MAX_WIRE: usize = MAX_BODY + HEADER_BYTES + SIGNATURE_BYTES;
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Verified {
    pub installation: String,
    pub issuer: String,
    pub recipient: String,
    pub message: Message,
}
fn signed_bytes(payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(DOMAIN.len() + payload.len());
    data.extend_from_slice(DOMAIN);
    data.extend_from_slice(payload);
    data
}
pub fn seal(
    key: &signature::Ed25519KeyPair,
    installation: &str,
    issuer: &str,
    recipient: &str,
    message: Message,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        [installation, issuer, recipient]
            .into_iter()
            .all(crate::identity),
        "invalid mission message identity"
    );
    let body = serde_json::to_vec(&Verified {
        installation: installation.into(),
        issuer: issuer.into(),
        recipient: recipient.into(),
        message,
    })?;
    anyhow::ensure!(
        body.len() <= MAX_BODY,
        "mission message exceeds signing budget"
    );
    let mut wire = Vec::with_capacity(body.len() + HEADER_BYTES + SIGNATURE_BYTES);
    wire.extend_from_slice(MAGIC);
    wire.extend_from_slice(&(body.len() as u16).to_le_bytes());
    wire.extend_from_slice(&body);
    let signature = key.sign(&signed_bytes(&wire));
    wire.extend_from_slice(signature.as_ref());
    Ok(wire)
}
pub fn open(
    wire: &[u8],
    installation: &str,
    recipient: &str,
    allowed_issuers: &[String],
    verify_key: &[u8],
) -> anyhow::Result<Verified> {
    anyhow::ensure!(
        (HEADER_BYTES + SIGNATURE_BYTES..=MAX_WIRE).contains(&wire.len())
            && verify_key.len() == 32
            && &wire[..3] == MAGIC,
        "invalid mission envelope bounds/version"
    );
    let length = usize::from(u16::from_le_bytes([wire[3], wire[4]]));
    let end = HEADER_BYTES + length;
    anyhow::ensure!(
        length <= MAX_BODY && end + SIGNATURE_BYTES == wire.len(),
        "invalid mission envelope length"
    );
    signature::UnparsedPublicKey::new(&signature::ED25519, verify_key)
        .verify(&signed_bytes(&wire[..end]), &wire[end..])
        .map_err(|_| anyhow::anyhow!("invalid mission message signature"))?;
    let verified: Verified = serde_json::from_slice(&wire[HEADER_BYTES..end])?;
    anyhow::ensure!(
        verified.installation == installation
            && verified.recipient == recipient
            && allowed_issuers.contains(&verified.issuer),
        "unpaired mission message identity"
    );
    Ok(verified)
}
#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;
    fn key(seed: u8) -> signature::Ed25519KeyPair {
        signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap()
    }
    #[test]
    fn signatures_bind_installation_recipient_peer_and_message_without_tls() {
        let signer = key(1);
        let peers = vec!["gs".into()];
        let wire = seal(
            &signer,
            "launch",
            "gs",
            "rrls",
            Message::Inventory {
                root: "x".repeat(43),
            },
        )
        .unwrap();
        assert_eq!(
            open(
                &wire,
                "launch",
                "rrls",
                &peers,
                signer.public_key().as_ref()
            )
            .unwrap()
            .issuer,
            "gs"
        );
        assert!(open(&wire, "other", "rrls", &peers, signer.public_key().as_ref()).is_err());
        assert!(open(&wire, "launch", "gs", &peers, signer.public_key().as_ref()).is_err());
        assert!(open(&wire, "launch", "rrls", &[], signer.public_key().as_ref()).is_err());
        assert!(
            open(
                &wire,
                "launch",
                "rrls",
                &peers,
                key(2).public_key().as_ref()
            )
            .is_err()
        );
        let mut tampered = wire.clone();
        tampered[HEADER_BYTES] ^= 1;
        assert!(
            open(
                &tampered,
                "launch",
                "rrls",
                &peers,
                signer.public_key().as_ref()
            )
            .is_err()
        );
        assert_eq!(
            wire.len(),
            HEADER_BYTES + usize::from(u16::from_le_bytes([wire[3], wire[4]])) + SIGNATURE_BYTES
        );
    }
    #[test]
    fn wrong_signature_domain_and_oversized_envelopes_are_rejected() {
        let signer = key(1);
        let peers = vec!["gs".into()];
        let wire = seal(
            &signer,
            "launch",
            "gs",
            "rrls",
            Message::Inventory {
                root: "x".repeat(43),
            },
        )
        .unwrap();
        let mut wrong_domain = wire.clone();
        let end = wire.len() - SIGNATURE_BYTES;
        wrong_domain[end..].copy_from_slice(signer.sign(&wire[..end]).as_ref());
        assert!(
            open(
                &wrong_domain,
                "launch",
                "rrls",
                &peers,
                signer.public_key().as_ref()
            )
            .is_err()
        );
        assert!(
            open(
                &vec![b' '; MAX_WIRE + 1],
                "launch",
                "rrls",
                &peers,
                signer.public_key().as_ref()
            )
            .is_err()
        );
        assert!(
            seal(
                &signer,
                "launch",
                "gs",
                "rrls",
                Message::Inventory {
                    root: "x".repeat(MAX_BODY)
                }
            )
            .is_err()
        );
    }
}
