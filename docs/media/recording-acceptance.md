# Linux synthetic camera recording acceptance

This is a Linux developer acceptance runner: it uses `statvfs`, Unix process groups, Docker host networking and the receiver’s `/proc` file descriptors. Its paths and artifact inputs work across Linux checkouts; this is not Windows/macOS or native GUI acceptance.

Run this manual developer test only with test executables from the current checkout. It uses isolated loopback services and synthetic FFmpeg cameras, never real hardware or a production backend. It checks actual growing-file download refusal, natural one-minute rollover, finalized original and processed H.264/AAC MP4 downloads, HTTP ranges, decoded camera audio/video and current permission revocation. It does not record crew voice, commentary or the composite programme.

Install FFmpeg/FFprobe and Docker. Have `bluenviron/mediamtx:1.21.0` and `debian:bookworm-slim` available. The receiver image digest is checked. A read-only, network-none inspector shares only the receiver PID namespace and uses `SYS_PTRACE` to observe its open file descriptors. The receiver itself binds exactly five random loopback sockets.

Build fresh test executables, retaining Cargo's actual JSON artifact records:

```sh
cargo test --locked -p groundstation_backend --bin groundstation_backend --no-run --message-format=json > backend-test-build.jsonl
cargo test --locked --manifest-path video_transcoder/Cargo.toml --no-run --message-format=json > helper-test-build.jsonl
```

Do not edit source or rebuild either executable between these commands and the test. Rebuild and regenerate the JSON after any source change; Cargo JSON does not retain historical source contents, so a newly recorded source snapshot cannot prove what an older executable compiled. The driver selects only Cargo `profile.test` executables whose package manifests belong to this checkout, records their hashes and source inputs, and refuses ambiguous records. Run with a fresh absolute evidence directory whose parent exists:

```sh
python3 tests/media_recording_roundtrip.py --backend-build-json backend-test-build.jsonl --helper-build-json helper-test-build.jsonl --output /absolute/private/camera-recording-test --check-only
python3 tests/media_recording_roundtrip.py --backend-build-json backend-test-build.jsonl --helper-build-json helper-test-build.jsonl --output /absolute/private/camera-recording-test
```

Trusted pre-existing test artifacts can alternatively use `--backend-proof` and `--helper-proof`: each JSON receipt must contain `path`, `sha256`, `test_target` and the complete current `source_snapshot` produced by this driver. Never substitute a production application or an untrusted executable.

Allow about 90 seconds, with at least 6 GiB free. The runner samples a 512 MiB evidence guard and bounds subprocess work; a small bounded overshoot between checks is possible, so this is not a hard storage quota. The test preserves the current one-minute segment, one-second part and unlimited-retention settings. It never deletes existing recordings or changes station retention. Recordings remain in the chosen directory; remove that test directory yourself when no longer needed.

`RESULT.json` requires the old receiver file descriptor to close, a new segment to open and the existing five-second recent-write guard to elapse. It does not merely infer finalization from an old timestamp. The test deliberately revokes a synthetic producer role and may interrupt its helper afterward; this is not another uninterrupted 100-second encoding run. Expected post-revocation or cleanup FFmpeg errors remain in the logs. Cleanup waits for direct child process groups and removes the receiver container; it does not claim an arbitrary-descendant census.
