# Ground Station receiver: native/systemd setup

Run these steps **on the computer running Ground Station**, such as the Pi at
`192.168.7.3`. A Mac or Linux camera publisher only needs FFmpeg; it does not need
MediaMTX installed locally.

## Automatic installation and startup

The updated `groundstation_backend` manages MediaMTX automatically on native
Linux and macOS deployments unless external relay URLs or `GS_VIDEO_MANAGED=0`
are configured. Your existing systemd service can keep its current `ExecStart`.
This adds video receiving; it does not install camera software on publishers or
replace your HTTPS reverse proxy, frontend bundle, or hardware configuration.

After transferring the updated repository to the Pi, build the backend there:

```sh
cd /home/rylan/Documents/Git/GroundStation26
python3 backend/build.py pi_build
```

Optionally pin the advertised WebRTC address to the Pi's LAN IP:

```sh
sudo systemctl edit sed-ground-station.service
```

Enter:

```ini
[Service]
Environment=GS_VIDEO_HOST=192.168.7.3
```

Without this setting, MediaMTX advertises addresses from its network interfaces.
Keep any existing `GS_VIDEO_PASSWORD` environment file if you already configured
one; it takes precedence over the generated password. An existing separate
MediaMTX service must either be stopped before switching to managed mode, or kept
with `GS_VIDEO_MANAGED=0` and matching backend credentials.

When ready to interrupt the running Ground Station session:

```sh
sudo systemctl daemon-reload
sudo systemctl restart sed-ground-station.service
sudo journalctl -u sed-ground-station.service -f
```

At web-server startup the backend:

1. Creates a private, persistent `data/video/password` if `GS_VIDEO_PASSWORD` is
   absent or empty. It uses the same secret for publishing and backend relay access.
2. Downloads **MediaMTX v1.21.0** for the Ground Station machine, verifies its pinned
   SHA-256 digest, and extracts its executable into `data/video/`. First startup
   requires access to GitHub, system CA certificates, and `tar`; later starts use
   the cached binary without downloading it again. Supported automatic downloads
   are Linux ARM64/AMD64 and macOS ARM64/AMD64.
3. Starts MediaMTX as a child of the backend, retries failures every 30 seconds,
   and terminates the child when the backend shuts down.

Downloads and receiver failures do not block telemetry startup. Check the journal
for failures; a spawned receiver is not proof that browser playback works.
Generated receiver configuration is rewritten at startup; do not edit that copy.
The service user must be able to write the runtime directory.

## Get the camera publishing password

With the shown service's working directory, read the generated secret **on the Pi**:

```sh
cat /home/rylan/Documents/Git/GroundStation26/data/video/password
```

This file is owned by the service user with mode `0600`. The generated value is
passed internally to the backend and child; it is not added to the backend's
process environment. Do not expect `/proc/.../environ` to show a generated secret.

If you supplied `GS_VIDEO_PASSWORD` explicitly, use the value in the service's
environment file instead. An old generated file is not authoritative while that
override is set. To inspect the running service's explicit override:

```sh
service_pid=$(systemctl show sed-ground-station.service --property=MainPID --value)
sudo cat "/proc/$service_pid/environ" | tr '\0' '\n' | sed -n 's/^GS_VIDEO_PASSWORD=//p'
```

Keep the output private. Use publisher username `camera`, this password, and an
RTSP URL such as `rtsp://camera:PASSWORD@192.168.7.3:8554/mac_test`.
The publisher guides URL-encode the password for you.

## Verify receiving and playback

```sh
sudo ss -ltnp 'sport = :8554'
systemctl status sed-ground-station.service --no-pager
```

Allow TCP **8554** for RTSP and UDP/TCP **8189** for WebRTC from your camera/viewer
network. API (9997), signaling (8889), and HLS (8888) bind to loopback; the backend
provides authenticated browser access. The backend's native port 3000 is HTTP.
Remote authenticated browser access still needs your existing TLS reverse proxy;
do not assume that starting MediaMTX adds HTTPS.

Use the [macOS publisher guide](SETUP_MACOS.md) or
[Linux publisher guide](SETUP_LINUX.md) to send a test pattern/webcam and verify
live playback on `/media` and Mission, then delayed playback in Streamer mode.

## Optional: manually install MediaMTX on the Pi

Normally automatic installation above is sufficient. For a manually provisioned
**64-bit Linux Pi 5** (`uname -m` reports `aarch64`), run these commands on the Pi.
They download the same pinned [official release](https://github.com/bluenviron/mediamtx/releases/tag/v1.21.0):

```sh
mkdir -p /tmp/groundstation-mediamtx-install
cd /tmp/groundstation-mediamtx-install
curl -fL --retry 3 -o mediamtx.tar.gz \
  https://github.com/bluenviron/mediamtx/releases/download/v1.21.0/mediamtx_v1.21.0_linux_arm64.tar.gz
printf '%s  mediamtx.tar.gz\n' a8113b5928ba1a934b81557b61b8a07954b76921a4b567d54c7f086f8b39d9a2 | sha256sum --check
```

Only after the checksum reports **OK**, extract and install:

```sh
tar -xzf mediamtx.tar.gz mediamtx
sudo install -m 0755 mediamtx /usr/local/bin/mediamtx
```

Add this setting to the Ground Station service's `[Service]` override, then reload
systemd and restart Ground Station as above:

```ini
Environment=GS_MEDIAMTX_BINARY=/usr/local/bin/mediamtx
```

The backend supplies configuration and credentials and supervises this executable;
do not also start a separate MediaMTX service on the same ports. The override skips
the automatic download; you are responsible for the installed binary's version.

## Configuration reference

| Variable | Behavior |
| --- | --- |
| `GS_VIDEO_MANAGED` | `auto` (default), `1` to require managed configuration, or `0` for an external receiver. |
| `GS_MEDIAMTX_BINARY` | Installed executable path to use instead of downloading the pinned release. |
| `GS_VIDEO_RUNTIME_DIR` | Cache/config/secret directory; defaults to `data/video` relative to the backend working directory. |
| `GS_VIDEO_PASSWORD` | Explicit shared secret; otherwise managed mode creates/reuses its password file. |
| `GS_VIDEO_HOST` | Additional reachable WebRTC host/IP; local interfaces are also advertised. |
| `GS_VIDEO_API_URL`, `GS_VIDEO_WEBRTC_URL`, `GS_VIDEO_HLS_URL` | Any override selects external mode by default, preserving Docker Compose behavior. Explicit managed mode with these overrides is rejected and logged. |

To use an existing native external receiver at the default loopback addresses, set
`GS_VIDEO_MANAGED=0` and `GS_VIDEO_PASSWORD` in the backend service. Docker's video
overlay already sets relay URLs and credentials and continues to manage its own
MediaMTX container.


## Video in the Linux native app (including Raspberry Pi)

The native Linux UI uses WebKitGTK and GStreamer codecs installed on the machine
**running the UI**. A working FFmpeg publisher or MediaMTX receiver does not prove
that the native UI has an H.264 decoder. On Raspberry Pi OS/Ubuntu/Debian:

```sh
sudo apt update
sudo apt install gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
gst-inspect-1.0 avdec_h264
gst-inspect-1.0 h264parse
```

New `.deb` packages declare these playback dependencies. Install them with
`sudo apt install ./PACKAGE.deb` so apt resolves dependencies; copying a binary or
using an older package still needs the manual codec installation above.

Quit and relaunch the native app after installing codecs. If the UI runs on the
Pi, run these commands on the Pi; installing FFmpeg on a Mac does not install the
Pi's WebKit codecs. The app's server URL must point to the Ground Station's HTTP(S)
port, not MediaMTX's RTSP port. The station in this setup is `192.168.3.7`.

Mission and Streamer now use the same buffered HLS program, including operators.
There are no automatic unbuffered camera previews in Mission. HLS travels through
the backend on the same HTTP(S) connection as the program, avoiding direct WebRTC
ICE/UDP connectivity requirements. Use H.264 baseline/yuv420p with one-second
keyframes as in the sender commands in these guides. On a slow native device,
use the featured-camera layout (only one camera is decoded), and try 720p or lower.

The rocket diagram is always in the program status bar, including during buffering
and camera loss. The bar wraps on portrait screens without covering the picture.
Crew comms, when enabled by the stream manager, use the same mute/volume control
as camera audio. If autoplay is blocked, select **Unmute stream** once.

If video still fails, compare the same station in a browser and collect:

```sh
sudo journalctl -u sed-ground-station.service -n 80 --no-pager
sudo ss -ltnp 'sport = :8554'
gst-inspect-1.0 avdec_h264
```

Also note the native player's status message. Redact secrets before sharing logs.
See [WebKit's multimedia dependencies](https://docs.webkit.org/Ports/WebKitGTK%20and%20WPE%20WebKit/Multimedia.html).
