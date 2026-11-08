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
            "ATI" => "RFD ASYNC 4.03 on RFD900X2\r\n".into(),
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
    let mut modem = SimModem::new();
    modem.ati5_names = false;
    let report = check(&mut modem, Mode::Fix);
    assert_eq!(report.map_source, MapSource::Fingerprint);
    assert_eq!(report.outcome, Outcome::Applied, "{report:?}");
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
