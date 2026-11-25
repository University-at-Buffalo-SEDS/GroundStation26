//! Overpressure protection on a thread of its own. Its samples come straight
//! from the router's receive handler: the telemetry worker that feeds the
//! runtime waits for the GSE lock and for the recorder, and both wait for the
//! disk.
use super::*;
use sedsnet::packet::Packet;
use std::collections::VecDeque;
use std::sync::{
    Condvar,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Readings not judged yet. A valve board sends 50 a second.
const MAX_WAITING: usize = 256;
/// How often the main-valve window is checked between readings.
const PERIOD: Duration = Duration::from_millis(20);

#[derive(Default)]
pub(super) struct Watch {
    running: AtomicBool,
    inbox: Mutex<Inbox>,
    received: Condvar,
}
#[derive(Default)]
struct Inbox {
    packets: VecDeque<Packet>,
    /// A reading taken from `packets` while a Launch was being saved.
    held: Option<Sample>,
    last_source: Option<Stamp>,
}
impl Watch {
    /// The next reading, as `observe_valve_pressure_ingress` would admit it.
    pub(super) fn next(&self, state: &AppState, published: &Published) -> Option<Sample> {
        loop {
            let packet = {
                let mut inbox = leaf(&self.inbox);
                if let Some(sample) = inbox.held.take() {
                    return Some(sample);
                }
                inbox.packets.pop_front()?
            };
            // Resolving the sender and the network time waits for the SDK only.
            if state.board_from_network_sender(packet.sender()) != Some(crate::types::Board::ValveBoard) {
                continue;
            }
            let Ok(source_ms) = i64::try_from(packet.timestamp()) else { continue };
            let Some(value) = crate::telemetry_task::valve_pressure_psi(&packet) else { continue };
            let receipt = ValveReceipt {
                network_ms: crate::network_stamp::receipt_ms_for_packet(
                    &packet,
                    crate::telemetry_task::displayed_network_utc_ms(),
                ),
                source_epoch: packet.time_epoch(),
                monotonic_at: Instant::now(),
                simulation_ms: crate::simulation_clock_ms(),
            };
            if receipt.network_ms < 0 {
                continue;
            }
            let mut inbox = leaf(&self.inbox);
            let source = Stamp::new(receipt.source_epoch, source_ms);
            if inbox.last_source.is_some_and(|last| {
                source < last && (source.ms < crate::network_stamp::UTC_2020_MS || last.ms < crate::network_stamp::UTC_2020_MS)
            }) {
                // A backwards capture is never fresh evidence.
                inbox.last_source = None;
                continue;
            }
            if let Some(at_ms) = scalar_capture_at(
                published.clock,
                published.now(),
                Some(source_ms),
                receipt,
                &mut inbox.last_source,
            ) {
                return Some(Sample { value, at_ms });
            }
        }
    }
    pub(super) fn put_back(&self, sample: Sample) {
        leaf(&self.inbox).held = Some(sample);
    }
}

/// A packet as the router's receive handler hands it over. RAM only.
pub(crate) fn receive(state: &AppState, packet: &Packet) {
    let watch = &state.gse.overpressure.0.watch;
    if !crate::telemetry_schema::is_ox_tank_pressure(packet.data_type()) || !watch.running.load(Ordering::Acquire) {
        return;
    }
    let mut inbox = leaf(&watch.inbox);
    if inbox.packets.len() >= MAX_WAITING {
        inbox.packets.pop_front();
    }
    inbox.packets.push_back(packet.clone());
    drop(inbox);
    watch.received.notify_one();
}

/// The thread, until the station shuts down.
pub(crate) fn spawn(state: Arc<AppState>) -> std::io::Result<std::thread::JoinHandle<()>> {
    // The runtime and the operator are told by a thread that may wait.
    let (unsettled, settling) = std::sync::mpsc::channel::<Sent>();
    let settled = state.clone();
    crate::runtime_mode::spawn_thread("overpressure_record", move || {
        for sent in settling {
            sent.settle(&settled);
        }
    })?;
    state.gse.overpressure.0.watch.running.store(true, Ordering::Release);
    crate::runtime_mode::spawn_thread("overpressure", move || {
        let protection = state.gse.overpressure.clone();
        let watch = &protection.0.watch;
        while !state.shutdown_requested.load(Ordering::Acquire) {
            evaluate_without_waiting(&state, |sent| {
                let _ = unsettled.send(sent);
            });
            let inbox = leaf(&watch.inbox);
            if inbox.packets.is_empty() {
                let _ = watch.received.wait_timeout(inbox, PERIOD);
            }
        }
    })
}
