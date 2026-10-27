# Set up a Raspberry Pi camera streamer

This turns a Raspberry Pi into a camera box: every USB webcam plugged into it
(and a Pi ribbon camera, if fitted) streams to Ground Station automatically,
including each webcam's built-in microphone. It starts on its own every time
the Pi powers on.

**Time:** about 20 minutes, most of it waiting for the build.

## What you need

- A Raspberry Pi 4 or 5 (1 GB RAM is enough) with a microSD card of 16 GB or more.
- One or more USB webcams. Most standard webcams work; one with a built-in
  microphone gives you audio too. Optionally a Pi ribbon (CSI) camera.
- A good power supply: the official Pi 4 (3 A) or Pi 5 (5 A) supply. Cameras
  draw power from the Pi, and a weak supply causes reboots or dropped cameras.
- A network the Pi and the Ground Station computer **both** join (same Wi-Fi
  or same switch), with internet access for the install.
- The Ground Station **video password** (step 3).

## 1. Put an operating system on the SD card

1. On any computer, install and open [Raspberry Pi Imager](https://www.raspberrypi.com/software/).
2. Choose your Pi model, then **Raspberry Pi OS Lite (64-bit)** or
   **Ubuntu Server (64-bit)**. Either works.
3. Choose the SD card, then click **Next → Edit settings**:
   - Set a hostname, e.g. `cam1` (use a different one for each camera Pi).
   - Set a username and a strong password.
   - Enter Wi-Fi details if the Pi will use Wi-Fi.
   - On the **Services** tab, turn on **Enable SSH**.
4. Write the card, put it in the Pi, connect cameras and network, and power it on.
   Give it a few minutes the first time.

## 2. Connect to the Pi

From a computer on the same network:

```sh
ssh YOUR_USERNAME@cam1.local
```

If `cam1.local` is not found, look up the Pi's IP address in your router's
device list and use that instead (`ssh YOUR_USERNAME@192.168.1.50`).

Everything from here on runs **on the Pi** in that SSH window.

## 3. Get the video password

The Pi needs the same video password as the Ground Station. On the **Ground
Station computer**, read it with:

```sh
cat ~/GroundStation26/data/video/password
```

(Use the path of the Ground Station checkout if it lives elsewhere. If the
station was given an explicit `GS_VIDEO_PASSWORD`, use that value instead; see
[Ground Station receiver setup](SETUP_GROUND_STATION.md#get-the-camera-publishing-password).)

Keep it private; anyone with it can publish video to the station.

## 4. Download the Ground Station code

```sh
sudo apt update
sudo apt install -y git
git clone https://github.com/University-at-Buffalo-SEDS/GroundStation26.git
cd GroundStation26
```

## 5. Run the installer

```sh
sh video_sender/install_pi.sh
```

When asked, paste the video password from step 3 (it is not shown as you type)
and press Enter.

The installer then, without further questions:

- installs FFmpeg and camera/audio tools,
- installs Rust and builds the streamer (5–10 minutes on a 1 GB Pi),
- creates a background service that starts on every boot,
- starts streaming.

It finishes with a status summary and `Installed.`

## 6. Check that it works

Show which cameras the Pi found and what each is doing:

```sh
sudo -u gs-video gs-video-sender status
```

Each USB camera appears under `streams` with a name like
`usb-046d_0825_CE9EA890` (its model and serial number, so the name stays the
same across reboots and USB ports). Its `state` should be:

| State | Meaning |
| --- | --- |
| `running` | Streaming to Ground Station. Done. |
| `discovering groundstation` | Camera found, still looking for the station. Is Ground Station running and on the same network? |
| `retrying` | The camera or connection keeps failing. See [Troubleshooting](#troubleshooting). |
| `disabled` | Turned off in the settings (normal for `front` when no ribbon camera is fitted). |

Then open Ground Station's **Media** page (`https://GROUND_STATION_IP:3000/media`)
or the **Mission** view and confirm the camera plays with sound.

To watch the live log: `sudo journalctl -u gs-video-sender -f` (Ctrl-C to exit).

## 7. Use it

- **Power on the Pi and it streams.** Nothing to start by hand.
- **Plug cameras in or out at any time.** New cameras start within a few
  seconds; unplugged ones stop. No restart needed.
- **Moving networks?** Nothing to change: the Pi finds the Ground Station by
  itself each time, as long as both are on the same network.

## Changing settings

All USB cameras share one set of settings. To change resolution, frame rate,
bitrate or audio:

```sh
sudo nano /var/lib/gs-video-sender/config.json
```

Edit the `usb_cameras` section, for example:

```json
"usb_cameras": {
  "enabled": true,
  "width": 1280,
  "height": 720,
  "fps": 30,
  "bitrate": 2000000,
  "input_format": "mjpeg",
  "encoder": "libx264",
  "audio": true
}
```

- `width`/`height`/`fps`: must be a mode every camera supports (see
  [Troubleshooting](#troubleshooting) to check). 1280×720 at 30 fps suits most webcams.
- `bitrate`: bits per second per camera. Lower it (e.g. `1000000`) on a weak network.
- `audio`: `false` to stream video only.
- `enabled`: `false` to stop streaming USB cameras.

Save (Ctrl-O, Enter, Ctrl-X), then apply without rebooting:

```sh
sudo systemctl reload gs-video-sender
```

If the file has a mistake, the Pi keeps the old settings and logs the error.

**Changing the video password** (e.g. the station's password changed):

```sh
sudo nano /etc/gs-video-sender/environment
sudo systemctl restart gs-video-sender
```

The file contains a single line: `GS_VIDEO_PASSWORD=the-password`.

## Updating

```sh
cd ~/GroundStation26
git pull
sh video_sender/install_pi.sh
```

Your settings and password are kept.

## Troubleshooting

| Problem | What to do |
| --- | --- |
| Stuck on `discovering groundstation` | Make sure Ground Station is running and both are on the same network/subnet. The network must allow UDP 49362 and TCP 8554. Check the password matches step 3. |
| Camera stuck on `retrying` | The camera doesn't support the configured mode. List its modes with `v4l2-ctl --list-devices` then `v4l2-ctl -d /dev/video0 --list-formats-ext`, and pick a size/fps listed under **MJPG** (or set `"input_format": "yuyv422"` and use a YUYV mode). |
| No sound | Check the camera has a microphone: `arecord -l` should list it. Cameras without one stream video only. A separate USB microphone is not paired automatically. |
| Pi reboots or cameras drop out | Power problem. Use the official supply; with several cameras use a powered USB hub. |
| Video stutters or lags | Lower `bitrate`, `fps` or resolution. Each USB camera costs CPU on the Pi; a Pi 4 handles one or two 720p30 cameras comfortably. |
| A camera's stream name changed | Cameras without a serial number are named by USB port (`usb-port-…`). Keep them in the same port. |
| Installer fails | Re-run it; it is safe to repeat. Check the Pi has internet (`ping -c 2 github.com`) and the correct date (`date`). |

Need more detail? See [live video and stage models](../docs/backend/video-and-models.md)
and [automatic GroundStation discovery](DISCOVERY.md).

## Removing it

```sh
sudo systemctl disable --now gs-video-sender
sudo rm /etc/systemd/system/gs-video-sender.service /usr/local/bin/gs-video-sender
sudo rm -r /etc/gs-video-sender /var/lib/gs-video-sender
sudo userdel gs-video
```
