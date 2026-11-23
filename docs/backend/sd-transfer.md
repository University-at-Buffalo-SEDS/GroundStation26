# SD file transfer protocol v2 (streaming, SDX encoding)

The GS (or an RRLS) copies board SD files over a SEDSnet P2P stream: port
4511 on the FC and the DAQ, port 4512 on the RRLS outbox. Version 2 replaces
the per-block READ request/response of v1 with a streamed, selectively
acknowledged transfer and a lossless binary wire encoding (SDX). Files on the
card stay CSV; SDX exists only on the wire.

All integers are little-endian. Every request and answer is one P2P stream
message; SEDSnet splits large messages into radio-sized frames and reassembles
them.

## Messages

| Op | Direction | Layout | Bytes |
| --- | --- | --- | --- |
| LIST `0x21` | GS -> board | `[0x21][dir][index u16]` (unchanged from v1) | 4 |
| LIST answer `0xA1` | board -> GS | `[0xA1][status][dir][index u16][size u32][flags][name_len][0][name]` | 12 + name |
| CANCEL `0x23` | GS -> board | `[0x23]`; ends the transfer or hash in progress | 1 |
| CANCEL answer `0xA3` | board -> GS | `[0xA3][status]` | 2 |
| CONFIRM `0x24` | GS -> RRLS | RRLS outbox only, unchanged | |
| OPEN `0x25` | GS -> board | `[0x25][xfer][dir][flags][offset u32][length u32][rate u16][msg u16][window][block][name_len][name]` | 19 + name |
| OPEN answer `0xA5` | board -> GS | `[0xA5][status][xfer][flags][size u32][offset u32][length u32][msg u16][block]` | 19 |
| ACK `0x26` | GS -> board | `[0x26][xfer][una u16][bitmap u32][rate u16][window]` | 11 |
| DATA `0xA6` | board -> GS | `[0xA6][xfer][flags][seq u16][block u32]` + (`LAST`: `[raw_len u32][crc u32]`) + body | 9 (+8) + body |
| HASH `0x27` | GS -> board | `[0x27][dir][name_len][name]` | 3 + name |
| HASH answer `0xA7` | board -> GS | `[0xA7][status][flags][size u32][sha256 32 B]` | 39 |

Status codes are those of v1: OK 0, BAD_MESSAGE 1, BAD_STATE 2, BUSY 3,
NOT_FOUND 4, IO_ERROR 5, END 6, BAD_OFFSET 7. READ (`0x22`) is gone; a board
answers it BAD_MESSAGE.

### OPEN

- `xfer` (1..255) names the transfer. Every DATA and ACK carries it; anything
  with another id is stale and ignored. Use a different id than the previous
  transfer on the same stream.
- `flags`: `0x01` SDX encoding wanted, `0x02` zstd allowed (honoured only by
  servers that have it, i.e. the RRLS outbox).
- `offset`, `length`: the raw byte range. `length` 0 means "to the end of the
  file as it is at OPEN". The answer states the actual range.
- `rate`: DATA bytes per second divided by 16 (whole messages, header
  included). 0 opens paused.
- `msg`: largest DATA message in bytes; the board answers the size it will use
  (FC at most 512, DAQ at most 1024, RRLS at most 4096).
- `window`: most unacknowledged DATA messages, 1..32.
- `block`: raw block target in KiB, 1..64 (the board may lower it).

The answer's `flags`: `0x01` SDX used, `0x02` zstd used, `0x04` the file is
still open for writing. `size` is the file size at OPEN. Errors: END when
`offset` equals the size, BAD_OFFSET beyond it, NOT_FOUND, IO_ERROR,
BAD_STATE, BUSY (a hash is running), BAD_MESSAGE.

### DATA and blocks

The board cuts the range into **blocks**: consecutive raw ranges starting at
the OPEN offset. A block ends at the first item boundary at or after `block`
KiB of raw bytes, or at the range end. The cut is deterministic for a given
offset, block size, message size and file content. Each block is independent:
the SDX state starts empty, so any block decodes on its own (resume, range
reads, out-of-order completion).

Each block is sent as one or more DATA messages with consecutive `seq`
numbers (per transfer, from 0, modulo 65536):

- `flags`: `0x01` FIRST message of the block, `0x02` LAST, `0x04` SDX body,
  `0x08` zstd (the concatenated bodies are one zstd frame of the SDX or raw
  bytes), `0x10` END (this block ends the range), `0x20` the file is growing.
- `block`: the raw offset of the block's first byte (same in all its
  messages).
- `raw_len`, `crc` (LAST only): the block's raw length and the zlib CRC-32
  (IEEE, reflected, init/xorout `0xFFFFFFFF`) of its raw bytes as read from
  the card.

A block's body is the concatenation of its messages' bodies in `seq` order.
Without `0x04` the body is the raw bytes. With SDX it is decoded (below);
the result must be `raw_len` bytes with that CRC, or the block is discarded
and requested again. Its messages are acknowledged as received all the same
(`una` never moves back); the GS asks for the block again with a new OPEN
(new `xfer`) at its committed offset and the same range end, keeping any
later verified blocks it holds. The board needs nothing beyond OPEN for it.

### ACK, window and pacing

`una` is the lowest `seq` the GS has not received; bit `i` of `bitmap` means
`seq una+1+i` was received. `rate` and `window` replace the OPEN values
(`rate` 0 pauses: the board sends nothing new and no retransmission).

Board:

- keeps at most `window` messages unacknowledged (`next_seq - una`);
- sends at most `rate * 16` DATA bytes per second and, while its router has
  other queued work, leaves at least 25 ms between messages, so recovery GPS,
  commands and telemetry go first;
- retransmits only gaps: when an ACK shows a missing `seq` below a received
  one, or no ACK has advanced `una` for 3 s while messages are outstanding,
  it rewinds to the block holding the first missing `seq`, regenerates that
  block and sends only the messages the GS lacks, then continues. A message
  is not resent within 1 s of its last send.
- ends the transfer when everything through the END block is acknowledged,
  after 15 s without any ACK, on CANCEL, a new OPEN, a stream reset, or when
  the board leaves its allowed states.

GS:

- ACKs after `window/2` new messages, at most every 500 ms on a gap, 1 s
  after the previous ACK while data arrives, on any rate/window change
  (pause/resume), when the END block completes, and at least every 5 s while
  the transfer is open, paused or not (the board ends a transfer after 15 s
  without an ACK);
- writes the contiguous verified prefix to the `.part` file and keeps later
  completed blocks in memory until the gap fills, so a restart resumes from
  the `.part` size;
- re-OPENs from its committed offset (new `xfer`) when no new DATA arrived
  for 8 s while not paused (the board may have dropped the transfer, e.g.
  after a reboot), and after a block failed its length or CRC-32;
- holds a pause (rate 0) for at most 15 s; a longer pause ends the
  transfer with CANCEL and resumes later with a new OPEN;
- after the last byte, sends a final ACK and CANCEL (so a board that missed
  the final ACK does not answer the HASH with BUSY), then asks HASH and compares the board's SHA-256 of the
  on-disk file with its own copy's. A mismatch discards the copy.

### HASH

The board reads the whole file as it is now and answers its size and SHA-256
(`flags` `0x04`: still open). It runs in bounded steps between other work;
the answer can take seconds for a large file. BUSY while a transfer runs.

## SDX block encoding

Lossless by construction: the encoder replaces only text the decoder
expands back to the identical bytes, and carries everything else verbatim.
The reference decoder is `backend/src/sd_download/sdx.rs`; the encoder is
FlightComputer26/DAQ-Board `Core/Src/sd_xfer_codec.c` (shared verbatim) and
the Rust encoder in the same module (RRLS outbox).

A block body is a sequence of items; all state starts empty at the block
start:

| Item | Layout |
| --- | --- |
| RAW `0x00` | varint n, n bytes verbatim (comments, partial or irregular lines) |
| LABEL `0x01` | varint n, n bytes: defines the next label id (0, 1, ...) |
| ROW `0x02 + 2*id + s` | a 13-field row ending in CRLF whose field 2 (sensor) is label `id` (< 127). `s=1`: the op list equals the previous op list of this label; `s=0`: varint count, then the op nibbles packed two per byte, low nibble first. Then each op's payload, in op order. |

Varints are unsigned LEB128 (at most 10 bytes); zigzag maps signed values
(`0,-1,1,-2,...` -> `0,1,2,3,...`).

A row is rebuilt as its 13 fields joined by `,` plus CRLF. Field 2 is the
label text. Each other field has one op (4 bits):

| Op | Meaning | Payload |
| --- | --- | --- |
| 0 EMPTY | empty unquoted field | |
| 1 SAME | the same token as the context's previous one | |
| 2 DELTA | a number with the same shape as the context's previous one | varint u (below) |
| 3 NUM | a number | shape b0, shape b1, [zigzag exponent varint if b0 bit0], zigzag value varint |
| 4 TEXT | an unquoted token | varint n, n bytes |
| 5 QTEXT | a quoted field, inner bytes verbatim (escapes included) | varint n, n bytes |
| 6 COPY | next nibble k: the identical bytes of field k of this row (quotes included) | |
| 7 QVEC | next nibble c (1..15), then c element ops from {SAME, DELTA, NUM, TEXT}: a quoted field of c tokens joined by single spaces | element payloads |

**Contexts** hold the previous token of each (label id, field, element)
(element 0 for unquoted fields). SAME, DELTA, NUM and TEXT update their
context; EMPTY, COPY and QTEXT do not.

**Numbers.** A number token is
`[-]D{i}[.D{f}][(e|E)[+|-]D{x}]` with `1 <= i <= 15`, `f <= 15`,
`i + f <= 18`, `1 <= x <= 3`. Shape `b0` = bit0 exponent present, bit1 `E`,
bit2 explicit `+` on a non-negative exponent, bits 3-4 `x`; `b1` = `i | f << 4`.
The value is the signed integer of all `i + f` digits (the sign of the
mantissa is folded in), the exponent is signed. Expansion: `-` if the value
is negative, the digits zero-padded to `i + f` and split after `i` (with
`.` when `f > 0`), then `e`/`E`, `-` for a negative exponent or `+` if b0
bit2, and the exponent zero-padded to `x` digits. Encoders never emit text
this cannot rebuild (`-0`, `e-0`).

**DELTA.** Same shape (`b0`, `b1`) as the context's previous number.
Without an exponent `u = zigzag(value - previous value)`. With one, the low
3 bits of `u` are `exponent - previous exponent + 3` (the change is -3..3)
and `u >> 3` is `zigzag(v * 10^(e - m) - pv * 10^(pe - m))` with
`m = min(e, pe)`: the difference at the finer of the two scales. The new
value is the scaled difference plus the scaled previous value, divided by
`10^(e - m)` (exact).

## Measured ratios and transfer times

See [`sd_download` FC collection](../data-pipeline.md#34a-fc-sd-collection-after-landing-sd_downloadfcrs-sd_downloadrelayrs)
for the transfer-time table and the measurements behind it.
