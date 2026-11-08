# RFD modem check

GroundStation and RRLS check their own RFD900x ASYNC modem against the selected
RFD profile before the AV-bay radio carries data. If a setting differs and may
be written by one node, they write only that setting, save it, reboot the
modem and verify it.

The default profile is the embedded `backend/comms/rfd-profile.json`
(`rfd-async-64k-compatible`). It holds the settings all three modems run
today, per the 10-28 RRLS readback: S3=3968, S4=10, S5=10, S6=50, S11=32768,
S18=0. On a correctly configured install the check changes nothing; it only
repairs drift. Code:
`backend/src/comms/rfd_modem.rs`. Tests against a simulated modem:
`backend/src/comms/rfd_modem_tests.rs`.

The fill link (Pico-Fi or gateway) is never touched. The check runs only on
the AV-bay link, and only when that link is a raw-UART serial radio with an RFD
profile selected.

## When it runs

* **Once per process, at startup**, just before the AV-bay port is first
  opened. On GroundStation this is in `open_rocket_comms`; on RRLS it is the
  first connect of the recovery radio. Reconnects after a USB drop do not
  re-run it.
* **On an explicit operator request**: Connectivity → Radio modem → *Verify
  modem* (read only) or *Fix modem settings*. `POST /api/radio/modem/check`
  with `{"mode":"verify"|"fix"}` needs a signed-in user with the
  `RadioModemCheck` command permission. The radio is held closed for the
  check (about 3 s to read, about 11 s if it writes) and then reopens.

It never runs, not even to read, when:

* the flight state is anything other than Startup, Idle, PreFill or
  FillTest. Launch through Landed are blocked as required. Fill states,
  Armed, Recovery and Aborted are blocked too, because the radio carries
  live AV-bay or recovery data then;
* the launch sequence is active, or any sequence run is not finished;
* the station state is not yet known.

At startup the gate is seeded from the restored flight state before the
radio opens, so a backend restart in flight skips the check. The gate is
checked again before the first write and before `AT&W`.

Operator override: `GS_RFD_MODEM_CHECK=skip` (no check), `verify` (read and
report only) or `fix` (default for operational builds). Builds with `testing`,
`hitl_mode` or `firmware_sim` default to `skip`, and a simulated PTY link
(`GS_SIMULATED_SERIAL_PTY=1`) is always skipped.

## What it does

Sources: RFDesign *RFD900x Asynchronous User Manual* V1.1 (2018), the
asynchronous release notes, and the X/uX datasheets. See
`FirmwareSimulator/docs/rfd-async-mesh-model.md` for the cited table.

1. Waits 1.1 s without sending (the manual asks for at least one second of
   host silence before `+++`), sends `+++`, and accepts an `OK` only after
   the guard time has passed and the line has gone quiet. Received air data
   can contain the bytes `OK`, but the modem stops passing air data in
   command mode. It then confirms with `AT`. If there is no answer, it tries
   once more, including a probe for a modem already left in command mode.
   After that it stops.
2. Reads `ATI`, `ATI5` (all EEPROM parameters, by name), `ATSn?` for anything
   `ATI5` did not list, and `AT+F?` (the forwarding table).
3. Compares the readback with the plan below. It writes a register only when
   `ATI5` names it as V1.1 does (for example `S5:GLOBAL_RETRIES`). If `ATI5`
   has no names, it writes only when the identity says ASYNC and S1/S2/S3/S11
   fit the V1.1 map. If the names differ from V1.1, it never writes.
4. If nothing differs, it sends `ATO` and finishes without writing EEPROM.
5. Otherwise it sends `ATSn=X` for each differing writable register, then
   `AT&W`, then `ATZ`. It waits 4 s for the reboot, re-enters command mode,
   re-reads and compares. A refused write or an unacknowledged `AT&W` is
   followed by `ATZ` without saving, which discards the RAM changes. A
   verification failure is reported and never retried.
6. It leaves data mode with `ATO`. If the modem answers `ERROR`, it
   reboots it with `ATZ`, which always starts in data mode.

Limits: one check at a time, 40 s hard bound, at most 3 EEPROM saves per
process run. RFDesign does not publish EEPROM endurance.

### Register plan

| Reg | Name | Default profile | Opt-in one-retry-layer | Policy |
|---|---|---|---|---|
| S1 | SERIAL_SPEED | 115 | 115 | report (changing it cuts this host off) |
| S2 | AIR_SPEED | 64 | 64 | report, always: link-critical |
| S3 | MAX_DATA | 3968 | 3968 | link-wide |
| S4 | MAX_RETRIES | 10 | 10 | link-wide |
| S5 | GLOBAL_RETRIES | 10 | **0** | link-wide |
| S6 | SER_BREAK_DETECT_MS | 50 | **20** | fix (local) |
| S7, S8 | TX/RXENCAP | 0 | 0 | link-wide |
| S11 | DESTID | 32768 | 32768 | fix (local; broadcast so RRLS hears both ends) |
| S13 | MAVLINK1 | 0 | 0 | fix (local) |
| S17 | LBT_RSSI | 0 | 0 | report, always ("shouldn't be changed") |
| S18 | RTSCTS | host flow control (0) | same | fix (local; an unwired CTS can stall the modem) |
| S9, S10, S12, S14-S16, S19, S22 | | | | info, never written |

**Link-wide** registers must be the same on every node (manual Table 3-2:
S2-S5, S7-S9, S14-S17, S19). One node writing them alone would create the
mismatch the manual forbids. They are therefore reported and left alone
unless the selected profile is explicitly marked as rolled out to all nodes:
`GS_RFD_PROFILE_ROLLED_OUT=<profile name>`, which must equal the selected
profile's `name`. Even then S2 and S17 are never written, and S9, S14-S16
and S19 have no expectation. Local registers (S6, S11, S13, S18) are always
repaired.

### Opt-in low-latency profile

`config/radio/rfd-async-64k-one-retry-layer.json` (S5=0, S6=20) ranks best
in the model (`FirmwareSimulator/docs/rfd-async-mesh-model.md`). It applies
only when the operator selects it, with `GS_RADIO_PROFILE` or the link's
`rfd_profile`. It keeps the transport tuple (gap 55 ms, shares, burst), so
budget negotiation is unaffected. Before selecting it:

1. confirm it with the hardware A/B (model doc, hardware test plan);
2. set it on all three modems: GS, RRLS, and the RF board (phase 2 firmware,
   or a one-time matched write);
3. only then set `GS_RFD_PROFILE_ROLLED_OUT=rfd-async-64k-one-retry-layer`
   on GS and RRLS.

Without step 3, selecting it changes only S6 (local) and reports S5 as a
link-wide difference. The UI shows these conditions in Connectivity.

## Status

`GET /api/radio/modem` returns `available`, `profile_name`, `opt_in_profile`,
`rolled_out_all_nodes`, `configured_mode`, `running`,
`reconfiguration_allowed`, `blocked_reason`, `needs_attention`, and the last
`report`. The report contains the outcome, summary, identity, register rows,
writes, `eeprom_saved`, `data_mode` and warnings. Outcomes: `matched`,
`applied`, `mismatch` (verify-only, link-wide or report-only differences
left in place, or writes blocked), `apply_failed`, `verify_failed`, `no_response`, `failed`,
`skipped`. Anything other than `matched` or `applied`, and any warning,
sets `needs_attention`. The UI shows this as a warning in Connectivity, and
the backend logs it at warn level. The station always continues on the
modem's current settings.

## Phase 2: RF firmware specification

Phase 1 changes nothing on a correctly configured install: GS and RRLS
enforce the deployed default profile, and link-wide registers are written
only for a profile marked as rolled out to all nodes. Nothing in phase 1
depends on RF support, so no capability gate is needed yet. In phase 2 the
RF board enforces **the same selected profile** as GS and RRLS, with the
same rules.

**R1. Startup check in RF firmware.** Use the same algorithm and constants
as `rfd_modem.rs`:

* guard 1100 ms, `+++`, accept `OK` only at least 1200 ms after `+++` and
  after 200 ms of quiet, confirm with `AT`;
* two attempts plus one already-in-command-mode probe;
* `ATI`, `ATI5`, `ATSn?` fallback, `AT+F?`;
* writes only for registers named as in V1.1, `AT&W`, `ATZ`, 4000 ms reboot
  wait, re-enter, verify, `ATO` with an `ATZ` fallback;
* a 40 s bound, at most one `AT&W` per MCU boot, no retry after a failed
  verification.

Profile: the RF board uses the same profile file GS and RRLS select. That is
`RFBoard26/config/rfd-profile.json`, kept byte-identical to the GS embedded
default, or to the opt-in profile when that is rolled out. The RF build
takes a matching rolled-out flag (CMake option `RFD_PROFILE_ROLLED_OUT`,
default OFF). Only with it ON may RF write link-wide registers (S3, S4, S5,
S7, S8). Local registers (S6, S11, S13, S18) are always repaired. Never
write S1, S2, S9, S14-S17 or S19. Keep `RADIO_PACKET_GAP_MS` at 55, which
is at least S6 + 5 for both profiles. Rollout order for the opt-in profile:
hardware A/B first, then flash RF with the profile and the flag ON, then set
`GS_RADIO_PROFILE` and `GS_RFD_PROFILE_ROLLED_OUT` on GS and RRLS in the same
session.

**R2. Flight gate on RF.** Run the check only before the RF radio task
starts sending. Run it only after a FLIGHT_STATE value arrives over CAN (not
the radio) within 10 s of boot, and only if that value is Startup, Idle,
PreFill or FillTest. If no value arrives, or it is any other state, skip the
check. This covers a brown-out reboot in flight. The modem's UART is
unavailable to the radio task while the check runs.

**R3. Report the result (capability-gated).** Add a SEDSnet telemetry type
`RFD_MODEM_STATUS` (schema change: build it on the released SDK and do not
rely on 4.3.12). Send it once after the check, then every 60 s at the lowest
priority. Payload, little-endian:

| Bytes | Field |
|---|---|
| 0 | version = 1 |
| 1 | outcome: 0 skipped, 1 matched, 2 applied, 3 mismatch, 4 apply_failed, 5 verify_failed, 6 no_response, 7 failed |
| 2 | map source: 0 ATI5 names, 1 fingerprint, 2 unconfirmed |
| 3 | flags: bit0 EEPROM saved, bit1 rebooted, bit2 data mode resumed with ATO, bit3 forwarding entries present, bit4 skipped by flight gate |
| 4-17 | u16 observed S2, S3, S4, S5, S6, S11, S18 (0xFFFF = unread) |

GS shows the RF modem row only when it has received this type. Older RF
firmware never sends it, and the UI then says "RF modem not reported".

**R4. Not needed.** The model gives no gain for RF ACK piggybacking, an RF
host backlog cap, or larger batches (see the model doc). Do not implement
them.

**SDK change (described, not made): per-side adaptive reliable retry.**
The model ranks RTT-derived retry as the only host change that removes
retry exhaustion under every fit, with about 5x fewer uplink copies.
`GS_RELIABLE_RETRANSMIT_MS` is global and also governs the fill link, so it
must not be used. The change needed is a per-side policy, for example
`RouterSide::with_reliable_retry(Adaptive { min_ms: 250, max_ms: 3000,
initial_ms: 1000, backoff: 1.5, deadline_ms: 15000 })`, with SRTT taken from
first-ACK samples on that side. GS would set it on the radio side only.
