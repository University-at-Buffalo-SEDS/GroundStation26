//! Application signatures are independent of HTTP, proxies and TLS certificates.
use crate::transport::Message;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature;
use serde::{Deserialize, Serialize};
const DOMAIN: &[u8] = b"gs-mission-settings-v1\0";
const MAX_BODY: usize = 2048;
const MAX_WIRE: usize = 3072;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: String,
    signature: String,
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Verified {
    pub installation: String,
    pub issuer: String,
    pub recipient: String,
    pub message: Message,
}
fn signed_bytes(payload: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(DOMAIN.len() + payload.len());
    data.extend_from_slice(DOMAIN);
    data.extend_from_slice(payload.as_bytes());
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
    let data = serde_json::to_vec(&Verified {
        installation: installation.into(),
        issuer: issuer.into(),
        recipient: recipient.into(),
        message,
    })?;
    anyhow::ensure!(
        data.len() <= MAX_BODY,
        "mission message exceeds signing budget"
    );
    let payload = B64.encode(data);
    let signature = B64.encode(key.sign(&signed_bytes(&payload)).as_ref());
    let wire = serde_json::to_vec(&Envelope { payload, signature })?;
    anyhow::ensure!(
        wire.len() <= MAX_WIRE,
        "mission envelope exceeds radio budget"
    );
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
        wire.len() <= MAX_WIRE && verify_key.len() == 32,
        "invalid mission envelope bounds"
    );
    let envelope: Envelope = serde_json::from_slice(wire)?;
    anyhow::ensure!(
        envelope.payload.len() <= MAX_BODY.div_ceil(3) * 4 && envelope.signature.len() == 86,
        "invalid mission signature bounds"
    );
    signature::UnparsedPublicKey::new(&signature::ED25519, verify_key)
        .verify(
            &signed_bytes(&envelope.payload),
            &B64.decode(envelope.signature)?,
        )
        .map_err(|_| anyhow::anyhow!("invalid mission message signature"))?;
    let data = B64.decode(envelope.payload)?;
    anyhow::ensure!(data.len() <= MAX_BODY, "mission payload exceeds budget");
    let verified: Verified = serde_json::from_slice(&data)?;
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
        let mut envelope: Envelope = serde_json::from_slice(&wire).unwrap();
        envelope.payload.push('x');
        assert!(
            open(
                &serde_json::to_vec(&envelope).unwrap(),
                "launch",
                "rrls",
                &peers,
                signer.public_key().as_ref()
            )
            .is_err()
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
        let mut envelope: Envelope = serde_json::from_slice(&wire).unwrap();
        envelope.signature = B64.encode(
            signer
                .sign(format!("paired1.{}", envelope.payload).as_bytes())
                .as_ref(),
        );
        assert!(
            open(
                &serde_json::to_vec(&envelope).unwrap(),
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
