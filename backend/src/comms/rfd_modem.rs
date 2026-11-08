//! Check the attached RFD900x ASYNC modem against the selected RFD profile.
//!
//! Runs once when the AV-bay radio is first opened, and again only on an
//! explicit operator request. It never runs in a flight state or while a
//! launch or sequence is active, and it never loops.
//!
//! Behaviour follows RFDesign's "RFD900x Asynchronous User Manual" V1.1
//! (files.rfdesign.com.au/Files/documents/RFD900x Asynchronous User Manual V1.1.pdf):
//! * command mode is `+++` after at least one second without host data (p. 5);
//!   while in command mode the modem stops passing received air data (p. 5);
//! * a change is `ATSn=X`, then `AT&W`, then `ATZ` to reboot (p. 5);
//! * `ATI5` lists every user-settable EEPROM parameter (p. 5);
//! * RT (remote) commands are not supported (pp. 4-5), so each station can
//!   check only its own modem;
//! * S2 (air rate), S9 (net ID), S14-S17 and S19 must match on every node
//!   (Table 3-2). Changing them on one node alone drops the link, so they are
//!   reported, never written.
//!
//! The 4.03 register map is unpublished. A register is written only when
//! `ATI5` names it as expected, or when the identity and readback values
//! fingerprint the V1.1 map. EEPROM is written only when a value differs,
//! at most once per check and a bounded number of times per process.
use crate::comms_config::{CommsLinkConfig, SerialLinkConfig, SerialProtocol};
use serde::Serialize;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Byte transport to one modem. `read` returns `Ok(0)` after a short poll
/// interval with nothing received. `sleep` and `now` let tests use a virtual clock.
pub(crate) trait ModemIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn now(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// Host silence before `+++` (manual p. 5: "at least one second").
    pub guard: Duration,
    /// Wait for `OK` after `+++`. SiK-derived firmware answers after its own
    /// one-second post-guard.
    pub plus_reply: Duration,
    pub command: Duration,
    /// Quiet gap that ends an answer without a trailing `OK` (ATI, ATI5, ATSn?).
    pub idle: Duration,
    pub save: Duration,
    /// Reboot time after `ATZ`. Undocumented; generous on purpose.
    pub reboot: Duration,
    /// Hard bound on one whole check, including a write and its verification.
    pub overall: Duration,
}

pub(crate) const TIMING: Timing = Timing {
    guard: Duration::from_millis(1100),
    plus_reply: Duration::from_millis(2500),
    command: Duration::from_millis(1000),
    idle: Duration::from_millis(150),
    save: Duration::from_millis(2000),
    reboot: Duration::from_millis(4000),
    overall: Duration::from_secs(40),
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Skip,
    Verify,
    Fix,
}

impl Mode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "skip" | "off" => Some(Self::Skip),
            "verify" | "read" => Some(Self::Verify),
            "fix" | "on" => Some(Self::Fix),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Policy {
    /// Written when it differs.
    Fix,
    /// Must match the other nodes; differences are surfaced, never written.
    ReportOnly,
    /// Recorded for the operator; no expectation.
    Info,
}

#[derive(Clone, Debug)]
pub(crate) struct Expect {
    pub number: u8,
    pub names: &'static [&'static str],
    pub value: Option<i64>,
    pub policy: Policy,
}

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub regs: Vec<Expect>,
}

/// Values the plan needs from the selected profile and the local link.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProfileValues {
    pub serial_baud: u64,
    pub air_bps: u64,
    pub max_data_bytes: u64,
    pub max_retries: u64,
    pub broadcast_retries: u64,
    pub serial_break_ms: u64,
    pub tx_encapsulation: u64,
    pub rx_encapsulation: u64,
    pub hardware_flow_control: bool,
    /// The operator marked this profile as set on GS, RRLS and RF. Only then
    /// may link-wide registers (manual Table 3-2 "must be the same") be written.
    pub rolled_out_all_nodes: bool,
}

const BROADCAST_DESTID: i64 = 32768;

impl Plan {
    pub(crate) fn new(p: ProfileValues) -> Self {
        let n = |v: u64| i64::try_from(v).ok();
        let fix = |number, names, value| Expect {
            number,
            names,
            value,
            policy: Policy::Fix,
        };
        let report = |number, names, value| Expect {
            number,
            names,
            value,
            policy: Policy::ReportOnly,
        };
        let info = |number, names| Expect {
            number,
            names,
            value: None,
            policy: Policy::Info,
        };
        // Must match on every node (Table 3-2). Writing one node alone would
        // create the mismatch the manual forbids, so these are written only
        // for a profile marked as rolled out to all nodes.
        let link = |number, names, value| Expect {
            number,
            names,
            value,
            policy: if p.rolled_out_all_nodes {
                Policy::Fix
            } else {
                Policy::ReportOnly
            },
        };
        Self {
            regs: vec![
                // Changing S1 would cut this host off; if we can talk, it matches.
                // S2 and S17 are link-critical and never written by one node.
                report(1, &["SERIAL_SPEED"], n(p.serial_baud / 1000)),
                report(2, &["AIR_SPEED"], n(p.air_bps / 1000)),
                link(3, &["MAX_DATA"], n(p.max_data_bytes)),
                link(4, &["MAX_RETRIES"], n(p.max_retries)),
                link(5, &["GLOBAL_RETRIES"], n(p.broadcast_retries)),
                fix(
                    6,
                    &["SER_BREAK_DETECT_MS", "SER_BREAK_DETECT", "SERIAL_BREAK", "SER_BRK_DETMS"],
                    n(p.serial_break_ms),
                ),
                link(7, &["TXENCAP", "TX_ENCAP", "TX_ENCAP_METHOD"], n(p.tx_encapsulation)),
                link(8, &["RXENCAP", "RX_ENCAP", "RX_ENCAP_METHOD"], n(p.rx_encapsulation)),
                info(9, &["NETID", "NET_ID"]),
                info(10, &["NODEID", "NODE_ID"]),
                fix(11, &["DESTID", "DEST_ID"], Some(BROADCAST_DESTID)),
                info(12, &["TXPOWER", "TX_POWER"]),
                fix(13, &["MAVLINK1", "MAVLINK"], Some(0)),
                info(14, &["MIN_FREQ"]),
                info(15, &["MAX_FREQ"]),
                info(16, &["NUM_CHANNELS"]),
                // Registers are matched by the name the modem reports (ATI5); the
                // number is only the documented fallback. ASYNC 4.03 on RFD900x2
                // inserts DUTY_CYCLE at S17, shifting LBT_RSSI to S18 and RTSCTS to S19.
                // "This parameter shouldn't be changed" and must match (Table 3-2).
                report(17, &["LBT_RSSI"], Some(0)),
                info(17, &["DUTY_CYCLE"]),
                // Local. Must agree with the host's flow control, or the
                // modem can hold its serial output on an unwired CTS line.
                fix(18, &["RTSCTS"], Some(i64::from(p.hardware_flow_control))),
                info(19, &["ENCRYPTION_LEVEL", "ENCRYPTION"]),
                info(22, &["ANTMODE", "ANT_MODE", "RESV"]),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Skipped,
    Matched,
    Applied,
    /// Differences remain: verify-only mode, report-only registers, or writes blocked.
    Mismatch,
    ApplyFailed,
    VerifyFailed,
    NoResponse,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Trigger {
    Startup,
    Operator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataMode {
    /// Command mode was never entered.
    Untouched,
    /// `ATO` accepted.
    Resumed,
    /// Restored by `ATZ`.
    Rebooted,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MapSource {
    Ati5Names,
    Fingerprint,
    Unconfirmed,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct RegisterRow {
    pub number: u8,
    pub name: Option<String>,
    pub observed: Option<i64>,
    pub expected: Option<i64>,
    pub policy: Policy,
    /// match, differs, unread, info or name_mismatch
    pub status: &'static str,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct WriteRow {
    pub number: u8,
    pub from: Option<i64>,
    pub to: i64,
    pub accepted: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Report {
    pub outcome: Outcome,
    pub summary: String,
    pub trigger: Trigger,
    pub mode: Mode,
    pub identity: Option<String>,
    pub map_source: MapSource,
    pub registers: Vec<RegisterRow>,
    pub writes: Vec<WriteRow>,
    pub eeprom_saved: bool,
    /// AT&W was sent, acknowledged or not. Counts against the save budget.
    pub save_attempted: bool,
    pub rebooted: bool,
    pub data_mode: DataMode,
    pub forwarding: Option<String>,
    pub warnings: Vec<String>,
    pub finished_unix_ms: u64,
    pub duration_ms: u64,
}

impl Report {
    fn new(trigger: Trigger, mode: Mode) -> Self {
        Self {
            outcome: Outcome::Failed,
            summary: String::new(),
            trigger,
            mode,
            identity: None,
            map_source: MapSource::Unconfirmed,
            registers: Vec::new(),
            writes: Vec::new(),
            eeprom_saved: false,
            save_attempted: false,
            rebooted: false,
            data_mode: DataMode::Untouched,
            forwarding: None,
            warnings: Vec::new(),
            finished_unix_ms: 0,
            duration_ms: 0,
        }
    }
    pub(crate) fn skipped(trigger: Trigger, mode: Mode, reason: impl Into<String>) -> Self {
        let mut report = Self::new(trigger, mode);
        report.outcome = Outcome::Skipped;
        report.summary = reason.into();
        report.finished_unix_ms = unix_ms();
        report
    }
    pub(crate) fn failed(trigger: Trigger, mode: Mode, reason: impl Into<String>) -> Self {
        let mut report = Self::skipped(trigger, mode, reason);
        report.outcome = Outcome::Failed;
        report
    }
    /// Anything other than a clean match or a successful fix needs the operator.
    pub(crate) fn needs_attention(&self) -> bool {
        !matches!(self.outcome, Outcome::Matched | Outcome::Applied) || !self.warnings.is_empty()
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

struct Driver<'a> {
    io: &'a mut dyn ModemIo,
    t: Timing,
    deadline: Duration,
    rx: Vec<u8>,
    saw_plus_ok: bool,
}

#[derive(Debug, Default)]
struct Answer {
    lines: Vec<String>,
    ok: bool,
    error: bool,
}

enum Abort {
    Io(io::Error),
    Deadline,
}

impl From<io::Error> for Abort {
    fn from(error: io::Error) -> Self {
        Abort::Io(error)
    }
}

const RX_LIMIT: usize = 16 * 1024;

impl Driver<'_> {
    fn left(&self) -> Result<Duration, Abort> {
        let now = self.io.now();
        if now >= self.deadline {
            return Err(Abort::Deadline);
        }
        Ok(self.deadline - now)
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Abort> {
        let duration = duration.min(self.left()?);
        self.io.sleep(duration);
        Ok(())
    }
    /// Retries partial writes; a write that makes no progress is an error.
    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), Abort> {
        let mut stalls = 0;
        while !bytes.is_empty() {
            self.left()?;
            match self.io.write(bytes) {
                Ok(0) => {
                    stalls += 1;
                    if stalls > 50 {
                        return Err(Abort::Io(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "modem accepted no bytes",
                        )));
                    }
                    self.io.sleep(Duration::from_millis(10));
                }
                Ok(n) => {
                    stalls = 0;
                    bytes = &bytes[n.min(bytes.len())..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.io.sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    /// Reads whatever arrives within `window`, bounded in size.
    fn pump(&mut self) -> Result<usize, Abort> {
        let mut buf = [0u8; 256];
        let n = match self.io.read(&mut buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                0
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => 0,
            Err(e) => return Err(e.into()),
        };
        self.rx.extend_from_slice(&buf[..n]);
        if self.rx.len() > RX_LIMIT {
            let excess = self.rx.len() - RX_LIMIT;
            self.rx.drain(..excess);
        }
        Ok(n)
    }
    /// Discards pending input. Air data may stream continuously, so the
    /// drain is time-bounded rather than waiting for silence.
    fn drain(&mut self, window: Duration) -> Result<(), Abort> {
        let start = self.io.now();
        let mut last_byte = start;
        loop {
            self.left()?;
            if self.pump()? > 0 {
                last_byte = self.io.now();
            }
            let now = self.io.now();
            if now.saturating_sub(start) >= window
                || now.saturating_sub(last_byte) >= Duration::from_millis(100)
            {
                break;
            }
        }
        self.rx.clear();
        Ok(())
    }
    /// Waits for the `+++` answer. Air data may itself contain "OK", so an
    /// OK counts only once a guard time has passed since `+++` and the line
    /// has then gone quiet: in command mode the modem stops passing air data.
    fn wait_for_plus_ok(&mut self) -> Result<bool, Abort> {
        let start = self.io.now();
        let settle = self.t.guard + Duration::from_millis(100);
        let quiet = Duration::from_millis(200);
        let mut seen = false;
        let mut last_byte = start;
        loop {
            self.left()?;
            if self.pump()? > 0 {
                last_byte = self.io.now();
            }
            if contains_ok_line(&self.rx) {
                seen = true;
                self.rx.clear();
            }
            let now = self.io.now();
            if seen && now.saturating_sub(start) >= settle && now.saturating_sub(last_byte) >= quiet
            {
                self.rx.clear();
                return Ok(true);
            }
            if now.saturating_sub(start) >= self.t.plus_reply {
                self.rx.clear();
                return Ok(false);
            }
        }
    }
    /// Sends one AT command and collects its answer. An answer ends on an
    /// `OK`/`ERROR` line, on a quiet gap after at least one line, or on timeout.
    fn command(&mut self, cmd: &str, timeout: Duration) -> Result<Answer, Abort> {
        self.rx.clear();
        self.write_all(cmd.as_bytes())?;
        self.write_all(b"\r")?;
        let start = self.io.now();
        let mut last_data = start;
        loop {
            let n = self.pump()?;
            let now = self.io.now();
            if n > 0 {
                last_data = now;
            }
            let answer = parse_answer(&self.rx, cmd);
            if answer.ok || answer.error {
                self.rx.clear();
                return Ok(answer);
            }
            let quiet = now.saturating_sub(last_data) >= self.t.idle;
            if (quiet && !answer.lines.is_empty()) || now.saturating_sub(start) >= timeout {
                self.rx.clear();
                return Ok(answer);
            }
            self.left()?;
        }
    }
    fn enter_command_mode(&mut self) -> Result<bool, Abort> {
        for attempt in 0..2 {
            self.sleep(self.t.guard)?;
            self.drain(Duration::from_millis(200))?;
            // Release notes 2.69: three '+' in quick succession are accepted.
            self.write_all(b"+++")?;
            if self.wait_for_plus_ok()? {
                self.saw_plus_ok = true;
                // Confirm: air data can contain "OK". In command mode the
                // modem no longer passes air data, so this answer is clean.
                if self.command("AT", self.t.command)?.ok {
                    return Ok(true);
                }
            }
            if attempt == 0 {
                // The modem may already be in command mode (for example
                // after an interrupted check), where "+++" is just text.
                self.write_all(b"\r")?;
                self.drain(Duration::from_millis(200))?;
                if self.command("AT", self.t.command)?.ok {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    fn read_value(&mut self, number: u8) -> Result<Option<i64>, Abort> {
        let answer = self.command(&format!("ATS{number}?"), self.t.command)?;
        if answer.error {
            return Ok(None);
        }
        Ok(answer.lines.iter().find_map(|l| parse_int(l)))
    }
    /// `ATO` usually prints nothing; only an explicit `ERROR` means it failed.
    /// The fallback is `ATZ`, which boots the modem into data mode.
    fn exit_command_mode(&mut self, report: &mut Report) {
        for _ in 0..2 {
            match self.command("ATO", Duration::from_millis(300)) {
                Ok(answer) if !answer.error => {
                    report.data_mode = DataMode::Resumed;
                    return;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        report
            .warnings
            .push("modem did not accept ATO; rebooting it into data mode".into());
        if self.reboot().is_ok() {
            report.data_mode = DataMode::Rebooted;
            report.rebooted = true;
        } else {
            report.data_mode = DataMode::Unknown;
            report
                .warnings
                .push("modem may still be in command mode; power-cycle it".into());
        }
    }
    fn reboot(&mut self) -> Result<(), Abort> {
        self.write_all(b"ATZ\r")?;
        // ATZ does not answer; the modem restarts in data mode.
        self.io.sleep(self.t.reboot);
        self.rx.clear();
        Ok(())
    }
}

fn contains_ok_line(rx: &[u8]) -> bool {
    rx.windows(4).any(|w| w == b"OK\r\n")
        || rx.windows(3).any(|w| w == b"OK\r")
        || rx.ends_with(b"OK\n")
}

fn parse_answer(rx: &[u8], cmd: &str) -> Answer {
    let mut answer = Answer::default();
    let text = String::from_utf8_lossy(rx);
    // Only complete lines count; a partial trailing line may still grow.
    let complete = match text.rfind(['\r', '\n']) {
        Some(end) => &text[..end],
        None => "",
    };
    for raw in complete.split(['\r', '\n']) {
        let line: String = raw.chars().filter(|c| !c.is_control()).collect();
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case(cmd) {
            continue;
        }
        if line == "OK" {
            answer.ok = true;
        } else if line == "ERROR" {
            answer.error = true;
        } else if answer.lines.len() < 64 {
            answer.lines.push(line.chars().take(120).collect());
        }
    }
    answer
}

fn parse_int(line: &str) -> Option<i64> {
    let line = line.trim();
    (!line.is_empty() && line.len() <= 12)
        .then(|| line.parse::<i64>().ok())
        .flatten()
}

/// One `ATI5` line: `S5:GLOBAL_RETRIES=10`.
fn parse_ati5_line(line: &str) -> Option<(u8, String, i64)> {
    let rest = line.trim().strip_prefix('S')?;
    let (number, rest) = rest.split_once(':')?;
    let (name, value) = rest.split_once('=')?;
    let number = number.trim().parse::<u8>().ok()?;
    let name = name.trim();
    if name.is_empty()
        || name.len() > 32
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '-'))
    {
        return None;
    }
    Some((number, name.to_ascii_uppercase(), parse_int(value)?))
}

fn names_match(observed: &str, expected: &[&str]) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_uppercase()
    };
    let observed = norm(observed);
    expected.iter().any(|e| norm(e) == observed)
}

struct Snapshot {
    named: Vec<(u8, String, i64)>,
    values: Vec<(u8, Option<String>, Option<i64>)>,
}

fn read_snapshot(d: &mut Driver<'_>, plan: &Plan) -> Result<Snapshot, Abort> {
    let ati5 = d.command("ATI5", d.t.command * 2)?;
    let named: Vec<_> = ati5
        .lines
        .iter()
        .filter_map(|l| parse_ati5_line(l))
        .collect();
    let mut values = Vec::new();
    for expect in &plan.regs {
        if let Some((_, name, value)) = named.iter().find(|(n, _, _)| *n == expect.number) {
            values.push((expect.number, Some(name.clone()), Some(*value)));
        } else {
            let value = d.read_value(expect.number)?;
            values.push((expect.number, None, value));
        }
    }
    Ok(Snapshot { named, values })
}

/// Whether a register's number is trusted for a write.
fn map_source(identity: Option<&str>, snap: &Snapshot, plan: &Plan) -> MapSource {
    let fixed = plan.regs.iter().filter(|e| e.policy == Policy::Fix);
    // Every writable register must be found by its own name somewhere in ATI5.
    let all_named = fixed.clone().all(|e| {
        snap.named.iter().any(|(_, name, _)| names_match(name, e.names))
    });
    if all_named {
        return MapSource::Ati5Names;
    }
    if !snap.named.is_empty() {
        // Names exist but differ from V1.1: a different map. Never guess.
        return MapSource::Unconfirmed;
    }
    let value = |number: u8| {
        snap.values
            .iter()
            .find(|(n, _, _)| *n == number)
            .and_then(|(_, _, v)| *v)
    };
    let expected = |number: u8| {
        plan.regs
            .iter()
            .find(|e| e.number == number)
            .and_then(|e| e.value)
    };
    let async_identity = identity.is_some_and(|s| s.to_ascii_uppercase().contains("ASYNC"));
    let fingerprint = async_identity
        && value(1).is_some()
        && value(1) == expected(1)
        && value(3).is_some_and(|v| (1..=4068).contains(&v))
        && value(2)
            .is_some_and(|v| [12, 56, 64, 100, 125, 188, 200, 224, 250, 500, 830].contains(&v))
        && value(11).is_some_and(|v| (1..=65535).contains(&v));
    if fingerprint {
        MapSource::Fingerprint
    } else {
        MapSource::Unconfirmed
    }
}

/// The register the modem itself names as `e`, else the documented number.
fn resolve(e: &Expect, snap: &Snapshot) -> (u8, Option<String>, Option<i64>) {
    if let Some((n, name, value)) = snap.named.iter().find(|(_, name, _)| names_match(name, e.names)) {
        return (*n, Some(name.clone()), Some(*value));
    }
    snap.values
        .iter()
        .find(|(n, _, _)| *n == e.number)
        .cloned()
        .unwrap_or((e.number, None, None))
}

fn compare(plan: &Plan, snap: &Snapshot, source: MapSource) -> Vec<RegisterRow> {
    plan.regs
        .iter()
        .map(|e| {
            let (number, name, observed) = resolve(e, snap);
            let name_ok = match (&name, source) {
                (Some(name), _) => names_match(name, e.names),
                (None, MapSource::Fingerprint) => true,
                (None, _) => false,
            };
            let status = if e.policy == Policy::Info {
                "info"
            } else if observed.is_none() {
                "unread"
            } else if name.is_some() && !name_ok {
                "name_mismatch"
            } else if observed == e.value {
                "match"
            } else {
                "differs"
            };
            RegisterRow {
                number,
                name,
                observed,
                expected: e.value,
                policy: e.policy,
                status,
            }
        })
        .collect()
}

pub(crate) type Gate<'a> = &'a dyn Fn() -> Result<(), String>;

/// Runs one bounded check. `gate` is consulted before command mode is
/// entered, before the first write and again before `AT&W`. `allow_save`
/// is false once this process has used its EEPROM write budget.
pub(crate) fn run(
    io: &mut dyn ModemIo,
    plan: &Plan,
    trigger: Trigger,
    mode: Mode,
    timing: Timing,
    gate: Gate<'_>,
    allow_save: bool,
) -> Report {
    let started = io.now();
    let mut report = Report::new(trigger, mode);
    if mode == Mode::Skip {
        return Report::skipped(trigger, mode, "modem check disabled by operator override");
    }
    if let Err(reason) = gate() {
        return Report::skipped(trigger, mode, format!("not checked: {reason}"));
    }
    let mut d = Driver {
        deadline: started + timing.overall,
        io,
        t: timing,
        rx: Vec::with_capacity(1024),
        saw_plus_ok: false,
    };
    let result = run_inner(&mut d, plan, mode, gate, allow_save, &mut report);
    if let Err(abort) = result {
        let what = match abort {
            Abort::Io(e) => format!("serial error: {e}"),
            Abort::Deadline => "check exceeded its time bound".into(),
        };
        report.outcome = if report.eeprom_saved {
            Outcome::VerifyFailed
        } else {
            Outcome::Failed
        };
        report.summary = what.clone();
        report.warnings.push(what);
        if report.data_mode == DataMode::Untouched {
            report.data_mode = DataMode::Unknown;
        }
    }
    report.duration_ms = d.io.now().saturating_sub(started).as_millis() as u64;
    report.finished_unix_ms = unix_ms();
    report
}

fn run_inner(
    d: &mut Driver<'_>,
    plan: &Plan,
    mode: Mode,
    gate: Gate<'_>,
    allow_save: bool,
    report: &mut Report,
) -> Result<(), Abort> {
    if !d.enter_command_mode()? {
        report.outcome = Outcome::NoResponse;
        report.summary = "modem did not enter command mode; running on its current settings".into();
        if d.saw_plus_ok {
            // It answered +++ but not AT: it may sit in command mode.
            d.exit_command_mode(report);
            report.data_mode = DataMode::Unknown;
            report
                .warnings
                .push("modem answered +++ but not AT; check that it passes data".into());
        }
        return Ok(());
    }
    report.data_mode = DataMode::Unknown;
    let identity = d.command("ATI", d.t.command)?;
    report.identity = identity.lines.first().cloned();
    let snap = read_snapshot(d, plan)?;
    let forwarding = d.command("AT+F?", d.t.command)?;
    report.forwarding = (!forwarding.lines.is_empty()).then(|| forwarding.lines.join(" | "));
    if forwarding_has_entries(&forwarding.lines) {
        report.warnings.push(
            "modem forwarding table has entries: modem-level relaying duplicates traffic beside the software RRLS relay"
                .into(),
        );
    }
    let source = map_source(report.identity.as_deref(), &snap, plan);
    report.map_source = source;
    report.registers = compare(plan, &snap, source);
    for row in &report.registers {
        match (row.status, row.policy) {
            ("differs", Policy::ReportOnly) => report.warnings.push(format!(
                "S{} is {:?}, profile expects {:?}; it must match on every node and is written only for a profile marked as rolled out to all nodes",
                row.number, row.observed, row.expected
            )),
            ("name_mismatch", _) => report.warnings.push(format!(
                "S{} is named {:?}, not as documented; it is not written",
                row.number,
                row.name.as_deref().unwrap_or("")
            )),
            ("unread", Policy::Fix | Policy::ReportOnly) => {
                report.warnings.push(format!("S{} could not be read", row.number))
            }
            _ => {}
        }
    }
    let pending: Vec<(u8, Option<i64>, i64)> = report
        .registers
        .iter()
        .filter(|r| r.policy == Policy::Fix && r.status == "differs")
        .filter_map(|r| Some((r.number, r.observed, r.expected?)))
        .collect();
    let report_only_differs = report.registers.iter().any(|r| {
        r.policy != Policy::Info && matches!(r.status, "differs" | "name_mismatch" | "unread")
    });
    if pending.is_empty() {
        report.outcome = if report_only_differs {
            Outcome::Mismatch
        } else {
            Outcome::Matched
        };
        report.summary = if report_only_differs {
            "settings read; some differ from the profile and need a matched change on every node"
                .into()
        } else {
            "modem settings match the profile; nothing written".into()
        };
        d.exit_command_mode(report);
        return Ok(());
    }
    let blocked = if mode != Mode::Fix {
        Some("verify-only mode".to_string())
    } else if source == MapSource::Unconfirmed {
        Some("the register map could not be confirmed".to_string())
    } else if !allow_save {
        Some("EEPROM write budget for this run is used up".to_string())
    } else {
        gate().err()
    };
    if let Some(reason) = blocked {
        report.outcome = Outcome::Mismatch;
        report.summary = format!("{} setting(s) differ; not written: {reason}", pending.len());
        d.exit_command_mode(report);
        return Ok(());
    }
    for &(number, from, to) in &pending {
        let answer = d.command(&format!("ATS{number}={to}"), d.t.command)?;
        report.writes.push(WriteRow {
            number,
            from,
            to,
            accepted: answer.ok,
        });
        if !answer.ok {
            // Discard every RAM change: ATZ without AT&W reloads EEPROM.
            report.outcome = Outcome::ApplyFailed;
            report.summary = format!("modem refused S{number}={to}; changes discarded");
            d.reboot()?;
            report.rebooted = true;
            report.data_mode = DataMode::Rebooted;
            return Ok(());
        }
    }
    if let Err(reason) = gate() {
        report.outcome = Outcome::Mismatch;
        report.summary = format!("not saved: {reason}; changes discarded");
        d.reboot()?;
        report.rebooted = true;
        report.data_mode = DataMode::Rebooted;
        return Ok(());
    }
    report.save_attempted = true;
    let save = d.command("AT&W", d.t.save)?;
    if !save.ok {
        report.outcome = Outcome::ApplyFailed;
        report.summary = "AT&W was not acknowledged; changes discarded".into();
        d.reboot()?;
        report.rebooted = true;
        report.data_mode = DataMode::Rebooted;
        return Ok(());
    }
    report.eeprom_saved = true;
    d.reboot()?;
    report.rebooted = true;
    report.data_mode = DataMode::Rebooted;
    if !d.enter_command_mode()? {
        report.outcome = Outcome::VerifyFailed;
        report.summary =
            "saved and rebooted, but the modem did not re-enter command mode to verify".into();
        return Ok(());
    }
    report.data_mode = DataMode::Unknown;
    let after = read_snapshot(d, plan)?;
    let rows = compare(plan, &after, source);
    let still: Vec<u8> = pending
        .iter()
        .filter(|(n, _, to)| {
            rows.iter()
                .find(|r| r.number == *n)
                .is_none_or(|r| r.observed != Some(*to))
        })
        .map(|(n, _, _)| *n)
        .collect();
    report.registers = rows;
    let unwritten = report.registers.iter().any(|r| {
        r.policy == Policy::ReportOnly && matches!(r.status, "differs" | "name_mismatch" | "unread")
    });
    if still.is_empty() && unwritten {
        report.outcome = Outcome::Mismatch;
        report.summary = format!(
            "{} local setting(s) written and verified; link-wide differences remain and were not written",
            pending.len()
        );
    } else if still.is_empty() {
        report.outcome = Outcome::Applied;
        report.summary = format!(
            "{} setting(s) written, saved and verified after reboot",
            pending.len()
        );
    } else {
        report.outcome = Outcome::VerifyFailed;
        report.summary = format!(
            "after reboot S{} still differ; not retried",
            still
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", S")
        );
    }
    d.exit_command_mode(report);
    Ok(())
}

fn forwarding_has_entries(lines: &[String]) -> bool {
    // Manual: "AT+Fx=Src,Dst". Any pair other than 0,0 is an entry.
    lines.iter().any(|line| {
        line.split(|c: char| !(c.is_ascii_digit() || c == ','))
            .filter_map(|tok| tok.split_once(','))
            .any(|(a, b)| {
                let (a, b) = (a.parse::<u32>(), b.parse::<u32>());
                matches!((a, b), (Ok(a), Ok(b)) if a != 0 || b != 0)
            })
    })
}

// --- EEPROM budget -----------------------------------------------------------

/// RFDesign does not publish EEPROM endurance. Bound saves per process run.
const MAX_SAVES_PER_RUN: u64 = 3;
static SAVES: AtomicU64 = AtomicU64::new(0);

fn budget_allows_save() -> bool {
    SAVES.load(Ordering::Acquire) < MAX_SAVES_PER_RUN
}
fn note_save() {
    SAVES.fetch_add(1, Ordering::AcqRel);
}

// --- Station gate ------------------------------------------------------------

/// `None` until the station state is known: unknown blocks everything.
static GATE: Mutex<Option<Option<String>>> = Mutex::new(None);

pub(crate) fn set_gate(block_reason: Option<String>) {
    *GATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(block_reason);
}

pub(crate) fn gate() -> Result<(), String> {
    match &*GATE.lock().unwrap_or_else(|e| e.into_inner()) {
        None => Err("station state is not known yet".into()),
        Some(Some(reason)) => Err(reason.clone()),
        Some(None) => Ok(()),
    }
}

/// The modem may enter command mode only before fill and launch: in
/// Startup, Idle, PreFill or FillTest, with no launch or sequence running.
/// Launch through Landed (and Recovery/Aborted, where the radio carries
/// recovery data) always block.
pub(crate) fn flight_state_block(state: crate::types::FlightState) -> Option<String> {
    use crate::types::FlightState as F;
    (!matches!(state, F::Startup | F::Idle | F::PreFill | F::FillTest))
        .then(|| format!("station is in flight state {state:?}"))
}

pub(crate) fn station_block_reason(state: &crate::state::AppState) -> Option<String> {
    let flight = *state.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(reason) = flight_state_block(flight) {
        return Some(reason);
    }
    if state.operator_queue.fill_launch_active() {
        return Some("the launch sequence is active".into());
    }
    let running = state
        .sequence_progress
        .lock()
        .map(|service| service.has_active_run())
        .unwrap_or(true);
    running.then(|| "a sequence is running".into())
}

// --- Orchestration -----------------------------------------------------------

#[derive(Default)]
struct Global {
    report: Option<Report>,
    startup_done: bool,
}

static GLOBAL: Mutex<Global> = Mutex::new(Global {
    report: None,
    startup_done: false,
});
static RUNNING: AtomicBool = AtomicBool::new(false);
static AV_BAY: std::sync::OnceLock<CommsLinkConfig> = std::sync::OnceLock::new();

/// Records the AV-bay link for the status API and operator checks.
pub(crate) fn remember_av_bay(link: &CommsLinkConfig) {
    let _ = AV_BAY.set(link.clone());
}
/// While set, the AV-bay radio stays closed so a check can own the port.
static HOLD: AtomicBool = AtomicBool::new(false);
static RELEASES: AtomicU64 = AtomicU64::new(0);

fn store(report: Report) {
    if report.needs_attention() {
        log::warn!(
            "RFD modem check ({:?}): {:?}: {}; warnings: {:?}",
            report.trigger,
            report.outcome,
            report.summary,
            report.warnings
        );
    } else {
        log::info!(
            "RFD modem check ({:?}): {:?}: {}",
            report.trigger,
            report.outcome,
            report.summary
        );
    }
    let passed = matches!(report.outcome, Outcome::Matched | Outcome::Applied);
    if !matches!(report.outcome, Outcome::Skipped) {
        let line = format!(
            "Radio modem check {}: {}",
            if passed { "passed" } else { "needs attention" },
            report.summary
        );
        *NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = Some(line);
    }
    GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).report = Some(report);
}

/// One operator notification per finished check (pass or fail), taken by the
/// station loop that owns AppState.
static NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
pub(crate) fn take_notice() -> Option<String> {
    NOTICE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

pub(crate) fn configured_mode() -> Mode {
    if let Some(mode) = std::env::var("GS_RFD_MODEM_CHECK")
        .ok()
        .and_then(|v| Mode::parse(&v))
    {
        return mode;
    }
    if cfg!(any(
        feature = "testing",
        feature = "hitl_mode",
        feature = "firmware_sim"
    )) {
        Mode::Skip
    } else {
        Mode::Fix
    }
}

fn raw_radio(link: &CommsLinkConfig) -> Option<&SerialLinkConfig> {
    match link {
        CommsLinkConfig::Serial { serial }
        | CommsLinkConfig::CustomSerial { serial }
        | CommsLinkConfig::RaspberryPiGpioUart { serial }
            if serial.protocol == SerialProtocol::RawUart =>
        {
            Some(serial)
        }
        _ => None,
    }
}

fn plan_for(serial: &SerialLinkConfig) -> Option<Plan> {
    let profile = super::rfd_profile::active()?;
    Some(Plan::new(profile.modem_values(
        serial.hardware_flow_control,
        rolled_out(profile.name()),
    )))
}

/// `GS_RFD_PROFILE_ROLLED_OUT` must name the selected profile exactly. It
/// records that the same profile is set on GS, RRLS and the RF board.
fn rolled_out(profile_name: &str) -> bool {
    std::env::var("GS_RFD_PROFILE_ROLLED_OUT").is_ok_and(|v| v.trim() == profile_name)
}

fn simulated_link() -> bool {
    std::env::var("GS_SIMULATED_SERIAL_PTY").as_deref() == Ok("1")
}

/// Called by the AV-bay opener before the radio port is opened for data.
/// Runs the startup check once per process; later opens do nothing.
pub(crate) fn before_av_bay_open(link: &CommsLinkConfig) {
    let Some(serial) = raw_radio(link) else {
        return;
    };
    {
        let mut global = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        if global.startup_done {
            return;
        }
        global.startup_done = true;
    }
    let mode = configured_mode();
    let report = match plan_for(serial) {
        None => Report::skipped(Trigger::Startup, mode, "no RFD profile is selected"),
        Some(_) if mode == Mode::Skip => Report::skipped(
            Trigger::Startup,
            mode,
            "modem check off: skipped by default in HITL, testing and simulator builds (set GS_RFD_MODEM_CHECK=verify or fix to run it)",
        ),
        Some(_) if simulated_link() => {
            Report::skipped(Trigger::Startup, mode, "simulated serial link")
        }
        Some(plan) => run_on_port(serial, &plan, Trigger::Startup, mode),
    };
    store(report);
}

fn run_on_port(serial: &SerialLinkConfig, plan: &Plan, trigger: Trigger, mode: Mode) -> Report {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return Report::skipped(trigger, mode, "another modem check is running");
    }
    let report = match open_port(serial) {
        Ok(mut io) => {
            let report = run(
                &mut io,
                plan,
                trigger,
                mode,
                TIMING,
                &gate,
                budget_allows_save(),
            );
            if report.save_attempted {
                note_save();
            }
            report
        }
        Err(error) => Report::failed(
            trigger,
            mode,
            format!(
                "could not open {} for the modem check: {error}",
                serial.port
            ),
        ),
    };
    RUNNING.store(false, Ordering::Release);
    report
}

fn open_port(serial: &SerialLinkConfig) -> io::Result<SerialModemIo> {
    // A just-released data handle may close a moment later.
    let mut last = None;
    for _ in 0..10 {
        match SerialModemIo::open(serial) {
            Ok(io) => return Ok(io),
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(last.unwrap_or_else(|| io::Error::other("serial open failed")))
}

/// For the AV-bay `RecoveryRadio`: true while a check owns the port.
pub(crate) fn hold_requested() -> bool {
    HOLD.load(Ordering::Acquire)
}
pub(crate) fn acknowledge_release() {
    RELEASES.fetch_add(1, Ordering::AcqRel);
}

#[cfg(test)]
pub(crate) fn test_set_hold(on: bool) {
    HOLD.store(on, Ordering::Release);
}
#[cfg(test)]
pub(crate) fn test_releases() -> u64 {
    RELEASES.load(Ordering::Acquire)
}

/// Operator request. Refused in a flight state, during a launch or a
/// sequence, while another check runs, or without an RFD radio.
pub(crate) fn request_operator_check(mode: Mode) -> Result<(), String> {
    let link = AV_BAY
        .get()
        .ok_or_else(|| "the AV-bay link is not configured".to_string())?;
    let serial = raw_radio(link)
        .ok_or_else(|| "the AV-bay link is not a raw-UART RFD radio".to_string())?
        .clone();
    let plan = plan_for(&serial).ok_or_else(|| "no RFD profile is selected".to_string())?;
    if mode == Mode::Skip {
        return Err("choose verify or fix".into());
    }
    if simulated_link() {
        return Err("the AV-bay link is simulated".into());
    }
    gate()?;
    if RUNNING.load(Ordering::Acquire) || HOLD.swap(true, Ordering::AcqRel) {
        return Err("a modem check is already running".into());
    }
    let releases = RELEASES.load(Ordering::Acquire);
    std::thread::Builder::new()
        .name("rfd-modem-check".into())
        .spawn(move || {
            // Wait for the radio worker to drop its handles.
            let until = Instant::now() + Duration::from_secs(3);
            while RELEASES.load(Ordering::Acquire) == releases && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            let report = if RELEASES.load(Ordering::Acquire) == releases {
                Report::failed(
                    Trigger::Operator,
                    mode,
                    "the radio link did not release the port; not checked",
                )
            } else {
                run_on_port(&serial, &plan, Trigger::Operator, mode)
            };
            store(report);
            HOLD.store(false, Ordering::Release);
        })
        .map_err(|e| {
            HOLD.store(false, Ordering::Release);
            format!("could not start the modem check: {e}")
        })?;
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct Status {
    pub available: bool,
    pub profile_name: Option<String>,
    /// The selected profile is not the embedded deployed-settings profile.
    pub opt_in_profile: bool,
    pub rolled_out_all_nodes: bool,
    pub configured_mode: Mode,
    pub running: bool,
    pub reconfiguration_allowed: bool,
    pub blocked_reason: Option<String>,
    pub needs_attention: bool,
    pub report: Option<Report>,
}

pub(crate) fn status() -> Status {
    let report = GLOBAL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .report
        .clone();
    let gate = gate();
    let profile = super::rfd_profile::active();
    Status {
        profile_name: profile.map(|p| p.name().to_string()),
        opt_in_profile: profile.is_some_and(|p| !p.is_embedded_default()),
        rolled_out_all_nodes: profile.is_some_and(|p| rolled_out(p.name())),
        available: AV_BAY
            .get()
            .and_then(raw_radio)
            .and_then(plan_for)
            .is_some(),
        configured_mode: configured_mode(),
        running: RUNNING.load(Ordering::Acquire) || HOLD.load(Ordering::Acquire),
        reconfiguration_allowed: gate.is_ok(),
        blocked_reason: gate.err(),
        needs_attention: report.as_ref().is_some_and(Report::needs_attention),
        report,
    }
}

// --- Real serial port ----------------------------------------------------------

pub(crate) struct SerialModemIo {
    #[cfg(target_os = "linux")]
    port: serialport::TTYPort,
    #[cfg(not(target_os = "linux"))]
    port: Box<dyn serialport::SerialPort>,
    epoch: Instant,
}

impl SerialModemIo {
    fn open(serial: &SerialLinkConfig) -> io::Result<Self> {
        let builder = serialport::new(&serial.port, serial.baud_rate as u32)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(if serial.hardware_flow_control {
                serialport::FlowControl::Hardware
            } else {
                serialport::FlowControl::None
            })
            .timeout(Duration::from_millis(20));
        #[cfg(target_os = "linux")]
        let port = builder.open_native().map_err(io::Error::other)?;
        #[cfg(not(target_os = "linux"))]
        let port = builder.open().map_err(io::Error::other)?;
        Ok(Self {
            port,
            epoch: Instant::now(),
        })
    }
}

impl ModemIo for SerialModemIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        use std::io::Write;
        let n = self.port.write(bytes)?;
        self.port.flush()?;
        Ok(n)
    }
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::io::Read;
        match self.port.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            other => other,
        }
    }
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
#[path = "rfd_modem_tests.rs"]
mod tests;
