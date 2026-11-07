//! Manual acceptance controls for isolated synthetic loopback services only.
use super::*;

fn origin(name: &str) -> String {
    let value = std::env::var(name).expect("isolated fixture address required");
    let url = Url::parse(&value).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"), "loopback only");
    value
}
async fn streams(session: &Session) -> serde_json::Value {
    json(session.client.get(endpoint(&session.base, "/api/live_streams").unwrap())
        .bearer_auth(&session.token).send().await.unwrap()).await.unwrap()
}
fn encoded(value: &serde_json::Value) -> bool {
    value["streams"].as_array().unwrap().iter()
        .any(|s| s["url"].as_str().unwrap().contains("/transcoded-"))
}
async fn until(mut f: impl AsyncFnMut() -> bool, budget: Duration) {
    let end = Instant::now() + budget;
    while !f().await {
        assert!(Instant::now() < end, "synthetic acceptance condition timed out");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn administrator() -> Session {
    connect(origin("GS_MEDIA_TEST_ORIGIN"), "administrator".into(),
        "fixture-only-password".into(), false).await.unwrap()
}
async fn select_host(admin: &Session, host: &str) {
    let result = admin.client.post(endpoint(&admin.base,"/api/video/transcode/hosts").unwrap())
        .bearer_auth(&admin.token).json(&serde_json::json!({"host_id":host})).send().await.unwrap();
    assert!(result.status().is_success());
}
#[tokio::test]
#[ignore = "manual current authentication/TLS acceptance; synthetic loopback only"]
async fn actual_auth_and_local_certificate_policy() {
    let plain = origin("GS_MEDIA_TEST_ORIGIN");
    assert!(connect(plain.clone(), "producer".into(), "wrong-password".into(), false).await.is_err());
    for user in ["viewer", "publisher", "operator"] {
        assert!(connect(plain.clone(), user.into(), "fixture-only-password".into(), false).await.is_err(),
            "non-manager cannot become worker");
    }
    let _ = connect(plain,"producer".into(),"fixture-only-password".into(),false).await.unwrap();
    assert!(connect("http://example.invalid".into(),"producer".into(),"unused".into(),false).await.is_err());
    assert!(connect("https://example.invalid".into(),"producer".into(),"unused".into(),true).await.is_err());
    let tls = origin("GS_MEDIA_TLS_ORIGIN");
    assert!(connect(tls.clone(),"producer".into(),"fixture-only-password".into(),false).await.is_err(),
        "self-signed certificate requires explicit local override");
    let _ = connect(tls,"producer".into(),"fixture-only-password".into(),true).await.unwrap();
    println!("PASS actual login, wrong-secret and nonmanager denial, remote plaintext/override refusal, explicit local TLS certificate policy");
}
#[tokio::test]
#[ignore = "manual synthetic real helper controller and live permission revocation"]
async fn actual_controller_qualification_jobs_and_revocation() {
    let admin = administrator().await;
    let controller = Controller::start();
    controller.tx.send(Action::Connect {station:origin("GS_MEDIA_TEST_ORIGIN"),username:"producer".into(),password:"fixture-only-password".into(),local_certificate:false}).unwrap();
    until(async || controller.view.lock().unwrap().connected, Duration::from_secs(15)).await;
    controller.tx.send(Action::Test{name:"Current controller acceptance".into()}).unwrap();
    until(async || controller.view.lock().unwrap().passed, Duration::from_secs(60)).await;
    let view=controller.view.lock().unwrap().clone();
    assert!(view.encoding_ms>0&&view.encoding_ms<=2500);
    assert!(view.upload_bps>=12_000_000&&view.download_bps>=12_000_000);
    assert!((1..=8).contains(&view.capacity));
    assert!(!view.offered, "qualification alone never offers");
    let hosts:serde_json::Value=json(admin.client.get(endpoint(&admin.base,"/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).send().await.unwrap()).await.unwrap();
    let host=hosts["hosts"].as_array().unwrap().iter().find(|h|h["name"]=="Current controller acceptance").unwrap()["id"].as_str().unwrap();
    let before=admin.client.post(endpoint(&admin.base,"/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).json(&serde_json::json!({"host_id":host})).send().await.unwrap();
    assert_eq!(before.status(),reqwest::StatusCode::CONFLICT);
    controller.tx.send(Action::Offer(true)).unwrap();
    until(async || {
        let hosts:serde_json::Value=json(admin.client.get(endpoint(&admin.base,"/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).send().await.unwrap()).await.unwrap();
        hosts["hosts"].as_array().unwrap().iter().any(|h|h["id"]==host&&h["online"]==true)
    },Duration::from_secs(10)).await;
    select_host(&admin,host).await;
    until(async || encoded(&streams(&admin).await),Duration::from_secs(45)).await;
    let jobs=controller.view.lock().unwrap().jobs;
    assert!(jobs>0&&jobs<=view.capacity);
    // Change only the isolated synthetic account file. Restore even on assertion failure.
    struct Restore {path:std::path::PathBuf,data:Vec<u8>}
    impl Drop for Restore {fn drop(&mut self){std::fs::write(&self.path,&self.data).unwrap();}}
    let path=std::path::PathBuf::from(std::env::var_os("GS_MEDIA_TEST_DIR").unwrap()).join("users.json");
    let restore=Restore{data:std::fs::read(&path).unwrap(),path};
    let mut accounts:serde_json::Value=serde_json::from_slice(&restore.data).unwrap();
    let user=accounts["users"].as_array_mut().unwrap().iter_mut().find(|u|u["username"]=="producer").unwrap();
    user["roles"]=serde_json::json!([]);
    std::fs::write(&restore.path,serde_json::to_vec(&accounts).unwrap()).unwrap();
    let revoked=Instant::now();
    until(async || controller.view.lock().unwrap().jobs==0,Duration::from_secs(7)).await;
    until(async || !encoded(&streams(&admin).await),Duration::from_secs(7)).await;
    controller.tx.send(Action::Disconnect).unwrap();
    until(async || !controller.view.lock().unwrap().connected,Duration::from_secs(5)).await;
    drop(restore);
    println!("CONTROLLER_REAL_QUALIFICATION {}",serde_json::json!({"encoding_ms":view.encoding_ms,"upload_bps":view.upload_bps,"download_bps":view.download_bps,"capacity":view.capacity,"actual_jobs":jobs,"revocation_fallback_ms":revoked.elapsed().as_millis(),"network_bytes_each_direction":4*1024*1024}));
}
#[tokio::test]
#[ignore = "manual synthetic heartbeat expiry; original fifteen-second lease"]
async fn actual_missing_heartbeat_falls_back_without_offer_false() {
    let admin=administrator().await;
    let mut session=connect(origin("GS_MEDIA_TEST_ORIGIN"),"producer".into(),"fixture-only-password".into(),false).await.unwrap();
    let report=test(&mut session,"Lease expiry acceptance".into()).await.unwrap();
    assert_eq!(report["passed"],true);
    heartbeat(&session,true).await.unwrap();
    select_host(&admin,session.host.as_deref().unwrap()).await;
    let mut jobs=Vec::new();
    for job in heartbeat(&session,true).await.unwrap().jobs {jobs.push(tokio::spawn(transcode(session.clone(),job)));}
    let started=Instant::now();
    let mut first_encoded=None;
    while !encoded(&streams(&admin).await) {
        assert!(started.elapsed()<Duration::from_secs(45));
        heartbeat(&session,true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _=heartbeat(&session,true).await.unwrap();
    let last=Instant::now();
    while last.elapsed()<Duration::from_secs(12) {
        assert!(encoded(&streams(&admin).await),"fallback must not be invented before original lease");
        first_encoded.get_or_insert(last.elapsed().as_millis());
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    until(async || !encoded(&streams(&admin).await),Duration::from_secs(6)).await;
    let elapsed=last.elapsed();assert!(elapsed>=Duration::from_secs(15)&&elapsed<Duration::from_secs(18));
    for job in jobs {job.abort();let _=job.await;}
    println!("PASS actual omitted-heartbeat original15s fallback {}ms; no Offer(false) or synthetic last_seen rewrite",elapsed.as_millis());
}

#[path = "finite_pipeline_tests.rs"]
mod finite_pipeline_tests;
