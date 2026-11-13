//! Programme-frame projection only. No live requests, output writes or flow inference.
use gse_sequence::{Phase, ProgressObservation};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Clock {
    pub kind: String,
    pub source_kind: String,
    pub sampled_timestamp_ms: i64,
    pub anchor_timestamp_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub remaining_ms: Option<i64>,
    /// Set from adjacent captured programme samples, never from formatted text.
    pub advancing: bool,
}
#[derive(Clone, Copy)]
pub struct CapturedOutput {
    pub value: bool,
    pub age_ms: u64,
}
fn output(value: Option<CapturedOutput>) -> serde_json::Value {
    value.filter(|v|v.age_ms<=2000).map_or(serde_json::Value::Null,|v|serde_json::json!({"value":v.value,"age_ms":v.age_ms,"valid_for_ms":2000,"evidence":"board_reported"}))
}
fn queued_command(value: Option<CapturedOutput>) -> serde_json::Value {
    value.filter(|v|v.age_ms<=2000).map_or(serde_json::Value::Null,|v|serde_json::json!({"value":v.value,"age_ms":v.age_ms,"valid_for_ms":2000,"evidence":"command_accepted","source_kind":"station_command_queue"}))
}
pub fn project(
    clock: Clock,
    view: Option<ProgressObservation>,
    outputs: [Option<CapturedOutput>; 6],
    commands: [Option<CapturedOutput>; 6],
) -> serde_json::Value {
    let planned = view.filter(|v| {
        !matches!(
            v.phase,
            Phase::Idle | Phase::Fault | Phase::Passed | Phase::Cancelled
        )
    });
    let mut valves = serde_json::Map::new();
    for (i, name) in ["pilot", "vent", "dump", "nitrogen", "nitrous"]
        .into_iter()
        .enumerate()
    {
        let plan=planned.map(|v|serde_json::json!({"value":v.valves[i].target_open,"age_ms":0,"valid_for_ms":2000,"evidence":"planned"}));
        let flow=planned.filter(|v|v.valves[i].target_open).map(|_|serde_json::json!({"active":true,"source_kind":"sequence_plan","age_ms":0,"valid_for_ms":2000,"evidence":"planned"}));
        valves.insert(
            name.into(),
            serde_json::json!({"output":output(outputs[i]),"command":queued_command(commands[i]),"plan":plan,"flow":flow}),
        );
    }
    serde_json::json!({"version":1,"clock":clock,"igniter":{"planned_t_minus_ms":5000,"plan_source":"presentation_t_minus_5","output":output(outputs[5]),"command":queued_command(commands[5])},"valves":valves,"flow_measurement_available":false})
}
/// Mutates only the new captured frame. A held model clock is not advancing;
/// historical/replay frames retain their own original observed clock/equipment.
pub fn observe_clock_progress(
    previous: Option<&serde_json::Value>,
    current: &mut serde_json::Value,
) {
    let current_clock = current.pointer_mut("/sequence_visuals/clock");
    let Some(clock) = current_clock else {
        return;
    };
    let advancing = previous
        .and_then(|p| p.pointer("/sequence_visuals/clock"))
        .is_some_and(|old| {
            !clock["anchor_timestamp_ms"].is_null()
                && old["kind"] == clock["kind"]
                && old["source_kind"] == clock["source_kind"]
                && old["anchor_timestamp_ms"] == clock["anchor_timestamp_ms"]
                && old["duration_ms"] == clock["duration_ms"]
                && old["sampled_timestamp_ms"]
                    .as_i64()
                    .zip(clock["sampled_timestamp_ms"].as_i64())
                    .is_some_and(|(a, b)| b > a && b.saturating_sub(a) <= 2000)
        });
    clock["advancing"] = serde_json::json!(advancing);
}
#[cfg(test)]
mod tests {
    use super::*;
    use gse_sequence::{Engine, Inputs};
    fn clock(at: i64) -> Clock {
        Clock {
            kind: "t_minus".into(),
            source_kind: "model_procedure".into(),
            sampled_timestamp_ms: at,
            anchor_timestamp_ms: Some(1000),
            duration_ms: Some(10000),
            remaining_ms: Some(5000),
            advancing: false,
        }
    }
    fn frame(at: i64) -> serde_json::Value {
        serde_json::json!({"sequence_visuals":project(clock(at),None,[None;6],[None;6])})
    }
    #[test]
    fn same_frame_clock_holds_pause_and_does_not_invent_replay_equipment() {
        let before = frame(6000);
        let mut after = frame(6250);
        observe_clock_progress(Some(&before), &mut after);
        assert_eq!(after["sequence_visuals"]["clock"]["advancing"], true);
        let mut paused = frame(6250);
        observe_clock_progress(Some(&after), &mut paused);
        assert_eq!(paused["sequence_visuals"]["clock"]["advancing"], false);
        let mut reset = frame(1000);
        observe_clock_progress(Some(&paused), &mut reset);
        assert_eq!(reset["sequence_visuals"]["clock"]["advancing"], false);
        assert!(after["sequence_visuals"]["igniter"]["output"].is_null());
        assert!(after["sequence_visuals"]["valves"]["nitrogen"]["flow"].is_null());
        let old = serde_json::json!({"t_clock":"T− 00:05.00"});
        let mut still_old = old.clone();
        observe_clock_progress(Some(&after), &mut still_old);
        assert_eq!(still_old, old);
    }
    #[test]
    fn source_readback_remains_separate_from_plan_and_measured_flow() {
        let engine = Engine::default();
        let mut view = engine.progress_observation(Inputs {
            now_ms: 100,
            pressure: None,
            valves: [None; 5],
            interlock: true,
            prelaunch: true,
        });
        view.phase = Phase::Filling;
        view.valves[4].target_open = true;
        let mut outputs = [None; 6];
        outputs[4] = Some(CapturedOutput {
            value: false,
            age_ms: 1800,
        });
        outputs[5] = Some(CapturedOutput {
            value: true,
            age_ms: 2001,
        });
        let value = project(clock(6000), Some(view), outputs, [None; 6]);
        assert_eq!(value["valves"]["nitrous"]["plan"]["value"], true);
        assert_eq!(value["valves"]["nitrous"]["output"]["value"], false);
        assert_eq!(value["valves"]["nitrous"]["flow"]["evidence"], "planned");
        assert!(value["valves"]["nitrous"]["command"].is_null());
        assert!(value["igniter"]["output"].is_null());
        assert_eq!(value["flow_measurement_available"], false);
    }
}
