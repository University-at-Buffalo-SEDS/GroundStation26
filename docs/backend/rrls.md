# Rocket Recovery and Localization System (RRLS)

RRLS runs the GroundStation backend on a Raspberry Pi 5 with an RFD900x radio.
It needs no attached display or desktop session. The existing WebUI and mobile
API remain available over native HTTP (3000) and HTTPS (443), or Docker ports
80 and 443, through a Wi-Fi router connected to the Pi's
Ethernet port. This Wi-Fi network is separate from the main GroundStation's
network; it is **not** the relay backhaul. Both relay directions use the RFD mesh.

## Radio firmware and addressing

Use RFDesign's **asynchronous mesh** firmware on all three modems, not the
peer-to-peer firmware. Follow the [RFDesign asynchronous manual, sections 3–4](https://rfdesign.com.au/wp-content/uploads/2024/04/RFD900x-Asynchronous-User-Manual-V1.1.pdf).
The register map differs from peer-to-peer firmware:

| Register | Asynchronous meaning | Deployment |
| --- | --- | --- |
| S1 | Serial speed | 115 (115200 baud), matching the host UART |
| S2 | Air speed | 64 (64 kbps); keep below serial speed |
| S7 / S8 | TX / RX encapsulation | 0 / 0; the application provides framing |
| S9 | NETID | Same network ID on all three radios |
| S10 | NODEID | Unique, for example main=1, RRLS=2, rocket=3 |
| S11 | DESTID | Broadcast: 32768 for firmware V2.45F and later; 65535 on earlier versions |
| S12 | TXPOWER | Match the existing approved installation |
| S18 | RTSCTS | Match wiring; current host UART driver uses no RTS/CTS |

`S4` is **MAX_RETRIES**, not transmit power, on asynchronous firmware. Existing
peer-to-peer AT scripts must not be reused. Do not install the manual's fixed
`AT+F` forwarding rules for this application relay: they forward regardless of
the main station's direct-link health. RSSI is not used by this implementation.

## Operation

Set `GS_RRLS_ROLE=primary` on the main GroundStation and `GS_RRLS_ROLE=rrls` on
RRLS. Leaving it unset preserves the existing two-radio behavior.

The main station emits a small link-health message twice per second. After
three seconds without direct AV-bay packets or radio-window advertisements, it
requests the recovery path. Main-station uplinks keep attempting the direct
path until RRLS confirms readiness for the current session. That readiness
expires after two seconds: an unplugged relay cannot retain the command path.
RRLS sends readiness only during requested recovery, keeping standby silent.
RRLS forwards while that request's two-second lease
is fresh. Two seconds of continuous direct reception restores the direct path.
A relayed reception never counts as proof that the direct path has recovered.
If the main station disappears altogether, RRLS stops forwarding when the lease
expires but continues receiving and serving its own local recovery data.

In standby RRLS receives and records locally without transmitting its router's
ACKs, discovery, time-source announcements, or local flight commands. While
relaying, it forwards the main station's original command and scheduler bytes,
and returns AV-bay telemetry, opaque transport fragments, and acknowledgements.
Ground-to-rocket traffic uses a separate envelope even on the direct path, so
broadcast ground traffic cannot overwrite rocket-side compact-header templates.
The original SEDSnet bytes, identities and timestamps are preserved; schemas are
still learned dynamically. Recovery wrappers are single-hop and never repeated
by another recovery-wrapper handler.

GPS_DATA has priority 255 on RF and GroundStation. The relay queue sends GPS
first and coalesces pending canonical GPS values to the latest fix. Compact
transport frames retain their order because timestamp deltas may depend on prior
frames. A bounded 256-entry header classification cache recognizes compact GPS
without expanding its payload. It is limited to 64
frames / 64 KiB, drops lower-priority work first, and expires stale pending work.
Forwarding is capped at 2,000 framed UART bytes/second, with a 4,160-byte burst,
to leave airtime for the original rocket transmission and return traffic. This
is an application budget; modem RF overhead is additional. Normal traffic
expires after 250 ms; GPS after 1.5 seconds. Equal-priority work
keeps FIFO order. Queue drops are observable rather than an allocation failure.

RRLS local clients can view recovery telemetry, maps, and recordings and control
local recording. Flight/GSE commands are issued at the main station and relayed;
RRLS's local flight controls are read-only. GPIO panel setup and the autonomous
fill-safety task are disabled in this role. No fill-system UART is opened.

The WebUI displays the active direct/recovery path. `GET /api/rrls/status` also
reports role, forwarding, direct age, queue depth, and drop/forward counters.

## Build and run

On the Pi, build the WebUI and backend without local launch GPIO:

```sh
python3 build.py rrls --frontend-dev
```

RRLS uses `backend/comms/rrls.json` by default, separate from the primary station's
`backend/comms/comms.json`. It does not migrate primary/legacy configuration.
Choose the radio once with `python3 run_rrls.py --frontend-dev --radio-port /dev/cu.YOUR_RADIO`
on macOS, or `/dev/serial0` on the Pi. This selection is saved in the RRLS file.
Use `--comms-config FILE` or `GS_RRLS_COMMS_LINK_CONFIG` for a different RRLS file.
An explicit `GS_COMMS_LINK_CONFIG` remains supported; the RRLS-specific setting
has priority. An unplugged radio keeps the WebUI running and retries once per
second; no telemetry is fabricated while disconnected.

The saved communications file has this format:

```json
{
  "version": 1,
  "av_bay": {"interface": "serial", "port": "/dev/serial0", "baud_rate": 115200, "protocol": "raw_uart"}
}
```

RRLS accepts and saves this radio-only file; it creates no fill-system router
side or worker. The primary GroundStation still requires its own fill-system
configuration. Select the RRLS file with `GS_RRLS_COMMS_LINK_CONFIG`.

```sh
GS_COMMS_LINK_CONFIG=/etc/rrls/comms.json python3 run_rrls.py --backend-only-build
```

On macOS, run the same launcher with an unprivileged HTTPS port:

```sh
GS_HTTPS_BIND_ADDRESS=0.0.0.0:3443 python3 run_rrls.py --frontend-dev --radio-port /dev/cu.YOUR_RADIO
```

Open `https://localhost:3443`. Use the Mac's actual `/dev/cu.*` radio device in
`rrls-comms.json`. `run_rrls.py` accepts the same options as
`run_groundstation.py`, including `--debug`, `--backend-only-build`, and `--frontend-dev` (or `frontend_dev`);
it always enables RRLS mode. As with `build.py`, the frontend defaults to `main`;
`--frontend-dev` selects the matching development WebUI. Run `python3 run_rrls.py --help` to list options.

For boot-time operation, adapt [rrls.service](rrls.service) to the installed user
and paths. `GS_USERS_FILE` selects RRLS's own users file; provision accounts using
the existing GroundStation authentication process. Serve the existing frontend
bundle at `frontend/dist/public` under the service working directory. Connect the
phone/tablet to RRLS's router and use `https://<pi-address>` for a native
service, or `https://<pi-address>` with Docker. Both roles automatically generate
persistent self-signed certificates; see [HTTPS setup](https.md).

For Docker RRLS, use:

```sh
GS_TLS_NAMES=rrls.local,192.168.4.7 docker compose -f docker-compose.yml -f docker-compose.rrls.yml up -d --build
```

Replace the address with RRLS's actual LAN address. Configure/pass through its
radio serial device and communications file as described above. The HTTPS
configuration is shared with the main GroundStation; RRLS uses its own stored identity.

Deploy the matching RF-board firmware too: it ignores host-only recovery envelopes, validates/unwraps avionics uplinks
before generic network parsing, and gives GPS first priority.
The modem firmware and STM32 RF-board firmware are separate updates.

## Validation scope

Regression tests cover standby silence, lease expiry, corrupt/reordered health,
primary restarts, loop suppression, stable direct-path restoration, opaque
fragment preservation, FIFO controls, queue pressure, and GPS coalescing. A
600-second logical mesh test includes repeated direct-link outages and commands.
This is software validation; physical RFD range/collision behavior still needs a
three-radio field test. The earlier seven-board network qualification is
separate from this new recovery-mesh mode.

`--backend-only` / `--backend-only-build` reuses existing WebUI assets. Do not
combine it with `--frontend-dev`: run `python3 run_rrls.py --frontend-dev`
first to rebuild and serve the development UI.

## Native app handover and application identity

Pairing is opt-in. Provision a persistent application signing identity, independent
of HTTP, HTTPS certificates, or a reverse proxy:

```sh
python3 backend/tools/pair_stations.py --output /private/path/station-pair \
  --primary-url https://groundstation.local --rrls-url https://rrls.local
```

Install `primary/` on the primary station and `rrls/` on RRLS, outside the source
checkout. Set `GS_STATION_PAIRING_FILE` to each station's `pairing.json` and restart
the service. Keep `identity.seed` private; it is the installation's application
identity and session-signing key. Do not regenerate it for HTTPS certificate
renewal. The configured URLs may use HTTP or HTTPS and may point at a proxy.
TLS trust is still governed by the app's connection settings. HTTP retains its
normal lack of transport encryption.

Provision the same username on both servers with the intended local permissions.
Sign in once after enabling pairing to obtain a portable session. The destination
checks both the signed source grant and its current local account: a missing or
disabled account is rejected, and destination permissions cannot exceed either
grant. Remembered logins survive app restarts. Logout clears copies of that
portable session on the device and records a revocation at the current station;
offline servers cannot receive a global logout. Disable the account on both
servers, or rotate the installation key, to revoke access installation-wide.

Native apps keep a healthy connection. On disconnect they sweep UDP 49363 (IPv4
LAN broadcast) and probe configured paired URLs, at most once every five seconds.
Replies must match a fresh nonce, the installation signing key, and a configured
peer identity/role before a session is sent. `/api/station/discovery?nonce=...` is
the corresponding public HTTP discovery endpoint. The native app validates the
portable session at the discovered station, changes address itself, and refreshes
its layout, branding, telemetry and WebSocket. No command is replayed by handover.

A backend behind Docker must publish UDP 49363 as well as its HTTP(S) ports.
`pairing.json`'s `scheme` and `port` describe the client-facing listener/proxy,
not the backend's internal port. `GS_STATION_DISCOVERY_BIND` overrides the UDP
listener (default `0.0.0.0:49363`). The signing identity is application-level; it
is not an HTTPS certificate pin.

Browsers cannot broadcast UDP or share local storage across unrelated origins.
Native apps perform the LAN sweep. An already-open browser page caches the paired
HTTP(S) origins and probes them only after three failed checks of its current
station. It verifies fresh, signed application identities with WebCrypto before
redirecting. Only configured paired origins receive discovery CORS access. This
is best-effort: browser local-network policies, certificate trust and WebCrypto
secure-context requirements still apply. Browsers retain each origin's own saved
login; cross-origin login handoff is not enabled. An unloaded page cannot discover
a replacement server until its application assets can be loaded.
