//! Genuine same-Router constrained-queue fixture. Warmup is explicit SDK setup;
//! queue processing ends before readonly pressure and original command offers.
use crate::{state::AppState, types::Board};
use sedsnet::{
    packet::Packet,
    router::{EndpointHandler, Router, RouterConfig, RouterSideOptions},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
struct Peer {
    router: Router,
    host_side: usize,
    side: usize,
    incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
    outgoing: Arc<Mutex<VecDeque<Vec<u8>>>>,
    requests: Arc<Mutex<VecDeque<Packet>>>,
    board: Board,
}
pub(crate) struct Scene {
    pub(crate) state: Arc<AppState>,
    pub(crate) host: Arc<Router>,
    peers: Vec<Peer>,
}
impl Drop for Scene {
    fn drop(&mut self) {
        self.state.request_shutdown();
    }
}
fn clocked(config: RouterConfig) -> Router {
    let host = Router::new(config);
    crate::telemetry_task::refresh_host_network_time(
        &host,
        crate::telemetry_task::get_current_timestamp_ms(),
    );
    host
}
fn memory(budget: usize) -> sedsnet::config::RuntimeMemoryConfig {
    sedsnet::config::RuntimeMemoryConfig::new(budget, 1, budget.min(128), 1.25).unwrap()
}
impl Scene {
    pub(crate) async fn new(budget: usize) -> Self {
        let state = crate::state::tests::test_app_state().await;
        let host = Arc::new(clocked(
            RouterConfig::new([super::fixture::handler(&state, |_| Ok(()))])
                .with_sender("GS")
                .with_memory_config(memory(budget))
                .unwrap(),
        ));
        state.topology_router.set(host.clone()).unwrap();
        Self {
            state,
            host,
            peers: Vec::new(),
        }
    }
    fn pump(&mut self) -> Result<(), String> {
        for _ in 0..20 {
            self.host
                .process_all_queues_with_timeout(0)
                .map_err(|e| format!("host process:{e:?}"))?;
            for peer in &mut self.peers {
                while let Some(wire) = peer.incoming.lock().unwrap().pop_front() {
                    peer.router
                        .rx_packed_from_side(&wire, peer.side)
                        .map_err(|e| format!("peer ingress:{e:?}"))?;
                }
                peer.router
                    .process_all_queues_with_timeout(0)
                    .map_err(|e| format!("peer process:{e:?}"))?;
                while let Some(packet) = peer.requests.lock().unwrap().pop_front() {
                    peer.router
                        .tx_queue(
                            super::fixture::query_reply(peer.board, &packet)
                                .map_err(|e| format!("reply encode:{e:?}"))?,
                        )
                        .map_err(|e| format!("peer reply queue:{e:?}"))?;
                }
                peer.router
                    .process_all_queues_with_timeout(0)
                    .map_err(|e| format!("peer reply process:{e:?}"))?;
                while let Some(wire) = peer.outgoing.lock().unwrap().pop_front() {
                    self.host
                        .rx_packed_from_side(&wire, peer.host_side)
                        .map_err(|e| format!("host ingress:{e:?}"))?;
                }
            }
        }
        Ok(())
    }
    pub(crate) async fn warm(&mut self) -> Result<(), String> {
        for board in crate::board_abort::BOARDS {
            let recovery = crate::telemetry_schema::endpoint(match board {
                Board::ActuatorBoard => "BOARD_RECOVERY_AB",
                Board::ValveBoard => "BOARD_RECOVERY_VB",
                Board::FlightComputer => "BOARD_RECOVERY_FC",
                Board::RFBoard => "BOARD_RECOVERY_RF",
                Board::PowerBoard => "BOARD_RECOVERY_PB",
                Board::GatewayBoard => "BOARD_RECOVERY_GB",
                Board::DaqBoard => "BOARD_RECOVERY_DAQ",
                _ => unreachable!(),
            });
            let command = match board {
                Board::ActuatorBoard => Some(crate::telemetry_schema::endpoints::ACTUATOR_BOARD),
                Board::ValveBoard => Some(crate::telemetry_schema::endpoints::VALVE_BOARD),
                Board::FlightComputer => {
                    Some(crate::telemetry_schema::endpoints::FLIGHT_CONTROLLER)
                }
                _ => None,
            };
            let requests = Arc::new(Mutex::new(VecDeque::<Packet>::new()));
            let capture = requests.clone();
            let mut handlers = vec![EndpointHandler::new_packet_handler(
                recovery,
                move |packet| {
                    capture.lock().unwrap().push_back(packet.clone());
                    Ok(())
                },
            )];
            if let Some(command) = command {
                handlers.push(EndpointHandler::new_packet_handler(command, |_| Ok(())));
            }
            let router = Router::new(RouterConfig::new(handlers).with_sender(board.sender_id()));
            let utc = self.host.network_time_ms().unwrap();
            router
                .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                    utc,
                ))
                .map_err(|e| format!("peer clock:{e:?}"))?;
            let incoming = Arc::new(Mutex::new(VecDeque::new()));
            let capture = incoming.clone();
            let hs = self.host.add_side_packed_with_options(
                board.sender_id(),
                move |wire| {
                    capture.lock().unwrap().push_back(wire.to_vec());
                    Ok(())
                },
                RouterSideOptions {
                    reliable_enabled: true,
                    ..Default::default()
                },
            );
            let outgoing = Arc::new(Mutex::new(VecDeque::new()));
            let capture = outgoing.clone();
            let side = router.add_side_packed_with_options(
                "GS",
                move |wire| {
                    capture.lock().unwrap().push_back(wire.to_vec());
                    Ok(())
                },
                RouterSideOptions {
                    reliable_enabled: true,
                    ..Default::default()
                },
            );
            let announce = sedsnet::discovery::build_discovery_announce(
                board.sender_id(),
                utc,
                &std::iter::once(recovery).chain(command).collect::<Vec<_>>(),
            )
            .map_err(|e| format!("peer announce:{e:?}"))?;
            self.host
                .rx_from_side(&announce, hs)
                .map_err(|e| format!("announce host admission:{e:?}"))?;
            router
                .rx_from_side(
                    &sedsnet::discovery::build_discovery_announce(
                        "GS",
                        utc,
                        &[crate::telemetry_schema::endpoints::GROUND_STATION],
                    )
                    .map_err(|e| format!("GS announce:{e:?}"))?,
                    side,
                )
                .map_err(|e| format!("announce peer admission:{e:?}"))?;
            self.peers.push(Peer {
                router,
                host_side: hs,
                side,
                incoming,
                outgoing,
                requests,
                board,
            });
            self.pump()?;
        }
        super::observer::start(&self.state);
        let end = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            self.pump()?;
            if super::require_routes(&self.state).is_ok() {
                break;
            }
            if std::time::Instant::now() >= end {
                return Err("bounded command-route observation unavailable".into());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        for board in crate::board_abort::BOARDS {
            let response = crate::board_recovery::request_board_query(&self.state, board);
            if response.status() != axum::http::StatusCode::OK {
                return Err(format!(
                    "actual QUERY {}:{}",
                    board.sender_id(),
                    response.status()
                ));
            }
            let nonce=crate::board_recovery::query_nonce_for_test(&self.state,board).ok_or("admitted QUERY correlation absent")?;
            let end=std::time::Instant::now()+Duration::from_secs(1);
            loop {
                self.pump()?;
                if crate::board_recovery::query_reply_current_for_test(&self.state,board,nonce){break;}
                if std::time::Instant::now()>=end{return Err(format!("bounded exact nonce reply unavailable:{}",board.sender_id()));}
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        let mask = crate::board_abort::required_mask(&self.state) | super::REQUIRED_MASK;
        let end = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            self.pump()?;
            let (aborted, proven) = self.state.board_abort.masks();
            if aborted == 0 && proven & mask == mask && super::require_routes(&self.state).is_ok() {
                break;
            }
            if std::time::Instant::now() >= end {
                return Err("bounded correlated query proof unavailable".into());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        self.pump()?;
        let queues=self.host.export_runtime_stats().queues;
        if queues.tx_len!=0||queues.rx_len!=0{return Err(format!("warmup not drained: tx={} rx={}",queues.tx_len,queues.rx_len));}
        Ok(())
    }
}

impl Scene {
    /// Exact GPS-first N5 readonly pressure used by the actual prefix probe.
    /// Processing never resumes after this method. Retention is not inferred
    /// from the reliable-pending API for these unreliable sensor types.
    pub(crate) fn preload_readonly_gps_pressure(&self) {
        for (name, values) in [("GPS_DATA", vec![43.0f32,-78.0,0.0]),("KG1000",vec![0.0]),("KG50",vec![0.0]),("FUEL_FLOW",vec![0.0]),("BATTERY_VOLTAGE",vec![0.0])] {
            let bytes=values.into_iter().flat_map(f32::to_le_bytes).collect::<Vec<_>>();
            let packet=Packet::new(crate::telemetry_schema::data_type(name),&[crate::telemetry_schema::endpoints::GROUND_STATION],"GS",100,Arc::from(bytes)).unwrap();
            let result=self.host.tx_queue(packet);
            if name=="GPS_DATA" {assert!(result.is_ok(),"actual priority255 GPS pressure must be admitted");}
            if result.is_err(){break;}
        }
    }
}

impl Scene {
    /// A real, readonly Ordered QUERY remains queued; no peer reply is pumped.
    /// Admission refusal is a fixture prerequisite failure, not SDK Launch0/N.
    pub(crate) fn preload_readonly_query(&self,board:Board)->bool {
        let old=crate::board_recovery::query_nonce_for_test(&self.state,board);
        let status=crate::board_recovery::request_board_query(&self.state,board).status();
        let nonce=crate::board_recovery::query_nonce_for_test(&self.state,board);
        status==axum::http::StatusCode::OK&&nonce.is_some()&&nonce!=old
            &&self.host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"))&&self.scope_current()
    }
    pub(crate) fn scope_current(&self)->bool {
        let required=crate::board_abort::required_mask(&self.state)|super::REQUIRED_MASK;
        let(aborted,proven)=self.state.board_abort.masks();
        aborted==0&&proven&required==required&&super::require_routes(&self.state).is_ok()
    }
}

impl Scene {
    /// Existing APP HEARTBEAT: priority255, NoData. A readonly queue owner,
    /// never a fabricated board proof or an output command.
    pub(crate) fn preload_readonly_heartbeat(&self)->bool {
        let packet=Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),&[crate::telemetry_schema::endpoints::GROUND_STATION],"GS",100,Arc::from(Vec::<u8>::new())).unwrap();
        self.host.tx_queue(packet).is_ok()&&self.scope_current()
    }
}
