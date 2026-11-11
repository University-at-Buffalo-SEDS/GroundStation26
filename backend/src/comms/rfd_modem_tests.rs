//! Simulated RFD900x ASYNC modem for the startup check. No serial device.
//!
//! The double follows the documented command-mode rules (manual V1.1 p. 5):
//! `+++` only after a second of host silence, OK after a further second of
//! silence, echoed command characters, ATI/ATI5/ATSn? answers without a
//! trailing OK (as SiK-derived firmware does), ATSn=X/AT&W answered OK,
//! ATZ reloading EEPROM and rebooting into data mode, and ATO without output.
use super::*;
use std::collections::BTreeMap;
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Data,
    Command,
    Rebooting(Duration),
}

struct SimModem {
    now: Duration,
    state: State,
    names: BTreeMap<u8, &'static str>,
    ram: BTreeMap<u8, i64>,
    eeprom: BTreeMap<u8, i64>,
    to_host: VecDeque<(Duration, u8)>,
    last_host_byte: Duration,
    plus: Option<Duration>,
    pluses: usize,
    line: Vec<u8>,
    /// Bytes the host sent while in data mode (would go on air).
    air_bytes: usize,
    eeprom_writes: usize,
    // Fault knobs.
    dead: bool,
    garbage: bool,
    max_write: usize,
    ato_error: bool,
    refuse_reg: Option<u8>,
    save_does_not_persist: bool,
    ati5_names: bool,
    /// The ATI identity line.
    identity: &'static str,
    noise: bool,
    write_error_after: Option<usize>,
    written: usize,
}

const DEFAULTS: [(u8, &str, i64); 21] = [
    (0, "FORMAT", 27),
    (1, "SERIAL_SPEED", 115),
    (2, "AIR_SPEED", 64),
    (3, "MAX_DATA", 3968),
    (4, "MAX_RETRIES", 10),
    (5, "GLOBAL_RETRIES", 10),
    (6, "SER_BREAK_DETECT_MS", 50),
    (7, "TXENCAP", 0),
    (8, "RXENCAP", 0),
    (9, "NETID", 25),
    (10, "NODEID", 1),
    (11, "DESTID", 32768),
    (12, "TXPOWER", 30),
    (13, "MAVLINK1", 0),
    (14, "MIN_FREQ", 915000),
    (15, "MAX_FREQ", 928000),
    (16, "NUM_CHANNELS", 20),
    (17, "LBT_RSSI", 0),
    (18, "RTSCTS", 0),
    (19, "ENCRYPTION_LEVEL", 0),
    (22, "ANTMODE", 2),
];

impl SimModem {
    fn new() -> Self {
        let names = DEFAULTS.iter().map(|(n, name, _)| (*n, *name)).collect();
        let values: BTreeMap<u8, i64> = DEFAULTS.iter().map(|(n, _, v)| (*n, *v)).collect();
        Self {
            now: Duration::from_secs(100),
            state: State::Data,
            names,
            ram: values.clone(),
            eeprom: values,
            to_host: VecDeque::new(),
            last_host_byte: Duration::ZERO,
            plus: None,
            pluses: 0,
            line: Vec::new(),
            air_bytes: 0,
            eeprom_writes: 0,
            dead: false,
            garbage: false,
            max_write: usize::MAX,
            ato_error: false,
            refuse_reg: None,
            save_does_not_persist: false,
            ati5_names: true,
            identity: "RFD ASYNC 4.03 on RFD900X2",
            noise: false,
            write_error_after: None,
            written: 0,
        }
    }
    fn set(mut self, reg: u8, value: i64) -> Self {
        self.ram.insert(reg, value);
        self.eeprom.insert(reg, value);
        self
    }
    fn say(&mut self, text: &str) {
        let at = self.now + Duration::from_millis(5);
        self.to_host.extend(text.bytes().map(|b| (at, b)));
    }
    fn tick(&mut self) {
        if let State::Rebooting(until) = self.state
            && self.now >= until
        {
            self.state = State::Data;
            self.ram = self.eeprom.clone();
        }
        // +++ is honoured only after a further second of host silence.
        if let Some(at) = self.plus
            && self.pluses == 3
            && self.now >= at + Duration::from_secs(1)
            && self.last_host_byte <= at
        {
            self.plus = None;
            self.pluses = 0;
            if !self.dead {
                self.state = State::Command;
                self.say("OK\r\n");
            }
        }
    }
    fn execute(&mut self, cmd: &str) {
        if self.garbage {
            self.say("\u{7f}#%\r\nZ9q\r\n");
            return;
        }
        let reply: String = match cmd {
            "AT" => "OK\r\n".into(),
            "ATI" => format!("{}\r\n", self.identity),
            "ATI5" => self
                .ram
                .iter()
                .map(|(n, v)| {
                    if self.ati5_names {
                        format!("S{n}:{}={v}\r\n", self.names[n])
                    } else {
                        format!("{v}\r\n")
                    }
                })
                .collect(),
            "AT+F?" => "Forwarding buffer 100\r\n".into(),
            "AT&W" => {
                if !self.save_does_not_persist {
                    self.eeprom = self.ram.clone();
                }
                self.eeprom_writes += 1;
                "OK\r\n".into()
            }
            "ATZ" => {
                self.state = State::Rebooting(self.now + Duration::from_millis(1500));
                String::new()
            }
            "ATO" => {
                if self.ato_error {
                    "ERROR\r\n".into()
                } else {
                    self.state = State::Data;
                    String::new()
                }
            }
            other => {
                let query = other
                    .strip_prefix("ATS")
                    .and_then(|r| r.strip_suffix('?'))
                    .and_then(|n| n.parse::<u8>().ok());
                let set = other
                    .strip_prefix("ATS")
                    .and_then(|r| r.split_once('='))
                    .and_then(|(n, v)| Some((n.parse::<u8>().ok()?, v.parse::<i64>().ok()?)));
                match (query, set) {
                    (Some(n), _) if self.ram.contains_key(&n) => format!("{}\r\n", self.ram[&n]),
                    (_, Some((n, v)))
                        if self.ram.contains_key(&n) && self.refuse_reg != Some(n) =>
                    {
                        self.ram.insert(n, v);
                        "OK\r\n".into()
                    }
                    _ => "ERROR\r\n".into(),
                }
            }
        };
        self.say(&reply);
    }
}

impl ModemIo for SimModem {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.tick();
        if let Some(limit) = self.write_error_after
            && self.written >= limit
        {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "unplugged"));
        }
        let n = bytes.len().min(self.max_write);
        self.written += n;
        for &b in &bytes[..n] {
            let silent = self.now.saturating_sub(self.last_host_byte) >= Duration::from_secs(1);
            match self.state {
                State::Rebooting(_) => {}
                State::Data => {
                    if b == b'+' && (self.pluses > 0 || silent) && self.pluses < 3 {
                        self.pluses += 1;
                        self.plus = Some(self.now);
                    } else {
                        self.air_bytes += 1 + self.pluses;
                        self.pluses = 0;
                        self.plus = None;
                    }
                }
                State::Command => {
                    if !self.garbage {
                        self.to_host.push_back((self.now, b));
                    }
                    if b == b'\r' {
                        let line = String::from_utf8_lossy(&self.line).trim().to_string();
                        self.line.clear();
                        if !line.is_empty() {
                            self.execute(&line);
                        }
                    } else {
                        self.line.push(b);
                    }
                }
            }
            self.last_host_byte = self.now;
        }
        // 115200 baud: about 87 us per byte.
        self.now += Duration::from_micros(87 * n as u64);
        Ok(n)
    }
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.tick();
        if self.noise && self.state == State::Data {
            // Air data keeps streaming in data mode, including a decoy "OK".
            for b in b"\x00\x7eOK\r\n\x55\xaa".iter().copied() {
                self.to_host.push_back((self.now, b));
            }
        }
        let mut n = 0;
        while n < buf.len() {
            match self.to_host.front() {
                Some((at, b)) if *at <= self.now => {
                    buf[n] = *b;
                    n += 1;
                    self.to_host.pop_front();
                }
                _ => break,
            }
        }
        if n == 0 {
            self.now += Duration::from_millis(20);
        } else {
            self.now += Duration::from_micros(87 * n as u64);
        }
        Ok(n)
    }
    fn now(&self) -> Duration {
        self.now
    }
    fn sleep(&mut self, duration: Duration) {
        self.now += duration;
        self.tick();
    }
}

fn values(s5: u64, s6: u64, rolled_out_all_nodes: bool) -> ProfileValues {
    ProfileValues {
        serial_baud: 115200,
        air_bps: 64000,
        max_data_bytes: 3968,
        max_retries: 10,
        broadcast_retries: s5,
        serial_break_ms: s6,
        tx_encapsulation: 0,
        rx_encapsulation: 0,
        hardware_flow_control: false,
        rolled_out_all_nodes,
    }
}

/// The opt-in S5=0/S6=20 profile, marked as rolled out to all nodes.
fn plan() -> Plan {
    Plan::new(values(0, 20, true))
}

/// The embedded default: the deployed, matched settings.
fn deployed_plan() -> Plan {
    Plan::new(values(10, 50, false))
}

fn open() -> Result<(), String> {
    Ok(())
}

fn check(modem: &mut SimModem, mode: Mode) -> Report {
    run(modem, &plan(), Trigger::Startup, mode, TIMING, &open, true)
}

/// A modem already at the profile values.
fn matched() -> SimModem {
    SimModem::new().set(5, 0).set(6, 20)
}

fn row(report: &Report, number: u8) -> &RegisterRow {
    report
        .registers
        .iter()
        .find(|r| r.number == number)
        .unwrap()
}

#[test]
fn matching_modem_is_read_and_left_alone() {
    let mut modem = matched();
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Matched, "{report:?}");
    assert!(report.writes.is_empty());
    assert!(!report.eeprom_saved && !report.rebooted);
    assert_eq!(modem.eeprom_writes, 0);
    assert_eq!(modem.state, State::Data);
    assert_eq!(report.data_mode, DataMode::Resumed);
    assert_eq!(report.map_source, MapSource::Ati5Names);
    assert_eq!(
        report.identity.as_deref(),
        Some("RFD ASYNC 4.03 on RFD900X2")
    );
    assert_eq!(
        modem.air_bytes, 0,
        "nothing but +++ may reach the modem in data mode"
    );
    assert!(!report.needs_attention());
    assert!(report.duration_ms < 6000, "{}", report.duration_ms);
}

#[test]
fn differing_settings_are_written_saved_rebooted_and_verified_once() {
    let mut modem = SimModem::new();
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Applied, "{report:?}");
    let written: Vec<_> = report
        .writes
        .iter()
        .map(|w| (w.number, w.from, w.to))
        .collect();
    assert_eq!(written, vec![(5, Some(10), 0), (6, Some(50), 20)]);
    assert!(report.eeprom_saved && report.rebooted);
    assert_eq!(modem.eeprom_writes, 1);
    assert_eq!((modem.eeprom[&5], modem.eeprom[&6]), (0, 20));
    assert_eq!(row(&report, 5).status, "match");
    assert_eq!(modem.state, State::Data);
    assert_eq!(modem.air_bytes, 0);
    assert!(report.duration_ms < TIMING.overall.as_millis() as u64);
}

#[test]
fn verify_mode_never_writes() {
    let mut modem = SimModem::new();
    let report = check(&mut modem, Mode::Verify);
    assert_eq!(report.outcome, Outcome::Mismatch);
    assert!(report.writes.is_empty());
    assert_eq!(modem.eeprom_writes, 0);
    assert_eq!(row(&report, 5).status, "differs");
    assert_eq!(modem.state, State::Data);
}

#[test]
fn skip_and_blocked_gate_touch_nothing() {
    let mut modem = SimModem::new();
    let report = run(
        &mut modem,
        &plan(),
        Trigger::Startup,
        Mode::Skip,
        TIMING,
        &open,
        true,
    );
    assert_eq!(report.outcome, Outcome::Skipped);
    let blocked = || Err::<(), _>("station is in flight state Ascent".to_string());
    let report = run(
        &mut modem,
        &plan(),
        Trigger::Operator,
        Mode::Fix,
        TIMING,
        &blocked,
        true,
    );
    assert_eq!(report.outcome, Outcome::Skipped);
    assert!(report.summary.contains("Ascent"));
    assert_eq!(modem.written, 0, "not even +++ may be sent");
}

#[test]
fn gate_closing_after_the_read_prevents_every_write() {
    let mut modem = SimModem::new();
    let calls = std::cell::Cell::new(0);
    let gate = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            Ok(())
        } else {
            Err("the launch sequence is active".to_string())
        }
    };
    let report = run(
        &mut modem,
        &plan(),
        Trigger::Operator,
        Mode::Fix,
        TIMING,
        &gate,
        true,
    );
    assert_eq!(report.outcome, Outcome::Mismatch);
    assert!(report.writes.is_empty());
    assert_eq!(modem.eeprom_writes, 0);
    assert_eq!(modem.ram[&5], 10);
    assert_eq!(modem.state, State::Data);
}

#[test]
fn silent_modem_times_out_without_writing() {
    let mut modem = SimModem::new();
    modem.dead = true;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::NoResponse);
    assert!(report.needs_attention());
    assert_eq!(modem.eeprom_writes, 0);
    // Two guarded +++ attempts plus one probe, then stop.
    assert!(report.duration_ms < 12_000, "{}", report.duration_ms);
}

#[test]
fn garbage_replies_never_authorize_a_write() {
    let mut modem = SimModem::new();
    modem.garbage = true;
    let report = check(&mut modem, Mode::Fix);
    assert_ne!(report.outcome, Outcome::Applied);
    assert!(report.writes.is_empty());
    assert_eq!(modem.eeprom_writes, 0);
    assert!(report.duration_ms < TIMING.overall.as_millis() as u64);
}

#[test]
fn partial_serial_writes_are_completed() {
    let mut modem = SimModem::new();
    modem.max_write = 2;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Applied, "{report:?}");
    assert_eq!(modem.eeprom_writes, 1);
}

#[test]
fn refused_write_discards_ram_changes_without_saving() {
    let mut modem = SimModem::new();
    modem.refuse_reg = Some(6);
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::ApplyFailed);
    assert_eq!(modem.eeprom_writes, 0);
    assert!(report.rebooted);
    // ATZ reloaded EEPROM, so the accepted S5 change did not survive.
    modem.sleep(Duration::from_secs(2));
    assert_eq!(modem.state, State::Data);
    assert_eq!(modem.ram[&5], 10);
}

#[test]
fn modem_that_ignores_ato_is_rebooted_into_data_mode() {
    let mut modem = matched();
    modem.ato_error = true;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Matched);
    assert_eq!(report.data_mode, DataMode::Rebooted);
    assert!(report.needs_attention());
    modem.sleep(Duration::from_secs(2));
    assert_eq!(modem.state, State::Data);
    assert_eq!(modem.eeprom_writes, 0);
}

#[test]
fn save_that_does_not_persist_is_reported_and_not_retried() {
    let mut modem = SimModem::new();
    modem.save_does_not_persist = true;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::VerifyFailed, "{report:?}");
    assert_eq!(modem.eeprom_writes, 1, "exactly one save attempt");
    assert_eq!(modem.state, State::Data);
}

#[test]
fn link_wide_registers_are_reported_never_written() {
    let mut modem = matched().set(2, 125).set(17, 3);
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Mismatch);
    assert!(report.writes.is_empty());
    assert_eq!(modem.ram[&2], 125);
    assert!(report.warnings.iter().any(|w| w.starts_with("S2 ")));
    assert!(report.warnings.iter().any(|w| w.starts_with("S17 ")));
}

#[test]
fn different_register_names_block_writes() {
    let mut modem = SimModem::new();
    modem.names.insert(5, "TXPOWER");
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.map_source, MapSource::Unconfirmed);
    assert!(report.writes.is_empty());
    assert_eq!(row(&report, 5).status, "name_mismatch");
}

#[test]
fn unnamed_ati5_falls_back_to_a_v1_1_fingerprint() {
    // Only an asynchronous 2.x modem: the firmware the V1.1 manual describes.
    let mut modem = SimModem::new();
    modem.ati5_names = false;
    modem.identity = "RFD ASYNC 2.69 on RFD900X";
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.map_source, MapSource::Fingerprint);
    assert_eq!(report.outcome, Outcome::Applied, "{report:?}");
    // The same unnamed readback from ASYNC 4.03 writes nothing.
    let mut modem = SimModem::new();
    modem.ati5_names = false;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.map_source, MapSource::Unconfirmed);
    assert!(report.writes.is_empty(), "{report:?}");
}

#[test]
fn modem_left_in_command_mode_is_recovered() {
    let mut modem = matched();
    modem.state = State::Command;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Matched, "{report:?}");
    assert_eq!(modem.state, State::Data);
}

#[test]
fn streaming_air_data_with_a_decoy_ok_is_handled() {
    let mut modem = SimModem::new();
    modem.noise = true;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Applied, "{report:?}");
    assert_eq!(modem.eeprom_writes, 1);
}

#[test]
fn serial_failure_mid_check_is_bounded_and_reported() {
    let mut modem = SimModem::new();
    modem.write_error_after = Some(40);
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(report.summary.contains("serial error"));
    assert_eq!(modem.eeprom_writes, 0);
}

#[test]
fn exhausted_save_budget_blocks_writes() {
    let mut modem = SimModem::new();
    let report = run(
        &mut modem,
        &plan(),
        Trigger::Operator,
        Mode::Fix,
        TIMING,
        &open,
        false,
    );
    assert_eq!(report.outcome, Outcome::Mismatch);
    assert!(report.summary.contains("budget"));
    assert_eq!(modem.eeprom_writes, 0);
}

#[test]
fn rtscts_follows_host_flow_control() {
    let rts = |p: Plan| p.regs.iter().find(|e| e.number == 18).unwrap().value;
    assert_eq!(rts(plan()), Some(0));
    assert_eq!(
        rts(Plan::new(ProfileValues {
            hardware_flow_control: true,
            ..values(0, 20, true)
        })),
        Some(1)
    );
}

#[test]
fn link_wide_registers_are_writable_only_for_a_rolled_out_profile() {
    let policy = |p: &Plan, n: u8| p.regs.iter().find(|e| e.number == n).unwrap().policy;
    let opt_in = Plan::new(values(0, 20, false));
    for n in [3, 4, 5, 7, 8] {
        assert_eq!(policy(&opt_in, n), Policy::ReportOnly, "S{n}");
        assert_eq!(policy(&plan(), n), Policy::Fix, "S{n}");
    }
    // Never written by one node, rolled out or not.
    for n in [1, 2, 17] {
        assert_eq!(policy(&plan(), n), Policy::ReportOnly, "S{n}");
    }
    for n in [9, 14, 15, 16, 19] {
        assert_eq!(policy(&plan(), n), Policy::Info, "S{n}");
    }
    // Local registers are always repaired.
    for n in [6, 11, 13, 18] {
        assert_eq!(policy(&opt_in, n), Policy::Fix, "S{n}");
    }
}

#[test]
fn deployed_profile_leaves_a_correct_install_untouched() {
    let mut modem = SimModem::new();
    let report = run(
        &mut modem,
        &deployed_plan(),
        Trigger::Startup,
        Mode::Fix,
        TIMING,
        &open,
        true,
    );
    assert_eq!(report.outcome, Outcome::Matched, "{report:?}");
    assert!(report.writes.is_empty());
    assert_eq!(modem.eeprom_writes, 0);
}

#[test]
fn deployed_profile_repairs_local_drift_but_reports_link_wide_drift() {
    let mut modem = SimModem::new().set(5, 3).set(6, 200).set(11, 2);
    let report = run(
        &mut modem,
        &deployed_plan(),
        Trigger::Startup,
        Mode::Fix,
        TIMING,
        &open,
        true,
    );
    let written: Vec<_> = report.writes.iter().map(|w| w.number).collect();
    assert_eq!(written, vec![6, 11]);
    assert_eq!(
        modem.eeprom[&5], 3,
        "S5 must match across nodes: never written alone"
    );
    assert_eq!((modem.eeprom[&6], modem.eeprom[&11]), (50, 32768));
    assert_eq!(report.outcome, Outcome::Mismatch, "{report:?}");
    assert!(report.needs_attention());
    assert!(report.warnings.iter().any(|w| w.starts_with("S5 ")));
}

#[test]
fn opt_in_profile_not_rolled_out_writes_only_local_registers() {
    let mut modem = SimModem::new();
    let p = Plan::new(values(0, 20, false));
    let report = run(
        &mut modem,
        &p,
        Trigger::Startup,
        Mode::Fix,
        TIMING,
        &open,
        true,
    );
    let written: Vec<_> = report.writes.iter().map(|w| w.number).collect();
    assert_eq!(written, vec![6]);
    assert_eq!(modem.eeprom[&5], 10);
    assert_eq!(report.outcome, Outcome::Mismatch);
}

#[test]
fn flight_states_from_launch_onward_block_reconfiguration() {
    use crate::types::FlightState as F;
    for state in [F::Startup, F::Idle, F::PreFill, F::FillTest] {
        assert_eq!(flight_state_block(state), None, "{state:?}");
    }
    for state in [
        F::NitrogenFill,
        F::NitrousFill,
        F::Armed,
        F::Launch,
        F::Ascent,
        F::Coast,
        F::Apogee,
        F::Descent,
        F::Reefing,
        F::Landed,
        F::Recovery,
        F::Aborted,
    ] {
        assert!(flight_state_block(state).is_some(), "{state:?}");
    }
}

/// Bench Pi: the GS restarted while Aborted, so its startup modem check was
/// skipped ("station is in flight state Aborted") and never ran again; both
/// modems stayed at S5=10 and the AV bay read about 36 s behind. Aborted at
/// rest (no launch, countdown, sequence or ABORT still being delivered) may
/// apply the profile: one modem reboot, a few seconds of radio loss.
#[tokio::test]
async fn aborted_at_rest_allows_the_modem_fix_but_not_while_an_abort_is_delivered() {
    let state = crate::state::tests::test_app_state().await;
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    assert_eq!(station_block_reason(&state), None, "Aborted at rest");
    crate::telemetry_schema::initialize().unwrap();
    let abort = sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("ABORT"),
        &[crate::telemetry_schema::endpoint("ABORT")],
        "GS",
        1,
        std::sync::Arc::from(&b"SEDS_MANUAL_ABORT_V1:K"[..]),
    )
    .unwrap();
    state.deferred_abort.observe(&abort, 1);
    assert!(
        station_block_reason(&state).is_some_and(|r| r.contains("abort")),
        "an operator ABORT still being processed blocks it"
    );
    let state = crate::state::tests::test_app_state().await;
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    state.launch_sequence_command_pending.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(station_block_reason(&state).is_some(), "a pending Launch blocks it");
    // Flight and fill states stay blocked.
    for flight in [crate::types::FlightState::Launch, crate::types::FlightState::NitrousFill, crate::types::FlightState::Recovery] {
        let state = crate::state::tests::test_app_state().await;
        *state.state.lock().unwrap() = flight;
        assert!(station_block_reason(&state).is_some(), "{flight:?}");
    }
}

#[tokio::test]
async fn station_gate_follows_flight_state_and_sequences() {
    let state = crate::state::tests::test_app_state().await;
    *state.state.lock().unwrap() = crate::types::FlightState::Idle;
    assert_eq!(station_block_reason(&state), None);
    *state.state.lock().unwrap() = crate::types::FlightState::Launch;
    assert!(station_block_reason(&state).unwrap().contains("Launch"));
}

#[test]
fn ati5_and_forwarding_parsers() {
    assert_eq!(
        parse_ati5_line("S6:SER_BREAK_DETECT_MS=50"),
        Some((6, "SER_BREAK_DETECT_MS".into(), 50))
    );
    assert_eq!(parse_ati5_line("S6:SER BREAK=5"), None);
    assert_eq!(parse_ati5_line("garbage"), None);
    assert!(names_match("SER_BREAK_DETECT_MS", &["SER_BREAK_DETECT_MS"]));
    assert!(!names_match("TXPOWER", &["GLOBAL_RETRIES"]));
    assert!(!forwarding_has_entries(&["Forwarding buffer 100".into()]));
    assert!(!forwarding_has_entries(&["F0=0,0".into()]));
    assert!(forwarding_has_entries(&["F0=1,3".into()]));
}

/// RFD ASYNC 4.03 on RFD900x2 as read from the field: S6-S8 use different
/// names and DUTY_CYCLE at S17 shifts LBT_RSSI to S18 and RTSCTS to S19.
fn field_403_layout() -> SimModem {
    let mut modem = SimModem::new();
    let layout: [(u8, &'static str, i64); 8] = [
        (6, "SER_BRK_DETMS", 50),
        (7, "TX_ENCAP_METHOD", 0),
        (8, "RX_ENCAP_METHOD", 0),
        (17, "DUTY_CYCLE", 100),
        (18, "LBT_RSSI", 0),
        (19, "RTSCTS", 0),
        (20, "MAX_WINDOW", 131),
        (22, "RESV", 0),
    ];
    for (n, name, v) in layout {
        modem.names.insert(n, name);
        modem.ram.insert(n, v);
        modem.eeprom.insert(n, v);
    }
    modem
}

#[test]
fn field_403_layout_is_matched_by_register_name() {
    let mut modem = field_403_layout();
    let report = run(&mut modem, &deployed_plan(), Trigger::Operator, Mode::Fix, TIMING, &open, true);
    assert_eq!(report.map_source, MapSource::Ati5Names, "{report:?}");
    assert_eq!(report.outcome, Outcome::Matched, "{report:?}");
    assert!(report.writes.is_empty() && modem.eeprom_writes == 0);
    assert!(report.warnings.iter().all(|w| !w.contains("not as documented")), "{:?}", report.warnings);
    let rts = report.registers.iter().find(|r| r.name.as_deref() == Some("RTSCTS")).unwrap();
    assert_eq!(rts.number, 19);
}

#[test]
fn field_403_layout_writes_flow_control_to_its_named_register() {
    let mut modem = field_403_layout();
    modem.ram.insert(19, 1);
    modem.eeprom.insert(19, 1);
    let report = run(&mut modem, &deployed_plan(), Trigger::Operator, Mode::Fix, TIMING, &open, true);
    assert_eq!(report.outcome, Outcome::Applied, "{report:?}");
    assert_eq!(report.writes.len(), 1);
    assert_eq!((report.writes[0].number, report.writes[0].to), (19, 0));
    assert_eq!(modem.eeprom[&18], 0, "LBT_RSSI untouched");
    assert_eq!(modem.eeprom[&19], 0);
}

#[test]
fn forwarding_table_in_403_arrow_format_is_parsed_per_entry() {
    let empty = vec!["0:0->0,1:0->0,2:0->0,3:0->0,4:0->0,5:0->0,6:0->0,7:0->0,8:0->0,9:0->0, | FwdBuff=13320".to_string()];
    assert!(!forwarding_has_entries(&empty), "all-zero slots are an empty table");
    let used = vec!["0:0->0,1:4->2,2:0->0, | FwdBuff=13320".to_string()];
    assert!(forwarding_has_entries(&used));
    assert!(forwarding_has_entries(&["AT+F1=4,2".to_string()]));
    assert!(!forwarding_has_entries(&["0,0".to_string()]));
}

/// RF BOARD_HEALTH words as RFBoard26 `rfd_status_words` packs them.
fn rf_health(w13: u32, w15: u32, w22: u32, uptime: u32) -> Vec<u32> {
    let mut words = vec![0u32; 24];
    words[0] = 1;
    words[1] = 1 | 8;
    words[2] = uptime;
    words[4] = 1000;
    words[13] = w13;
    words[15] = w15;
    words[22] = w22;
    words
}

#[test]
fn rf_board_status_block_decodes() {
    // Applied, ATI5 names, fix, flags saved|rebooted|resumed, S5=10, RTSCTS 0.
    let w13 = 1 | (2 << 4) | (2 << 10) | (0x07 << 16) | (10 << 24);
    let status = decode_rf_status(&rf_health(
        w13,
        3968 | (50 << 16),
        32768 | (64 << 16) | (10 << 24),
        5,
    ))
    .unwrap();
    assert_eq!(status.outcome, Outcome::Applied);
    assert_eq!(status.map_source, MapSource::Ati5Names);
    assert_eq!(status.mode, Mode::Fix);
    assert!(status.eeprom_saved && status.rebooted && status.resumed_with_ato);
    assert!(!status.needs_attention && !status.pending);
    assert_eq!(
        (
            status.air_speed,
            status.max_data,
            status.max_retries,
            status.global_retries
        ),
        (Some(64), Some(3968), Some(10), Some(10))
    );
    assert_eq!(
        (status.serial_break_ms, status.dest_id, status.rtscts),
        (Some(50), Some(32768), Some(0))
    );
    // Unread fields, no response.
    let none = decode_rf_status(&rf_health(
        1 | (6 << 4) | (2 << 8) | (2 << 10) | (3 << 13) | (0x40 << 16) | (0xFF << 24),
        u32::MAX,
        u32::MAX,
        5,
    ))
    .unwrap();
    assert_eq!(none.outcome, Outcome::NoResponse);
    assert!(none.needs_attention);
    assert_eq!(
        (none.max_data, none.rtscts, none.air_speed),
        (None, None, None)
    );
    // Older RF firmware (zero words) and other boards' flag-owned words.
    assert!(decode_rf_status(&rf_health(0, 0, 0, 5)).is_none());
    let mut adc = rf_health(w13, 0, 0, 5);
    adc[1] |= 32;
    assert!(decode_rf_status(&adc).is_none());
}

/// RF firmware that verified this profile on an earlier boot skips the check
/// (flag 0x80, outcome Skipped, registers unread): shown, never an attention
/// case, and no S5 readback this boot. Pending on the same boot wins.
#[test]
fn rf_board_verified_on_an_earlier_boot_is_not_an_attention_case() {
    let unread = 1 | (2 << 8) | (2 << 10) | (3 << 13) | (0xFF << 24);
    let verified = decode_rf_status(&rf_health(unread | (0x80 << 16), u32::MAX, u32::MAX, 5)).unwrap();
    assert!(verified.verified_on_earlier_boot && !verified.needs_attention && !verified.pending);
    assert_eq!(verified.outcome, Outcome::Skipped);
    assert_eq!(verified.global_retries, None, "no readback this boot");
    assert!(verified.summary.contains("already verified the profile on an earlier boot"), "{}", verified.summary);
    let repending = decode_rf_status(&rf_health(unread | ((0x80 | 0x20) << 16), u32::MAX, u32::MAX, 70)).unwrap();
    assert!(repending.pending && !repending.verified_on_earlier_boot);
    // RF 7067e57 checks only at boot, before it joins the network: the
    // summary never says the check waits for a station flight state.
    assert!(repending.summary.contains("at boot, before it joins the network"), "{}", repending.summary);
    assert!(!repending.summary.contains("once a flight state"), "{}", repending.summary);
    let gate = decode_rf_status(&rf_health(unread | (0x10 << 16), u32::MAX, u32::MAX, 5)).unwrap();
    assert_eq!(gate.summary, "RF modem not checked this boot: RF's flight state was past Idle (nothing reached the modem)");
    // Stopped by a flight-state change after saving: saved, not verified.
    let stopped = decode_rf_status(&rf_health(1 | (5 << 4) | (2 << 10) | ((0x10 | 0x01) << 16) | (1 << 24), 3968 | (10 << 16), 32768 | (64 << 16) | (10 << 24), 9)).unwrap();
    assert!(stopped.needs_attention && stopped.eeprom_saved && stopped.skipped_by_flight_gate);
    assert_eq!(stopped.summary, "RF modem check stopped by a flight-state change after saving; not verified");
}

#[test]
fn rf_board_check_notifies_once_per_result_and_again_after_reboot() {
    *RF_BOARD.lock().unwrap() = None;
    let pending = rf_health(1 | (2 << 10) | (0x20 << 16), 0, 0, 1);
    assert_eq!(observe_rf_board_health(&pending), None, "still running");
    let matched = rf_health(
        1 | (1 << 4) | (2 << 10) | (0x04 << 16) | (10 << 24),
        3968 | (50 << 16),
        32768 | (64 << 16) | (10 << 24),
        9,
    );
    let notice = observe_rf_board_health(&matched).unwrap();
    assert!(
        notice.starts_with("RF radio modem check passed"),
        "{notice}"
    );
    let mut later = matched.clone();
    later[2] = 20;
    assert_eq!(
        observe_rf_board_health(&later),
        None,
        "same boot, same result"
    );
    let mut rebooted = matched.clone();
    rebooted[2] = 3;
    assert!(
        observe_rf_board_health(&rebooted).is_some(),
        "new boot reports again"
    );
    let mismatch = rf_health(
        1 | (3 << 4) | (2 << 10) | (0x44 << 16) | (0 << 24),
        3968 | (50 << 16),
        32768 | (64 << 16) | (10 << 24),
        30,
    );
    let notice = observe_rf_board_health(&mismatch).unwrap();
    assert!(
        notice.starts_with("RF radio modem check needs attention"),
        "{notice}"
    );
    // A flight-gate skip is shown in the card but raises no notification.
    let skipped = rf_health(
        1 | (2 << 8) | (2 << 10) | (3 << 13) | (0x10 << 16) | (0xFF << 24),
        u32::MAX,
        u32::MAX,
        2,
    );
    assert_eq!(observe_rf_board_health(&skipped), None);
    let status = rf_board_status().unwrap();
    assert!(status.skipped_by_flight_gate && status.summary.contains("flight state"));
    *RF_BOARD.lock().unwrap() = None;
}

#[test]
fn reset_reboots_the_modem_without_writing_settings() {
    let mut modem = SimModem::new();
    // A RAM-only change (never saved) is discarded by the reboot.
    modem.ram.insert(6, 20);
    let report = run_reset(&mut modem, TIMING, &open);
    assert_eq!(report.outcome, Outcome::Reset, "{report:?}");
    assert_eq!(report.mode, Mode::Reset);
    assert!(report.rebooted && !report.eeprom_saved && report.writes.is_empty());
    assert!(!report.needs_attention());
    assert_eq!(modem.eeprom_writes, 0);
    modem.tick();
    assert_eq!(modem.state, State::Data, "ATZ boots into data mode");
    assert_eq!(modem.ram[&6], 50, "EEPROM settings reloaded");
    assert!(report.duration_ms < 12_000, "{}", report.duration_ms);
}

#[test]
fn reset_of_a_silent_modem_reports_no_response() {
    let mut modem = SimModem::new();
    modem.dead = true;
    let report = run_reset(&mut modem, TIMING, &open);
    assert_eq!(report.outcome, Outcome::NoResponse, "{report:?}");
    assert!(report.summary.contains("closed and reopened"));
    assert!(report.needs_attention() && !report.rebooted);
    assert_eq!(modem.eeprom_writes, 0);
}

#[test]
fn reset_is_refused_by_the_flight_gate() {
    let mut modem = SimModem::new();
    let blocked = || Err::<(), _>("station is in flight state Ascent".to_string());
    let report = run_reset(&mut modem, TIMING, &blocked);
    assert_eq!(report.outcome, Outcome::Skipped);
    assert!(report.summary.contains("Ascent"));
    assert_eq!(modem.written, 0, "not even +++ may be sent");
    // The gate closing after command mode is entered: no ATZ, data mode resumed.
    let mut modem = SimModem::new();
    let calls = std::cell::Cell::new(0);
    let closing = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 { Ok(()) } else { Err("a sequence is running".to_string()) }
    };
    let report = run_reset(&mut modem, TIMING, &closing);
    assert_eq!(report.outcome, Outcome::Skipped, "{report:?}");
    assert!(!report.rebooted && report.data_mode == DataMode::Resumed);
    assert_eq!(modem.state, State::Data);
}

#[test]
fn reset_is_never_a_startup_mode() {
    assert_eq!(Mode::parse("reset"), None);
}

/// The embedded default profile is the deployed configuration: a default
/// build expects S5=1 and writes it (link-wide registers included) with no
/// environment flag. Opt-in profiles still need GS_RFD_PROFILE_ROLLED_OUT.
#[test]
fn default_build_expects_and_applies_one_broadcast_retry() {
    let profile = super::super::rfd_profile::Profile::parse(include_bytes!("../../comms/rfd-profile.json")).unwrap();
    let plan = plan_for_profile(&profile);
    let s5 = plan.regs.iter().find(|e| e.number == 5).unwrap();
    assert_eq!(s5.value, Some(1));
    assert_eq!(s5.policy, Policy::Fix, "the default profile is applied automatically");
    let mut modem = SimModem::new();
    let report = run(&mut modem, &plan, Trigger::Startup, Mode::Fix, TIMING, &open, true);
    assert_eq!(report.outcome, Outcome::Applied, "{report:?}");
}

/// Bench Pi: both modems still at S5=10 while the GS paced for the profile's
/// S5=1. The host must pace for the copies the air actually carries.
#[test]
fn pacing_follows_the_modems_actual_global_retries() {
    assert_eq!(link_share_cap_percent(64_000, 30_000, 1), None, "S5=1 fits the profile");
    // 64000 * 0.94 / 11 = 5469 bit/s of 30000.
    assert_eq!(link_share_cap_percent(64_000, 30_000, 10), Some(18));
    assert_eq!(link_share_cap_percent(64_000, 30_000, 100), Some(1));
}

/// Until a readback proves otherwise the station assumes the deployed S5=10:
/// in the faithful bench reproduction neither the GS (simulated link) nor
/// the bench RF reported S5, the GS paced 30 kbit/s into a link carrying
/// about 5.8 kbit/s, and its modem dropped 59 KB in 90 s.
#[test]
fn unproven_link_paces_for_the_deployed_global_retries() {
    assert_eq!(UNPROVEN_GLOBAL_RETRIES, 10);
    if link_global_retries().is_none() {
        assert_eq!(pacing_global_retries(), 10);
        assert_eq!(link_share_cap_percent(64_000, 30_000, pacing_global_retries()), Some(18));
    }
}

/// The bench deploy runs in HITL mode with the real AV-bay radio. The modem
/// configuration check was skipped there by default, so the station's own
/// RFD kept GLOBAL_RETRIES 10 whatever the RF board was reflashed to. HITL
/// applies the profile like normal mode; only testing mode, simulator builds
/// and simulated links skip it.
/// A simulator build runs the check only against a simulated modem that
/// answers the command mode; otherwise S5 stays unproven and pacing assumes
/// 10. Testing mode never runs it.
#[test]
fn simulator_builds_check_only_a_simulated_modem() {
    assert_eq!(default_mode(false, true, false), Mode::Skip);
    assert_eq!(default_mode(false, true, true), Mode::Fix);
    assert_eq!(default_mode(true, true, true), Mode::Skip);
    assert_eq!(default_mode(true, false, false), Mode::Skip);
    assert_eq!(default_mode(false, false, false), Mode::Fix);
}

#[test]
fn hitl_applies_the_modem_profile_like_normal_mode() {
    crate::mode_suite!(Hitl);
    if std::env::var_os("GS_RFD_MODEM_CHECK").is_none() && !cfg!(feature = "firmware_sim") {
        assert_eq!(configured_mode(), Mode::Fix);
    }
}

/// The V1.1 number fallback must never be used on ASYNC 4.x: its map differs
/// above S5 (S18 is LBT_RSSI there, not RTSCTS). A 4.03 modem that lists no
/// names is unconfirmed (nothing written); a 2.x modem with V1.1 values still
/// fingerprints.
#[test]
fn an_async_4_modem_without_register_names_is_never_fingerprinted() {
    let plan = plan();
    let v11 = |n: u8, v: i64| (n, None, Some(v));
    let snap = Snapshot {
        named: Vec::new(),
        values: vec![v11(1, 115), v11(2, 64), v11(3, 3968), v11(11, 32768), v11(18, 0)],
    };
    assert_eq!(map_source(Some("RFD ASYNC 4.03 on RFD900X2"), &snap, &plan), MapSource::Unconfirmed);
    assert_eq!(map_source(Some("RFD ASYNC 2.69 on RFD900X"), &snap, &plan), MapSource::Fingerprint);
}
