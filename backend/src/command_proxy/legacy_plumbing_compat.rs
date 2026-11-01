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
    Capability {
        mode: Mode,
        commands: u64,
        observation: Vec<u8>,
    },
    Request {
        mode: Mode,
        actor: String,
        operation: Operation,
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
        Message::Query => 0,
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
            mode,
            actor,
            operation,
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
        0 => Message::Query,
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
            Message::Request {
                mode,
                actor,
                operation,
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
