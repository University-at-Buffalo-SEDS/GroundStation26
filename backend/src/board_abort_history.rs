//! Standalone candidate: no default roster and no runtime integration.
//!
//! The caller must choose a prospective roster BEFORE admitting observations.
//! A crash restores uncertainty for that entire roster. Persisted masks are not
//! physical-state proof. No proof nonce, board boot, or monotonic clock is stored.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const BOARD_BITS: u8 = 0x7f;
const STATE_SIZE: usize = 112;
const ANCHOR_SIZE: usize = 32;
const STATE_MAGIC: &[u8; 8] = b"BAHIST01";
const ANCHOR_MAGIC: &[u8; 8] = b"BAHANC01";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub revision: u64,
    pub actual_abort: u8,
    pub unknown: u8,
    pub enrolled: u8,
    /// Original observation timestamps only, never a regenerated recovery time.
    pub first_abort_ms: [u64; 7],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restoration {
    pub snapshot: Snapshot,
    pub previous_session_unclean: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    TempCreated,
    PartialWrite,
    Written,
    FileSynced,
    ReadBack,
    Renamed,
    DirectorySynced,
}

/// The adapter must derive these facts from actual successful closure/join
/// results. A shutdown signal or a worker returning after logging a timeout is
/// insufficient. The final snapshot is taken AFTER freezing ingress/admission.
pub struct QuiescenceReceipt {
    final_revision: u64,
}

impl QuiescenceReceipt {
    pub fn checked(
        final_revision: u64,
        ingress_and_admission_frozen: bool,
        producers_joined_successfully: bool,
        owned_writer_drained_successfully: bool,
    ) -> io::Result<Self> {
        if !(ingress_and_admission_frozen
            && producers_joined_successfully
            && owned_writer_drained_successfully)
        {
            return Err(invalid("shutdown did not establish quiescence"));
        }
        Ok(Self { final_revision })
    }
}

#[derive(Clone, Copy)]
struct Record {
    identity: [u8; 16],
    session: u64,
    prospective: u8,
    active: bool,
    snapshot: Snapshot,
}

/// Use exactly one owner/writer. This type intentionally contains no shared
/// mutex, callback registration, or SDK integration. Every failure is returned;
/// caller admission remains closed until bootstrap's durable receipt succeeds.
pub struct Store {
    directory: PathBuf,
    record: Record,
    poisoned: bool,
    _ownership: File,
}
impl Drop for Store {
    fn drop(&mut self) {
        // A concurrent process spawn may briefly inherit the open description
        // before exec closes CLOEXEC descriptors. Explicitly relinquish our
        // ownership instead of depending on the inherited descriptor closing.
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn flock(fd: i32, operation: i32) -> i32;
            }
            let _ = unsafe { flock(self._ownership.as_raw_fd(), 8) }; // LOCK_UN
        }
    }
}

impl Store {
    /// Explicit provisioning only. Never call this as an error fallback from
    /// open(). Existing or incomplete stores are refused rather than erased.
    #[cfg(test)]
    pub fn provision(
        directory: &Path,
        identity: [u8; 16],
        prospective_mask: u8,
    ) -> io::Result<Self> {
        Self::provision_with_cut(directory, identity, prospective_mask, &mut |_| Ok(()))
    }

    pub fn provision_uncertain(directory:&Path, identity:[u8;16], prospective_mask:u8) -> io::Result<Self> {
        Self::provision_initial(directory,identity,prospective_mask,prospective_mask,&mut |_| Ok(()))
    }
    #[cfg(test)]
    fn provision_with_cut(directory:&Path, identity:[u8;16], prospective_mask:u8, cut:&mut dyn FnMut(Cut)->io::Result<()>) -> io::Result<Self> {
        Self::provision_initial(directory,identity,prospective_mask,0,cut)
    }
    fn provision_initial(directory:&Path, identity:[u8;16], prospective_mask:u8, initial_unknown:u8, cut:&mut dyn FnMut(Cut)->io::Result<()>) -> io::Result<Self> {
        valid_identity(identity)?;
        valid_mask(prospective_mask)?;
        // create_dir refuses an existing directory, including an incomplete one.
        fs::create_dir(directory)?;
        sync_directory(
            directory
                .parent()
                .ok_or_else(|| invalid("store has no parent"))?,
        )?;
        let ownership = exclusive_owner(directory)?;
        let mut anchor = Vec::with_capacity(ANCHOR_SIZE);
        anchor.extend_from_slice(ANCHOR_MAGIC);
        anchor.extend_from_slice(&identity);
        append_checksum(&mut anchor);
        // Anchor first: a failure after this cannot masquerade as a fresh store.
        atomic_replace(directory, "anchor", &anchor, cut)?;
        let record = Record {
            identity,
            session: 0,
            prospective: prospective_mask,
            active: false,
            snapshot: Snapshot {
                revision: 0,
                actual_abort: 0,
                unknown: initial_unknown,
                enrolled: 0,
                first_abort_ms: [0; 7],
            },
        };
        atomic_replace(directory, "state", &encode(record), cut)?;
        Ok(Self {
            directory: directory.to_owned(),
            record,
            poisoned: false,
            _ownership: ownership,
        })
    }

    /// Unpaired installations retain a store-local random identity in their
    /// anchor. This validates internal consistency, not machine authentication.
    pub fn anchor_identity(directory: &Path) -> io::Result<[u8; 16]> {
        let anchor = read_exact_file(&directory.join("anchor"), ANCHOR_SIZE)?;
        if &anchor[..8] != ANCHOR_MAGIC || !checksum_valid(&anchor) {
            return Err(invalid("invalid abort-history anchor"));
        }
        let identity = anchor[8..24].try_into().unwrap();
        valid_identity(identity)?;
        Ok(identity)
    }

    pub fn open(directory: &Path, expected_identity: [u8; 16]) -> io::Result<Self> {
        valid_identity(expected_identity)?;
        let ownership = exclusive_owner(directory)?;
        let anchor = read_exact_file(&directory.join("anchor"), ANCHOR_SIZE)?;
        if &anchor[..8] != ANCHOR_MAGIC
            || !checksum_valid(&anchor)
            || anchor[8..24] != expected_identity
        {
            return Err(invalid("invalid or replaced abort-history anchor"));
        }
        let record = decode(&read_exact_file(&directory.join("state"), STATE_SIZE)?)?;
        if record.identity != expected_identity {
            return Err(invalid("abort-history state identity differs from anchor"));
        }
        Ok(Self {
            directory: directory.to_owned(),
            record,
            poisoned: false,
            _ownership: ownership,
        })
    }

    /// Durable startup receipt. Call before any ingress, query, or command
    /// producer. The roster input is explicit; no board policy is selected here.
    pub fn begin_session(&mut self, prospective_mask: u8) -> io::Result<Restoration> {
        self.begin_with_cut(prospective_mask, &mut |_| Ok(()))
    }

    fn begin_with_cut(
        &mut self,
        prospective_mask: u8,
        cut: &mut dyn FnMut(Cut) -> io::Result<()>,
    ) -> io::Result<Restoration> {
        self.ready()?;
        valid_mask(prospective_mask)?;
        // No implicit roster shrinking or silent retirement of unknown boards.
        if self.record.prospective & !prospective_mask != 0 {
            return Err(invalid("prospective roster cannot silently shrink"));
        }
        let unclean = self.record.active;
        let mut next = self.record;
        next.session = next
            .session
            .checked_add(1)
            .ok_or_else(|| invalid("session exhausted"))?;
        next.prospective = prospective_mask;
        if unclean {
            next.snapshot.unknown |= self.record.prospective;
        }
        next.active = true;
        self.commit(next, cut)?;
        Ok(Restoration {
            snapshot: next.snapshot,
            previous_session_unclean: unclean,
        })
    }

    /// Snapshot origin is the RAM register, AFTER its existing correlated proof
    /// checks. This method does not authorize a clear from a heartbeat or history
    /// acknowledgment. A stale/equal revision cannot overwrite a later snapshot.
    pub fn save(&mut self, snapshot: Snapshot) -> io::Result<()> {
        self.save_with_cut(snapshot, &mut |_| Ok(()))
    }

    fn save_with_cut(
        &mut self,
        snapshot: Snapshot,
        cut: &mut dyn FnMut(Cut) -> io::Result<()>,
    ) -> io::Result<()> {
        self.ready()?;
        if !self.record.active {
            return Err(invalid("session is not active"));
        }
        validate_snapshot(snapshot, self.record.prospective)?;
        if snapshot.revision <= self.record.snapshot.revision
            || self.record.snapshot.enrolled & !snapshot.enrolled != 0
        {
            return Err(invalid("stale revision or enrollment erasure"));
        }
        let mut next = self.record;
        next.snapshot = snapshot;
        self.commit(next, cut)
    }

    pub fn seal_clean(&mut self, receipt: QuiescenceReceipt) -> io::Result<()> {
        self.seal_with_cut(receipt, &mut |_| Ok(()))
    }

    fn seal_with_cut(
        &mut self,
        receipt: QuiescenceReceipt,
        cut: &mut dyn FnMut(Cut) -> io::Result<()>,
    ) -> io::Result<()> {
        self.ready()?;
        if !self.record.active || receipt.final_revision != self.record.snapshot.revision {
            return Err(invalid("clean seal does not match drained final snapshot"));
        }
        let mut next = self.record;
        next.active = false;
        self.commit(next, cut)
    }

    fn ready(&self) -> io::Result<()> {
        if self.poisoned {
            Err(invalid("previous persistence failure requires reopen"))
        } else {
            Ok(())
        }
    }

    fn commit(
        &mut self,
        next: Record,
        cut: &mut dyn FnMut(Cut) -> io::Result<()>,
    ) -> io::Result<()> {
        // Poison BEFORE I/O: even an error after rename leaves an uncertain
        // commit outcome. The same instance must never continue admitting work.
        self.poisoned = true;
        atomic_replace(&self.directory, "state", &encode(next), cut)?;
        self.record = next;
        self.poisoned = false;
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn valid_identity(identity: [u8; 16]) -> io::Result<()> {
    if identity == [0; 16] {
        Err(invalid("zero store identity"))
    } else {
        Ok(())
    }
}
fn valid_mask(mask: u8) -> io::Result<()> {
    if mask & !BOARD_BITS != 0 {
        Err(invalid("unknown board bits"))
    } else {
        Ok(())
    }
}
fn validate_snapshot(value: Snapshot, prospective: u8) -> io::Result<()> {
    for mask in [value.actual_abort, value.unknown, value.enrolled] {
        valid_mask(mask)?;
    }
    if (value.actual_abort | value.unknown | value.enrolled) & !prospective != 0
        || value.actual_abort & !value.enrolled != 0
        || value.revision == u64::MAX
    {
        return Err(invalid("invalid snapshot bounds or enrollment"));
    }
    Ok(())
}
fn checksum(bytes: &[u8]) -> u64 {
    // Integrity/error detection only. This does not authenticate a writable disk.
    bytes.iter().fold(0xcbf29ce484222325, |sum, b| {
        (sum ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
fn append_checksum(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&checksum(bytes).to_le_bytes());
}
fn checksum_valid(bytes: &[u8]) -> bool {
    let end = bytes.len() - 8;
    checksum(&bytes[..end]).to_le_bytes() == bytes[end..]
}
fn encode(record: Record) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(STATE_SIZE);
    bytes.extend_from_slice(STATE_MAGIC);
    bytes.extend_from_slice(&record.identity);
    bytes.extend_from_slice(&record.session.to_le_bytes());
    bytes.extend_from_slice(&record.snapshot.revision.to_le_bytes());
    bytes.extend_from_slice(&[
        record.prospective,
        record.snapshot.actual_abort,
        record.snapshot.unknown,
        record.snapshot.enrolled,
        u8::from(record.active),
        0,
        0,
        0,
    ]);
    for timestamp in record.snapshot.first_abort_ms {
        bytes.extend_from_slice(&timestamp.to_le_bytes());
    }
    append_checksum(&mut bytes);
    bytes
}
fn decode(bytes: &[u8]) -> io::Result<Record> {
    if bytes.len() != STATE_SIZE
        || &bytes[..8] != STATE_MAGIC
        || !checksum_valid(bytes)
        || bytes[44] > 1
        || bytes[45..48] != [0; 3]
    {
        return Err(invalid("invalid abort-history record"));
    }
    let record = Record {
        identity: bytes[8..24].try_into().unwrap(),
        session: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        prospective: bytes[40],
        active: bytes[44] != 0,
        snapshot: Snapshot {
            revision: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            actual_abort: bytes[41],
            unknown: bytes[42],
            enrolled: bytes[43],
            first_abort_ms: std::array::from_fn(|i| {
                u64::from_le_bytes(bytes[48 + i * 8..56 + i * 8].try_into().unwrap())
            }),
        },
    };
    valid_identity(record.identity)?;
    valid_mask(record.prospective)?;
    validate_snapshot(record.snapshot, record.prospective)?;
    Ok(record)
}
fn read_exact_file(path: &Path, size: usize) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() != size as u64 {
        return Err(invalid("abort-history file is not a bounded regular file"));
    }
    let mut bytes = Vec::with_capacity(size + 1);
    File::open(path)?
        .take((size + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() != size {
        return Err(invalid("abort-history length changed"));
    }
    Ok(bytes)
}
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
#[cfg(unix)]
fn exclusive_owner(directory: &Path) -> io::Result<File> {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    let path = directory.join("owner.lock");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_file()) {
        return Err(invalid("abort-history lock is not a regular file"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // LOCK_EX | LOCK_NB. The kernel releases ownership on exit or crash.
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
#[cfg(not(unix))]
fn exclusive_owner(_directory: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "store requires reviewed platform locking",
    ))
}
fn atomic_replace(
    directory: &Path,
    name: &str,
    bytes: &[u8],
    cut: &mut dyn FnMut(Cut) -> io::Result<()>,
) -> io::Result<()> {
    let temporary = directory.join(format!(".{name}.pending"));
    // Ownership is held for this Store's lifetime. A previous crashed owner's
    // incomplete temporary file is never authoritative and can be replaced.
    match fs::symlink_metadata(&temporary) {
        Ok(m) if m.file_type().is_file() => fs::remove_file(&temporary)?,
        Ok(_) => return Err(invalid("pending history path is not a regular file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    cut(Cut::TempCreated)?;
    file.write_all(&bytes[..bytes.len() / 2])?;
    cut(Cut::PartialWrite)?;
    file.write_all(&bytes[bytes.len() / 2..])?;
    cut(Cut::Written)?;
    file.sync_all()?;
    cut(Cut::FileSynced)?;
    if read_exact_file(&temporary, bytes.len())? != bytes {
        return Err(invalid("write readback mismatch"));
    }
    cut(Cut::ReadBack)?;
    fs::rename(&temporary, directory.join(name))?;
    cut(Cut::Renamed)?;
    sync_directory(directory)?;
    cut(Cut::DirectorySynced)?;
    Ok(())
}

#[cfg(test)]
#[path = "board_abort_history_tests.rs"]
mod tests;
