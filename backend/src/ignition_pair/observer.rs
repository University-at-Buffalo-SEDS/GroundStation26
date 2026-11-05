//! Fixed-size local observations. Command and Stop paths never wait for SDK IO.
use crate::{gse, state::AppState};
use sedsnet::router::Router;
use std::{
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
const REFRESH: Duration = Duration::from_millis(50);
const MAX_AGE: Duration = Duration::from_millis(100);
const MAX_SIDES: usize = 64;
static WORKER_ALIVE: AtomicBool = AtomicBool::new(false);
#[derive(Clone)]
struct ConnectionToken {
    source: Arc<AtomicU64>,
    generation: u64,
}
#[derive(Clone)]
pub(crate) struct Token {
    owner: usize,
    router: usize,
    generation: u64,
    epoch: u64,
    revision: u64,
    requested: Instant,
    sides: [Option<usize>; 2],
    side_versions: [u64; 2],
    connections: [Option<ConnectionToken>; 2],
}
pub(crate) struct Observer {
    #[cfg(test)]
    pub(crate) launch_sdk_offers: AtomicU64,
    cache: Mutex<Option<Token>>,
    epoch: AtomicU64,
    revision: AtomicU64,
    side_versions: [AtomicU64; MAX_SIDES],
    unsupported_faults: AtomicU64,
    connection_mask: AtomicU64,
    worker_dead: AtomicU64,
    connections: Mutex<[Option<Arc<AtomicU64>>; MAX_SIDES]>,
    exhausted: AtomicBool,
    in_flight: AtomicBool,
    started: AtomicBool,
}
impl Default for Observer {
    fn default() -> Self {
        Self {
            #[cfg(test)]
            launch_sdk_offers: AtomicU64::new(0),
            cache: Mutex::new(None),
            epoch: AtomicU64::new(0),
            revision: AtomicU64::new(0),
            side_versions: std::array::from_fn(|_| AtomicU64::new(0)),
            unsupported_faults: AtomicU64::new(0),
            connection_mask: AtomicU64::new(0),
            worker_dead: AtomicU64::new(0),
            connections: Mutex::new(std::array::from_fn(|_| None)),
            exhausted: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
            started: AtomicBool::new(false),
        }
    }
}
struct Job {
    owner: Weak<Observer>,
    router: Arc<Router>,
    generation: u64,
    epoch: u64,
    requested: Instant,
    side_versions: [u64; MAX_SIDES],
    connections: [Option<ConnectionToken>; MAX_SIDES],
}
fn young(at: Instant) -> bool {
    Instant::now()
        .checked_duration_since(at)
        .is_some_and(|age| age <= MAX_AGE)
}
fn side_bit(side: usize) -> Option<u64> {
    u32::try_from(side).ok().and_then(|n| 1u64.checked_shl(n))
}
impl Observer {
    pub(crate) fn invalidate(&self) {
        if self
            .epoch
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .is_err()
        {
            self.exhausted.store(true, Ordering::Release);
        }
    }
    /// Startup registration only. Final guards retain captured atomic sources,
    /// never a provider callback or a mutex protecting a transport object.
    pub(crate) fn register_connection(&self, side: usize, source: Option<Arc<AtomicU64>>) {
        let Some(slot) = self.side_versions.get(side) else {
            self.exhausted.store(true, Ordering::Release);
            return;
        };
        let Ok(mut connections) = self.connections.lock() else {
            self.exhausted.store(true, Ordering::Release);
            return;
        };
        let same = match (&connections[side], &source) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            if slot
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
                .is_err()
            {
                self.exhausted.store(true, Ordering::Release);
            }
            connections[side] = source;
        }
        let bit = 1u64 << side;
        if connections[side].is_some() {
            self.connection_mask.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.connection_mask.fetch_and(!bit, Ordering::AcqRel);
        }
        self.worker_dead.fetch_and(!bit, Ordering::AcqRel);
    }
    pub(crate) fn io_error(&self, side: usize) {
        let Some(slot) = self.side_versions.get(side) else {
            self.exhausted.store(true, Ordering::Release);
            return;
        };
        if slot
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .is_err()
        {
            self.exhausted.store(true, Ordering::Release);
        }
    }
    /// Only transports with no authoritative connection generation need a
    /// conservative hard-failure latch. Ordinary parser/queue errors never call this.
    pub(crate) fn unsupported_hard_fault(&self, side: usize) {
        self.io_error(side);
        if let Some(bit) = side_bit(side) {
            if self.connection_mask.load(Ordering::Acquire) & bit == 0 {
                self.unsupported_faults.fetch_or(bit, Ordering::AcqRel);
            }
        } else {
            self.exhausted.store(true, Ordering::Release);
        }
    }
    pub(crate) fn valid(&self, token: &Token, router: &Router, generation: u64) -> bool {
        WORKER_ALIVE.load(Ordering::Acquire)
            && !self.exhausted.load(Ordering::Acquire)
            && token.owner == self as *const Self as usize
            && token.router == router as *const Router as usize
            && token.generation == generation
            && token.epoch == self.epoch.load(Ordering::Acquire)
            && token.revision == self.revision.load(Ordering::Acquire)
            && young(token.requested)
            && token.sides.iter().enumerate().all(|(i, side)| {
                side.is_some_and(|side| {
                    self.side_versions.get(side).is_some_and(|version| {
                        version.load(Ordering::Acquire) == token.side_versions[i]
                    }) && side_bit(side).is_some_and(|bit| {
                        (self.unsupported_faults.load(Ordering::Acquire)
                            | self.worker_dead.load(Ordering::Acquire))
                            & bit
                            == 0
                    }) && token.connections[i].as_ref().is_none_or(|connection| {
                        connection.generation != 0
                            && connection.source.load(Ordering::Acquire) == connection.generation
                    })
                })
            })
    }
    pub(crate) fn read(&self, router: &Router, generation: u64) -> Result<Token, &'static str> {
        let token = self
            .cache
            .try_lock()
            .map_err(|_| "Ignition route observation owner busy")?
            .as_ref()
            .cloned()
            .ok_or("Ignition route observation pending")?;
        if self.valid(&token, router, generation) {
            Ok(token)
        } else {
            Err("Ignition requires a fresh valid Actuator and Valve Board route observation")
        }
    }
    fn request(self: &Arc<Self>, router: Arc<Router>, generation: u64) {
        if self.exhausted.load(Ordering::Acquire)
            || self
                .in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        // Capture local ownership before enqueue and every SDK observation.
        // An old route must never acquire a newer reconnect generation.
        let requested = Instant::now();
        let epoch = self.epoch.load(Ordering::Acquire);
        let (side_versions, connections) = {
            let Ok(connections) = self.connections.try_lock() else {
                self.invalidate();
                self.in_flight.store(false, Ordering::Release);
                return;
            };
            (
                std::array::from_fn(|index| self.side_versions[index].load(Ordering::Acquire)),
                std::array::from_fn(|index| {
                    connections[index].as_ref().map(|source| ConnectionToken {
                        source: source.clone(),
                        generation: source.load(Ordering::Acquire),
                    })
                }),
            )
        };
        let job = Job {
            owner: Arc::downgrade(self),
            router,
            generation,
            epoch,
            requested,
            side_versions,
            connections,
        };
        if dispatcher()
            .as_ref()
            .is_none_or(|sender| sender.try_send(job).is_err())
        {
            self.invalidate();
            self.in_flight.store(false, Ordering::Release);
        }
    }
}
// Exactly one OS worker and one queued job globally; no thread per command.
fn dispatcher() -> &'static Option<mpsc::SyncSender<Job>> {
    static DISPATCH: OnceLock<Option<mpsc::SyncSender<Job>>> = OnceLock::new();
    DISPATCH.get_or_init(|| {
        let (send, receive) = mpsc::sync_channel::<Job>(1);
        std::thread::Builder::new()
            .name("ignition-route-observer".into())
            .spawn(move || {
                struct Alive;
                impl Drop for Alive {
                    fn drop(&mut self) {
                        WORKER_ALIVE.store(false, Ordering::Release);
                    }
                }
                let _alive = Alive;
                WORKER_ALIVE.store(true, Ordering::Release);
                while let Ok(job) = receive.recv() {
                    let Some(owner) = job.owner.upgrade() else {
                        continue;
                    };
                    if young(job.requested) && job.epoch == owner.epoch.load(Ordering::Acquire) {
                        let topology = job.router.export_topology();
                        #[cfg(test)] test_pause::after_topology(&owner);
                        let runtime = job.router.export_runtime_stats();
                        {
                            let mut sides = [None; 2];
                            let mut side_versions = [0; 2];
                            let mut connection_tokens: [Option<ConnectionToken>; 2] = [None, None];
                            for (index, name) in
                                ["ACTUATOR_BOARD", "VALVE_BOARD"].into_iter().enumerate()
                            {
                                let endpoint = crate::telemetry_schema::endpoint(name);
                                if let Some(route) = topology.routes.iter().find(|route| {
                                    route.age_ms <= super::ROUTE_MAX_AGE_MS
                                        && route.reachable_endpoints.contains(&endpoint)
                                        && side_bit(route.side_id).is_some_and(|bit| {
                                            (owner.unsupported_faults.load(Ordering::Acquire)
                                                | owner.worker_dead.load(Ordering::Acquire))
                                                & bit
                                                == 0
                                        })
                                        && job.connections.get(route.side_id).is_some_and(
                                            |source| {
                                                owner.side_versions[route.side_id]
                                                    .load(Ordering::Acquire)
                                                    == job.side_versions[route.side_id]
                                                    && source.as_ref().is_none_or(|connection| {
                                                        connection.generation != 0
                                                            && connection
                                                                .source
                                                                .load(Ordering::Acquire)
                                                                == connection.generation
                                                    })
                                            },
                                        )
                                        && runtime.sides.iter().any(|side| {
                                            side.side_id == route.side_id
                                                && side.ingress_enabled
                                                && side.egress_enabled
                                        })
                                }) {
                                    sides[index] = Some(route.side_id);
                                    side_versions[index] = job.side_versions[route.side_id];
                                    connection_tokens[index] =
                                        job.connections[route.side_id].clone();
                                }
                            }
                            let unchanged = sides.iter().enumerate().all(|(index, side)| {
                                side.is_none_or(|side| {
                                    owner.side_versions[side].load(Ordering::Acquire)
                                        == side_versions[index]
                                        && connection_tokens[index].as_ref().is_none_or(
                                            |connection| {
                                                connection.generation != 0
                                                    && connection.source.load(Ordering::Acquire)
                                                        == connection.generation
                                            },
                                        )
                                })
                            });
                            if unchanged
                                && young(job.requested)
                                && job.epoch == owner.epoch.load(Ordering::Acquire)
                                && let Ok(mut cache) = owner.cache.try_lock()
                            {
                                if let Ok(previous) = owner.revision.try_update(
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                    |n| n.checked_add(1),
                                ) {
                                    *cache = Some(Token {
                                        owner: Arc::as_ptr(&owner) as usize,
                                        router: Arc::as_ptr(&job.router) as usize,
                                        generation: job.generation,
                                        epoch: job.epoch,
                                        revision: previous + 1,
                                        requested: job.requested,
                                        sides,
                                        side_versions,
                                        connections: connection_tokens,
                                    });
                                } else {
                                    owner.exhausted.store(true, Ordering::Release);
                                }
                            }
                        }
                    }
                    owner.in_flight.store(false, Ordering::Release);
                }
            })
            .ok()
            .map(|_| send)
    })
}
pub(crate) fn start(state: &Arc<AppState>) {
    if state
        .ignition_observer
        .started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(REFRESH);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(state) = weak.upgrade() else {
                break;
            };
            if state.shutdown_requested.load(Ordering::Acquire) {
                break;
            }
            if let Some(router) = state.topology_router.get() {
                state
                    .ignition_observer
                    .request(router.clone(), gse::automatic_generation(&state));
            }
        }
    });
}

/// Existing transport errors retire cached local observations. Only an actual
/// adapter connection generation can restore a default transport; no generic
/// successful read, buffered packet or unrelated board proof can do so.
pub(crate) struct SideWatcher {
    owner: Arc<Observer>,
    side: usize,
}
impl SideWatcher {
    pub(crate) fn new(state: &AppState, side: usize) -> Self {
        Self {
            owner: state.ignition_observer.clone(),
            side,
        }
    }
    pub(crate) fn receive_error(&self, error: &sedsnet::TelemetryError) {
        if matches!(error, sedsnet::TelemetryError::GenericError(_)) {
            self.owner.unsupported_hard_fault(self.side);
        } else {
            self.owner.io_error(self.side);
        }
    }
    pub(crate) fn send_error(&self, error: &(dyn std::error::Error + Send + Sync + 'static)) {
        let hard = crate::rrls_radio::boxed_io_failure(error);
        if hard {
            self.owner.unsupported_hard_fault(self.side);
        } else {
            self.owner.io_error(self.side);
        }
    }
}
impl Drop for SideWatcher {
    fn drop(&mut self) {
        if let Some(bit) = side_bit(self.side) {
            self.owner.worker_dead.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.owner.exhausted.store(true, Ordering::Release);
        }
        self.owner.io_error(self.side);
    }
}

#[cfg(test)]
#[path="observer_test_pause.rs"]
mod test_pause;
#[cfg(test)]
#[path="observer_tests.rs"]
mod tests;

// Test-only readback of a completed real SDK observation, never an owner mutation.
#[cfg(test)]
pub(crate) fn observed_after(state: &AppState, after: Instant, ready: bool) -> bool {
    let Ok(cache) = state.ignition_observer.cache.try_lock() else { return false; };
    cache.as_ref().is_some_and(|token| token.requested >= after && young(token.requested)
        && token.revision == state.ignition_observer.revision.load(Ordering::Acquire)
        && token.sides.iter().all(Option::is_some) == ready)
}
