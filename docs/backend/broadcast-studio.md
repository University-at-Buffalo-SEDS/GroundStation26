# Broadcast studio and delayed audience program

![Browser-tested program showing a synthetic camera and delayed banner](delayed-program.png)

Operators and stream masters receive live WebRTC camera previews. Spectators and
**Settings → Streamer** receive a separate HLS program with delayed video and
telemetry. Camera cuts change the current delayed program, not the manager's live
preview. This gives the manager time to cut away before an upcoming event reaches
the audience. No extra encoder is required on the camera Pi.

## Roles and setup

Roles are separate from `session_type` (which remains `session`/`remembered`) and
from rocket command permissions:

| Account role | Broadcast access | Hardware access |
| --- | --- | --- |
| Ordinary viewer / `stream_viewer` | Delayed program | Unchanged; normally none |
| `stream_master` | Live previews, cuts, visibility, layout, label, delay | Unchanged |
| `stream_admin` | Stream master plus role assignment | Unchanged |

Bootstrap an existing trusted administrator in `backend/users/users.json` by adding
`"roles": ["stream_admin"]` and ensuring `permissions.view_data` is true. Do **not**
enable `send_commands` just to manage streams. This change requires filesystem
administrator access; the app cannot self-promote users to administrator.
Sign in, open **Mission → Stream controls → Broadcast studio → Stream manager
roles**, then grant/revoke stream-master access for existing accounts. The UI does
not create passwords or expose password hashes. Backend authorization rereads roles
on every request, so revocation applies to existing sessions and media tickets.
Revocation also schedules closure of that user's tracked live WebRTC previews, unless
they retain operator or stream-admin access. Relay connection failures are logged;
network-delivered media cannot be recalled, so use relay-side session termination if
a failed closure must be enforced immediately. No new preview can register after
the role change.
The capability returned by `/api/live_streams` refreshes studio controls as roles change.

Legacy explicit `StreamControl` grants remain supported. Revocation assigns
`stream_viewer`, overriding that legacy grant without modifying the command allowlist
(an empty command allowlist otherwise means all hardware commands). Stream admins
retain access; changing their role requires filesystem administration. Stream-role
changes never grant valve, launch, calibration, or model-editing privileges.

For unauthenticated spectators, explicitly enable `anonymous.view_data` in the users
file. Anonymous command access should remain disabled. No real accounts or credentials
are changed by installing this feature.

## Delay and cutting

Any account with viewing access can open **Settings → General → Viewing mode →
Streamer**; a stream-manager role is needed only to direct the broadcast. Use
**Exit streamer** at the upper right to return to Dashboard. Ground Station view
is a separate, per-device settings toggle; it does not grant hardware permissions.

The model dashboard and program bottom bars cycle three telemetry fields every
five seconds while keeping flight state and T clock visible. Backend defaults
include RF GPS altitude/latitude/longitude, calibrated tank pressure, fill mass and
fill percentage. Missing/stale samples show “—”, never fabricated values. Configure
`stats` in the backend `_presentation.json` to change channels, sources or units;
empty/missing legacy stats lists inherit these defaults. `/api/dashboard_status`
returns server-resolved live values, while the program uses delayed snapshots.

The studio exposes **Audience delay**, 3–60 seconds, default 10. This is the minimum
server release delay; playback adds approximately 2.5 seconds plus network/decoder
jitter. The browser integration test measured about 12.6 seconds for a 10-second
setting. Keep enough lead for the operator's reaction time and network conditions.

MediaMTX retains 90 one-second fMP4 segments per camera. The backend truncates
playlists to complete segments older than the delay and independently checks segment
requests. Guessing a newer segment URL cannot bypass release time. Spectators cannot
open the live WebRTC preview. Never expose MediaMTX's HLS/API/signaling listeners to
the public internet; use the authenticated backend routes. RTSP publishing still
requires the configured publisher credentials.

The program preloads up to eight visible cameras and aligns them using HLS program
date/time. A cut fades between already-decoding feeds over 550 ms. An unavailable
target produces a buffering screen instead of silently showing live video. Delay
changes discard browser buffers and re-align; camera disconnects, missing timing
anchors, and expired authentication fail closed. Keep ground-station time accurate
(NTP). Timing reflects relay ingestion, not a camera shutter timestamp. The delay
cannot retract frames already delivered under a previous, shorter setting.

Phase, T−/T+ launch clock and configured statistics use a bounded delayed snapshot history. The T clock is calculated at snapshot time and displayed beside the other program data; it never advances using the live clock. New viewers
may initially see “Buffering delayed telemetry”; current telemetry is never substituted
into the delayed banner. Banner labels, layouts and camera cuts are editorial changes
and apply promptly (500 ms polling), rather than being delayed again.

The stream manager can hide/unhide cameras, choose hero/grid layout, change the label,
and watch the audience monitor while using the live preview strip to select cameras.
Streamer mode hides the studio and displays only the delayed program. Operators can
retain their normal live operations/recovery interface.

## Deployment and limits

Rebuild the backend/frontend and recreate the video service with
`docker compose -f docker-compose.yml -f docker-compose.video.yml up -d --build`.
Native installations set `GS_VIDEO_HLS_URL` (default `http://127.0.0.1:8888`) and use
the updated `backend/config/mediamtx.yml`. HLS uses internal port 8888, not a new
public port. The bundled HLS.js 1.6.13 is BSD-2-Clause licensed (`backend/assets/hls.LICENSE`).

Delayed playback requires Media Source Extensions and H.264 support. A browser lacking
those capabilities displays an explicit message, with no live fallback. Chrome desktop
has been tested; actual Pi, Android and iOS/WebView validation remains a deployment
requirement. This is an in-app/browser program suitable for browser-source capture;
it does not publish to an external streaming service or provide synchronized audio.

Buffer memory and bandwidth scale with camera count and bitrate. At 2 Mbit/s, 90
seconds is roughly 22.5 MB per camera before overhead. All eight preloaded angles may
consume viewer bandwidth, even in hero mode. The existing per-path reader cap remains
16; load-test and size that limit before a larger public event.

## API

- `GET /api/live_streams`: adds `program_url`, `can_manage_stream`, `can_preview_live`.
- `POST /api/live_streams/control`: existing broadcast fields plus `delay_seconds`;
  invalid limits return 400, unauthorized changes 403, stale revisions 409.
- `GET /api/stream-roles`: stream-admin-only sanitized account/role list.
- `POST /api/stream-roles`: `{ "username": "producer", "stream_master": true }`.
- `/api/media-assets/program`: scoped audience player URL; embedded in the frontend.
- `/api/media-assets/program/state`: scoped camera/editorial/delayed-telemetry state.
- `/api/media-assets/hls/{id}/{file}`: scoped playlist/segment proxy with release checks.

## Reproducing the isolated browser checks

Use Node 22+ (global WebSocket), FFmpeg, Docker and a Chromium browser. These tests
use synthetic media/accounts and must not be pointed at production services. Reserve
loopback ports 18554 (RTSP), 19997 (relay API), 19998 (HLS), 19090 (fixture) and
19101 (browser debugging). In a shell at the backend repository root:

```sh
export GS_MEDIA_TEST_DIR=$(mktemp -d /private/tmp/gs-delay-test.XXXXXX)
docker run --rm --name gs-delay-test \
  -p 127.0.0.1:18554:8554 -p 127.0.0.1:19997:9997 -p 127.0.0.1:19998:8888 \
  -v "$PWD/backend/config/mediamtx.yml:/mediamtx.yml:ro" \
  -e MTX_WEBRTCADDITIONALHOSTS=127.0.0.1 \
  -e MTX_AUTHINTERNALUSERS_0_USER=camera -e MTX_AUTHINTERNALUSERS_0_PASS=test-only \
  -e MTX_AUTHINTERNALUSERS_0_PERMISSIONS_0_ACTION=publish \
  -e MTX_AUTHINTERNALUSERS_1_USER=groundstation -e MTX_AUTHINTERNALUSERS_1_PASS=test-only \
  -e MTX_AUTHINTERNALUSERS_1_PERMISSIONS_0_ACTION=read \
  -e MTX_AUTHINTERNALUSERS_1_PERMISSIONS_1_ACTION=api \
  bluenviron/mediamtx:1.21.0
```

In separate terminals, publish two synthetic cameras using the command below, once
with `front` and once with `side` as the final path (optionally replace `testsrc2`
with `smptebars` for the second angle):

```sh
ffmpeg -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=640x360:rate=15 \
  -an -c:v libx264 -preset ultrafast -tune zerolatency -profile:v baseline \
  -g 15 -bf 0 -f rtsp -rtsp_transport tcp \
  rtsp://camera:test-only@127.0.0.1:18554/front
```

Run the fixture in another terminal, exporting the **same** `GS_MEDIA_TEST_DIR`:

```sh
GS_STAGE_MODELS_DIR="$GS_MEDIA_TEST_DIR/models" \
GS_VIDEO_API_URL=http://127.0.0.1:19997 GS_VIDEO_HLS_URL=http://127.0.0.1:19998 \
GS_VIDEO_PASSWORD=test-only cargo test -p groundstation_backend \
  media::tests::serve_synthetic_media -- --ignored --nocapture
```

This ignored fixture creates only isolated fake users/in-memory sessions and starts
the media router; it does not start hardware/GSE tasks. Start a separate Chromium
profile with `--headless --remote-debugging-port=19101
--user-data-dir=<test-directory>/chrome --autoplay-policy=no-user-gesture-required`.
Enable working WebGL2 for the model test (macOS testing used `--use-angle=metal
--ignore-gpu-blocklist`). Keep the debug port loopback-only. Wait at least 25 seconds
for media retention, then run sequentially:

```sh
node scripts/test-broadcast.cjs
node scripts/test-model-animation.cjs
```

The scripts verify real decoding/delay, unreleased-segment denial, cuts, grid/hiding,
role grant/revoke, privilege separation, and named GLB node transforms. Screenshots
are written under `GS_MEDIA_TEST_DIR`. Stop the fixture, FFmpeg and test browser with
Ctrl-C and stop only the test container with `docker stop gs-delay-test` afterward.
These checks do not certify real camera hardware or mobile playback.

Implementation references: [MediaMTX configuration](https://mediamtx.org/docs/references/configuration-file)
and [HLS.js program-date/time APIs](https://hlsjs.video-dev.org/api-docs/hls.js.hls).
