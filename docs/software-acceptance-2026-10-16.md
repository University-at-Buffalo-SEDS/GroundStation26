# GroundStation software acceptance — 16 October 2026

The source changes are implemented. Extended linked-network qualification and
publication remain pending; see the [work plan](work-plans/antenna-and-comms.json)
and simulator's `docs/qualification/sedsnet-4.2.3.md` for the current release gate.

## Verified software paths

| Area | Acceptance evidence |
| --- | --- |
| Command controls | All 55 dashboard commands dispatch once over an isolated WebSocket. Recovery confirmation acceptance sends once; cancellation sends none. The backend captures every exposed FC command through a discovered fake radio route; disconnected or closed-worker requests fail visibly. |
| Operating procedures | Simulated test-fire self-test → nitrogen test → nitrous fill to target, saved qualifications, reset, restart with empty/filled tank, immediate cross-flow handling and launch boundary. Flight mode checks GPS/FC routes and waits five seconds from QD ACK. Existing HITL operation remains available. |
| Physical panel | Browser mirror uses the actual GPIO mapping, explicit requested lamp states and unavailable/locked/active states. Requested state is not a measured physical lamp. |
| Antenna aiming | Exclusive horizontal/vertical guidance, all origin/target choices, bearing wraparound, altitude datum checks, stale/missing fixes, sensor cleanup and manual trim. Headless viewports: 320, 390, 844 and 1280 pixels. |
| Administration | Separate application-admin permission, account changes, last-admin protection, session revocation, stream/view and voice controls; no stream role implicitly grants account administration. |
| Cameras and program | Shared authentication, synthetic camera WebRTC publishing, hidden-tab behavior, logout/permission loss, H.264 single decoder, sound, portrait layout, delay, camera/server failure and 2D fallback. |
| Data UI | Chart/history regressions, synthetic live sensor updates, maps, narrow-screen checklist and scrollable export controls. |
| RRLS | Paired position validation, direct/fallback routing and 600-second virtual loss/recovery tests. Optional 8 kb/s Opus PTT remains off by default; 6000 command/GPS updates precede voice under congestion. No video enters radio relay. |
| Ethernet bridge | Six real PICOFI1 TCP loopback tests on macOS and Ubuntu ARM64, including fragmented framing, duplex traffic, reconnect and bounded backpressure. Client/server configuration is exposed in CLI, TUI and Tk. |

## Platforms and counts

- macOS backend: 291 HITL, 299 test-fire and 298 flight tests pass; one existing
  manual test is ignored per profile. The final radio scheduling subset adds
  a separate successful run of all 19 relevant regressions.
- Ubuntu ARM64 on Raspberry Pi: backend with `raspberry_pi` enabled: 305 passed, one ignored; headless
  dashboard command, chart, sensor and map acceptance passes.
- Native frontend: 96 passed, seven ignored on macOS and Ubuntu ARM64.
  The Wasm application builds with the matching Dioxus 0.7.10 CLI.
- The frontend dependency remains a separate repository. GroundStation's SDK
  manifest remains `SEDSnet = "4"`; board scripts retain `main` and offline-cache
  fallback. No temporary qualification path patch is committed.

## What simulation cannot establish

No physical outputs were actuated, no firmware was flashed, and the live Pi
service was not restarted during this acceptance work. Phone compass accuracy,
physical GPS and camera/audio devices, actual panel illumination, RF propagation,
and native iOS/Android packaging require later device/field validation. Native
Linux compilation and headless browser tests do not prove a native window renders
on a Pi display. Test-fire qualification never requires an absent AV bay.

The linked MCU soak is the remaining software release gate. Bulk telemetry can
be shed at bounded queues under pressure; successful control/ACK tests must not
be described as lossless acquisition. DAQ SD recording and network reporting
remain distinct paths.
