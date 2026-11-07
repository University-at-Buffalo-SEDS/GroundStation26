#![allow(non_snake_case)]
mod engine;
mod native_close_gate;
use dioxus::prelude::*;
use engine::{Action, Controller, ShutdownPoll, View};
use native_close_gate::NativeCloseGate;
use std::process::ExitCode;
fn main() -> ExitCode {
    if std::env::args().any(|a| a == "--benchmark") {
        let (controller, mut owner) = Controller::start_benchmark_owned();
        let result = wait_for_join(|| owner.poll_shutdown(), true);
        drop(controller);
        return result;
    }
    // Create the menu before starting the worker: a menu construction error owns no child work.
    let config = dioxus::desktop::Config::new()
        .with_close_behaviour(dioxus::desktop::WindowCloseBehaviour::WindowHides);
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    let config = match native_close_gate::gated_menu() {
        Ok(menu) => config.with_menu(menu),
        Err(_) => {
            eprintln!("Unable to create the application menu.");
            return ExitCode::FAILURE;
        }
    };
    let (controller, owner) = Controller::start_owned();
    let gate = NativeCloseGate::new(controller, owner);
    let pre_default = gate.clone();
    let config = config.with_custom_event_handler(move |event, _| {
        if matches!(
            event,
            dioxus::desktop::tao::event::Event::WindowEvent {
                event: dioxus::desktop::tao::event::WindowEvent::CloseRequested,
                ..
            }
        ) {
            pre_default.request_close();
        }
    });
    dioxus::LaunchBuilder::new()
        .with_cfg(config)
        .with_context(gate.clone())
        .launch(App);
    // Desktop normally never returns. Retain ownership if another supported launcher does.
    gate.request_close();
    wait_for_join(|| gate.poll_shutdown().expect("shutdown requested"), false)
}

/// Observe an owned worker without blocking on join or treating an observation timeout as cleanup.
/// Local uncertainty keeps the process and the unique join capability alive for further observation.
fn wait_for_join(mut poll: impl FnMut() -> ShutdownPoll, benchmark: bool) -> ExitCode {
    let mut reported = None;
    loop {
        match poll() {
            ShutdownPoll::Pending => {}
            ShutdownPoll::LocalUncertain(code) => {
                if reported != Some(code) {
                    eprintln!("Local cleanup is unresolved ({code}); the worker remains owned.");
                    reported = Some(code);
                }
            }
            ShutdownPoll::Joined(report) => {
                let mut failed = false;
                if let Some(code) = report.remote_uncertain() {
                    eprintln!("Worker joined; remote completion remains unconfirmed ({code}).");
                    failed = true;
                }
                if let Some(code) = report.operational_failure() {
                    eprintln!("Worker joined after an operational failure ({code}).");
                    if benchmark {
                        eprintln!(
                            "Encoding test failed. Install FFmpeg with libx264 and AAC support."
                        );
                    }
                    failed = true;
                }
                if failed {
                    return ExitCode::FAILURE;
                }
                if benchmark {
                    let Some(ms) = report.benchmark_ms() else {
                        eprintln!("Encoding test did not produce a completed benchmark.");
                        return ExitCode::FAILURE;
                    };
                    println!(
                        "{}",
                        serde_json::json!({"encoding_ms":ms,"input":"1280x720@30fps,3seconds,H.264+AAC","compute_passed":ms<=2500})
                    );
                }
                return ExitCode::SUCCESS;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
#[component]
fn App() -> Element {
    let gate = use_context::<NativeCloseGate>();
    let controller = gate.controller();
    let window = dioxus::desktop::use_window();
    let mut closing_notice = use_signal(|| None::<String>);
    let close_events = gate.clone();
    let restore_window = window.clone();
    dioxus::desktop::use_wry_event_handler(move |event, _| {
        // The framework hides after CloseRequested. Restore on the next event-loop boundary;
        // keep the VDOM and its owner alive while cleanup is pending or locally uncertain.
        if matches!(event, dioxus::desktop::tao::event::Event::MainEventsCleared)
            && close_events.is_requested()
            && !restore_window.is_visible()
        {
            restore_window.set_visible(true);
        }
    });
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    {
        let menu_gate = gate.clone();
        dioxus::desktop::use_muda_event_handler(move |event| {
            if event.id.0 == native_close_gate::CLOSE_MENU_ID
                || event.id.0 == native_close_gate::QUIT_MENU_ID
            {
                menu_gate.request_close();
            }
        });
    }
    let mut view = use_signal(View::default);
    let mut station = use_signal(|| "https://groundstation.local".to_owned());
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut name = use_signal(|| "Transcoding computer".to_owned());
    let mut local_certificate = use_signal(|| false);
    let observer = controller.clone();
    let shutdown = gate.clone();
    use_future(move || {
        let observer = observer.clone();
        let shutdown = shutdown.clone();
        let window = window.clone();
        async move {
            loop {
                if let Some(outcome) = shutdown.poll_shutdown() {
                    let message = match outcome {
                        ShutdownPoll::Pending => {
                            "Finishing local processing before closing…".to_owned()
                        }
                        ShutdownPoll::LocalUncertain(code) => format!(
                            "Local cleanup is unresolved ({code}). This window will remain open."
                        ),
                        ShutdownPoll::Joined(report) => {
                            if let Some(code) = report.remote_uncertain() {
                                eprintln!(
                                    "Worker joined; remote completion remains unconfirmed ({code})."
                                );
                            }
                            if let Some(code) = report.operational_failure() {
                                eprintln!("Worker joined after an operational failure ({code}).");
                            }
                            window.set_close_behavior(
                                dioxus::desktop::WindowCloseBehaviour::WindowCloses,
                            );
                            window.close();
                            return;
                        }
                    };
                    if closing_notice.peek().as_ref() != Some(&message) {
                        closing_notice.set(Some(message));
                    }
                    if !window.is_visible() {
                        window.set_visible(true);
                    }
                }
                if let Ok(value) = observer.view.lock() {
                    view.set(value.clone());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    });
    let busy = view().busy || gate.is_requested();
    let connected = view().connected;
    let passed = view().passed;
    let offered = view().offered;
    let connect = controller.clone();
    let test = controller.clone();
    let offer = controller.clone();
    let disconnect = controller.clone();
    rsx! {style {{r#"*{box-sizing:border-box}body{margin:0;background:#0b1420;color:#e9eff8;font:16px system-ui}main{max-width:760px;padding:28px;margin:auto}label{display:grid;gap:7px;margin:14px 0}input,button{font:inherit;padding:12px;border:1px solid #52667f;border-radius:9px;background:#14243a;color:inherit;min-height:44px}button{cursor:pointer;margin:6px}button:disabled{opacity:.45;cursor:default}.row{display:flex;gap:8px;flex-wrap:wrap}.check{display:flex;align-items:center}section{padding:18px;border:1px solid #30455e;border-radius:12px;margin:18px 0}p{line-height:1.5}input[type=checkbox]{min-height:22px;width:22px}"#}}
    main{h1{"GroundStation Video Transcoder"}p{"Use this computer’s encoding capacity for the station’s cameras. Video and microphone audio remain on the network, separate from crew voice and radio telemetry."}
     if let Some(message) = closing_notice() {p{role:"alert","{message}"}}
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
