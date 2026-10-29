# Radio reference regression

The backend test `rrls::air_campaign::finite_three_radio_mesh_recovers_actual_reliable_commands_and_receipts`
exercises real packed SEDSnet routers, GroundStation/RRLS Mesh state machines,
and the production raw-UART stream decoder. It uses diagnostic integer commands;
it does not operate valves, ignition or deployment outputs.

From the GroundStation repository, with the sibling frontend dev checkout:

```sh
GS_FRONTEND_SOURCE_DIR="$PWD/../Seds-Ground-Station-Frontend" \
  cargo test -p groundstation_backend --bin groundstation_backend \
  finite_three_radio_mesh_recovers_actual_reliable_commands_and_receipts -- --nocapture
```

For unreleased SDK changes, retain the normal `SEDSnet = "4"` requirement and
select the sibling source explicitly for qualification:

```sh
GS_FRONTEND_SOURCE_DIR="$PWD/../Seds-Ground-Station-Frontend" \
  cargo test -p groundstation_backend --bin groundstation_backend \
  --config 'patch.crates-io.SEDSnet.path="../SEDSnet"' -- --nocapture
```

Six scenarios cover ordered and unordered receipts, 4/16 KiB finite modem
buffers, varied framing overhead, direct-link failure/recovery, RRLS loss,
command deduplication and telemetry freshness. The 4 KiB scenarios intentionally
cause startup FIFO loss; reliable commands must recover. The 16 KiB reference
requires no FIFO overflow. Aggregate airtime is bounded to 64 kbit/s. Stable
command and telemetry windows retain their 500 ms gates.

This is a deterministic capacity and recovery reference. Its round-robin
contention and frame-sized air bursts are explicit approximations. It does not
execute MCU firmware or reproduce proprietary RFD packetization, CSMA backoff,
FEC, propagation or packet-error rates. Split/coalesced serial reads are modeled;
interleaving bytes from proprietary modem bursts is not. Linked firmware and
physical radio validation remain separate gates.


## Queued retries and modem congestion

The radio TX budget refuses a repeated, byte-identical independent command
while its original copy remains accepted for UART transmission. It returns
transient backpressure so the SDK retains the retry; it does not acknowledge
or silently discard it. Popping a queue entry or failing a write does not
release that admission. Successful physical-write completion releases it,
allowing a later retry to recover a lost receipt. A new packet identity,
ordered/piggyback frame, or stateful SDT frame retains its own admission.
The `sdk_retry_keeps_its_owner_while_the_original_uart_copy_is_pending` test
exercises that ownership with the published SDK and a virtual clock.

The simulator's `test_radio_congestion_envelope.py` separately holds the
captured source-rate envelope constant while reducing effective air capacity.
It reproduces seconds of ACK buffering, repeated command submissions, finite
modem FIFO overflow, and record-framing loss without injected bit flips.
This is a capacity sensitivity experiment, not a proprietary ASYNC MAC model
or confirmation of the physical modem's retransmission count.

The [RFDesign ASYNC manual](https://files.rfdesign.com.au/Files/documents/RFD900x%20Asynchronous%20User%20Manual%20V1.1.pdf)
documents collision retries, a separate broadcast retry limit, serial break
detection, and fixed forwarding. Capture the installed firmware identity and
public settings by their printed names on GS, RF, and RRLS before comparing
profiles. The RRLS readback alone does not establish the other modems' live
configuration. A matching, quiesced hardware comparison must measure both
command arrival and owner-receipt return; do not change one endpoint's retry
or encapsulation settings in isolation. Serial framing gaps remain necessary
until a replacement packetization contract has been validated.
