# Crew voice

Open `/radio` on the Ground Station's HTTPS origin, or select **Crew voice** in
the navigation menu. The embedded voice session stays mounted while you use
other dashboard pages. Sign in with an application Admin account, or an account with view access and explicit
**voice transmit** permission, then select **Join voice** and approve microphone
access. Anonymous users and ordinary viewers cannot join the transmitting channel.
They can listen through the audience program when a stream manager enables it.

## Grant permission to talk

Application admins inherit voice permission. Other accounts require an explicit
voice grant, including hardware operators and stream admins. In `backend/tools/users_config_gui.py`, check **Transmit Crew Voice** on
the account and save the configuration. From the Ground Station repository you
can also change just this permission for an existing user:

```sh
python3 backend/tools/users_config_gui.py --cli set-voice-permission USERNAME allow
```

Use `deny` to revoke it. With a custom users file, add `--file /path/to/users.json`
before the subcommand. The stored field is `permissions.voice_transmit` (default
false). The user must also have view access. Active connections recheck current
permission every second and disconnect when it is revoked or the account is disabled.
Changing this permission does not grant hardware-control or broadcast-management access.

## Include comms in the audience stream

In **Mission → Stream controls → Broadcast studio**, check **Include crew voice (VC)** under **Audience audio**, then select **Apply broadcast settings**. This
requires stream-manager/admin access and is off by default. Connected crew see a
notice when their audio is included in the broadcast.

In the audience program (including Streamer mode), crew comms play with the camera
sound through one stream mute/volume control. Sound starts automatically when the
webview allows it; otherwise select **Unmute stream** once. There is no separate
crew-audio join or enable control for viewers. Receive-only playback also works
on plain LAN HTTP; microphone transmission still requires HTTPS. The audience player never requests a microphone or opens a
transmit socket. Normal audience view permissions, including configured guest
viewing, apply; viewers do not need transmit permission.

The backend withholds audio for the configured video delay plus the program's
2.5-second playback margin. Short client buffering/network jitter can still cause
small alignment differences. It never substitutes live comms when the delayed
program is unavailable. Disabling the option clears the temporary audio buffer.
Only audio received while the option is enabled enters that buffer; private
conversation before enabling is not replayed. Restarting the backend also clears it.

This adds comms to the browser audience program. It does not mux voice into raw
camera MP4 archives or into standalone camera HLS URLs, and does not save a voice
recording. Audio is held only in a bounded in-memory delay buffer. (Crew voice
recording is a separate opt-in; see "Recording" below.)

The default is **push to talk**: hold the button or Space while the voice page has
focus. Space does not activate while typing or using another control. Release,
window blur, and hiding the page release push-to-talk. **Open mic** allows a
continuous call. **Mic off · listen only** stops transmission while keeping incoming audio connected. **Mute microphone** overrides either transmitting mode. **Leave voice**, sign
out, a disconnected microphone, or a lost server connection releases capture.
There is no automatic reconnect that silently turns a microphone back on.

Select the microphone before joining; leave and rejoin to change it. Device names
may remain hidden until the browser grants permission. Microphone gain is 0–200%;
speaker volume is 0–150%. Browser echo cancellation is requested. **Noise suppression** can be turned on or off before joining or during a call when the browser/device supports it; unsupported devices show the control disabled. Changing transmit mode or suppressing noise does not grant new permissions; headphones remain useful. The input meter shows the enabled microphone
level. The roster indicates whose microphone is open, not a speech detector.

## Connection and deployment

Voice runs over `/api/voice/ws` on the same HTTPS/WebSocket connection as the UI.
The backend relays audio between participants; it requires no separate voice
server, STUN/TURN configuration, MediaMTX, or radio hardware. The existing nginx
WebSocket proxy configuration covers this path. Browser microphone access requires
HTTPS or localhost; plain HTTP on a remote LAN IP will not work.

Participants must have an IP route to the Ground Station, through the LAN, an
existing long-distance data link, or a VPN. This feature does not itself provide
a radio link or extend network range. Backend relaying avoids direct browser-to-
browser NAT connectivity requirements. TCP congestion can still delay voice.

One shared channel supports up to 12 authorized crew participants. Audio is mono 16 kHz
PCM16, 20 ms per packet: approximately 256 kbit/s per transmitting participant,
plus protocol overhead. The station sends each speaker to every other listener.
This favors broad browser support over bandwidth efficiency; capacity-plan your
link for the number of simultaneous speakers. Browser buffering and server queues
are bounded. Old/excess audio is dropped rather than accumulating without limit.

Voice is not recorded unless the deployment opts in (below). Watch audio
(below) is decoded into the same PCM16 pipeline as browser audio, so the
audience program buffer, the radio bridge and the recorder treat it exactly
like browser audio. HTTPS encrypts traffic in transit; the Ground Station
relays decoded PCM, so this is not end-to-end encryption between participants.
It grants no hardware command access.

## Watches

The Apple Watch and Wear OS companions join the same channel with a scoped
watch token (`docs/backend/watch-direct.md`): the phone asks for `"voice":
true` when issuing it, and the station grants voice only to an account with
the voice transmit permission, rechecked every second like a browser session.
Watches send Opus instead of PCM (about 24 kbit/s instead of 256 kbit/s per
speaker, which matters on watch LTE and Bluetooth). The station decodes each
Opus packet to one PCM16 frame for browsers, the program buffer and the radio
bridge, and encodes PCM speakers to Opus once per frame for Opus listeners
(libopus, 16 kHz mono, VoIP mode). Opus packets from a watch are forwarded to
other Opus listeners unchanged.

**One person's phone and watch never echo.** The station groups a phone
session with the watch whose token that session issued (the token row stores
the issuing session's fingerprint). Within a group no audio is relayed. While
the watch is joined and has not yielded (`{"type":"active","enabled":false}`),
every other member of its group is *held*: its microphone frames are ignored,
it receives no audio, `transmitting` is refused and it receives
`{"type":"handoff","held":true,"by":"watch"}`; the crew voice page then shows
that the watch has voice and switches open mic to listen-only. When the watch
leaves or yields, the held page receives `held:false` and listens again; its
microphone needs a new press. Other groups, including other people signed in
with the same shared account, are unaffected. Code: `backend/src/voice/pairing.rs`.

## Protocol

1. Upgrade `/api/voice/ws`; within five seconds send
   `{"type":"auth","token":"SESSION_TOKEN"}` (optionally `"codec":"opus"`;
   the default is `"pcm16"`). Tokens are sent inside the connection,
   not in URL query parameters. Invalid, expired, and anonymous sessions are rejected.
2. Receive `{"type":"joined","id":N,"sample_rate":16000,"codec":"pcm16","frame_samples":320,"device":"web"}`
   and `roster` updates (`peers[]`: `id`, `name`, `transmitting`, `device` = `web|watch`).
3. Send `{"type":"transmitting","enabled":true}` before audio, or `false` to mute.
4. PCM16 client binary frames contain exactly 640 bytes: 320 little-endian signed PCM16
   samples; Opus clients send one packet (1–400 bytes) that decodes to exactly
   320 samples. Anything else closes the connection. Server binary frames prepend the authenticated sender's four-byte
   little-endian ID to the payload in the listener's codec. Clients cannot choose another sender ID.
   Watches may send `{"type":"active","enabled":…}`; held peers receive `handoff`.
5. Send `{"type":"ping"}` every ten seconds while idle. Accounts and sessions are
   rechecked every second; dead clients are removed after 40 seconds without
   messages. A socket closing removes its participant immediately.

Audio and control messages are rate-limited, and outgoing listener queues are
bounded. There are no persistent voice files or database tables.

`GET /api/voice/status` returns `can_transmit` and `broadcast_enabled` for a
view-authorized session. `broadcast.comms_audio_enabled` is persisted with the
existing revision-checked `POST /api/live_streams/control` settings.
`GET /api/media-assets/program/audio?ticket=PROGRAM_TICKET&after=CURSOR` is
receive-only: it returns `enabled`, `generation`, `cursor`, and delayed `frames`
(`id` and base64 `pcm`). The server chooses the release delay, not the viewer.
The page polls at 100 ms while listening; base64/HTTP overhead adds to the voice
bandwidth figures above. Audience listeners do not consume crew participant slots.

## Validation

Run backend tests and the audio processor tests:

```sh
cargo test -p groundstation_backend --no-default-features
(cd ui && node tests/station-voice_audio.cjs)
```

`voice::tests` include a fake watch client: it joins over a real socket with
a watch token and Opus next to a PCM browser client, and checks both hear each
other, that watch audio reaches the program buffer, the phone/watch handoff,
and that a token without the voice grant (or an account without voice
permission) is refused.

For the optional browser test, start the existing isolated media fixture (no
hardware tasks or MediaMTX are started):

```sh
GS_MEDIA_TEST_DIR=/tmp/gs-voice-fixture \
GS_STAGE_MODELS_DIR=/tmp/gs-voice-fixture/models \
GS_VIDEO_RECORDINGS_DIR=/tmp/gs-voice-fixture/recordings \
cargo test -p groundstation_backend media::tests::serve_synthetic_media -- --ignored --nocapture
```

Create an MP4 fixture under
`/tmp/gs-voice-fixture/recordings/test_camera/2026-09-17_12-00-00-000001.mp4`
(any short H.264/AAC clip works, for example
`ffmpeg -f lavfi -i testsrc2=size=320x240:rate=15 -f lavfi -i sine=frequency=440 -t 4 -c:v libx264 -pix_fmt yuv420p -c:a aac <path>`;
the archive tests fail without it), wait
at least five seconds for its modification time to settle, then run
`(cd ui && node tests/station-voice_browser.mjs)` with Playwright installed. `PLAYWRIGHT_MODULE` can
point to its `index.mjs`; `CHROME_BINARY` can select an installed Chrome executable.
The test uses fake microphones and synthetic fixture sessions. Never run this
fixture as a production service. Stop the fixture after testing.


To check the buffered player independently of a station, install Playwright and
FFmpeg and run `(cd ui && node tests/station-program_browser.mjs)`. `PLAYWRIGHT_MODULE` and
`CHROME_BINARY` select an existing Playwright installation and Chrome binary.
The test generates temporary H.264/fMP4 segments and intercepts fixture requests;
it starts no MediaMTX instance or hardware tasks. It checks advancing playback,
portrait layout, the always-visible status-bar diagram, camera loss, delay changes,
and unified audience sound without microphone access or AudioWorklet support.

Application administrators inherit voice transmit and stream management access, including existing sessions. Other roles still need an explicit voice transmit grant. Joining reports audio, permission and connection progress beside the Join button; failed startup releases the microphone and permits retry.

An expired session or a failed access request is reported separately from a voice
permission denial. Voice capability responses are not cached. A username such as
`admin` or hardware-command access does not itself grant the application `admin`
role. The SSH terminal tool in `docs/application-administration.md` can inspect
and change roles without a graphical desktop.

## Recording

Crew voice recording is off by default. Set `GS_VOICE_RECORDINGS_DIR` on the
Ground Station service to turn it on. Tell the crew before you do.

When it is on, `backend/src/voice/recorder.rs` writes every crew and radio
frame the station relays:

- **Files.** One file per transmission per speaker,
  `<dir>/<stream>/<start_ms>-<speaker>.wav`. `stream` is `crew` (browser and
  watch push-to-talk) or `radio` (the RRLS bridge), and `start_ms` is the
  Unix time of the first frame. The audio is 16 kHz mono PCM16 WAV, about
  1.9 MB per minute of speech.
- **Bursts.** A transmission ends after one second without frames, or after
  ten minutes. Shorter gaps are written as silence so the file keeps real
  time.
- **Commentary.** Programme commentary is not recorded.
- **No effect on live voice.** Frames go through a bounded queue
  (`try_send`, 500 frames) to one writer thread that does all file I/O. A
  full queue or a disk error drops audio from the recording, never from the
  live channel.
- **Retention.** Files are kept until removed, as camera recordings are.
  There is no quota.

Replay bundles include the bursts that overlap a recording with `?voice=1`.
This needs the voice permission (`replay-bundle.md`, "Recorded media").

