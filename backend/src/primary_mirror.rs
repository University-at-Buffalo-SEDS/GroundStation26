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
const BOARD_LEN: usize = 75;
const ORIGIN_LEN: usize = 83;
const SAFETY_LEN: usize = 87;
const PROOF_POLICY_LEN:usize=88;
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
const PHASES: [&str; 22] = [
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
    "turbocharge",
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
    board_guard: Option<(u8, u8, u8, [u16; 7])>,
    launch_revision: Option<u64>,
    safety: Option<(u8, u8, u16)>,
    board_proof_policy:Option<bool>,
    board_proof_test_fire: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LaunchOrigin {
    pub(crate) revision: u64,
    pub(crate) boot: [u8; 16],
    pub(crate) session: u32,
}
struct Received {
    owner: usize,
    observation: Observation,
    at: Instant,
    transit_ms: u64,
    launch_origin: Option<LaunchOrigin>,
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
        if let Some((active, required, proven, ages)) = self.board_guard {
            out.extend([
                if self.board_proof_policy.is_some() {
                    5
                } else if self.safety.is_some() {
                    4
                } else if self.launch_revision.is_some() {
                    3
                } else {
                    2
                },
                active,
                required,
                proven,
            ]);
            for age in ages {
                out.extend(age.to_le_bytes());
            }
            if let Some(revision) = self.launch_revision {
                out.extend(revision.to_le_bytes());
                if let Some((policy, phase, remaining)) = self.safety {
                    out.extend([policy, phase]);
                    out.extend(remaining.to_le_bytes());
                    if let Some(enforced)=self.board_proof_policy {out.push(2|u8::from(enforced)|(u8::from(self.board_proof_test_fire)<<2));}
                }
            }
        }
        out
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        if !matches!(bytes.len(), LEN | BOARD_LEN | ORIGIN_LEN | SAFETY_LEN | PROOF_POLICY_LEN)
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
        if bytes.len() != LEN
            && (bytes[57]
                != match bytes.len() {
                    PROOF_POLICY_LEN=>5,
                    SAFETY_LEN => 4,
                    ORIGIN_LEN => 3,
                    _ => 2,
                }
                || bytes[58..61].iter().any(|b| b & 0x80 != 0)
                || bytes[58] & bytes[60] != 0)
        {
            return None;
        }
        if bytes.len() >= SAFETY_LEN {
            let policy = bytes[83];
            let phase = bytes[84];
            let remaining = u16::from_le_bytes([bytes[85], bytes[86]]);
            if policy & 0xc0 != 0
                || policy & 7 & !((policy >> 3) & 7) != 0
                || phase > 6
                || match phase {
                    4 => remaining == UNKNOWN || remaining > 45,
                    5 => remaining != 0,
                    _ => remaining != UNKNOWN,
                }
            {
                return None;
            }
        }
        if bytes.len()==PROOF_POLICY_LEN && (bytes[87]&!7!=0 ||bytes[87]&5!=0&&bytes[87]&2==0) {return None;}
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
            board_guard: (bytes.len() != LEN).then(|| {
                (
                    bytes[58],
                    bytes[59],
                    bytes[60],
                    std::array::from_fn(|i| word(61 + i * 2)),
                )
            }),
            launch_revision: (bytes.len() >= ORIGIN_LEN)
                .then(|| u64::from_le_bytes(bytes[75..83].try_into().unwrap())),
            safety: (bytes.len() >= SAFETY_LEN).then(|| (bytes[83], bytes[84], word(85))),
            board_proof_policy:(bytes.len()==PROOF_POLICY_LEN&&bytes[87]&2!=0).then(||bytes[87]&1!=0),
            board_proof_test_fire: bytes.len()==PROOF_POLICY_LEN&&bytes[87]&4!=0,
        })
    }
}
/// Configuration projection only; never freshness, proof or execution authority.
pub(crate) fn configuration_policy(bytes: &[u8], mode: crate::station_mode::Mode) -> Option<crate::command_proxy::retained_config::Policy> {
    let observation = Observation::decode(bytes)?;
    let revision = observation.launch_revision.filter(|v| *v != u64::MAX)?;
    let mut policies = [None; 4];
    if mode == crate::station_mode::Mode::Hitl
        && let Some((bits, _, _)) = observation.safety
    {
        for (index, value) in policies.iter_mut().take(3).enumerate() {
            *value = (bits & (1 << (index + 3)) != 0).then_some(bits & (1 << index) != 0);
        }
    }
    policies[3] = observation.board_proof_policy;
    Some(crate::command_proxy::retained_config::Policy {settings:policies,source_revision:revision,signed_test_fire_scope:observation.board_proof_policy.map(|_|observation.board_proof_test_fire)})
}

pub fn encode_control_observation(state: &AppState, max_bytes: usize) -> Vec<u8> {
    if max_bytes < LEN || crate::rrls::is_rrls() {
        return Vec::new();
    }
    let policy = state.action_policy_snapshot();
    crate::board_abort::synchronize_stop_revision(state);
    let proof = (max_bytes >= BOARD_LEN).then(|| state.board_abort.mirror_proof());
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
        board_guard: proof.map(|(active, proven, ages, _)| {
            (
                active,
                crate::board_abort::required_mask(state),
                proven,
                ages,
            )
        }),
        launch_revision: proof
            .filter(|_| max_bytes >= ORIGIN_LEN)
            .map(|(_, _, _, revision)| revision),
        safety: (max_bytes >= SAFETY_LEN).then(|| {
            #[cfg(feature = "hitl_mode")]
            let policy = 0b111000
                | (0..3).fold(0, |mask, index| {
                    mask | u8::from(state.hitl_safety_interlock_enabled(index)) << index
                });
            #[cfg(not(feature = "hitl_mode"))]
            let policy = 0;
            let gate = crate::actuator_plumbing::retraction_gate(state);
            let phase = match gate.phase.as_str() {
                "not_configured" => 0,
                "hitl_bypassed" => 1,
                "needs_check" => 2,
                "pending" => 3,
                "countdown" => 4,
                "ready" => 5,
                _ => 6,
            };
            let remaining = match phase {
                4 => gate
                    .remaining_ms
                    .map(|ms| ms.div_ceil(1000).min(45) as u16)
                    .unwrap_or(UNKNOWN),
                5 => 0,
                _ => UNKNOWN,
            };
            (policy, phase, remaining)
        }),
        board_proof_policy:(max_bytes>=PROOF_POLICY_LEN).then(||state.board_state_proof_enforced()),
        board_proof_test_fire: cfg!(feature = "test_fire_mode"),
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
    for (a, left_sender) in SENDERS.iter().enumerate() {
        for right_sender in SENDERS.iter().skip(a + 1) {
            let ids = |sender: &str| {
                topology
                    .nodes
                    .iter()
                    .filter(move |n| n.sender_id.as_deref() == Some(sender))
                    .map(|n| n.id.as_str())
                    .collect::<Vec<_>>()
            };
            let left = ids(left_sender);
            let right = ids(right_sender);
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
#[cfg(test)]
pub fn accept_control_observation(
    state: &AppState,
    bytes: &[u8],
    roundtrip_upper_bound_ms: u64,
) -> bool {
    accept_control_observation_inner(state, bytes, roundtrip_upper_bound_ms, None)
}
pub(crate) fn accept_control_observation_with_context(
    state: &AppState,
    bytes: &[u8],
    roundtrip_upper_bound_ms: u64,
    boot: [u8; 16],
    session: u32,
) -> bool {
    accept_control_observation_inner(
        state,
        bytes,
        roundtrip_upper_bound_ms,
        Some((boot, session)),
    )
}
fn accept_control_observation_inner(
    state: &AppState,
    bytes: &[u8],
    roundtrip_upper_bound_ms: u64,
    context: Option<([u8; 16], u32)>,
) -> bool {
    let Some(observation) = Observation::decode(bytes) else {
        return false;
    };
    if let Some((active, _, proven, ages)) = observation.board_guard {
        state.board_abort.observe_authenticated_mirror(
            active,
            proven,
            ages,
            roundtrip_upper_bound_ms,
        );
    }
    let launch_origin = context
        .filter(|(boot, _)| *boot != [0; 16])
        .and_then(|(boot, session)| {
            observation
                .launch_revision
                .filter(|r| *r != u64::MAX)
                .map(|revision| LaunchOrigin {
                    revision,
                    boot,
                    session,
                })
        });
    *cache().lock().unwrap() = Some(Received {
        owner: state as *const _ as usize,
        observation,
        launch_origin,
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
pub(crate) fn legacy_observation(bytes: &[u8]) -> Vec<u8> {
    if let Some(mut observation) = Observation::decode(bytes) {
        observation.board_guard = None;
        observation.launch_revision = None;
        observation.safety = None;
        observation.board_proof_policy=None;
        observation.encode()
    } else {
        bytes.to_vec()
    }
}
pub(crate) fn observation_has_board_guard(bytes: &[u8]) -> bool {
    Observation::decode(bytes).is_some_and(|observation| observation.board_guard.is_some()
        &&observation.launch_revision.is_some()&&observation.board_proof_policy.is_some())
}
/// Current authenticated primary policy only. Unknown/legacy/stale must never
/// be replaced with this RRLS executable's locally compiled policy.
pub(crate) fn board_state_proof_policy(state: &AppState) -> Option<bool> {
    let cache = cache().lock().unwrap();
    let received = cache.as_ref()?;
    if received.owner != state as *const _ as usize || received.launch_origin.is_none() {
        return None;
    }
    let elapsed = received.transit_ms.saturating_add(
        received.at.elapsed().as_millis().min(u64::MAX as u128) as u64);
    (elapsed <= SOURCE_FRESH_MS).then_some(received.observation.board_proof_policy).flatten()
}
/// Signed primary compile scope; the Mode::Hitl label alone does not reveal
/// whether that primary is a TestFire build. Zero means explicitly optional.
pub(crate) fn board_history_required_scope(state: &AppState) -> Option<u8> {
    let cache = cache().lock().unwrap();
    let received = cache.as_ref()?;
    if received.owner != state as *const _ as usize || received.launch_origin.is_none() {
        return None;
    }
    let elapsed = received.transit_ms.saturating_add(
        received.at.elapsed().as_millis().min(u64::MAX as u128) as u64);
    if elapsed > SOURCE_FRESH_MS { return None; }
    Some(match received.observation.board_proof_policy? {
        false => 0,
        true if received.observation.board_proof_test_fire => 0x78,
        true => 0x7f,
    })
}
/// Ordinary readiness is the authenticated primary's projection. A normal
/// RRLS executable must not add its local flight requirements to a HITL or
/// TestFire primary's signed policy. Crash-history scope remains separate.
pub(crate) fn board_required_state_mask(state: &AppState) -> Option<u8> {
    let guard = cache().lock().unwrap();
    let received = guard.as_ref()?;
    if received.owner != state as *const _ as usize || received.launch_origin.is_none() {
        return None;
    }
    let elapsed = received.transit_ms.saturating_add(
        received.at.elapsed().as_millis().min(u64::MAX as u128) as u64,
    );
    if elapsed > SOURCE_FRESH_MS {
        return None;
    }
    let enforced = received.observation.board_proof_policy?;
    let (_, required, _, _) = received.observation.board_guard?;
    if !enforced && required != 0 {
        return None;
    }
    Some(required)
}
pub(crate) fn capture_launch_origin(state: &AppState) -> Option<LaunchOrigin> {
    board_state_proof_policy(state)?;
    board_history_required_scope(state)?;
    // Copy immutable authenticated context, then release CACHE before touching
    // the board register. Board guards may sample CACHE while holding that
    // register, so the opposing nesting would deadlock an RRLS Launch.
    let (origin,observation,at,transit_ms) = {
        let cache=cache().lock().unwrap();
        let received=cache.as_ref()?;
        if received.owner != state as *const _ as usize { return None; }
        let origin=received.launch_origin?;
        received.observation.board_proof_policy?;
        let elapsed=received.transit_ms.saturating_add(received.at.elapsed().as_millis().min(u64::MAX as u128) as u64);
        let (active,required,proven,ages)=received.observation.board_guard?;
        if elapsed>SOURCE_FRESH_MS || active!=0 || required & !proven != 0
            || !(0..7).all(|i| required & (1<<i)==0 || aged(ages[i],elapsed).is_some_and(|age| age<=2000)) {
            return None;
        }
        (origin,received.observation.clone(),received.at,received.transit_ms)
    };
    let local_revision=state.board_abort.abort_revision();
    if crate::board_abort::blocked_message(state).is_some() { return None; }
    // Recheck the SAME authenticated source/boot/session/policy context after
    // the board lookup. An intervening cache replacement is not a new ticket.
    {
        let cache=cache().lock().unwrap();
        let current=cache.as_ref()?;
        if current.owner != state as *const _ as usize || current.at!=at
            || current.transit_ms!=transit_ms || current.launch_origin!=Some(origin)
            || current.observation!=observation
            || transit_ms.saturating_add(at.elapsed().as_millis().min(u64::MAX as u128) as u64)>SOURCE_FRESH_MS {
            return None;
        }
    }
    (state.board_abort.abort_revision()==local_revision).then_some(origin)
}

pub(crate) fn launch_provenance_current(state: &AppState) -> bool {
    capture_launch_origin(state).is_some()
}
/// Signed presentation only. Never authorizes standalone RRLS Launch.
pub(crate) fn retraction_presentation(
    state: &AppState,
) -> Option<crate::actuator_plumbing::RetractionGate> {
    with_received(state,|r,elapsed| {
        if elapsed>SOURCE_FRESH_MS{return None;}
        let (_,phase,remaining)=r.safety?;
        let (name,message)=match phase {
            0=>("not_configured","Primary: fill plumbing is not configured; retraction delay is inactive."),
            1=>("hitl_bypassed","Primary HITL minimum-delay interlock is disabled."),
            2=>("needs_check","Primary requires a fresh explicit readiness check before Launch."),
            3=>("pending","Primary has a pending plumbing owner or setting; Launch is inhibited."),
            4=>("countdown","Primary retraction minimum interval is running; fresh completion is still required."),
            5=>("ready","Primary reports retraction timing checks passed; other Launch interlocks still apply."),
            _=>("fault","Primary plumbing status needs explicit authorized attention; Launch is inhibited."),
        };
        Some(crate::actuator_plumbing::RetractionGate {configured:phase!=0,enforced:!matches!(phase,0|1),
            allowed:matches!(phase,0|1|5),phase:name.into(),remaining_ms:(remaining!=UNKNOWN).then_some(u32::from(remaining)*1000),message:message.into()})
    }).flatten()
}
/// Presentation policy only. The primary still performs every permission/interlock check.
pub fn action_policy(state: &AppState) -> Option<ActionPolicyMsg> {
    let launch_provenance = launch_provenance_current(state);
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
                    let actuated = match *name {
                        "ToggleBoardStateProofInterlock"=>r.board_proof_policy.filter(|_|fresh),
                        "TogglePlumbingDelayInterlock"
                        | "ToggleChecklistInterlock"
                        | "ToggleSupplyInterlock" => {
                            let index = match *name {
                                "TogglePlumbingDelayInterlock" => 0,
                                "ToggleChecklistInterlock" => 1,
                                _ => 2,
                            };
                            r.safety
                                .filter(|(bits, _, _)| fresh && bits & (1 << (index + 3)) != 0)
                                .map(|(bits, _, _)| bits & (1 << index) != 0)
                        }
                        "NitrogenClose" => valve_observation(r, 3, elapsed).map(|open| !open),
                        "NitrousClose" => valve_observation(r, 4, elapsed).map(|open| !open),
                        _ => VALVES
                            .iter()
                            .position(|v| v == name)
                            .and_then(|index| valve_observation(r, index, elapsed)),
                    };
                    ActionControl {
                        cmd: (*name).into(),
                        enabled: fresh
                            && (*name!="ToggleBoardStateProofInterlock"||r.board_proof_policy.is_some())
                            && r.enabled & (1 << i) != 0
                            && (!crate::board_abort::launch_command(name)
                                || launch_provenance
                                    && r.board_guard.is_some_and(
                                        |(active, required, proven, ages)| {
                                            active == 0
                                                && required & !proven == 0
                                                && (0..7).all(|i| {
                                                    required & (1 << i) == 0
                                                        || aged(ages[i], elapsed)
                                                            .is_some_and(|age| age <= 2000)
                                                })
                                        },
                                    )),
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
}pub fn project_board_status(
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
            board_guard: None,
            launch_revision: None,
            safety: None,
            board_proof_policy:None,
            board_proof_test_fire:false,
        }
    }
    #[test]
    fn bounded_roundtrip_and_malformed_rejection() {
        let value = example();
        let bytes = value.encode();
        assert_eq!(bytes.len(), LEN);
        const { assert!(LEN <= 96); }
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
    #[test]
    fn board_abort_signed_safety87_preserves_legacy_prefix_and_rejects_unknown_policy_or_timer() {
        let old = example();
        let legacy = old.encode();
        let mut value = old.clone();
        value.board_guard = Some((0, 0, 0, [UNKNOWN; 7]));
        value.launch_revision = Some(8);
        value.safety = Some((0b111101, 4, 45));
        let bytes = value.encode();
        assert_eq!(bytes.len(), 87);
        assert_eq!(&bytes[..57], &legacy);
        assert_eq!(Observation::decode(&bytes), Some(value.clone()));
        assert_eq!(legacy_observation(&bytes), legacy);
        let mut old83 = value.clone();
        old83.safety = None;
        let old83 = old83.encode();
        assert_eq!(old83.len(), 83);
        assert!(Observation::decode(&old83).unwrap().safety.is_none());
        let mut old75 = value.clone();
        old75.safety = None;
        old75.launch_revision = None;
        let old75 = old75.encode();
        assert_eq!(old75.len(), 75);
        assert!(
            Observation::decode(&old75)
                .unwrap()
                .launch_revision
                .is_none()
        );
        for length in [58, 74, 76, 82, 84, 85, 86] {
            assert!(Observation::decode(&bytes[..length]).is_none());
        }
        for (at, byte) in [(57, 3), (83, 0x80), (83, 1), (84, 7), (85, 46)] {
            let mut invalid = bytes.clone();
            invalid[at] = byte;
            assert!(
                Observation::decode(&invalid).is_none(),
                "offset{at} value{byte}"
            );
        }
        let mut invalid = bytes.clone();
        invalid[85..87].copy_from_slice(&UNKNOWN.to_le_bytes());
        assert!(Observation::decode(&invalid).is_none());
        let mut invalid = bytes.clone();
        invalid[84] = 5;
        invalid[85] = 1;
        invalid[86] = 0;
        assert!(Observation::decode(&invalid).is_none());
        let mut invalid = bytes;
        invalid.push(0);
        assert!(Observation::decode(&invalid).is_none());
    }
    #[test]
    fn board_abort_proof_policy88_preserves_legacy_and_rejects_unknown_authority() {
        let mut value = example();
        value.board_guard = Some((0, 0, 0, [UNKNOWN; 7]));
        value.launch_revision = Some(8);
        value.safety = Some((0b111101, 4, 45));
        let old87 = value.encode();
        assert_eq!(old87.len(), 87);
        assert_eq!(Observation::decode(&old87).unwrap().board_proof_policy, None);
        for enabled in [false, true] {
            value.board_proof_policy = Some(enabled);
            let bytes = value.encode();
            assert_eq!(bytes.len(), 88);
            assert_eq!(&bytes[..57], &old87[..57]);
            assert_eq!(bytes[57], 5);
            assert_eq!(bytes[87], 2 | u8::from(enabled));
            assert_eq!(Observation::decode(&bytes), Some(value.clone()));
            assert_eq!(legacy_observation(&bytes), example().encode());
            for test_fire in [false, true] {
                let mut scoped = value.clone();
                scoped.board_proof_test_fire = test_fire;
                let scoped_bytes = scoped.encode();
                assert_eq!(scoped_bytes[87], 2 | u8::from(enabled) | (u8::from(test_fire) << 2));
                assert_eq!(Observation::decode(&scoped_bytes), Some(scoped));
            }
            for bad in [1, 4, 5, 8, 128, 255] {
                let mut invalid = bytes.clone();
                invalid[87] = bad;
                assert!(Observation::decode(&invalid).is_none());
            }
            let mut unknown = bytes.clone();
            unknown[87] = 0;
            assert_eq!(Observation::decode(&unknown).unwrap().board_proof_policy, None);
            let mut too_long = bytes;
            too_long.push(0);
            assert!(Observation::decode(&too_long).is_none());
        }
    }
    #[tokio::test]
    async fn board_abort_signed_history_scope_table_and_context_revocation() {
        let state = crate::state::tests::test_app_state().await;
        let mut value = example();
        value.board_guard = Some((0, 0, 0, [UNKNOWN; 7]));
        value.launch_revision = Some(8);
        value.safety = Some((0b111101, 4, 45));
        for (enforced, test_fire, expected) in [(true,false,0x7f),(true,true,0x78),(false,false,0),(false,true,0)] {
            value.board_proof_policy = Some(enforced);
            value.board_proof_test_fire = test_fire;
            assert!(accept_control_observation_with_context(&state, &value.encode(), 0, [9;16], 23));
            assert_eq!(board_state_proof_policy(&state), Some(enforced));
            assert_eq!(board_history_required_scope(&state), Some(expected));
            assert!(accept_control_observation_with_context(&state, &value.encode(), SOURCE_FRESH_MS+1, [9;16], 23));
            assert_eq!(board_history_required_scope(&state), None);
            assert!(accept_control_observation_with_context(&state, &value.encode(), 0, [0;16], 23));
            assert_eq!(board_history_required_scope(&state), None);
        }
        value.board_proof_policy = None;
        value.board_proof_test_fire = false;
        assert_eq!(value.encode().len(), 87);
        assert!(accept_control_observation_with_context(&state, &value.encode(), 0, [9;16], 23));
        assert_eq!(board_history_required_scope(&state), None);
        invalidate_control_observation();
        assert_eq!(board_history_required_scope(&state), None);
    }
    #[tokio::test]
    async fn board_abort_versioned_mirror_preserves_old_bytes_and_requires_later_actual_proof() {
        let state = crate::state::tests::test_app_state().await;
        let original = example();
        let legacy = original.encode();
        assert_eq!(legacy.len(), 57);
        assert!(!launch_provenance_current(&state));
        assert!(accept_control_observation(&state, &legacy, 0));
        assert!(
            !launch_provenance_current(&state),
            "old57 is unknown, not a non-Abort proof"
        );
        let mut extended = original.clone();
        extended.board_guard = Some((1 << 3, 1 << 3, 0, [UNKNOWN; 7]));
        let wire = extended.encode();
        assert_eq!(wire.len(), 75);
        assert_eq!(&wire[..57], legacy.as_slice());
        assert_eq!(Observation::decode(&wire), Some(extended.clone()));
        let mut bad = wire.clone();
        bad[58] |= 128;
        assert!(Observation::decode(&bad).is_none());
        let mut bad = wire.clone();
        bad[60] |= 1 << 3;
        assert!(
            Observation::decode(&bad).is_none(),
            "Abort and proven non-Abort cannot overlap"
        );
        assert!(accept_control_observation(&state, &wire, 0));
        assert!(crate::board_abort::blocked_message(&state).is_some());
        // An authenticated primary clear can retire a mirror-only condition.
        extended.board_guard = Some((0, 1 << 3, 1 << 3, [0; 7]));
        assert!(accept_control_observation(&state, &extended.encode(), 0));
        assert!(crate::board_abort::blocked_message(&state).is_none());
        // A separately observed local Abort needs actual later query provenance.
        state
            .board_abort
            .observe_abort(Board::ActuatorBoard, Instant::now(), 100);
        assert!(accept_control_observation(&state, &extended.encode(), 2001));
        assert!(crate::board_abort::blocked_message(&state).is_some());
        assert!(!launch_provenance_current(&state));
        assert!(accept_control_observation(&state, &legacy, 0));
        assert!(crate::board_abort::blocked_message(&state).is_some());
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert!(accept_control_observation(&state, &extended.encode(), 0));
        assert!(crate::board_abort::blocked_message(&state).is_none());
        assert!(
            !launch_provenance_current(&state),
            "presentation-only v2 lacks primary boot/session/revision authority"
        );
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
