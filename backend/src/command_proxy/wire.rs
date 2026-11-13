//! Bounded signed host control frames. These are never SEDSnet packets.
use crate::{station_mode::Mode, station_pairing::Pairing};
use anyhow::{Result, bail, ensure};
use ring::{
    digest,
    signature::{self, KeyPair},
};

const MAGIC: &[u8] = b"CP\x01";
const DOMAIN: &[u8] = b"gs26-rrls-command-proxy-v1\0";
pub(super) const MAX_WIRE: usize = 256;
pub(super) const MAX_ACTOR: usize = 64;
pub(super) const MAX_COMMAND: usize = 48;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Command(String),
    NetworkUtc(i64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Outcome {
    Queued = 0,
    Denied = 1,
    Invalid = 2,
    Unavailable = 3,
    Stale = 4,
    Busy = 5,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Message {
    Query,
    QueryObserve,
    QueryReadiness,
    Capability {
        mode: Mode,
        commands: u64,
        observation: Vec<u8>,
    },
    Request {
        launch_revision: Option<u64>,
        mode: Mode,
        actor: String,
        operation: Operation,
        plumbing_origin_utc_ms: Option<u64>,
    },
    Reply {
        digest: [u8; 16],
        outcome: Outcome,
        observation: Vec<u8>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Envelope {
    pub issuer: String,
    pub session: u32,
    pub sequence: u32,
    pub request: [u8; 16],
    pub boot: [u8; 16],
    pub message: Message,
}
fn identity(value: &str) -> [u8; 8] {
    digest::digest(&digest::SHA256, value.as_bytes()).as_ref()[..8]
        .try_into()
        .unwrap()
}
pub(super) fn contract_digest(wire: &[u8]) -> [u8; 16] {
    digest::digest(&digest::SHA256, wire).as_ref()[..16]
        .try_into()
        .unwrap()
}
fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::Normal => 0,
        Mode::Hitl => 1,
        Mode::TestFire => 2,
        Mode::Testing => 3,
    }
}
fn mode(value: u8) -> Result<Mode> {
    Ok(match value {
        0 => Mode::Normal,
        1 => Mode::Hitl,
        2 => Mode::TestFire,
        3 => Mode::Testing,
        _ => bail!("Unknown primary mode"),
    })
}
fn signed_bytes(pairing: &Pairing, issuer: &str, recipient: &str, wire: &[u8]) -> Vec<u8> {
    let mut bytes = DOMAIN.to_vec();
    for text in [&pairing.public.installation_id, issuer, recipient] {
        bytes.extend(text.as_bytes());
        bytes.push(0);
    }
    bytes.extend(wire);
    bytes
}
pub(super) fn seal(pairing: &Pairing, recipient: &str, envelope: &Envelope) -> Result<Vec<u8>> {
    ensure!(
        envelope.issuer == pairing.public.station_id,
        "Wrong local issuer"
    );
    ensure!(
        pairing.public.peers.iter().any(|peer| peer.id == recipient),
        "Unpaired recipient"
    );
    let mut wire = MAGIC.to_vec();
    wire.push(match envelope.message {
        Message::Query | Message::QueryObserve | Message::QueryReadiness => 0,
        Message::Capability { .. } => 1,
        Message::Request { .. } => 2,
        Message::Reply { .. } => 3,
    });
    for value in [
        &pairing.public.installation_id,
        &pairing.public.station_id,
        recipient,
    ] {
        wire.extend(identity(value));
    }
    wire.extend(envelope.session.to_le_bytes());
    wire.extend(envelope.sequence.to_le_bytes());
    wire.extend(envelope.request);
    wire.extend(envelope.boot);
    match &envelope.message {
        Message::Query => {}
        Message::QueryObserve => wire.push(0),
        Message::QueryReadiness => wire.push(1),
        Message::Capability {
            mode,
            commands,
            observation,
        } => {
            wire.push(mode_byte(*mode));
            wire.extend(commands.to_le_bytes());
            append_observation(&mut wire, observation)?;
        }
        Message::Request {
            launch_revision,
            mode,
            actor,
            operation,
            plumbing_origin_utc_ms,
        } => {
            ensure!(
                !actor.is_empty()
                    && actor.len() <= MAX_ACTOR
                    && !actor.chars().any(char::is_control),
                "Invalid actor"
            );
            wire.push(mode_byte(*mode));
            wire.push(actor.len() as u8);
            wire.extend(actor.as_bytes());
            match operation {
                Operation::Command(command) => {
                    ensure!(valid_command(command), "Invalid command");
                    wire.push(0);
                    wire.push(command.len() as u8);
                    wire.extend(command.as_bytes());
                }
                Operation::NetworkUtc(utc) => {
                    wire.push(1);
                    wire.extend(utc.to_le_bytes());
                }
            }
            ensure!(
                launch_revision.is_none()
                    || matches!(operation,Operation::Command(name) if launch_command(name)),
                "Launch origin extension is only valid for a potentially launching command"
            );
            ensure!(
                launch_revision.is_none() || plumbing_origin_utc_ms.is_none(),
                "Conflicting command origins"
            );
            if let Some(stamp) = plumbing_origin_utc_ms {
                ensure!(
                    matches!(operation, Operation::Command(name) if name=="RetractPlumbing")
                        && !matches!(*stamp, 0 | u64::MAX),
                    "Invalid plumbing origin extension"
                );
                wire.extend(stamp.to_le_bytes());
            }
            if let Some(revision) = launch_revision {
                ensure!(*revision != u64::MAX, "Exhausted launch revision");
                wire.extend(revision.to_le_bytes());
            }
        }
        Message::Reply {
            digest,
            outcome,
            observation,
        } => {
            wire.extend(digest);
            wire.push(*outcome as u8);
            append_observation(&mut wire, observation)?;
        }
    }
    ensure!(wire.len() + 64 <= MAX_WIRE, "Oversized control envelope");
    wire.extend(
        pairing
            .mission_signing_key()
            .sign(&signed_bytes(
                pairing,
                &pairing.public.station_id,
                recipient,
                &wire,
            ))
            .as_ref(),
    );
    Ok(wire)
}
fn append_observation(wire: &mut Vec<u8>, observation: &[u8]) -> Result<()> {
    ensure!(observation.len() <= 96, "Oversized observation");
    wire.push(observation.len() as u8);
    wire.extend(observation);
    Ok(())
}
pub(super) fn launch_command(name: &str) -> bool {
    matches!(
        name,
        "Launch" | "GroundStationLaunch" | "AdvanceFlightState" | "RewindFlightState"
    )
}
pub(super) fn valid_command(command: &str) -> bool {
    !command.is_empty()
        && command.len() <= MAX_COMMAND
        && command.bytes().all(|c| c.is_ascii_alphanumeric())
        && command != "AutomaticAbort"
}
struct Cursor<'a> {
    wire: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or_else(|| anyhow::anyhow!("Overflow"))?;
        ensure!(end <= self.wire.len(), "Truncated control envelope");
        let out = &self.wire[self.at..end];
        self.at = end;
        Ok(out)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn observation(&mut self) -> Result<Vec<u8>> {
        let size = usize::from(self.byte()?);
        ensure!(size <= 96, "Oversized observation");
        Ok(self.take(size)?.to_vec())
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
}
pub(super) fn open(pairing: &Pairing, wire: &[u8], issuer_role: &str) -> Result<Envelope> {
    ensure!(
        wire.len() >= 132 && wire.len() <= MAX_WIRE && wire.starts_with(MAGIC),
        "Invalid control envelope size/version"
    );
    let end = wire.len() - 64;
    let mut cursor = Cursor {
        wire: &wire[..end],
        at: MAGIC.len(),
    };
    let kind = cursor.byte()?;
    ensure!(
        cursor.array::<8>()? == identity(&pairing.public.installation_id),
        "Wrong installation"
    );
    let issuer_hash = cursor.array::<8>()?;
    let peers: Vec<_> = pairing
        .public
        .peers
        .iter()
        .filter(|peer| identity(&peer.id) == issuer_hash && peer.role == issuer_role)
        .collect();
    ensure!(peers.len() == 1, "Unknown/ambiguous issuer or role");
    let issuer = &peers[0].id;
    ensure!(
        cursor.array::<8>()? == identity(&pairing.public.station_id),
        "Wrong recipient"
    );
    signature::UnparsedPublicKey::new(
        &signature::ED25519,
        pairing.mission_signing_key().public_key().as_ref(),
    )
    .verify(
        &signed_bytes(pairing, issuer, &pairing.public.station_id, &wire[..end]),
        &wire[end..],
    )
    .map_err(|_| anyhow::anyhow!("Invalid control signature"))?;
    let session = u32::from_le_bytes(cursor.array()?);
    let sequence = u32::from_le_bytes(cursor.array()?);
    let request = cursor.array()?;
    let boot = cursor.array()?;
    let message = match kind {
        0 => match end.saturating_sub(cursor.at) {
            0 => Message::Query,
            1 => match cursor.byte()? {
                0 => Message::QueryObserve,
                1 => Message::QueryReadiness,
                _ => bail!("Unknown readiness query flag"),
            },
            _ => bail!("Invalid readiness query length"),
        },
        1 => Message::Capability {
            mode: mode(cursor.byte()?)?,
            commands: u64::from_le_bytes(cursor.array()?),
            observation: cursor.observation()?,
        },
        2 => {
            let mode = mode(cursor.byte()?)?;
            let count = usize::from(cursor.byte()?);
            ensure!(count != 0 && count <= MAX_ACTOR, "Invalid actor size");
            let actor = std::str::from_utf8(cursor.take(count)?)?.to_owned();
            ensure!(!actor.chars().any(char::is_control), "Invalid actor");
            let operation = match cursor.byte()? {
                0 => {
                    let count = usize::from(cursor.byte()?);
                    ensure!(count <= MAX_COMMAND, "Invalid command size");
                    let command = std::str::from_utf8(cursor.take(count)?)?.to_owned();
                    ensure!(valid_command(&command), "Invalid command");
                    Operation::Command(command)
                }
                1 => Operation::NetworkUtc(i64::from_le_bytes(cursor.array()?)),
                _ => bail!("Unknown control operation"),
            };
            let (plumbing_origin_utc_ms, launch_revision) = match end - cursor.at {
                0 => (None, None),
                8 if matches!(&operation,Operation::Command(name) if name=="RetractPlumbing") => {
                    let stamp = u64::from_le_bytes(cursor.array()?);
                    ensure!(
                        !matches!(stamp, 0 | u64::MAX),
                        "Invalid plumbing origin timestamp"
                    );
                    (Some(stamp), None)
                }
                8 if matches!(&operation,Operation::Command(name) if launch_command(name)) => {
                    let revision = u64::from_le_bytes(cursor.array()?);
                    ensure!(revision != u64::MAX, "Exhausted launch revision");
                    (None, Some(revision))
                }
                _ => bail!("Unexpected control request extension"),
            };
            Message::Request {
                launch_revision,
                mode,
                actor,
                operation,
                plumbing_origin_utc_ms,
            }
        }
        3 => Message::Reply {
            digest: cursor.array()?,
            outcome: match cursor.byte()? {
                0 => Outcome::Queued,
                1 => Outcome::Denied,
                2 => Outcome::Invalid,
                3 => Outcome::Unavailable,
                4 => Outcome::Stale,
                5 => Outcome::Busy,
                _ => bail!("Unknown result"),
            },
            observation: cursor.observation()?,
        },
        _ => bail!("Unknown control message"),
    };
    ensure!(cursor.at == end, "Trailing control bytes");
    Ok(Envelope {
        issuer: issuer.clone(),
        session,
        sequence,
        request,
        boot,
        message,
    })
}

#[cfg(test)]
#[path = "legacy_plumbing_compat.rs"]
mod legacy;
#[cfg(test)]
mod plumbing_compatibility_tests {
    use super::*;
    fn envelope(message: Message) -> Envelope {
        Envelope {
            issuer: "rrls".into(),
            session: 7,
            sequence: 20,
            request: [1; 16],
            boot: [2; 16],
            message,
        }
    }
    fn old_envelope(message: legacy::Message) -> legacy::Envelope {
        legacy::Envelope {
            issuer: "rrls".into(),
            session: 7,
            sequence: 20,
            request: [1; 16],
            boot: [2; 16],
            message,
        }
    }
    #[test]
    fn unmarked_messages_keep_exact_old_signed_bytes() {
        let (_root, _primary, recovery) = crate::command_proxy::tests::fixtures();
        let pairs = vec![
            (Message::Query, legacy::Message::Query),
            (
                Message::Capability {
                    mode: Mode::Hitl,
                    commands: 7,
                    observation: vec![1, 2],
                },
                legacy::Message::Capability {
                    mode: Mode::Hitl,
                    commands: 7,
                    observation: vec![1, 2],
                },
            ),
            (
                Message::Request {
                    launch_revision: None,
                    mode: Mode::Hitl,
                    actor: "operator".into(),
                    operation: Operation::Command("RetractPlumbing".into()),
                    plumbing_origin_utc_ms: None,
                },
                legacy::Message::Request {
                    mode: Mode::Hitl,
                    actor: "operator".into(),
                    operation: legacy::Operation::Command("RetractPlumbing".into()),
                },
            ),
            (
                Message::Request {
                    launch_revision: None,
                    mode: Mode::Hitl,
                    actor: "operator".into(),
                    operation: Operation::NetworkUtc(1000),
                    plumbing_origin_utc_ms: None,
                },
                legacy::Message::Request {
                    mode: Mode::Hitl,
                    actor: "operator".into(),
                    operation: legacy::Operation::NetworkUtc(1000),
                },
            ),
            (
                Message::Reply {
                    digest: [3; 16],
                    outcome: Outcome::Queued,
                    observation: vec![],
                },
                legacy::Message::Reply {
                    digest: [3; 16],
                    outcome: legacy::Outcome::Queued,
                    observation: vec![],
                },
            ),
        ];
        for (new, old) in pairs {
            let bytes = seal(&recovery, "primary", &envelope(new)).unwrap();
            assert_eq!(
                bytes,
                legacy::seal(&recovery, "primary", &old_envelope(old)).unwrap()
            );
            assert_eq!(contract_digest(&bytes), legacy::contract_digest(&bytes));
        }
    }
    #[test]
    fn marked_retract_binds_original_utc_and_old_decoder_rejects_it() {
        let (_root, primary, recovery) = crate::command_proxy::tests::fixtures();
        let request = envelope(Message::Request {
            launch_revision: None,
            mode: Mode::Hitl,
            actor: "operator".into(),
            operation: Operation::Command("RetractPlumbing".into()),
            plumbing_origin_utc_ms: Some(1_800_000_000_123),
        });
        let bytes = seal(&recovery, "primary", &request).unwrap();
        assert_eq!(open(&primary, &bytes, "rrls").unwrap(), request);
        assert!(
            legacy::open(&primary, &bytes, "rrls")
                .unwrap_err()
                .to_string()
                .contains("Trailing control bytes")
        );
        let mut tampered = bytes.clone();
        let at = tampered.len() - 64 - 8;
        tampered[at] ^= 1;
        assert!(open(&primary, &tampered, "rrls").is_err());
        let unmarked = seal(
            &recovery,
            "primary",
            &envelope(Message::Request {
                launch_revision: None,
                mode: Mode::Hitl,
                actor: "operator".into(),
                operation: Operation::Command("RetractPlumbing".into()),
                plumbing_origin_utc_ms: None,
            }),
        )
        .unwrap();
        assert_eq!(bytes.len(), unmarked.len() + 8);
    }
    #[test]
    fn board_abort_readiness_query_extensions_are_signed_optional_and_legacy_safe() {
        let (_root, primary, recovery) = crate::command_proxy::tests::fixtures();
        let original = seal(&recovery, "primary", &envelope(Message::Query)).unwrap();
        let old =
            legacy::seal(&recovery, "primary", &old_envelope(legacy::Message::Query)).unwrap();
        assert_eq!(original, old);
        for message in [Message::QueryObserve, Message::QueryReadiness] {
            let frame = seal(&recovery, "primary", &envelope(message.clone())).unwrap();
            assert_eq!(frame.len(), original.len() + 1);
            assert_eq!(open(&primary, &frame, "rrls").unwrap().message, message);
            assert!(
                legacy::open(&primary, &frame, "rrls").is_err(),
                "old decoder must refuse extension safely"
            );
            let mut tampered = frame.clone();
            let at = tampered.len() - 65;
            tampered[at] ^= 1;
            assert!(
                open(&primary, &tampered, "rrls").is_err(),
                "flag is covered by actual Ed25519 signature"
            );
        }
        for extra in [&[2u8][..], &[255][..], &[0, 0][..], &[1, 0][..]] {
            let mut frame = original[..original.len() - 64].to_vec();
            frame.extend(extra);
            let signature = recovery
                .mission_signing_key()
                .sign(&signed_bytes(&recovery, "rrls", "primary", &frame));
            frame.extend(signature.as_ref());
            assert!(
                open(&primary, &frame, "rrls").is_err(),
                "malformed signed flag/length cannot request queries"
            );
        }
    }

    #[test]
    fn extensions_on_other_operations_or_wrong_sizes_are_rejected_even_when_signed() {
        let (_root, primary, recovery) = crate::command_proxy::tests::fixtures();
        let signed_extra = |message: Message, extra: &[u8]| {
            let mut wire = seal(&recovery, "primary", &envelope(message)).unwrap();
            wire.truncate(wire.len() - 64);
            wire.extend_from_slice(extra);
            let signature = recovery
                .mission_signing_key()
                .sign(&signed_bytes(&recovery, "rrls", "primary", &wire));
            wire.extend_from_slice(signature.as_ref());
            wire
        };
        for operation in [
            Operation::Command("Abort".into()),
            Operation::NetworkUtc(1000),
        ] {
            assert!(
                seal(
                    &recovery,
                    "primary",
                    &envelope(Message::Request {
                        launch_revision: None,
                        mode: Mode::Hitl,
                        actor: "operator".into(),
                        operation: operation.clone(),
                        plumbing_origin_utc_ms: Some(1000)
                    })
                )
                .is_err()
            );
            let wire = signed_extra(
                Message::Request {
                    launch_revision: None,
                    mode: Mode::Hitl,
                    actor: "operator".into(),
                    operation,
                    plumbing_origin_utc_ms: None,
                },
                &1000u64.to_le_bytes(),
            );
            assert!(open(&primary, &wire, "rrls").is_err());
        }
        for extra in [vec![1; 7], vec![1; 9], vec![0; 8], vec![255; 8]] {
            let wire = signed_extra(
                Message::Request {
                    launch_revision: None,
                    mode: Mode::Hitl,
                    actor: "operator".into(),
                    operation: Operation::Command("RetractPlumbing".into()),
                    plumbing_origin_utc_ms: None,
                },
                &extra,
            );
            assert!(open(&primary, &wire, "rrls").is_err());
        }
    }
}
