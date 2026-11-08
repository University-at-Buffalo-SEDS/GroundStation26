//! Tank overpressure protection on the valve-board psi lane.
//!
//! Independent of every sequence, mode (HITL, test-fire, flight) and
//! pause/hold state before Launch:
//! - at or above `overpressure_relief_psi` (default 850): close Nitrous, close
//!   Nitrogen, open Dump, open Vent, and latch an error;
//! - at or above `overpressure_abort_psi` (default 950): additionally open the
//!   main (pilot) valve. After `overpressure_pilot_ms` (default 3 s) the
//!   station commands the pilot closed and then dispatches an automatic Abort.
//!   The Abort is sent only after the pilot window, so neither the station's
//!   own abort fence (which refuses non-relief outputs once the abort latch is
//!   set) nor the boards' abort safe state (which closes the pilot) can cut
//!   the window short.
//!
//! Once Launch is admitted or Fire is committed (T0 onward) the main valve is
//! already open for firing: the monitor only logs and alarms, never commands.
//! The latch never re-closes anything automatically; the operator
//! acknowledges it after a fresh reading is back below the relief pressure.
use super::*;

/// Consecutive distinct valve-board samples (50 Hz, so about 20 ms apart)
/// required at or above a threshold. A single-sample spike never trips.
pub(crate) const DEBOUNCE_SAMPLES: u8 = 2;
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
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Monitor {
    last_sample_at: Option<u64>,
    relief_streak: u8,
    abort_streak: u8,
    pub(crate) latch: Option<Latch>,
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
    /// Count each new fresh sample once. Returns the debounced level.
    pub(crate) fn observe(&mut self, sample: Option<Sample>, now: u64, t: &Thresholds) -> Level {
        if let Some(sample) = fresh(sample, now)
            && self.last_sample_at.is_none_or(|last| sample.at_ms > last)
        {
            self.last_sample_at = Some(sample.at_ms);
            let bump = |streak: &mut u8, over: bool| {
                *streak = if over { streak.saturating_add(1) } else { 0 };
            };
            bump(&mut self.relief_streak, sample.value >= t.relief_psi);
            bump(&mut self.abort_streak, sample.value >= t.abort_psi);
            if let Some(latch) = &mut self.latch {
                latch.peak_psi = latch.peak_psi.max(sample.value);
            }
            if self.abort_streak >= DEBOUNCE_SAMPLES {
                return Level::Abort(sample.value);
            }
            if self.relief_streak >= DEBOUNCE_SAMPLES {
                return Level::Relief(sample.value);
            }
        }
        Level::Normal
    }
    pub(crate) fn latched(&self) -> bool {
        self.latch.is_some()
    }
    /// The latch owns the valves: no sequence, hold or correction may run.
    pub(crate) fn owns_valves(&self) -> bool {
        self.latch.as_ref().is_some_and(|l| l.relief_commanded)
    }
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
    Commands(Effects),
    Alarm(String),
}

/// Evaluate the freshest valve-board pressure. Called on every valve-board
/// pressure receipt and every GSE tick. Never under the dispatch gate.
pub(crate) fn evaluate(state: &Arc<AppState>) {
    let launched = launched(state);
    let (action, abort) = {
        let mut rt = state.gse.lock().unwrap();
        let now = rt.now();
        let thresholds = Thresholds::from_config(&rt.engine.config);
        let sample = rt.valve_pressure;
        let level = rt.overpressure.observe(sample, now, &thresholds);
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
                    });
                    action = Some(Action::Commands(Effects {
                        valves,
                        notification: Some(notice),
                    }));
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
                    action = Some(Action::Commands(Effects {
                        valves,
                        notification: Some(message),
                    }));
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
                    action = Some(Action::Commands(Effects {
                        valves: vec![(0, false)],
                        notification: None,
                    }));
                    abort = true;
                }
            }
            (None, Level::Normal) => {}
        }
        (action, abort)
    };
    match action {
        Some(Action::Commands(effects)) => apply(state, effects),
        Some(Action::Alarm(message)) => {
            state.add_notification_with_persistence(message, true);
        }
        None => {}
    }
    if abort {
        dispatch_abort(state);
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
    if !rt.overpressure.owns_valves() {
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
pub(crate) fn acknowledge(state: &AppState) -> Result<(), String> {
    let mut rt = state.gse.lock().unwrap();
    let Some(latch) = rt.overpressure.latch.clone() else {
        return Err("No tank overpressure error is latched".into());
    };
    if matches!(latch.abort, AbortStage::PilotOpen { .. }) {
        return Err("Main-valve overpressure relief is still running".into());
    }
    let now = rt.now();
    let thresholds = Thresholds::from_config(&rt.engine.config);
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
    rt.overpressure.relief_streak = 0;
    rt.overpressure.abort_streak = 0;
    rt.engine.status.message = format!(
        "Tank overpressure acknowledged (peak {:.0} psi); valves unchanged, manual control",
        latch.peak_psi
    );
    Ok(())
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
