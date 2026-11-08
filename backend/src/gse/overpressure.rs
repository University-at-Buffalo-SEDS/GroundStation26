//! Tank overpressure protection on the valve-board psi lane.
//!
//! Independent of every sequence, mode (HITL, test-fire, flight) and
//! pause/hold state before Launch:
//! - at or above `overpressure_relief_psi` (default 850): close Nitrous, close
//!   Nitrogen, open Dump, open Vent, send the Actuator Board overpressure
//!   abort (automatic ACTUATOR_COMMAND 16, all AB outputs off), and latch an
//!   error;
//! - at or above `overpressure_abort_psi` (default 950): additionally open the
//!   main (pilot) valve, and send the Actuator Board overpressure abort at once
//!   if this latch has not sent it yet. After `overpressure_pilot_ms` (default
//!   3 s) the station commands the pilot closed and then dispatches an
//!   automatic Abort. The broadcast Abort is sent only after the pilot window,
//!   so neither the station's own abort fence (which refuses non-relief
//!   outputs once the abort latch is set) nor the Valve Board's abort safe
//!   state (which closes the pilot) can cut the window short. The Actuator
//!   Board command is AB-only and never touches the main valve, so it does
//!   not wait for the window.
//!
//! A threshold counts only when readings stay at or above it for
//! `SUSTAIN_MS`. A physically impossible step between readings (an
//! unconnected, floating transducer) is a sensor fault, reported to the
//! operator and in the status, never a trip; readings must then settle for
//! `SETTLE_MS` before they count again. The Valve Board applies the same rules.
//!
//! Once Launch is admitted or Fire is committed (T0 onward) the main valve is
//! already open for firing: the monitor only logs and alarms, never commands.
//! The latch never re-closes anything automatically; the operator
//! acknowledges it after a fresh reading is back below the relief pressure.
use super::*;

/// A threshold trips only when every counted valve-board reading has stayed
/// at or above it for this long, measured on capture timestamps (about 13
/// samples at 50 Hz), so it does not depend on the sample rate. Real tank
/// pressure rises over seconds; a floating (unconnected) transducer input or a
/// solenoid switching glitch lasts tens of milliseconds. A gap longer than
/// this between readings restarts the timer. Same value as the Valve Board.
pub(crate) const SUSTAIN_MS: u64 = 250;
/// A step larger than `MAX_STEP_PSI` per `STEP_MS` (7500 psi/s) between
/// consecutive readings is not tank pressure. It resets the trip timers and
/// raises an implausible-sensor fault, never a trip. Same as the Valve Board.
pub(crate) const MAX_STEP_PSI: f32 = 150.0;
pub(crate) const STEP_MS: u64 = 20;
/// After an implausible step readings count again only once this long has
/// passed with no further implausible step, so a glitch that sits high for a
/// few samples cannot trip. Same as the Valve Board.
pub(crate) const SETTLE_MS: u64 = 1_000;
/// A sensor fault stays reported to the operator this long after the last
/// implausible step, and is notified at most once per this interval.
const SENSOR_FAULT_SHOW_MS: u64 = 60_000;
/// A sample older than this is stale and neither counts nor clears a streak.
const FRESH_MS: u64 = 2000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum AbortStage {
    /// Relief only; abort pressure not reached.
    NotRequired,
    /// Main valve commanded open; Abort follows at `until_ms`.
    PilotOpen { until_ms: u64 },
    /// Main valve commanded closed and Abort dispatched (or refused).
    Aborted,
}

#[derive(Clone, Debug)]
pub(crate) struct Latch {
    pub(crate) peak_psi: f32,
    pub(crate) threshold_psi: f32,
    pub(crate) at_ms: u64,
    pub(crate) message: String,
    /// False when it tripped after T0: alarm only, nothing commanded.
    pub(crate) relief_commanded: bool,
    pub(crate) abort: AbortStage,
    /// The Actuator Board overpressure abort was queued for this latch.
    pub(crate) ab_abort_sent: bool,
}

/// The valve-board tank pressure made a physically impossible step: most
/// likely the transducer is not connected and the ADC input is floating.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SensorFault {
    pub(crate) from_psi: f32,
    pub(crate) to_psi: f32,
    pub(crate) step_ms: u64,
    pub(crate) at_ms: u64,
    /// Implausible steps since the fault was first raised.
    pub(crate) count: u32,
    first_at_ms: u64,
    notified_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Monitor {
    last_sample_at: Option<u64>,
    /// Last fresh reading, the reference for the plausible-rate check.
    last_valid: Option<Sample>,
    /// Capture time since which readings stayed at or above each threshold.
    relief_since: Option<u64>,
    abort_since: Option<u64>,
    /// Capture time of the last implausible step while readings settle.
    settle_from: Option<u64>,
    pub(crate) sensor_fault: Option<SensorFault>,
    pub(crate) latch: Option<Latch>,
    /// The Valve Board's own latch, from its WARNING reports.
    pub(crate) board: Option<BoardReport>,
    /// The Valve Board's latest sensor fault report (F invalid, S stale,
    /// I implausible), shown to the operator; never a trip.
    pub(crate) board_sensor: Option<BoardReport>,
    board_ack_at: Option<u64>,
}

/// The Valve Board enforces the same rules autonomously (fixed 850/950 psi,
/// 3 s; this station's configuration does not change them) and reports them
/// as WARNING text such as "Valve Board overpressure relief: 912 psi [R3]".
/// The tag is the stage (R relief, P main valve open, A Abort, L launch
/// alarm, C acknowledged, F/S/I sensor invalid/stale/implausible) and the
/// trip number. A
/// live latch is re-announced every second.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BoardReport {
    pub(crate) code: char,
    pub(crate) psi: u32,
    pub(crate) trip: u32,
    pub(crate) at_ms: u64,
}
const BOARD_PREFIX: &str = "Valve Board overpressure ";
/// Three missed one-second announcements and more.
const BOARD_FRESH_MS: u64 = 5_000;
/// A report already in flight when the operator acknowledged does not relatch.
const BOARD_ACK_SETTLE_MS: u64 = 3_000;
/// VALVE_COMMAND that acknowledges the board latch; it never moves a valve and
/// the board accepts it only below its relief pressure.
pub(crate) const BOARD_ACK_COMMAND: u8 = 16;

pub(crate) fn parse_board_report(text: &str) -> Option<(char, u32, u32)> {
    let body = text.strip_prefix(BOARD_PREFIX)?.strip_suffix(']')?;
    let open = body.rfind(" [")?;
    let mut tag = body[open + 2..].chars();
    let code = tag.next().filter(|c| "RPALCFSI".contains(*c))?;
    let trip = tag.as_str().parse().ok()?;
    let psi = if code == 'S' {
        0
    } else {
        body[..open]
            .rsplit(' ')
            .find(|word| !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()))?
            .parse()
            .ok()?
    };
    Some((code, psi, trip))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Level {
    Normal,
    Relief(f32),
    Abort(f32),
}

pub(crate) struct Thresholds {
    pub(crate) relief_psi: f32,
    pub(crate) abort_psi: f32,
    pub(crate) pilot_ms: u64,
}
impl Thresholds {
    /// Invalid settings can never disable protection: fall back to defaults.
    pub(crate) fn from_config(config: &Config) -> Self {
        let config = if config.validate_overpressure().is_ok() {
            config.clone()
        } else {
            Config::default()
        };
        Self {
            relief_psi: config.overpressure_relief_psi,
            abort_psi: config.overpressure_abort_psi,
            pilot_ms: config.overpressure_pilot_ms,
        }
    }
}

fn fresh(sample: Option<Sample>, now: u64) -> Option<Sample> {
    sample.filter(|s| s.value.is_finite() && s.at_ms <= now && now - s.at_ms <= FRESH_MS)
}

impl Monitor {
    /// Count each new fresh sample once. Returns the sustained level.
    // Safety catalog (About safety): overpressure-debounce, overpressure-plausibility. Keep backend/src/safety_catalog_rules.rs in step.
    pub(crate) fn observe(&mut self, sample: Option<Sample>, now: u64, t: &Thresholds) -> Level {
        let Some(sample) = fresh(sample, now) else {
            return Level::Normal;
        };
        if self.last_sample_at.is_some_and(|last| sample.at_ms <= last) {
            return Level::Normal;
        }
        self.last_sample_at = Some(sample.at_ms);
        let previous = self.last_valid.replace(sample);
        let gap = previous.map(|p| sample.at_ms - p.at_ms);
        if let (Some(p), Some(gap)) = (previous, gap) {
            let limit = MAX_STEP_PSI * gap.max(STEP_MS) as f32 / STEP_MS as f32;
            if (sample.value - p.value).abs() > limit {
                self.settle_from = Some(sample.at_ms);
                let fault = self.sensor_fault.get_or_insert(SensorFault {
                    from_psi: p.value,
                    to_psi: sample.value,
                    step_ms: gap,
                    at_ms: sample.at_ms,
                    count: 0,
                    first_at_ms: sample.at_ms,
                    notified_at_ms: None,
                });
                fault.from_psi = p.value;
                fault.to_psi = sample.value;
                fault.step_ms = gap;
                fault.at_ms = sample.at_ms;
                fault.count = fault.count.saturating_add(1);
            }
        }
        if self
            .settle_from
            .is_some_and(|from| sample.at_ms - from >= SETTLE_MS)
        {
            self.settle_from = None;
        }
        if self.settle_from.is_some() {
            self.relief_since = None;
            self.abort_since = None;
            return Level::Normal;
        }
        let restart = gap.is_none_or(|gap| gap > SUSTAIN_MS);
        let sustained = |since: &mut Option<u64>, over: bool| {
            if !over {
                *since = None;
                return false;
            }
            if since.is_none() || restart {
                *since = Some(sample.at_ms);
            }
            since.is_some_and(|from| sample.at_ms - from >= SUSTAIN_MS)
        };
        let relief = sustained(&mut self.relief_since, sample.value >= t.relief_psi);
        let abort = sustained(&mut self.abort_since, sample.value >= t.abort_psi);
        if let Some(latch) = &mut self.latch {
            latch.peak_psi = latch.peak_psi.max(sample.value);
        }
        if abort {
            Level::Abort(sample.value)
        } else if relief {
            Level::Relief(sample.value)
        } else {
            Level::Normal
        }
    }
    /// Readings are settling after an implausible step: they are neither
    /// trip nor acknowledgement evidence.
    pub(crate) fn settling(&self) -> bool {
        self.settle_from.is_some()
    }
    /// The station's sensor fault while it is still recent.
    pub(crate) fn sensor_fault_recent(&self, now: u64) -> Option<&SensorFault> {
        self.sensor_fault
            .as_ref()
            .filter(|f| self.settling() || now.saturating_sub(f.at_ms) <= SENSOR_FAULT_SHOW_MS)
    }
    /// Operator notice for a new implausible step, at most once a minute.
    fn take_sensor_notice(&mut self, now: u64) -> Option<String> {
        let fault = self.sensor_fault.as_mut()?;
        if fault.at_ms != self.settle_from? {
            return None;
        }
        if fault
            .notified_at_ms
            .is_some_and(|at| now.saturating_sub(at) < SENSOR_FAULT_SHOW_MS)
        {
            return None;
        }
        fault.notified_at_ms = Some(now);
        Some(sensor_fault_message(fault))
    }
    pub(crate) fn latched(&self) -> bool {
        self.latch.is_some()
    }
    /// The latch owns the valves: no sequence, hold or correction may run.
    pub(crate) fn owns_valves(&self) -> bool {
        self.latch.as_ref().is_some_and(|l| l.relief_commanded)
    }
    /// The Valve Board latch holds dump and vent open right now.
    pub(crate) fn board_owns(&self, now: u64) -> bool {
        self.board.as_ref().is_some_and(|b| {
            matches!(b.code, 'R' | 'P' | 'A') && now.saturating_sub(b.at_ms) <= BOARD_FRESH_MS
        })
    }
}

pub(crate) fn sensor_fault_message(fault: &SensorFault) -> String {
    format!(
        "Tank pressure sensor implausible or not connected: valve-board reading jumped {:.0} -> {:.0} psi in {} ms. Overpressure protection ignores readings until they settle; this is not an overpressure.",
        fault.from_psi, fault.to_psi, fault.step_ms
    )
}

/// Launch admitted (T-clock sticky inhibit) or in flight. Read without the
/// GSE lock held, so this never orders other locks behind it.
fn launched(state: &AppState) -> bool {
    state
        .launch_history
        .lock()
        .unwrap()
        .automatic_actions_inhibited()
        || matches!(
            *state.state.lock().unwrap(),
            FlightState::Launch
                | FlightState::Ascent
                | FlightState::Coast
                | FlightState::Apogee
                | FlightState::Descent
                | FlightState::Reefing
                | FlightState::Landed
                | FlightState::Recovery
        )
}

/// Test-fire Fire commitment is T0 for the overpressure policy.
fn fire_committed(rt: &Runtime) -> bool {
    #[cfg(not(feature = "hitl_mode"))]
    {
        rt.operational.saved.qualification.stage()
            == gse_sequence::qualification::MissionStage::Fire
    }
    #[cfg(feature = "hitl_mode")]
    {
        let _ = rt;
        false
    }
}

const RELIEF: [(usize, bool); 4] = [(4, false), (3, false), (2, true), (1, true)];
/// ACTUATOR_COMMAND payload (with the automatic-origin tag) that puts the
/// Actuator Board in its Abort safe state before Launch. Only the AB handles
/// ACTUATOR_COMMAND, so the Valve Board (and its main-valve window) is never
/// touched by it.
pub(crate) const AB_ABORT_PAYLOAD: u8 = ActuatorBoardCommands::OverpressureAbort as u8 | 0x80;

/// Retire every automatic owner so nothing re-closes the relief path.
fn retire_automation(rt: &mut Runtime, now: u64, message: &str) {
    if let Some(mut controller) = rt.fill_trim.take() {
        controller.cancel();
    }
    if rt.turbocharge.take().is_some() {
        rt.turbo_evidence = None;
    }
    cold_flow::retire_for_overpressure(rt);
    rt.fill_cutoff.reset();
    #[cfg(not(feature = "hitl_mode"))]
    {
        rt.operational.running = false;
        rt.operational.target_reached = None;
        rt.operational.qd = QdWait::default();
        rt.operational.qd_launch_admission = None;
    }
    // Engine Fault, but its own relief outputs are replaced below.
    let _ = rt.engine.fault(now, message);
    rt.engine.claimed = true;
    rt.progress.complete(
        crate::sequence_progress_projection::TerminalProof::Failed {
            message: message.into(),
        },
        crate::sequence_progress_service::now_ms(),
    );
}

enum Action {
    /// Relief valve effects, plus the Actuator Board overpressure abort when
    /// `ab_abort` is set.
    Commands {
        effects: Effects,
        ab_abort: bool,
    },
    Alarm(String),
}

/// Evaluate the freshest valve-board pressure. Called on every valve-board
/// pressure receipt and every GSE tick. Never under the dispatch gate.
// Safety catalog (About safety): overpressure-relief, overpressure-abort, overpressure-after-t0. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn evaluate(state: &Arc<AppState>) {
    let launched = launched(state);
    let (action, abort, notice) = {
        let mut rt = state.gse.lock().unwrap();
        let now = rt.now();
        let thresholds = Thresholds::from_config(&rt.engine.config);
        let sample = rt.valve_pressure;
        let level = rt.overpressure.observe(sample, now, &thresholds);
        let notice = rt.overpressure.take_sensor_notice(now);
        if let Some(notice) = &notice {
            log::warn!("{notice}");
        }
        let burn = launched || fire_committed(&rt);
        let mut action = None;
        let mut abort = false;
        match (rt.overpressure.latch.clone(), level) {
            (None, Level::Relief(psi) | Level::Abort(psi)) => {
                let abort_level = matches!(level, Level::Abort(_));
                let threshold = if abort_level {
                    thresholds.abort_psi
                } else {
                    thresholds.relief_psi
                };
                if burn {
                    let message = format!(
                        "Tank overpressure during launch: {psi:.0} psi > {threshold:.0}. No automatic valve action: the main valve is already open for firing."
                    );
                    log::error!("{message}");
                    rt.overpressure.latch = Some(Latch {
                        peak_psi: psi,
                        threshold_psi: threshold,
                        at_ms: now,
                        message: message.clone(),
                        relief_commanded: false,
                        abort: AbortStage::NotRequired,
                        ab_abort_sent: false,
                    });
                    action = Some(Action::Alarm(message));
                } else {
                    let message = format!(
                        "Tank overpressure: {psi:.0} psi > {:.0}",
                        thresholds.relief_psi
                    );
                    log::error!("{message}; closing supplies and opening dump and vent");
                    retire_automation(&mut rt, now, &message);
                    let mut valves = RELIEF.to_vec();
                    let stage = if abort_level {
                        valves.push((0, true));
                        AbortStage::PilotOpen {
                            until_ms: now + thresholds.pilot_ms,
                        }
                    } else {
                        AbortStage::NotRequired
                    };
                    let notice = if abort_level {
                        format!(
                            "{message}. Supplies closed, dump and vent open; {psi:.0} psi >= {:.0}: main valve open for {} s, then Abort.",
                            thresholds.abort_psi,
                            thresholds.pilot_ms as f64 / 1000.0
                        )
                    } else {
                        format!("{message}. Supplies closed, dump and vent open. Acknowledge after pressure drops.")
                    };
                    rt.engine.status.message = notice.clone();
                    rt.overpressure.latch = Some(Latch {
                        peak_psi: psi,
                        threshold_psi: thresholds.relief_psi,
                        at_ms: now,
                        message,
                        relief_commanded: true,
                        abort: stage,
                        ab_abort_sent: false,
                    });
                    action = Some(Action::Commands {
                        effects: Effects {
                            valves,
                            notification: Some(notice),
                        },
                        ab_abort: true,
                    });
                }
            }
            (Some(latch), Level::Abort(psi)) if latch.abort == AbortStage::NotRequired => {
                if latch.relief_commanded && !burn {
                    let message = format!(
                        "Tank overpressure: {psi:.0} psi >= {:.0}: main valve open for {} s, then Abort",
                        thresholds.abort_psi,
                        thresholds.pilot_ms as f64 / 1000.0
                    );
                    log::error!("{message}");
                    // Re-assert the relief path with the main valve.
                    let mut valves = RELIEF.to_vec();
                    valves.push((0, true));
                    if let Some(l) = &mut rt.overpressure.latch {
                        l.abort = AbortStage::PilotOpen {
                            until_ms: now + thresholds.pilot_ms,
                        };
                    }
                    rt.engine.status.message = message.clone();
                    // Normally already sent at the relief stage; never wait
                    // for the main-valve window.
                    action = Some(Action::Commands {
                        effects: Effects {
                            valves,
                            notification: Some(message),
                        },
                        ab_abort: !latch.ab_abort_sent,
                    });
                } else if !latch.relief_commanded {
                    let message = format!(
                        "Tank overpressure during launch: {psi:.0} psi >= {:.0}. No automatic valve action or Abort after T0.",
                        thresholds.abort_psi
                    );
                    log::error!("{message}");
                    if let Some(l) = &mut rt.overpressure.latch {
                        l.abort = AbortStage::Aborted;
                    }
                    action = Some(Action::Alarm(message));
                }
            }
            (Some(latch), _) => {
                if let AbortStage::PilotOpen { until_ms } = latch.abort
                    && now >= until_ms
                {
                    if let Some(l) = &mut rt.overpressure.latch {
                        l.abort = AbortStage::Aborted;
                    }
                    // Pilot closed first, then Abort, in that order.
                    action = Some(Action::Commands {
                        effects: Effects {
                            valves: vec![(0, false)],
                            notification: None,
                        },
                        ab_abort: false,
                    });
                    abort = true;
                }
            }
            (None, Level::Normal) => {}
        }
        (action, abort, notice)
    };
    match action {
        Some(Action::Commands { effects, ab_abort }) => {
            apply(state, effects);
            if ab_abort {
                dispatch_ab_abort(state);
            }
        }
        Some(Action::Alarm(message)) => {
            state.add_notification_with_persistence(message, true);
        }
        None => {}
    }
    if let Some(notice) = notice {
        state.add_notification_with_persistence(notice, true);
    }
    if abort {
        dispatch_abort(state);
    }
}

/// Valve Board WARNING ingress. Returns the text to show the operator, or
/// None for a repeat of the same stage and trip. A board relief the station
/// has not latched itself (link was down, or a different configured
/// threshold) is adopted: station automation is retired so nothing re-opens
/// a supply or re-closes dump/vent, and the same relief outputs and the
/// Actuator Board overpressure abort are commanded (harmless duplicates of the
/// board's own; they also cover a lost Valve Board to Actuator Board command).
/// The board owns its main-valve window and Abort.
pub(crate) fn board_warning(state: &Arc<AppState>, text: &str) -> Option<String> {
    let Some((code, psi, trip)) = parse_board_report(text) else {
        return Some(text.to_string());
    };
    let display = text[..text.rfind(" [").unwrap_or(text.len())].to_string();
    let burn = launched(state);
    let action = {
        let mut rt = state.gse.lock().unwrap();
        let now = rt.now();
        let changed = rt
            .overpressure
            .board
            .as_ref()
            .is_none_or(|b| b.code != code || b.trip != trip);
        match code {
            'C' => rt.overpressure.board = None,
            'F' | 'S' | 'I' => {
                // A sensor fault is reported, never adopted as a trip.
                let repeat = rt.overpressure.board_sensor.as_ref().is_some_and(|b| {
                    b.code == code && now.saturating_sub(b.at_ms) < SENSOR_FAULT_SHOW_MS
                });
                rt.overpressure.board_sensor = Some(BoardReport { code, psi, trip, at_ms: now });
                return (!repeat).then(|| match code {
                    'I' => format!(
                        "{display}. Tank pressure sensor implausible or not connected (reading {psi} psi); not an overpressure."
                    ),
                    _ => display,
                });
            }
            _ => rt.overpressure.board = Some(BoardReport { code, psi, trip, at_ms: now }),
        }
        let settling = rt
            .overpressure
            .board_ack_at
            .is_some_and(|at| now.saturating_sub(at) < BOARD_ACK_SETTLE_MS);
        let adopt = code != 'C' && rt.overpressure.latch.is_none() && (changed || !settling);
        if !adopt {
            return changed.then_some(display);
        }
        let relief_psi = Thresholds::from_config(&rt.engine.config).relief_psi;
        if code == 'L' || burn || fire_committed(&rt) {
            rt.overpressure.latch = Some(Latch {
                peak_psi: psi as f32,
                threshold_psi: relief_psi,
                at_ms: now,
                message: display.clone(),
                relief_commanded: false,
                abort: AbortStage::NotRequired,
                ab_abort_sent: false,
            });
            Action::Alarm(display)
        } else {
            log::error!("{display}; adopting the Valve Board overpressure latch");
            retire_automation(&mut rt, now, &display);
            let notice = format!(
                "{display}. Supplies closed, dump and vent open by the Valve Board. Acknowledge after pressure drops."
            );
            rt.engine.status.message = notice.clone();
            rt.overpressure.latch = Some(Latch {
                peak_psi: psi as f32,
                threshold_psi: relief_psi,
                at_ms: now,
                message: display,
                relief_commanded: true,
                abort: if code == 'R' {
                    AbortStage::NotRequired
                } else {
                    AbortStage::Aborted
                },
                ab_abort_sent: false,
            });
            Action::Commands {
                effects: Effects {
                    valves: RELIEF.to_vec(),
                    notification: Some(notice),
                },
                ab_abort: true,
            }
        }
    };
    match action {
        Action::Commands { effects, ab_abort } => {
            apply(state, effects);
            if ab_abort {
                dispatch_ab_abort(state);
            }
        }
        Action::Alarm(message) => {
            state.add_notification_with_persistence(message, true);
        }
    }
    None
}

/// Queue the Actuator Board overpressure abort through the same controller
/// fence as the automatic supply closes. It is an abort-safe direction (all
/// AB outputs off), so a pending Stop never holds it back. Recorded on the
/// latch only once queued, so a later abort stage retries a failed send.
// Safety catalog (About safety): overpressure-relief, overpressure-abort. Keep backend/src/safety_catalog_rules.rs in step.
fn dispatch_ab_abort(state: &Arc<AppState>) {
    let Some(router) = state.topology_router.get() else {
        let _ = state.add_notification_with_persistence(
            "Tank overpressure: Actuator Board abort could not be sent: no hardware command router".to_string(),
            true,
        );
        return;
    };
    let queued = queue_controller_with_stop_fence(
        state,
        router,
        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
        &[AB_ABORT_PAYLOAD],
    );
    match queued {
        Ok(true) => {
            log::warn!("tank overpressure: Actuator Board abort queued payload={AB_ABORT_PAYLOAD:02x}");
            let mut rt = state.gse.lock().unwrap();
            rt.effects_queued = true;
            if let Some(latch) = &mut rt.overpressure.latch {
                latch.ab_abort_sent = true;
            }
        }
        Ok(false) => log::error!("tank overpressure: Actuator Board abort not admitted"),
        Err(error) => {
            log::error!("tank overpressure: Actuator Board abort failed: {error:?}");
            let _ = state.add_notification_with_persistence(
                "Tank overpressure: Actuator Board abort could not be sent".to_string(),
                true,
            );
        }
    }
}

fn dispatch_abort(state: &Arc<AppState>) {
    let generation = automatic_generation(state);
    let Some(router) = state.topology_router.get() else {
        let _ = state.add_notification_with_persistence(
            "Tank overpressure Abort could not be sent: no hardware command router".to_string(),
            true,
        );
        return;
    };
    match claim_automatic_abort_for_generation(state, generation) {
        Some(_claim) => {
            // Same dispatch as the safety task's automatic abort, in every build.
            state.set_abort_indicator_latched(true);
            crate::sequences::refresh_action_policy_now(state);
            state.broadcast_action_policy_snapshot();
            match crate::telemetry_task::queue_automatic_abort_packet(router, "Tank overpressure abort") {
                Ok(()) => {
                    if let Err(error) = router.process_all_queues_with_timeout(3) {
                        log::error!("failed to flush overpressure abort: {error:?}");
                    }
                }
                Err(error) => log::error!("failed to queue overpressure abort: {error:?}"),
            }
        }
        None => {
            log::error!("tank overpressure abort not admitted (already aborted, Stop pending or launch admitted)");
            let _ = state.add_notification_with_persistence(
                "Tank overpressure Abort was not admitted (already aborted, Stop pending or launch admitted); dump and vent remain open".to_string(),
                true,
            );
        }
    }
}

/// While the latch owns the valves, only relief-direction controls remain.
pub(crate) fn command_allowed(rt: &Runtime, cmd: &TelemetryCommand) -> Option<bool> {
    // The board latch refuses these itself; the station never fights it.
    if !rt.overpressure.owns_valves() && !rt.overpressure.board_owns(rt.now()) {
        return None;
    }
    let open = |i: usize| rt.valves[i].is_some_and(|v| v.open);
    Some(match cmd {
        TelemetryCommand::Abort | TelemetryCommand::NitrogenClose | TelemetryCommand::NitrousClose => true,
        TelemetryCommand::Dump => !open(2),
        TelemetryCommand::NormallyOpen => !open(1),
        TelemetryCommand::Pilot => open(0),
        TelemetryCommand::Nitrogen => open(3),
        TelemetryCommand::Nitrous => open(4),
        _ if valve_command(cmd) => false,
        TelemetryCommand::Turbocharge | TelemetryCommand::Launch => false,
        _ if action(cmd).is_some() => false,
        _ => {
            return matches!(
                crate::sequences::command_name(cmd),
                "GroundStationLaunch" | "IgniterSequence" | "RetractPlumbing" | "Igniter" | "Postinit"
            )
            .then_some(false);
        }
    })
}

/// Explicit operator acknowledgement once a fresh reading is below relief.
// Safety catalog (About safety): overpressure-latch. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn acknowledge(state: &AppState) -> Result<(), String> {
    let mut rt = state.gse.lock().unwrap();
    let now = rt.now();
    let board = rt.overpressure.board.clone();
    let latch = match (rt.overpressure.latch.clone(), &board) {
        (Some(latch), _) => latch,
        (None, Some(b)) => Latch {
            peak_psi: b.psi as f32,
            threshold_psi: 0.0,
            at_ms: b.at_ms,
            message: String::new(),
            relief_commanded: false,
            abort: AbortStage::NotRequired,
            ab_abort_sent: false,
        },
        (None, None) => return Err("No tank overpressure error is latched".into()),
    };
    if matches!(latch.abort, AbortStage::PilotOpen { .. })
        || board
            .as_ref()
            .is_some_and(|b| b.code == 'P' && now.saturating_sub(b.at_ms) <= BOARD_FRESH_MS)
    {
        return Err("Main-valve overpressure relief is still running".into());
    }
    let thresholds = Thresholds::from_config(&rt.engine.config);
    if rt.overpressure.settling() {
        return Err(
            "Tank pressure sensor implausible or not connected: wait for a settled reading to acknowledge".into(),
        );
    }
    match fresh(rt.valve_pressure, now) {
        Some(p) if p.value < thresholds.relief_psi => {}
        Some(p) => {
            return Err(format!(
                "Tank pressure {:.0} psi is still at or above {:.0} psi",
                p.value, thresholds.relief_psi
            ));
        }
        None => return Err("A fresh valve-board tank pressure is required to acknowledge".into()),
    }
    rt.overpressure.latch = None;
    rt.overpressure.relief_since = None;
    rt.overpressure.abort_since = None;
    rt.engine.status.message = format!(
        "Tank overpressure acknowledged (peak {:.0} psi); valves unchanged, manual control",
        latch.peak_psi
    );
    if board.is_none() {
        return Ok(());
    }
    // The board validates its own fresh pressure; while it stays latched it
    // keeps re-announcing and the station adopts it again.
    rt.overpressure.board_ack_at = Some(now);
    drop(rt);
    let Some(router) = state.topology_router.get() else {
        return Err("Valve Board overpressure acknowledgement not sent: no hardware command router".into());
    };
    crate::telemetry_task::queue_controller_command(
        router,
        crate::telemetry_schema::data_type("VALVE_COMMAND"),
        &[BOARD_ACK_COMMAND],
    )
    .map_err(|error| format!("Valve Board overpressure acknowledgement not sent: {error:?}"))
}

pub(super) fn decorate_status(rt: &Runtime, response: &mut serde_json::Value) {
    response["overpressure_latched"] = serde_json::json!(rt.overpressure.latched());
    response["overpressure"] = rt.overpressure.latch.as_ref().map_or(
        serde_json::json!({"latched": false}),
        |l| {
            serde_json::json!({
                "latched": true,
                "message": l.message,
                "peak_psi": l.peak_psi,
                "threshold_psi": l.threshold_psi,
                "relief_commanded": l.relief_commanded,
                "abort": format!("{:?}", l.abort),
                "at_ms": l.at_ms,
            })
        },
    );
    response["overpressure"]["valve_board"] = rt.overpressure.board.as_ref().map_or(
        serde_json::Value::Null,
        |b| {
            serde_json::json!({
                "stage": b.code.to_string(),
                "psi": b.psi,
                "trip": b.trip,
                "holds_relief": rt.overpressure.board_owns(rt.now()),
            })
        },
    );
    response["overpressure"]["sensor"] = sensor_status(rt);
}

/// Tank pressure sensor health for the operator: an implausible step on the
/// station's own check, or a Valve Board sensor fault report. Never a trip.
fn sensor_status(rt: &Runtime) -> serde_json::Value {
    let now = rt.now();
    let station = rt.overpressure.sensor_fault_recent(now);
    let board = rt
        .overpressure
        .board_sensor
        .as_ref()
        .filter(|b| now.saturating_sub(b.at_ms) <= SENSOR_FAULT_SHOW_MS);
    let message = match (station, board) {
        (Some(fault), _) => Some(sensor_fault_message(fault)),
        (None, Some(b)) => Some(match b.code {
            'I' => format!(
                "Valve Board: tank pressure sensor implausible or not connected (reading {} psi); not an overpressure.",
                b.psi
            ),
            'F' => format!("Valve Board: tank pressure sensor invalid (raw ADC {}); not an overpressure.", b.psi),
            _ => "Valve Board: tank pressure sensor stale; overpressure protection has no reading.".to_string(),
        }),
        (None, None) => None,
    };
    serde_json::json!({
        "fault": message.is_some(),
        "implausible": station.is_some() || board.is_some_and(|b| b.code == 'I'),
        "settling": rt.overpressure.settling(),
        "message": message,
        "station": station.map(|f| serde_json::json!({
            "from_psi": f.from_psi,
            "to_psi": f.to_psi,
            "step_ms": f.step_ms,
            "at_ms": f.at_ms,
            "count": f.count,
            "first_at_ms": f.first_at_ms,
        })),
        "valve_board": board.map(|b| serde_json::json!({
            "code": b.code.to_string(),
            "value": b.psi,
            "at_ms": b.at_ms,
        })),
    })
}

pub(super) async fn acknowledge_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if !principal.allows_command_name("CancelFill") {
        return StatusCode::FORBIDDEN.into_response();
    }
    match acknowledge(&state) {
        Ok(()) => {
            crate::sequences::refresh_action_policy_now(&state);
            Json(serde_json::json!({"acknowledged": true})).into_response()
        }
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}
