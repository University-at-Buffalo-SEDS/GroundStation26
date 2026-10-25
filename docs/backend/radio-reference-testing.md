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
