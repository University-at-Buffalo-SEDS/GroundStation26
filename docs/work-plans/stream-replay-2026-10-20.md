# Stream isolation and replay

- [x] Dedicated stream manager and viewer screens exclude mission alerts and controls.
- [x] Flight launch hides fill widgets from Launch onward. Test-fire keeps them and labels the calibrated 1000 kg channel as thrust in newtons.
- [x] Capture up to 20 seconds from one to eight retained camera streams, including cameras omitted from the live program. Freeze encoded HLS segments on disk with bounded storage.
- [x] Replay has ordered cameras, per-camera framing and 0.25x/0.5x/1x playback, an explicit replay indicator, synchronized source timestamps, and automatic/explicit return to live.
- [x] Authenticate clip and segment access. Validate bounds and reject insufficient footage without altering the live program.
- [x] Verify mode rules, role isolation, replay limits, retained playlists, access control and browser playback.

## Verification

- Full backend suite: 318 passed, one existing manual media fixture ignored.
- Test-fire media suite: 19 passed, the same manual fixture ignored.
- Frontend native tests: 105 passed, seven existing ignored tests.
- Browser build uses Dioxus CLI 0.7.10 and completes without warnings.
- Headless Chrome with real H.264: frozen mid-stream VOD replay, quarter-speed advance, off-air camera selection, framing, reordering, return to live, layouts and camera/server loss pass.
- Full dashboard browser smoke: dedicated operator/viewer roles, replay controls, responsive layouts, preserved camera/voice documents and all 55 command buttons pass with mocked transports. No hardware outputs are connected.
- Retained replay telemetry survives live-ring eviction. Per-camera tickets, viewer control rejection, traversal rejection, upstream loss and failed-capture rollback pass.

## Limits and follow-up

Replay freezes the last 1–20 seconds of the audience timeline. Select up to eight cameras; rates are 1, 0.5 and 0.25. Two encoded clips are retained, each capped at 256 MiB, plus an in-progress capture. Up to 128 source telemetry snapshots are retained per clip. Segment delivery streams from disk rather than buffering a whole file per viewer. Replay does not retransmit video over the radio link.

Physical multi-camera throughput, codecs and storage on the deployed Ubuntu Pi still need hardware qualification. Backend and UI must be rebuilt together. No firmware or SEDSnet update is required.
