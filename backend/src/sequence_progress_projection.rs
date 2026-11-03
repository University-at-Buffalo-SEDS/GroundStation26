//! Typed projection of actual engine observations. Idle is never a completion proof.
use super::sequence_progress::{
    Error, Evidence, Kind, MAX_STEPS, Outcome, RunKey, Snapshot, Step, StepState, VERSION, Value,
    Watch,
};
use gse_sequence::fill_trim::{Phase as TrimPhase, ProgressObservation as TrimObservation};
use gse_sequence::turbocharge::{Phase as TurboPhase, ProgressObservation as TurboObservation};
use gse_sequence::{NitrogenStep, Phase, ProgressObservation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Tick,
    PauseAccepted,
    ResumeAccepted,
    CancelAccepted,
    Fault,
    Reset,
}
#[derive(Clone, Debug)]
#[expect(clippy::large_enum_variant, reason = "Bounded terminal evidence is captured into the fixed Copy stage; boxing would add heap allocation to owner-held capture paths") ]
pub enum TerminalProof {
    /// The caller supplies the actual successful persisted completion event ID.
    DurablePass {
        kind: Kind,
        event_id: String,
    },
    /// Safe cancellation is backed by the engine's actual confirmed transition.
    SafeCancelled(ProgressObservation),
    /// Only the actual engine tick hook supplies these consecutive observations
    /// when this build has no durable operational qualification store (HITL).
    #[cfg(any(test, feature = "hitl_mode"))]
    EngineCompleted {
        kind: Kind,
        before: ProgressObservation,
        after: ProgressObservation,
    },
    #[cfg(any(test, feature = "hitl_mode"))]
    VerifiedFillTrim {
        trim: TrimObservation,
        input: gse_sequence::Inputs,
    },
    Interrupted,
    Failed {
        message: String,
    },
}
pub struct Cursor {
    snapshot: Snapshot,
    previous: ProgressObservation,
    control_step: Option<String>,
    control_count: u16,
    turbo_phase: Option<TurboPhase>,
    trim_progress: Option<TrimObservation>,
    resume_configure: Option<String>,
}
fn title(kind: Kind) -> &'static str {
    match kind {
        Kind::ValveSelfTest => "Valve self-test",
        Kind::NitrogenTest => "Nitrogen test",
        Kind::Fill => "Automatic fill",
        Kind::Turbocharge => "Turbocharge",
        Kind::ContinueFillSequence => "Fill sequence",
        Kind::IgniterSequence => "Igniter sequence",
    }
}
fn step(id: String, label: String) -> Step {
    Step {
        id,
        label,
        state: StepState::Planned,
        watch: vec![],
    }
}
fn plan(kind: Kind, nitrogen: &[NitrogenStep]) -> Result<Vec<Step>, Error> {
    if nitrogen.len() > 32 {
        return Err(Error::Bounds);
    }
    let mut values = Vec::new();
    if matches!(kind, Kind::ValveSelfTest | Kind::NitrogenTest) {
        values.push(step(
            "baseline-configure".into(),
            "Confirm baseline valve configuration".into(),
        ));
        values.push(step(
            "baseline-sample".into(),
            "Measure empty-tank pressure baseline".into(),
        ));
    }
    match kind {
        Kind::ValveSelfTest => {
            values.push(step(
                "self-resting".into(),
                "Confirm resting board outputs".into(),
            ));
            for (index, name) in ["pilot", "vent", "dump", "nitrogen", "nitrous"]
                .into_iter()
                .enumerate()
            {
                for (stage, label) in [
                    ("move", "Check output"),
                    ("hold", "Verify dwell"),
                    ("restore", "Restore output"),
                ] {
                    values.push(step(
                        format!("self-{index}-{stage}"),
                        format!("{label}: {name}"),
                    ));
                }
            }
            values.push(step(
                "self-finish".into(),
                "Confirm final dump and vent configuration".into(),
            ));
        }
        Kind::NitrogenTest => {
            for (index, target) in nitrogen.iter().enumerate() {
                if !target.target_psi.is_finite() {
                    return Err(Error::Invalid);
                }
                for (stage, label) in [
                    ("configure", "Configure outputs"),
                    ("raise", "Raise pressure"),
                    ("settle", "Verify settling"),
                    ("hold", "Verify pressure hold"),
                ] {
                    values.push(step(
                        format!("nitrogen-{index}-{stage}"),
                        format!("{label}: +{:.1} psi above baseline", target.target_psi),
                    ));
                }
            }
            values.push(step(
                "nitrogen-dump".into(),
                "Verify dump and zero pressure".into(),
            ));
        }
        Kind::Fill => {
            values.push(step(
                "fill-configure".into(),
                "Confirm fill configuration".into(),
            ));
            values.push(step(
                "fill-target".into(),
                "Reach automatic cutoff mass".into(),
            ));
            for (id, label) in [
                ("fill-close", "Confirm all outputs closed"),
                ("fill-settle-0", "Settle and assess calibrated mass"),
                ("fill-record", "Record verified fill result"),
            ] {
                values.push(step(id.into(), label.into()));
            }
        }
        Kind::Turbocharge => {
            for (id, label) in [
                ("turbo-closing", "Confirm all five outputs closed"),
                ("turbo-opening", "Confirm nitrogen output on"),
                ("turbo-raising", "Reach the qualified pressure target"),
                ("turbo-closing-nitrogen", "Confirm nitrogen output off"),
                ("turbo-settling", "Verify settled mass and closed outputs"),
                ("turbo-record", "Record verified inventory evidence"),
            ] {
                values.push(step(id.into(), label.into()));
            }
        }
        Kind::ContinueFillSequence | Kind::IgniterSequence => values.push(step(
            "producer-progress".into(),
            "Waiting for authoritative producer progress".into(),
        )),
    }
    if values.len() > MAX_STEPS {
        return Err(Error::Bounds);
    }
    Ok(values)
}
fn current(kind: Kind, view: ProgressObservation) -> Option<String> {
    match view.phase {
        Phase::BaselineSetup => Some("baseline-configure".into()),
        Phase::Baseline => Some("baseline-sample".into()),
        Phase::SelfSetup => Some("self-resting".into()),
        Phase::SelfOpen => Some(format!("self-{}-move", view.self_index)),
        Phase::SelfHold => Some(format!("self-{}-hold", view.self_index)),
        Phase::SelfClose => Some(format!("self-{}-restore", view.self_index)),
        Phase::SelfFinish => Some("self-finish".into()),
        Phase::NitrogenSetup => Some(format!("nitrogen-{}-configure", view.nitrogen_step_index)),
        Phase::Raising => Some(format!("nitrogen-{}-raise", view.nitrogen_step_index)),
        Phase::Settling => Some(format!("nitrogen-{}-settle", view.nitrogen_step_index)),
        Phase::Holding => Some(format!("nitrogen-{}-hold", view.nitrogen_step_index)),
        Phase::Dumping => Some("nitrogen-dump".into()),
        Phase::FillSetup => Some("fill-configure".into()),
        Phase::Filling => Some("fill-target".into()),
        Phase::Turbocharge => None,
        _ if matches!(kind, Kind::ContinueFillSequence | Kind::IgniterSequence) => {
            Some("producer-progress".into())
        }
        _ => None,
    }
}
// Credit only the actual engine's normal progression edges. Overrides and unrelated
// phase changes cannot turn an unfinished step into a passed step.
fn completed_edge(before: ProgressObservation, after: ProgressObservation) -> bool {
    use Phase::*;
    match (before.phase, after.phase) {
        (BaselineSetup, Baseline)
        | (Baseline, SelfSetup | NitrogenSetup)
        | (SelfSetup, SelfOpen)
        | (SelfOpen, SelfHold)
        | (SelfHold, SelfClose)
        | (SelfClose, SelfFinish)
        | (SelfFinish, Idle)
        | (NitrogenSetup, Raising)
        | (Raising, Settling)
        | (Settling, Holding)
        | (Holding, Dumping)
        | (Dumping, Passed)
        | (FillSetup, Filling)
        | (PauseSetup, Paused)
        | (CancelSetup, Cancelled) => true,
        (SelfClose, SelfOpen) => after.self_index == before.self_index + 1,
        (Holding, NitrogenSetup) => after.nitrogen_step_index == before.nitrogen_step_index + 1,
        _ => false,
    }
}
fn trim_step(trim: TrimObservation) -> Option<String> {
    Some(match trim.phase {
        TrimPhase::Closing => "fill-close".into(),
        TrimPhase::Settling if trim.pulses == 0 => "fill-settle-0".into(),
        TrimPhase::Settling => format!("fill-pulse-{}-settle", trim.pulses),
        TrimPhase::VentOpening => format!("fill-pulse-{}-open", trim.pulses),
        TrimPhase::Venting => format!("fill-pulse-{}-hold", trim.pulses),
        TrimPhase::VentClosing => format!("fill-pulse-{}-close", trim.pulses),
        TrimPhase::Complete => "fill-record".into(),
        TrimPhase::Fault | TrimPhase::Cancelled => return None,
    })
}
fn turbo_step(phase: TurboPhase) -> Option<&'static str> {
    Some(match phase {
        TurboPhase::Closing => "turbo-closing",
        TurboPhase::OpeningNitrogen => "turbo-opening",
        TurboPhase::Raising => "turbo-raising",
        TurboPhase::ClosingNitrogen => "turbo-closing-nitrogen",
        TurboPhase::Settling => "turbo-settling",
        TurboPhase::Complete => "turbo-record",
        TurboPhase::Cancelled => return None,
    })
}
fn bounded_message(value: &str) -> String {
    let mut end = value.len().min(512);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}
fn watch(view: ProgressObservation) -> Vec<Watch> {
    let mut values = Vec::with_capacity(7);
    for (index, valve) in view.valves.iter().enumerate() {
        let fresh = valve.fresh_readback_open;
        values.push(Watch {
            id: format!("board-output-{index}"),
            label: format!(
                "{} board output",
                ["Pilot", "Vent", "Dump", "Nitrogen", "Nitrous"][index]
            ),
            target: Some(Value::Boolean(valve.target_open)),
            observed: fresh.map(Value::Boolean),
            unit: None,
            age_ms: valve.readback_age_ms,
            valid_for_ms: 2000,
            evidence: if fresh.is_some() {
                Evidence::BoardReported
            } else {
                Evidence::Unknown
            },
        });
    }
    values.push(Watch {
        id: "tank-pressure".into(),
        label: "Tank pressure".into(),
        target: view
            .nitrogen_absolute_target_psi
            .map(|value| Value::Number(value as f64)),
        observed: view.fresh_pressure_psi.map(|v| Value::Number(v as f64)),
        unit: Some("psi".into()),
        age_ms: view.pressure_age_ms,
        valid_for_ms: 2000,
        evidence: if view.fresh_pressure_psi.is_some() {
            Evidence::Measured
        } else {
            Evidence::Unknown
        },
    });
    values
}
impl Cursor {
    pub fn accepted(
        key: RunKey,
        kind: Kind,
        origin_client: Option<String>,
        view: ProgressObservation,
        nitrogen: &[NitrogenStep],
    ) -> Result<Self, Error> {
        let steps = plan(kind, nitrogen)?;
        let snapshot = Snapshot {
            version: VERSION,
            key,
            revision: 1,
            kind,
            outcome: Outcome::Running,
            origin_client,
            title: title(kind).into(),
            message: "Sequence accepted by the station".into(),
            current_step: None,
            next_step: None,
            steps,
            observed_age_ms: 0,
            valid_for_ms: 2000,
            cancel_allowed: kind.has_gse_safe_cancel(),
            notification_id: None,
            notification_timestamp_ms: None,
            reporting_error: None,
            watch_history_truncated: false,
            details_complete: true,
        };
        let mut value = Self {
            snapshot,
            previous: view,
            control_step: None,
            control_count: 0,
            turbo_phase: None,
            trim_progress: None,
            resume_configure: None,
        };
        value.present(view)?;
        value.update_cancel(view);
        value.snapshot.validate()?;
        Ok(value)
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    pub fn observe_fill(
        &mut self,
        view: ProgressObservation,
        mass: Option<gse_sequence::Sample>,
        target: Option<f32>,
        source_now_ms: u64,
        cause: Cause,
    ) -> Result<&Snapshot, Error> {
        if self.snapshot.kind != Kind::Fill {
            return Err(Error::Kind);
        }
        self.observe(view, cause)?;
        if let Some(id) = &self.snapshot.current_step
            && let Some(step) = self.snapshot.steps.iter_mut().find(|s| &s.id == id)
        {
            let mass = mass.filter(|m| {
                m.value.is_finite()
                    && source_now_ms
                        .checked_sub(m.at_ms)
                        .is_some_and(|age| age <= 2000)
            });
            step.watch.push(Watch {
                id: "calibrated-fill-mass".into(),
                label: "Calibrated fill mass".into(),
                target: target
                    .filter(|v| v.is_finite())
                    .map(|v| Value::Number(v as f64)),
                observed: mass.map(|m| Value::Number(m.value as f64)),
                unit: Some("kg".into()),
                age_ms: mass.and_then(|m| source_now_ms.checked_sub(m.at_ms)),
                valid_for_ms: 2000,
                evidence: if mass.is_some() {
                    Evidence::Measured
                } else {
                    Evidence::Unknown
                },
            });
        }
        self.bound_watch_history()?;
        self.snapshot.validate()?;
        Ok(&self.snapshot)
    }
    pub fn observe_trim(
        &mut self,
        trim: TrimObservation,
        mut view: ProgressObservation,
        mass: Option<gse_sequence::Sample>,
        source_now_ms: u64,
        reason: Option<&str>,
    ) -> Result<&Snapshot, Error> {
        if self.snapshot.kind != Kind::Fill {
            return Err(Error::Kind);
        }
        if self.snapshot.outcome.terminal() {
            return Err(Error::NotCurrent);
        }
        if trim.pulses > trim.max_pulses || usize::from(trim.max_pulses) * 4 + 5 > MAX_STEPS {
            return Err(Error::Bounds);
        }
        let previous = self.trim_progress;
        if previous.is_none() {
            if trim.phase != TrimPhase::Closing {
                return Err(Error::Invalid);
            }
            if let Some(goal) = self
                .snapshot
                .steps
                .iter_mut()
                .find(|s| s.id == "fill-target")
            {
                goal.state = StepState::Passed;
            }
            self.control_step = None;
        }
        let old_pulses = previous.map_or(0, |p| p.pulses);
        if trim.pulses > old_pulses {
            if trim.pulses != old_pulses + 1 || trim.phase != TrimPhase::VentOpening {
                return Err(Error::RevisionGap);
            }
            let record = self
                .snapshot
                .steps
                .iter()
                .position(|s| s.id == "fill-record")
                .ok_or(Error::Invalid)?;
            for (offset, (stage, label)) in [
                ("open", "Confirm vent output on"),
                ("hold", "Observe bounded vent pulse"),
                ("close", "Confirm vent output off"),
                ("settle", "Settle and reassess mass"),
            ]
            .into_iter()
            .enumerate()
            {
                self.snapshot.steps.insert(
                    record + offset,
                    step(
                        format!("fill-pulse-{}-{stage}", trim.pulses),
                        format!("Trim pulse {}: {label}", trim.pulses),
                    ),
                );
            }
        }
        if let Some(before) = previous {
            let normal = matches!(
                (before.phase, trim.phase),
                (TrimPhase::Closing, TrimPhase::Settling)
                    | (
                        TrimPhase::Settling,
                        TrimPhase::VentOpening | TrimPhase::Complete
                    )
                    | (TrimPhase::VentOpening, TrimPhase::Venting)
                    | (TrimPhase::Venting, TrimPhase::VentClosing)
                    | (TrimPhase::VentClosing, TrimPhase::Settling)
            );
            if normal
                && let Some(id) = trim_step(before)
                && let Some(step) = self.snapshot.steps.iter_mut().find(|s| s.id == id)
            {
                step.state = StepState::Passed;
            }
        }
        self.trim_progress = Some(trim);
        for (i, valve) in view.valves.iter_mut().enumerate() {
            valve.target_open = trim.desired[i];
            valve.awaiting_dispatch = trim.pending_mask & (1 << i) != 0;
            valve.awaiting_self_readback = trim.waiting == Some(i);
            valve.causal_since_ms = trim.command_since_ms;
        }
        self.observe(
            view,
            if trim.phase == TrimPhase::Fault {
                Cause::Fault
            } else if trim.phase == TrimPhase::Cancelled {
                Cause::Reset
            } else {
                Cause::Tick
            },
        )?;
        if let Some(id) = &self.snapshot.current_step
            && let Some(step) = self.snapshot.steps.iter_mut().find(|s| &s.id == id)
        {
            let mass = mass.filter(|m| {
                m.value.is_finite()
                    && source_now_ms
                        .checked_sub(m.at_ms)
                        .is_some_and(|age| age <= 2000)
            });
            step.watch.push(Watch {
                id: "calibrated-fill-mass".into(),
                label: "Calibrated fill mass".into(),
                target: Some(Value::Number(trim.target_mass_kg as f64)),
                observed: mass.map(|m| Value::Number(m.value as f64)),
                unit: Some("kg".into()),
                age_ms: mass.and_then(|m| source_now_ms.checked_sub(m.at_ms)),
                valid_for_ms: 2000,
                evidence: if mass.is_some() {
                    Evidence::Measured
                } else {
                    Evidence::Unknown
                },
            });
        }
        self.snapshot.cancel_allowed = !self.snapshot.outcome.terminal()
            && !matches!(
                trim.phase,
                TrimPhase::Complete | TrimPhase::Fault | TrimPhase::Cancelled
            );
        if let Some(reason) = reason {
            self.snapshot.message = bounded_message(reason);
        }
        self.bound_watch_history()?;
        self.snapshot.validate()?;
        Ok(&self.snapshot)
    }
    pub fn observe_turbo(
        &mut self,
        turbo: TurboObservation,
        mut view: ProgressObservation,
        mass: Option<gse_sequence::Sample>,
        source_now_ms: u64,
        reason: Option<&str>,
    ) -> Result<&Snapshot, Error> {
        if self.snapshot.kind != Kind::Turbocharge {
            return Err(Error::Kind);
        }
        if self.snapshot.outcome.terminal() {
            return Err(Error::NotCurrent);
        }
        if self.turbo_phase != Some(turbo.phase)
            && let Some(before) = self.turbo_phase
        {
                let normal = matches!(
                    (before, turbo.phase),
                    (
                        TurboPhase::Closing,
                        TurboPhase::OpeningNitrogen | TurboPhase::Settling
                    ) | (
                        TurboPhase::OpeningNitrogen,
                        TurboPhase::Raising | TurboPhase::ClosingNitrogen
                    ) | (TurboPhase::Raising, TurboPhase::ClosingNitrogen)
                        | (TurboPhase::ClosingNitrogen, TurboPhase::Settling)
                        | (TurboPhase::Settling, TurboPhase::Complete)
                );
                if normal
                    && let Some(id) = turbo_step(before)
                    && let Some(step) = self.snapshot.steps.iter_mut().find(|s| s.id == id)
                {
                    step.state = StepState::Passed;
                }
                if before == TurboPhase::Closing && turbo.phase == TurboPhase::Settling {
                    for step in &mut self.snapshot.steps {
                        if matches!(
                            step.id.as_str(),
                            "turbo-opening" | "turbo-raising" | "turbo-closing-nitrogen"
                        ) {
                            step.state = StepState::Skipped;
                        }
                    }
                }
                if before == TurboPhase::OpeningNitrogen
                    && turbo.phase == TurboPhase::ClosingNitrogen
                    && let Some(step) = self.snapshot.steps.iter_mut().find(|s| s.id == "turbo-raising")
                {
                    step.state = StepState::Passed;
                }
        }
        self.turbo_phase = Some(turbo.phase);
        view.nitrogen_absolute_target_psi = Some(turbo.pressure_target_psi);
        for (i, valve) in view.valves.iter_mut().enumerate() {
            valve.target_open = turbo.desired[i];
            valve.awaiting_dispatch = turbo.pending_mask & (1 << i) != 0;
            valve.awaiting_self_readback = turbo.waiting == Some(i);
            valve.causal_since_ms = turbo.command_since_ms;
        }
        self.observe(
            view,
            if turbo.phase == TurboPhase::Cancelled {
                Cause::Fault
            } else {
                Cause::Tick
            },
        )?;
        if let Some(id) = &self.snapshot.current_step
            && let Some(step) = self.snapshot.steps.iter_mut().find(|s| &s.id == id)
        {
            let now = source_now_ms;
            let mass = mass.filter(|m| {
                m.value.is_finite() && now.checked_sub(m.at_ms).is_some_and(|age| age <= 2000)
            });
            step.watch.push(Watch {
                id: "combined-tank-mass".into(),
                label: "Combined tank mass (includes pressurant)".into(),
                target: None,
                observed: mass.map(|m| Value::Number(m.value as f64)),
                unit: Some("kg".into()),
                age_ms: mass.and_then(|m| now.checked_sub(m.at_ms)),
                valid_for_ms: 2000,
                evidence: if mass.is_some() {
                    Evidence::Measured
                } else {
                    Evidence::Unknown
                },
            });
        }
        if turbo.phase == TurboPhase::Cancelled {
            self.snapshot.message=reason.map(bounded_message).unwrap_or_else(||"Turbocharge stopped. Verify actual output reports; safe closure is not implied.".into());
        }
        self.bound_watch_history()?;
        self.snapshot.validate()?;
        Ok(&self.snapshot)
    }
    pub fn observe(&mut self, view: ProgressObservation, cause: Cause) -> Result<&Snapshot, Error> {
        if self.snapshot.outcome.terminal() {
            return Err(Error::NotCurrent);
        }
        self.snapshot.revision = self.snapshot.revision.checked_add(1).ok_or(Error::Bounds)?;
        if cause == Cause::ResumeAccepted && self.snapshot.kind == Kind::Fill {
            self.control_step = None;
            self.trim_progress = None;
            self.control_count = self.control_count.checked_add(1).ok_or(Error::Bounds)?;
            let id = format!("control-resume-{}", self.control_count);
            self.snapshot.steps.push(step(
                id.clone(),
                "Confirm resumed fill configuration".into(),
            ));
            self.resume_configure = Some(id);
        }
        if matches!(cause, Cause::PauseAccepted | Cause::CancelAccepted) {
            if let Some(id) = &self.snapshot.current_step
                && let Some(step) = self.snapshot.steps.iter_mut().find(|s| &s.id == id)
            {
                step.state = if cause == Cause::CancelAccepted || step.id != "fill-target" {
                    StepState::Skipped
                } else {
                    StepState::Planned
                };
            }
            self.control_count = self.control_count.checked_add(1).ok_or(Error::Bounds)?;
            let id = format!(
                "control-{}-{}",
                if cause == Cause::CancelAccepted {
                    "cancel"
                } else {
                    "pause"
                },
                self.control_count
            );
            self.snapshot.steps.push(step(
                id.clone(),
                if cause == Cause::CancelAccepted {
                    "Confirm safe cancellation"
                } else {
                    "Confirm safe pause"
                }
                .into(),
            ));
            self.control_step = Some(id);
        } else if cause == Cause::Tick && self.previous.phase != view.phase
            && let Some(id) = &self.snapshot.current_step
            && let Some(step) = self.snapshot.steps.iter_mut().find(|s| &s.id == id)
        {
                if view.phase == Phase::Fault {
                    step.state = StepState::Failed;
                } else if completed_edge(self.previous, view)
                    && !matches!(step.id.as_str(), "fill-record" | "turbo-record")
                {
                    step.state = StepState::Passed;
                }
        }
        if matches!(cause, Cause::Fault | Cause::Reset)
            && let Some(id) = &self.snapshot.current_step
            && let Some(step) = self.snapshot.steps.iter_mut().find(|s| &s.id == id)
        {
                step.state = if cause == Cause::Fault {
                    StepState::Failed
                } else {
                    StepState::Unknown
                };
        }
        self.previous = view;
        if cause == Cause::Reset {
            self.snapshot.outcome = Outcome::Interrupted;
            self.snapshot.cancel_allowed = false;
            self.snapshot.message = "Sequence interrupted by reset".into();
        } else if cause == Cause::Fault || view.phase == Phase::Fault {
            self.snapshot.outcome = Outcome::Failed;
            self.snapshot.cancel_allowed = false;
            self.snapshot.message = "The station stopped the sequence".into();
        } else if matches!(view.phase, Phase::CancelSetup) {
            self.snapshot.outcome = Outcome::Cancelling;
            self.snapshot.cancel_allowed = false;
        } else if view.phase == Phase::Paused {
            self.snapshot.outcome = Outcome::Paused;
        } else if view.phase == Phase::Cancelled {
            self.snapshot.outcome = Outcome::Cancelling;
        } else {
            self.snapshot.outcome = Outcome::Running;
        }
        self.update_cancel(view);
        if self.snapshot.outcome.terminal() {
            self.finish_steps();
        }
        self.present(view)?;
        self.snapshot.validate()?;
        Ok(&self.snapshot)
    }
    fn finish_steps(&mut self) {
        for step in &mut self.snapshot.steps {
            if self.snapshot.outcome == Outcome::Passed
                && matches!(
                    (self.snapshot.kind, step.id.as_str()),
                    (Kind::Fill, "fill-record") | (Kind::Turbocharge, "turbo-record")
                )
            {
                step.state = StepState::Passed;
            } else if self.snapshot.outcome != Outcome::Passed {
                if step.state == StepState::Planned {
                    step.state = StepState::Skipped;
                } else if matches!(step.state, StepState::Current | StepState::Watching) {
                    step.state = match self.snapshot.outcome {
                        Outcome::Failed => StepState::Failed,
                        Outcome::Cancelled => StepState::Skipped,
                        _ => StepState::Unknown,
                    };
                }
            }
        }
    }
    fn update_cancel(&mut self, view: ProgressObservation) {
        self.snapshot.cancel_allowed = match self.snapshot.kind {
            Kind::ValveSelfTest => matches!(
                view.phase,
                Phase::BaselineSetup
                    | Phase::Baseline
                    | Phase::SelfSetup
                    | Phase::SelfOpen
                    | Phase::SelfHold
                    | Phase::SelfClose
                    | Phase::SelfFinish
            ),
            Kind::NitrogenTest => matches!(
                view.phase,
                Phase::BaselineSetup
                    | Phase::Baseline
                    | Phase::NitrogenSetup
                    | Phase::Raising
                    | Phase::Settling
                    | Phase::Holding
                    | Phase::Dumping
            ),
            Kind::Fill => matches!(
                view.phase,
                Phase::FillSetup | Phase::Filling | Phase::Paused
            ),
            Kind::Turbocharge => matches!(view.phase, Phase::Turbocharge | Phase::Paused),
            _ => false,
        } && !self.snapshot.outcome.terminal();
    }
    fn present(&mut self, view: ProgressObservation) -> Result<(), Error> {
        let id = if self.snapshot.kind == Kind::Fill
            && self.control_step.is_none()
            && self.trim_progress.is_some()
            && matches!(view.phase, Phase::PauseSetup | Phase::Paused)
        {
            self.trim_progress.and_then(trim_step)
        } else if self.snapshot.kind == Kind::Fill
            && view.phase == Phase::FillSetup
            && self.resume_configure.is_some()
        {
            self.resume_configure.clone()
        } else if matches!(
            view.phase,
            Phase::PauseSetup | Phase::CancelSetup | Phase::Paused | Phase::Cancelled
        ) {
            self.control_step.clone()
        } else if self.snapshot.kind == Kind::Turbocharge && view.phase == Phase::Turbocharge {
            self.turbo_phase.and_then(turbo_step).map(str::to_string)
        } else {
            current(self.snapshot.kind, view)
        };
        if let Some(id) = &id {
            let index = self
                .snapshot
                .steps
                .iter()
                .position(|s| &s.id == id)
                .ok_or(Error::Invalid)?;
            let step = &mut self.snapshot.steps[index];
            if !self.snapshot.outcome.terminal() {
                if !matches!(
                    step.state,
                    StepState::Passed | StepState::Failed | StepState::Skipped
                ) {
                    step.state = StepState::Watching;
                }
                // A paused but ongoing run still needs current readbacks, not
                // a repeatedly refreshed envelope around old confirmation cells.
                step.watch = watch(view);
            }
            self.snapshot.next_step = self
                .snapshot
                .steps
                .iter()
                .skip(index + 1)
                .find(|s| s.state == StepState::Planned)
                .map(|s| s.id.clone());
        } else {
            self.snapshot.next_step = None;
        }
        if self.snapshot.kind == Kind::Fill
            && view.phase == Phase::FillSetup
            && self.resume_configure.is_some()
        {
            self.snapshot.next_step = Some("fill-target".into());
        }
        self.snapshot.current_step = id;
        self.bound_watch_history()?;
        if matches!(view.phase, Phase::Idle | Phase::Passed) && !self.snapshot.outcome.terminal() {
            self.snapshot.message = "Waiting for the station's recorded completion result".into();
        }
        Ok(())
    }
    fn bound_watch_history(&mut self) -> Result<(), Error> {
        let limit = if self.snapshot.outcome.terminal() {
            super::sequence_progress::MAX_RESULT_BYTES
        } else {
            super::sequence_progress::MAX_ACTIVE_BYTES
        };
        while serde_json::to_vec(&self.snapshot)
            .map_err(|_| Error::Invalid)?
            .len()
            > limit
        {
            let Some(oldest) = self.snapshot.steps.iter_mut().find(|step| {
                !step.watch.is_empty()
                    && self.snapshot.current_step.as_deref() != Some(step.id.as_str())
            }) else {
                return Err(Error::Bounds);
            };
            oldest.watch.clear();
            self.snapshot.watch_history_truncated = true;
        }
        Ok(())
    }
    pub fn seal(&mut self, proof: TerminalProof) -> Result<&Snapshot, Error> {
        let mut failure_message = None;
        let outcome = match proof {
            TerminalProof::DurablePass { kind, event_id }
                if kind == self.snapshot.kind && !event_id.is_empty() && event_id.len() <= 128 =>
            {
                Outcome::Passed
            }
            TerminalProof::SafeCancelled(view)
                if view.phase == Phase::Cancelled && view.all_valves_confirmed_by_engine =>
            {
                Outcome::Cancelled
            }
            #[cfg(any(test, feature = "hitl_mode"))]
            TerminalProof::EngineCompleted {
                kind,
                before,
                after,
            } if kind == self.snapshot.kind
                && after.all_valves_confirmed_by_engine
                && matches!(
                    (kind, before.phase, after.phase),
                    (Kind::ValveSelfTest, Phase::SelfFinish, Phase::Idle)
                        | (Kind::NitrogenTest, Phase::Dumping, Phase::Passed)
                ) =>
            {
                Outcome::Passed
            }
            #[cfg(any(test, feature = "hitl_mode"))]
            TerminalProof::VerifiedFillTrim { trim, input }
                if self.snapshot.kind == Kind::Fill
                    && trim.phase == TrimPhase::Complete
                    && trim.desired == gse_sequence::CLOSED
                    && input.valves.iter().all(|v| {
                        v.is_some_and(|v| {
                            !v.open
                                && input
                                    .now_ms
                                    .checked_sub(v.at_ms)
                                    .is_some_and(|age| age <= 2000)
                        })
                    }) =>
            {
                Outcome::Passed
            }
            TerminalProof::Interrupted => Outcome::Interrupted,
            TerminalProof::Failed { message } if !message.trim().is_empty() => {
                failure_message = Some(bounded_message(&message));
                Outcome::Failed
            }
            _ => return Err(Error::Invalid),
        };
        if self.snapshot.outcome.terminal() {
            return if self.snapshot.outcome == outcome {
                Ok(&self.snapshot)
            } else {
                Err(Error::Conflict)
            };
        }
        self.snapshot.revision = self.snapshot.revision.checked_add(1).ok_or(Error::Bounds)?;
        self.snapshot.outcome = outcome;
        self.finish_steps();
        self.snapshot.cancel_allowed = false;
        self.snapshot.current_step = None;
        self.snapshot.next_step = None;
        self.snapshot.message = match outcome {
            Outcome::Passed => "Sequence passed",
            Outcome::Cancelled => "Safe cancellation confirmed",
            Outcome::Failed => "Sequence failed",
            _ => "Sequence interrupted",
        }
        .into();
        if let Some(message) = failure_message {
            self.snapshot.message = message;
        }
        self.snapshot.validate()?;
        Ok(&self.snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gse_sequence::{Action, Config, Engine, Inputs, RELIEVED, Sample, ValveState};
    fn fixture() -> (Engine, Inputs, Cursor) {
        let mut engine = Engine::default();
        engine.config = Config {
            dry_self_test_confirmed: true,
            pressure_ceiling_psi: Some(200.),
            maximum_zero_offset_psi: Some(8.),
            ..Config::default()
        };
        let input = Inputs {
            now_ms: 100,
            pressure: Some(Sample {
                value: 5.,
                at_ms: 100,
            }),
            valves: RELIEVED.map(|open| Some(ValveState { open, at_ms: 100 })),
            interlock: true,
            prelaunch: true,
        };
        engine.request(Action::SelfTest, input).unwrap();
        let key = RunKey {
            backend_instance: "11".repeat(16),
            reset_epoch: "22".repeat(8),
            run_id: "33".repeat(16),
        };
        let cursor = Cursor::accepted(
            key,
            Kind::ValveSelfTest,
            None,
            engine.progress_observation(input),
            engine.progress_nitrogen_plan(),
        )
        .unwrap();
        (engine, input, cursor)
    }
    #[test]
    fn idle_and_sticky_flags_never_create_a_pass_result() {
        let (engine, input, mut cursor) = fixture();
        let mut view = engine.progress_observation(input);
        view.phase = Phase::Idle;
        cursor.observe(view, Cause::Tick).unwrap();
        assert_eq!(cursor.snapshot().outcome, Outcome::Running);
        assert_eq!(
            cursor
                .seal(TerminalProof::EngineCompleted {
                    kind: Kind::ValveSelfTest,
                    before: view,
                    after: view
                })
                .err(),
            Some(Error::Invalid)
        );
        assert!(!cursor.snapshot().cancel_allowed);
        assert!(
            cursor
                .snapshot()
                .steps
                .iter()
                .all(|step| step.state != StepState::Passed)
        );
        assert_eq!(
            cursor
                .seal(TerminalProof::DurablePass {
                    kind: Kind::NitrogenTest,
                    event_id: "wrong-producer-event".into()
                })
                .err(),
            Some(Error::Invalid)
        );
        cursor
            .seal(TerminalProof::DurablePass {
                kind: Kind::ValveSelfTest,
                event_id: "actual-persisted-event-fixture".into(),
            })
            .unwrap();
        assert_eq!(cursor.snapshot().outcome, Outcome::Passed);
    }
    #[test]
    fn actual_self_test_tick_trace_accounts_for_every_planned_step() {
        let (mut engine, mut input, mut cursor) = fixture();
        let mut valves = RELIEVED;
        let mut completed = false;
        for _ in 0..1000 {
            input.now_ms += 100;
            input.pressure = Some(Sample {
                value: 5.,
                at_ms: input.now_ms,
            });
            input.valves = valves.map(|open| {
                Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                })
            });
            let before = engine.progress_observation(input);
            let effects = engine.tick(input);
            for (index, open) in effects.valves {
                valves[index] = open;
            }
            let after = engine.progress_observation(input);
            cursor.observe(after, Cause::Tick).unwrap();
            assert_ne!(after.phase, Phase::Fault);
            if before.phase == Phase::SelfFinish && after.phase == Phase::Idle {
                cursor
                    .seal(TerminalProof::EngineCompleted {
                        kind: Kind::ValveSelfTest,
                        before,
                        after,
                    })
                    .unwrap();
                completed = true;
                break;
            }
        }
        assert!(completed);
        let unfinished = cursor
            .snapshot()
            .steps
            .iter()
            .filter(|s| s.state != StepState::Passed)
            .map(|s| (&s.id, s.state))
            .collect::<Vec<_>>();
        assert!(
            unfinished.is_empty(),
            "actual completed trace has unaccounted steps: {unfinished:?}"
        );
    }
    #[test]
    fn maximum_actual_nitrogen_plan_preserves_all_step_outcomes_with_bounded_active_snapshot() {
        let (mut engine, mut input, _) = fixture();
        engine.manual_override();
        engine.config.nitrogen_steps = (1..=32)
            .map(|i| NitrogenStep {
                target_psi: i as f32 * 5.,
                settle_ms: 1000,
                hold_ms: 2000,
            })
            .collect();
        engine.request(Action::NitrogenTest, input).unwrap();
        let mut cursor = Cursor::accepted(
            RunKey {
                backend_instance: "1".repeat(32),
                reset_epoch: "2".repeat(16),
                run_id: "4".repeat(32),
            },
            Kind::NitrogenTest,
            None,
            engine.progress_observation(input),
            engine.progress_nitrogen_plan(),
        )
        .unwrap();
        let mut valves = RELIEVED;
        let mut completed = false;
        for _ in 0..3000 {
            input.now_ms += 100;
            let pressure = if matches!(
                engine.status.phase,
                Phase::Raising | Phase::Settling | Phase::Holding
            ) {
                engine
                    .status
                    .baseline
                    .as_ref()
                    .map_or(5., |b| b.average_psi)
                    + engine.status.current_step_psi
            } else {
                5.
            };
            input.pressure = Some(Sample {
                value: pressure,
                at_ms: input.now_ms,
            });
            input.valves = valves.map(|open| {
                Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                })
            });
            let before = engine.progress_observation(input);
            let effects = engine.tick(input);
            for (i, open) in effects.valves {
                valves[i] = open;
            }
            let after = engine.progress_observation(input);
            assert_ne!(after.phase, Phase::Fault);
            cursor.observe(after, Cause::Tick).unwrap_or_else(|error| {
                panic!(
                    "valid step {} phase {:?} reporting error {error:?}",
                    after.nitrogen_step_index, after.phase
                )
            });
            assert!(
                serde_json::to_vec(cursor.snapshot()).unwrap().len()
                    <= super::super::sequence_progress::MAX_ACTIVE_BYTES
            );
            if before.phase == Phase::Dumping && after.phase == Phase::Passed {
                cursor
                    .seal(TerminalProof::EngineCompleted {
                        kind: Kind::NitrogenTest,
                        before,
                        after,
                    })
                    .unwrap();
                completed = true;
                break;
            }
        }
        assert!(completed);
        assert_eq!(cursor.snapshot().steps.len(), 131);
        assert!(cursor.snapshot().watch_history_truncated);
        assert!(
            cursor
                .snapshot()
                .steps
                .iter()
                .all(|s| s.state == StepState::Passed)
        );
    }
    #[test]
    fn actual_turbo_controller_drives_real_steps_targets_and_pressurant_mass_without_early_pass() {
        use gse_sequence::turbocharge::{Context, Controller};
        let config = Config {
            pressure_ceiling_psi: Some(900.),
            turbocharge_max_added_mass_kg: Some(0.3),
            fill_settle_ms: 1000,
            ..Config::default()
        };
        let context = Context {
            reset_generation: 1,
            qualification_generation: 2,
            configuration: [3; 32],
        };
        let mut input = Inputs {
            now_ms: 100,
            pressure: Some(Sample {
                value: 100.,
                at_ms: 100,
            }),
            valves: gse_sequence::CLOSED.map(|open| Some(ValveState { open, at_ms: 100 })),
            interlock: true,
            prelaunch: true,
        };
        let mut mass = Sample {
            value: 1.,
            at_ms: 100,
        };
        let (mut controller, initial) =
            Controller::start(&config, 1., 200., input, Some(mass), context).unwrap();
        let mut engine = Engine::default();
        engine.status.phase = Phase::Turbocharge;
        let mut cursor = Cursor::accepted(
            RunKey {
                backend_instance: "1".repeat(32),
                reset_epoch: "2".repeat(16),
                run_id: "5".repeat(32),
            },
            Kind::Turbocharge,
            None,
            engine.progress_observation(input),
            &[],
        )
        .unwrap();
        cursor
            .observe_turbo(
                controller.progress_observation(),
                engine.progress_observation(input),
                Some(mass),
                input.now_ms,
                None,
            )
            .unwrap();
        let mut valves = gse_sequence::CLOSED;
        for (i, open) in initial.valves {
            valves[i] = open;
        }
        let mut saw_open = false;
        let mut done = false;
        for _ in 0..100 {
            input.now_ms += 100;
            input.valves = valves.map(|open| {
                Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                })
            });
            let pressure = if matches!(
                controller.phase(),
                TurboPhase::Raising | TurboPhase::ClosingNitrogen | TurboPhase::Settling
            ) {
                200.
            } else {
                100.
            };
            input.pressure = Some(Sample {
                value: pressure,
                at_ms: input.now_ms,
            });
            mass = Sample {
                value: if matches!(
                    controller.phase(),
                    TurboPhase::ClosingNitrogen | TurboPhase::Settling
                ) {
                    1.1
                } else {
                    1.
                },
                at_ms: input.now_ms,
            };
            let update = controller.tick(input, Some(mass), context);
            assert!(update.cancelled.is_none());
            for (i, open) in update.effects.valves {
                valves[i] = open;
            }
            cursor
                .observe_turbo(
                    controller.progress_observation(),
                    engine.progress_observation(input),
                    Some(mass),
                    input.now_ms,
                    None,
                )
                .unwrap();
            if controller.phase() == TurboPhase::Raising {
                saw_open = true;
                assert!(
                    !engine.progress_observation(input).valves[3].target_open,
                    "engine proxy intentionally differs from real controller"
                );
                let step = cursor
                    .snapshot()
                    .steps
                    .iter()
                    .find(|s| Some(&s.id) == cursor.snapshot().current_step.as_ref())
                    .unwrap();
                assert_eq!(
                    step.watch
                        .iter()
                        .find(|w| w.id == "board-output-3")
                        .unwrap()
                        .target,
                    Some(Value::Boolean(true))
                );
                assert_eq!(
                    step.watch
                        .iter()
                        .find(|w| w.id == "tank-pressure")
                        .unwrap()
                        .target,
                    Some(Value::Number(200.))
                );
                assert!(
                    step.watch
                        .iter()
                        .find(|w| w.id == "combined-tank-mass")
                        .unwrap()
                        .target
                        .is_none(),
                    "liquid inventory target is not a combined mass target"
                );
            }
            if update.completed.is_some() {
                done = true;
                break;
            }
        }
        assert!(saw_open && done);
        assert_eq!(
            cursor.snapshot().current_step.as_deref(),
            Some("turbo-record")
        );
        assert_eq!(
            cursor.snapshot().outcome,
            Outcome::Running,
            "controller completion alone is not durable inventory completion"
        );
        assert!(
            cursor.snapshot().steps[..5]
                .iter()
                .all(|s| s.state == StepState::Passed)
        );
        assert_eq!(cursor.snapshot().steps[5].state, StepState::Watching);
    }
    #[test]
    fn actual_fill_and_trim_trace_tracks_real_pulse_and_waits_for_completion_hook() {
        use gse_sequence::{Baseline, fill_trim::Controller};
        let mut engine = Engine::default();
        engine.config = Config {
            pressure_ceiling_psi: Some(200.),
            maximum_zero_offset_psi: Some(8.),
            fill_settle_ms: 1000,
            ..Config::default()
        };
        engine.nitrous_target_psi = 100.;
        engine.status.phase = Phase::Passed;
        engine.status.nitrogen_passed = true;
        engine.status.baseline = Some(Baseline {
            average_psi: 5.,
            min_psi: 5.,
            max_psi: 5.,
            noise_psi: 0.,
            samples: 100,
        });
        let mut input = Inputs {
            now_ms: 100,
            pressure: Some(Sample {
                value: 5.,
                at_ms: 100,
            }),
            valves: RELIEVED.map(|open| Some(ValveState { open, at_ms: 100 })),
            interlock: true,
            prelaunch: true,
        };
        let initial = engine.request(Action::StartFill, input).unwrap();
        let mut valves = RELIEVED;
        for (i, open) in initial.valves {
            valves[i] = open;
        }
        let mut cursor = Cursor::accepted(
            RunKey {
                backend_instance: "1".repeat(32),
                reset_epoch: "2".repeat(16),
                run_id: "6".repeat(32),
            },
            Kind::Fill,
            None,
            engine.progress_observation(input),
            &[],
        )
        .unwrap();
        for _ in 0..20 {
            input.now_ms += 100;
            input.pressure = Some(Sample {
                value: 5.,
                at_ms: input.now_ms,
            });
            input.valves = valves.map(|open| {
                Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                })
            });
            let effects = engine.tick(input);
            for (i, open) in effects.valves {
                valves[i] = open;
            }
            cursor
                .observe(engine.progress_observation(input), Cause::Tick)
                .unwrap();
            if engine.status.phase == Phase::Filling {
                break;
            }
        }
        assert_eq!(engine.status.phase, Phase::Filling);
        let (mut controller, initial) =
            Controller::start(input.now_ms, 10., 200., &engine.config).unwrap();
        engine.manual_override();
        engine.status.phase = Phase::PauseSetup;
        let first_mass = Sample {
            value: 10.2,
            at_ms: input.now_ms,
        };
        cursor
            .observe_trim(
                controller.progress_observation(),
                engine.progress_observation(input),
                Some(first_mass),
                input.now_ms,
                None,
            )
            .unwrap();
        for (i, open) in initial.valves {
            valves[i] = open;
        }
        let mut pulsed = false;
        let mut complete = None;
        for _ in 0..200 {
            input.now_ms += 100;
            input.pressure = Some(Sample {
                value: 5.,
                at_ms: input.now_ms,
            });
            input.valves = valves.map(|open| {
                Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                })
            });
            if matches!(
                controller.phase(),
                TrimPhase::Venting | TrimPhase::VentClosing
            ) {
                pulsed = true;
            }
            let mass = Sample {
                value: if pulsed { 10. } else { 10.2 },
                at_ms: input.now_ms,
            };
            let update = controller.tick(input, Some(mass));
            assert!(update.fault.is_none(), "{:?}", update.fault);
            for (i, open) in update.effects.valves {
                valves[i] = open;
            }
            cursor
                .observe_trim(
                    controller.progress_observation(),
                    engine.progress_observation(input),
                    Some(mass),
                    input.now_ms,
                    update.fault.as_deref(),
                )
                .unwrap();
            if controller.phase() == TrimPhase::VentOpening {
                let current = cursor
                    .snapshot()
                    .steps
                    .iter()
                    .find(|s| Some(&s.id) == cursor.snapshot().current_step.as_ref())
                    .unwrap();
                assert_eq!(
                    current
                        .watch
                        .iter()
                        .find(|w| w.id == "board-output-1")
                        .unwrap()
                        .target,
                    Some(Value::Boolean(true))
                );
                assert!(
                    cursor.snapshot().cancel_allowed,
                    "actual active trim remains safely cancellable"
                );
            }
            if update.completed {
                complete = Some(controller.progress_observation());
                break;
            }
        }
        let actual = complete.expect("actual controller completes after one observed trim pulse");
        assert!(pulsed);
        assert_eq!(actual.pulses, 1);
        assert_eq!(cursor.snapshot().steps.len(), 9);
        assert_eq!(
            cursor.snapshot().current_step.as_deref(),
            Some("fill-record")
        );
        assert_eq!(cursor.snapshot().outcome, Outcome::Running);
        assert_eq!(
            cursor.snapshot().steps.last().unwrap().state,
            StepState::Watching
        );
        assert!(
            cursor.snapshot().steps[..8]
                .iter()
                .all(|s| s.state == StepState::Passed)
        );
        let mut missing = input;
        missing.valves[0] = None;
        assert_eq!(
            cursor
                .seal(TerminalProof::VerifiedFillTrim {
                    trim: actual,
                    input: missing
                })
                .err(),
            Some(Error::Invalid)
        );
        cursor
            .seal(TerminalProof::VerifiedFillTrim {
                trim: actual,
                input,
            })
            .unwrap();
        assert_eq!(cursor.snapshot().outcome, Outcome::Passed);
        assert!(
            cursor
                .snapshot()
                .steps
                .iter()
                .all(|s| s.state == StepState::Passed)
        );
    }
    #[test]
    fn real_pause_resume_keeps_run_identity_and_requires_new_configuration_evidence() {
        let mut engine = Engine::default();
        engine.config.pressure_ceiling_psi = Some(200.);
        engine.config.maximum_zero_offset_psi = Some(8.);
        engine.nitrous_target_psi = 100.;
        engine.status.phase = Phase::Passed;
        engine.status.nitrogen_passed = true;
        engine.status.baseline = Some(gse_sequence::Baseline {
            average_psi: 0.,
            min_psi: 0.,
            max_psi: 0.,
            noise_psi: 0.,
            samples: 20,
        });
        let mut input = Inputs {
            now_ms: 100,
            pressure: Some(Sample {
                value: 0.,
                at_ms: 100,
            }),
            valves: gse_sequence::CLOSED.map(|open| Some(ValveState { open, at_ms: 100 })),
            interlock: true,
            prelaunch: true,
        };
        let mut effects = engine.request(Action::StartFill, input).unwrap();
        let key = RunKey {
            backend_instance: "1".repeat(32),
            reset_epoch: "2".repeat(16),
            run_id: "7".repeat(32),
        };
        let mut cursor = Cursor::accepted(
            key.clone(),
            Kind::Fill,
            None,
            engine.progress_observation(input),
            &[],
        )
        .unwrap();
        let mut valves = gse_sequence::CLOSED;
        for _ in 0..20 {
            input.now_ms += 100;
            for (i, open) in effects.valves {
                valves[i] = open;
            }
            input.valves = valves.map(|open| {
                Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                })
            });
            input.pressure = Some(Sample {
                value: 0.,
                at_ms: input.now_ms,
            });
            effects = engine.tick(input);
            cursor
                .observe(engine.progress_observation(input), Cause::Tick)
                .unwrap();
            if engine.status.phase == Phase::Filling {
                break;
            }
        }
        assert_eq!(engine.status.phase, Phase::Filling);
        effects = engine.request(Action::PauseFill, input).unwrap();
        cursor
            .observe(engine.progress_observation(input), Cause::PauseAccepted)
            .unwrap();
        for _ in 0..20 {
            input.now_ms += 100;
            for (i, open) in effects.valves {
                valves[i] = open;
            }
            input.valves = valves.map(|open| {
                Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                })
            });
            input.pressure = Some(Sample {
                value: 0.,
                at_ms: input.now_ms,
            });
            effects = engine.tick(input);
            cursor
                .observe(engine.progress_observation(input), Cause::Tick)
                .unwrap();
            if engine.status.phase == Phase::Paused {
                break;
            }
        }
        assert_eq!(cursor.snapshot().outcome, Outcome::Paused);
        engine.request(Action::StartFill, input).unwrap();
        cursor
            .observe(engine.progress_observation(input), Cause::ResumeAccepted)
            .unwrap();
        assert_eq!(cursor.snapshot().key, key);
        let current = cursor.snapshot().current_step.as_ref().unwrap();
        assert!(current.starts_with("control-resume-"));
        assert_eq!(
            cursor
                .snapshot()
                .steps
                .iter()
                .find(|s| &s.id == current)
                .unwrap()
                .state,
            StepState::Watching
        );
        assert_eq!(cursor.snapshot().next_step.as_deref(), Some("fill-target"));
    }
    #[test]
    fn cancel_submission_waits_for_actual_engine_confirmation() {
        let (mut engine, mut input, mut cursor) = fixture();
        let mut effects = engine.request(Action::CancelFill, input).unwrap();
        cursor
            .observe(engine.progress_observation(input), Cause::CancelAccepted)
            .unwrap();
        assert_eq!(cursor.snapshot().outcome, Outcome::Cancelling);
        assert_eq!(
            cursor
                .seal(TerminalProof::SafeCancelled(
                    engine.progress_observation(input)
                ))
                .err(),
            Some(Error::Invalid)
        );
        // The actual engine dispatches one valve at a time and requires a new
        // causal readback for each one, even when the cached value already matches.
        for _ in 0..6 {
            input.now_ms += 100;
            for (index, open) in effects.valves {
                input.valves[index] = Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                });
            }
            input.pressure = Some(Sample {
                value: 5.,
                at_ms: input.now_ms,
            });
            effects = engine.tick(input);
            if engine.status.phase == Phase::Cancelled {
                break;
            }
        }
        let actual = engine.progress_observation(input);
        assert_eq!(actual.phase, Phase::Cancelled);
        cursor.observe(actual, Cause::Tick).unwrap();
        assert_ne!(cursor.snapshot().outcome, Outcome::Cancelled);
        cursor.seal(TerminalProof::SafeCancelled(actual)).unwrap();
        assert_eq!(cursor.snapshot().outcome, Outcome::Cancelled);
    }
    #[test]
    fn board_readback_is_not_marked_commanded_and_missing_remains_unknown() {
        let (engine, mut input, mut cursor) = fixture();
        input.valves = [None; 5];
        input.pressure = None;
        cursor
            .observe(engine.progress_observation(input), Cause::Tick)
            .unwrap();
        let current = cursor.snapshot().current_step.as_ref().unwrap();
        let step = cursor
            .snapshot()
            .steps
            .iter()
            .find(|s| &s.id == current)
            .unwrap();
        assert!(
            step.watch
                .iter()
                .all(|w| w.observed.is_none() && w.evidence == Evidence::Unknown)
        );
    }
}
