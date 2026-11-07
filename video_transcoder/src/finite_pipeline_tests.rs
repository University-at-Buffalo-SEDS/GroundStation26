//! Actual controller and audience-path decoding; only owned loopback fixtures.
use super::*;
use std::path::{Path, PathBuf};

async fn wait_view(view: &Arc<Mutex<View>>, ready: impl Fn(&View) -> bool, seconds: u64) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let snapshot = view.lock().unwrap().clone();
        if ready(&snapshot) { return; }
        assert!(Instant::now() < deadline, "actual controller condition timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn owned_get(session: &Session, address: &Url) -> Result<Vec<u8>> {
    anyhow::ensure!(address.origin() == session.base.origin(), "fixture crossed origin");
    let response = session.client.get(address.clone()).bearer_auth(&session.token).send().await?;
    anyhow::ensure!(response.status().is_success(), "audience path not yet available");
    let data = response.bytes().await?;
    anyhow::ensure!(data.len() <= 8 * 1024 * 1024, "bounded audience object");
    Ok(data.to_vec())
}

fn uri_in_line(line: &str) -> Option<&str> {
    if line.starts_with("#EXT-X-MAP:") || line.starts_with("#EXT-X-MEDIA:") {
        line.split_once("URI=\"").and_then(|(_, rest)| rest.split('"').next())
    } else if !line.is_empty() && !line.starts_with('#') { Some(line) } else { None }
}

async fn released_media(session: &Session, address: &Url, destination: &Path, prefix: &str, total: &mut usize) -> Result<(PathBuf, usize)> {
    let playlist = String::from_utf8(owned_get(session, address).await?)?;
    anyhow::ensure!(playlist.starts_with("#EXTM3U") && playlist.contains("#EXTINF:"), "no released audience segments");
    let mut rewritten = String::new();
    let mut objects = 0;
    for line in playlist.lines() {
        if let Some(reference) = uri_in_line(line) {
            anyhow::ensure!(objects < 24, "bounded audience segment count");
            let data = owned_get(session, &address.join(reference)?).await?;
            *total += data.len();
            anyhow::ensure!(*total <= 24 * 1024 * 1024, "bounded audience bytes");
            let filename = format!("{prefix}-object-{objects}.mp4");
            std::fs::write(destination.join(&filename), data)?;
            rewritten.push_str(&line.replace(reference, &filename));
            objects += 1;
        } else { rewritten.push_str(line); }
        rewritten.push('\n');
    }
    anyhow::ensure!(objects >= 2, "audience map and media required");
    rewritten.push_str("#EXT-X-ENDLIST\n");
    let path = destination.join(format!("{prefix}.m3u8"));
    std::fs::write(&path, rewritten)?;
    Ok((path, objects))
}

async fn audience_snapshot(session: &Session, destination: &Path) -> Result<(PathBuf, usize)> {
    let address = endpoint(&session.base, "/api/media-assets/hls/transcoded-front/index.m3u8")?;
    let master = String::from_utf8(owned_get(session, &address).await?)?;
    std::fs::create_dir_all(destination)?;
    let mut total = 0;
    if master.contains("#EXTINF:") { return released_media(session, &address, destination, "muxed", &mut total).await; }
    anyhow::ensure!(master.starts_with("#EXTM3U") && master.contains("#EXT-X-STREAM-INF:"), "actual master required");
    let mut rewritten = String::new();
    let mut renditions = 0;
    let mut objects = 0;
    for line in master.lines() {
        if let Some(reference) = uri_in_line(line) {
            anyhow::ensure!(renditions < 4, "bounded audience rendition graph");
            let prefix = format!("rendition-{renditions}");
            let (path, count) = released_media(session, &address.join(reference)?, destination, &prefix, &mut total).await?;
            rewritten.push_str(&line.replace(reference, path.file_name().unwrap().to_str().unwrap()));
            renditions += 1;
            objects += count;
        } else { rewritten.push_str(line); }
        rewritten.push('\n');
    }
    anyhow::ensure!(renditions > 0, "actual rendition required");
    let path = destination.join("released-master.m3u8");
    std::fs::write(&path, rewritten)?;
    Ok((path, objects))
}

fn run_decode(program: &str, args: &[&str], output: &Path) -> Result<()> {
    let file = std::fs::File::create(output)?;
    let mut command = std::process::Command::new(program);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.args(args).stdin(Stdio::null()).stdout(file).stderr(Stdio::null()).spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(status.success(), "actual decoder failed");
            return Ok(());
        }
        if Instant::now() >= deadline {
            let killed = child.kill();
            let waited = child.wait();
            anyhow::bail!("decoder timeout cleanup kill={} wait={}", killed.is_ok(), waited.is_ok());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn decode_audience(path: PathBuf) -> Result<(usize, usize)> {
    let folder = path.parent().context("audience folder")?;
    let input = path.to_str().context("audience path UTF8")?;
    let probe = folder.join("probe.json");
    run_decode("ffprobe", &["-v", "quiet", "-show_entries", "stream=codec_name,codec_type", "-of", "json", input], &probe)?;
    let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(probe)?)?;
    let streams = metadata["streams"].as_array().context("actual stream metadata")?;
    anyhow::ensure!(streams.iter().any(|s| s["codec_type"] == "video" && s["codec_name"] == "h264"), "actual H264 video required");
    anyhow::ensure!(streams.iter().any(|s| s["codec_type"] == "audio" && s["codec_name"] == "aac"), "actual AAC audio required");
    let video = folder.join("video.framemd5");
    let audio = folder.join("audio.framemd5");
    run_decode("ffmpeg", &["-v", "quiet", "-nostdin", "-i", input, "-map", "0:v:0", "-frames:v", "5", "-f", "framemd5", "pipe:1"], &video)?;
    run_decode("ffmpeg", &["-v", "quiet", "-nostdin", "-i", input, "-map", "0:a:0", "-frames:a", "5", "-f", "framemd5", "pipe:1"], &audio)?;
    let count = |file: &Path| -> Result<usize> { Ok(std::fs::read_to_string(file)?.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).count()) };
    let frames = (count(&video)?, count(&audio)?);
    anyhow::ensure!(frames == (5, 5), "five actually decoded video and audio frames required");
    Ok(frames)
}

fn frame_hashes(path: &Path) -> Vec<String> {
    let frames: Vec<_> = std::fs::read_to_string(path).unwrap().lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.rsplit(',').next().unwrap().trim().to_owned()).collect();
    assert_eq!(frames.len(), 5);
    assert!(frames.iter().all(|hash|hash.len()==32 && hash.bytes().all(|byte|byte.is_ascii_hexdigit())));
    frames
}
fn actual_media_processes() -> HashSet<u32> {
    let source: u32 = std::env::var("GS_MEDIA_TEST_SOURCE_PID").expect("owned fixture source pid required").parse().unwrap();
    assert!(Path::new(&format!("/proc/{source}")).exists());
    std::fs::read_dir("/proc").unwrap().filter_map(|entry| {
        let entry = entry.ok()?;
        let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
        (pid != source && std::fs::read_to_string(entry.path().join("comm")).ok()?.trim() == "ffmpeg").then_some(pid)
    }).collect()
}

#[tokio::test]
#[ignore = "manual actual finite loopback MediaMTX/controller/audience decode"]
async fn actual_controller_to_audience_decoded_h264_aac_and_stop() {
    let origin = std::env::var("GS_MEDIA_TEST_ORIGIN").unwrap();
    assert_eq!(Url::parse(&origin).unwrap().host_str(), Some("127.0.0.1"));
    let admin = connect(origin.clone(), "administrator".into(), "fixture-only-password".into(), false).await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let view = Arc::new(Mutex::new(View::default()));
    let task = tokio::spawn(control(rx, view.clone()));
    tx.send(Action::Connect { station: origin, username: "producer".into(), password: "fixture-only-password".into(), local_certificate: false }).unwrap();
    wait_view(&view, |v| v.connected && !v.busy, 15).await;
    tx.send(Action::Test { name: "Finite actual worker".into() }).unwrap();
    wait_view(&view, |v| !v.busy && (v.passed || v.message.starts_with("Test failed") || v.message.starts_with("Insufficient")), 65).await;
    let qualified = view.lock().unwrap().clone();
    assert!(qualified.passed && qualified.encoding_ms > 0 && qualified.encoding_ms <= 2500);
    assert!(qualified.upload_bps >= 12_000_000 && qualified.download_bps >= 12_000_000);
    tx.send(Action::Offer(true)).unwrap();
    wait_view(&view, |v| v.offered, 5).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let host = loop {
        let response = admin.client.get(endpoint(&admin.base, "/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).send().await.unwrap();
        let hosts: serde_json::Value = json(response).await.unwrap();
        if let Some(host) = hosts["hosts"].as_array().unwrap().iter().find(|h| h["name"] == "Finite actual worker" && h["online"] == true) { break host["id"].as_str().unwrap().to_owned(); }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let selected = admin.client.post(endpoint(&admin.base, "/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).json(&serde_json::json!({"host_id":host})).send().await.unwrap();
    assert!(selected.status().is_success());
    wait_view(&view, |v| v.jobs > 0, 10).await;
    let owned = PathBuf::from(std::env::var_os("GS_MEDIA_TEST_DIR").unwrap()).join("decoded-audience");
    let deadline = Instant::now() + Duration::from_secs(50);
    let (snapshot, objects) = loop {
        if let Ok(result) = audience_snapshot(&admin, &owned).await { break result; }
        assert!(Instant::now() < deadline, "actual released program media never arrived");
        assert!(view.lock().unwrap().offered, "actual processing stopped before audience decode");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let decoded = tokio::task::spawn_blocking(move || decode_audience(snapshot)).await.unwrap().unwrap();
    let first_video = frame_hashes(&owned.join("video.framemd5"));
    let first_processes = actual_media_processes();
    assert!(first_processes.len() >= 2, "actual local encoder and station remux must exist");
    tx.send(Action::Offer(false)).unwrap();
    wait_view(&view, |v| !v.offered && !v.busy && v.jobs == 0 && v.message.contains("acknowledged remote cleanup"), 15).await;
    assert!(first_processes.iter().all(|pid| !Path::new(&format!("/proc/{pid}")).exists()), "acknowledged Stop must have reaped old local and remote media children");
    assert!(view.lock().unwrap().passed, "safe Stop retains qualification without a helper restart");
    tx.send(Action::Offer(true)).unwrap();
    wait_view(&view, |v| v.offered && v.jobs > 0, 10).await;
    let second_owned = PathBuf::from(std::env::var_os("GS_MEDIA_TEST_DIR").unwrap()).join("decoded-reoffered-audience");
    let deadline = Instant::now() + Duration::from_secs(50);
    let (second_objects, second_decoded) = loop {
        if let Ok((snapshot, count)) = audience_snapshot(&admin, &second_owned).await
            && let Ok(frames) = tokio::task::spawn_blocking(move || decode_audience(snapshot)).await.unwrap()
            && frame_hashes(&second_owned.join("video.framemd5")) != first_video {
            break (count, frames);
        }
        assert!(Instant::now() < deadline, "fresh reoffered audience media must actually decode");
        assert!(view.lock().unwrap().offered);
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let second_processes = actual_media_processes();
    assert!(second_processes.len() >= 2 && second_processes.is_disjoint(&first_processes));
    tx.send(Action::Offer(false)).unwrap();
    wait_view(&view, |v| !v.offered && !v.busy && v.jobs == 0 && v.message.contains("acknowledged remote cleanup"), 15).await;
    assert!(second_processes.iter().all(|pid| !Path::new(&format!("/proc/{pid}")).exists()));
    tx.send(Action::Disconnect).unwrap();
    wait_view(&view, |v| !v.connected && !v.busy, 10).await;
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap();
    println!("ACTUAL_FINITE_HELPER_PROGRAM {}", serde_json::json!({"actual_compute_ms":qualified.encoding_ms,"actual_upload_bps":qualified.upload_bps,"actual_download_bps":qualified.download_bps,"actual_audience_objects":objects,"decoded_video_frames":decoded.0,"decoded_audio_frames":decoded.1,"reoffered_audience_objects":second_objects,"reoffered_decoded_video_frames":second_decoded.0,"reoffered_decoded_audio_frames":second_decoded.1,"codecs":["h264","aac"],"actual_stop_and_disconnect":true,"acknowledged_stop_and_same_controller_reoffer":true,"old_media_children_reaped_before_reoffer":true,"fresh_media_processes_disjoint":true,"fresh_video_frame_bytes":true,"controller_task_joined":true,"scope":"owned finite source, actual controller/remote generation acknowledgement, two audience decode cycles; no GUI/window-exit or physical platform claim"}));

}

#[tokio::test]
#[ignore = "manual actual finite valid upload natural completion and audience decode"]
async fn actual_finite_valid_upload_decodes_and_returns_204_after_encoder_exit0() {
    let origin = std::env::var("GS_MEDIA_TEST_ORIGIN").unwrap();
    assert_eq!(Url::parse(&origin).unwrap().host_str(), Some("127.0.0.1"));
    let admin = connect(origin.clone(), "administrator".into(), "fixture-only-password".into(), false).await.unwrap();
    let mut session = connect(origin, "producer".into(), "fixture-only-password".into(), false).await.unwrap();
    let qualified = test(&mut session, "Natural EOF worker".into()).await.unwrap();
    assert_eq!(qualified["passed"], true);
    let _ = heartbeat(&session, true).await.unwrap();
    let selected = admin.client.post(endpoint(&admin.base, "/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token).json(&serde_json::json!({"host_id":session.host})).send().await.unwrap();
    assert!(selected.status().is_success());
    let job_deadline = Instant::now() + Duration::from_secs(10);
    let job = loop {
        if let Some(job) = heartbeat(&session, true).await.unwrap().jobs.into_iter().find(|job| job.id == "front") { break job; }
        assert!(Instant::now() < job_deadline, "actual assigned camera job required");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let destination = endpoint(&session.base, &job.output).unwrap();
    // A real finite producer tests valid container EOF and the receiver's
    // successful process-exit path. Test19 separately proves the actual input
    // authentication/proxy/controller path; this does not replace that proof.
    let mut command = Command::new("ffmpeg");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut encoder = command.args(["-hide_banner", "-loglevel", "quiet", "-nostdin", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-f", "lavfi", "-i", "sine=frequency=660:sample_rate=48000", "-t", "16", "-map", "0:v:0", "-map", "1:a:0", "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-pix_fmt", "yuv420p", "-g", "30", "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2", "-f", "matroska", "-live", "1", "-cluster_time_limit", "500", "-flush_packets", "1", "pipe:1"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let output = encoder.stdout.take().unwrap();
    let captured = session.clone();
    let generation = job.generation.clone().expect("original assigned job generation");
    assert_eq!(Some(&generation), session.generation.as_ref());
    let upload = tokio::spawn(async move {
        captured.upload_client.post(destination).bearer_auth(&captured.token).header("Content-Type", "video/x-matroska")
            .header("X-Station-Transcode-Generation", generation)
            .body(reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(output))).send().await
    });
    let owned = PathBuf::from(std::env::var_os("GS_MEDIA_TEST_DIR").unwrap()).join("decoded-natural-audience");
    let started = Instant::now();
    let mut beat = Instant::now() - Duration::from_secs(3);
    let mut decoded = None;
    let mut objects = 0;
    while !upload.is_finished() {
        assert!(started.elapsed() < Duration::from_secs(30), "finite upload completion bounded");
        if beat.elapsed() >= Duration::from_secs(1) { let _ = heartbeat(&session, true).await.unwrap(); beat = Instant::now(); }
        if decoded.is_none() && let Ok((snapshot, count)) = audience_snapshot(&admin, &owned).await {
            decoded = Some(tokio::task::spawn_blocking(move || decode_audience(snapshot)).await.unwrap().unwrap());
            objects = count;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let response = upload.await.unwrap().unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT, "valid finite input must receive successful natural completion");
    assert!(tokio::time::timeout(Duration::from_secs(5), encoder.wait()).await.unwrap().unwrap().success(), "actual producer must exit zero");
    assert_eq!(decoded, Some((5, 5)), "actual audience video and audio decoding required before completion");
    let _ = heartbeat(&session, false).await.unwrap();
    let streams: serde_json::Value = json(admin.client.get(endpoint(&admin.base, "/api/live_streams").unwrap()).bearer_auth(&admin.token).send().await.unwrap()).await.unwrap();
    assert!(streams["streams"].as_array().unwrap().iter().any(|s| s["id"] == "front" && s["online"] == true && !s["url"].as_str().unwrap().contains("transcoded-")), "natural completion restores the live original feed");
    println!("ACTUAL_FINITE_NATURAL_COMPLETION {}", serde_json::json!({"http_status":204,"actual_encoder_exit0":true,"decoded_video_frames":5,"decoded_audio_frames":5,"actual_audience_objects":objects,"qualified_worker":true,"original_feed_restored":true,"scope":"finite valid container natural EOF/actual response+process exit and audience decode; actual input proxy/controller covered separately by19"}));
}

#[tokio::test]
#[ignore = "manual actual authenticated loopback generation route contract"]
async fn actual_original_generation_late_upload_and_stale_close_are_refused() {
    let origin = std::env::var("GS_MEDIA_TEST_ORIGIN").unwrap();
    let mut worker = connect(origin.clone(), "producer".into(), "fixture-only-password".into(), false).await.unwrap();
    let admin = connect(origin, "administrator".into(), "fixture-only-password".into(), false).await.unwrap();
    let report = test(&mut worker, "Generation route control".into()).await.unwrap();
    assert_eq!(report["passed"], true);
    let original = worker.generation.clone().expect("qualified opaque generation");
    let _ = heartbeat(&worker, true).await.unwrap();
    let host = worker.host.as_ref().unwrap();
    let selection = admin.client.post(endpoint(&admin.base, "/api/video/transcode/hosts").unwrap()).bearer_auth(&admin.token)
        .json(&serde_json::json!({"host_id":host})).send().await.unwrap();
    assert!(selection.status().is_success());
    let reply = heartbeat(&worker, true).await.unwrap();
    assert!(reply.jobs.iter().all(|job|job.generation.as_ref()==Some(&original)));
    let ack = heartbeat(&worker, false).await.unwrap();
    assert!(stop_acknowledges(&original, &ack));
    let next = ack.generation.clone().unwrap();
    let output = endpoint(&worker.base, &format!("/api/video/transcode/output/{host}/front")).unwrap();
    let late = worker.upload_client.post(output.clone()).bearer_auth(&worker.token).header("X-Station-Transcode-Generation", &original)
        .header("Content-Type", "video/x-matroska").body(Vec::new()).send().await.unwrap();
    assert_eq!(late.status(), reqwest::StatusCode::CONFLICT);
    worker.generation=Some(next.clone());
    let _ = heartbeat(&worker, true).await.unwrap();
    // Both old and missing generation must be refused while the NEW
    // generation is offered/selected, not merely because the host is stopped.
    let late = worker.upload_client.post(output.clone()).bearer_auth(&worker.token)
        .header("X-Station-Transcode-Generation", &original).header("Content-Type", "video/x-matroska")
        .body(Vec::new()).send().await.unwrap();
    assert_eq!(late.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(late.text().await.unwrap(), "video_processing_generation_stale");
    let missing = worker.upload_client.post(output.clone()).bearer_auth(&worker.token)
        .header("Content-Type", "video/x-matroska").body(Vec::new()).send().await.unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(missing.text().await.unwrap(), "video_processing_generation_stale");
    let missing_stop = worker.client.post(endpoint(&worker.base, &format!("/api/video/transcode/host/{host}")).unwrap())
        .bearer_auth(&worker.token).json(&serde_json::json!({"offered":false})).send().await.unwrap();
    assert_eq!(missing_stop.status(), reqwest::StatusCode::CONFLICT);
    let stale = worker.client.post(endpoint(&worker.base, &format!("/api/video/transcode/host/{host}")).unwrap()).bearer_auth(&worker.token)
        .json(&serde_json::json!({"offered":false,"generation":original})).send().await.unwrap();
    assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    assert!(!stop_acknowledges(&next, &ack));
    let after = heartbeat(&worker, true).await.unwrap();
    assert!(after.selected && after.generation.as_ref()==Some(&next));
    let unauthenticated = worker.client.post(output).header("X-Station-Transcode-Generation", &next).body(Vec::new()).send().await.unwrap();
    assert!(matches!(unauthenticated.status().as_u16(),401|403));
    let ack=heartbeat(&worker,false).await.unwrap();assert!(stop_acknowledges(&next,&ack));
    println!("ACTUAL_GENERATION_ROUTE_CONTROL {}",serde_json::json!({"late_upload_status":409,"late_upload_after_reoffer_status":409,"missing_generation_after_reoffer_status":409,"missing_generation_close_status":409,"stale_close_status":409,"stale_ack_rejected":true,"new_offer_survived_stale_close":true,"unauthenticated_upload_rejected":true,"actual_same_origin_authenticated_http":true}));
}
