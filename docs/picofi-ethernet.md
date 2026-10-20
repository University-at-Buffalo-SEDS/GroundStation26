# Pico-Fi Ethernet link

GroundStation uses the Pi’s main Ethernet interface for gateway Pico-Fi TCP traffic and browser/mobile clients. The web UI, camera streams and Pico-Fi share the physical interface and use separate TCP ports. The AV-bay RFD radio retains its 115,200-baud UART connection. I²C is a legacy option and is outside current deployment qualification.

New installations host TCP on the Pi and discover the gateway Pico-Fi automatically. The OS selects an available TCP port; the Pico learns it over the same Ethernet LAN. No fixed Pico or Pi IP is required. Browser/mobile clients continue using the web server's port.

```json
{"interface":"pico_fi_tcp","mode":"server","address":"0.0.0.0:0","discovery":true,"discovery_port":49361}
```

Build the Pico with `pico-fi-gateway-ethernet.json`: automatic MAC from its physical flash identity, DHCP first, then ARP-probed IPv4 link-local addressing if DHCP is unavailable. It discovers GroundStation and connects as a TCP client. UART1 connects to the gateway at 1,000,000 baud. A saved Pico shell override remains authoritative; use `reset` to restore compiled defaults, or explicitly select `set auto` and `set discover`, then `start` (settings persist automatically).

For the reverse direction, build `pico-fi-gateway-ethernet-server.json` and select GroundStation client discovery:

```json
{"interface":"pico_fi_tcp","mode":"client","address":"0.0.0.0:0","discovery":true,"discovery_port":49361}
```

Discovery uses 32-byte versioned UDP messages on port 49361. Disconnected clients query once per second; servers reply at most four times per second. Clients validate the query challenge and peer role, collect offers for 400 ms and choose a stable device identity. A healthy TCP session sends no discovery queries and stays with its current peer. Disconnects restart discovery; old candidates expire. Discovery does not enter SEDSnet, CAN or the RFD link. Allow UDP 49361 and the advertised TCP port on the trusted local Ethernet network. This is compatible-device discovery, not cryptographic authentication.

`discovery_interface` can select the Pi's wired interface (for example `eth0`); otherwise wired interfaces are detected. `allowed_peer` optionally restricts discovery and connections to a specific IP. Existing saved manual configurations retain their addresses; migrating from I²C requires selecting the new `fill_box` configuration explicitly. Manual mode (`discovery:false`) remains available: set matching numeric endpoints, and set `allowed_peer` for a non-loopback GroundStation listener.

### Ubuntu addressing without DHCP

Pico-Fi assigns itself an unused 169.254/16 address after DHCP times out. The Pi's Ethernet manager must also allow IPv4 link-local addressing; the unprivileged backend cannot assign OS addresses. This keeps addressing on the main Ethernet interface. The setup helper only prints configuration and never changes or restarts a live interface:

```sh
python3 backend/tools/ethernet_link_local.py --renderer networkd --interface eth0
python3 backend/tools/ethernet_link_local.py --renderer networkmanager --interface eth0 --connection 'Wired connection 1'
```

For networkd, merge the printed fields into the existing Ethernet definition in `/etc/netplan`, preserving its existing ID, match rules and other settings; back up the file first and use `sudo netplan try` locally. Pass `--netplan-id` when that definition has a different ID. For NetworkManager, use the existing wired connection name and the printed native `ipv4.link-local enabled` setting; Netplan's `link-local` field alone does not enable this alongside DHCP for that renderer. Apply at a suitable maintenance point because changing addressing can interrupt SSH. See the [Netplan addressing documentation](https://netplan.readthedocs.io/en/stable/netplan-yaml/) and [NetworkManager IPv4 configuration](https://networkmanager.pages.freedesktop.org/NetworkManager/NetworkManager/nm-settings-nmcli.html).

Pico-Fi detects address conflicts while probing and after assignment, abandons conflicted addresses, and retries DHCP while disconnected. While link-local is active it also sends a bounded DHCP-presence probe roughly every 15 seconds, with per-device jitter. A fresh matching offer triggers normal DHCP acquisition and reconnects discovery; probes without an offer leave healthy TCP sessions alone. Both TCP roles use the same address-change handling. Both peers must share the same broadcast LAN; discovery is not intended to cross routed networks.

The existing Pico protocol is unchanged: `PICOFI1` handshake, then `B5 4E`, little-endian 16-bit length, and 1–4096 payload bytes. No UART envelope is added inside the TCP frame. Either end can reconnect. Both directions have bounded 128-frame queues and the decoder validates length before allocating a frame. A stalled writer resets the connection after 500 ms; accepted transmit frames from a failed connection are discarded, not replayed into a new session. Network reliability and command expiry remain owned by SEDSnet.

This transport carries network packets only. It does not relay camera/video traffic over the RFD link.

The configuration tool also supports this interface through CLI, TUI and Tk.
Run `python3 backend/tools/comms_config_gui.py --help` for options. TCP is the default. The backend uses the Pi’s existing Ethernet routing; no second Ethernet adapter or I²C link is required.

The gateway/Pico UART pair used by network qualification is 1,000,000 baud.
Both firmware ends must match. The RFD radio UART remains 115,200 baud. Full
network tests select the Pico1M build explicitly; set
`SEDS_FIRMWARE_SIM_PICO_BAUD=115200` only for a deliberate lower-capacity comparison.

The board network runners default to `pico_fi_discovery_to_uart` host links.
They supply a private UDP discovery port through `GS_FILL_PICO_DISCOVERY_PORT`
when `GS_SIMULATED_SERIAL_PTY=1`; the Pico model discovers GroundStation's
automatically selected TCP listener. This tests actual UDP discovery and TCP
framing against gateway UART firmware without involving I²C slots. Set
`SEDS_FIRMWARE_SIM_FILL_TRANSPORT=tcp` for a manual-address comparison using
`GS_FILL_PICO_TCP_ADDRESS`. The
I²C model supports both protocol versions; v2 retains a packet across header
peeks, partial reads and host reconnection until a full body is read.

For the single-Pico Ethernet deployment, build either automatic profile:

```sh
PICO_FI_UART_BAUD_RATE=1000000 python3 build.py --config pico-fi-gateway-ethernet.json --release
# Optional reverse role:
PICO_FI_UART_BAUD_RATE=1000000 python3 build.py --config pico-fi-gateway-ethernet-server.json --release
```

The build prints the UF2 path. These profiles connect the gateway-side Pico directly to the Pi's Ethernet LAN. Existing two-Pico profiles remain available for older deployments. Builds do not flash hardware or alter the Pi's network configuration.


## Automatic Pi Ethernet addressing

Native Linux root services provision DHCP plus IPv4 link-local on physical wired
interfaces before starting discovery. NetworkManager defaults are installed in
`/etc/NetworkManager/conf.d/90-groundstation-auto-<interface>.conf`; networkd
uses a drop-in for an existing DHCP profile. Wi-Fi, VPN and explicit static
profiles are preserved. Set `GS_ETHERNET_AUTOCONFIG=off` to opt out. Containers
and unprivileged processes leave host network configuration to the administrator.

Use `sudo python3 backend/tools/install_native_service.py` once if the existing
systemd service runs as an ordinary user. Thereafter pulling, rebuilding and
restarting the service is sufficient. Both TCP roles bind/discover using current
addresses and reconnect when the Pico migrates between link-local and DHCP.
The Ethernet cable must be connected; Wi-Fi connectivity alone does not power
or connect the gateway-side Ethernet interface.


## Bridge restart without a TCP close

A restarted or readdressed Pico may never close its previous TCP connection.
The GroundStation server continues accepting discovery-authorized peers while
that old connection is open. A valid PICOFI1 handshake replaces the old session;
a failed handshake leaves it operating. The new connection receives a new TX
generation and a fresh frame decoder, so an old partial frame cannot consume a
new session's first packet. TCP work remains outside the router thread.

The regression holds the original connection open with a partial frame, rejects
an invalid replacement, then reconnects a discovered Pico and verifies both
traffic directions without waiting for an old FIN. This failed before the
repair and passes afterward. Pico builds also use fresh oscillator entropy for
TCP state on every boot, retaining the same physical MAC and link-local identity.
