# Pico-Fi Ethernet link

GroundStation can connect directly to the existing Pico-Fi TCP bridge instead of polling I²C. This uses the Pi's normal Ethernet interface. The web UI and local camera streams continue using their existing ports.

In `backend/comms/comms.json`, replace only `fill_box` with one of these configurations, then restart the backend. Keep `av_bay` unchanged.

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
Run `python3 backend/tools/comms_config_gui.py --help` for options. TCP and I²C
are selectable; changing the bridge interface does not replace the Pi's main
Ethernet setup.

The gateway/Pico UART pair used by network qualification is 1,000,000 baud.
Both firmware ends must match. The RFD radio UART remains 115,200 baud. Full
network tests select the Pico1M build explicitly; set
`SEDS_FIRMWARE_SIM_PICO_BAUD=115200` only for a deliberate lower-capacity comparison.

The linked simulator also supports `pico_fi_tcp_to_uart` host links: it binds
a loopback Pico-Fi peer and supplies its address through
`GS_FILL_PICO_TCP_ADDRESS` when `GS_SIMULATED_SERIAL_PTY=1`. This tests actual
TCP framing against gateway UART firmware without involving I²C slots. The
I²C model supports both protocol versions; v2 retains a packet across header
peeks, partial reads and host reconnection until a full body is read.
