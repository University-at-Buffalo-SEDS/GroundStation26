# Backend I2C Transport

This document describes the chunked I2C packet transport used by the Ground Station backend and the Pico firmware currently deployed on the fill-system side.

## Scope

The Linux host is the I2C master. The Pico is the I2C slave at address `0x55`.

The transport no longer treats I2C as a fake fixed-length frame buffer. Every I2C transaction is a self-describing `32` byte slot. Multi-slot transfers carry one logical packet split across as many transactions as needed.

That removes the old corruption mode where repeated or restarted slave reads were stitched together as if they were one contiguous `258` byte buffer.

## Electrical Setup

- Pico `GPIO0` = `I2C0 SDA`
- Pico `GPIO1` = `I2C0 SCL`
- Pi physical pin `3` / `GPIO2` -> Pico `GPIO0`
- Pi physical pin `5` / `GPIO3` -> Pico `GPIO1`
- Pi GND -> Pico GND

External pull-ups must be present on SDA and SCL if the carrier board does not already provide them.

## Slot Format

Every master write and master read transaction is exactly `32` bytes.

Header layout:

- byte `0`: magic0 = `0x49`
- byte `1`: magic1 = `0x32`
- byte `2`: version = `0x01`
- byte `3`: kind
- byte `4`: flags
- byte `5`: reserved
- bytes `6..9`: packet offset, little-endian `u32`
- bytes `10..13`: total packet length, little-endian `u32`
- bytes `14..15`: slot payload length, little-endian `u16`
- bytes `16..17`: transfer id, little-endian `u16`
- bytes `18..31`: slot payload bytes

Constants:

- slot size = `32`
- header size = `18`
- slot payload size = `14`
- magic = `0x49 0x32`
- version = `1`

Kinds:

- `0x00`: idle
- `0x01`: data
- `0x02`: command
- `0x7f`: error

Flags:

- `0x01`: start of transfer
- `0x02`: end of transfer

## Transfer Rules

Each logical packet is sent as one or more slots with:

- one transfer id for the whole packet
- offset `0` on the first slot
- monotonically increasing offsets on later slots
- the same total packet length on every slot

The first slot must have `START`.
The last slot must have `END`.
A one-slot packet has both `START | END`.

Receivers must reject a transfer if:

- the magic or version is wrong
- the transfer id changes mid-stream
- the kind changes mid-stream
- the next offset does not match the previously received byte count
- the received bytes exceed the declared total length
- the transfer ends before the declared total length is reached

## Polling Model

I2C slaves cannot push data. The master must read.

The receive side therefore works as a mailbox:

- the master issues a `32` byte read
- the slave returns either one packet slot or an idle slot
- the master keeps polling until it has reassembled a full packet

Idle reads are either:

- an explicit idle slot (`kind = 0x00`)
- all-zero data
- all-`0xff` data

Those must be treated as “no packet available yet”.

## Large Packet Support

Packet size is no longer limited by the I2C transport itself.

The total packet length field is `u32`, so the wire format can carry payloads up to `4 GiB - 1` bytes. In practice, the usable size depends on endpoint memory, queueing, and the application producing or consuming the packet.

The Ground Station Rust backend now streams outgoing packets across as many I2C slots as needed and reassembles incoming packets from as many slots as required.

Current Pico firmware error responses on this link are `KIND_ERROR` packets with short ASCII payloads such as:

- `error invalid i2c slot`
- `error invalid i2c kind`

## Compatibility

This slot protocol is not wire-compatible with the old `258` byte `magic + len + payload` staging format.

Every host implementation that speaks to the device must use the same slot format:

- Ground Station backend Rust I2C transport
- Python I2C tools
- Pico I2C firmware task

If one side is still on the old fixed-frame transport, the link will not decode correctly.

## References

- Ground Station Rust host transport: [comms.rs](/Users/rylan/Documents/GitKraken/GroundStation26/backend/src/comms.rs)
- Python host tools: `/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py`
