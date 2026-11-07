//! Genuine native worker/child ownership controls. Each parent uses a fresh child process.
use super::*;
use std::io::Read;
#[cfg(target_os="linux")]
use std::path::{Path, PathBuf};

pub(super) fn in_isolated_child(name: &str) -> bool {
    let name=name.split_once("::").map_or(name,|(_,path)|path);
    if std::env::var("GS_OWNED_WORKER_CASE").as_deref() == Ok(name) { return true; }
    let path=std::env::temp_dir().join(format!("owned-worker-{}-{}.log",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let output=std::fs::File::create(&path).unwrap();
    let mut child=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact",name,"--include-ignored","--test-threads=1","--nocapture"])
        .env("GS_OWNED_WORKER_CASE",name).stdin(Stdio::null()).stdout(output.try_clone().unwrap()).stderr(output).spawn().unwrap();
    let deadline=Instant::now()+Duration::from_secs(90);
    let status=loop {
        if let Some(status)=child.try_wait().unwrap(){break status;}
        if Instant::now()>=deadline {let _=child.kill();let _=child.wait();panic!("owned-worker child exceeded90s");}
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut text=String::new();std::fs::File::open(&path).unwrap().take(1024*1024).read_to_string(&mut text).unwrap();
    let _=std::fs::remove_file(&path);
    print!("{text}");
    assert!(status.success(),"actual owned-worker child failed");
    assert!(text.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    false
}
fn joined(owner:&mut WorkerOwner,seconds:u64)->JoinedReport {
    let end=Instant::now()+Duration::from_secs(seconds);
    loop {
        match owner.poll_shutdown(){
            ShutdownPoll::Joined(report)=>return report,
            ShutdownPoll::Pending=>{},
            ShutdownPoll::LocalUncertain(reason)=>panic!("local cleanup unverified: {reason}"),
        }
        assert!(Instant::now()<end,"actual native join was not observed");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn idle_native_shutdown_joins_before_controller_clones_drop() {
    if !in_isolated_child(concat!(module_path!(),"::idle_native_shutdown_joins_before_controller_clones_drop")){return;}
    let (controller,mut owner)=Controller::start_owned();
    let observer=controller.clone();let button=controller.clone();
    controller.request_shutdown();controller.request_shutdown();
    let _=button.tx.send(Action::Connect{station:"not-a-station".into(),username:"unused".into(),password:"unused".into(),local_certificate:false});
    let report=joined(&mut owner,5);
    assert!(owner.thread.is_none());
    assert!(report.remote_uncertain().is_none()&&report.operational_failure().is_none());
    assert!(matches!(owner.poll_shutdown(),ShutdownPoll::Joined(_)));
    assert!(!observer.view.lock().unwrap().connected);
    println!("ACTUAL_OWNED_WORKER {}",serde_json::json!({"case":"idle","actual_std_join":true,"extra_controller_clones_alive":true,"postseal_connect_not_committed":true,"repeated_poll_joined":true}));
}
struct ReleaseBarrier(Arc<(Mutex<bool>,std::sync::Condvar)>);
impl Drop for ReleaseBarrier {
    fn drop(&mut self){let(lock,wake)=&*self.0;*lock.lock().unwrap()=true;wake.notify_all();}
}
#[test]
fn pending_native_join_keeps_exact_handle_until_real_thread_release() {
    if !in_isolated_child(concat!(module_path!(),"::pending_native_join_keeps_exact_handle_until_real_thread_release")){return;}
    let barrier=Arc::new((Mutex::new(false),std::sync::Condvar::new()));
    let(reached_tx,reached_rx)=std::sync::mpsc::channel();
    assert!(BEFORE_THREAD_RETURN.set(BeforeReturnProbe{barrier:barrier.clone(),reached:reached_tx}).is_ok());
    let release=ReleaseBarrier(barrier);
    let (controller,mut owner)=Controller::start_owned();
    let id=owner.thread.as_ref().unwrap().thread().id();
    controller.request_shutdown();
    reached_rx.recv_timeout(Duration::from_secs(5)).expect("actual worker reached post-runtime native-return boundary");
    let end=Instant::now()+Duration::from_millis(200);
    while Instant::now()<end {
        assert!(matches!(owner.poll_shutdown(),ShutdownPoll::Pending));
        assert_eq!(owner.thread.as_ref().unwrap().thread().id(),id);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(release);
    let _=joined(&mut owner,5);
    assert!(owner.thread.is_none());
    println!("ACTUAL_OWNED_WORKER {}",serde_json::json!({"case":"held-native-return","observer_deadline_pending":true,"same_handle_retained":true,"actual_join_after_release":true}));
}
#[test]
fn self_join_is_refused_without_consuming_actual_native_handle() {
    if !in_isolated_child(concat!(module_path!(),"::self_join_is_refused_without_consuming_actual_native_handle")){return;}
    let(to_worker,from_main)=std::sync::mpsc::channel();let(to_main,from_worker)=std::sync::mpsc::channel();
    assert!(SELF_JOIN_PROBE.set(SelfJoinProbe{owner:Mutex::new(from_main),returned:to_main}).is_ok());
    let(controller,owner)=Controller::start_owned();controller.request_shutdown();
    to_worker.send(owner).unwrap_or_else(|_|panic!("send actual owner to worker failed"));
    let(mut owner,refused)=from_worker.recv_timeout(Duration::from_secs(5)).unwrap();assert!(refused);
    let _=joined(&mut owner,5);assert!(owner.thread.is_none());
    println!("ACTUAL_OWNED_WORKER {}",serde_json::json!({"case":"actual-selfjoin","self_join_refused":true,"same_native_handle_returned":true,"external_actual_join":true}));
}
#[test]
fn unexpected_owner_drop_retains_unclaimed_join_capability_and_blocks_new_worker() {
    if !in_isolated_child(concat!(module_path!(),"::unexpected_owner_drop_retains_unclaimed_join_capability_and_blocks_new_worker")){return;}
    let (controller,owner)=Controller::start_owned();
    drop(owner);
    assert!(controller.shutdown.is_cancelled());
    assert_eq!(unclaimed().lock().unwrap().len(),1);
    let (_blocked,mut blocked_owner)=Controller::start_owned();
    assert!(matches!(blocked_owner.poll_shutdown(),ShutdownPoll::LocalUncertain("video_processing_prior_worker_unjoined")));
    assert!(blocked_owner.thread.is_none());
    // Test-only reclamation observes the actual retained native join. It never
    // publishes a Clean/Joined production report for the lost-owner path.
    let retained=unclaimed().lock().unwrap().pop().unwrap();
    let end=Instant::now()+Duration::from_secs(5);
    while !retained._thread.is_finished(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}
    retained._thread.join().unwrap();
    println!("ACTUAL_OWNED_WORKER {}",serde_json::json!({"case":"unexpected-owner-drop","native_join_capability_retained":true,"new_worker_blocked":true,"no_successful_terminal_report":true}));
}
#[cfg(target_os="linux")]
fn private_loopback() {
    assert_eq!(std::env::var("GS_VIDEO_COMPLETION_TEST").as_deref(),Ok("private-loopback-only"));
    assert!(std::env::var_os("RUST_MIN_STACK").is_none());
    assert_eq!(std::fs::read_to_string("/proc/net/dev").unwrap().lines().skip(2).map(|line|line.split_once(':').unwrap().0.trim().to_owned()).collect::<Vec<_>>(),vec!["lo"]);
}
#[cfg(target_os="linux")]
fn benchmark_case(timeout:bool) {
    private_loopback();
    let (tx,rx)=std::sync::mpsc::channel();assert!(BENCHMARK_PROBE.set(BenchmarkProbe{spawned:tx,stop_owned:true}).is_ok());
    let start=Instant::now();let(controller,mut owner)=Controller::start_benchmark_owned();
    let pid=rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let stop_observed=Instant::now()+Duration::from_secs(5);
    while !std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap().lines().any(|line|line.starts_with("State:")&&line.contains('T')) {
        assert!(Instant::now()<stop_observed,"owned child did not enter actual stopped state");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap().lines().any(|line|line.starts_with("State:")&&line.contains('T')));
    if !timeout{controller.request_shutdown();}
    let report=joined(&mut owner,40);
    assert!(report.operational_failure().is_some()&&report.benchmark_ms().is_none());
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    let elapsed=start.elapsed().as_millis();
    if timeout{assert!((29_500..40_000).contains(&elapsed));}else{assert!(elapsed<10_000);}
    println!("ACTUAL_OWNED_BENCHMARK {}",serde_json::json!({"timeout":timeout,"actual_owned_pid_stopped":true,"actual_pid_reaped":true,"actual_std_join":true,"elapsed_ms":elapsed,"production_deadline_seconds":30,"local_settled_before_join_report":true}));
}
#[test]
#[cfg(target_os="linux")]
#[ignore="manual actual owned FFmpeg cancellation; isolated privateNET only"]
fn actual_benchmark_cancel_kills_waits_and_native_joins() {
    if !in_isolated_child(concat!(module_path!(),"::actual_benchmark_cancel_kills_waits_and_native_joins")){return;}
    benchmark_case(false);
}
#[test]
#[cfg(target_os="linux")]
#[ignore="manual unchanged real30s benchmark timeout; isolated privateNET only"]
fn actual_benchmark_timeout_uses_real30s_then_kill_wait_and_native_join() {
    if !in_isolated_child(concat!(module_path!(),"::actual_benchmark_timeout_uses_real30s_then_kill_wait_and_native_join")){return;}
    benchmark_case(true);
}

#[test]
#[cfg(target_os="linux")]
#[ignore="manual actual MediaMTX/input/auth/proxy/owned native worker shutdown; private loopback only"]
fn actual_active_media_shutdown_settles_children_proxy_and_native_thread() {
    if !in_isolated_child(concat!(module_path!(),"::actual_active_media_shutdown_settles_children_proxy_and_native_thread")){return;}
    active_media_case(false);
}
#[test]
#[cfg(target_os="linux")]
#[ignore="manual real15s proxy observation timeout with actual retained task and known remote ack"]
fn actual_proxy_pending_retains_task_and_local_uncertainty_despite_matching_remote_ack() {
    if !in_isolated_child(concat!(module_path!(),"::actual_proxy_pending_retains_task_and_local_uncertainty_despite_matching_remote_ack")){return;}
    active_media_case(true);
}
#[cfg(target_os="linux")]
fn active_media_case(hold_proxy:bool) {
    private_loopback();
    struct ReleaseNotify(Arc<tokio::sync::Notify>);
    impl Drop for ReleaseNotify {fn drop(&mut self){self.0.notify_one();}}
    let release=if hold_proxy {
        let barrier=Arc::new(tokio::sync::Notify::new());assert!(PROXY_RETURN_BARRIER.set(barrier.clone()).is_ok());Some(ReleaseNotify(barrier))
    }else{None};
    let(ack_tx,ack_rx)=std::sync::mpsc::channel();assert!(STOP_ACK_PROBE.set(ack_tx).is_ok());
    let runtime=tokio::runtime::Runtime::new().unwrap();
    let origin=std::env::var("GS_MEDIA_TEST_ORIGIN").unwrap();
    let admin=runtime.block_on(connect(origin.clone(),"administrator".into(),"fixture-only-password".into(),false)).unwrap();
    let(controller,mut owner)=Controller::start_owned();
    controller.tx.send(Action::Connect{station:origin,username:"producer".into(),password:"fixture-only-password".into(),local_certificate:false}).unwrap();
    wait_view_sync(&controller.view,|v|v.connected&&!v.busy,15);
    controller.tx.send(Action::Test{name:"Owned native shutdown".into()}).unwrap();
    wait_view_sync(&controller.view,|v|v.passed&&!v.busy,65);
    controller.tx.send(Action::Offer(true)).unwrap();
    let host=runtime.block_on(async{
        let end=Instant::now()+Duration::from_secs(10);
        loop{
            let rows:serde_json::Value=json(admin.client.get(endpoint(&admin.base,"/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).send().await.unwrap()).await.unwrap();
            if let Some(host)=rows["hosts"].as_array().unwrap().iter().find(|h|h["name"]=="Owned native shutdown"&&h["online"]==true){break host["id"].as_str().unwrap().to_owned();}
            assert!(Instant::now()<end);tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    runtime.block_on(async {let response=admin.client.post(endpoint(&admin.base,"/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).json(&serde_json::json!({"host_id":host})).send().await.unwrap();assert!(response.status().is_success());});
    wait_view_sync(&controller.view,|v|v.jobs>0,10);
    let owned=PathBuf::from(std::env::var_os("GS_MEDIA_TEST_DIR").unwrap()).join("owned-native-audience");
    let decoded=runtime.block_on(async {
        let end=Instant::now()+Duration::from_secs(50);
        loop{
            if let Ok((snapshot,_))=qualification_tests::finite_pipeline_tests::audience_snapshot(&admin,&owned).await{
                break qualification_tests::finite_pipeline_tests::decode_audience(snapshot).unwrap();
            }
            assert!(Instant::now()<end);tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });assert_eq!(decoded,(5,5));
    let pids=qualification_tests::finite_pipeline_tests::actual_media_processes();assert!(pids.len()>=2);
    let observer=controller.clone();let stopping=Instant::now();controller.request_shutdown();
    if hold_proxy {
        let end=Instant::now()+Duration::from_secs(25);
        loop{
            match owner.poll_shutdown(){
                ShutdownPoll::Pending=>{},
                ShutdownPoll::LocalUncertain("video_processing_proxy_cleanup_pending")=>break,
                _=>panic!("unsettled proxy must not produce a Joined report"),
            }
            assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));
        }
        assert!(stopping.elapsed()>=Duration::from_millis(14_500));
        assert!(owner.thread.is_some()&&owner.local.active.load(Ordering::SeqCst)>0);
        assert!(ack_rx.recv_timeout(Duration::from_secs(5)).unwrap(),"actual originalgeneration remote ack required while local task still pending");
        assert!(matches!(owner.poll_shutdown(),ShutdownPoll::LocalUncertain("video_processing_proxy_cleanup_pending")));
    }
    drop(release);
    let report=if hold_proxy {
        let deadline=Instant::now()+Duration::from_secs(45);
        loop {
            match owner.poll_shutdown(){
                ShutdownPoll::Joined(report)=>break report,
                ShutdownPoll::Pending|ShutdownPoll::LocalUncertain("video_processing_proxy_cleanup_pending")=>{},
                ShutdownPoll::LocalUncertain(reason)=>panic!("unexpected local uncertainty after release: {reason}"),
            }
            assert!(Instant::now()<deadline,"released original proxy must settle and native thread actually join");
            std::thread::sleep(Duration::from_millis(10));
        }
    }else{joined(&mut owner,45)};
    if hold_proxy{assert!(report.remote_uncertain().is_some()&&report.operational_failure().is_some());}
    else{assert!(report.remote_uncertain().is_none());}
    assert!(pids.iter().all(|pid|!Path::new(&format!("/proc/{pid}")).exists()));
    assert!(observer.tx.send(Action::Offer(true)).is_err());
    println!("ACTUAL_OWNED_ACTIVE_MEDIA {}",serde_json::json!({"decoded_video_frames":5,"decoded_audio_frames":5,"local_media_pids_reaped":true,"authenticated_original_generation_ack":true,"held_proxy_timeout":hold_proxy,"local_pending_task_retained_until_release":hold_proxy,"remote_uncertainty_explicit_after_delayed_local_settlement":report.remote_uncertain().is_some(),"actual_std_join":true,"controller_clone_alive":true,"postseal_reoffer_rejected":true,"scope":"production native worker API/manual hook, NOT actual OS window close"}));
}
#[cfg(target_os="linux")]
fn wait_view_sync(view:&Arc<Mutex<View>>,ready:impl Fn(&View)->bool,seconds:u64){
    let end=Instant::now()+Duration::from_secs(seconds);
    loop{if ready(&view.lock().unwrap()){return;}assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(20));}
}
