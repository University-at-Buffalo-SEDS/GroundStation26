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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Record")]
pub struct Qualification {
    version: u8,
    generation: u64,
    self_test: bool,
    nitrogen_test: bool,
    fill_target: bool,
    fault: Option<Fault>,
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
}
impl TryFrom<Record> for Qualification {
    type Error = &'static str;
    fn try_from(r: Record) -> Result<Self, Self::Error> {
        if r.version != 1 {
            return Err("Unsupported qualification version");
        }
        if (r.nitrogen_test && !r.self_test) || (r.fill_target && !r.nitrogen_test) {
            return Err("Invalid prerequisite order in saved qualification");
        }
        Ok(Self {
            version: r.version,
            generation: r.generation,
            self_test: r.self_test,
            nitrogen_test: r.nitrogen_test,
            fill_target: r.fill_target,
            fault: r.fault,
        })
    }
}
impl Default for Qualification {
    fn default() -> Self {
        Self {
            version: 1,
            generation: 0,
            self_test: false,
            nitrogen_test: false,
            fill_target: false,
            fault: None,
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
            && match stage {
                Milestone::SelfTest => true,
                Milestone::NitrogenTest => self.self_test,
                Milestone::FillTarget => self.nitrogen_test,
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
            Milestone::NitrogenTest => self.nitrogen_test = true,
            Milestone::FillTarget => self.fill_target = true,
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
    pub fn launch_qualified(&self) -> bool {
        self.fill_target && self.fault.is_none()
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
