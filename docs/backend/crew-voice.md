# Crew voice

Open `/radio` on the Ground Station's HTTPS origin, or select **Crew voice** in
Mission. The link opens a separate tab so a call can continue while you use the
dashboard. Sign in with a Ground Station account that has view access and explicit
**voice transmit** permission, then select **Join voice** and approve microphone
access. Anonymous users and ordinary viewers cannot join the transmitting channel.
They can listen through the audience program when a stream manager enables it.

## Grant permission to talk

Transmission is denied by default, including for hardware operators and stream
admins. In `backend/tools/users_config_gui.py`, check **Transmit Crew Voice** on
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

In **Mission → Stream controls → Broadcast studio**, check **Include crew audio
in audience stream (delayed)**, then select **Apply broadcast settings**. This
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
recording. Audio is held only in a bounded in-memory delay buffer.

The default is **push to talk**: hold the button or Space while the voice page has
focus. Space does not activate while typing or using another control. Release,
window blur, and hiding the page release push-to-talk. **Open mic** allows a
continuous call. **Mute microphone** overrides either mode. **Leave voice**, sign
out, a disconnected microphone, or a lost server connection releases capture.
There is no automatic reconnect that silently turns a microphone back on.

Select the microphone before joining; leave and rejoin to change it. Device names
may remain hidden until the browser grants permission. Microphone gain is 0–200%;
speaker volume is 0–150%. Browser echo cancellation and noise suppression are
requested; headphones remain useful. The input meter shows the enabled microphone
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

Voice is not recorded. HTTPS encrypts traffic in transit; the Ground Station
relays decoded PCM, so this is not end-to-end encryption between participants.
It grants no hardware command access.

## Protocol

1. Upgrade `/api/voice/ws`; within five seconds send
   `{"type":"auth","token":"SESSION_TOKEN"}`. Tokens are sent inside the connection,
   not in URL query parameters. Invalid, expired, and anonymous sessions are rejected.
2. Receive `{"type":"joined","id":N,"sample_rate":16000}` and `roster` updates.
3. Send `{"type":"transmitting","enabled":true}` before audio, or `false` to mute.
4. Client binary frames contain exactly 640 bytes: 320 little-endian signed PCM16
   samples. Server binary frames prepend the authenticated sender's four-byte
   little-endian ID. Clients cannot choose another sender ID.
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
node tests/voice_audio.cjs
```

For the optional browser test, start the existing isolated media fixture (no
hardware tasks or MediaMTX are started):

```sh
GS_MEDIA_TEST_DIR=/tmp/gs-voice-fixture \
GS_STAGE_MODELS_DIR=/tmp/gs-voice-fixture/models \
GS_VIDEO_RECORDINGS_DIR=/tmp/gs-voice-fixture/recordings \
cargo test -p groundstation_backend media::tests::serve_synthetic_media -- --ignored --nocapture
```

Create an MP4 fixture under
`/tmp/gs-voice-fixture/recordings/test_camera/2026-09-17_12-00-00-000001.mp4`, wait
at least five seconds for its modification time to settle, then run
`node tests/voice_browser.mjs` with Playwright installed. `PLAYWRIGHT_MODULE` can
point to its `index.mjs`; `CHROME_BINARY` can select an installed Chrome executable.
The test uses fake microphones and synthetic fixture sessions. Never run this
fixture as a production service. Stop the fixture after testing.
