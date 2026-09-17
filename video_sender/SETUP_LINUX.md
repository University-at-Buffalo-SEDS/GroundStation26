# Test video from Linux through Ground Station

This validates **Linux FFmpeg → RTSP/TCP → Ground Station MediaMTX → backend → browser**,
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

## 3. Publish from Linux

On an Ubuntu/Debian publishing computer, install FFmpeg, Python, camera utilities,
and the connection-check tool:

```sh
sudo apt update
sudo apt install ffmpeg python3 v4l-utils netcat-openbsd
```

For other distributions, install the equivalent packages with your package manager.
The FFmpeg build must include the `libx264` encoder and V4L2 input support for webcams.
No local Ground Station backend is needed: this computer publishes to `192.168.7.3`.

In **Bash**, enter the receiver's existing password and encode it for the RTSP URL:

```bash
read -r -s -p 'Video password: ' GS_VIDEO_PASSWORD
echo
export GS_VIDEO_PASSWORD
VIDEO_PASSWORD_ENCODED=$(python3 -c 'import os, urllib.parse; print(urllib.parse.quote(os.environ["GS_VIDEO_PASSWORD"], safe=""))')
```

### Start with a test pattern

```bash
ffmpeg -re -f lavfi -i 'testsrc2=size=1280x720:rate=30' \
  -an -c:v libx264 -preset ultrafast -tune zerolatency \
  -profile:v baseline -pix_fmt yuv420p \
  -b:v 2M -maxrate 2M -bufsize 2M \
  -g 30 -keyint_min 30 -sc_threshold 0 -bf 0 \
  -f rtsp -rtsp_transport tcp \
  "rtsp://camera:${VIDEO_PASSWORD_ENCODED}@192.168.7.3:8554/linux_test"
```

Leave it running while checking playback in section 4. Stop it with Ctrl-C before
starting the webcam on the same stream ID. Use a unique stream ID for each simultaneous publisher.

### Use a Linux USB or built-in webcam

List cameras and inspect the supported capture modes for the selected device:

```sh
v4l2-ctl --list-devices
v4l2-ctl --device=/dev/video0 --list-formats-ext
```

Replace `/dev/video0` with your camera's capture device. A camera may expose several
nodes; select one with video capture formats, not a metadata-only node. The example
below requires **MJPG at 1280×720, 30 fps** in the mode list:

```bash
ffmpeg -f v4l2 -input_format mjpeg -framerate 30 -video_size 1280x720 \
  -i /dev/video0 \
  -an -c:v libx264 -preset ultrafast -tune zerolatency \
  -profile:v baseline -pix_fmt yuv420p \
  -b:v 2M -maxrate 2M -bufsize 2M \
  -g 30 -keyint_min 30 -sc_threshold 0 -bf 0 \
  -f rtsp -rtsp_transport tcp \
  "rtsp://camera:${VIDEO_PASSWORD_ENCODED}@192.168.7.3:8554/linux_test"
```

Match `-video_size` and `-framerate` to a supported mode. If the camera only offers
YUYV for your selected mode, replace `-input_format mjpeg` with
`-input_format yuyv422`. If changing the frame rate, set `-g` and `-keyint_min` to
the same number for approximately one-second keyframes. This sends video only.
Pi CSI cameras using `rpicam-vid` should use the
[Pi sender setup](../docs/backend/video-and-models.md#build-and-install-the-sender-on-the-pi).

If opening the device returns permission denied, inspect its ownership and your
groups with `ls -l /dev/video0` and `id`. On systems where the device belongs to the
`video` group, grant your account access with:

```sh
sudo usermod -aG video "$USER"
```

Log out and back in before retrying. If the device is busy, close other camera
applications. For a remote Linux publisher, run these capture commands on the
Linux machine that physically owns the webcam.

FFmpeg output and process arguments can contain the authenticated URL; redact it
before sharing logs. When finished, stop FFmpeg with Ctrl-C and clear the variables:

```sh
unset GS_VIDEO_PASSWORD VIDEO_PASSWORD_ENCODED
```

## 4. Validate Ground Station to UI

1. Open `https://192.168.7.3:3000/media` for the Docker TLS setup, or `/media` on
   your existing HTTPS reverse proxy for a native backend. Native backend port
   3000 itself is HTTP; MediaMTX does not add TLS. For the repository's self-signed TLS
   setup, accept the certificate for your known Ground Station host if prompted.
2. Sign in on that page with operator or stream-manager access. Its login is
   independent of other tabs. Confirm `linux_test` appears and plays with motion.
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
