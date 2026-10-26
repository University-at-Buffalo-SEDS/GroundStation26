#![allow(non_snake_case)]
mod engine;
use dioxus::prelude::*;
use engine::{Action, Controller, View};
fn main() {
    if std::env::args().any(|a| a == "--benchmark") {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        match runtime.block_on(engine::benchmark()) {
            Ok(ms) => {
                println!(
                    "{}",
                    serde_json::json!({"encoding_ms":ms,"input":"1280x720@30fps,3seconds,H.264+AAC","compute_passed":ms<=2500})
                );
            }
            Err(_) => {
                eprintln!("Encoding test failed. Install FFmpeg with libx264 and AAC support.");
                std::process::exit(1);
            }
        }
        return;
    }
    dioxus::launch(App);
}
#[component]
fn App() -> Element {
    let controller = use_hook(Controller::start);
    let mut view = use_signal(View::default);
    let mut station = use_signal(|| "https://groundstation.local".to_owned());
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut name = use_signal(|| "Transcoding computer".to_owned());
    let mut local_certificate = use_signal(|| false);
    let observer = controller.clone();
    use_future(move || {
        let observer = observer.clone();
        async move {
            loop {
                if let Ok(value) = observer.view.lock() {
                    view.set(value.clone());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    });
    let busy = view().busy;
    let connected = view().connected;
    let passed = view().passed;
    let offered = view().offered;
    let connect = controller.clone();
    let test = controller.clone();
    let offer = controller.clone();
    let disconnect = controller.clone();
    rsx! {style {{r#"*{box-sizing:border-box}body{margin:0;background:#0b1420;color:#e9eff8;font:16px system-ui}main{max-width:760px;padding:28px;margin:auto}label{display:grid;gap:7px;margin:14px 0}input,button{font:inherit;padding:12px;border:1px solid #52667f;border-radius:9px;background:#14243a;color:inherit;min-height:44px}button{cursor:pointer;margin:6px}button:disabled{opacity:.45;cursor:default}.row{display:flex;gap:8px;flex-wrap:wrap}.check{display:flex;align-items:center}section{padding:18px;border:1px solid #30455e;border-radius:12px;margin:18px 0}p{line-height:1.5}input[type=checkbox]{min-height:22px;width:22px}"#}}
    main{h1{"GroundStation Video Transcoder"}p{"Use this computer’s encoding capacity for the station’s cameras. Video and microphone audio remain on the network, separate from crew voice and radio telemetry."}
     section{h2{"Connect"}label{"Station address" input{value:station(),disabled:busy||connected,oninput:move|e|station.set(e.value()),placeholder:"https://groundstation.local"}}label{"Username" input{value:username(),disabled:busy||connected,oninput:move|e|username.set(e.value())}}label{"Password" input{r#type:"password",value:password(),disabled:busy||connected,oninput:move|e|password.set(e.value())}}
      label{class:"check",input{r#type:"checkbox",checked:local_certificate(),disabled:busy||connected,onchange:move|e|local_certificate.set(e.checked())}"Allow this local station’s self-signed certificate"}
      button{disabled:busy||connected,onclick:move|_|{let secret=password();password.set(String::new());let _=connect.tx.send(Action::Connect{station:station(),username:username(),password:secret,local_certificate:local_certificate()});},"Connect"}
      button{disabled:busy||!connected,onclick:move|_|{let _=disconnect.tx.send(Action::Disconnect);},"Disconnect"}
     }
     section{h2{"Qualification"}label{"Host name" input{value:name(),disabled:busy||offered,oninput:move|e|name.set(e.value())}}
      button{disabled:busy||!connected||offered,onclick:move|_|{let _=test.tx.send(Action::Test{name:name()});},"Test compute and bandwidth"}
      if view().encoding_ms>0{p{"Encoding test: {view().encoding_ms} ms · upload {view().upload_bps / 1_000_000} Mb/s · download {view().download_bps / 1_000_000} Mb/s"}}
      if passed{p{"Qualified capacity: {view().capacity} camera(s), H.264/AAC at up to 720p30."}}
      button{disabled:busy||!connected||!passed,onclick:move|_|{let _=offer.tx.send(Action::Offer(!offered));},if offered{"Stop offering this host"}else{"Make available as transcoding host"}}
      p{role:"status","{view().message}"}
     }
     p{"Keep this helper open. Select it in GroundStation’s Stream Manager after the tests pass. If it disconnects, the station falls back to the original camera feeds."}
    }
    }
}
