# Test video from macOS through Ground Station

This validates **macOS FFmpeg → RTSP/TCP → Ground Station MediaMTX → backend → browser**,
including live WebRTC and delayed HLS playback. It does not test Pi camera capture
or SEDSNet telemetry. Examples use Ground Station at `192.168.7.3`.

## 1. Find the existing video password

For the updated native/systemd backend, the automatically generated secret is
`data/video/password` on the Ground Station machine, unless `GS_VIDEO_PASSWORD`
was explicitly configured. See [native receiver setup](SETUP_GROUND_STATION.md)
for the exact Pi commands and override precedence. This is separate from your UI
login; there is no built-in default or browser setting that reveals it.

For Docker or an older external receiver, the video secret is `GS_VIDEO_PASSWORD`,
supplied when the receiver was deployed.

SSH into the Ground Station using your normal account, then change to its repository
directory. For the Docker deployment, list containers and find the `video` service:

```sh
docker ps --format 'table {{.ID}}\t{{.Names}}\t{{.Image}}'
```

Replace `VIDEO_CONTAINER_ID` below with that container's ID. This prints only the
configured camera publishing password from the running receiver:

```sh
docker inspect VIDEO_CONTAINER_ID --format '{{range .Config.Env}}{{println .}}{{end}}' \
  | sed -n 's/^MTX_AUTHINTERNALUSERS_0_PASS=//p'
```

The repository's Compose overlay sets this value from `GS_VIDEO_PASSWORD`. The
`web` container also receives that variable; inspect it if needed:

```sh
docker inspect WEB_CONTAINER_ID --format '{{range .Config.Env}}{{println .}}{{end}}' \
  | sed -n 's/^GS_VIDEO_PASSWORD=//p'
```

These commands display a secret in your terminal; do not paste their output into
issues or shared logs. Use `sudo docker` if your account requires it. Inspecting
running containers does not require the original Compose environment to be loaded.

The deployment may also keep the value in the repository's `.env`, a file passed
with `docker compose --env-file`, or a service environment file. For a native
receiver, check its MediaMTX/service configuration and the backend service's
`GS_VIDEO_PASSWORD`. A Pi sender installed using the repository guide stores its
copy in `/etc/gs-video-sender/environment` (requires privileged read access).

If no video receiver was deployed, there may be no secret yet; configure one in
the next section. Reuse an existing deployment's secret rather than changing it
just for this test.

## 2. Set up the receiver if needed

For native/systemd deployments, follow [Ground Station receiver setup](SETUP_GROUND_STATION.md).
The updated backend installs and starts MediaMTX on the Ground Station machine.
Do not run the Docker stack alongside a native backend using the same ports.

For a Docker deployment only, skip this if the video overlay already runs. On the
Ground Station, from the repository root, set a strong secret and its reachable IP:

```sh
export GS_VIDEO_HOST=192.168.7.3
export GS_VIDEO_PASSWORD='REPLACE_WITH_A_LONG_RANDOM_SECRET'
docker compose -f docker-compose.yml -f docker-compose.video.yml build --build-arg FRONTEND_DEV=TRUE
docker compose -f docker-compose.yml -f docker-compose.video.yml up -d --no-build
```

Keep these settings in your deployment's private environment configuration for
future Compose operations; shell exports last only for that shell. Do not commit
the secret. Changing it requires updating both receiver/backend and all publishers.

Required reachable ports from the publishing computer are TCP **8554** (publishing), TCP **3000**
(HTTPS UI), and UDP/TCP **8189** (live WebRTC). MediaMTX API/HLS/signaling ports
stay internal to Docker. `GS_VIDEO_HOST` must be the LAN address, not a container IP.

## 3. Publish from macOS

Install FFmpeg if needed:

```sh
brew install ffmpeg
```

In macOS's default **zsh**, enter the existing video password at the hidden prompt:

```sh
read -s "GS_VIDEO_PASSWORD?Video password: "
echo
export GS_VIDEO_PASSWORD
VIDEO_PASSWORD_ENCODED=$(python3 -c 'import os, urllib.parse; print(urllib.parse.quote(os.environ["GS_VIDEO_PASSWORD"], safe=""))')
```

Publish a moving 720p test pattern with browser-compatible H.264 and one-second
keyframes. `mac_test` is the stream ID; choose another unique ID if it is in use.

```sh
ffmpeg -re -f lavfi -i 'testsrc2=size=1280x720:rate=30' \
  -an -c:v libx264 -preset ultrafast -tune zerolatency \
  -profile:v baseline -pix_fmt yuv420p \
  -b:v 2M -maxrate 2M -bufsize 2M \
  -g 30 -keyint_min 30 -sc_threshold 0 -bf 0 \
  -f rtsp -rtsp_transport tcp \
  "rtsp://camera:${VIDEO_PASSWORD_ENCODED}@192.168.7.3:8554/mac_test"
```

Leave FFmpeg running. Its output and process arguments can contain the authenticated
URL; redact credentials before sharing diagnostics. Stop with Ctrl-C, then clear
the shell variables with `unset GS_VIDEO_PASSWORD VIDEO_PASSWORD_ENCODED`.

The repository's Pi sender uses `rpicam-vid`; this FFmpeg command substitutes for
that capture pipeline on a Mac while exercising the same receiver path.

### Use the Mac webcam instead

With the password variables above still set, list macOS capture devices:

```sh
ffmpeg -f avfoundation -list_devices true -i ""
```

This lists video and audio devices, then may exit with an input error because it
is only enumerating devices. Find the webcam's index under **video devices**.
Allow camera access for your terminal application when macOS prompts; if denied,
enable it in **System Settings → Privacy & Security → Camera**.

Stop the test-pattern publisher first if reusing `mac_test`. In the command below,
replace `0` in `0:none` with the webcam's video index; `none` disables audio:

```sh
ffmpeg -f avfoundation -framerate 30 -video_size 1280x720 -i '0:none' \
  -an -c:v libx264 -preset ultrafast -tune zerolatency \
  -profile:v baseline -pix_fmt yuv420p \
  -b:v 2M -maxrate 2M -bufsize 2M \
  -g 30 -keyint_min 30 -sc_threshold 0 -bf 0 \
  -f rtsp -rtsp_transport tcp \
  "rtsp://camera:${VIDEO_PASSWORD_ENCODED}@192.168.7.3:8554/mac_test"
```

If the camera rejects the requested size or frame rate, select a supported mode
from FFmpeg's diagnostics. Close other camera applications if capture cannot
start. Follow the same UI checks below; waving at the camera makes both motion
and audience delay easy to verify. This test sends video only.

## 4. Validate Ground Station to UI

1. Open `https://192.168.7.3:3000/media` for the Docker TLS setup, or `/media` on
   your existing HTTPS reverse proxy for a native backend. Native backend port
   3000 itself is HTTP; MediaMTX does not add TLS. For the repository's self-signed TLS
   setup, accept the certificate for your known Ground Station host if prompted.
2. Sign in on that page with operator or stream-manager access. Its login is
   independent of other tabs. Confirm `mac_test` appears and plays with motion.
3. Open the main UI's **Mission** view and confirm the feed plays there. Live
   previews require operator or stream-manager access.
4. Select **Settings → General → Viewing mode → Streamer** to test the audience
   path. Allow the configured delay (default 10 seconds) plus playback buffering.
   Ensure the stream is not hidden in broadcast controls. Ordinary viewers can
   watch the delayed program without stream-manager access.

Success means continuous FFmpeg publishing, a moving live preview, and moving
delayed audience video. A listed stream alone does not prove browser playback.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Connection refused or timeout publishing | From the publishing computer run `nc -vz 192.168.7.3 8554`; verify the video container, routing, and firewall. |
| RTSP authorization failure | Use username `camera` and the running receiver's publishing secret. The command above URL-encodes special characters. |
| Publisher rejected on an existing path | Choose a unique stream ID; the receiver disables publisher replacement. |
| Receiver unavailable or no stream listed | Verify the overlay configures `GS_VIDEO_API_URL=http://video:9997` and the backend/receiver secrets match. |
| Stream listed but live video never connects | Verify `GS_VIDEO_HOST=192.168.7.3` and UDP/TCP 8189 reach the receiver; check preview permissions. |
| Live preview works but delayed program fails | Check stream visibility, wait for buffering, and verify `GS_VIDEO_HLS_URL=http://video:8888` on the backend. |

On the Ground Station, inspect container logs using the IDs from `docker ps`:

```sh
docker logs --tail 100 VIDEO_CONTAINER_ID
docker logs --tail 100 WEB_CONTAINER_ID
```

See [receiver and Pi sender setup](../docs/backend/video-and-models.md) and
[broadcast roles and delayed playback](../docs/backend/broadcast-studio.md) for further details.
