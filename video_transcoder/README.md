# GroundStation Video Transcoder

A separate native Rust desktop application for Linux, macOS and Windows. It connects to GroundStation and encodes selected camera feeds on this computer. The station forwards the original inputs and remuxes returned H.264/AAC with `-c copy`; it does not encode the video. Keep MediaMTX running on the station.

Install Rust and FFmpeg with libx264/AAC support on the helper computer. FFmpeg must also be available on the station for copy remuxing. Linux desktop builds require the GTK/WebKit development libraries used by Dioxus Desktop. Build from the GroundStation repository:

```sh
cargo build --release --manifest-path video_transcoder/Cargo.toml
```

Run `video_transcoder/target/release/gs-video-transcoder` (`.exe` on Windows). This is a separate Cargo workspace so station/Pi builds do not acquire desktop GUI dependencies. The executable needs FFmpeg on PATH. `gs-video-transcoder --benchmark` runs the synthetic encoding check without opening the GUI.

1. Connect to the station using a stream manager or administrator account. An explicit certificate option supports a local station's self-signed certificate. Remote stations require trusted HTTPS. Credentials and session tokens stay in memory.
2. Choose a host name and select **Test compute and bandwidth**. This transfers 4 MiB in each direction over the actual station connection and encodes three seconds of synthetic 720p30 video and audio. Passing requires encoding within 2.5 seconds and at least 12 Mb/s in both directions. Capacity is an estimate, limited to eight cameras; actual performance depends on source decoding, concurrent work and sustained network quality.
3. Select **Make available as transcoding host**. This action becomes available after passing the test.
4. In GroundStation's **Stream Manager → Transcoding host**, select the offered computer. Camera jobs are assigned up to its qualified capacity. Outputs contain H.264 video and AAC camera audio, up to 720p30, with one-second keyframes.
5. Keep the helper open. Stop offering, disconnect or close it to return viewers to original feeds. A missing heartbeat expires within 15 seconds. Permissions are checked by the server throughout the stream; browser-only devices do not act as encoding workers.

The helper receives authenticated media through GroundStation; it never receives the camera receiver's publishing password. It rejects off-station media URLs and redirects. No encoded copy is saved locally by the helper. Crew voice remains a separate channel from camera microphones and programme commentary.

Linux build, real synthetic H.264/AAC roundtrip, qualification gates, one/two concurrent Chromium viewers, layout changes, clean OBS output and disconnect fallback are tested. Physical Pi load, iOS camera controls and native Windows/macOS GUI execution still require testing on those devices.
