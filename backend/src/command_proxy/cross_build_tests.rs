//! Cross-compiled fixture: the external runner pairs actual normal and HITL
//! executables. Without that runner the test checks only its local enum contract.
use super::*;
use std::path::Path;
use tower::ServiceExt;
const NAMES:[&str;4]=["TogglePlumbingDelayInterlock","ToggleChecklistInterlock","ToggleSupplyInterlock","ToggleBoardStateProofInterlock"];
fn put(root:&Path,name:&str,bytes:&[u8]) {
    let temporary=root.join(format!("{name}.partial"));std::fs::write(&temporary,bytes).unwrap();
    std::fs::rename(temporary,root.join(name)).unwrap();
}
async fn get(root:&Path,name:&str)->Vec<u8>{
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {if let Ok(bytes)=std::fs::read(root.join(name)){return bytes;}
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
    }).await.unwrap_or_else(|_|panic!("missing cross-compiled fixture message {name}"))
}
fn health(mesh:&crate::rrls::Shared,view:Transport){
    let mut bytes=b"RR\x01\x00".to_vec();bytes.extend(view.session.to_le_bytes());
    bytes.extend(view.sequence.to_le_bytes());bytes.push(1);
    let mut crc=u32::MAX;for byte in &bytes {crc^=u32::from(*byte);for _ in 0..8{crc=(crc>>1)^(0xedb88320&0u32.wrapping_sub(crc&1));}}
    bytes.extend((!crc).to_le_bytes());let mut mesh=mesh.lock().unwrap();let now=mesh.now();mesh.incoming(true,&bytes,now);
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn cross_compiled_interlock_exchange_or_local_enum_contract(){
    for name in NAMES {assert_eq!(serde_json::from_value::<crate::types::TelemetryCommand>(serde_json::json!(name)).is_ok(),cfg!(feature="hitl_mode"));}
    let Some(path)=std::env::var_os("GS_CROSS_COMPILED_EXCHANGE") else{return;};
    let shared=std::path::PathBuf::from(path);
    let (root,p,r)=super::tests::fixtures();let mesh=super::handler_tests::configure();
    let mut state=crate::recording_export::tests::export_state(&root.0,false).await;
    let (tx,rx)=tokio::sync::mpsc::channel(8);Arc::get_mut(&mut state).unwrap().cmd_tx=tx;
    super::handler_tests::user(&state.auth,true,false,false,&NAMES);
    if cfg!(feature="hitl_mode") {
        assert!(!crate::rrls::is_rrls());
        let pv={let m=mesh.lock().unwrap();m.command_proxy_transport(m.now()).unwrap()};
        put(&shared,"transport",&serde_json::to_vec(&(pv.session,pv.sequence)).unwrap());
        let mut runtime=Runtime::new(Role::Primary);runtime.connect(Some(pv));
        let router=Arc::new(sedsnet::router::Router::new_with_clock(sedsnet::router::RouterConfig::new([]).with_sender("GS"),Box::new(||0)));
        state.topology_router.set(router.clone()).ok().unwrap();
        state.set_local_flight_state(crate::types::FlightState::Idle);
        let (_,db_rx)=tokio::sync::mpsc::channel(1);
        let worker=tokio::spawn(crate::telemetry_task::telemetry_task(state.clone(),router,Vec::new(),rx,db_rx,state.shutdown_subscribe()));
        runtime.observation=crate::primary_mirror::encode_control_observation(&state,96);
        let query=get(&shared,"query").await;
        let Received::Reply(reply)=runtime.receive(&p,pv,crate::station_mode::local_mode(),&query).unwrap()else{panic!("signed query reply")};
        assert!(!runtime.take_readiness_demand(),"Observe must not schedule a board poll");put(&shared,"capability",&reply);
        for (index,name) in NAMES.iter().enumerate(){
            let bytes=get(&shared,&format!("command{index}")).await;
            let Received::Execute(request)=runtime.receive(&p,pv,crate::station_mode::local_mode(),&bytes).unwrap()else{panic!("actual primary request")};
            assert_eq!(request.operation,Operation::Command((*name).into()));
            assert_eq!(execute_primary(state.clone(),&request).await,Outcome::Queued);
            #[cfg(feature="hitl_mode")]
            tokio::time::timeout(std::time::Duration::from_secs(2),async {
                let expected=matches!(index,1|2);
                while state.hitl_safety_interlock_enabled(index)==expected {tokio::task::yield_now().await;}
                while state.action_policy_snapshot().controls.iter().find(|c|c.cmd==*name).is_none_or(|c|c.actuated!=Some(!expected)){tokio::task::yield_now().await;}
            }).await.unwrap();
            runtime.observation=crate::primary_mirror::encode_control_observation(&state,96);
            let reply=runtime.finish(&p,request,Outcome::Queued).unwrap();put(&shared,&format!("result{index}"),&reply);
            if index==0 {
                let retry=get(&shared,"duplicate0").await;assert_eq!(retry,bytes);
                let Received::Reply(cached)=runtime.receive(&p,pv,crate::station_mode::local_mode(),&retry).unwrap()else{panic!("idempotent cached reply")};
                assert_eq!(cached,reply);put(&shared,"duplicate-result0",&cached);
            }
        }
        // Explicit read-only refresh after the receiver's durable-save drain.
        let query=get(&shared,"refresh-after-save").await;
        let Received::Reply(reply)=runtime.receive(&p,pv,crate::station_mode::local_mode(),&query).unwrap()else{panic!("fresh configuration observation")};
        put(&shared,"refresh-after-save-result",&reply);
        super::handler_tests::user(&state.auth,false,false,false,&NAMES);
        let bytes=get(&shared,"revoked").await;
        let Received::Execute(request)=runtime.receive(&p,pv,crate::station_mode::local_mode(),&bytes).unwrap()else{panic!("revoked actor request")};
        assert_eq!(execute_primary(state.clone(),&request).await,Outcome::Denied);
        put(&shared,"revoked-result",&runtime.finish(&p,request,Outcome::Denied).unwrap());
        state.request_shutdown();worker.await.unwrap();
    } else {
        assert!(crate::rrls::is_rrls());drop(rx);
        let (session,sequence):(u32,u32)=serde_json::from_slice(&get(&shared,"transport").await).unwrap();
        let rv=Transport{session,sequence:sequence.wrapping_sub(1)};health(&mesh,rv);
        let mut runtime=Runtime::new(Role::Rrls);runtime.connect(Some(rv));
        put(&shared,"query",&runtime.query_observe(&r,rv).unwrap());
        let Received::Capability{boot,mode,observation}=runtime.receive(&r,rv,crate::station_mode::local_mode(),&get(&shared,"capability").await).unwrap()else{panic!("actual HITL capability")};
        assert_eq!(mode,Mode::Hitl);assert_eq!(observation.len(),88);
        assert!(accept_presentation(&state,&observation,0,boot,session));
        let saved_path=root.0.join("retained-configuration.json");
        let mut retained=retained_config::Worker::open_at(Arc::new(Pairing::from_file(&root.0.join("rrls.json")).unwrap()),saved_path.clone());retained.idle().await;
        let disk=retained_config::hold_next_write();
        retained.accepted(mode,boot,&observation,0);
        tokio::time::timeout(std::time::Duration::from_secs(2),async{while !disk.entered(){tokio::task::yield_now().await;}}).await.unwrap();
        assert!(retained_config::snapshot(Some(mode),true).persistence_pending);
        assert_eq!(retained_config::snapshot(Some(mode),true).status,"unknown","no durable receipt invented while fsync held");
        assert_eq!(crate::primary_mirror::board_state_proof_policy(&state),Some(false));
        assert_eq!(crate::primary_mirror::board_history_required_scope(&state),Some(0),"normal RRLS must use signed primary optional policy rather than its own mandatory policy");
        {
            let mut broker=broker().lock().unwrap();broker.status.available=true;broker.status.primary_mode=Some(mode);
            let mask=runtime.capability.unwrap().2;
            broker.status.supported_commands=command_catalog().iter().enumerate().filter(|(i,_)|mask&(1<<i)!=0).map(|(_,name)|(*name).into()).collect();
        }
        crate::ensure_auth_sessions_table(&state.auth_db).await.unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES ('cross-token','operator','password',1,1,0,9223372036854775807)").execute(&state.auth_db).await.unwrap();
        for(index,name)in NAMES.iter().enumerate(){
            // Actual normal-build HTTP ingress must proxy before local enum
            // deserialization (which was asserted unsupported above).
            let response=crate::web::router(state.clone(),String::new()).oneshot(axum::http::Request::builder().method("POST").uri("/api/command")
                .header("content-type","application/json").header("authorization","Bearer cross-token")
                .body(axum::body::Body::from(serde_json::json!(name).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(),axum::http::StatusCode::ACCEPTED);
            let pending=broker().lock().unwrap().pending.take().unwrap();
            let bytes=runtime.request(&r,rv,mode,pending.id,pending.actor,pending.operation).unwrap();
            put(&shared,&format!("command{index}"),&bytes);
            let Received::Result{outcome,observation,..}=runtime.receive(&r,rv,Mode::Normal,&get(&shared,&format!("result{index}")).await).unwrap()else{panic!("signed command result")};
            assert_eq!(outcome,Outcome::Queued);assert_eq!(observation.len(),88);
            assert!(accept_presentation(&state,&observation,0,boot,session));
            retained.accepted(mode,boot,&observation,0);retained.service().await;
            assert!(retained_config::snapshot(Some(mode),true).persistence_pending,"actual signed control replies continue while metadata writer is held");
            assert_eq!(crate::primary_mirror::board_state_proof_policy(&state),Some(index==3));
            assert_eq!(crate::primary_mirror::board_history_required_scope(&state),Some(if index==3{0x7f}else{0}));
            let policy=crate::primary_mirror::action_policy(&state).unwrap();
            assert_eq!(policy.controls.iter().find(|control|control.cmd==*name).unwrap().actuated,Some(matches!(index,0|3)));
            if index==0 {put(&shared,"duplicate0",&bytes);assert!(matches!(runtime.receive(&r,rv,Mode::Normal,&get(&shared,"duplicate-result0").await).unwrap(),Received::Result{outcome:Outcome::Queued,..}));}
        }
        disk.release();retained.idle().await;
        assert_eq!(retained_config::snapshot(Some(mode),true).settings,[Some(true),Some(false),Some(false),Some(true)]);
        let before_refresh=std::fs::read(&saved_path).unwrap();
        put(&shared,"refresh-after-save",&runtime.query_observe(&r,rv).unwrap());
        let Received::Capability{boot,mode,observation}=runtime.receive(&r,rv,Mode::Normal,&get(&shared,"refresh-after-save-result").await).unwrap()else{panic!("fresh accepted capability")};
        assert!(accept_presentation(&state,&observation,0,boot,session));retained.accepted(mode,boot,&observation,0);retained.service().await;
        assert_eq!(retained_config::snapshot(Some(mode),true).status,"current");
        assert_eq!(std::fs::read(&saved_path).unwrap(),before_refresh,"fresh unchanged observation does not fsync");
        assert!(!String::from_utf8_lossy(&before_refresh).contains("cross-token"));
        assert!(!String::from_utf8_lossy(&before_refresh).contains("actor"));
        put(&shared,"revoked",&runtime.request(&r,rv,mode,[99;16],"operator".into(),Operation::Command(NAMES[0].into())).unwrap());
        let before_revoked=std::fs::read(&saved_path).unwrap();
        let Received::Result{outcome:Outcome::Denied,boot,observation,..}=runtime.receive(&r,rv,Mode::Normal,&get(&shared,"revoked-result").await).unwrap()else{panic!("revoked actor denied")};
        retained.accepted(mode,boot,&observation,0);retained.idle().await;
        assert_eq!(std::fs::read(&saved_path).unwrap(),before_revoked,"denied command cannot mutate retained configuration");
        runtime.connect(Some(Transport{session:session.wrapping_add(1),sequence:1}));
        assert!(runtime.capability.is_none());
        invalidate_presentation(&state);
        assert_eq!(crate::primary_mirror::board_history_required_scope(&state),None);
        assert_eq!(retained_config::snapshot(Some(mode),false).status,"disconnected");
        retained.shutdown().await;
        let mut restarted=retained_config::Worker::open_at(Arc::new(Pairing::from_file(&root.0.join("rrls.json")).unwrap()),saved_path);restarted.idle().await;
        assert_eq!(retained_config::snapshot(Some(mode),false).settings,[Some(true),Some(false),Some(false),Some(true)]);
        assert_eq!(retained_config::snapshot(Some(Mode::Normal),false).settings,[None;4]);
        restarted.shutdown().await;
        assert!(runtime.request(&r,Transport{session:session.wrapping_add(1),sequence:1},mode,[100;16],"operator".into(),Operation::Command(NAMES[0].into())).is_err(),"reconnect cannot invent primary HITL authority");
        state.request_shutdown();
    }
    println!("CROSS_COMPILED_INTERLOCK_EXCHANGE actual_build={:?} four_authorized_toggles=true duplicate_no_reexecution=true revoked_actor_denied=true reconnect_unknown=true",crate::station_mode::local_mode());
}
