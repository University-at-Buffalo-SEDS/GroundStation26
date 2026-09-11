# Live video and stage models

The ground station receives H.264 over RTSP/TCP on **port 8554**, with a unique
path per camera (`front`, `side`, `aft`, etc.). MediaMTX relays encoded video to
live WebRTC previews and a [delayed HLS audience program](broadcast-studio.md) without transcoding.
The default **Dashboard** displays the single-stage model with aft fins only;
**Mission** displays video/studio controls. They consume `/api/dashboard_status`,
`/api/vehicle_visualization`, and `/api/live_streams`. There is no separate Vehicle tab.
Settings → General → Viewing mode offers **Ground Station view** (instruments and
controls on this device) and **Streamer** (delayed program). Watching does not require
a stream-manager role. See the [frontend contract](../frontend/api.md).
Open **`https://GROUND_STATION_IP:3000/media`** for
model upload/download, model selection, and a standalone multi-camera monitor.

The initial sender settings are 720p, 30 fps, 2 Mbit/s per camera. Tune them for
scene motion and available network capacity. Two feeds need approximately 4 Mbit/s
inbound plus overhead. Each viewer adds outgoing traffic for every camera watched.
This uses an IP network independently of SEDSNet telemetry. TCP simplifies camera
connections, but sustained packet loss increases latency; reduce bitrate when
the link cannot keep up.

Pi 4 and earlier camera-capable Pis can encode H.264 in hardware. Pi 5 uses
software encoding. The Rust daemon supervises **rpicam-vid and FFmpeg**, which must
also be installed. It requests baseline H.264 without B-frames, inline codec
headers, and one-second keyframe intervals. Encoded bytes pass directly through
an OS pipe; FFmpeg uses stream copy. Rust does not replace the camera/codec stack.

## Ground station setup

Set the ground station's reachable LAN IP and a shared secret:

```sh
export GS_VIDEO_HOST=192.168.1.100
export GS_VIDEO_PASSWORD=REPLACE_WITH_A_LONG_RANDOM_SECRET
docker compose -f docker-compose.yml -f docker-compose.video.yml build --build-arg FRONTEND_DEV=TRUE
docker compose -f docker-compose.yml -f docker-compose.video.yml up -d --no-build
```

The overlay adds MediaMTX 1.21.0 and publishes TCP 8554 (camera input), UDP 8189
(WebRTC), and TCP 8189 (fallback). The existing UI stays on HTTPS port 3000. Use a
LAN IP, not the container IP, for `GS_VIDEO_HOST`. MediaMTX's API (9997) and
signaling (8889) and HLS (8888) stay inside Docker. Without the overlay, existing deployments
continue to work and the media page reports the receiver as unavailable.

The camera account can publish only; the groundstation account can read and list
streams. Browser requests use existing ground station authorization: `ViewData`
for the delayed program/models and `SendCommands` for model uploads. Live camera
previews additionally require stream-manager/admin access or hardware-operator
permission. Sign in on the media page;
its session is held in page memory independently of other tabs. Guest access
follows the existing backend configuration. Mission's iframe player and Dashboard's
GLB loader receive stable, expiring, resource-scoped capability URLs, so they work
without bearer headers or cross-origin cookies. These tickets do not expose the
login token, are reauthorized on access, and cannot grant upload/control access.
Treat returned media URLs as private. RTSP is unencrypted: use the trusted camera
network or a VPN.

For a native backend, run MediaMTX with `backend/config/mediamtx.yml`, setting the
indexed `MTX_AUTHINTERNALUSERS_*` environment variables shown in the Compose
overlay and `MTX_WEBRTCADDITIONALHOSTS` to the LAN IP. Bind API/signaling to loopback
with `MTX_APIADDRESS=127.0.0.1:9997`, `MTX_WEBRTCADDRESS=127.0.0.1:8889`, and
`MTX_HLSADDRESS=127.0.0.1:8888`. Set
`GS_VIDEO_PASSWORD` on the backend. Its default relay URLs are those loopback
addresses; override `GS_VIDEO_API_URL`, `GS_VIDEO_WEBRTC_URL`, and `GS_VIDEO_HLS_URL` if necessary.
Native backend HTTP is port 3000; remote browser login needs the TLS reverse proxy.

## Build and install the sender on the Pi

Use current Raspberry Pi OS. From this repository:

```sh
sudo apt update
sudo apt install rpicam-apps ffmpeg
cargo build --release -p gs-video-sender
sudo useradd --system --user-group --groups video,render --no-create-home gs-video
sudo install -m 0755 target/release/gs-video-sender /usr/local/bin/gs-video-sender
sudo install -d -m 0750 /etc/gs-video-sender
sudo install -d -o gs-video -g gs-video -m 0750 /var/lib/gs-video-sender
sudo install -o gs-video -g gs-video -m 0600 video_sender/config.example.json /var/lib/gs-video-sender/config.json
sudo install -m 0644 video_sender/gs-video-sender.service /etc/systemd/system/gs-video-sender.service
sudoedit /var/lib/gs-video-sender/config.json
sudoedit /etc/gs-video-sender/environment
```

Change `host` in the JSON to the ground station IP. The environment file contains:

```ini
GS_VIDEO_PASSWORD=THE_SAME_SECRET_AS_THE_GROUND_STATION
```

Start the daemon:

```sh
sudo chmod 0600 /etc/gs-video-sender/environment
sudo -u gs-video gs-video-sender check /var/lib/gs-video-sender/config.json
sudo systemctl daemon-reload
sudo systemctl enable --now gs-video-sender
sudo journalctl -u gs-video-sender -f
```

The binary runs in the foreground under systemd, starts on boot, and stops its
children on shutdown. Each pipeline retries independently after failure or a
20-second progress stall, with a three-second retry delay. FFmpeg diagnostics are
suppressed because they can contain credentials; daemon logs identify failed
streams without logging authenticated URLs. Camera device access needs the
`video` and `render` groups; the unit intentionally does not hide devices.

## Runtime controls

```sh
sudo -u gs-video gs-video-sender status
sudo -u gs-video gs-video-sender set front bitrate 1000000
sudo -u gs-video gs-video-sender set front fps 20
sudo -u gs-video gs-video-sender set front enabled false
sudo -u gs-video gs-video-sender set front enabled true
```

Supported fields: `bitrate` (bits/s), `fps`, `width`, `height`, `camera`, `enabled`.
Updates are validated and saved atomically. The affected stream reconnects briefly;
other cameras continue. Status describes pipeline processes, not confirmed browser
delivery. For several settings at once, a new destination, or added/removed angles:

```sh
sudoedit /var/lib/gs-video-sender/config.json
sudo systemctl reload gs-video-sender
```

Invalid reloads keep the current settings and log an error. Add another object to
`streams` with a different `id` and camera index. Find cameras using
`rpicam-hello --list-cameras`. Simultaneous CSI cameras need independent camera
interfaces (Pi 5 or a suitable Compute Module); single-port camera multiplexers
cannot capture multiple angles simultaneously. Separate Pis may each use index 0
but must publish different stream IDs.

Controls use a protected local Unix socket, not a network port; use SSH for remote
adjustment. For development, set `GS_VIDEO_SOCKET` to a path in a private directory
and run `gs-video-sender daemon CONFIG.json`. A password change requires restarting
the service. The sender targets Unix/Linux, including arm64 Raspberry Pi OS.

## Model storage and API

Models are GLB 2.0 files, up to 64 MiB, keyed by stage and model name. Reusing both
names atomically replaces a file. IDs contain 1–64 ASCII letters, digits, hyphens
or underscores. Files persist at `backend/data/stage_models/STAGE/NAME.glb` under
the existing Docker data volume; override with `GS_STAGE_MODELS_DIR`. The server
validates the GLB container, not the entire scene. Use **Use in Vehicle view** to
select a stored model from the legacy `/media` administration page, then reopen Dashboard.
Bindings are configured by backend administrators, not through a frontend editor.
Without an explicit selection,
the first model sorted by stage/name is used. Stored stage IDs are included in the
vehicle metadata. The existing frontend renders one selected GLB at a time; it
does not assemble multiple stage files into one scene.

`PUT /api/vehicle_visualization` saves the frontend's vehicle configuration,
including attitude bindings, stage components, ground systems, and phase animation
names. For selection, use `model_url: "/api/stage-models/STAGE/NAME"`; the GET
response converts this to a browser-readable URL. The current frontend always loads
`/assets/three/vehicle-renderer.js` and its bundled Three.js dependencies; `renderer_url`
is retained for compatibility and is not a frontend renderer override. Animation names
must match clips in your GLB. POST saves the same document and returns `{"saved":true}`;
PUT returns 204. See [named-node mappings](model-animations.md).

`POST /api/live_streams/control` accepts the frontend broadcast object (label,
featured_stream_id, hidden_stream_ids, layout, delay_seconds, revision). Delay is
3–60 seconds, default 10, plus playback buffering. Only authenticated
`stream_master`/`stream_admin` account roles or legacy explicit `StreamControl` grants can edit
it; other sessions receive 403. Revisions increment on every save; stale edits
receive 409. Broadcast and vehicle configuration persist together in
`backend/data/stage_models/_presentation.json`. Its optional `title`,
`stream_labels`, and `stats` fields configure labels and backend-owned stat bindings.
Empty/missing stats inherit six real telemetry fields; the bars cycle three at a time.
GET `/api/live_streams` returns a valid empty stream list if the receiver is down,
allowing the operator Mission view to fall back to the model. Audience/streamer playback
buffers or displays an error rather than falling back to live video or live telemetry.

| Endpoint | Behavior |
| --- | --- |
| `GET /api/live_streams` | Frontend Mission contract with browser-readable players |
| `GET /api/dashboard_status` | Server-resolved live phase, T clock and nullable statistics |
| `GET` / `POST /api/stream-roles` | Stream-admin-only account role listing/assignment |
| `POST /api/live_streams/control` | Persist stream-master broadcast state |
| `GET /api/vehicle_visualization` | Dashboard model/stage/binding contract |
| `PUT /api/vehicle_visualization` | Save selected model and visualization settings |
| `POST /api/vehicle_visualization` | Save the same configuration, JSON acknowledgement |
| `GET /api/video/streams` | List camera IDs and live status |
| `POST /api/video/streams/ID/whep` | SDP offer → answer and session Location |
| `DELETE /api/video/streams/ID/whep/SESSION` | Release viewer session |
| `GET /api/stage-models` | List stage/name/size metadata |
| `PUT /api/stage-models/STAGE/NAME` | Raw GLB upload |
| `GET /api/stage-models/STAGE/NAME` | Download GLB |

Use `Authorization: Bearer SESSION_TOKEN` for metadata and management APIs. The
returned `/api/media-assets/...` URLs include scoped tickets for media elements.
No extension appears in model URLs.
WHEP clients must gather ICE before posting; trickle-ICE PATCH is not implemented.
The included client reconnects automatically. HLS segments are retained transiently
for the audience delay; this is not durable video recording. See the
[broadcast contract](broadcast-studio.md) for scoped program/HLS routes, role
bootstrapping, revocation behavior, and delay limitations.

References: [MediaMTX configuration](https://mediamtx.org/docs/references/configuration-file),
[WebRTC compatibility](https://mediamtx.org/docs/features/webrtc-specific-features),
[Pi camera/encoder documentation](https://www.raspberrypi.com/documentation/computers/camera_software.html).
