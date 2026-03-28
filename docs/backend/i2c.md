# Backend I2C Transport

This document describes the I2C transport contract used by the Ground Station backend when it talks to the Pico bridge device.

## Scope

The Linux backend is the I2C master. The Pico is an `I2C0` slave at address `0x55`.

This is not plain byte-stream I2C. It is a framed application protocol carried over a sequence of I2C transactions.

## Electrical Setup

The Pico side uses:

- `GPIO0` for `I2C0 SDA`
- `GPIO1` for `I2C0 SCL`

Typical Raspberry Pi wiring:

- Pi SDA -> Pico `GPIO0`
- Pi SCL -> Pico `GPIO1`
- Pi GND -> Pico GND

External pull-ups must be present on SDA and SCL if the carrier board does not already provide them.

## Address And Framing Constants

- slave address: `0x55`
- maximum payload: `256` bytes
- framed response buffer size: `258` bytes
- host chunk size: `32` bytes
- data request magic: `0xA5`
- command request magic: `0xA6`
- data response magic: `0x5A`
- command response magic: `0x5B`

Frame header layout:

- byte `0`: magic
- byte `1`: payload length `N`
- bytes `2..2+N`: payload bytes
- remaining bytes in a staged response buffer: zero-filled

## Transaction Model

The device firmware does not require the host to write a fully padded `258`-byte request frame in one transaction.

Instead, the host writes request bytes in one or more I2C write transactions, normally up to `32` bytes each. The Pico accumulates those chunks and treats the request as complete once it has received:

- at least the 2-byte header, and
- `payload_length + 2` total bytes

That means a request with a 10-byte payload is complete after 12 received bytes, not after 258 bytes.

The Pico stages responses in a fixed `258`-byte response buffer. The host reads that staged buffer back in one or more I2C read transactions, normally `32` bytes at a time.

## Request Types

### Data Request `0xA5`

Carries raw bridged payload bytes intended for the remote link behind the Pico.

Behavior:

- non-empty payload: forward payload across the Pico bridge
- empty payload: poll request that asks the Pico to return any currently staged response

### Command Request `0xA6`

Carries an ASCII command handled locally on the Pico, such as `/ping` or `/show`.

Behavior:

- payload is interpreted as a local command line
- Pico replies with a `0x5B` command response frame

## Response Types

### Data Response `0x5A`

Carries raw bridged payload returned from the remote side.

### Command Response `0x5B`

Carries the Pico-local command output as bytes.

## Polling And Idle Reads

I2C slaves cannot transmit spontaneously. The master must read to fetch any staged response.

The Pico firmware currently supports two practical polling patterns:

- explicit poll write: send an empty `0xA5` request, then read the staged response
- direct read poll: issue a read transaction without a preceding empty write and consume the currently staged response buffer

The current Ground Station backend relies on direct reads for receive-side polling. That works with the current Pico firmware, even though the stricter protocol description often uses the empty `0xA5` request as the canonical poll operation.

If no valid frame is staged yet, host reads may return an all-zero or mostly `0xFF` buffer. The backend should treat those as idle or garbage reads rather than valid frames.

## Ground Station Compatibility Notes

The current backend implementation in [comms.rs](/Users/rylan/Documents/GitKraken/GroundStation26/backend/src/comms.rs):

- matches the Pico data-plane transport for `0xA5` requests and `0x5A` responses
- uses `32`-byte chunked writes and reads
- reads a full `258`-byte staged response buffer
- does not currently send `0xA6` command requests
- does not currently parse `0x5B` command responses
- does not currently send explicit empty-payload poll writes

This means the current backend is compatible with the Pico's bridged data path, but not with the full command-oriented portion of the device protocol.

## References

- Ground Station host implementation: [comms.rs](/Users/rylan/Documents/GitKraken/GroundStation26/backend/src/comms.rs)
- Pico frame definitions: `/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/i2c.rs`
- Pico I2C slave task: `/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/i2c_task.rs`
