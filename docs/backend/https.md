# HTTP and HTTPS — GroundStation and RRLS

Both roles use the same Rust server. No reverse proxy or OpenSSL executable is
required, including for native builds on macOS, Ubuntu and Raspberry Pi Ubuntu.
HTTP and HTTPS serve the same WebUI, authenticated API, camera/voice endpoints
and WebSockets. HTTP remains available for existing clients; it does not redirect.
Use HTTPS for browser camera, microphone, location and orientation features.

## Docker

```sh
# Set names and the Pi's LAN address, not the container's private address.
GS_TLS_NAMES=groundstation.local,192.168.3.7 docker compose up -d --build
```

Open `https://192.168.3.7` (443); HTTP is also available on 80. Docker forwards
these to the binary's unprivileged ports 3443 and 3000. `GS_HTTP_PORT` and
`GS_HTTPS_PORT` override the published ports. No privileged backend port binding
is needed. Certificates persist in the existing `./backend/data` mount.
The previous nginx service is no longer required by Compose.

For RRLS, add `-f docker-compose.yml -f docker-compose.rrls.yml`; set
`GS_TLS_NAMES` to RRLS's own address/name. Configure its serial device separately.
The role override disables local fill control and preserves the RRLS radio behavior.

## Native binary / systemd

Run the usual build/run command or installed backend binary. The defaults are:

| Setting | Default / purpose |
| --- | --- |
| `GS_BIND_ADDRESS` | `0.0.0.0:3000` — HTTP |
| `GS_HTTPS_BIND_ADDRESS` | `0.0.0.0:3443` — HTTPS; `off` explicitly disables it for isolated tests |
| `GS_TLS_DIR` | `backend/data/tls` relative to the working directory |
| `GS_TLS_NAMES` | Additional comma-separated DNS names and IPs, without schemes or ports |
| `GS_TLS_CERT`, `GS_TLS_KEY` | Optional PEM certificate chain and private key; set both |

For example, native RRLS can use `GS_RRLS_ROLE=rrls` and
`GS_TLS_NAMES=rrls.local,192.168.4.7`, then open `https://192.168.4.7:3443`.
Localhost names are always included. Without explicit `GS_TLS_NAMES`, native
interface addresses are included automatically. Explicit names keep the identity
stable even if Docker assigns a different private container IP.
Use explicit names in Docker because container interfaces do not identify host IPs.
Names in a certificate do not configure DNS or mDNS; the name must resolve separately.

To bind native ports 80/443 instead, change the two bind addresses. Linux systemd
services then need `AmbientCapabilities=CAP_NET_BIND_SERVICE` and
`CapabilityBoundingSet=CAP_NET_BIND_SERVICE`; running the backend as root is unnecessary.

## Certificate lifecycle and trust

On first HTTPS startup the binary generates an ECDSA self-signed certificate and
private key. It reuses that identity across restarts. On subsequent starts it
renews generated certificates within 30 days of expiry, or when configured LAN
names/IPs change or stored generated files are damaged. The validity is 397 days.
Unix certificate directories are owner-only and private keys are mode 0600.
Keep each station's data directory persistent and private. Do not share keys between stations.

A self-signed certificate is **not automatically trusted by phones or browsers**.
Explicitly trust the station's certificate on your client device, or supply a
certificate issued by a CA that device trusts. A warning exception alone may not
enable every camera/sensor feature on every mobile browser. Generated public
certificates are at `backend/data/tls/cert.pem`; never distribute `key.pem`.
Renewal or a changed IP/name may require trusting the new certificate again.
Custom PEM pairs are loaded unchanged and are never renewed or overwritten by
the application; maintain those through your certificate provider.

Changing to HTTPS changes the browser origin, so sign in again and update saved
client/server URLs to the HTTPS URL. Existing HTTP API clients can keep their URLs.
Both listeners must bind successfully before either starts serving; a TLS error
fails startup visibly instead of silently leaving only the insecure listener.
