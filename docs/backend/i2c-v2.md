# I2C packet protocol v2

The GroundStation-side Pico remains an I2C slave at `0x55`. V2 carries one
complete bridge packet immediately, without a timer, aggregation, or 32-byte
padding. Ethernet and gateway-side UART framing do not change.

## Wire contract

Every packet has a four-byte header: `[0xD2, kind, length_lo, length_hi]`.
The length counts payload bytes only and is limited to 4092. Kinds are idle
`0`, data `1`, local command `2`, explicit USB bootloader request `0x7e`, and
error `0x7f`. Idle requires length zero. Application packets are nonempty.
DATA payloads are opaque SEDSnet transport units, including compact frames
and discovery chunks; the former nested four-byte UART envelope is removed.
SEDSnet validates its own packet integrity.

The host selects v2 by writing `[0xD2, 0, 0, 0]`, then verifies a v2 header.
A legacy reply is an explicit firmware mismatch, not an empty mailbox. A Pico
starts in v1 mode and still accepts the existing 32-byte slot protocol. V1
writes select v1 again. Stop the host before changing protocol versions.

- **Write:** one transaction containing the header and exactly `length` bytes.
- **Read:** read four bytes to peek at the next packet's kind/length, then read
  `4 + length` bytes in a second transaction. That transaction repeats the same
  header followed by the payload. Validate that both headers match.
- **Idle:** a four-byte idle header; there is no second transaction.
- **Aborted read:** retain the mailbox packet. Only a completed payload read
  consumes it. Every subsequent transaction starts with a fresh header.
- **Restart:** the host reselects v2 after an I/O or header error. Selection does
  not discard a staged v2 packet. The bridge queues remain bounded; this is not
  an end-to-end lossless delivery guarantee under overload.
- **USB bootloader:** an explicit kind `0x7e` write with payload `BOOTSEL`
  enters the RP2040 ROM USB loader. It requires this firmware already installed.

A 35-byte message costs 43 I2C data bytes to receive (4-byte peek + 39-byte
packet), versus 96 with v1 even before its nested UART envelope. I2C address,
ACK, START/STOP, clock stretching, and Linux scheduling add overhead. The
change does **not** establish 5,000 messages/s; measure the actual traffic mix.

## Build

From pico-fi:

```sh
PICO_FI_CONFIG=pico-fi-server.json PICO_FI_UART_BAUD_RATE=1000000 cargo build --release
cargo host-test
elf2uf2-rs target/thumbv6m-none-eabi/release/pico-fi pico-fi-i2c-v2.uf2
```

The UART setting is unchanged and does not set I2C speed. The Pi is the I2C
clock master. Both controllers support Fast-mode Plus; request 1 MHz on the Pi
and verify operation with the actual wiring. The supported maximum is 1 MHz.

## Paired deployment from GroundStation's Pi

First rebuild GroundStation with this change. Missing `protocol_version`
continues to mean v1, so the rebuilt backend can run with the old firmware.
The deploy helper is `backend/tools/pico_i2c.py` in GroundStation26.

1. Stop both the backend and frontend before direct bus access. On the current
   Pi installation, the backend is a separate system service:

   ```sh
   sudo systemctl stop sed-ground-station.service
   systemctl --user stop seds-gs-app.service
   ```

   Stopping only the frontend leaves the backend using I2C. Killing the backend
   process alone lets its service restart it automatically.
2. Connect the GroundStation-side Pico by **USB data cable to the Pi** and enter
   BOOTSEL. The first upgrade requires the physical BOOTSEL procedure or SWD;
   the old firmware cannot service the new software bootloader command.
3. Flash the validated RP2040 UF2 from the Pi, using either installed picotool
   or the mounted bootloader drive:

   ```sh
   sudo python3 backend/tools/pico_i2c.py flash /path/pico-fi-i2c-v2.uf2
   # Alternative when RPI-RP2 is mounted:
   python3 backend/tools/pico_i2c.py flash /path/pico-fi-i2c-v2.uf2 --mount /media/rylan/RPI-RP2
   ```

4. With GroundStation still stopped, verify v2 at the existing clock and then
   prepare the paired backend configuration and 1 MHz Pi clock:

   ```sh
   sudo python3 backend/tools/pico_i2c.py probe
   sudo python3 backend/tools/pico_i2c.py configure --comms-config backend/comms/comms.json --clock-hz 1000000
   ```

   `configure` verifies v2 first, saves backups, sets fill_box protocol_version
   to 2, and changes `/boot/firmware/config.txt`. It does not reboot automatically.
   Serial `baud_rate` has no effect on I2C and is removed from that link config.
5. Reboot the Pi to apply the clock, start the backend and frontend services,
   and measure delivery
   rate, gaps, and transport errors. If 1 MHz is unreliable, configure 400000
   and reboot; v2 still provides the framing improvement at 400 kHz.

Future updates can enter BOOTSEL from the stopped GroundStation host with:

```sh
sudo python3 backend/tools/pico_i2c.py bootloader
```

A USB data connection is still required to transfer firmware. This is USB
flashing initiated over I2C, not a firmware upload over the I2C data protocol.

## Tests and simulator

Pico host tests exercise the exact codec/mailbox used by firmware, including
oversized/malformed packets, short reads, repeated peeks, and exact consumption.
GroundStation Linux tests exercise its actual read/write implementation over a
mock transaction socket for small, 1 KiB, and maximum-size packets and reject
legacy firmware before sending application data. Its v2 socket test interface
uses `w`/`r` followed by a u16 transaction length; v1 `W`/`R` stays unchanged.
The existing full-board simulator uses v1 unless updated to this interface.
