//! Bounded presentation-only observations carried inside authenticated control replies.
//! No periodic stream, route installation, desired-state confirmation or control authority.
use crate::{
    sequences::{ActionControl, ActionPolicyMsg, BlinkMode},
    state::AppState,
    types::{Board, NetworkTopologyStatus},
};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
const VERSION: u8 = 1;
const LEN: usize = 57;
const UNKNOWN: u16 = u16::MAX;
const SOURCE_FRESH_MS: u64 = 1500;
const VALVES: [&str; 5] = ["Pilot", "NormallyOpen", "Dump", "Nitrogen", "Nitrous"];
const FILL: [Board; 4] = [
    Board::GatewayBoard,
    Board::ActuatorBoard,
    Board::ValveBoard,
    Board::DaqBoard,
];
const SENDERS: [&str; 5] = ["GS", "GB", "AB", "VB", "DAQ"];
const PHASES: [&str; 21] = [
    "idle",
    "baseline_setup",
    "baseline",
    "self_setup",
    "self_open",
    "self_hold",
    "self_close",
    "self_finish",
    "nitrogen_setup",
    "raising",
    "settling",
    "holding",
    "dumping",
    "passed",
    "fill_setup",
    "filling",
    "pause_setup",
    "paused",
    "cancel_setup",
    "cancelled",
    "fault",
];
#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    flags: u8,
    enabled: u64,
    slow: u64,
    fast: u64,
    valve_known: u8,
    valve_open: u8,
    valve_ages: [u16; 5],
    board_ages: [u16; 4],
    cpu: i16,
    cpu_age: u16,
    edges: u16,
    phase: u8,
    step_bits: u32,
}
struct Received {
    owner: usize,
    observation: Observation,
    at: Instant,
    transit_ms: u64,
}
static CACHE: OnceLock<Mutex<Option<Received>>> = OnceLock::new();
static COMMAND_BARRIER: OnceLock<Mutex<Option<(usize, Instant)>>> = OnceLock::new();
fn command_barrier() -> &'static Mutex<Option<(usize, Instant)>> {
    COMMAND_BARRIER.get_or_init(|| Mutex::new(None))
}

fn cache() -> &'static Mutex<Option<Received>> {
    CACHE.get_or_init(|| Mutex::new(None))
}
fn age(value: Option<u64>) -> u16 {
    value
        .and_then(|v| u16::try_from(v).ok())
        .filter(|v| *v != UNKNOWN)
        .unwrap_or(UNKNOWN)
}
fn aged(source: u16, elapsed: u64) -> Option<u64> {
    (source != UNKNOWN).then(|| u64::from(source).saturating_add(elapsed))
}
fn valve_observation(r: &Observation, index: usize, elapsed: u64) -> Option<bool> {
    (r.valve_known & (1 << index) != 0
        && aged(r.valve_ages[index], elapsed).is_some_and(|age| age <= SOURCE_FRESH_MS))
    .then_some(r.valve_open & (1 << index) != 0)
}
impl Observation {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION, self.flags];
        for mask in [self.enabled, self.slow, self.fast] {
            out.extend(mask.to_le_bytes());
        }
        out.extend([self.valve_known, self.valve_open]);
        for value in self.valve_ages.into_iter().chain(self.board_ages) {
            out.extend(value.to_le_bytes());
        }
        out.extend(self.cpu.to_le_bytes());
        out.extend(self.cpu_age.to_le_bytes());
        out.extend(self.edges.to_le_bytes());
        out.push(self.phase);
        out.extend(self.step_bits.to_le_bytes());
        out
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != LEN
            || bytes[0] != VERSION
            || bytes[1] & !31 != 0
            || bytes[26] & !31 != 0
            || bytes[27] & !31 != 0
            || bytes[52] as usize >= PHASES.len()
        {
            return None;
        }
        let step_bits = u32::from_le_bytes(bytes[53..57].try_into().unwrap());
        if !f32::from_bits(step_bits).is_finite() {
            return None;
        }
        let word = |i| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let mask = |i| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        let edges = word(50);
        if edges & !1023 != 0 {
            return None;
        }
        Some(Self {
            flags: bytes[1],
            enabled: mask(2),
            slow: mask(10),
            fast: mask(18),
            valve_known: bytes[26],
            valve_open: bytes[27],
            valve_ages: std::array::from_fn(|i| word(28 + i * 2)),
            board_ages: std::array::from_fn(|i| word(38 + i * 2)),
            cpu: i16::from_le_bytes([bytes[46], bytes[47]]),
            cpu_age: word(48),
            edges,
            phase: bytes[52],
            step_bits,
        })
    }
}
pub fn encode_control_observation(state: &AppState, max_bytes: usize) -> Vec<u8> {
    if max_bytes < LEN || crate::rrls::is_rrls() {
        return Vec::new();
    }
    let policy = state.action_policy_snapshot();
    let mut out = Observation {
        flags: u8::from(policy.key_enabled) | (u8::from(policy.software_buttons_enabled) << 1),
        enabled: 0,
        slow: 0,
        fast: 0,
        valve_known: 0,
        valve_open: 0,
        valve_ages: [UNKNOWN; 5],
        board_ages: [UNKNOWN; 4],
        cpu: 0,
        cpu_age: UNKNOWN,
        edges: 0,
        phase: 0,
        step_bits: 0,
    };
    for (i, name) in crate::command_proxy::command_catalog()
        .iter()
        .take(64)
        .enumerate()
    {
        if let Some(control) = policy.controls.iter().find(|c| c.cmd == *name) {
            if control.enabled {
                out.enabled |= 1 << i;
            }
            match control.blink {
                BlinkMode::Slow => out.slow |= 1 << i,
                BlinkMode::Fast => out.fast |= 1 << i,
                BlinkMode::None => {}
            }
        }
    }
    {
        let runtime = state.gse.lock().unwrap();
        for (i, value) in runtime.mirror_valves().into_iter().enumerate() {
            if let Some((open, source_age)) = value {
                let key = [0, 1, 2, 9, 10][i];
                let barrier = *command_barrier().lock().unwrap();
                if barrier.is_some_and(|(owner, at)| {
                    owner == state as *const _ as usize && !state.umbilical_report_after(key, at)
                }) {
                    continue;
                }
                // A still-fresh previous report cannot confirm a newly pending request.
                if state.get_pending_umbilical_valve_state(key).is_some()
                    || state.get_umbilical_valve_state(key) != Some(open)
                {
                    continue;
                }
                out.valve_known |= 1 << i;
                if open {
                    out.valve_open |= 1 << i;
                }
                out.valve_ages[i] = age(Some(source_age));
            }
        }
        let status = &runtime.engine.status;
        if !status.current_step_psi.is_finite() {
            return Vec::new();
        }
        out.step_bits = status.current_step_psi.to_bits();
        out.flags |= (u8::from(status.nitrogen_passed) << 2)
            | (u8::from(status.nitrogen_overridden) << 3)
            | (u8::from(status.self_test_locked) << 4);
        let phase = serde_json::to_value(status.phase).ok();
        out.phase = phase
            .as_ref()
            .and_then(|v| v.as_str())
            .and_then(|v| PHASES.iter().position(|p| *p == v))
            .unwrap_or(0) as u8;
    }
    let now = crate::telemetry_task::get_current_timestamp_ms();
    let boards = state.board_status_snapshot(now);
    for (i, board) in FILL.iter().enumerate() {
        out.board_ages[i] = age(boards
            .boards
            .iter()
            .find(|b| b.board == *board)
            .and_then(|b| b.age_ms));
    }
    if let Some((value, source_age)) = crate::host_temperature::mirror_sample() {
        out.cpu = value as i16;
        out.cpu_age = age(Some(source_age));
    }
    let topology = state.network_topology_snapshot(now);
    let mut bit = 0;
    for a in 0..5 {
        for b in a + 1..5 {
            let ids = |sender: &str| {
                topology
                    .nodes
                    .iter()
                    .filter(move |n| n.sender_id.as_deref() == Some(sender))
                    .map(|n| n.id.as_str())
                    .collect::<Vec<_>>()
            };
            let left = ids(SENDERS[a]);
            let right = ids(SENDERS[b]);
            if topology.links.iter().any(|edge| {
                edge.status != NetworkTopologyStatus::Offline
                    && ((left.contains(&edge.source.as_str())
                        && right.contains(&edge.target.as_str()))
                        || (right.contains(&edge.source.as_str())
                            && left.contains(&edge.target.as_str())))
            }) {
                out.edges |= 1 << bit;
            }
            bit += 1;
        }
    }
    out.encode()
}
/// Caller authenticates and matches the current boot + nonce/digest before invoking.
/// Whole original request RTT bounds transit even with replayed cached replies.
pub fn accept_control_observation(
    state: &AppState,
    bytes: &[u8],
    roundtrip_upper_bound_ms: u64,
) -> bool {
    let Some(observation) = Observation::decode(bytes) else {
        return false;
    };
    *cache().lock().unwrap() = Some(Received {
        owner: state as *const _ as usize,
        observation,
        at: Instant::now(),
        transit_ms: roundtrip_upper_bound_ms,
    });
    true
}
pub fn note_command_outcome(state: &AppState) {
    // Primary HTTP/channel admission can precede the command worker's pending mark.
    // Require a NEW actual board report; neither admission nor target values qualify.
    *command_barrier().lock().unwrap() = Some((state as *const _ as usize, Instant::now()));
}
pub fn invalidate_control_observation() {
    *cache().lock().unwrap() = None;
}
fn with_received<T>(state: &AppState, f: impl FnOnce(&Observation, u64) -> T) -> Option<T> {
    let guard = cache().lock().unwrap();
    let r = guard.as_ref()?;
    if r.owner != state as *const _ as usize {
        return None;
    }
    Some(f(
        &r.observation,
        r.transit_ms
            .saturating_add(r.at.elapsed().as_millis().min(u64::MAX as u128) as u64),
    ))
}
/// Presentation policy only. The primary still performs every permission/interlock check.
pub fn action_policy(state: &AppState) -> Option<ActionPolicyMsg> {
    with_received(state, |r, elapsed| {
        let fresh = elapsed <= SOURCE_FRESH_MS;
        ActionPolicyMsg {
            key_enabled: fresh && r.flags & 1 != 0,
            software_buttons_enabled: fresh && r.flags & 2 != 0,
            controls: crate::command_proxy::command_catalog()
                .iter()
                .take(64)
                .enumerate()
                .map(|(i, name)| {
                    let actuated = VALVES
                        .iter()
                        .position(|v| v == name)
                        .and_then(|index| valve_observation(r, index, elapsed));
                    ActionControl {
                        cmd: (*name).into(),
                        enabled: fresh && r.enabled & (1 << i) != 0,
                        blink: if !fresh {
                            BlinkMode::None
                        } else if r.fast & (1 << i) != 0 {
                            BlinkMode::Fast
                        } else if r.slow & (1 << i) != 0 {
                            BlinkMode::Slow
                        } else {
                            BlinkMode::None
                        },
                        actuated,
                    }
                })
                .collect(),
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> Observation {
        Observation {
            flags: 3,
            enabled: 5,
            slow: 1,
            fast: 4,
            valve_known: 31,
            valve_open: 10,
            valve_ages: [0, 1, 1499, 1500, 65534],
            board_ages: [1, 2, 3, UNKNOWN],
            cpu: 4532,
            cpu_age: 5000,
            edges: 513,
            phase: 20,
            step_bits: 50.0f32.to_bits(),
        }
    }
    #[test]
    fn bounded_roundtrip_and_malformed_rejection() {
        let value = example();
        let bytes = value.encode();
        assert_eq!(bytes.len(), LEN);
        assert!(LEN <= 96);
        assert_eq!(Observation::decode(&bytes), Some(value));
        for len in 0..LEN {
            assert!(Observation::decode(&bytes[..len]).is_none());
        }
        for (at, bad) in [(0, 2), (1, 128), (26, 128), (27, 128), (51, 128), (52, 255)] {
            let mut badbytes = bytes.clone();
            badbytes[at] = bad;
            assert!(Observation::decode(&badbytes).is_none());
        }
    }
    #[tokio::test]
    async fn delayed_source_reports_expire_without_confirming_command_intent() {
        let state = crate::state::tests::test_app_state().await;
        let mut snapshot = example();
        snapshot.valve_ages = [0, 1499, UNKNOWN, 0, 0];
        snapshot.valve_known = 31;
        snapshot.valve_open = 31;
        assert!(accept_control_observation(&state, &snapshot.encode(), 10));
        let policy = action_policy(&state).unwrap();
        let valve = |name: &str| {
            policy
                .controls
                .iter()
                .find(|c| c.cmd == name)
                .unwrap()
                .actuated
        };
        assert_eq!(valve("Pilot"), Some(true));
        assert_eq!(valve("Nitrogen"), Some(true));
        // Unknown ages are not refreshed by a valid authenticated envelope.
        assert_eq!(valve("Dump"), None);
        assert!(!accept_control_observation(&state, &[1], 0));
        assert!(
            action_policy(&state).is_some(),
            "malformed optional observation cannot erase prior evidence"
        );
        state.set_umbilical_valve_state(0, false);
        assert_ne!(
            Observation::decode(&encode_control_observation(&state, 96))
                .unwrap()
                .valve_known
                & 1,
            0
        );
        note_command_outcome(&state);
        assert_eq!(
            Observation::decode(&encode_control_observation(&state, 96))
                .unwrap()
                .valve_known
                & 1,
            0,
            "channel admission cannot reuse a still-fresh previous report"
        );
        state.set_umbilical_valve_state(0, false);
        let next = Observation::decode(&encode_control_observation(&state, 96)).unwrap();
        assert_ne!(
            next.valve_known & 1,
            0,
            "a new actual OFF report restores known OFF, independently of desired state"
        );
        assert_eq!(next.valve_open & 1, 0);
        assert_eq!(
            action_policy(&state)
                .unwrap()
                .controls
                .iter()
                .find(|c| c.cmd == "Dump")
                .unwrap()
                .actuated,
            None
        );
        assert!(accept_control_observation(&state, &snapshot.encode(), 1600));
        let expired = action_policy(&state).unwrap();
        assert!(
            gse_status(&state).is_none(),
            "expired primary phase must not masquerade as local GSE status"
        );
        assert!(!expired.software_buttons_enabled);
        assert!(
            expired
                .controls
                .iter()
                .all(|c| !c.enabled && c.actuated.is_none())
        );
        invalidate_control_observation();
        assert!(action_policy(&state).is_none());
    }
    #[test]
    fn five_second_health_reports_keep_twelve_second_liveness_and_fifteen_second_cpu_age() {
        use crate::types::{BoardStatusEntry, BoardStatusMsg, NetworkTopologyMsg};
        let mut snapshot = example();
        snapshot.valve_ages = [0; 5];
        snapshot.valve_open = 1;
        assert_eq!(valve_observation(&snapshot, 0, 1500), Some(true));
        assert_eq!(
            valve_observation(&snapshot, 0, 1501),
            None,
            "health cadence must never extend actual valve-state confirmation"
        );
        snapshot.board_ages = [5000, 5000, 5000, UNKNOWN];
        snapshot.cpu_age = 5000;
        snapshot.edges = 1; // Only the actually observed GS-to-gateway link.
        let project = |elapsed| {
            let mut boards = BoardStatusMsg {
                boards: FILL
                    .iter()
                    .map(|board| BoardStatusEntry {
                        board: *board,
                        board_label: board.as_str().into(),
                        sender_id: board.sender_id().into(),
                        seen: false,
                        packet_count: 0,
                        last_seen_ms: None,
                        age_ms: None,
                    })
                    .collect(),
            };
            let mut topology = NetworkTopologyMsg {
                generated_ms: 30_000,
                ..Default::default()
            };
            project_board_observation(&snapshot, elapsed, &mut boards, 30_000);
            project_topology_observation(&snapshot, elapsed, &mut topology);
            (boards, topology)
        };
        // The sample was already five seconds old at the primary; include the
        // full request RTT instead of requiring a new health packet every1.5s.
        let (boards, graph) = project(600);
        assert!(
            boards.boards[..3]
                .iter()
                .all(|row| row.seen && row.age_ms == Some(5600))
        );
        assert!(
            graph
                .nodes
                .iter()
                .filter(|node| node.sender_id.as_deref() != Some("GS"))
                .all(|node| node.status == NetworkTopologyStatus::Online
                    && node.age_ms == Some(5600))
        );
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| node.sender_id.as_deref() != Some("DAQ")),
            "absent board evidence must not create a node"
        );
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].status, NetworkTopologyStatus::Online);
        assert_eq!(graph.health[0].cpu_temperature_centi_c, Some(4532));
        assert_eq!(graph.health[0].cpu_temperature_age_ms, 5600);
        // Source age keeps advancing even if the same observation is inspected again.
        let (boards, graph) = project(7000);
        assert!(
            boards.boards[..3]
                .iter()
                .all(|row| row.seen && row.age_ms == Some(12000))
        );
        assert_eq!(graph.links[0].status, NetworkTopologyStatus::Online);
        let (boards, graph) = project(7001);
        assert!(
            boards.boards[..3]
                .iter()
                .all(|row| !row.seen && row.age_ms == Some(12001))
        );
        assert_eq!(graph.links[0].status, NetworkTopologyStatus::Offline);
        assert_eq!(graph.health[0].cpu_temperature_centi_c, Some(4532));
        let (_, graph) = project(10_000);
        assert_eq!(graph.health[0].cpu_temperature_centi_c, Some(4532));
        assert_eq!(graph.health[0].cpu_temperature_age_ms, 15000);
        let (_, graph) = project(10_001);
        assert_eq!(graph.health[0].cpu_temperature_centi_c, None);
        assert_eq!(graph.health[0].cpu_temperature_age_ms, 15001);
        assert!(
            graph.health[0].healthy,
            "unsupported/stale temperature is not evidence that the primary disconnected"
        );
        let (_, graph) = project(12_001);
        assert!(!graph.health[0].healthy);
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| node.status == NetworkTopologyStatus::Offline)
        );
    }
    #[test]
    fn age_is_conservative_unknown_and_overflow_stay_expired() {
        assert_eq!(aged(UNKNOWN, 0), None);
        assert_eq!(aged(1499, 2), Some(1501));
        assert_eq!(aged(1, u64::MAX), Some(u64::MAX));
        assert_eq!(age(Some(u64::MAX)), UNKNOWN);
        assert_eq!(age(Some(65534)), 65534);
        assert_eq!(age(Some(65535)), UNKNOWN);
    }
}

pub fn project_board_status(
    state: &AppState,
    out: &mut crate::types::BoardStatusMsg,
    wall_ms: u64,
) {
    if !crate::rrls::is_rrls() {
        return;
    }
    let _ = with_received(state, |r, elapsed| {
        project_board_observation(r, elapsed, out, wall_ms)
    });
}
// These ages include the original query/request RTT and local cache residence.
// A newly signed envelope does not reset an unchanged five-second health sample.
fn project_board_observation(
    r: &Observation,
    elapsed: u64,
    out: &mut crate::types::BoardStatusMsg,
    wall_ms: u64,
) {
    for (index, board) in FILL.iter().enumerate() {
        let Some(source_age) = aged(r.board_ages[index], elapsed) else {
            continue;
        };
        if let Some(row) = out.boards.iter_mut().find(|row| row.board == *board)
            && row.age_ms.is_none_or(|local| source_age < local)
        {
            row.age_ms = Some(source_age);
            row.last_seen_ms = Some(wall_ms.saturating_sub(source_age));
            row.seen = source_age <= crate::state::BOARD_SEEN_TIMEOUT_MS;
        }
    }
}
pub fn project_topology(state: &AppState, out: &mut crate::types::NetworkTopologyMsg) {
    if !crate::rrls::is_rrls() || out.simulated {
        return;
    }
    let _ = with_received(state, |r, elapsed| {
        project_topology_observation(r, elapsed, out)
    });
}
fn project_topology_observation(
    r: &Observation,
    elapsed: u64,
    out: &mut crate::types::NetworkTopologyMsg,
) {
    use crate::types::{
        NetworkBoardHealthEntry, NetworkTopologyLink, NetworkTopologyNode, NetworkTopologyNodeKind,
    };
    let wall = out.generated_ms;
    let mut selected: [Option<(String, bool)>; 5] = std::array::from_fn(|_| None);
    for i in 0..5 {
        let source_age = if i == 0 {
            Some(elapsed)
        } else {
            aged(r.board_ages[i - 1], elapsed)
        };
        let Some(source_age) = source_age else {
            continue;
        };
        let fresh = source_age <= crate::state::BOARD_SEEN_TIMEOUT_MS;
        let status = if fresh {
            NetworkTopologyStatus::Online
        } else {
            NetworkTopologyStatus::Offline
        };
        let id = format!("board_{}", SENDERS[i].to_ascii_lowercase());
        if let Some(node) = out
            .nodes
            .iter_mut()
            .find(|n| n.sender_id.as_deref() == Some(SENDERS[i]))
        {
            if node.age_ms.is_none_or(|local| source_age < local) {
                node.age_ms = Some(source_age);
                node.last_seen_ms = Some(wall.saturating_sub(source_age));
                node.status = status;
                node.detail = Some("Observed by primary ground station".into());
            }
            selected[i] = Some((
                node.id.clone(),
                node.status != NetworkTopologyStatus::Offline,
            ));
        } else {
            let label = Board::from_sender_id(SENDERS[i])
                .map(|b| b.as_str())
                .unwrap_or(SENDERS[i]);
            out.nodes.push(NetworkTopologyNode {
                id: id.clone(),
                label: label.into(),
                kind: NetworkTopologyNodeKind::Board,
                status,
                group: "primary observations".into(),
                sender_id: Some(SENDERS[i].into()),
                last_seen_ms: Some(wall.saturating_sub(source_age)),
                age_ms: Some(source_age),
                endpoints: Vec::new(),
                show_in_details: true,
                detail: Some("Observed by primary ground station".into()),
                stats: None,
            });
            selected[i] = Some((id, fresh));
        }
    }
    let mut bit = 0;
    for a in 0..5 {
        for b in a + 1..5 {
            if r.edges & (1 << bit) != 0
                && let (Some((left, lf)), Some((right, rf))) = (&selected[a], &selected[b])
                && !out.links.iter().any(|e| {
                    (&e.source == left && &e.target == right)
                        || (&e.source == right && &e.target == left)
                })
            {
                out.links.push(NetworkTopologyLink {
                    source: left.clone(),
                    target: right.clone(),
                    label: Some("Observed by primary".into()),
                    status: if elapsed <= crate::state::BOARD_SEEN_TIMEOUT_MS && *lf && *rf {
                        NetworkTopologyStatus::Online
                    } else {
                        NetworkTopologyStatus::Offline
                    },
                });
            }
            bit += 1;
        }
    }
    let cpu_age = aged(r.cpu_age, elapsed);
    let temperature = cpu_age
        .filter(|age| *age <= 15000)
        .map(|_| i32::from(r.cpu));
    if let Some(row) = out.health.iter_mut().find(|h| h.sender_id == "GS") {
        row.cpu_temperature_centi_c = temperature;
        row.cpu_temperature_age_ms = cpu_age.unwrap_or(0);
    } else {
        out.health.push(NetworkBoardHealthEntry {
            sender_id: "GS".into(),
            healthy: elapsed <= crate::state::BOARD_SEEN_TIMEOUT_MS,
            age_ms: elapsed,
            last_seen_ms: wall.saturating_sub(elapsed),
            cpu_temperature_centi_c: temperature,
            cpu_temperature_age_ms: cpu_age.unwrap_or(0),
            ..Default::default()
        });
    }
}

/// A sparse primary status is returned only while the authenticated capture is fresh.
/// Missing pressure/baseline/configuration are explicitly unknown, never local RRLS values.
pub fn gse_status(state: &AppState) -> Option<serde_json::Value> {
    with_received(state, |r, elapsed| {
        if elapsed > SOURCE_FRESH_MS { return None; }
        let valves:[Option<bool>;5]=std::array::from_fn(|index|valve_observation(r, index, elapsed));
        Some(serde_json::json!({
            "phase":PHASES[r.phase as usize],
            "message":"Observed at primary; configuration and qualification details must be viewed at the primary.",
            "nitrogen_passed":r.flags&4!=0,
            "nitrogen_overridden":r.flags&8!=0,
            "self_test_locked":r.flags&16!=0,
            "current_step_psi":f32::from_bits(r.step_bits),
            "pressure_psi":null,"baseline":null,"valves":valves,
            "primary_observation":{"age_ms":elapsed,"fresh":true,"configuration_available":false}
        }))
    }).flatten()
}
