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

Normal Stop currently ends the live upload without receiving the station remuxer's final completion acknowledgement. The helper can verify that its own encoder and proxy have stopped, but it cannot prove remote cleanup from that alone. It therefore displays `video_processing_remote_completion_unverified`, stops offering work, and blocks Test, Connect and a new offer for the lifetime of that controller. Disconnect still works, but does not clear this state. Starting a new helper/controller resets the local state; it does not prove that an old remote upload has finished, so a restart or a fixed wait must not be treated as permission for immediate overlapping work. Safe Stop followed by re-offering in the same session remains an open follow-up requiring an authenticated backend completion or generation fence. Healthy finite uploads that receive the actual successful completion response are a separate, tested case.

The helper receives authenticated media through GroundStation; it never receives the camera receiver's publishing password. An ephemeral loopback-only proxy keeps station authorization out of FFmpeg arguments and preserves complete HLS masters, including alternate camera-audio renditions. It bounds concurrent transfers and response sizes, restricts every playlist reference to that camera's authenticated input path, rejects redirects, and closes on job cancellation. Control requests retain bounded read timeouts; a live upload waits for its response until encoding ends, while host heartbeats and cancellation govern its lifetime. No encoded copy is saved locally by the helper. Crew voice remains a separate channel from camera microphones and programme commentary.

Linux build, real synthetic H.264/AAC roundtrip, qualification gates, one/two concurrent Chromium viewers, decoded camera-audio samples before and after transcoding, layout changes, clean OBS output and disconnect fallback are tested. Physical Pi load, iOS camera controls and native Windows/macOS GUI execution still require testing on those devices.

The ignored `engine::tests::native_worker_roundtrip` acceptance fixture requires an isolated synthetic backend and MediaMTX receiver. Set `GS_MEDIA_TEST_ORIGIN`, `GS_VIDEO_API_URL` and `GS_VIDEO_PASSWORD` to those loopback fixture services. It measures both legacy video-only MPEG-TS and current Matroska availability, asserts that encoding jobs survive the requested `GS_TRANSCODE_TEST_HOLD_SECONDS`, and checks disconnect fallback. Browser acceptance separately checks decoded camera audio and rejects unintended fallback during encoding.
