# Pico-Fi Ethernet link

GroundStation uses the Pi’s main Ethernet interface for gateway Pico-Fi TCP traffic and browser/mobile clients. The web UI, camera streams and Pico-Fi share the physical interface and use separate TCP ports. The AV-bay RFD radio retains its 115,200-baud UART connection. I²C is a legacy option and is outside current deployment qualification.

The shipped `backend/comms/comms.json` and new-install defaults select TCP client mode. `192.168.3.20:9000` is an example Pico address: set it to the actual endpoint before deployment. Existing saved configurations are preserved; change `fill_box` explicitly when migrating from I²C, then restart the backend. Keep `av_bay` unchanged.

GroundStation connects to a Pico running `TcpServer`:

```json
{"interface":"pico_fi_tcp","mode":"client","address":"192.168.3.20:9000"}
```

GroundStation hosts a server for a Pico running `TcpClient`:

```json
{"interface":"pico_fi_tcp","mode":"server","address":"0.0.0.0:9000","allowed_peer":"192.168.3.20"}
```

Use the actual Pico IP and matching port. A non-loopback server requires an allowed peer IP. This is a local bridge protocol, not an Internet-facing authenticated service; restrict the port to the Pico's network. It does not share the HTTP server's TCP port.

The existing Pico protocol is unchanged: `PICOFI1` handshake, then `B5 4E`, little-endian 16-bit length, and 1–4096 payload bytes. No UART envelope is added inside the TCP frame. Either end can reconnect. Both directions have bounded 128-frame queues and the decoder validates length before allocating a frame. A stalled writer resets the connection after 500 ms; accepted transmit frames from a failed connection are discarded, not replayed into a new session. Network reliability and command expiry remain owned by SEDSnet.

This transport carries network packets only. It does not relay camera/video traffic over the RFD link.

The configuration tool also supports this interface through CLI, TUI and Tk.
Run `python3 backend/tools/comms_config_gui.py --help` for options. TCP is the default. The backend uses the Pi’s existing Ethernet routing; no second Ethernet adapter or I²C link is required.

The gateway/Pico UART pair used by network qualification is 1,000,000 baud.
Both firmware ends must match. The RFD radio UART remains 115,200 baud. Full
network tests select the Pico1M build explicitly; set
`SEDS_FIRMWARE_SIM_PICO_BAUD=115200` only for a deliberate lower-capacity comparison.

The linked simulator defaults to `pico_fi_tcp_to_uart` host links: it binds
a loopback Pico-Fi peer and supplies its address through
`GS_FILL_PICO_TCP_ADDRESS` when `GS_SIMULATED_SERIAL_PTY=1`. This tests actual
TCP framing against gateway UART firmware without involving I²C slots. The
I²C model supports both protocol versions; v2 retains a packet across header
peeks, partial reads and host reconnection until a full body is read.
