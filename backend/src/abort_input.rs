//! Bias supervision for the physical abort button input (BCM GPIO 7).
//!
//! Wiring assumed by the code (no schematic is kept in this repository): a
//! normally-open push button between GPIO 7 and 3V3, read active-high with the
//! pad's internal pulldown holding the released level low. With that wiring a
//! broken wire reads "not pressed" and cannot be detected; the pulldown can
//! only hold the released level. A pad that loses its pull reverts to the
//! board default (GPIO 0-8 default to pullup on Raspberry Pi), which reads as
//! the abort level and hides any real press while it lasts.
//!
//! This module requests the bias explicitly, writes it again after any line
//! re-request, reads back the bias the pad actually has where the platform
//! allows it, and reports contradictions as station warnings.

use crate::gpio::{BiasReadback, GpioPins, InputBias};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Bias that matches the assumed normally-open-to-3V3 wiring.
pub(crate) const ABORT_INPUT_BIAS: InputBias = InputBias::PullDown;
/// Abort is the high level.
pub(crate) const ABORT_ACTIVE_HIGH: bool = true;
/// How often the poll thread reads the pad bias back.
pub(crate) const SELF_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// A level held against the requested pull for this long is reported.
pub(crate) const HELD_AGAINST_BIAS_WARN_AFTER: Duration = Duration::from_secs(5);

/// The operations the supervisor needs from one GPIO input line.
pub(crate) trait BiasedInputLine {
    fn pin(&self) -> u8;
    /// Configure the input with this bias, written explicitly.
    fn request(&self, bias: InputBias) -> Result<(), String>;
    /// Write the requested bias to the pad again.
    fn reassert_bias(&self) -> Result<InputBias, String>;
    /// Release and request the line again; the bias is written after the request.
    fn reopen(&self) -> Result<InputBias, String>;
    fn read_level(&self) -> Result<bool, String>;
    fn read_bias(&self) -> Result<BiasReadback, String>;
}

/// Production line: one pin of the shared GPIO owner.
pub(crate) struct GpioLine<'a> {
    pub(crate) gpio: &'a GpioPins,
    pub(crate) pin: u8,
}

impl BiasedInputLine for GpioLine<'_> {
    fn pin(&self) -> u8 {
        self.pin
    }
    fn request(&self, bias: InputBias) -> Result<(), String> {
        self.gpio
            .setup_input_with_bias(self.pin, bias)
            .map_err(|e| e.to_string())
    }
    fn reassert_bias(&self) -> Result<InputBias, String> {
        self.gpio
            .reassert_input_bias(self.pin)
            .map_err(|e| e.to_string())
    }
    fn reopen(&self) -> Result<InputBias, String> {
        self.gpio.reopen_input(self.pin).map_err(|e| e.to_string())
    }
    fn read_level(&self) -> Result<bool, String> {
        self.gpio
            .read_input_pin(self.pin)
            .map_err(|e| e.to_string())
    }
    fn read_bias(&self) -> Result<BiasReadback, String> {
        self.gpio
            .read_input_bias(self.pin)
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    /// Log only.
    Info(String),
    /// Station warning.
    Warning(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Fault {
    BiasContradiction,
    BiasUnreadable,
    LevelUnreadable,
    HeldAgainstBias,
}

pub(crate) struct AbortInputMonitor {
    requested: InputBias,
    active_high: bool,
    active: HashSet<Fault>,
    against_bias_since: Option<Instant>,
    last_level_reopen: Option<Instant>,
    readback_supported: bool,
}

#[derive(Clone, Copy)]
enum Repair {
    Reassert,
    Reopen,
}

impl Repair {
    fn label(self) -> &'static str {
        match self {
            Repair::Reassert => "re-writing the bias",
            Repair::Reopen => "re-requesting the line",
        }
    }
}

fn level_name(high: bool) -> &'static str {
    if high { "high" } else { "low" }
}

impl AbortInputMonitor {
    pub(crate) fn new(requested: InputBias, active_high: bool) -> Self {
        Self {
            requested,
            active_high,
            active: HashSet::new(),
            against_bias_since: None,
            last_level_reopen: None,
            readback_supported: true,
        }
    }

    /// Level the requested pull holds when nothing drives the pin.
    fn idle_level(&self) -> Option<bool> {
        match self.requested {
            InputBias::PullDown => Some(false),
            InputBias::PullUp => Some(true),
            InputBias::Off => None,
        }
    }

    fn describe_level(&self, high: bool) -> String {
        let meaning = if high == self.active_high {
            "abort"
        } else {
            "released"
        };
        format!("{} ({meaning})", level_name(high))
    }

    fn enter(&mut self, fault: Fault) -> bool {
        self.active.insert(fault)
    }

    fn leave(&mut self, fault: Fault) -> bool {
        self.active.remove(&fault)
    }

    /// Request the line with the explicit bias and report the bias the pad has.
    pub(crate) fn open(&mut self, line: &dyn BiasedInputLine) -> Result<Vec<Report>, String> {
        line.request(self.requested)?;
        let pin = line.pin();
        let mut reports = Vec::new();
        match line.read_bias() {
            Ok(BiasReadback::Applied { bias, source }) if bias == self.requested => {
                reports.push(Report::Info(format!(
                    "GPIO abort button configured on BCM GPIO {pin} as active-{} input; requested {}, pad reports {} ({source})",
                    level_name(self.active_high),
                    self.requested.label(),
                    bias.label(),
                )));
            }
            Ok(BiasReadback::Unverifiable(why)) => {
                self.readback_supported = false;
                reports.push(Report::Info(format!(
                    "GPIO abort button configured on BCM GPIO {pin} as active-{} input; requested {}; applied bias not verified: {why}",
                    level_name(self.active_high),
                    self.requested.label(),
                )));
            }
            Ok(BiasReadback::Applied { .. }) | Err(_) => {
                reports.push(Report::Info(format!(
                    "GPIO abort button configured on BCM GPIO {pin} as active-{} input; requested {}",
                    level_name(self.active_high),
                    self.requested.label(),
                )));
                reports.extend(self.check_bias(line));
            }
        }
        Ok(reports)
    }

    /// Read the pad bias back; on contradiction or failure write it again.
    pub(crate) fn check_bias(&mut self, line: &dyn BiasedInputLine) -> Vec<Report> {
        if !self.readback_supported {
            return Vec::new();
        }
        let pin = line.pin();
        let wanted = self.requested.label();
        let mut reports = Vec::new();
        match line.read_bias() {
            Ok(BiasReadback::Applied { bias, .. }) if bias == self.requested => {
                if self.leave(Fault::BiasContradiction) | self.leave(Fault::BiasUnreadable) {
                    reports.push(Report::Info(format!(
                        "GPIO abort input BCM GPIO {pin}: pad reports {wanted} again"
                    )));
                }
            }
            Ok(BiasReadback::Applied { bias, source }) => {
                if self.enter(Fault::BiasContradiction) {
                    reports.push(Report::Warning(format!(
                        "Abort button input BCM GPIO {pin}: requested {wanted}, pad reports {} ({source}); writing {wanted} again",
                        bias.label()
                    )));
                }
                reports.extend(self.repair(line, Repair::Reassert));
            }
            Ok(BiasReadback::Unverifiable(why)) => {
                self.readback_supported = false;
                reports.push(Report::Info(format!(
                    "GPIO abort input BCM GPIO {pin}: bias readback unavailable ({why}); checking level only"
                )));
            }
            Err(error) => {
                if self.enter(Fault::BiasUnreadable) {
                    reports.push(Report::Warning(format!(
                        "Abort button input BCM GPIO {pin}: cannot read the pad bias back ({error}); requesting the line again with {wanted}"
                    )));
                }
                reports.extend(self.repair(line, Repair::Reopen));
            }
        }
        reports
    }

    /// Write the bias (or re-request the line), read it back and report what
    /// the pad now has. A re-assert that does not take escalates to a re-request.
    fn repair(&mut self, line: &dyn BiasedInputLine, how: Repair) -> Vec<Report> {
        let pin = line.pin();
        let wanted = self.requested.label();
        let action = match how {
            Repair::Reassert => line.reassert_bias(),
            Repair::Reopen => line.reopen(),
        };
        if let Err(error) = action {
            return vec![Report::Info(format!(
                "GPIO abort input BCM GPIO {pin}: {} failed: {error}",
                how.label()
            ))];
        }
        match line.read_bias() {
            Ok(BiasReadback::Applied { bias, source }) if bias == self.requested => {
                self.leave(Fault::BiasContradiction);
                self.leave(Fault::BiasUnreadable);
                vec![Report::Info(format!(
                    "GPIO abort input BCM GPIO {pin}: after {}, pad now reports {} ({source})",
                    how.label(),
                    bias.label()
                ))]
            }
            Ok(BiasReadback::Applied { bias, source }) => {
                let mut reports = vec![Report::Info(format!(
                    "GPIO abort input BCM GPIO {pin}: after {}, pad still reports {} ({source}), wanted {wanted}",
                    how.label(),
                    bias.label()
                ))];
                if matches!(how, Repair::Reassert) {
                    reports.extend(self.repair(line, Repair::Reopen));
                }
                reports
            }
            Ok(BiasReadback::Unverifiable(_)) => Vec::new(),
            Err(error) => vec![Report::Info(format!(
                "GPIO abort input BCM GPIO {pin}: after {}, bias readback failed: {error}",
                how.label()
            ))],
        }
    }

    /// Account for one level read made by the abort poll loop.
    pub(crate) fn observe_level(
        &mut self,
        line: &dyn BiasedInputLine,
        now: Instant,
        level: &Result<bool, String>,
    ) -> Vec<Report> {
        let pin = line.pin();
        let wanted = self.requested.label();
        let mut reports = Vec::new();
        let high = match level {
            Err(error) => {
                if self.enter(Fault::LevelUnreadable) {
                    reports.push(Report::Warning(format!(
                        "Abort button input BCM GPIO {pin} cannot be read ({error}); requesting the line again with {wanted}"
                    )));
                }
                // The poll loop runs every few ms; re-request at most once per
                // self-check interval while the failure persists.
                let due = self
                    .last_level_reopen
                    .is_none_or(|last| now.saturating_duration_since(last) >= SELF_CHECK_INTERVAL);
                if due {
                    self.last_level_reopen = Some(now);
                    reports.extend(self.repair(line, Repair::Reopen));
                }
                return reports;
            }
            Ok(high) => *high,
        };
        if self.leave(Fault::LevelUnreadable) {
            reports.push(Report::Info(format!(
                "GPIO abort input BCM GPIO {pin} is readable again"
            )));
        }
        let Some(idle) = self.idle_level() else {
            return reports;
        };
        if high == idle {
            self.against_bias_since = None;
            if self.leave(Fault::HeldAgainstBias) {
                reports.push(Report::Info(format!(
                    "GPIO abort input BCM GPIO {pin} reads {} again",
                    self.describe_level(high)
                )));
            }
            return reports;
        }
        let since = *self.against_bias_since.get_or_insert(now);
        let held = now.saturating_duration_since(since);
        if held >= HELD_AGAINST_BIAS_WARN_AFTER && self.enter(Fault::HeldAgainstBias) {
            reports.push(Report::Warning(format!(
                "Abort button input BCM GPIO {pin} has read {} for {} s with {wanted} requested: button held, wiring fault, or the {wanted} is not applied. A new press cannot be seen until it reads {} again; writing {wanted} again",
                self.describe_level(high),
                held.as_secs(),
                level_name(idle),
            )));
            reports.extend(self.repair(line, Repair::Reassert));
        }
        reports
    }

    /// Bias readback followed by a level read.
    pub(crate) fn self_check(&mut self, line: &dyn BiasedInputLine, now: Instant) -> Vec<Report> {
        let mut reports = self.check_bias(line);
        let level = line.read_level();
        reports.extend(self.observe_level(line, now, &level));
        reports
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Fake line: `applied` is the bias the pad holds; requests, re-asserts and
    /// re-opens write the requested bias into it and are counted.
    pub(crate) struct FakeLine {
        pub requested: Cell<Option<InputBias>>,
        pub applied: Cell<Option<InputBias>>,
        pub requests: RefCell<Vec<InputBias>>,
        pub reasserts: Cell<u32>,
        pub reopens: Cell<u32>,
        pub level: Cell<bool>,
        pub level_errors: Cell<u32>,
        pub bias_errors: Cell<u32>,
        pub readback: Cell<bool>,
        /// When set, a re-assert does not change the pad (bias overridden elsewhere).
        pub reassert_ineffective: Cell<bool>,
    }

    impl FakeLine {
        pub(crate) fn new() -> Self {
            Self {
                requested: Cell::new(None),
                applied: Cell::new(None),
                requests: RefCell::new(Vec::new()),
                reasserts: Cell::new(0),
                reopens: Cell::new(0),
                level: Cell::new(false),
                level_errors: Cell::new(0),
                bias_errors: Cell::new(0),
                readback: Cell::new(true),
                reassert_ineffective: Cell::new(false),
            }
        }
        fn requested_or_err(&self) -> Result<InputBias, String> {
            self.requested
                .get()
                .ok_or_else(|| "not requested".to_string())
        }
    }

    impl BiasedInputLine for FakeLine {
        fn pin(&self) -> u8 {
            7
        }
        fn request(&self, bias: InputBias) -> Result<(), String> {
            self.requests.borrow_mut().push(bias);
            self.requested.set(Some(bias));
            self.applied.set(Some(bias));
            Ok(())
        }
        fn reassert_bias(&self) -> Result<InputBias, String> {
            let bias = self.requested_or_err()?;
            self.reasserts.set(self.reasserts.get() + 1);
            if !self.reassert_ineffective.get() {
                self.applied.set(Some(bias));
            }
            Ok(bias)
        }
        fn reopen(&self) -> Result<InputBias, String> {
            let bias = self.requested_or_err()?;
            self.reopens.set(self.reopens.get() + 1);
            self.applied.set(Some(bias));
            Ok(bias)
        }
        fn read_level(&self) -> Result<bool, String> {
            if self.level_errors.get() > 0 {
                self.level_errors.set(self.level_errors.get() - 1);
                return Err("line lost".into());
            }
            Ok(self.level.get())
        }
        fn read_bias(&self) -> Result<BiasReadback, String> {
            if self.bias_errors.get() > 0 {
                self.bias_errors.set(self.bias_errors.get() - 1);
                return Err("RP1 PADS read 0xFFFFFFFF".into());
            }
            if !self.readback.get() {
                return Ok(BiasReadback::Unverifiable("no readback"));
            }
            self.applied
                .get()
                .map(|bias| BiasReadback::Applied {
                    bias,
                    source: "fake pad",
                })
                .ok_or_else(|| "not configured".into())
        }
    }

    fn warnings(reports: &[Report]) -> Vec<&str> {
        reports
            .iter()
            .filter_map(|r| match r {
                Report::Warning(w) => Some(w.as_str()),
                Report::Info(_) => None,
            })
            .collect()
    }

    fn infos(reports: &[Report]) -> Vec<&str> {
        reports
            .iter()
            .filter_map(|r| match r {
                Report::Info(i) => Some(i.as_str()),
                Report::Warning(_) => None,
            })
            .collect()
    }

    fn opened() -> (AbortInputMonitor, FakeLine, Vec<Report>) {
        let line = FakeLine::new();
        let mut monitor = AbortInputMonitor::new(ABORT_INPUT_BIAS, ABORT_ACTIVE_HIGH);
        let reports = monitor.open(&line).unwrap();
        (monitor, line, reports)
    }

    #[test]
    fn abort_input_requests_pulldown_explicitly_and_logs_the_readback() {
        assert_eq!(ABORT_INPUT_BIAS, InputBias::PullDown);
        let (_monitor, line, reports) = opened();
        assert_eq!(*line.requests.borrow(), vec![InputBias::PullDown]);
        assert!(warnings(&reports).is_empty(), "{reports:?}");
        let info = infos(&reports).join("\n");
        assert!(info.contains("requested pulldown"), "{info}");
        assert!(info.contains("pad reports pulldown (fake pad)"), "{info}");
    }

    #[test]
    fn abort_input_open_reports_when_the_requested_bias_did_not_take() {
        let line = FakeLine::new();
        line.reassert_ineffective.set(true);
        let mut monitor = AbortInputMonitor::new(ABORT_INPUT_BIAS, ABORT_ACTIVE_HIGH);
        // The pad is already at the board default when the request lands and
        // keeps it: simulate by making the request itself not change the pad.
        line.applied.set(Some(InputBias::PullUp));
        struct StickyPullUp<'a>(&'a FakeLine);
        impl BiasedInputLine for StickyPullUp<'_> {
            fn pin(&self) -> u8 {
                7
            }
            fn request(&self, bias: InputBias) -> Result<(), String> {
                self.0.requests.borrow_mut().push(bias);
                self.0.requested.set(Some(bias));
                Ok(())
            }
            fn reassert_bias(&self) -> Result<InputBias, String> {
                self.0.reassert_bias()
            }
            fn reopen(&self) -> Result<InputBias, String> {
                self.0.reopens.set(self.0.reopens.get() + 1);
                self.0.requested_or_err()
            }
            fn read_level(&self) -> Result<bool, String> {
                self.0.read_level()
            }
            fn read_bias(&self) -> Result<BiasReadback, String> {
                self.0.read_bias()
            }
        }
        let reports = monitor.open(&StickyPullUp(&line)).unwrap();
        let warning = warnings(&reports).join("\n");
        assert!(
            warning.contains("requested pulldown") && warning.contains("pad reports pullup"),
            "{reports:?}"
        );
    }

    #[test]
    fn abort_input_bias_is_reapplied_after_a_level_read_error_reopens_the_line() {
        let (mut monitor, line, _) = opened();
        let now = Instant::now();
        // The pad also lost its pull at the same moment the line failed.
        line.applied.set(Some(InputBias::PullUp));
        line.level_errors.set(2);
        let first = line.read_level();
        let reports = monitor.observe_level(&line, now, &first);
        assert_eq!(line.reopens.get(), 1, "line must be re-requested");
        assert_eq!(line.applied.get(), Some(InputBias::PullDown));
        let warning = warnings(&reports).join("\n");
        assert!(warning.contains("BCM GPIO 7"), "{warning}");
        assert!(warning.contains("pulldown"), "{warning}");
        // A repeated failure inside the interval does not re-request yet; one
        // after the interval re-requests again, without repeating the warning.
        let second = line.read_level();
        assert!(monitor.observe_level(&line, now, &second).is_empty());
        assert_eq!(line.reopens.get(), 1);
        let reports = monitor.observe_level(&line, now + SELF_CHECK_INTERVAL, &second);
        assert_eq!(line.reopens.get(), 2);
        assert!(warnings(&reports).is_empty(), "{reports:?}");
        // Recovery is logged.
        let ok = line.read_level();
        let reports = monitor.observe_level(&line, now, &ok);
        assert!(
            infos(&reports).join("\n").contains("readable again"),
            "{reports:?}"
        );
    }

    #[test]
    fn abort_input_bias_is_reapplied_after_a_bias_readback_error() {
        let (mut monitor, line, _) = opened();
        line.bias_errors.set(1);
        line.applied.set(Some(InputBias::Off));
        let reports = monitor.check_bias(&line);
        assert_eq!(line.reopens.get(), 1);
        assert_eq!(line.applied.get(), Some(InputBias::PullDown));
        assert_eq!(warnings(&reports).len(), 1, "{reports:?}");
        assert!(
            infos(&reports).join("\n").contains("now reports pulldown"),
            "{reports:?}"
        );
    }

    #[test]
    fn abort_input_bias_contradiction_is_warned_once_and_reasserted() {
        let (mut monitor, line, _) = opened();
        line.applied.set(Some(InputBias::PullUp));
        let reports = monitor.check_bias(&line);
        let warning = warnings(&reports).join("\n");
        assert!(warning.contains("requested pulldown"), "{warning}");
        assert!(warning.contains("pad reports pullup"), "{warning}");
        assert_eq!(line.reasserts.get(), 1);
        assert_eq!(line.applied.get(), Some(InputBias::PullDown));
        assert!(
            infos(&reports).join("\n").contains("now reports pulldown"),
            "{reports:?}"
        );
        // Healthy afterwards: no further warning or write.
        let reports = monitor.check_bias(&line);
        assert!(warnings(&reports).is_empty(), "{reports:?}");
        assert_eq!(line.reasserts.get(), 1);
        // A second glitch is a new event and is reported again.
        line.applied.set(Some(InputBias::Off));
        assert_eq!(warnings(&monitor.check_bias(&line)).len(), 1);
    }

    #[test]
    fn abort_input_contradiction_that_survives_reassert_requests_the_line_again() {
        let (mut monitor, line, _) = opened();
        line.reassert_ineffective.set(true);
        line.applied.set(Some(InputBias::PullUp));
        let reports = monitor.check_bias(&line);
        assert_eq!(line.reasserts.get(), 1);
        assert_eq!(line.reopens.get(), 1);
        assert_eq!(line.applied.get(), Some(InputBias::PullDown));
        assert!(!warnings(&reports).is_empty());
    }

    #[test]
    fn abort_input_level_held_against_the_pulldown_is_reported() {
        let (mut monitor, line, _) = opened();
        line.readback.set(false);
        let start = Instant::now();
        line.level.set(true);
        let high = Ok(true);
        assert!(warnings(&monitor.observe_level(&line, start, &high)).is_empty());
        assert!(
            warnings(&monitor.observe_level(
                &line,
                start + HELD_AGAINST_BIAS_WARN_AFTER - Duration::from_millis(1),
                &high
            ))
            .is_empty()
        );
        let reports = monitor.observe_level(&line, start + HELD_AGAINST_BIAS_WARN_AFTER, &high);
        let warning = warnings(&reports).join("\n");
        assert!(warning.contains("high (abort)"), "{warning}");
        assert!(warning.contains("pulldown"), "{warning}");
        assert_eq!(line.reasserts.get(), 1, "pulldown is written again");
        // Not repeated while it stays high.
        assert!(
            warnings(&monitor.observe_level(
                &line,
                start + HELD_AGAINST_BIAS_WARN_AFTER * 3,
                &high
            ))
            .is_empty()
        );
        let reports =
            monitor.observe_level(&line, start + HELD_AGAINST_BIAS_WARN_AFTER * 3, &Ok(false));
        assert!(
            infos(&reports).join("\n").contains("released"),
            "{reports:?}"
        );
    }

    #[test]
    fn abort_input_without_readback_says_so_instead_of_claiming_the_bias() {
        let line = FakeLine::new();
        line.readback.set(false);
        let mut monitor = AbortInputMonitor::new(ABORT_INPUT_BIAS, ABORT_ACTIVE_HIGH);
        let reports = monitor.open(&line).unwrap();
        let info = infos(&reports).join("\n");
        assert!(info.contains("not verified"), "{info}");
        assert!(!info.contains("pad reports"), "{info}");
    }

    #[cfg(not(feature = "raspberry_pi"))]
    #[test]
    fn abort_input_gpio_line_writes_the_bias_on_request_reassert_and_reopen() {
        let gpio = GpioPins::new_isolated();
        let line = GpioLine {
            gpio: &gpio,
            pin: 7,
        };
        let mut monitor = AbortInputMonitor::new(ABORT_INPUT_BIAS, ABORT_ACTIVE_HIGH);
        let reports = monitor.open(&line).unwrap();
        assert!(warnings(&reports).is_empty(), "{reports:?}");
        assert_eq!(
            gpio.bias_counters_for_test(7),
            (Some(InputBias::PullDown), 1, 0)
        );
        gpio.set_applied_bias_for_test(7, InputBias::PullUp);
        assert_eq!(warnings(&monitor.check_bias(&line)).len(), 1);
        assert_eq!(
            gpio.bias_counters_for_test(7),
            (Some(InputBias::PullDown), 2, 0)
        );
        gpio.set_applied_bias_for_test(7, InputBias::Off);
        line.reopen().unwrap();
        assert_eq!(
            gpio.bias_counters_for_test(7),
            (Some(InputBias::PullDown), 3, 1)
        );
        assert_eq!(
            line.read_bias().unwrap(),
            BiasReadback::Applied {
                bias: InputBias::PullDown,
                source: "simulated pad (no GPIO hardware)"
            }
        );
    }
}
