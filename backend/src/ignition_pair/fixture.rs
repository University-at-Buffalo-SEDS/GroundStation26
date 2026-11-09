//! Test-only real SDK peers and correlated recovery ingress on the enqueue Router.
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

pub(crate) fn handler<F>(state: &Arc<AppState>, control: F) -> EndpointHandler
where
    F: Fn(&Packet) -> sedsnet::TelemetryResult<()> + Send + Sync + 'static,
{
    let weak = Arc::downgrade(state);
    EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("GROUND_STATION"),
        move |packet| {
            if packet.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS") {
                if let Some(state) = weak.upgrade() {
                    crate::board_abort::observe_raw(&state, packet);
                    if packet.payload().len() == 40
                        && let Some(board) = state.board_from_network_sender(packet.sender())
                    {
                        state.mark_board_seen(
                            board.sender_id(),
                            crate::telemetry_task::get_current_timestamp_ms(),
                        );
                    }
                    crate::board_recovery::receive_status(&state, packet);
                }
                Ok(())
            } else {
                control(packet)
            }
        },
    )
}
pub(crate) fn query_reply(board: Board, packet: &Packet) -> sedsnet::TelemetryResult<Packet> {
    let request = packet.payload();
    assert_eq!(
        packet.data_type(),
        crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST")
    );
    assert_eq!(request.len(), 32);
    assert_eq!(request[1], 0, "fixture never recovers boards implicitly");
    let mut response = [0; 40];
    response[0] = 1;
    response[2] = if matches!(board, Board::ActuatorBoard | Board::ValveBoard) {
        1
    } else {
        0
    };
    response[3] = if matches!(
        board,
        Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
    ) {
        0x7e
    } else {
        0x2c
    };
    response[4..12].copy_from_slice(&request[4..12]);
    response[12..20].copy_from_slice(
        &(if board == Board::ActuatorBoard {
            101u64
        } else {
            102u64
        })
        .to_le_bytes(),
    );
    response[20..24].copy_from_slice(&1u32.to_le_bytes());
    response[28..32].copy_from_slice(
        &(if board == Board::ValveBoard {
            6u32
        } else {
            0u32
        })
        .to_le_bytes(),
    );
    response[32..40].copy_from_slice(&request[24..32]);
    Packet::new(
        crate::telemetry_schema::data_type("BOARD_STATE_STATUS"),
        &[crate::telemetry_schema::endpoints::GROUND_STATION],
        board.sender_id(),
        packet.timestamp(),
        Arc::from(response),
    )
}
struct Peer {
    board: Board,
    router: Router,
    host_side: usize,
    side: usize,
    to_peer: Arc<Mutex<VecDeque<Vec<u8>>>>,
    to_host: Arc<Mutex<VecDeque<Vec<u8>>>>,
    requests: Arc<Mutex<VecDeque<Packet>>>,
}
pub(crate) struct Peers {
    host: Arc<Router>,
    peers: Vec<Peer>,
}
impl Peers {
    pub(crate) fn attach(state: &Arc<AppState>, host: Arc<Router>, excluded: &[Board]) -> Self {
        if let Some(bound) = state.topology_router.get() {
            assert!(
                Arc::ptr_eq(bound, &host),
                "proof must use exact enqueue Router"
            );
        } else {
            state.topology_router.set(host.clone()).unwrap();
        }
        let mut fixture = Self {
            host,
            peers: Vec::new(),
        };
        if fixture.host.network_time_ms().is_none() {
            crate::telemetry_task::refresh_host_network_time(
                &fixture.host,
                crate::telemetry_task::get_current_timestamp_ms(),
            );
        }
        for board in crate::board_abort::BOARDS {
            if excluded.contains(&board) {
                continue;
            }
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
            let utc = fixture.host.network_time_ms().unwrap();
            router
                .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                    utc,
                ))
                .unwrap();
            let to_peer = Arc::new(Mutex::new(VecDeque::new()));
            let capture = to_peer.clone();
            let hs = fixture.host.add_side_packed_with_options(
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
            let to_host = Arc::new(Mutex::new(VecDeque::new()));
            let capture = to_host.clone();
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
            fixture
                .host
                .rx_from_side(
                    &sedsnet::discovery::build_discovery_announce(
                        board.sender_id(),
                        utc,
                        &std::iter::once(recovery).chain(command).collect::<Vec<_>>(),
                    )
                    .unwrap(),
                    hs,
                )
                .unwrap();
            router
                .rx_from_side(
                    &sedsnet::discovery::build_discovery_announce(
                        "GS",
                        utc,
                        &[crate::telemetry_schema::endpoints::GROUND_STATION],
                    )
                    .unwrap(),
                    side,
                )
                .unwrap();
            fixture.peers.push(Peer {
                board,
                router,
                host_side: hs,
                side,
                to_peer,
                to_host,
                requests,
            });
        }
        crate::ignition_pair::observer::start(state);
        fixture
    }
    pub(crate) async fn warm(state: &Arc<AppState>, host: Arc<Router>) -> Self {
        let mut fixture = Self::attach(state, host, &[]);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                fixture.pump();
                if crate::ignition_pair::require_routes(state).is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("actual command peers must be discovered on exact enqueue Router");
        fixture.refresh(state).await;
        // No background queue worker remains: original enqueue/prompt-return tests
        // retain exclusive control of processing after this drained warmup.
        fixture.pump();
        fixture
    }
    pub(crate) async fn warm_pair_only(state: &Arc<AppState>, host: Arc<Router>) -> Self {
        let mut fixture = Self::attach(state, host, &[Board::FlightComputer,Board::RFBoard,Board::PowerBoard,Board::GatewayBoard,Board::DaqBoard]);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                fixture.pump();
                if crate::ignition_pair::require_routes(state).is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("actual command peers must be discovered on exact enqueue Router");
        fixture.refresh_boards_with(state,&[Board::ActuatorBoard,Board::ValveBoard],||{}).await;
        // No background queue worker remains: original enqueue/prompt-return tests
        // retain exclusive control of processing after this drained warmup.
        fixture.pump();
        fixture
    }
    pub(crate) fn pending_transport_wires(&self)->Vec<Vec<u8>> {
        // Snapshot actual enqueue Router side callbacks without advancing any
        // peer, ACK, retry or application effect after the original offer.
        self.peers.iter().flat_map(|peer|peer.to_peer.lock().unwrap().iter().cloned().collect::<Vec<_>>()).collect()
    }
    pub(crate) fn pump(&mut self) {
        for _ in 0..20 {
            self.host.process_all_queues_with_timeout(0).unwrap();
            for peer in &mut self.peers {
                while let Some(wire) = peer.to_peer.lock().unwrap().pop_front() {
                    peer.router.rx_packed_from_side(&wire, peer.side).unwrap();
                }
                peer.router.process_all_queues_with_timeout(0).unwrap();
                while let Some(packet) = peer.requests.lock().unwrap().pop_front() {
                    let request = packet.payload();
                    assert_eq!(request.len(), 32);
                    assert_eq!(request[1], 0, "read-only fixture never sends Recovery");
                    let mut response = [0; 40];
                    response[0] = 1;
                    response[2] = if matches!(peer.board, Board::ActuatorBoard | Board::ValveBoard)
                    {
                        1
                    } else {
                        0
                    };
                    response[3] = if matches!(
                        peer.board,
                        Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
                    ) {
                        0x7e
                    } else {
                        0x2c
                    };
                    response[4..12].copy_from_slice(&request[4..12]);
                    response[12..20].copy_from_slice(
                        &(if peer.board == Board::ActuatorBoard {
                            101u64
                        } else {
                            102u64
                        })
                        .to_le_bytes(),
                    );
                    response[20..24].copy_from_slice(&1u32.to_le_bytes());
                    response[28..32].copy_from_slice(
                        &(if peer.board == Board::ValveBoard {
                            6u32
                        } else {
                            0u32
                        })
                        .to_le_bytes(),
                    );
                    response[32..40].copy_from_slice(&request[24..32]);
                    let reply = Packet::new(
                        crate::telemetry_schema::data_type("BOARD_STATE_STATUS"),
                        &[crate::telemetry_schema::endpoints::GROUND_STATION],
                        peer.board.sender_id(),
                        peer.router.network_time_ms().unwrap(),
                        Arc::from(response),
                    )
                    .unwrap();
                    peer.router.tx_queue(reply).unwrap();
                }
                peer.router.process_all_queues_with_timeout(0).unwrap();
                while let Some(wire) = peer.to_host.lock().unwrap().pop_front() {
                    self.host
                        .rx_packed_from_side(&wire, peer.host_side)
                        .unwrap();
                }
            }
        }
    }
    pub(crate) async fn refresh(&mut self,state:&Arc<AppState>){
        self.refresh_with(state,||{}).await;
    }
    /// Serial QUERY ownership works with the unchanged one-entry TX policy.
    /// Other explicit fixture transports may pump alongside this peer cohort.
    pub(crate) async fn refresh_with(&mut self,state:&Arc<AppState>,other:impl FnMut()){
        self.refresh_boards_with(state,&crate::board_abort::BOARDS,other).await;
    }
    async fn refresh_boards_with(&mut self,state:&Arc<AppState>,boards:&[Board],mut other:impl FnMut()){
        for board in boards.iter().copied() {
            other();self.pump();
            assert_eq!(crate::board_recovery::request_board_query(state,board).status(),axum::http::StatusCode::OK,"actual nonce QUERY must be admitted");
            let nonce=crate::board_recovery::query_nonce_for_test(state,board).expect("admitted QUERY installs actual correlation");
            tokio::time::timeout(Duration::from_secs(2),async {loop{
                other();self.pump();
                if crate::board_recovery::query_reply_current_for_test(state,board,nonce){break;}
                tokio::time::sleep(Duration::from_millis(2)).await;
            }}).await.expect("this board's current nonce must receive a fresh correlated SDK reply before next QUERY");
        }
        tokio::time::timeout(Duration::from_secs(2),async{loop{
            other();self.pump();
            if state.board_abort.ignition_pair_current()&&crate::ignition_pair::require_routes(state).is_ok(){break;}
            tokio::time::sleep(Duration::from_millis(2)).await;
        }}).await.expect("actual current pair and route observation after sequential QUERY cohort");
    }
}
