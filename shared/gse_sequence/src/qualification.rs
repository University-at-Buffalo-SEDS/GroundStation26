//! Persistable operator qualification, independent of live valve confirmations.
//! Completing a test never bypasses fresh pressure, connectivity or launch gates.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Milestone {
    SelfTest,
    NitrogenTest,
    FillTarget,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    TopologyLost,
    UnsafeSupplyFlow,
    ValveMismatch,
    UnsafePressure,
    MissingTelemetry,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStage {
    Idle,
    Fill,
    Armed,
    Fire,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColdFlowOverride {
    pub operator: String,
    pub reason: String,
    pub at_ms: i64,
}
impl ColdFlowOverride {
    fn valid(&self) -> bool {
        !self.operator.trim().is_empty()
            && self.operator.len() <= 80
            && !self.reason.trim().is_empty()
            && self.reason.len() <= 500
            && self.at_ms >= 0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Record")]
pub struct Qualification {
    version: u8,
    generation: u64,
    self_test: bool,
    nitrogen_test: bool,
    fill_target: bool,
    fault: Option<Fault>,
    nitrogen_override: Option<ColdFlowOverride>,
    filling_started: bool,
    fire_started: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    generation: u64,
    self_test: bool,
    nitrogen_test: bool,
    fill_target: bool,
    fault: Option<Fault>,
    nitrogen_override: Option<ColdFlowOverride>,
    filling_started: Option<bool>,
    fire_started: Option<bool>,
}
impl TryFrom<Record> for Qualification {
    type Error = &'static str;
    fn try_from(r: Record) -> Result<Self, Self::Error> {
        let (filling_started, fire_started) = match r.version {
            1 if r.nitrogen_override.is_none()
                && r.filling_started.is_none()
                && r.fire_started.is_none() =>
            {
                (r.fill_target, false)
            }
            2 => (
                r.filling_started.ok_or("Missing fill stage")?,
                r.fire_started.ok_or("Missing Fire stage")?,
            ),
            _ => return Err("Unsupported qualification version"),
        };
        let nitrogen_satisfied = r.nitrogen_test || r.nitrogen_override.is_some();
        if (nitrogen_satisfied && !r.self_test)
            || (r.fill_target && (!nitrogen_satisfied || !filling_started))
            || (filling_started && !nitrogen_satisfied)
            || (r.nitrogen_test && r.nitrogen_override.is_some())
            || r.nitrogen_override.as_ref().is_some_and(|v| !v.valid())
            || (fire_started && (nitrogen_satisfied || r.fill_target || filling_started))
        {
            return Err("Invalid prerequisite order in saved qualification");
        }
        Ok(Self {
            version: 2,
            generation: r.generation,
            self_test: r.self_test,
            nitrogen_test: r.nitrogen_test,
            fill_target: r.fill_target,
            fault: r.fault,
            nitrogen_override: r.nitrogen_override,
            filling_started,
            fire_started,
        })
    }
}
impl Default for Qualification {
    fn default() -> Self {
        Self {
            version: 2,
            generation: 0,
            self_test: false,
            nitrogen_test: false,
            fill_target: false,
            fault: None,
            nitrogen_override: None,
            filling_started: false,
            fire_started: false,
        }
    }
}
impl Qualification {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn completed(&self, stage: Milestone) -> bool {
        match stage {
            Milestone::SelfTest => self.self_test,
            Milestone::NitrogenTest => self.nitrogen_test,
            Milestone::FillTarget => self.fill_target,
        }
    }
    pub fn fault(&self) -> Option<Fault> {
        self.fault
    }
    pub fn permits(&self, stage: Milestone) -> bool {
        self.fault.is_none()
            && !self.fire_started
            && match stage {
                Milestone::SelfTest => true,
                Milestone::NitrogenTest => self.self_test,
                Milestone::FillTarget => self.nitrogen_satisfied(),
            }
    }
    /// Generation belongs to the run's start. A delayed completion after an
    /// operator reset must never re-qualify the newly reset workflow.
    pub fn complete(&mut self, stage: Milestone, generation: u64) -> Result<(), &'static str> {
        if generation != self.generation {
            return Err("Qualification was reset during this run");
        }
        if !self.permits(stage) {
            return Err("Qualification prerequisite missing or safety fault latched");
        }
        match stage {
            Milestone::SelfTest => self.self_test = true,
            Milestone::NitrogenTest => {
                self.nitrogen_test = true;
                self.nitrogen_override = None;
            }
            Milestone::FillTarget => {
                self.fill_target = true;
                self.filling_started = true;
            }
        };
        Ok(())
    }
    pub fn latch(&mut self, fault: Fault) {
        if self.fault.is_none() {
            self.fault = Some(fault)
        }
    }
    /// Explicit operator action; power cycles and service restarts do not call it.
    pub fn reset(&mut self) -> Result<(), &'static str> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or("Qualification generation exhausted")?;
        *self = Self {
            generation,
            ..Self::default()
        };
        Ok(())
    }
    pub fn stage(&self) -> MissionStage {
        if self.fire_started {
            MissionStage::Fire
        } else if self.fill_target {
            MissionStage::Armed
        } else if self.filling_started {
            MissionStage::Fill
        } else {
            MissionStage::Idle
        }
    }
    pub fn nitrogen_satisfied(&self) -> bool {
        self.nitrogen_test || self.nitrogen_override.is_some()
    }
    pub fn nitrogen_override(&self) -> Option<&ColdFlowOverride> {
        self.nitrogen_override.as_ref()
    }
    pub fn override_cold_flow(&mut self, evidence: ColdFlowOverride) -> Result<(), &'static str> {
        if !evidence.valid() || !self.permits(Milestone::NitrogenTest) || self.filling_started {
            return Err("Cold-flow override requires a passed self-test, Idle stage and no fault");
        }
        if self.nitrogen_test {
            return Err("Nitrogen test is already complete");
        }
        self.nitrogen_override = Some(evidence);
        Ok(())
    }
    pub fn begin_fill(&mut self, generation: u64) -> Result<(), &'static str> {
        if generation != self.generation || !self.permits(Milestone::FillTarget) {
            return Err("Fill prerequisites are not satisfied");
        }
        self.filling_started = true;
        self.fill_target = false;
        Ok(())
    }
    /// Durable Fire intent is committed before dispatch. A restart must never replay it.
    pub fn commit_fire(&mut self) -> Result<(), &'static str> {
        if !self.launch_qualified() {
            return Err("Operation is not armed");
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or("Qualification generation exhausted")?;
        self.fire_started = true;
        self.nitrogen_test = false;
        self.nitrogen_override = None;
        self.fill_target = false;
        self.filling_started = false;
        Ok(())
    }
    pub fn launch_qualified(&self) -> bool {
        self.fill_target && self.nitrogen_satisfied() && !self.fire_started && self.fault.is_none()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatingMode {
    TestFire,
    FlightLaunch,
}
/// Live conditions are evaluated at dispatch, even after persisted tests passed.
pub fn live_launch_gate(
    mode: OperatingMode,
    fill_topology_stable: bool,
    fc_roundtrip_routes: bool,
    rocket_gps_locked: bool,
) -> bool {
    fill_topology_stable
        && (mode == OperatingMode::TestFire || (fc_roundtrip_routes && rocket_gps_locked))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationStage {
    Idle,
    Prelaunch,
    LaunchCountdown,
    Flight,
}
pub fn automatic_abort_allowed(stage: OperationStage) -> bool {
    stage == OperationStage::Prelaunch
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_order_survives_serialization_and_restart() {
        let mut q = Qualification::default();
        assert!(q.complete(Milestone::NitrogenTest, 0).is_err());
        assert!(q.complete(Milestone::FillTarget, 0).is_err());
        for stage in [
            Milestone::SelfTest,
            Milestone::NitrogenTest,
            Milestone::FillTarget,
        ] {
            q.complete(stage, 0).unwrap();
            q = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
            assert!(q.completed(stage));
        }
        assert!(q.launch_qualified());
        q.complete(Milestone::SelfTest, 0).unwrap();
        assert!(q.launch_qualified());
    }
    #[test]
    fn malformed_and_out_of_order_saved_states_fail_closed() {
        for body in [
            r#"{"version":2,"generation":0,"self_test":false,"nitrogen_test":false,"fill_target":false,"fault":null}"#,
            r#"{"version":1,"generation":0,"self_test":false,"nitrogen_test":true,"fill_target":true,"fault":null}"#,
            r#"{"version":1,"generation":0,"self_test":true,"nitrogen_test":false,"fill_target":true,"fault":null}"#,
        ] {
            assert!(serde_json::from_str::<Qualification>(body).is_err());
        }
    }
    #[test]
    fn safety_fault_is_latched_through_restart_until_explicit_reset() {
        let mut q = Qualification::default();
        q.complete(Milestone::SelfTest, 0).unwrap();
        q.latch(Fault::UnsafeSupplyFlow);
        q.latch(Fault::TopologyLost);
        let mut restored: Qualification =
            serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(restored.fault(), Some(Fault::UnsafeSupplyFlow));
        assert!(!restored.permits(Milestone::NitrogenTest));
        restored.reset().unwrap();
        assert_eq!(restored.fault(), None);
        assert!(!restored.completed(Milestone::SelfTest));
        assert!(restored.complete(Milestone::SelfTest, 0).is_err());
        restored.complete(Milestone::SelfTest, 1).unwrap();
    }
    #[test]
    fn test_fire_never_requires_absent_avionics() {
        assert!(live_launch_gate(
            OperatingMode::TestFire,
            true,
            false,
            false
        ));
        assert!(!live_launch_gate(
            OperatingMode::TestFire,
            false,
            true,
            true
        ));
        for (route, gps) in [(false, false), (false, true), (true, false)] {
            assert!(!live_launch_gate(
                OperatingMode::FlightLaunch,
                true,
                route,
                gps
            ));
        }
        assert!(live_launch_gate(
            OperatingMode::FlightLaunch,
            true,
            true,
            true
        ));
    }
    #[test]
    fn automatic_abort_boundary_includes_launch_countdown() {
        assert!(!automatic_abort_allowed(OperationStage::Idle));
        assert!(automatic_abort_allowed(OperationStage::Prelaunch));
        assert!(!automatic_abort_allowed(OperationStage::LaunchCountdown));
        assert!(!automatic_abort_allowed(OperationStage::Flight));
    }
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    fn passed() -> Qualification {
        let mut q = Qualification::default();
        q.complete(Milestone::SelfTest, 0).unwrap();
        q.complete(Milestone::NitrogenTest, 0).unwrap();
        q
    }
    #[test]
    fn idle_fill_armed_fire_survives_restart_and_consumes_cold_flow_once() {
        let mut q = passed();
        assert_eq!(q.stage(), MissionStage::Idle);
        q.begin_fill(0).unwrap();
        assert_eq!(q.stage(), MissionStage::Fill);
        q = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        q.complete(Milestone::FillTarget, 0).unwrap();
        assert_eq!(q.stage(), MissionStage::Armed);
        assert!(q.launch_qualified());
        q.commit_fire().unwrap();
        let mut restored: Qualification =
            serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(restored.stage(), MissionStage::Fire);
        assert!(!restored.nitrogen_satisfied());
        assert!(!restored.launch_qualified());
        assert!(restored.commit_fire().is_err());
        assert!(restored.complete(Milestone::NitrogenTest, 0).is_err());
        assert!(restored.begin_fill(restored.generation()).is_err());
        restored.reset().unwrap();
        assert_eq!(restored.stage(), MissionStage::Idle);
        assert!(!restored.nitrogen_satisfied());
    }
    #[test]
    fn override_is_audited_and_never_misrepresented_as_a_passed_test() {
        let mut q = Qualification::default();
        let evidence = ColdFlowOverride {
            operator: "operator".into(),
            reason: "Approved external cold-flow procedure".into(),
            at_ms: 123,
        };
        assert!(q.override_cold_flow(evidence.clone()).is_err());
        q.complete(Milestone::SelfTest, 0).unwrap();
        q.override_cold_flow(evidence.clone()).unwrap();
        let mut restored: Qualification =
            serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(restored.nitrogen_override(), Some(&evidence));
        assert!(!restored.completed(Milestone::NitrogenTest));
        assert!(restored.permits(Milestone::FillTarget));
        restored.complete(Milestone::NitrogenTest, 0).unwrap();
        assert!(restored.completed(Milestone::NitrogenTest));
        assert!(restored.nitrogen_override().is_none());
    }
    #[test]
    fn old_saved_results_migrate_without_inventing_new_evidence() {
        let old = r#"{"version":1,"generation":4,"self_test":true,"nitrogen_test":true,"fill_target":true,"fault":null}"#;
        let q: Qualification = serde_json::from_str(old).unwrap();
        assert_eq!(q.stage(), MissionStage::Armed);
        assert_eq!(q.generation(), 4);
        assert!(q.nitrogen_override().is_none());
        assert_eq!(serde_json::to_value(q).unwrap()["version"], 2);
    }
    #[test]
    fn fire_and_overrides_cannot_be_reconstituted_from_invalid_records() {
        let mut q = passed();
        q.complete(Milestone::FillTarget, 0).unwrap();
        let mut value = serde_json::to_value(&q).unwrap();
        value["fire_started"] = true.into();
        assert!(serde_json::from_value::<Qualification>(value).is_err());
        q.latch(Fault::UnsafeSupplyFlow);
        assert!(q.commit_fire().is_err());
        assert!(
            q.override_cold_flow(ColdFlowOverride {
                operator: "operator".into(),
                reason: " ".into(),
                at_ms: 1
            })
            .is_err()
        );
    }
}
