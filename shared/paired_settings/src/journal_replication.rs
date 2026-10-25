//! Incremental, signed replication of durable paired document journals.
//!
//! Each peer has one cursor and at most one64KiB document assembly. Initial
//! reconciliation pages cached key/digest entries. Later reconciliation follows
//! per-row revisions; a checkbox never rebuilds or transmits the full catalog.
//! The caller's typed validator precedes durable merge. Runtime activation is a
//! separate operation and never follows merely from receiving a valid document.
use crate::{
    Document,
    auth::MAX_WIRE,
    journal::{Journal, ScanCursor},
    radio::Attempt,
    transport,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{
    digest::{SHA256, digest},
    signature::{self, Ed25519KeyPair},
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
const DOMAIN: &[u8] = b"gs-mission-journal-v1\0";
const MAGIC: &[u8] = b"MS\x02";
const MAX_BODY: usize = 2048;
const HEADER: usize = 5;
const SIGNATURE: usize = 64;
const CHUNK: usize = 128;
const MAX_CHUNKS: usize = 512;
const RETRY_MS: u64 = 5000;
const HELLO_MS: u64 = 5000;
const MAX_CONTROL: usize = 8;
const PROFILE: u8 = 1;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    key: String,
    hash: String,
    /// The scan's cursor revision stays fixed even if a row changes during its
    /// transfer. Advancing to a newer row revision could skip another key.
    cursor_revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Hello {
        profile: u8,
        incarnation: String,
        revision: i64,
        max_documents: u16,
        max_bytes: u32,
    },
    Scan {
        incarnation: String,
        request: u64,
        cursor: ScanCursor,
    },
    Entry {
        incarnation: String,
        request: u64,
        cursor: ScanCursor,
        fence: i64,
        entry: Option<Entry>,
    },
    Read {
        incarnation: String,
        request: u64,
        cursor: ScanCursor,
        fence: i64,
        entry: Entry,
        missing: Option<String>,
    },
    Chunk {
        incarnation: String,
        request: u64,
        key: String,
        hash: String,
        index: u16,
        total: u16,
        bytes: String,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    installation: String,
    issuer: String,
    recipient: String,
    message: Message,
}
fn signed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DOMAIN.len() + bytes.len());
    out.extend_from_slice(DOMAIN);
    out.extend_from_slice(bytes);
    out
}
fn seal(
    key: &Ed25519KeyPair,
    installation: &str,
    issuer: &str,
    recipient: &str,
    message: Message,
) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(&Envelope {
        installation: installation.into(),
        issuer: issuer.into(),
        recipient: recipient.into(),
        message,
    })?;
    anyhow::ensure!(
        body.len() <= MAX_BODY,
        "journal frame exceeds signing budget"
    );
    let mut wire = Vec::with_capacity(body.len() + HEADER + SIGNATURE);
    wire.extend_from_slice(MAGIC);
    wire.extend_from_slice(&(body.len() as u16).to_le_bytes());
    wire.extend_from_slice(&body);
    wire.extend_from_slice(key.sign(&signed(&wire)).as_ref());
    Ok(wire)
}
fn open(
    wire: &[u8],
    installation: &str,
    recipient: &str,
    issuers: &[String],
    key: &[u8],
) -> anyhow::Result<Envelope> {
    anyhow::ensure!(
        (HEADER + SIGNATURE..=MAX_WIRE).contains(&wire.len())
            && wire.starts_with(MAGIC)
            && key.len() == 32,
        "unsupported journal envelope bounds/version"
    );
    let end = HEADER + usize::from(u16::from_le_bytes([wire[3], wire[4]]));
    anyhow::ensure!(
        end <= HEADER + MAX_BODY && end + SIGNATURE == wire.len(),
        "invalid journal envelope length"
    );
    signature::UnparsedPublicKey::new(&signature::ED25519, key)
        .verify(&signed(&wire[..end]), &wire[end..])
        .map_err(|_| anyhow::anyhow!("invalid journal signature"))?;
    let envelope: Envelope = serde_json::from_slice(&wire[HEADER..end])?;
    anyhow::ensure!(
        envelope.installation == installation
            && envelope.recipient == recipient
            && issuers.contains(&envelope.issuer),
        "unpaired journal identity"
    );
    Ok(envelope)
}
fn valid_hash(value: &str) -> bool {
    value.len() == 43 && B64.decode(value).is_ok_and(|bytes| bytes.len() == 32)
}
fn valid_entry(entry: &Entry) -> bool {
    !entry.key.is_empty()
        && entry.key.len() <= 128
        && entry.cursor_revision >= 0
        && valid_hash(&entry.hash)
}
struct Transfer {
    incarnation: String,
    request: u64,
    key: String,
    hash: String,
    bytes: Vec<u8>,
    remaining: VecDeque<usize>,
}
struct Assembly {
    parts: Vec<Option<Vec<u8>>>,
    last_progress: u64,
}
#[derive(Clone)]
struct Request {
    id: u64,
    cursor: ScanCursor,
    last_sent: Option<u64>,
}
struct Pending {
    request: Request,
    fence: i64,
    entry: Entry,
    assembly: Option<Assembly>,
    last_read: Option<u64>,
}
struct Remote {
    incarnation: String,
    advertised: i64,
    baseline: i64,
    cursor: ScanCursor,
    request: Option<Request>,
    pending: Option<Pending>,
}
impl Remote {
    fn advance(&mut self, entry: &Entry) {
        self.cursor = match &self.cursor {
            ScanCursor::Full { .. } => ScanCursor::Full {
                after: entry.key.clone(),
            },
            ScanCursor::Changes { .. } => ScanCursor::Changes {
                after: entry.cursor_revision,
            },
        };
        self.pending = None;
        self.request = None;
    }
}
#[derive(Default)]
struct Protocol {
    remote: Option<Remote>,
    outgoing: VecDeque<Message>,
    transfer: Option<Transfer>,
    last_hello: Option<u64>,
    request_id: u64,
}
impl Protocol {
    fn enqueue(&mut self, message: Message) -> anyhow::Result<()> {
        if self.outgoing.contains(&message) {
            return Ok(());
        }
        anyhow::ensure!(
            self.outgoing.len() < MAX_CONTROL,
            "journal control budget exhausted"
        );
        self.outgoing.push_back(message);
        Ok(())
    }
    async fn poll(&mut self, journal: &Journal, now: u64) -> anyhow::Result<Option<Message>> {
        if let Some(message) = self.outgoing.pop_front() {
            return Ok(Some(message));
        }
        if self
            .last_hello
            .is_none_or(|time| now.saturating_sub(time) >= HELLO_MS)
        {
            self.last_hello = Some(now);
            let metadata = journal.metadata().await?;
            let limits = journal.limits();
            return Ok(Some(Message::Hello {
                profile: PROFILE,
                incarnation: metadata.incarnation,
                revision: metadata.revision,
                max_documents: limits.documents as u16,
                max_bytes: limits.encoded_bytes as u32,
            }));
        }
        if let Some(transfer) = self.transfer.as_mut() {
            let index = transfer.remaining.pop_front().expect("nonempty transfer");
            let total = transfer.bytes.len().div_ceil(CHUNK);
            let message = Message::Chunk {
                incarnation: transfer.incarnation.clone(),
                request: transfer.request,
                key: transfer.key.clone(),
                hash: transfer.hash.clone(),
                index: index as u16,
                total: total as u16,
                bytes: B64.encode(
                    &transfer.bytes[index * CHUNK..((index + 1) * CHUNK).min(transfer.bytes.len())],
                ),
            };
            if transfer.remaining.is_empty() {
                self.transfer = None;
            }
            return Ok(Some(message));
        }
        let Some(remote) = self.remote.as_mut() else {
            return Ok(None);
        };
        if let Some(pending) = remote.pending.as_mut() {
            if pending
                .last_read
                .is_none_or(|time| now.saturating_sub(time) >= RETRY_MS)
            {
                pending.last_read = Some(now);
                return Ok(Some(Message::Read {
                    incarnation: remote.incarnation.clone(),
                    request: pending.request.id,
                    cursor: pending.request.cursor.clone(),
                    fence: pending.fence,
                    entry: pending.entry.clone(),
                    missing: pending.assembly.as_ref().map(|assembly| {
                        let mut mask = vec![0u8; assembly.parts.len().div_ceil(8)];
                        for (index, part) in assembly.parts.iter().enumerate() {
                            if part.is_none() {
                                mask[index / 8] |= 1 << (index % 8);
                            }
                        }
                        B64.encode(mask)
                    }),
                }));
            }
            return Ok(None);
        }
        if matches!(remote.cursor,ScanCursor::Changes {after} if after>=remote.advertised) {
            return Ok(None);
        }
        if remote.request.is_none() {
            self.request_id = self
                .request_id
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("journal request counter exhausted"))?;
            remote.request = Some(Request {
                id: self.request_id,
                cursor: remote.cursor.clone(),
                last_sent: None,
            });
        }
        let request = remote.request.as_mut().unwrap();
        if request
            .last_sent
            .is_some_and(|time| now.saturating_sub(time) < RETRY_MS)
        {
            return Ok(None);
        }
        request.last_sent = Some(now);
        Ok(Some(Message::Scan {
            incarnation: remote.incarnation.clone(),
            request: request.id,
            cursor: request.cursor.clone(),
        }))
    }
    async fn receive(
        &mut self,
        journal: &Journal,
        message: Message,
        now: u64,
        validate: impl Fn(&str, &Document) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        match message {
            Message::Hello {
                profile,
                incarnation,
                revision,
                max_documents,
                max_bytes,
            } => {
                let limits = journal.limits();
                anyhow::ensure!(
                    profile == PROFILE
                        && valid_hash(&incarnation)
                        && revision >= 0
                        && usize::from(max_documents) == limits.documents
                        && max_bytes as usize == limits.encoded_bytes,
                    "unsupported journal profile/capacity"
                );
                if self
                    .remote
                    .as_ref()
                    .is_none_or(|remote| remote.incarnation != incarnation)
                {
                    self.remote = Some(Remote {
                        incarnation,
                        advertised: revision,
                        baseline: revision,
                        cursor: ScanCursor::Full {
                            after: String::new(),
                        },
                        request: None,
                        pending: None,
                    });
                } else {
                    let remote = self.remote.as_mut().unwrap();
                    // Signed old Hello frames can arrive after a newer one.
                    // They cannot move a durable cursor backwards.
                    remote.advertised = remote.advertised.max(revision);
                }
            }
            Message::Scan {
                incarnation,
                request,
                cursor,
            } => {
                let page = journal.scan(&cursor).await?;
                if page.metadata.incarnation != incarnation {
                    return Ok(false);
                }
                let entry = page.entry.map(|entry| Entry {
                    key: entry.key,
                    hash: entry.hash,
                    cursor_revision: entry.revision,
                });
                self.enqueue(Message::Entry {
                    incarnation,
                    request,
                    cursor,
                    fence: page.metadata.revision,
                    entry,
                })?;
            }
            Message::Entry {
                incarnation,
                request,
                cursor,
                fence,
                entry,
            } => {
                let Some(remote) = self.remote.as_mut() else {
                    return Ok(false);
                };
                if remote.incarnation != incarnation {
                    return Ok(false);
                }
                let expected = remote
                    .request
                    .as_ref()
                    .or_else(|| remote.pending.as_ref().map(|pending| &pending.request));
                if expected
                    .is_none_or(|expected| expected.id != request || expected.cursor != cursor)
                {
                    return Ok(false);
                }
                anyhow::ensure!(fence >= 0, "invalid journal catalog fence");
                if let Some(entry) = entry {
                    anyhow::ensure!(
                        valid_entry(&entry)
                            && entry.cursor_revision <= fence
                            && match &cursor {
                                ScanCursor::Full { after } => entry.key > *after,
                                ScanCursor::Changes { after } => entry.cursor_revision > *after,
                            },
                        "invalid journal catalog cursor"
                    );
                    if journal.digest(&entry.key).await?.as_deref() == Some(&entry.hash) {
                        remote.advance(&entry);
                    } else {
                        let expected = expected.unwrap().clone();
                        let retain = remote
                            .pending
                            .as_ref()
                            .is_some_and(|pending| pending.entry == entry);
                        if !retain {
                            remote.pending = Some(Pending {
                                request: expected,
                                fence,
                                entry,
                                assembly: None,
                                last_read: None,
                            });
                            remote.request = None;
                        }
                    }
                } else {
                    remote.cursor = match cursor {
                        ScanCursor::Full { .. } => ScanCursor::Changes {
                            after: remote.baseline,
                        },
                        ScanCursor::Changes { .. } => ScanCursor::Changes { after: fence },
                    };
                    remote.advertised = remote.advertised.max(fence);
                    remote.pending = None;
                    remote.request = None;
                }
            }
            Message::Read {
                incarnation,
                request,
                cursor,
                fence,
                entry,
                missing,
            } => {
                anyhow::ensure!(valid_entry(&entry), "invalid journal document request");
                let metadata = journal.metadata().await?;
                if metadata.incarnation != incarnation {
                    return Ok(false);
                }
                if self.transfer.as_ref().is_some_and(|transfer| {
                    transfer.request == request
                        && transfer.key == entry.key
                        && transfer.hash == entry.hash
                }) {
                    return Ok(false);
                }
                let document = journal
                    .document(&entry.key)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("requested journal document disappeared"))?;
                let bytes = transport::document_bytes(&document);
                let hash = transport::document_hash(&document);
                if hash != entry.hash {
                    // Preserve the earlier scan cursor: another key may have
                    // changed between it and this row's newer revision.
                    self.enqueue(Message::Entry {
                        incarnation,
                        request,
                        cursor,
                        fence,
                        entry: Some(Entry { hash, ..entry }),
                    })?;
                } else {
                    anyhow::ensure!(
                        bytes.len() <= CHUNK * MAX_CHUNKS,
                        "journal transfer exceeds quota"
                    );
                    let total = bytes.len().div_ceil(CHUNK);
                    let remaining = if let Some(missing) = missing {
                        let mask = B64.decode(missing)?;
                        anyhow::ensure!(
                            mask.len() == total.div_ceil(8)
                                && (total..mask.len() * 8)
                                    .all(|index| mask[index / 8] & (1 << (index % 8)) == 0),
                            "invalid journal missing-chunk mask"
                        );
                        (0..total)
                            .filter(|index| mask[index / 8] & (1 << (index % 8)) != 0)
                            .collect::<VecDeque<_>>()
                    } else {
                        (0..total).collect()
                    };
                    if !remaining.is_empty() {
                        self.transfer = Some(Transfer {
                            incarnation,
                            request,
                            key: entry.key,
                            hash,
                            bytes,
                            remaining,
                        });
                    }
                }
            }
            Message::Chunk {
                incarnation,
                request,
                key,
                hash,
                index,
                total,
                bytes,
            } => {
                let Some(remote) = self.remote.as_mut() else {
                    return Ok(false);
                };
                if remote.incarnation != incarnation {
                    return Ok(false);
                }
                let Some(pending) = remote.pending.as_mut() else {
                    return Ok(false);
                };
                if pending.request.id != request
                    || pending.entry.key != key
                    || pending.entry.hash != hash
                {
                    return Ok(false);
                }
                let total = usize::from(total);
                let index = usize::from(index);
                anyhow::ensure!(
                    (1..=MAX_CHUNKS).contains(&total) && index < total,
                    "invalid journal chunk bounds"
                );
                let bytes = B64.decode(bytes)?;
                anyhow::ensure!(
                    !bytes.is_empty()
                        && bytes.len() <= CHUNK
                        && (index + 1 == total || bytes.len() == CHUNK),
                    "invalid journal chunk size"
                );
                if pending
                    .assembly
                    .as_ref()
                    .is_none_or(|assembly| now.saturating_sub(assembly.last_progress) > 60_000)
                {
                    pending.assembly = Some(Assembly {
                        parts: vec![None; total],
                        last_progress: now,
                    });
                }
                let assembly = pending.assembly.as_mut().unwrap();
                anyhow::ensure!(
                    assembly.parts.len() == total,
                    "journal transfer chunk count changed"
                );
                if let Some(old) = &assembly.parts[index] {
                    anyhow::ensure!(*old == bytes, "journal chunk equivocation");
                } else {
                    assembly.parts[index] = Some(bytes);
                    assembly.last_progress = now;
                }
                if assembly.parts.iter().all(Option::is_some) {
                    let joined = assembly
                        .parts
                        .iter()
                        .flat_map(|part| part.as_ref().unwrap().iter().copied())
                        .collect::<Vec<_>>();
                    anyhow::ensure!(
                        B64.encode(digest(&SHA256, &joined).as_ref()) == pending.entry.hash,
                        "journal document digest mismatch"
                    );
                    let document: Document = serde_json::from_slice(&joined)?;
                    let changed = journal.merge(&key, &document, validate).await?.is_some();
                    let entry = pending.entry.clone();
                    remote.advance(&entry);
                    return Ok(changed);
                }
            }
        }
        Ok(false)
    }
}
struct Peer {
    id: String,
    protocol: Protocol,
    pending: Option<Vec<u8>>,
}
pub struct JournalRadio {
    installation: String,
    station: String,
    peers: Vec<Peer>,
    cursor: usize,
    next_at: Option<u64>,
    exhausted: bool,
}
impl JournalRadio {
    pub fn new(installation: String, station: String, peers: Vec<String>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            crate::identity(&installation)
                && crate::identity(&station)
                && !peers.is_empty()
                && peers.len() <= 8
                && peers
                    .iter()
                    .all(|peer| crate::identity(peer) && peer != &station)
                && peers
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == peers.len(),
            "invalid journal radio identity"
        );
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
            exhausted: false,
        })
    }
    fn validate_journal(&self, journal: &Journal) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.installation == journal.installation()
                && journal.peers().contains(&self.station)
                && self
                    .peers
                    .iter()
                    .all(|peer| journal.peers().contains(&peer.id)),
            "journal radio context does not match persisted installation"
        );
        Ok(())
    }
    pub fn recognizes(wire: &[u8]) -> bool {
        wire.starts_with(MAGIC)
    }
    /// One shared256B/s actual signed-wire allowance across all paired peers.
    /// This replaces, rather than runs alongside, the legacy radio allowance.
    pub async fn poll(
        &mut self,
        journal: &Journal,
        now: u64,
        signer: &Ed25519KeyPair,
        mut send: impl FnMut(&str, &[u8]) -> bool,
    ) -> anyhow::Result<Option<Attempt>> {
        self.validate_journal(journal)?;
        if self.exhausted || self.next_at.is_some_and(|at| now < at) {
            return Ok(None);
        }
        for _ in 0..self.peers.len() {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.peers.len();
            let peer = &mut self.peers[index];
            if peer.pending.is_none()
                && let Some(message) = peer.protocol.poll(journal, now).await?
            {
                peer.pending = Some(seal(
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
            let accepted = send(&peer.id, wire);
            let delay = (bytes as u64 * 1000).div_ceil(crate::radio::BYTES_PER_SECOND);
            self.next_at = now.checked_add(delay);
            self.exhausted = self.next_at.is_none();
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
    pub async fn receive(
        &mut self,
        journal: &Journal,
        wire: &[u8],
        now: u64,
        key: &[u8],
        validate: impl Fn(&str, &Document) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        self.validate_journal(journal)?;
        let issuers = self
            .peers
            .iter()
            .map(|peer| peer.id.clone())
            .collect::<Vec<_>>();
        let envelope = open(wire, &self.installation, &self.station, &issuers, key)?;
        self.peers
            .iter_mut()
            .find(|peer| peer.id == envelope.issuer)
            .unwrap()
            .protocol
            .receive(journal, envelope.message, now, validate)
            .await
    }
}

#[cfg(test)]
mod tests;
