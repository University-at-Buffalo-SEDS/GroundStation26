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

## Capacity and talk floor

One shared channel holds up to **32** authorized participants by default,
and at most **4** of them are *on air* (forwarded) at once. Both are set on
the Ground Station service:

| Variable | Default | Range | Meaning |
| --- | --- | --- | --- |
| `GS_VOICE_MAX_PARTICIPANTS` | 32 | 2–128 | Joined participants (browsers, phones, watches, the broadcast studio's crew link). The next one is refused with `Voice channel is full (N participants)`; watches retry a full channel. |
| `GS_VOICE_MAX_TALKERS` | 4 | 1–8 | Talkers forwarded at once. Never more than the participant cap. |

Unset or unparsable values use the defaults; out-of-range values are
clamped. The backend logs the limits at start-up, and `joined` and
`GET /api/voice/status` report them (`max_participants`, `max_talkers`).
Audience listeners do not count.

### Why the old limit was 12

Before this change the cap was a hard-coded `MAX_PEERS = 12`. The relay
already forwarded through the station (no browser-to-browser mesh, no
per-listener re-encoding: Opus is decoded once per watch speaker and PCM is
encoded once per frame for Opus listeners), but **every participant could
transmit at once** (open mic) and every frame went to every other
participant. Browsers send PCM16, about 260 kbit/s per talker with framing,
so egress grew as talkers × listeners: 12 open mics already meant
12 × 11 × 260 kbit/s ≈ 34 Mbit/s, and 32 would have been ≈ 260 Mbit/s.
On top of that every participant's one-second status tick sent the full
roster to *every* participant, an N² message storm (32 peers: about 1,000
roster messages and 20 Mbit/s per second with nobody talking).

### The talk floor

The station now forwards only on-air talkers, so egress is bounded by
`max_talkers × listeners` regardless of how many people join. The rules
(`backend/src/voice/floor.rs`):

1. `{"type":"transmitting","enabled":true}` (push to talk pressed, or open
   mic) is a request for the floor. With a free slot it goes on air at once.
2. A talker on air keeps the floor while it asks for it; a newer request
   never interrupts it. Releasing push-to-talk frees the slot.
3. When the floor is full, requests wait in line: administrators first,
   then oldest first. The first waiter goes on air as soon as a slot frees.
4. **Quiet yield.** A talker on air whose audio has stayed below about
   −40 dBFS for 1.5 s gives its slot to a waiter (an open mic left on does
   not block the channel). It keeps its request and goes back on air when
   a slot frees.
5. **Administrator override.** An application administrator (for example
   the range safety officer's account) who asks when no slot is quiet takes
   the slot of the ordinary talker that has been on air longest. That
   talker returns to the line with `reason: "priority"`. Administrators are
   never overridden, except by the quiet yield. Losing the admin role takes
   effect at the next one-second recheck.

Audio from a participant that is not on air is dropped at the station. The
watch handoff (below) still applies first: a held phone cannot ask for the
floor. The radio bridge (RRLS) is a separate source and does not take a slot;
a talker's `radio` relay only carries audio while it is on air.

The station tells each participant its own state with
`{"type":"floor","state":"on_air"|"waiting"|"idle","reason":null|"priority"|"quiet","talkers":N,"max_talkers":K}`
whenever it changes, and the roster marks `transmitting` (on air) and
`waiting` for everyone. The roster is sent to everyone when anything
changes and to each participant once a second, so a dropped `floor` message
is repaired within a second. The crew voice page shows **Transmitting**
while on air and **Waiting for a free channel · 4 of 4 talking** while in
line, sends no audio while waiting, and lists people as *On air*, *Waiting
to talk* or *Listening*. Watches show **Waiting** on the push-to-talk
button.

### Bandwidth and CPU

Each listener receives one stream per on-air talker in its own codec:
PCM16 for browsers (≈ 260 kbit/s per talker with framing), Opus for
watches (≈ 27 kbit/s). Worst case with the defaults, four talkers and 31
PCM listeners, is 4 × 31 × 260 kbit/s ≈ 32 Mbit/s, the same as the old
12-person worst case; the usual case, one talker, is about 8 Mbit/s.
Plan Wi-Fi for about 1 Mbit/s per browser listener. The watch relay through
a phone carries at most four Opus streams (about 18 kB/s with base64).

#### Measurements behind the defaults

`voice::tests::voice_load_report` runs the station's voice routes on their
own four-thread runtime (a Raspberry Pi 5 has four cores) and drives bots
over loopback WebSockets: every fourth bot is an Opus (watch) client, all
ask for the floor, on-air bots send a 20 ms frame every 20 ms, halfway the
first talkers leave and waiters take over, then everyone leaves. CPU is the
station runtime's and SQLite workers' thread time (`/proc/self/task/*/schedstat`,
which includes their system calls); latency is talker send to listener
receive for PCM frames. Measured 2026-11-24 on the development workstation
(AMD Ryzen 9 7900X), release build, 20 s runs:

```sh
VOICE_LOAD_BOTS=32 VOICE_LOAD_TALKERS=4 VOICE_LOAD_SECONDS=20 \
cargo test -p groundstation_backend --release voice_load_report -- --ignored --nocapture
```

| Participants / talkers | Station CPU (host, one core) | Pi 5 estimate (×3.5) | Latency p50 / p99 | Station egress |
| --- | --- | --- | --- | --- |
| 12 / 12, everyone talking (old worst case)¹ | 6.9 % | ≈ 24 % of one core | 0.5 / 1.2 ms | 26 Mbit/s |
| 32 / 1 | 3.1 % | ≈ 11 % | 0.1 / 0.5 ms | 6 Mbit/s |
| **32 / 4 (defaults)** | **5.1 %** | **≈ 18 %** | **0.2 / 0.9 ms** | **23 Mbit/s** |
| 32 / 32, everyone talking (no floor)¹ | 33.7 % | ≈ 1.2 cores | 1.4 / 3.4 ms | 188 Mbit/s |
| 64 / 4 | 10.1 % | ≈ 35 % | 0.4 / 0.8 ms | 48 Mbit/s |
| 128 / 4 | 28.0 % | ≈ one core | 0.7 / 4.1 ms (max 60) | 99 Mbit/s |

¹ Everyone is on air from the start and leaves halfway, so these windows
are 10 s.

The Pi 5 column scales host CPU by 3.5, the ratio of single-core
benchmark scores between a 2.4 GHz Cortex-A76 and the 7900X; it was not
measured on a Pi. TLS adds AES-GCM over the egress (the Pi 5 has ARMv8
crypto instructions; 23 Mbit/s is about 3 MB/s, well under 1 % of a core),
and a real network interface adds driver work the loopback runs do not
include. Loopback latency shows the station adds well under a frame of
delay; the network and the clients' 40 ms playout buffer dominate.

**Why 32 and 4.** With the defaults the voice relay needs about a fifth of
one Pi 5 core (a twentieth of the four) and at most about 32 Mbit/s, the
same worst case as the old 12-person channel, leaving the telemetry, radio
and recording tasks their headroom and staying within what a field Wi-Fi
access point delivers. Without a talker cap, 32 open microphones would need
188 Mbit/s and more than a Pi core. Doubling participants doubles egress
(64: about 48 Mbit/s), so raise `GS_VOICE_MAX_PARTICIPANTS` only on a wired
or fast network; 128 needs about one Pi 5 core and 100 Mbit/s. Four talkers
cover crossing calls without letting the channel become a wall of sound;
each extra talker adds about 8 Mbit/s with 31 PCM listeners.

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
2. Receive `{"type":"joined","id":N,"sample_rate":16000,"codec":"pcm16","frame_samples":320,"device":"web","max_participants":32,"max_talkers":4}`
   and `roster` updates (`peers[]`: `id`, `name`, `transmitting` = on air, `waiting`, `device` = `web|watch`).
3. Send `{"type":"transmitting","enabled":true}` to ask for the floor, or `false` to release it.
   Send audio once `floor` says `on_air`; frames sent while waiting are dropped (see "The talk floor").
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
(cd ui && node tests/station-voice-floor_browser.mjs)   # Playwright; fake station socket
```

`voice::floor` unit tests cover the limits parsing and clamping, oldest-first
grants, hand-over on release, the quiet yield, the administrator override
and the cap for any mix of requests. `voice::tests::the_channel_cap_and_talk_floor_hold_for_thirty_two_participants`
joins 32 clients over real sockets, checks the 33rd is refused, and walks
the floor through waiting, release, override and quiet yield.
`voice::tests::thirty_two_bots_join_talk_and_leave_within_the_caps` is the
load test in the normal suite (32 bots, every fourth Opus, 4 s);
`voice_load_report` is the longer measurement run (see "Bandwidth and CPU").

`voice::tests` also include a fake watch client: it joins over a real socket with
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

