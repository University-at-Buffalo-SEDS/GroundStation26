//! Separate firmware-psi lane and operator-requested, bounded nitrogen top-up.
use super::*;
use gse_sequence::turbocharge::{Context, Controller};

pub(crate) fn observe_valve_pressure_ingress(
    state: &AppState,
    sender: &str,
    rows: &[(String, Vec<Option<f32>>)],
    source_ms: i64,
    receipt: ValveReceipt,
) {
    if source_ms < 0
        || receipt.network_ms < 0
        || sender != crate::types::Board::ValveBoard.sender_id()
    {
        return;
    }
    let Some((_, values)) = rows.iter().find(|(kind, _)| kind == "FUEL_TANK_PRESSURE") else {
        return;
    };
    let value = values
        .first()
        .copied()
        .flatten()
        .filter(|v| v.is_finite() && *v >= 0.0);
    let mut rt = state.gse.lock().unwrap();
    if rt.last_valve_pressure_source_ms.is_some_and(|last| {
        source_ms < last && (source_ms < 1_577_836_800_000 || last < 1_577_836_800_000)
    }) {
        rt.valve_pressure = None;
        rt.turbo_evidence = None;
        rt.valve_pressure_epoch = rt.valve_pressure_epoch.saturating_add(1);
        // A backwards capture is never fresh evidence, including boot-clock reset.
        rt.last_valve_pressure_source_ms = None;
        return;
    }
    let now = rt.now();
    if let Some(at_ms) = scalar_capture_at(
        rt.clock,
        now,
        Some(source_ms),
        receipt,
        &mut rt.last_valve_pressure_source_ms,
    ) {
        rt.valve_pressure = value.map(|value| Sample { value, at_ms });
    }
}
pub(crate) fn mass_calibration_signature(
    cfg: &crate::loadcell::LoadcellCalibrationFile,
) -> [u8; 32] {
    let bytes = serde_json::to_vec(cfg).expect("serializable loadcell calibration");
    ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .try_into()
        .unwrap()
}
fn mass_binding_current(state: &AppState, rt: &Runtime) -> bool {
    rt.mass_calibration
        == Some(mass_calibration_signature(
            &state.loadcell_calibration.lock().unwrap(),
        ))
}
fn route_binding(state: &AppState) -> Vec<(String, Vec<String>)> {
    let Some(router) = state.topology_router.get() else {
        return Vec::new();
    };
    let owners = [
        crate::telemetry_schema::endpoint("VALVE_BOARD"),
        crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
    ];
    let mut routes: Vec<_> = router
        .export_topology()
        .routes
        .into_iter()
        .filter_map(|route| {
            let mut endpoints: Vec<_> = route
                .reachable_endpoints
                .iter()
                .filter(|e| owners.contains(e))
                .map(|e| e.as_str().to_string())
                .collect();
            endpoints.sort();
            (!endpoints.is_empty()).then(|| (format!("{}", route.side_id), endpoints))
        })
        .collect();
    routes.sort();
    routes
}
/// Preserve every original serialized field/order. Only the newly added inert
/// Cold Flow preference is outside the existing Turbocharge evidence context.
pub(super) fn config_for_binding(config: &Config) -> serde_json::Value {
    let value = serde_json::json!(config);
    let object = value.as_object().unwrap();
    let mut filtered = serde_json::Map::new();
    for (key, value) in object {
        if key != "cold_flow_target_psi" {
            filtered.insert(key.clone(), value.clone());
        }
    }
    serde_json::Value::Object(filtered)
}
pub(super) fn only_cold_target_changed(a: &Config, b: &Config) -> bool {
    a.cold_flow_target_psi != b.cold_flow_target_psi
        && a.nitrogen_target_psi == b.nitrogen_target_psi
        && a.nitrogen_steps == b.nitrogen_steps
        && a.fill_overfill_kg == b.fill_overfill_kg
        && a.fill_tolerance_kg == b.fill_tolerance_kg
        && a.fill_settle_ms == b.fill_settle_ms
        && a.turbocharge_max_added_mass_kg == b.turbocharge_max_added_mass_kg
        && a.turbocharge_pressure_deadband_psi == b.turbocharge_pressure_deadband_psi
        && a.pressure_step_psi == b.pressure_step_psi
        && a.pressure_ceiling_psi == b.pressure_ceiling_psi
        && a.maximum_zero_offset_psi == b.maximum_zero_offset_psi
        && a.dry_self_test_confirmed == b.dry_self_test_confirmed
        && a.grouped_panel == b.grouped_panel
}
/// Existing context comparison, used only by inactive configuration saving.
/// Calibration/targets/routes/capture epochs and qualification generation all
/// remain bound; an inert preference cannot preserve an already-invalid context.
pub(super) fn evidence_context_current(state: &AppState, rt: &Runtime) -> bool {
    rt.turbo_evidence
        .as_ref()
        .is_none_or(|e| e.valid() && e.context == context(state, rt))
}
fn capture_binding(
    state: &AppState,
    rt: &Runtime,
) -> (
    serde_json::Value,
    [u8; 32],
    crate::fill_targets::FillTargetsConfig,
) {
    let configured = state.fill_targets.lock().unwrap().clone();
    let calibration = state.loadcell_calibration.lock().unwrap().clone();
    let targets = crate::flight_sim::effective_fill_targets(
        configured,
        &calibration,
        *state.state.lock().unwrap(),
    );
    let signature = mass_calibration_signature(&calibration);
    let binding = serde_json::json!({"config":config_for_binding(&rt.engine.config),"targets":targets,"calibration":calibration,
        "pressure_source":"VB/FUEL_TANK_PRESSURE/firmware_psi","pressure_capture_epoch":rt.valve_pressure_epoch,"mass_capture_epoch":rt.mass_source_epoch,"routes":route_binding(state)});
    (binding, signature, targets)
}
#[cfg(test)]
pub(super) fn fixture_context(state: &AppState, rt: &Runtime) -> Context {
    context(state, rt)
}
fn context(state: &AppState, rt: &Runtime) -> Context {
    context_from_binding(state, rt, &capture_binding(state, rt).0)
}
fn context_from_binding(state: &AppState, _rt: &Runtime, binding: &serde_json::Value) -> Context {
    let bytes = serde_json::to_vec(binding).expect("serializable GSE binding");
    let mut configuration = [0; 32];
    configuration.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
    #[cfg(not(feature = "hitl_mode"))]
    let qualification_generation = _rt.operational.saved.qualification.generation();
    #[cfg(feature = "hitl_mode")]
    let qualification_generation = 0;
    Context {
        reset_generation: super::automatic_generation(state),
        qualification_generation,
        configuration,
    }
}
fn input(rt: &Runtime, input: Inputs) -> Inputs {
    Inputs {
        pressure: rt.valve_pressure,
        ..input
    }
}
fn routes_current(state: &AppState) -> bool {
    let Some(router) = state.topology_router.get() else {
        return false;
    };
    let routes = router.export_topology();
    ["VALVE_BOARD", "ACTUATOR_BOARD"].into_iter().all(|name| {
        routes.routes.iter().any(|side| {
            side.reachable_endpoints
                .contains(&crate::telemetry_schema::endpoint(name))
        })
    }) && [
        crate::types::Board::ValveBoard,
        crate::types::Board::ActuatorBoard,
        crate::types::Board::DaqBoard,
        crate::types::Board::GatewayBoard,
    ]
    .into_iter()
    .all(|b| state.operational_board_recent(b, 3000))
}

fn prerequisites(state: &AppState, rt: &Runtime, input: Inputs) -> bool {
    if super::manual_stop_pending(state)
        || matches!(super::automatic_generation(state), 0 | u64::MAX)
        || !mass_binding_current(state, rt)
        || !input.prelaunch
        || !input.interlock
        || state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
        || (rt.turbo_inventory_pending && rt.turbo_evidence.is_none())
        || rt.turbocharge.is_some()
        || rt.fill_trim.is_some()
        || !rt.engine.nitrous_session()
        || rt.engine.status.phase != Phase::Paused
    {
        return false;
    }
    if !routes_current(state) {
        return false;
    }
    #[cfg(not(feature = "hitl_mode"))]
    if !rt.operational.topology_stable
        || !rt.operational.startup_configured
        || !rt.operational.saved.pending_checklists.is_empty()
        || !crate::checklists::paired::qualification_allowed(
            rt.operational.saved.checklist_digest.as_deref(),
            rt.operational.has_checklist_evidence(),
        )
        || rt.operational.persist_error.is_some()
        || !rt
            .operational
            .saved
            .qualification
            .completed(Milestone::FillTarget)
        || rt.operational.saved.qualification.fault().is_some()
        || rt.operational.saved.automatic_abort_claimed
    {
        return false;
    }
    true
}
pub(super) fn allowed(state: &AppState, rt: &Runtime, original: Inputs) -> bool {
    if !prerequisites(state, rt, original) {
        return false;
    }
    let (binding, signature, targets) = capture_binding(state, rt);
    if rt.mass_calibration != Some(signature) {
        return false;
    }
    let target = targets.nitrous;
    Controller::start_with_inventory(
        &rt.engine.config,
        target.target_mass_kg,
        target.target_pressure_psi,
        input(rt, original),
        rt.fresh_mass(original.now_ms),
        context_from_binding(state, rt, &binding),
        rt.turbo_evidence.as_ref(),
    )
    .is_ok()
}
pub(super) fn request(state: &Arc<AppState>) {
    let progress_reserved = super::progress::reserve(state);
    let policy = state.action_policy_snapshot();
    let result = (|| -> Result<Effects, String> {
        let mut rt = state.gse.lock().unwrap();
        let original = rt.input(prelaunch(state), super::interlock(state, &policy));
        if !prerequisites(state, &rt, original) {
            return Err("Turbocharge requires paused qualified inventory, fresh routes and interlocks; interrupted nitrogen accounting requires requalification/reset".into());
        }
        let (binding, signature, targets) = capture_binding(state, &rt);
        if rt.mass_calibration != Some(signature) {
            return Err("Fresh mass must use the captured current calibration".into());
        }
        let target = targets.nitrous;
        let reporting_input = input(&rt, original);
        let reporting_mass = rt.fresh_mass(original.now_ms);
        let (controller, effects) = Controller::start_with_inventory(
            &rt.engine.config,
            target.target_mass_kg,
            target.target_pressure_psi,
            reporting_input,
            reporting_mass,
            context_from_binding(state, &rt, &binding),
            rt.turbo_evidence.as_ref(),
        )?;
        rt.turbo_binding = Some(binding);
        write_audit(&rt, "pending", None, Some(controller.inventory_anchor()))?;
        rt.turbo_inventory_pending = true;
        #[cfg(not(feature = "hitl_mode"))]
        {
            let generation = rt.operational.saved.qualification.generation();
            rt.operational
                .saved
                .qualification
                .begin_fill(generation)
                .map_err(str::to_string)?;
            rt.operational.persist()?;
        }
        let reporting_controller = controller.progress_observation();
        rt.turbocharge = Some(controller);
        rt.engine.manual_override();
        rt.engine.claimed = true;
        rt.engine.status.phase = Phase::Turbocharge;
        rt.engine.status.current_step_psi = target.target_pressure_psi;
        rt.engine.status.message = "Turbocharge: confirming all five valves closed".into();
        rt.progress_start(
            progress_reserved,
            crate::sequence_progress::Kind::Turbocharge,
            original,
        );
        let reporting_view = rt.engine.progress_observation(reporting_input);
        rt.progress.observe_turbo(
            reporting_controller,
            reporting_view,
            reporting_mass,
            reporting_input.now_ms,
            None,
            crate::sequence_progress_service::now_ms(),
        );
        Ok(effects)
    })();
    match result {
        Ok(effects) => apply(state, effects),
        Err(reason) => {
            state.gse.lock().unwrap().engine.status.message = reason.clone();
            state.add_temporary_notification(reason);
        }
    }
}
fn audit_path(rt: &Runtime) -> Result<std::path::PathBuf, String> {
    #[cfg(test)]
    {
        rt.turbo_audit_path
            .clone()
            .ok_or("Turbocharge audit fixture path missing".into())
    }
    #[cfg(not(test))]
    {
        let _ = rt;
        Ok(config_path().with_file_name("gse_turbocharge_evidence.json"))
    }
}
pub(super) fn restore_pending_inventory(state: &AppState) {
    let mut rt = state.gse.lock().unwrap();
    // Never restore chemical inventory authority from a prior process/epoch.
    // Even a malformed record remains an explicit requalification barrier.
    if audit_path(&rt).is_ok_and(|path| path.exists()) {
        rt.turbo_inventory_pending = true;
    }
}
pub(super) fn liquid_inventory_requalified(rt: &mut Runtime) -> Result<(), String> {
    if !rt.turbo_inventory_pending {
        return Ok(());
    }
    let path = audit_path(rt)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    rt.turbo_inventory_pending = false;
    rt.turbo_evidence = None;
    #[cfg(not(feature = "hitl_mode"))]
    {
        rt.operational.saved.turbocharge = None;
        rt.operational.saved.turbocharge_binding = None;
    }
    Ok(())
}
fn write_audit(
    rt: &Runtime,
    phase: &str,
    evidence: Option<&gse_sequence::turbocharge::Evidence>,
    anchor: Option<Sample>,
) -> Result<(), String> {
    let path = audit_path(rt)?;
    let record = serde_json::json!({"version":1,"state":phase,"evidence":evidence,"prior_evidence":rt.turbo_evidence,"original_inventory":anchor.map(|s|serde_json::json!({"kg":s.value,"at_ms":s.at_ms})),"binding":rt.turbo_binding,
        "mass_source_timestamp_ms":rt.last_mass_source_ms,"pressure_source_timestamp_ms":rt.last_valve_pressure_source_ms,
        "recorded_at_ms":crate::telemetry_task::get_current_timestamp_ms(),"pressure_units":"valve-board psi","mass_units":"calibrated kg"});
    let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("Turbocharge audit record exceeds its bounded size".into());
    }
    let parent = path
        .parent()
        .ok_or("Turbocharge audit path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("upload");
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(|e| e.to_string())
}
/// A pressure plateau stops liquid admission but never qualifies missing mass.
pub(super) fn pressure_cutoff(rt: &mut Runtime, input: Inputs) -> Option<Effects> {
    if !matches!(rt.engine.status.phase, Phase::FillSetup | Phase::Filling)
        || !rt.ground_station_control
        || !rt.engine.nitrous_target_psi.is_finite()
        || rt.engine.nitrous_target_psi <= 0.0
        || !rt.valve_pressure.is_some_and(|p| {
            p.value.is_finite()
                && p.at_ms <= input.now_ms
                && input.now_ms - p.at_ms <= 2000
                && p.value >= rt.engine.nitrous_target_psi
        })
    {
        return None;
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        rt.operational.target_reached = None;
    }
    let effects = rt.engine.request(Action::PauseFill, input).ok();
    if effects.is_some() {
        rt.progress_observe(
            input,
            crate::sequence_progress_projection::Cause::PauseAccepted,
        );
    }
    effects
}
fn invalidate_for_manual_stop(rt: &mut Runtime) -> bool {
    let changed = rt.turbocharge.take().is_some()
        | rt.turbo_evidence.take().is_some()
        | rt.turbo_binding.is_some();
    let owned =
        changed || rt.turbo_inventory_pending || rt.engine.status.phase == Phase::Turbocharge;
    if owned {
        rt.progress.complete(
            crate::sequence_progress_projection::TerminalProof::Interrupted,
            crate::sequence_progress_service::now_ms(),
        );
    }
    if changed {
        // Manual Stop owns all resulting physical effects. Keep the durable
        // inventory barrier; cancellation cannot certify prior mass movement.
        rt.turbo_binding = None;
        #[cfg(not(feature = "hitl_mode"))]
        {
            rt.operational.saved.turbocharge = None;
            rt.operational.saved.turbocharge_binding = None;
            if rt
                .operational
                .saved
                .qualification
                .completed(Milestone::FillTarget)
            {
                let generation = rt.operational.saved.qualification.generation();
                let _ = rt.operational.saved.qualification.begin_fill(generation);
            }
            let _ = rt.operational.persist();
        }
        rt.engine.status.message =
            "Manual Stop pending; Turbocharge cancelled and inventory requires requalification"
                .into();
    }
    owned
}
pub(super) fn tick(state: &Arc<AppState>, interlock: bool) -> bool {
    if super::manual_stop_pending(state) {
        return invalidate_for_manual_stop(&mut state.gse.lock().unwrap());
    }
    let effects = {
        let mut rt = state.gse.lock().unwrap();
        #[cfg(not(feature = "hitl_mode"))]
        let blocked = rt.operational.saved.qualification.fault().is_some()
            || rt.operational.saved.automatic_abort_claimed;
        #[cfg(feature = "hitl_mode")]
        let blocked = false;
        if blocked || rt.engine.status.phase == Phase::Fault {
            let active = rt.turbocharge.take().is_some();
            if active {
                rt.progress.complete(crate::sequence_progress_projection::TerminalProof::Failed{message:"Turbocharge stopped by the current qualification or fault gate; verify actual output reports.".into()},crate::sequence_progress_service::now_ms());
            }
            rt.turbo_evidence = None;
            return active;
        }
        let Some(mut controller) = rt.turbocharge.take() else {
            return false;
        };
        rt.engine.status.valves = rt.valves.map(|v| v.map(|v| v.open));
        let original = rt.input(
            prelaunch(state),
            interlock
                && routes_current(state)
                && rt.valve_pressure_epoch != u64::MAX
                && rt.mass_source_epoch != u64::MAX,
        );
        let reporting_input = input(&rt, original);
        let reporting_mass = rt
            .fresh_mass(original.now_ms)
            .filter(|_| mass_binding_current(state, &rt));
        let update = controller.tick(reporting_input, reporting_mass, context(state, &rt));
        let reporting_view = rt.engine.progress_observation(reporting_input);
        rt.progress.observe_turbo(
            controller.progress_observation(),
            reporting_view,
            reporting_mass,
            reporting_input.now_ms,
            update.cancelled.as_deref(),
            crate::sequence_progress_service::now_ms(),
        );
        if update.severe_pressure {
            rt.turbo_evidence = None;
            #[cfg(not(feature = "hitl_mode"))]
            {
                rt.operational
                    .saved
                    .qualification
                    .latch(Fault::UnsafePressure);
                rt.operational.running = false;
                let _ = rt.operational.persist();
            }
            let effects = rt
                .engine
                .fault(original.now_ms, "Turbocharge pressure ceiling reached");
            drop(rt);
            apply(state, effects);
            return true;
        }
        if let Some(evidence) = update.completed {
            if super::manual_stop_pending(state) {
                invalidate_for_manual_stop(&mut rt);
                return true;
            }
            if let Err(error) = write_audit(&rt, "complete", Some(&evidence), None) {
                rt.turbo_evidence = None;
                rt.engine.status.phase = Phase::Paused;
                rt.engine.status.message =
                    format!("Turbocharge closed; audit storage failed: {error}");
                rt.progress_failed_current();
                return true;
            }
            #[cfg(not(feature = "hitl_mode"))]
            {
                let generation = rt.operational.saved.qualification.generation();
                if let Err(error) = rt
                    .operational
                    .saved
                    .qualification
                    .complete(Milestone::FillTarget, generation)
                {
                    rt.turbo_evidence = None;
                    rt.engine.status.phase = Phase::Paused;
                    rt.engine.status.message =
                        format!("Turbocharge closed; qualification changed: {error}");
                    rt.progress_failed_current();
                    return true;
                }
                rt.operational.saved.turbocharge = Some(evidence.clone());
                rt.operational.saved.turbocharge_binding = Some(serde_json::json!({
                    "configuration":rt.turbo_binding,"pressure_source_timestamp_ms":rt.last_valve_pressure_source_ms,
                    "mass_source_timestamp_ms":rt.last_mass_source_ms,"recorded_at_ms":crate::telemetry_task::get_current_timestamp_ms(),
                    "pressure_units":"valve-board psi","mass_units":"calibrated kg",
                }));
                if let Err(error) = rt.operational.persist() {
                    rt.turbo_evidence = None;
                    rt.engine.status.phase = Phase::Paused;
                    rt.engine.status.message =
                        format!("Turbocharge closed; evidence storage failed: {error}");
                    rt.progress_failed_current();
                    return true;
                }
            }
            rt.turbo_evidence = Some(evidence);
            rt.progress_pass(
                crate::sequence_progress::Kind::Turbocharge,
                format!("turbocharge-audit:{}", original.now_ms),
            );
            rt.engine.status.phase = Phase::Paused;
            rt.engine.status.message =
                "Turbocharge complete: nitrogen closed, fresh mass settled".into();
        } else if let Some(reason) = update.cancelled {
            rt.turbo_evidence = None;
            rt.engine.status.message = reason.clone();
            // Controller cancellation requests closes only, never Abort, dump
            // or launch. Engine owns the ordinary ACK-gated pause continuation.
            if !update.effects.valves.is_empty() {
                let reason = reason.trim_end_matches("; pausing").to_string();
                let effects = super::pause_for_failure(&mut rt, original, &reason);
                #[cfg(not(feature = "hitl_mode"))]
                {
                    rt.operational.run_generation = rt.operational.saved.qualification.generation();
                }
                drop(rt);
                apply(state, effects);
                return true;
            }
            rt.engine.status.phase = Phase::Paused;
        } else {
            rt.engine.status.message =
                format!("Turbocharge: {:?} (valve-board psi)", controller.phase());
            rt.turbocharge = Some(controller);
        }
        update.effects
    };
    apply(state, effects);
    true
}
fn inventory_matches(state: &AppState, rt: &Runtime, mass: Sample) -> bool {
    mass_binding_current(state, rt)
        && rt.turbo_evidence.as_ref().is_some_and(|e| {
            e.accepts_mass(
                context(state, rt),
                mass,
                rt.now(),
                rt.engine.config.fill_tolerance_kg,
            )
        })
}
/// A completed/interrupted Turbo hold retains the SAME raw pressure authority.
/// Unavailable telemetry holds CLOSED; only actual fresh overpressure invokes
/// the established severe relief policy. Never substitute calibrated IADC.
pub(super) fn guard_closed_pressure(state: &Arc<AppState>) -> bool {
    let mut rt = state.gse.lock().unwrap();
    if !rt.turbo_inventory_pending
        || !matches!(rt.engine.status.phase, Phase::Paused | Phase::PauseSetup)
    {
        return false;
    }
    if !prelaunch(state)
        || state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    {
        return true;
    }
    let now = rt.now();
    let pressure = rt
        .valve_pressure
        .filter(|p| p.value.is_finite() && p.at_ms <= now && now - p.at_ms <= 2000);
    let limit = rt.engine.config.pressure_limit(
        state.fill_targets_snapshot().nitrous.target_pressure_psi,
        50.0,
    );
    if pressure
        .zip(limit.as_ref().ok().copied())
        .is_some_and(|(p, limit)| p.value >= limit)
    {
        rt.turbo_evidence = None;
        #[cfg(not(feature = "hitl_mode"))]
        {
            rt.operational
                .saved
                .qualification
                .latch(Fault::UnsafePressure);
            rt.operational.running = false;
            let _ = rt.operational.persist();
        }
        let effects = rt
            .engine
            .fault(now, "Valve-board Turbocharge pressure ceiling reached");
        drop(rt);
        apply(state, effects);
        return true;
    }
    let evidence_mismatch = rt.turbo_evidence.is_some()
        && rt
            .fresh_mass(now)
            .is_none_or(|m| !inventory_matches(state, &rt, m));
    if pressure.is_some() && limit.is_ok() && !evidence_mismatch {
        return false;
    }
    rt.turbo_evidence = None;
    #[cfg(not(feature = "hitl_mode"))]
    if rt
        .operational
        .saved
        .qualification
        .completed(Milestone::FillTarget)
    {
        let generation = rt.operational.saved.qualification.generation();
        let _ = rt.operational.saved.qualification.begin_fill(generation);
        let _ = rt.operational.persist();
    }
    rt.engine.status.message="Turbocharge pressure/inventory evidence unavailable or changed; holding closed, requalification required".into();
    if rt.engine.status.phase == Phase::PauseSetup {
        return false;
    }
    if rt
        .valves
        .iter()
        .all(|v| v.is_some_and(|v| !v.open && v.at_ms <= now && now - v.at_ms <= 2000))
    {
        return true;
    }
    rt.engine.status.phase = Phase::Filling;
    let input = rt.input(true, true);
    let effects = rt
        .engine
        .request(Action::PauseFill, input)
        .unwrap_or_default();
    #[cfg(not(feature = "hitl_mode"))]
    {
        rt.operational.running = true;
        rt.operational.run_generation = rt.operational.saved.qualification.generation();
        rt.operational.target_reached = None;
    }
    drop(rt);
    apply(state, effects);
    true
}
pub(super) fn accepts_mass(state: &AppState, rt: &Runtime, mass: Sample) -> bool {
    mass_binding_current(state, rt)
        && rt.valve_pressure.is_some_and(|p| {
            p.at_ms <= rt.now()
                && rt.now() - p.at_ms <= 2000
                && p.value >= state.fill_targets_snapshot().nitrous.target_pressure_psi
                    - rt.engine.config.turbocharge_pressure_deadband_psi
        })
        && rt.valves.iter().all(|v| {
            v.is_some_and(|v| !v.open && v.at_ms <= rt.now() && rt.now() - v.at_ms <= 2000)
        })
        && rt.turbo_evidence.as_ref().is_some_and(|e| {
            e.accepts_mass(
                context(state, rt),
                mass,
                rt.now(),
                rt.engine.config.fill_tolerance_kg,
            )
        })
}
pub(crate) fn mass_ingress_snapshot(rt: &Runtime) -> serde_json::Value {
    serde_json::json!({"kg":rt.mass.map(|m|m.value),"source":"DAQ selected loadcell","source_timestamp_ms":rt.last_mass_source_ms,"age_ms":rt.mass.and_then(|m|rt.now().checked_sub(m.at_ms)),"calibration_bound":rt.mass_calibration.is_some()})
}
pub(super) fn decorate_status(state: &AppState, response: &mut serde_json::Value) {
    let rt = state.gse.lock().unwrap();
    let now = rt.now();
    response["fill_mass_ingress"] = mass_ingress_snapshot(&rt);
    response["pressure_psi"] = serde_json::json!(
        rt.pressure
            .filter(|p| p.at_ms <= now && now - p.at_ms <= 2000)
            .map(|p| p.value)
    );
    response["control_pressure_source"] = serde_json::json!(if rt.turbo_inventory_pending
        && matches!(
            rt.engine.status.phase,
            Phase::Turbocharge | Phase::PauseSetup | Phase::Paused
        ) {
        "VB/FUEL_TANK_PRESSURE/firmware_psi"
    } else {
        "GS calibrated IADC"
    });
    response["turbocharge_evidence_valid"] = serde_json::json!(
        rt.fresh_mass(now)
            .is_some_and(|mass| accepts_mass(state, &rt, mass))
    );
    response["valve_pressure"]=rt.valve_pressure.map_or(serde_json::Value::Null,|p|serde_json::json!({"psi":p.value,"source":"VB/FUEL_TANK_PRESSURE","source_timestamp_ms":rt.last_valve_pressure_source_ms,"age_ms":now.checked_sub(p.at_ms)}));
    response["turbocharge"] = serde_json::json!({"active":rt.turbocharge.is_some(),"pending_inventory_qualification":rt.turbo_inventory_pending,"phase":rt.turbocharge.as_ref().map(Controller::phase),"evidence":rt.turbo_evidence,"max_added_nitrogen_mass_kg":rt.engine.config.turbocharge_max_added_mass_kg,"disabled_reason":if rt.engine.config.turbocharge_max_added_mass_kg.is_none(){Some("Configure maximum added nitrogen mass before Turbocharge")}else if rt.turbo_inventory_pending && rt.turbo_evidence.is_none(){Some("Interrupted nitrogen accounting requires a verified liquid fill or explicit reset/requalification")}else{None}});
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nominal_instant_fixture_reset_rewinds_delayed_tick_and_production_controller_refuses_it() {
        let wall = Instant::now();
        let old_clock = wall - std::time::Duration::from_millis(10_000);
        let delayed_wall = wall + std::time::Duration::from_millis(350);
        let delayed = delayed_wall.duration_since(old_clock).as_millis() as u64;
        let next_clock = delayed_wall - std::time::Duration::from_millis(10_200);
        let nominal = delayed_wall.duration_since(next_clock).as_millis() as u64;
        assert_eq!((delayed, nominal), (10_350, 10_200));
        let config = Config {
            turbocharge_max_added_mass_kg: Some(0.3),
            pressure_ceiling_psi: Some(1000.0),
            ..Config::default()
        };
        let context = Context {
            reset_generation: 1,
            qualification_generation: 0,
            configuration: [1; 32],
        };
        let input = |now| Inputs {
            now_ms: now,
            pressure: Some(Sample {
                value: 100.0,
                at_ms: now,
            }),
            valves: [Some(ValveState {
                open: false,
                at_ms: now,
            }); 5],
            prelaunch: true,
            interlock: true,
        };
        let (mut controller, _) = Controller::start(
            &config,
            10.0,
            745.0,
            input(10_000),
            Some(Sample {
                value: 10.0,
                at_ms: 10_000,
            }),
            context,
        )
        .unwrap();
        assert!(
            controller
                .tick(
                    input(delayed),
                    Some(Sample {
                        value: 10.0,
                        at_ms: delayed
                    }),
                    context
                )
                .cancelled
                .is_none()
        );
        let rollback = controller.tick(
            input(nominal),
            Some(Sample {
                value: 10.0,
                at_ms: nominal,
            }),
            context,
        );
        assert_eq!(
            rollback.cancelled.as_deref(),
            Some("Turbocharge clock regressed; pausing")
        );
        assert!(rollback.effects.valves.iter().all(|(_, open)| !*open));
    }

    #[test]
    fn scalar_receipt_age_survives_mutex_delay_without_committing_stale_history() {
        let clock = Instant::now();
        let source = 1_793_472_388_379;
        let receipt = ValveReceipt {
            network_ms: source,
            monotonic_at: clock + std::time::Duration::from_millis(1000),
            simulation_ms: None,
        };
        let mut previous = None;
        assert_eq!(
            scalar_capture_at(clock, 3001, Some(source), receipt, &mut previous),
            None
        );
        assert_eq!(previous, None);
        assert_eq!(
            scalar_capture_at(clock, 3000, Some(source), receipt, &mut previous),
            Some(1000)
        );
        assert_eq!(previous, Some(source));
    }
    fn observe_valve_pressure_ingress(
        state: &AppState,
        sender: &str,
        rows: &[(String, Vec<Option<f32>>)],
        source_ms: i64,
        receipt_ms: i64,
    ) {
        super::observe_valve_pressure_ingress(
            state,
            sender,
            rows,
            source_ms,
            ValveReceipt {
                network_ms: receipt_ms,
                monotonic_at: Instant::now(),
                simulation_ms: crate::simulation_clock_ms(),
            },
        );
    }
    #[tokio::test]
    async fn firmware_psi_lane_never_uses_chamber_iadc_or_other_sender_and_does_not_renew_replay() {
        let state = crate::state::tests::test_app_state().await;
        let raw = vec![("FUEL_TANK_PRESSURE".into(), vec![Some(370.0)])];
        observe_valve_pressure_ingress(&state, "DAQ", &raw, 100, 100);
        observe_valve_pressure_ingress(
            &state,
            "VB",
            &[("IADC".into(), vec![Some(745.0)])],
            100,
            100,
        );
        assert!(state.gse.lock().unwrap().valve_pressure.is_none());
        observe_valve_pressure_ingress(&state, "VB", &raw, 100, 100);
        let stamp = {
            let mut rt = state.gse.lock().unwrap();
            rt.observe_pressure(Some(745.0));
            assert_eq!(rt.valve_pressure.unwrap().value, 370.0);
            rt.valve_pressure.unwrap().at_ms
        };
        observe_valve_pressure_ingress(
            &state,
            "VB",
            &[("FUEL_TANK_PRESSURE".into(), vec![Some(999.0)])],
            100,
            101,
        );
        assert_eq!(
            state.gse.lock().unwrap().valve_pressure.unwrap().at_ms,
            stamp
        );
        assert_eq!(
            state.gse.lock().unwrap().valve_pressure.unwrap().value,
            370.0
        );
        observe_valve_pressure_ingress(&state, "VB", &raw, 99, 102);
        assert!(state.gse.lock().unwrap().valve_pressure.is_none());
        assert_eq!(state.gse.lock().unwrap().valve_pressure_epoch, 1);
    }
    #[tokio::test]
    async fn canonical_source_age_is_retained_and_stale_pressure_cannot_enable_turbocharge() {
        let state = crate::state::tests::test_app_state().await;
        state.gse.lock().unwrap().clock = Instant::now() - std::time::Duration::from_secs(10);
        let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
        let rows = vec![("FUEL_TANK_PRESSURE".into(), vec![Some(370.0)])];
        observe_valve_pressure_ingress(&state, "VB", &rows, now - 1500, now);
        let rt = state.gse.lock().unwrap();
        let pressure = rt.valve_pressure.unwrap();
        assert!((1500..=1520).contains(&(rt.now() - pressure.at_ms)));
        drop(rt);
        observe_valve_pressure_ingress(&state, "VB", &rows, now - 5000, now);
        assert_eq!(
            state.gse.lock().unwrap().last_valve_pressure_source_ms,
            Some(now - 1500)
        );
        assert_eq!(
            state.gse.lock().unwrap().valve_pressure_epoch,
            0,
            "an out-of-orderUTCpacket is not evidenceofanotherboot"
        );
    }
    #[tokio::test]
    async fn actual_router_sequence_tags_only_nitrogen_open_and_sticky_launch_inhibit_stops_all_outputs()
     {
        actual_router_sequence_case(false, false).await;
    }
    #[tokio::test]
    async fn retained_manual_stop_before_settling_completion_cannot_mint_inventory_evidence() {
        actual_router_sequence_case(true, false).await;
    }
    #[tokio::test]
    async fn missing_raw_pressure_after_nitrogen_open_closes_with_acks_and_holds_without_false_fault()
     {
        actual_router_sequence_case(false, true).await;
    }
    async fn actual_router_sequence_case(stop_before_completion: bool, missing_pressure: bool) {
        actual_router_inventory_case(stop_before_completion, missing_pressure, 10.0, 745.0, 0.0).await;
    }
    #[tokio::test]
    async fn near_full_paused_fill_noop_then_actual_turbo_settles_and_qualifies_launch() {
        actual_router_inventory_case(false, false, 8.0, 750.0, 0.1).await;
    }
    async fn actual_router_inventory_case(stop_before_completion: bool, missing_pressure: bool,
        target_mass: f32, target_pressure: f32, inventory_loss: f32) {
        let (state, sent, peer) = super::super::tests::button_test_state().await;
        // This fixture exercises configured targets, without a simulation-only
        // calibration full-mass override changing the original inventory goal.
        state.loadcell_calibration.lock().unwrap().full_mass_kg = None;
        let mut targets = state.fill_targets_snapshot();
        targets.nitrous.target_mass_kg = target_mass;
        targets.nitrous.target_pressure_psi = target_pressure;
        state.set_fill_targets(targets);
        let target = state.fill_targets_snapshot().nitrous;
        let inventory = target.target_mass_kg - inventory_loss * target.target_mass_kg.signum();
        let mut policy = state.action_policy_snapshot();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        state.set_action_policy(policy);
        for b in [
            crate::types::Board::GatewayBoard,
            crate::types::Board::ValveBoard,
            crate::types::Board::ActuatorBoard,
            crate::types::Board::DaqBoard,
        ] {
            state.mark_board_seen(
                b.sender_id(),
                crate::telemetry_task::get_current_timestamp_ms(),
            );
        }
        let path =
            std::env::temp_dir().join(format!("turbo-evidence-{}.json", rand::random::<u128>()));
        let calibration = mass_calibration_signature(&state.loadcell_calibration.lock().unwrap());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.test_now_ms = Some(10_000);
            rt.turbo_audit_path = Some(path.with_extension("audit.json"));
            rt.engine.config.pressure_ceiling_psi = Some(1000.0);
            rt.engine.config.turbocharge_max_added_mass_kg = Some(0.3);
            rt.engine.config.fill_settle_ms = 1000;
            rt.engine.status.phase = Phase::Paused;
            rt.engine.nitrous_target_psi = target.target_pressure_psi;
            rt.engine.status.nitrogen_passed = true;
            rt.engine.status.baseline = Some(gse_sequence::Baseline {
                average_psi: 0.0,
                min_psi: 0.0,
                max_psi: 0.0,
                noise_psi: 0.1,
                samples: 50,
            });
            let seed = Inputs {
                now_ms: rt.now(),
                pressure: Some(Sample {
                    value: 0.0,
                    at_ms: rt.now(),
                }),
                valves: [None; 5],
                prelaunch: true,
                interlock: true,
            };
            rt.engine.request(Action::StartFill, seed).unwrap();
            rt.engine.manual_override();
            rt.engine.status.phase = Phase::Paused;
            assert!(
                rt.engine.nitrous_session(),
                "exercise real retained nitrous inventory safety"
            );
            if inventory_loss > 0.0 { rt.observe_pressure(Some(100.0)); }
            rt.observe_mass(Some(inventory));
            rt.mass_calibration = Some(calibration);
            rt.valve_pressure = Some(Sample {
                value: 100.0,
                at_ms: rt.now(),
            });
            #[cfg(not(feature = "hitl_mode"))]
            {
                rt.operational.path = Some(path.clone());
                rt.operational.topology_stable = true;
                rt.operational.startup_configured = true;
                rt.operational.saved.baseline = Some(gse_sequence::Baseline {
                    average_psi: 0.0,
                    min_psi: 0.0,
                    max_psi: 0.0,
                    noise_psi: 0.1,
                    samples: 50,
                });
                for milestone in [
                    Milestone::SelfTest,
                    Milestone::NitrogenTest,
                    Milestone::FillTarget,
                ] {
                    rt.operational
                        .saved
                        .qualification
                        .complete(milestone, 0)
                        .unwrap();
                }
                rt.operational.saved.fill_target_kg = Some(target.target_mass_kg);
            }
        }
        if inventory_loss > 0.0 {
            let generation = super::super::automatic_generation(&state);
            assert!(super::super::consume_fill_noop(&state, &TelemetryCommand::StartFill));
            assert!(handle_command(&state, &TelemetryCommand::StartFill));
            super::super::tests::pump_test_link(&state, &peer);
            assert!(sent.lock().unwrap().is_empty());
            assert_eq!(super::super::automatic_generation(&state), generation);
            assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Paused);
        }
        let original_calibration = state.loadcell_calibration.lock().unwrap().clone();
        state.loadcell_calibration.lock().unwrap().ch1.b = Some(12.0);
        assert!(
            !command_allowed(&state, &TelemetryCommand::Turbocharge).unwrap(),
            "old mass cannot acquire a newly sampled calibration binding"
        );
        handle_command(&state, &TelemetryCommand::Turbocharge);
        assert!(state.gse.lock().unwrap().turbocharge.is_none());
        assert!(sent.lock().unwrap().is_empty());
        *state.loadcell_calibration.lock().unwrap() = original_calibration;
        assert!(command_allowed(&state, &TelemetryCommand::Turbocharge).unwrap());
        assert!(handle_command(&state, &TelemetryCommand::Turbocharge));
        assert!(state.gse.lock().unwrap().turbo_inventory_pending);
        #[cfg(not(feature = "hitl_mode"))]
        assert!(
            !state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .qualification
                .completed(Milestone::FillTarget),
            "old fill qualification must not survive an unsettled top-up"
        );
        let pending: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.with_extension("audit.json")).unwrap())
                .unwrap();
        assert_eq!(pending["state"], "pending");
        let mut actual = [false; 5];
        let mut consumed = 0;
        let mut opened = false;
        let mut settling_ticks = 0;
        let mut stopped_at = None;
        for step in 1..60 {
            super::super::tests::pump_test_link(&state, &peer);
            {
                let commands = sent.lock().unwrap();
                for (kind, bytes) in &commands[consumed..] {
                    assert_eq!(bytes.len(), 1);
                    assert_ne!(bytes[0] & 0x80, 0);
                    let code = bytes[0] & 0x7f;
                    let i = (if kind == "VALVE_COMMAND" { 0..3 } else { 3..5 })
                        .find(|i| code == OPEN[*i] || code == CLOSE[*i])
                        .unwrap();
                    let open = code == OPEN[i];
                    if open {
                        assert_eq!(i, 3, "only nitrogen may open");
                        assert!(actual.iter().all(|v| !*v));
                        opened = true;
                    }
                    actual[i] = open;
                }
                consumed = commands.len();
            }
            {
                let mut rt = state.gse.lock().unwrap();
                rt.test_now_ms = Some(10_000 + step * 100);
                for (key, open) in KEYS.into_iter().zip(actual) {
                    rt.observe_valve(key, open);
                }
                rt.observe_mass(Some(if opened {
                    inventory + 0.2 * inventory.signum()
                } else {
                    inventory
                }));
                rt.mass_calibration = Some(calibration);
                rt.valve_pressure = Some(Sample {
                    value: if opened {
                        target.target_pressure_psi
                    } else {
                        100.0
                    },
                    at_ms: rt.now(),
                });
                if missing_pressure && opened {
                    rt.valve_pressure = None;
                    rt.observe_pressure(Some(2000.0)); // unrelated IADC must never substitute
                }
            }
            if stop_before_completion
                && state
                    .gse
                    .lock()
                    .unwrap()
                    .turbocharge
                    .as_ref()
                    .is_some_and(|c| c.phase() == gse_sequence::turbocharge::Phase::Settling)
            {
                settling_ticks += 1;
                // The next (11th) fresh 100ms sample spans the required1000ms.
                // Without the Stop guard this real controller completes here.
                if settling_ticks == 11 {
                    state.manual_abort_intent.request();
                    stopped_at = Some(sent.lock().unwrap().len());
                }
            }
            super::super::report_safety(&state);
            // Actual controller→automatic0x80Router→endpoint, with fresh physical reports.
            if missing_pressure && opened {
                super::super::tick(&state, true);
            } else {
                let progressed = {
                    let _gate = DISPATCH_GATE.lock().unwrap();
                    tick(&state, true)
                };
                assert!(progressed);
            }
            flush_effects(&state);
            super::super::progress::flush(&state);
            if (missing_pressure
                && opened
                && state.gse.lock().unwrap().engine.status.phase == Phase::Paused)
                || stopped_at.is_some()
                || state.gse.lock().unwrap().turbo_evidence.is_some()
            {
                if state.gse.lock().unwrap().turbo_evidence.is_some() {
                    let rt = state.gse.lock().unwrap();
                    let progress = rt.progress.current().expect("actual accepted turbo run");
                    assert_eq!(progress.outcome, crate::sequence_progress::Outcome::Passed);
                    let key = progress.key.clone();
                    drop(rt);
                    let result = state
                        .sequence_progress
                        .lock()
                        .unwrap()
                        .result(&key)
                        .unwrap();
                    assert!(result.steps.iter().all(|step| matches!(
                        step.state,
                        crate::sequence_progress::StepState::Passed
                            | crate::sequence_progress::StepState::Skipped
                    )));
                    let rt = state.gse.lock().unwrap();
                    assert!(rt.progress.error.is_none());
                }
                break;
            }
        }
        if missing_pressure {
            assert!(
                opened,
                "exercise actual admitted nitrogen before telemetry loss"
            );
            {
                let rt = state.gse.lock().unwrap();
                assert_eq!(
                    rt.engine.status.phase,
                    Phase::Paused,
                    "{}",
                    rt.engine.status.message
                );
                assert!(rt.valves.iter().all(|v| v.is_some_and(|v| !v.open)));
                assert!(rt.turbocharge.is_none() && rt.turbo_evidence.is_none());
                assert!(rt.turbo_inventory_pending && rt.valve_pressure.is_none());
                #[cfg(not(feature = "hitl_mode"))]
                assert!(
                    !rt.operational
                        .saved
                        .qualification
                        .completed(Milestone::FillTarget)
                );
            }
            let before = sent.lock().unwrap().len();
            super::super::tick(&state, true);
            super::super::tests::pump_test_link(&state, &peer);
            assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Paused);
            assert_eq!(
                sent.lock().unwrap().len(),
                before,
                "closed hold must not reopen or fabricate PT"
            );
            assert!(!command_allowed(&state, &TelemetryCommand::Turbocharge).unwrap());
            let audit: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path.with_extension("audit.json")).unwrap())
                    .unwrap();
            assert_eq!(audit["state"], "pending");
            std::fs::remove_file(path.with_extension("audit.json")).unwrap();
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
            return;
        }
        if stop_before_completion {
            let before = stopped_at.expect("reach final settling sample before Stop");
            assert!(super::super::manual_stop_pending(&state));
            {
                let rt = state.gse.lock().unwrap();
                assert!(rt.turbocharge.is_none() && rt.turbo_evidence.is_none());
                assert!(rt.turbo_inventory_pending);
                #[cfg(not(feature = "hitl_mode"))]
                {
                    assert!(
                        !rt.operational
                            .saved
                            .qualification
                            .completed(Milestone::FillTarget)
                    );
                    assert!(rt.operational.saved.turbocharge.is_none());
                }
            }
            let audit: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path.with_extension("audit.json")).unwrap())
                    .unwrap();
            assert_eq!(audit["state"], "pending");
            super::super::tick(&state, true);
            super::super::tests::pump_test_link(&state, &peer);
            assert_eq!(
                sent.lock().unwrap().len(),
                before,
                "Turbo must not compete with retained Stop or emit another control"
            );
            assert!(!command_allowed(&state, &TelemetryCommand::Turbocharge).unwrap());
            std::fs::remove_file(path.with_extension("audit.json")).unwrap();
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
            return;
        }
        {
            let rt = state.gse.lock().unwrap();
            assert_eq!(rt.engine.status.phase, Phase::Paused);
            assert!(rt.turbo_evidence.is_some());
            assert!(rt.valves.iter().all(|v| v.is_some_and(|v| !v.open)));
        }
        assert!(opened);
        {
            let mut rt = state.gse.lock().unwrap();
            let mass = rt.fresh_mass(rt.now()).unwrap();
            assert!(accepts_mass(&state, &rt, mass));
            rt.mass_source_epoch += 1;
            assert!(
                !accepts_mass(&state, &rt, mass),
                "source restart invalidates original inventory proof"
            );
            rt.mass_source_epoch -= 1;
        }
        assert!(
            command_allowed(&state, &TelemetryCommand::Turbocharge).unwrap(),
            "a settled bounded top-up can be repeated within its ORIGINAL inventory allowance"
        );
        if inventory_loss > 0.0 {
            let rt = state.gse.lock().unwrap();
            let evidence = rt.turbo_evidence.as_ref().unwrap();
            assert!(evidence.valid());
            assert!((evidence.before_mass.value - inventory).abs() < 0.0001);
            assert!((evidence.added_mass_kg - 0.2).abs() < 0.0001);
            #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
            assert!(super::super::operational_launch_allowed(&state, &rt),
                "actual settled inventory, closed reports and raw750psi must satisfy the TestFire launch gate");
        }
        if inventory_loss > 0.0 {
            let mut rt = state.gse.lock().unwrap();
            let original = rt.valve_pressure.unwrap();
            let anchor = rt.turbo_evidence.as_ref().unwrap().before_mass.value;
            let band = rt.engine.config.turbocharge_pressure_deadband_psi;
            for pressure in [target_pressure, target_pressure - band] {
                rt.valve_pressure = Some(Sample { value: pressure, ..original });
                assert!(accepts_mass(&state, &rt, rt.fresh_mass(rt.now()).unwrap()));
            }
            rt.valve_pressure = Some(Sample { value: target_pressure - band - 0.125, ..original });
            assert!(!accepts_mass(&state, &rt, rt.fresh_mass(rt.now()).unwrap()));
            #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
            assert!(!super::super::operational_launch_allowed(&state, &rt));
            assert_eq!(rt.turbo_evidence.as_ref().unwrap().before_mass.value, anchor);
            // Even a mass still in the liquid-only ready band cannot bypass existing Turbo pressure authority.
            let original_mass = rt.mass.unwrap();
            rt.mass = Some(Sample { value: target_mass + 0.04, ..original_mass });
            let pending = rt.turbo_inventory_pending;
            rt.turbo_inventory_pending = false;
            #[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
            assert!(!super::super::operational_launch_allowed(&state, &rt));
            rt.turbo_inventory_pending = pending;
            rt.mass = Some(original_mass);
            let previous = rt.turbo_evidence.as_ref().unwrap();
            let (repeat, _) = Controller::start_with_inventory(
                &rt.engine.config, target_mass, target_pressure,
                Inputs { now_ms: rt.now(), pressure: rt.valve_pressure, valves: rt.valves,
                    prelaunch: true, interlock: true }, rt.mass, context(&state, &rt), Some(previous),
            ).unwrap();
            assert_eq!(repeat.inventory_anchor().value, anchor);
            rt.valve_pressure = Some(original);
        }
        let before = sent.lock().unwrap().len();
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        *state.state.lock().unwrap() = FlightState::Idle;
        assert!(!command_allowed(&state, &TelemetryCommand::Turbocharge).unwrap());
        handle_command(&state, &TelemetryCommand::Turbocharge);
        super::super::tick(&state, true);
        super::super::tests::pump_test_link(&state, &peer);
        assert_eq!(sent.lock().unwrap().len(), before);
        #[cfg(not(feature = "hitl_mode"))]
        {
            assert!(
                operational::Saved::load(&path)
                    .unwrap()
                    .turbocharge
                    .is_some()
            );
            std::fs::remove_file(path).unwrap();
        }
        #[cfg(feature = "hitl_mode")]
        let _ = path;
    }
    /// Firmware-sim run g1 race: Turbocharge is cancelled right after its
    /// Nitrogen-open command, then the board's in-flight Nitrogen-open report
    /// arrives while the cancel is closing. That report must not latch
    /// UnsafeSupplyFlow or dump: the pause closes it and holds Paused.
    #[tokio::test]
    async fn late_nitrogen_open_report_after_turbo_cancel_pauses_closed_without_dump() {
        let (state, sent, peer) = super::super::tests::button_test_state().await;
        state.loadcell_calibration.lock().unwrap().full_mass_kg = None;
        let mut targets = state.fill_targets_snapshot();
        targets.nitrous.target_mass_kg = 10.0;
        targets.nitrous.target_pressure_psi = 745.0;
        state.set_fill_targets(targets);
        let mut policy = state.action_policy_snapshot();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        state.set_action_policy(policy);
        for b in [
            crate::types::Board::GatewayBoard,
            crate::types::Board::ValveBoard,
            crate::types::Board::ActuatorBoard,
            crate::types::Board::DaqBoard,
        ] {
            state.mark_board_seen(b.sender_id(), crate::telemetry_task::get_current_timestamp_ms());
        }
        let path =
            std::env::temp_dir().join(format!("turbo-late-report-{}.json", rand::random::<u128>()));
        let calibration = mass_calibration_signature(&state.loadcell_calibration.lock().unwrap());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.test_now_ms = Some(10_000);
            rt.turbo_audit_path = Some(path.with_extension("audit.json"));
            rt.engine.config.pressure_ceiling_psi = Some(1000.0);
            rt.engine.config.turbocharge_max_added_mass_kg = Some(0.3);
            rt.engine.config.fill_settle_ms = 1000;
            rt.engine.status.phase = Phase::Paused;
            rt.engine.nitrous_target_psi = 745.0;
            rt.engine.status.nitrogen_passed = true;
            rt.engine.status.baseline = Some(gse_sequence::Baseline {
                average_psi: 0.0,
                min_psi: 0.0,
                max_psi: 0.0,
                noise_psi: 0.1,
                samples: 50,
            });
            let seed = Inputs {
                now_ms: rt.now(),
                pressure: Some(Sample { value: 0.0, at_ms: rt.now() }),
                valves: [None; 5],
                prelaunch: true,
                interlock: true,
            };
            rt.engine.request(Action::StartFill, seed).unwrap();
            rt.engine.manual_override();
            rt.engine.status.phase = Phase::Paused;
            rt.observe_mass(Some(10.0));
            rt.mass_calibration = Some(calibration);
            rt.valve_pressure = Some(Sample { value: 100.0, at_ms: rt.now() });
            #[cfg(not(feature = "hitl_mode"))]
            {
                rt.operational.path = Some(path.clone());
                rt.operational.topology_stable = true;
                rt.operational.startup_configured = true;
                rt.operational.saved.baseline = rt.engine.status.baseline.clone();
                for milestone in [Milestone::SelfTest, Milestone::NitrogenTest, Milestone::FillTarget] {
                    rt.operational.saved.qualification.complete(milestone, 0).unwrap();
                }
                rt.operational.saved.fill_target_kg = Some(10.0);
            }
        }
        assert!(handle_command(&state, &TelemetryCommand::Turbocharge));
        assert!(
            state.gse.lock().unwrap().turbocharge.is_some(),
            "{}",
            state.gse.lock().unwrap().engine.status.message
        );
        let mut actual = [false; 5];
        let mut consumed = 0;
        let mut nitrogen_commanded_open = false;
        let mut late_report_delivered = false;
        let mut relief_commanded = false;
        for step in 1..200u64 {
            super::super::tests::pump_test_link(&state, &peer);
            {
                let commands = sent.lock().unwrap();
                for (kind, bytes) in &commands[consumed..] {
                    let code = bytes[0] & 0x7f;
                    let i = (if kind == "VALVE_COMMAND" { 0..3 } else { 3..5 })
                        .find(|i| code == OPEN[*i] || code == CLOSE[*i])
                        .unwrap();
                    let open = code == OPEN[i];
                    relief_commanded |= open && (i == 1 || i == 2);
                    if (i, open) == (3, true) {
                        // The board has not applied it yet.
                        nitrogen_commanded_open = true;
                        continue;
                    }
                    if !(i == 3 && !late_report_delivered && nitrogen_commanded_open) {
                        actual[i] = open;
                    }
                }
                consumed = commands.len();
            }
            let phase = {
                let mut rt = state.gse.lock().unwrap();
                rt.test_now_ms = Some(10_000 + step * 100);
                let cancelled = nitrogen_commanded_open && rt.turbocharge.is_none();
                if cancelled && !late_report_delivered {
                    // The Nitrogen-open command was executed after all.
                    actual[3] = true;
                    late_report_delivered = true;
                } else if late_report_delivered && actual[3] {
                    // ...and the cancel's queued close follows it.
                    actual[3] = false;
                }
                for (key, open) in KEYS.into_iter().zip(actual) {
                    rt.observe_valve(key, open);
                }
                rt.observe_mass(Some(10.0));
                rt.mass_calibration = Some(calibration);
                // A non-safety cancel cause: raw valve-board pressure goes
                // missing right after the Nitrogen-open command.
                rt.valve_pressure = (!nitrogen_commanded_open || late_report_delivered)
                    .then_some(Sample { value: 100.0, at_ms: rt.now() });
                rt.engine.status.phase
            };
            let _ = phase;
            super::super::report_safety(&state);
            super::super::tick(&state, true);
            flush_effects(&state);
            let rt = state.gse.lock().unwrap();
            assert_ne!(rt.engine.status.phase, Phase::Fault, "{}", rt.engine.status.message);
            #[cfg(not(feature = "hitl_mode"))]
            assert_eq!(rt.operational.saved.qualification.fault(), None);
            if late_report_delivered && rt.engine.status.phase == Phase::Paused {
                break;
            }
        }
        assert!(nitrogen_commanded_open && late_report_delivered);
        assert!(!relief_commanded, "no vent or dump may be commanded");
        {
            let rt = state.gse.lock().unwrap();
            assert_eq!(rt.engine.status.phase, Phase::Paused, "{}", rt.engine.status.message);
            assert!(rt.engine.failure().is_some());
            assert!(rt.valves.iter().all(|v| v.is_some_and(|v| !v.open)));
            #[cfg(not(feature = "hitl_mode"))]
            assert!(!super::super::operational_launch_allowed(&state, &rt));
        }
        let _ = std::fs::remove_file(path.with_extension("audit.json"));
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn pressure_target_stops_short_liquid_fill_with_fresh_ack_gated_pause_not_qualification() {
        let mut rt = Runtime::default();
        rt.engine.status.phase = Phase::Filling;
        rt.engine.nitrous_target_psi = 745.0;
        rt.observe_pressure(Some(999.0));
        let input = Inputs {
            now_ms: 1000,
            pressure: Some(Sample {
                value: 999.0,
                at_ms: 1000,
            }),
            valves: [None; 5],
            prelaunch: true,
            interlock: true,
        };
        assert!(
            pressure_cutoff(&mut rt, input).is_none(),
            "GS-calibratedIADC cannot substitute for VB psi"
        );
        rt.valve_pressure = Some(Sample {
            value: 745.0,
            at_ms: 1000,
        });
        let effects = pressure_cutoff(&mut rt, input).unwrap();
        assert!(!effects.valves.is_empty());
        assert!(effects.valves.iter().all(|(_, open)| !*open));
        assert_eq!(rt.engine.status.phase, Phase::PauseSetup);
        // Closing still enforces the session ceiling, so the engine's own
        // pressure must be below it for the closure to complete.
        let input = Inputs {
            pressure: Some(Sample {
                value: 100.0,
                at_ms: 1000,
            }),
            ..input
        };
        rt.engine.tick(input);
        assert_eq!(
            rt.engine.status.phase,
            Phase::PauseSetup,
            "intent is not confirmation"
        );
        let confirmed = Inputs {
            now_ms: 1001,
            valves: [Some(ValveState {
                open: false,
                at_ms: 1001,
            }); 5],
            ..input
        };
        for step in 0..5 {
            rt.engine.tick(Inputs {
                now_ms: 1001 + step,
                valves: [Some(ValveState {
                    open: false,
                    at_ms: 1001 + step,
                }); 5],
                ..confirmed
            });
        }
        assert_eq!(rt.engine.status.phase, Phase::Paused);
        #[cfg(not(feature = "hitl_mode"))]
        assert!(rt.operational.target_reached.is_none());
    }
    #[tokio::test]
    async fn operator_pause_cancels_turbo_and_ack_gates_closed_without_erasing_pending_inventory() {
        let (state, sent, peer) = super::super::tests::button_test_state().await;
        let mut policy = state.action_policy_snapshot();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        state.set_action_policy(policy);
        let mut pressure_calibration = state.loadcell_calibration.lock().unwrap().clone();
        pressure_calibration.iadc.m = Some(5.0);
        pressure_calibration.iadc.b = Some(150.0);
        pressure_calibration.iadc_fit = None;
        pressure_calibration.iadc_zero_raw = None;
        let recalibrated = crate::loadcell::calibrated_sensor_value(
            &pressure_calibration,
            crate::loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE,
            370.0,
        )
        .unwrap();
        assert_eq!(recalibrated, 2000.0);
        let mut rt = state.gse.lock().unwrap();
        rt.test_now_ms = Some(1_000);
        rt.engine.status.phase = Phase::Turbocharge;
        rt.turbo_inventory_pending = true;
        let now = rt.now();
        let context = Context {
            reset_generation: super::super::automatic_generation(&state),
            qualification_generation: 0,
            configuration: [0; 32],
        };
        let config = Config {
            turbocharge_max_added_mass_kg: Some(0.3),
            pressure_ceiling_psi: Some(1000.0),
            ..rt.engine.config.clone()
        };
        rt.engine.config = config.clone();
        rt.engine.status.phase = Phase::Passed;
        rt.engine.status.nitrogen_passed = true;
        rt.engine.status.baseline = Some(gse_sequence::Baseline {
            average_psi: 0.0,
            min_psi: 0.0,
            max_psi: 0.0,
            noise_psi: 0.1,
            samples: 50,
        });
        rt.engine.nitrous_target_psi = 745.0;
        rt.engine
            .request(
                Action::StartFill,
                Inputs {
                    now_ms: now,
                    pressure: Some(Sample {
                        value: 0.0,
                        at_ms: now,
                    }),
                    valves: [None; 5],
                    prelaunch: true,
                    interlock: true,
                },
            )
            .unwrap();
        rt.engine.manual_override();
        assert!(
            rt.engine.nitrous_session(),
            "Turbo must start from a real nitrous-fill session"
        );
        rt.engine.status.phase = Phase::Turbocharge;
        rt.observe_pressure(Some(recalibrated));
        rt.valve_pressure = Some(Sample {
            value: 370.0,
            at_ms: now,
        });
        let input = Inputs {
            now_ms: now,
            pressure: Some(Sample {
                value: 100.0,
                at_ms: now,
            }),
            valves: [None; 5],
            prelaunch: true,
            interlock: true,
        };
        rt.turbocharge = Some(
            Controller::start(
                &config,
                10.0,
                745.0,
                input,
                Some(Sample {
                    value: 10.0,
                    at_ms: now,
                }),
                context,
            )
            .unwrap()
            .0,
        );
        drop(rt);
        super::super::report_safety(&state);
        assert_ne!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Fault,
            "unrelated IADC spike cannot fault raw-VB Turbocharge"
        );
        assert_eq!(
            command_allowed(&state, &TelemetryCommand::PauseFill),
            Some(true)
        );
        assert!(handle_command(&state, &TelemetryCommand::PauseFill));
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::PauseSetup
        );
        for step in 1..=6 {
            super::super::tests::pump_test_link(&state, &peer);
            let effects = {
                let mut rt = state.gse.lock().unwrap();
                rt.test_now_ms = Some(1_000 + step * 10);
                for key in KEYS {
                    rt.observe_valve(key, false);
                }
                let input = rt.input(true, true);
                rt.engine.tick(input)
            };
            {
                let _gate = DISPATCH_GATE.lock().unwrap();
                apply(&state, effects);
            }
            flush_effects(&state);
        }
        super::super::tests::pump_test_link(&state, &peer);
        let rt = state.gse.lock().unwrap();
        assert_eq!(rt.engine.status.phase, Phase::Paused);
        assert!(
            rt.turbocharge.is_none() && rt.turbo_evidence.is_none() && rt.turbo_inventory_pending
        );
        drop(rt);
        super::super::report_safety(&state);
        {
            let mut rt = state.gse.lock().unwrap();
            assert_eq!(rt.pressure.unwrap().value, 2000.0);
            let input = rt.input(true, true);
            assert_eq!(input.pressure.unwrap().value, 370.0);
            rt.engine.tick(input);
            assert_eq!(rt.engine.status.phase, Phase::Paused);
            rt.valve_pressure = None;
        }
        {
            let _gate = DISPATCH_GATE.lock().unwrap();
            assert!(guard_closed_pressure(&state));
        }
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Paused,
            "missing raw pressure must hold closed, not fault from IADC fallback"
        );
        assert!(!sent.lock().unwrap().is_empty());
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|(_, bytes)| bytes.len() == 1
                    && bytes[0] & 0x80 != 0
                    && CLOSE.contains(&(bytes[0] & 0x7f)))
        );
        {
            let mut rt = state.gse.lock().unwrap();
            rt.valve_pressure = Some(Sample {
                value: 2000.0,
                at_ms: rt.now(),
            });
        }
        {
            let _gate = DISPATCH_GATE.lock().unwrap();
            assert!(guard_closed_pressure(&state));
        }
        flush_effects(&state);
        super::super::tests::pump_test_link(&state, &peer);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Fault,
            "actual fresh canonical VB overpressure retains severe relief"
        );
        assert_ne!(*state.state.lock().unwrap(), FlightState::Aborted);
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .filter(|(kind, _)| kind == "ACTUATOR_COMMAND")
                .all(|(_, bytes)| !matches!(bytes[0] & 0x7f, 9 | 10)),
            "severe relief may never open either supply"
        );
    }
    #[tokio::test]
    async fn cold_flow_target_only_edit_does_not_change_actual_turbo_binding_bytes() {
        let state = crate::state::tests::test_app_state().await;
        let mut rt = state.gse.lock().unwrap();
        rt.engine.config.cold_flow_target_psi = None;
        let binding = capture_binding(&state, &rt).0;
        let c = &rt.engine.config;
        // Current Turbo binding projection: the pressure deadband is intentionally bound.
        let current_projection = serde_json::json!({"nitrogen_target_psi":c.nitrogen_target_psi,"nitrogen_steps":c.nitrogen_steps,"fill_overfill_kg":c.fill_overfill_kg,"fill_tolerance_kg":c.fill_tolerance_kg,"fill_settle_ms":c.fill_settle_ms,"turbocharge_max_added_mass_kg":c.turbocharge_max_added_mass_kg,"turbocharge_pressure_deadband_psi":c.turbocharge_pressure_deadband_psi,"pressure_step_psi":c.pressure_step_psi,"pressure_ceiling_psi":c.pressure_ceiling_psi,"maximum_zero_offset_psi":c.maximum_zero_offset_psi,"dry_self_test_confirmed":c.dry_self_test_confirmed,"grouped_panel":c.grouped_panel});
        let mut expected = binding.clone();
        expected["config"] = current_projection;
        let expected = serde_json::to_vec(&expected).unwrap();
        let before = serde_json::to_vec(&binding).unwrap();
        assert_eq!(before, expected);
        rt.engine.config.cold_flow_target_psi = Some(95.);
        let after = serde_json::to_vec(&capture_binding(&state, &rt).0).unwrap();
        assert_eq!(before, after);
        assert_eq!(
            ring::digest::digest(&ring::digest::SHA256, &before).as_ref(),
            ring::digest::digest(&ring::digest::SHA256, &after).as_ref()
        );
        println!(
            "COLD_FLOW_CONTROL current_turbo_binding_projection=IDENTICAL cold_target_only=INERT deadband=BOUND"
        );
        let original = rt.engine.config.clone();
        let mutations: [fn(&mut Config); 12] = [
            |c| c.nitrogen_target_psi += 1.,
            |c| c.turbocharge_pressure_deadband_psi = 16.,
            |c| {
                c.nitrogen_steps = vec![gse_sequence::NitrogenStep {
                    target_psi: 35.,
                    settle_ms: 4000,
                    hold_ms: 5000,
                }]
            },
            |c| c.fill_overfill_kg = 0.06,
            |c| c.fill_tolerance_kg = 0.06,
            |c| c.fill_settle_ms = 4000,
            |c| c.turbocharge_max_added_mass_kg = Some(0.3),
            |c| c.pressure_step_psi += 1.,
            |c| c.pressure_ceiling_psi = Some(500.),
            |c| c.maximum_zero_offset_psi = Some(9.),
            |c| c.dry_self_test_confirmed = !c.dry_self_test_confirmed,
            |c| c.grouped_panel = !c.grouped_panel,
        ];
        for mutate in mutations {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert_ne!(
                config_for_binding(&original),
                config_for_binding(&changed),
                "Every preexisting config field must remain bound"
            );
            assert!(!only_cold_target_changed(&original, &changed));
        }
    }
}
