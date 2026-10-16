# Find GroundStation automatically

Use `"host": "auto"` in the sender's JSON configuration (now the example default).
Keep `GS_VIDEO_PASSWORD` identical on the sender and receiver. Camera names,
resolution, bitrate and publishing credentials work as before. Existing fixed
IP/hostname configurations remain fixed until you change them to `auto` and run
`gs-video-sender reload` or restart its service.

GroundStation's managed video receiver advertises its RTSP publishing port on
UDP **49362**. Permit that UDP port and RTSP TCP **8554** on the local LAN. The
sender finds the receiver's current source address, verifies its signed reply,
and publishes over RTSP/TCP. Configure `"discovery_interface": "eth0"` to restrict
queries to one interface; otherwise it checks available IPv4 LAN broadcasts.
Both devices still need reachable addresses on the same LAN/subnet; this does
not configure OS addresses, traverse routers or discover across the RFD radio.

An interrupted/stalled pipeline is stopped and discovery is retried. FFmpeg uses
a five-second network I/O timeout; the daemon also restarts a pipeline after
20 seconds without encoded-frame progress, then retries after three seconds.
It does not persist discovered IPs, so DHCP or IPv4 link-local changes can be
recovered without editing configuration. Healthy streams send no discovery
queries. `gs-video-sender status` shows each stream's resolved destination or
`discovering groundstation`. If several receivers share the same password,
the lowest responding IPv4 address/port wins; use different passwords to pair
separate stations.

Replies use a separate, versioned 64-byte camera discovery protocol with
HMAC-SHA256 and a fresh 128-bit challenge. Passwords are never broadcast and a
reply for an earlier attempt cannot redirect the current publisher. Publishing
continues to require MediaMTX's existing username/password; discovery does not
make streams public or change viewer permissions. Use a strong video password.

For a locally hosted **external/Compose MediaMTX** receiver, set these on the
GroundStation backend:

```ini
GS_VIDEO_MANAGED=0
GS_VIDEO_PASSWORD=<same publishing password as MediaMTX and the sender>
GS_VIDEO_DISCOVERY_RTSP_PORT=8554
```

Only advertise a receiver reachable at GroundStation's own LAN address. Remote
external relays should retain a fixed sender destination. To disable discovery,
set `GS_VIDEO_DISCOVERY=0`. For installations changing the discovery port, set
`GS_VIDEO_DISCOVERY_BIND=0.0.0.0:49362` on the backend and the matching
`"discovery_port": 49362` on the sender. Port 49361 belongs to Pico-Fi discovery;
the protocols and services are separate. Video never enters CAN, SEDSnet or RFD.

Validation uses real UDP discovery and TCP connections on macOS and Ubuntu,
including receiver restart/port changes, changed IPv4 address on Linux,
unmatched passwords, malformed packets and replayed challenges. Camera capture
and actual MediaMTX publishing still need the camera hardware for a field check.
