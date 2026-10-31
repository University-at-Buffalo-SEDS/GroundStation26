//! Old-product negative control. The intended assertion must fail until the
//! primary RX/background and SDK TX admission paths share arbitration.
use super::*;
use sedsnet::router::{
    EndpointHandler, RetryTrackerLimits, RouterConfig,
    RouterSideOptions,
};
use serialport::SerialPort;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Instant;

type Events = Arc<Mutex<Vec<serde_json::Value>>>;
static EVENT_SINK: OnceLock<Events> = OnceLock::new();

struct PhaseAdmissionLogger;
impl log::Log for PhaseAdmissionLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool { true }
    fn log(&self, record: &log::Record<'_>) {
        let message = record.args().to_string();
        if message.starts_with("full-bay UART capacity") {
            if let Some(events)=EVENT_SINK.get(){event(events,"actual_UART_capacity",serde_json::json!({"message":message}));}
        } else if message.starts_with("full-bay UART admission") {
            if let Some(events) = EVENT_SINK.get() {
                event(events, "actual_production_UART_admission", serde_json::json!({"message":message}));
            }
        }
    }
    fn flush(&self) {}
}
static PHASE_ADMISSION_LOGGER: PhaseAdmissionLogger = PhaseAdmissionLogger;

struct EvidenceOnDrop(Events);
impl Drop for EvidenceOnDrop {
    fn drop(&mut self) {
        if let Some(path) = std::env::var_os("GS_ARBITRATION_EVENTS") {
            if let Ok(bytes) = serde_json::to_vec_pretty(&*self.0.lock().unwrap()) {
                let _ = std::fs::write(path, bytes);
            }
        }
    }
}


pub(super) fn record_service(kind: &str, fields: serde_json::Value) {
    if let Some(events) = EVENT_SINK.get() { event(events, kind, fields); }
}

fn event(events: &Events, kind: &str, fields: serde_json::Value) {
    static WALL_START: OnceLock<Instant> = OnceLock::new();
    let elapsed = WALL_START.get_or_init(Instant::now).elapsed().as_micros();
    let mut entries = events.lock().unwrap();
    assert!(entries.len() < 2048, "finite native event capture exhausted");
    entries.push(serde_json::json!({
        "wall_elapsed_us": elapsed,
        "event": kind,
        "virtual_ms": crate::simulation_clock_ms(),
        "fields": fields,
    }));
}


fn packet_id(bytes: &[u8]) -> u64 {
    serialize::packet_id_from_wire(bytes).expect("actual canonical SDK owner")
}

fn drain_peer(peer: &mut serialport::TTYPort, captured: &mut Vec<u8>) {
    drain_peer_state(peer, captured, false);
}

fn drain_peer_state(peer: &mut serialport::TTYPort, captured: &mut Vec<u8>, writers_closed: bool) {
    let mut bytes = [0; 4096];
    loop {
        match peer.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => {
                assert!(
                    captured.len() + n <= 65536,
                    "finite native UART capture exhausted"
                );
                captured.extend_from_slice(&bytes[..n]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                break;
            }
            Err(error) if writers_closed && error.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(error) => panic!("private PTY capture failed: {error}"),
        }
    }
}

fn physical_frames(raw: &[u8], require_complete: bool) -> Vec<Vec<u8>> {
    let mut cursor = 0;
    let mut frames = Vec::new();
    while cursor < raw.len() {
        if raw.len() - cursor < 4 {
            assert!(
                !require_complete,
                "partial physical header is not completion"
            );
            break;
        }
        assert_eq!(&raw[cursor..cursor + 2], &[0xa6, 0x5b]);
        let length = u16::from_le_bytes(raw[cursor + 2..cursor + 4].try_into().unwrap()) as usize;
        let end = cursor + 4 + length;
        if end > raw.len() {
            assert!(
                !require_complete,
                "partial physical owner cannot count as delivered"
            );
            break;
        }
        let body = raw[cursor + 4..end].to_vec();
        assert!(body.starts_with(b"RR\x01"));
        assert_eq!(
            crc32fast::hash(&body[..body.len() - 4]),
            u32::from_le_bytes(body[body.len() - 4..].try_into().unwrap())
        );
        frames.push(body);
        cursor = end;
    }
    frames
}

#[tokio::test]
async fn root_profile_clock_and_catalog_share_ordinary_service() {
    const NAME:&str="telemetry_task::radio_io::root_ordinary_turn_tests::root_profile_clock_and_catalog_share_ordinary_service";
    if std::env::var_os("GS_WARM_ADDRESS_CHILD").is_none() {
        for (hz,with_clock) in [(2,true)] {
            let dir=std::env::temp_dir().join(format!("gs-warm-address-{}-{}-{hz}-{with_clock}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            std::fs::create_dir_all(&dir).unwrap();std::fs::write(dir.join("clock"),"2000").unwrap();
            let mut child=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact",NAME,"--nocapture","--test-threads=1"])
                .env("GS_WARM_ADDRESS_CHILD",hz.to_string()).env("GS_WARM_ADDRESS_WITH_CLOCK",if with_clock{"1"}else{"0"}).env("GS_SIM_CLOCK_FILE",dir.join("clock"))
                .env("GS_SIMULATED_SERIAL_PTY","1").env("GS_RRLS_ROLE","primary").env("GS_RADIO_AIR_BIT_RATE_BPS","32000")
                .env_remove("GS_RADIO_AIR_SHARE_PERCENT").env("GS_SIM_TRACE_RELIABLE","1")
                .env("GS_NETWORK_VARIABLE_CACHE",dir.join("network.json")).env("GS_ARBITRATION_EVENTS",dir.join("events.json"))
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            let until=Instant::now()+Duration::from_secs(60);let mut killed=false;
            while child.try_wait().unwrap().is_none(){if Instant::now()>=until{child.kill().unwrap();killed=true;break;}std::thread::sleep(Duration::from_millis(5));}
            let result=child.wait_with_output().unwrap();println!("WARM_ADDRESS_ARTIFACTS hz={hz} clock={with_clock} {}",dir.display());print!("{}",String::from_utf8_lossy(&result.stdout));eprint!("{}",String::from_utf8_lossy(&result.stderr));
            assert!(!killed && result.status.success(),"actual pipeline fixture failed/precondition; no field-cause claim");
        }
        return;
    }
    fn write_warm_clock(path:&std::path::Path,now:&AtomicU64,value:u64){
        let next=path.with_extension("next");std::fs::write(&next,value.to_string()).unwrap();std::fs::rename(next,path).unwrap();now.store(value,Ordering::SeqCst);
    }
    crate::comms::rfd_profile::initialize(&crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "native-pty-fixture".into(), hardware_flow_control: false,
            rfd_profile: None, baud_rate: 115200, protocol: crate::comms_config::SerialProtocol::RawUart,
        },
    }).unwrap();
    assert!(crate::comms::rfd_profile::active().is_some(), "real primary startup matched profile is required");
    crate::telemetry_schema::initialize().unwrap();
    let with_clock=std::env::var("GS_WARM_ADDRESS_WITH_CLOCK").unwrap()=="1";
    let hz:u64=std::env::var("GS_WARM_ADDRESS_CHILD").unwrap().parse().unwrap();assert!([2,4].contains(&hz));
    let clock=std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());let now=Arc::new(AtomicU64::new(2000));let sdk_now=now.clone();
    let events:Events=Arc::new(Mutex::new(Vec::new()));assert!(EVENT_SINK.set(events.clone()).is_ok());log::set_logger(&PHASE_ADMISSION_LOGGER).unwrap();log::set_max_level(log::LevelFilter::Info);let _dump=EvidenceOnDrop(events.clone());
    let router=Arc::new(Router::new_with_clock(RouterConfig::new([EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("GROUND_STATION"), |_| Ok(()))]).with_sender("GS"),Box::new(move||sdk_now.load(Ordering::SeqCst))));
    let budget=Arc::new(TxQueueBudget::new_radio(2048,256));let(sender,receiver)=mpsc::unbounded_channel();let queue_budget=budget.clone();let callback_events=events.clone();
    let accepted=Arc::new(AtomicBool::new(false));let accepting=accepted.clone();let transfer=Arc::new(Mutex::new(None::<u32>));let seen_transfer=transfer.clone();let stages=Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));let callback_stages=stages.clone();
    let (side,tracker)=router.add_side_packed_leased_with_priority_and_options("rocket_comms",move|lease,priority|{
        if !accepting.load(Ordering::SeqCst){return Ok(());}
        let b=lease.bytes();let chunk=b.starts_with(b"SDT\x03") && b.len()>=16 && u16::from_le_bytes(b[10..12].try_into().unwrap())>1;
        let is_ack=serialize::peek_frame_info_unchecked(b).is_ok_and(|i|i.envelope.ty==sedsnet::config::DataType::ReliableAck);
        if !chunk && !is_ack{return Ok(());}
        if chunk {
            let id=u32::from_le_bytes(b[4..8].try_into().unwrap());let index=u16::from_le_bytes(b[8..10].try_into().unwrap());let mut known=seen_transfer.lock().unwrap();
            if index==0 && b.get(12..32)==Some(&include_bytes!("../test-fixtures/root-default-catalog.bin")[..20]){assert!(known.is_none_or(|v|v==id));*known=Some(id);}
            if *known!=Some(id){return Ok(());}
        }
        let index=chunk.then(||u16::from_le_bytes(b[8..10].try_into().unwrap()));
        let result=queue_budget.try_send_leased(&sender,priority,lease);
        if result.is_ok(){let row=serde_json::json!({"at_ms":crate::simulation_clock_ms(),"chunk_index":index,"priority":priority});callback_stages.lock().unwrap().push(row.clone());event(&callback_events,"actual_SDK_accepted",row);}
        result
    },RouterSideOptions{reliable_enabled:false,..Default::default()}.with_small_packet_transport(192),RetryTrackerLimits::default()).unwrap();
    router.clear_tx_queue();
    let mut primary=crate::rrls::Mesh::new(crate::rrls::Role::Primary,42);let mut relay=crate::rrls::Mesh::new(crate::rrls::Role::Rrls,43);primary.enable_settings_framing();relay.enable_settings_framing();
    write_warm_clock(&clock,&now,30843);
    fn boot(from:&mut crate::rrls::Mesh,to:&mut crate::rrls::Mesh){let mut total=0;while let Some(frame)=from.pop(30843){total+=frame.bytes.len()+20;to.incoming(frame.command,&frame.bytes,30843);assert!(total<1024);}}
    primary.tick(30843);boot(&mut primary,&mut relay);relay.tick(30843);boot(&mut relay,&mut primary);boot(&mut primary,&mut relay);assert_eq!(primary.recovery_air_share(30843),Some(15));assert_eq!(relay.recovery_air_share(30843),Some(35));
    let primary=Arc::new(Mutex::new(primary));let relay=Arc::new(Mutex::new(relay));
    let(mut peer,port)=serialport::TTYPort::pair().unwrap();peer.set_timeout(Duration::from_millis(1)).unwrap();let mut rx=crate::comms::UartComms::test_mesh_pty(port,primary.clone());rx.set_side_id(side);rx.bind_retry_tracker(tracker).unwrap();let mut tx=rx.try_clone_tx().unwrap().unwrap();
    let(mut relay_peer,relay_port)=serialport::TTYPort::pair().unwrap();relay_peer.set_timeout(Duration::from_millis(1)).unwrap();let mut relay_uart=crate::comms::UartComms::test_mesh_pty(relay_port,relay.clone());
    let source=include_bytes!("../test-fixtures/root-default-catalog.bin");assert_eq!(source.len(),3725);let source_packet=serialize::unpack_packet(source).unwrap();assert_eq!(source_packet.data_type(),sedsnet::config::DataType::DiscoverySchema);
    // Actual SDK-generated ACK return routing, not application-origin ACK injection.
    let load_type=crate::telemetry_schema::data_type("BATTERY_VOLTAGE");
    let evidence=std::path::PathBuf::from(std::env::var_os("GS_ARBITRATION_EVENTS").unwrap());
    let mut primary_bytes=Vec::new();let mut relay_bytes=Vec::new();let mut pi=0;let mut ri=0;let mut physical=Vec::new();let mut rf_records=Vec::new();let mut offered=0;
    let mut clock_fixture=with_clock.then(||crate::launch_clock_sync::pipeline_fixture::Fixture::new(evidence.parent().unwrap(),relay.lock().unwrap().operating_mode_session(30843).unwrap(),30843));
    if let Some(f)=clock_fixture.as_mut(){
        f.queue_query();
        for setup in 30843..31643 {
            write_warm_clock(&clock,&now,setup);
            if setup%10!=3{continue;}
            f.flush_observer(&relay,setup);relay_uart.service_transmit().unwrap();
            let previous=relay_bytes.len();drain_peer(&mut relay_peer,&mut relay_bytes);
            for byte in &relay_bytes[previous..]{rf_records.extend(setup.to_le_bytes());rf_records.push(*byte);}
            let frames=physical_frames(&relay_bytes,false);for body in frames.iter().skip(ri){primary.lock().unwrap().incoming(true,body,setup);}ri=frames.len();
            let query=primary.lock().unwrap().take_launch_clock();if let Some(wire)=query{f.accept_query(&wire,setup);}
            rx.service_transmit().unwrap();tx.service_background_transmit().unwrap();
            drain_peer(&mut peer,&mut primary_bytes);let frames=physical_frames(&primary_bytes,false);for body in frames.iter().skip(pi){relay.lock().unwrap().incoming(true,body,setup);}pi=frames.len();
        }
        assert!(f.query_received,"actual charged query must fullyreach real primaryRuntime before measurement");
        assert!(f.original_reply.len()>80);
    }
    let state=crate::state::tests::test_app_state().await;
    let workers=spawn_comms_worker_threads(router.clone(),state.clone(),CommsWorkerHandle{transport_activity:None,name:"rocket_comms",tx_budget:Some(budget.clone()),comms:Arc::new(Mutex::new(Box::new(rx))),tx_comms:Some(Arc::new(Mutex::new(tx))),side_id:side,tx_rx:receiver,legacy_single_worker:false,prioritize_rx:false,dedicated_radio_io:false}).unwrap();
    write_warm_clock(&clock,&now,31643);accepted.store(true,Ordering::SeqCst);event(&events,"declared_source_offer",serde_json::json!({"hz":hz,"packet_id":packet_id(source),"clock_reply_included":with_clock,"scope":"CRC-valid captured source CONTROL, declared load; not inferred original callback time"}));router.tx_packed_queue(Arc::from(source.as_slice())).unwrap();
    let mut clock_offer_at=None;
    let mut owned_clock_lane_seen=false;
    let mut offer_release_at=None;
    let mut delayed_offers:Vec<Vec<u8>>=Vec::new();
    for tick in 31643..=51643 {
        write_warm_clock(&clock,&now,tick);
        if offer_release_at.is_some_and(|at|tick>=at){
            for wire in std::mem::take(&mut delayed_offers){primary.lock().unwrap().incoming(true,&wire,tick);event(&events,"declared_OFFER_released",serde_json::json!({"at_ms":tick,"cause":"600ms after first complete counted PROBE stop, no byte corruption or quota bypass"}));}
        }
        if tick<37643 && (tick-31643)%(1000/hz)==0 {let packet=sedsnet::packet::Packet::new(load_type,&[crate::telemetry_schema::endpoint("GROUND_STATION")],"PB",tick,12.5f32.to_le_bytes().to_vec().into()).unwrap();router.rx_from_side(&packet,side).unwrap();offered+=1;}
        if clock_offer_at.is_some_and(|at|tick>=at) {
            if let Some(f)=clock_fixture.as_mut(){if !f.offered{f.offer(tick);}}
        }
        if let Some(f)=clock_fixture.as_mut(){if tick%10==3{f.flush_primary(&primary,tick);}}
        {
            let mesh=primary.lock().unwrap();
            if mesh.has_settings_parent_for_test() {
                assert_ne!(mesh.ordinary_background_state(),crate::comms::OrdinaryLaneState::Empty,
                    "a live owned SETTINGS parent must never be classified as an absent lane");
                owned_clock_lane_seen=true;
            }
        }
        router.dispatch_tx_queue_with_timeout(0).unwrap();std::thread::sleep(Duration::from_millis(1));
        if tick%10==3 {
            drain_peer(&mut peer,&mut primary_bytes);let frames=physical_frames(&primary_bytes,false);
            for body in frames.iter().skip(pi){
                if body.get(3)==Some(&2) && body[4..].starts_with(b"SDT\x03") {physical.push(serde_json::json!({"at_ms":tick,"index":u16::from_le_bytes(body[12..14].try_into().unwrap()),"inner_hex":body[4..body.len()-4].iter().map(|b|format!("{b:02x}")).collect::<String>()}));}
                if with_clock && clock_offer_at.is_none() && body.get(3)==Some(&2) && body[4..].starts_with(b"SDT\x03") && u16::from_le_bytes(body[12..14].try_into().unwrap())==0 {clock_offer_at=Some(tick+1);}
                if offer_release_at.is_none() && body.get(3)==Some(&15) && body.get(4)==Some(&0x10) && u16::from_le_bytes(body[30..32].try_into().unwrap()) as usize==clock_fixture.as_ref().unwrap().original_reply.len() {
                    let entries=events.lock().unwrap();
                    let message=entries.iter().rev().filter(|e|e["event"]=="actual_UART_capacity").filter_map(|e|e["fields"]["message"].as_str()).find(|m|m.contains("side=Some(0)") && m.contains("encoded_bytes=59 ")).unwrap();
                    let stop:u64=message.split("stop_us=Some(").nth(1).unwrap().split(')').next().unwrap().parse().unwrap();
                    let release=stop.div_ceil(1000)+600;drop(entries);offer_release_at=Some(release);
                    event(&events,"first_clock_PROBE_counted_stop",serde_json::json!({"observed_tick":tick,"stop_us":stop,"OFFER_release_at":release}));
                }
                relay.lock().unwrap().incoming(true,body,tick);
            }pi=frames.len();
            let (clock_wire,session,fresh)={let mut r=relay.lock().unwrap();(r.take_launch_clock(),r.operating_mode_session(tick),r.settings_available(tick))};
            if let Some(wire)=clock_wire {clock_fixture.as_mut().expect("no unsolicitedclockfixture").consume(&wire,tick,session,fresh).await;}
            relay_uart.service_transmit().unwrap();
            let previous=relay_bytes.len();drain_peer(&mut relay_peer,&mut relay_bytes);
            // Diagnostic byte timestamps are conservative observed drain ticks,
            // not physical stop-bit measurements. Actual admissions also logged.
            for byte in &relay_bytes[previous..]{rf_records.extend(tick.to_le_bytes());rf_records.push(*byte);}
            let replies=physical_frames(&relay_bytes,false);for body in replies.iter().skip(ri){
                let delayed=body.get(3)==Some(&15) && body.get(4)==Some(&0x11) && body.len()>=49 && u16::from_le_bytes(body[47..49].try_into().unwrap()) as usize==clock_fixture.as_ref().unwrap().original_reply.len();
                if delayed && offer_release_at.is_some_and(|at|tick<at){assert!(delayed_offers.len()<2);delayed_offers.push(body.clone());event(&events,"declared_OFFER_delayed_after_charged_emission",serde_json::json!({"at_ms":tick,"release_at":offer_release_at}));}
                else {primary.lock().unwrap().incoming(true,body,tick);}
            }ri=replies.len();
        }
    }
    state.request_shutdown();for worker in workers{worker.join().unwrap();}
    drain_peer_state(&mut peer,&mut primary_bytes,true);
    let path=std::path::PathBuf::from(std::env::var_os("GS_ARBITRATION_EVENTS").unwrap());let dir=path.parent().unwrap();std::fs::write(dir.join("primary-uart.bin"),&primary_bytes).unwrap();std::fs::write(dir.join("relay-uart.bin"),&relay_bytes).unwrap();std::fs::write(dir.join("rf-observed-ticks.records"),&rf_records).unwrap();
    let summary=serde_json::json!({"hz":hz,"clock":clock_fixture.as_ref().map(|f|f.summary()),"clock_offer_at":clock_offer_at,"receipt_offers":offered,"accepted_callbacks":*stages.lock().unwrap(),"physical_address_chunks":physical,"source_packet_id":packet_id(source),"pending_control":budget.pending_control.load(Ordering::Acquire),"primary_bytes":primary_bytes.len(),"relay_bytes":relay_bytes.len(),"scope":"Actual SDK/leased/channel/backlog/UART under declared finite load; callback/drainobservations, not field chronology or ACKcompletion. RF recordedticks areobservations, not exactstopbits; productionC+SDKpostflightstillrequired."});std::fs::write(dir.join("summary.json"),serde_json::to_vec_pretty(&summary).unwrap()).unwrap();println!("DECLARED_LOAD_SUMMARY {}",summary);
    let frames=physical_frames(&primary_bytes,true);
    let legacy=frames.iter().filter(|b|b.get(3)==Some(&10) && b[4..b.len()-4]==clock_fixture.as_ref().unwrap().original_reply).count();
    event(&events,"original_parent_anchor_result",serde_json::json!({"legacy_whole_reply_count":legacy,"offer_release_at":offer_release_at,"clock":clock_fixture.as_ref().unwrap().summary()}));
    // Reconcile policy against actual completed writes, including every real
    // SDK-generated receipt. Critical service must neither spend nor erase it.
    let mut expected_balance=0i32;
    let mut critical_writes=0;
    let mut ordinary_writes=0;
    let limit=crate::comms::MAX_ORDINARY_SERVICE_CHARGE as i32;
    for row in events.lock().unwrap().iter() {
        let fields=&row["fields"];
        match row["event"].as_str() {
            Some("ordinary_idle_reset")=>{
                assert_eq!(fields["mesh_state"],"Empty","SDK channel gaps must never erase earned Mesh history");
                assert_eq!(fields["transport_ready"],true);
                expected_balance=0;
            }
            Some("sdk_completed_service")=>{
                let cost=fields["actual_charge"].as_u64().expect("matched Primary must report actual counted SDK cost") as i32;
                if fields["ordinary"]==true {
                    ordinary_writes+=1;
                    assert_eq!(cost as u64,fields["payload_bytes"].as_u64().unwrap()+8+4+16,
                        "actual completed SDK bytes must include RR, UART and configured per-frame overhead");
                    if fields["mesh_state"]=="Some(Ready)" { expected_balance=expected_balance.saturating_add(cost).clamp(-limit,limit); }
                } else { critical_writes+=1; }
                assert_eq!(fields["balance"],expected_balance,"critical/blocked service cannot change ordinary byte history");
            }
            Some("mesh_completed_service")=>{
                let cost=fields["ordinary_charge"].as_u64().unwrap() as i32;
                if cost>0 && fields["sdk_ready"]==true { expected_balance=expected_balance.saturating_sub(cost).clamp(-limit,limit); }
                assert_eq!(fields["balance"],expected_balance);
            }
            _=>{}
        }
    }
    assert_eq!(critical_writes,12,"all twelve actual SDK receipt writes must retain the critical path");
    assert!(ordinary_writes>1,"actual root-default catalog writes must exercise byte service");
    assert!(owned_clock_lane_seen,"actual owned SETTINGS lane classification must be exercised");
    assert!(offer_release_at.is_some(),"a real complete counted PROBE is required");
    assert_eq!(legacy,0,"INTENDED_QUEUE_ANCHORED_PROBE_VIOLATION: clock metadata waited before first physical PROBE; a physically timely OFFER must not force wholeframe legacy");
    let actual_receipts=stages.lock().unwrap().iter().filter(|r|r["chunk_index"].is_null()).count();
    assert_eq!(actual_receipts,offered,"DECLARED_LOAD_PRECONDITION: every source offer must yield an actual SDK receipt callback before any scheduling claim");
    assert!(transfer.lock().unwrap().is_some(),"actual SDK must encode target catalog owner into multiple root192 chunks");
    assert!(stages.lock().unwrap().iter().any(|r|r["chunk_index"]==0),"target first actual callback acceptance required");
    assert!(physical.len()>1,"actual root192 policy must encode the genuine3725B catalog into multiple physical chunks");
    let reconstructed: Vec<u8> = physical.iter().flat_map(|row| {
        let hex=row["inner_hex"].as_str().unwrap();
        let wire:Vec<u8>=(0..hex.len()).step_by(2).map(|i|u8::from_str_radix(&hex[i..i+2],16).unwrap()).collect();
        wire[12..wire.len()-4].to_vec()
    }).collect();
    assert_eq!(reconstructed,source,"all original canonical catalog bytes must physically leave, unchanged");
    assert_eq!(clock_fixture.as_ref().unwrap().applied,1,"real ClockReply must apply and persist inside its original5s transport limit");
    let largest_gap=physical.windows(2).map(|pair|pair[1]["at_ms"].as_u64().unwrap()-pair[0]["at_ms"].as_u64().unwrap()).max().unwrap();
    assert!(largest_gap<=2000,"ROOT_DEFAULT_ORDINARY_TURN_STARVATION: actual anchored5s ClockReply and2Hz receipts leave catalog fragment gap {largest_gap}ms beyond unchanged SDK assembly deadline");
}
