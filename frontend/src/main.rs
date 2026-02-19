mod app;
mod telemetry_dashboard;

use dioxus::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use dioxus_desktop::wry::http::{Request as HttpRequest, Response as HttpResponse};
#[cfg(not(target_arch = "wasm32"))]
use dioxus_desktop::RequestAsyncResponder;
#[cfg(not(target_arch = "wasm32"))]
use std::borrow::Cow;

#[cfg(target_arch = "wasm32")]
fn init_panic_hook() {
    console_error_panic_hook::set_once();
}

#[cfg(not(target_arch = "wasm32"))]
fn init_panic_hook() {}

#[cfg(target_arch = "wasm32")]
fn main() {
    init_panic_hook();

    // Web launch (wasm)
    // You can add assets config here if you want; default is fine.
    launch(app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    init_panic_hook();
    #[cfg(target_os = "windows")]
    let cfg = dioxus_desktop::Config::new()
        .with_custom_protocol("gs26", move |_id, request| handle_gs26_protocol(request));

    #[cfg(not(target_os = "windows"))]
    let cfg = dioxus_desktop::Config::new().with_asynchronous_custom_protocol(
        "gs26",
        |_id, request, responder| {
            _handle_gs26_protocol_async(request, responder);
        },
    );
    LaunchBuilder::desktop().with_cfg(cfg).launch(app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_gs26_protocol(request: HttpRequest<Vec<u8>>) -> HttpResponse<Cow<'static, [u8]>> {
    fn build_response(
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> HttpResponse<Cow<'static, [u8]>> {
        let mut builder = HttpResponse::builder().status(status);
        if let Some(ct) = content_type {
            builder = builder.header("Content-Type", ct);
        }
        builder.body(Cow::Owned(body)).unwrap_or_else(|_| {
            HttpResponse::builder()
                .status(500)
                .body(Cow::Owned(Vec::new()))
                .unwrap()
        })
    }

    let path = request.uri().path();
    let segs: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // Accept either:
    // - /tiles/{z}/{x}/{y}.jpg
    // - /{host}/tiles/{z}/{x}/{y}.jpg
    let parts: &[&str] = if segs.len() >= 4 && segs[segs.len() - 4] == "tiles" {
        &segs[segs.len() - 4..]
    } else {
        &[]
    };

    if parts.len() != 4 || !parts[3].ends_with(".jpg") {
        return build_response(404, None, Vec::new());
    }

    let z = match parts[1].parse::<u32>() {
        Ok(v) => v,
        Err(_) => return build_response(400, None, Vec::new()),
    };
    let x = match parts[2].parse::<u32>() {
        Ok(v) => v,
        Err(_) => return build_response(400, None, Vec::new()),
    };
    let y = match parts[3].trim_end_matches(".jpg").parse::<u32>() {
        Ok(v) => v,
        Err(_) => return build_response(400, None, Vec::new()),
    };

    let base = telemetry_dashboard::persisted_base_http_for_native_io();
    let skip_tls = telemetry_dashboard::persisted_skip_tls_for_base_for_native_io(&base);
    let tile_url = format!("{}/tiles/{z}/{x}/{y}.jpg", base.trim_end_matches('/'));

    let client = match reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(skip_tls)
        .build()
    {
        Ok(c) => c,
        Err(_) => return build_response(500, None, Vec::new()),
    };

    let upstream = match client.get(tile_url).send() {
        Ok(r) => r,
        Err(_) => return build_response(502, None, Vec::new()),
    };

    let status = upstream.status().as_u16();
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = match upstream.bytes() {
        Ok(b) => b.to_vec(),
        Err(_) => return build_response(502, None, Vec::new()),
    };

    build_response(status, content_type.as_deref(), bytes)
}

#[cfg(not(target_arch = "wasm32"))]
fn _handle_gs26_protocol_async(request: HttpRequest<Vec<u8>>, responder: RequestAsyncResponder) {
    let _ = std::thread::Builder::new()
        .name("gs26-proto-req".to_string())
        .spawn(move || {
            let response = handle_gs26_protocol(request);
            responder.respond(response);
        });
}
