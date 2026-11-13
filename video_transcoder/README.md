# GroundStation Video Transcoder

A separate native Rust desktop application for Linux, macOS and Windows. It connects to GroundStation and encodes selected camera feeds on this computer. The station forwards the original inputs and remuxes returned H.264/AAC with `-c copy`; it does not encode the video. Keep MediaMTX running on the station.

Install Rust and FFmpeg with libx264/AAC support on the helper computer. FFmpeg must also be available on the station for copy remuxing. Linux desktop builds require the GTK/WebKit development libraries used by Dioxus Desktop. Build from the GroundStation repository:

```sh
python3 build.py video_transcoder
# Optional development build or command log:
python3 build.py video_transcoder debug
python3 build.py video_transcoder log=video-helper-build.log

# Equivalent direct Cargo command:
cargo build --locked --release --manifest-path video_transcoder/Cargo.toml
```

The helper selector builds only this separate workspace on the current host. It does not fetch/build the station frontend, compile the backend, install FFmpeg or GTK/WebKit, or produce a cross-platform installer. Station modes such as `testing`, `rrls`, `docker` and `frontend_web` are rejected with this selector. The tracked helper `Cargo.lock` is required; `CARGO_TARGET_DIR`, if set, still controls Cargo output.

Run `video_transcoder/target/release/gs-video-transcoder` (`.exe` on Windows). This is a separate Cargo workspace so station/Pi builds do not acquire desktop GUI dependencies. The executable needs FFmpeg on PATH. `gs-video-transcoder --benchmark` runs the synthetic encoding check without opening the GUI.

1. Connect to the station using a stream manager or administrator account. An explicit certificate option supports a local station's self-signed certificate. Remote stations require trusted HTTPS. Credentials and session tokens stay in memory.
2. Choose a host name and select **Test compute and bandwidth**. This transfers 4 MiB in each direction over the actual station connection and encodes three seconds of synthetic 720p30 video and audio. Passing requires encoding within 2.5 seconds and at least 12 Mb/s in both directions. Capacity is an estimate, limited to eight cameras; actual performance depends on source decoding, concurrent work and sustained network quality.
3. Select **Make available as transcoding host**. This action becomes available after passing the test.
4. In GroundStation's **Stream Manager → Transcoding host**, select the offered computer. Camera jobs are assigned up to its qualified capacity. Outputs contain H.264 video and AAC camera audio, up to 720p30, with one-second keyframes. The upload uses live Matroska to retain AAC codec headers, flushes packets immediately, and bounds cluster duration to 500 ms. Station copy-remux probing is limited to one second and 1 MiB; legacy video-only MPEG-TS helpers remain supported.
5. Keep the helper open. Stop offering, disconnect or close it to return viewers to original feeds. A missing heartbeat expires within 15 seconds. Permissions are checked by the server throughout the stream; browser-only devices do not act as encoding workers.

A station supporting the completion fence issues an opaque generation with the qualified host. Each assigned job and upload retains that original generation. Stop closes that generation and waits for the station's owned remux relays to verify child cleanup before acknowledging it and issuing a fresh generation. Only after this matching authenticated acknowledgement and verified local encoder/proxy cleanup does the helper permit an explicit new offer in the same controller. A delayed old upload or old Stop request cannot enter or stop the new generation. This does not infer successful encoding from cancellation; natural completion still requires the actual successful upload response and encoder exit.

Backend and helper must both support this fence for reusable Stop. A legacy helper that never sends a generation remains explicitly unfenced: its stop heartbeat receives no reusable completion acknowledgement and never rotates a generation. After a host enables generation fencing, it cannot downgrade to legacy requests; missing or stale upload generations and missing-generation heartbeats are rejected, including after re-offering. Legacy operation is not acceptance of safe Stop followed by reuse.

If the station lacks that acknowledgement, authentication is lost, or local/remote cleanup is unknown, the helper displays a cleanup-unverified status and blocks new work. Disconnect remains available, but neither a fixed delay nor restarting the helper proves remote cleanup. Local child/proxy cleanup failures cannot be cleared by a remote acknowledgement.

The helper retains a native worker join owner separately from its cloneable UI handles. A normal close or custom Quit request seals new work and cancels pending control phases; it keeps the application and owner alive while cleanup is pending. The worker retains child processes and proxy/job task handles through observation timeouts, continues observing their actual completion on its runtime, and destroys that runtime before returning for the native join. Local cleanup uncertainty keeps the window visible with an error. A joined worker may still report remote-only uncertainty; that remains explicit and never permits re-offering work.

The standalone `--benchmark` path uses the same owned-worker reporting. Its original thirty-second benchmark deadline is followed by owned kill/wait when necessary. A pending or uncertain cleanup result does not force process exit. Qualification retains its original sixty-second budget and the same codec, compute and bandwidth thresholds.

These mechanisms do not promise cleanup after OS force-kill, crash or unsupported external application termination. Manual production-API/close-hook tests are not actual OS-window evidence, and native Windows/macOS/Apple window and external Dock/system Quit behavior require separate qualification. Unexpected destruction of the join owner retains an unclaimed native join capability, marks cleanup unverified and blocks replacement workers; it is a fallback, not a successful close.

The helper receives authenticated media through GroundStation; it never receives the camera receiver's publishing password. An ephemeral loopback-only proxy keeps station authorization out of FFmpeg arguments and preserves complete HLS masters, including alternate camera-audio renditions. It bounds concurrent transfers and response sizes, restricts every playlist reference to that camera's authenticated input path, rejects redirects, and closes on job cancellation. Control requests retain bounded read timeouts; a live upload waits for its response until encoding ends, while host heartbeats and cancellation govern its lifetime. No encoded copy is saved locally by the helper. Crew voice remains a separate channel from camera microphones and programme commentary.

Linux build, real synthetic H.264/AAC roundtrip, qualification gates, one/two concurrent Chromium viewers, decoded camera-audio samples before and after transcoding, layout changes, clean OBS output and disconnect fallback are tested. Physical Pi load, iOS camera controls and native Windows/macOS GUI execution still require testing on those devices.

The ignored `engine::tests::native_worker_roundtrip` acceptance fixture requires an isolated synthetic backend and MediaMTX receiver. Set `GS_MEDIA_TEST_ORIGIN`, `GS_VIDEO_API_URL` and `GS_VIDEO_PASSWORD` to those loopback fixture services. It measures both legacy video-only MPEG-TS and current Matroska availability, asserts that encoding jobs survive the requested `GS_TRANSCODE_TEST_HOLD_SECONDS`, and checks disconnect fallback. Browser acceptance separately checks decoded camera audio and rejects unintended fallback during encoding.
