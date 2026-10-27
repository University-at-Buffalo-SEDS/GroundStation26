//! Opt-in simulator evidence. No seed, UART or UTC timestamp substitutes for an API request.
use sedsnet::{packet::Packet, router::Router};
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! { static CONTROLLED_ROUND: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
pub(crate) fn in_controlled_round<T>(round: usize, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTROLLED_ROUND.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(CONTROLLED_ROUND.with(|slot| slot.replace(Some(round))));
    run()
}
/// The audit covers every managed publication, not unrelated typed commands.
/// IDs come from this process's actual loaded application schema.
pub(crate) fn trace_scope() {
    if !cfg!(any(feature = "hitl_mode", feature = "firmware_sim"))
        || std::env::var("GS_SIM_TRACE_API_REQUESTS").as_deref() != Ok("1")
    {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let types=|names: &[&str]| names.iter().map(|name|serde_json::json!({"name":name,"id":crate::telemetry_schema::data_type(name).as_u32()})).collect::<Vec<_>>();
        let covered=types(&["FLIGHT_STATE","AV_BAY_UNDERGLOW","FLIGHT_BUZZER","DAQ_LOADCELL_CALIBRATION","DAQ_KG50_CALIBRATION","DAQ_LOG_CLOCK","DAQ_THERMAL_CALIBRATION","DAQ_FILTER_CALIBRATION","DAQ_KG50_INPUT"]);
        let excluded=types(&["FLIGHT_COMMAND","VALVE_COMMAND","ACTUATOR_COMMAND"]);
        eprintln!("[gs-qualification-scope] {}",serde_json::json!({"version":1,"covered":covered,"excluded":excluded,"managed_limit_ms":500,"excluded_existing_gates":"Valve startup and Valve/Actuator soak physical status default 2500 ms; FC startup execution-counter assertion and existing RF-soak reliable 500 ms (FC+underglow). Original runner gates remain unchanged; excluded typed commands have no new per-packet 500 ms claim."}));
    });
}

thread_local! { static BOOTSTRAP_PHASE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) }; }
const BOOTSTRAP_SOURCE: &str = "main.initial_publish_current.before_workers";
const MANAGED_NAMES: [&str; 9] = [
    "FLIGHT_STATE",
    "AV_BAY_UNDERGLOW",
    "FLIGHT_BUZZER",
    "DAQ_LOADCELL_CALIBRATION",
    "DAQ_KG50_CALIBRATION",
    "DAQ_LOG_CLOCK",
    "DAQ_THERMAL_CALIBRATION",
    "DAQ_FILTER_CALIBRATION",
    "DAQ_KG50_INPUT",
];

fn bootstrap_observation(router: &Router) -> (usize, usize, usize) {
    (
        router.export_runtime_stats().sides.len(),
        router
            .export_topology()
            .routes
            .iter()
            .map(|route| route.announcers.len())
            .sum(),
        router
            .export_reliable_pending_stats()
            .iter()
            .map(|row| row.pending_destinations.len())
            .sum(),
    )
}

/// Only main's initial cache publication may enter this one-shot context.
/// It describes local cache priming, never remote delivery or an ACK.
pub(crate) fn prime_initial_cache(
    router: &Router,
    run: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if !cfg!(any(feature = "hitl_mode", feature = "firmware_sim"))
        || std::env::var("GS_SIM_TRACE_API_REQUESTS").as_deref() != Ok("1")
        || std::env::var("GS_SIM_POLICY_CORRECT_CONTROLS").as_deref() != Ok("1")
    {
        return run();
    }
    static USED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    anyhow::ensure!(
        !USED.swap(true, Ordering::SeqCst),
        "bootstrap cache phase cannot reopen"
    );
    anyhow::ensure!(
        bootstrap_observation(router) == (2, 0, 0),
        "bootstrap cache phase requires two configured sides and no discovered readers/pending owners"
    );
    trace_scope();
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BOOTSTRAP_PHASE.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(BOOTSTRAP_PHASE.with(|slot| slot.replace(Some(1))));
    eprintln!(
        "[gs-qualification-bootstrap] {}",
        serde_json::json!({"event":"begin","phase":1,"source":BOOTSTRAP_SOURCE,"at_ms":crate::simulation_clock_ms(),"configured_sides":2,"discovered_announcers":0,"pending_owners":0})
    );
    let result = run();
    let observation = bootstrap_observation(router);
    let cached = MANAGED_NAMES
        .iter()
        .map(|name| {
            let ty = crate::telemetry_schema::data_type(name);
            let packet = router.get_cached_network_variable(ty)?.ok_or(
                sedsnet::TelemetryError::HandlerError("bootstrap cache value missing"),
            )?;
            Ok(serde_json::json!({"name":name,"id":ty.as_u32(),"packet_id":packet.packet_id()}))
        })
        .collect::<sedsnet::TelemetryResult<Vec<_>>>();
    eprintln!(
        "[gs-qualification-bootstrap] {}",
        serde_json::json!({"event":"seal","phase":1,"source":BOOTSTRAP_SOURCE,"at_ms":crate::simulation_clock_ms(),"ok":result.is_ok()&&cached.is_ok()&&observation==(2,0,0),"configured_sides":observation.0,"discovered_announcers":observation.1,"pending_owners":observation.2,"cache":cached.as_ref().ok(),"meaning":"LOCAL_CACHE_PRIMED; remote delivery not proven"})
    );
    result?;
    cached?;
    anyhow::ensure!(
        observation == (2, 0, 0),
        "bootstrap ownership changed before phase sealed"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn bootstrap_phase_for_test() -> Option<u64> {
    BOOTSTRAP_PHASE.with(|slot| slot.get())
}

pub(crate) struct Request {
    token: Option<u64>,
    submissions: std::cell::Cell<u64>,
}
impl Request {
    pub(crate) fn begin(publisher: &'static str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let token = (cfg!(any(feature = "hitl_mode", feature = "firmware_sim"))
            && std::env::var("GS_SIM_TRACE_API_REQUESTS").as_deref() == Ok("1"))
        .then(|| NEXT.fetch_add(1, Ordering::Relaxed));
        if let Some(token) = token {
            trace_scope();
            emit(
                serde_json::json!({"event":"begin","token":token,"publisher":publisher,"request_ms":crate::simulation_clock_ms(),"controlled_round":CONTROLLED_ROUND.with(|slot|slot.get()),"bootstrap_phase":BOOTSTRAP_PHASE.with(|slot|slot.get())}),
            );
        }
        Self {
            token,
            submissions: std::cell::Cell::new(0),
        }
    }
    pub(crate) fn submit(&self, router: &Router, packet: Packet) -> anyhow::Result<()> {
        if self.token.is_none() {
            return router.set_network_variable(packet).map_err(Into::into);
        }
        let id = packet.packet_id();
        if let Some(token) = self.token {
            emit(
                serde_json::json!({"event":"bind","token":token,"packet_id":id,"submit_ms":crate::simulation_clock_ms(),"data_type":packet.data_type().as_u32()}),
            );
            self.submissions.set(self.submissions.get() + 1);
        }
        let result = router.set_network_variable(packet);
        if let Some(token) = self.token {
            emit(
                serde_json::json!({"event":"submit_result","token":token,"packet_id":id,"ok":result.is_ok(),"at_ms":crate::simulation_clock_ms()}),
            );
        }
        result.map_err(Into::into)
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if let Some(token) = self.token {
            emit(
                serde_json::json!({"event":"return","token":token,"submissions":self.submissions.get(),"at_ms":crate::simulation_clock_ms()}),
            );
        }
    }
}
fn emit(event: serde_json::Value) {
    eprintln!("[gs-qualification-request] {event}");
}

pub(crate) fn policy_states(hitl: bool, requested: &[u8]) -> anyhow::Result<Vec<u8>> {
    if hitl {
        return Ok(requested.to_vec());
    }
    anyhow::ensure!(
        requested.iter().all(|state| matches!(state, 0 | 1)),
        "normal qualification must remain within nonactuating prelaunch policy"
    );
    Ok(vec![1; requested.len()])
}
#[cfg(test)]
mod tests {
    #[test]
    fn normal_stable_idle_and_hitl_state_exercise_preserve_cosmetic_round_count() {
        assert_eq!(
            super::policy_states(false, &[1, 0, 1]).unwrap(),
            vec![1, 1, 1]
        );
        assert_eq!(
            super::policy_states(true, &[1, 0, 1]).unwrap(),
            vec![1, 0, 1]
        );
        assert!(super::policy_states(false, &[2]).is_err());
    }
}
