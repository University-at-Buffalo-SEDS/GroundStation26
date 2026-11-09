# RFD modem check

GroundStation, RRLS and the RF board each check their own RFD900x ASYNC modem
against the selected RFD profile. GS and RRLS do it before the AV-bay radio
carries data; the RF board does it once per boot (see "Phase 2" below). If a setting differs and may
be written by one node, they write only that setting, save it, reboot the
modem and verify it.

The default profile is the embedded `backend/comms/rfd-profile.json`
(`rfd-async-64k-global1`): S3=3968, S4=10, **S5=1**, S6=50, S11=32768, S18=0.
The modems read S5=10 on 10-28. Every broadcast packet (DESTID 32768) holds the
air for 1 + S5 copies, so S5=10 leaves 64000/11 = 5818 bit/s of air for
32000 bit/s of host pacing: the modems' buffers overflowed and most AV-bay
frames never arrived (bench: AV-bay data 2-4 s old, about 11 frames/s heard).
The profile check now rejects a usable air rate the copies cannot carry.
The default profile is applied automatically: GS, RRLS and the RF board's
default build write its link-wide registers too, so S5=1 lands on all three
modems with no flag, environment variable or manual AT command. On a correctly
configured install the check then changes nothing and only repairs drift. Code:
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

* **Reset radio** (Connectivity → Radio modem → *Reset radio*,
  `POST /api/radio/modem/check` with `{"mode":"reset"}`, same permission):
  the radio port is held closed as for a check, the modem enters command mode
  and is rebooted with `ATZ` (settings reload from EEPROM; nothing is
  written), then the port reopens. If the modem does not enter command mode,
  the port is still closed and reopened, which restarts the host side of the
  link, and the result is `no_response`. A successful reset is `reset`. Every
  reset raises a "Radio reset succeeded / needs attention" notification. It
  works on GS and RRLS and needs no RFD profile. `reset` is never accepted as
  the startup mode.

Neither the check nor the reset runs, not even to read, when:

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
report only) or `fix` (default in normal and test-fire modes). HITL and testing
modes and `firmware_sim` builds default to `skip`, and a simulated PTY link
(`GS_SIMULATED_SERIAL_PTY=1`) is always skipped. The startup check runs once,
under the mode the backend started in; a later switch into normal or test fire
does not rerun it (the operator check stays available).

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

| Reg | Name | Default profile (global1) | Opt-in one-retry-layer | Policy |
|---|---|---|---|---|
| S1 | SERIAL_SPEED | 115 | 115 | report (changing it cuts this host off) |
| S2 | AIR_SPEED | 64 | 64 | report, always: link-critical |
| S3 | MAX_DATA | 3968 | 3968 | link-wide |
| S4 | MAX_RETRIES | 10 | 10 | link-wide |
| S5 | GLOBAL_RETRIES | **1** (modems read 10 until rolled out) | **0** | link-wide |
| S6 | SER_BREAK_DETECT_MS | 50 | **20** | fix (local) |
| S7, S8 | TX/RXENCAP | 0 | 0 | link-wide |
| S11 | DESTID | 32768 | 32768 | fix (local; broadcast so RRLS hears both ends) |
| S13 | MAVLINK1 | 0 | 0 | fix (local) |
| S17 | LBT_RSSI | 0 | 0 | report, always ("shouldn't be changed") |
| S18 | RTSCTS | host flow control (0) | same | fix (local; an unwired CTS can stall the modem) |
| S9, S10, S12, S14-S16, S19, S22 | | | | info, never written |

**Link-wide** registers must be the same on every node (manual Table 3-2:
S2-S5, S7-S9, S14-S17, S19). One node writing them alone would create the
mismatch the manual forbids. They are written for the embedded default
profile, which every node's default build applies, and for an opt-in profile
only when it is explicitly marked as rolled out to all nodes:
`GS_RFD_PROFILE_ROLLED_OUT=<profile name>`, which must equal the selected
profile's `name`. Otherwise they are reported and left alone. Even then S2 and S17 are never written, and S9, S14-S16
and S19 have no expectation. Local registers (S6, S11, S13, S18) are always
repaired.

### S5=1 (default profile)

Faithful simulation (FirmwareSimulator `--scenario faithful`, 7 boards, Normal,
Idle): with S5=10 the GS heard 2.9 frames/s from the AV bay; with S5=1, 11.8
frames/s. RF data age p50 went from 2020 ms to 135 ms, FC from 501 ms to 227 ms.

It is applied automatically: by GS and RRLS at their startup check (and on an
operator *Fix modem settings*), and by the RF board's default build on its
next boot at Idle. Each node writes it at most once per boot, only at Idle
(see "When it runs"), saves, reboots the modem, verifies and reports the
result; a failed write or verification is reported and not retried. The link
keeps working while the modems differ in S5 during the transition: each modem
only repeats its own packets a different number of times. Check every modem
with *Verify modem* (GS, RRLS) and the RF health block.

### Opt-in low-latency profile

`config/radio/rfd-async-64k-one-retry-layer.json` (S5=0, S6=20) ranks best
in the model (`FirmwareSimulator/docs/rfd-async-mesh-model.md`). It applies
only when the operator selects it, with `GS_RADIO_PROFILE` or the link's
`rfd_profile`. It keeps the transport tuple (gap 55 ms, shares, burst), so
budget negotiation is unaffected. Before selecting it:

1. confirm it with the hardware A/B (model doc, hardware test plan);
2. set it on all three modems: GS, RRLS, and the RF board (flash RF with
   `config/rfd-profile.json` replaced by this profile and
   `build.py build --rfd-profile-rolled-out`);
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

`receive_quality` counts raw-UART radio frames taken and frames rejected by
their CRC32, over the last minute and in total. Connectivity → Radio modem
shows "Receive quality (last minute): N good frames, M CRC failures (x%)".
The radio worker no longer logs each rejected frame; it logs one line per
30 s while rejections occur: "N raw UART CRC mismatches in the last 30 s, M
good frames (x% rejected)".

Rejected frames are expected on a mesh at a low rate. The RFD checks its own
air CRC, so a host-level mismatch means a frame reached the UART incomplete
or interleaved; RRLS hears both GS and RF, and ASYNC can send a packet that
holds only part of a UART frame when it gets channel access, so fragments
from two senders can interleave at RRLS. A parser bug made this worse: after
a CRC rejection it resynchronized on an apparent sync inside the rejected
frame's payload, which surfaced garbage as a frame, caused further CRC
mismatches, or swallowed the next real frame. The parser now waits for a
header after the rejected span (`rejected_rr_v1_span_is_never_reparsed_for_a_false_sync`).

`rf_board` holds the RF board's own result once its BOARD_HEALTH carries it
(see phase 2): outcome, map source, mode, `rolled_out_all_nodes`, flags
(`eeprom_saved`, `rebooted`, `resumed_with_ato`, `forwarding_entries`,
`skipped_by_flight_gate`, `pending`, `needs_attention`), the observed
`air_speed`, `max_data`, `max_retries`, `global_retries`, `serial_break_ms`,
`dest_id` and `rtscts`, and a summary. Connectivity → Radio modem shows it
below the station result, or "RF modem not reported" for older RF firmware.
Every finished RF check (pass or needs attention, not a skip) raises one
operator notification, again after an RF reboot.

## Phase 2: RF board check (implemented)

Implemented in RFBoard26: `Core/Src/rfd_modem.c` (hardware-independent check,
the same algorithm and constants as `rfd_modem.rs`), `Core/Src/rfd_modem_port.c`
(gate, UART, status) and `radio.c` (UART hold). Native tests against a
simulated modem: `tests/native/rfd_modem_test.c`, run by
`tests/test_rfd_modem.py` (`build.py test`).

**Algorithm.** As GS: guard 1100 ms, `+++`, `OK` accepted only 1200 ms after
`+++` and after 200 ms of quiet, confirmed with `AT`; two attempts plus one
already-in-command-mode probe; `ATI`, `ATI5`, `ATSn?` for registers `ATI5`
did not list, `AT+F?`; writes, `AT&W`, `ATZ`, 4000 ms reboot wait, re-enter,
verify, `ATO` with an `ATZ` fallback; 40 s bound, no retry after a failed
verification, at most one `AT&W` per MCU boot. The `AT+F?` forwarding table is parsed
per entry in the 4.03 `slot:src->dst` format (an entry exists only when src or
dst is nonzero), or as the manual's `Src,Dst` pairs. If the modem does not answer,
the board gives up and runs on the modem's current settings.

**Registers are matched by the name the modem reports in `ATI5`**, as GS does
since the RFD ASYNC 4.03 field readback (S6 `SER_BRK_DETMS`, S7/S8
`TX_ENCAP_METHOD`/`RX_ENCAP_METHOD`, S17 `DUTY_CYCLE`, S18 `LBT_RSSI`, S19
`RTSCTS`, S20 `MAX_WINDOW`, S22 `RESV`). A write goes to the register number
the modem itself gives that name, so a flow-control fix lands on S19 and
never on `LBT_RSSI`. The documented number is used only to read a modem that
lists no names (V1.1 fingerprint, as GS). If `ATI5` lists names but a
writable register's name is missing, nothing is written. The RF plan holds
only registers with an expectation: S1 and S2 (report), S3, S4, S5, S7, S8
(link-wide), S6, S11, S13, RTSCTS (local, always repaired) and `LBT_RSSI`
(report). Info-only registers are not read, which keeps the link down for
less time.

**Profile.** `RFBoard26/config/rfd-profile.json`, byte-identical to the GS
embedded default (`tests/test_rfd_modem.py` checks the deployed values, and byte identity when a sibling GroundStation26 checkout is present). CMake reads its `modem` section into
the build. A default build writes the link-wide registers of this checked-in
profile; another profile writes them only with
`build.py build --rfd-profile-rolled-out` (CMake `RFD_PROFILE_ROLLED_OUT`). S1, S2 and `LBT_RSSI` are never written. RTSCTS must be 0: USART1
has no RTS/CTS. A compile-time check keeps `RADIO_PACKET_GAP_MS` at least
S6 + 5.

**Mode.** `build.py build --rfd-modem-check default|skip|verify|fix` (CMake
`RFD_MODEM_CHECK`). `default` is `fix` for operational builds and `skip` for
simulator builds (`SEDS_FIRMWARE_SIM_TEST=1`), like GS. LoRa builds never
check. `skip` compiles the check out.

**Flight gate (differs from the original R2).** R2 asked for a FLIGHT_STATE
received over CAN within 10 s of boot, with the radio held until then.
FLIGHT_STATE is written only by the ground station (FC and RF are read-only
replicas, and a replica never answers a refresh from its own cache), so on
the real bay it reaches RF only over the radio. Holding the radio would have
cost a 10 s outage on every boot and the check would never have run. The
implemented gate:

* a restored (persisted) flight phase other than Startup, Idle, PreFill or
  FillTest, for example after a brown-out reboot in flight, skips the check
  at once and never holds the radio;
* otherwise the radio runs normally. The first validated FLIGHT_STATE
  received after boot (the same authorization path as every managed update)
  decides: Startup, Idle, PreFill or FillTest runs the check, anything else
  skips it for this boot. GS answers RF's refresh with its stored flight
  state, so this is normally a few seconds after the link comes up; on a
  fresh install with no stored state it is the operator's first flight-state
  change (for example Startup to Idle);
* only the check itself holds the RF radio: no frame TX and air RX ignored,
  about 3 s if nothing differs and about 11 s if it writes, bounded at 40 s.
  CAN, routing, time sync and the watchdog keep running during the check.
  A flight-state change seen over CAN closes the gate before the first write
  and before `AT&W`. Queued radio frames age out as on a busy link.

**Report (reuses BOARD_HEALTH; no SEDSnet schema change).** Instead of a new
`RFD_MODEM_STATUS` type, RF reports in BOARD_HEALTH words 13, 15 and 22,
which other boards use only with flags 4, 64 and 32 (RF leaves those flags
clear; older RF firmware leaves the words zero). It is sent in every
5-second BOARD_HEALTH report at priority 0.

| Word | Bits | Field |
|---|---|---|
| 13 | 3:0 | version = 1 (0: not reported) |
| 13 | 7:4 | outcome: 0 skipped, 1 matched, 2 applied, 3 mismatch, 4 apply_failed, 5 verify_failed, 6 no_response, 7 failed |
| 13 | 9:8 | map source: 0 ATI5 names, 1 fingerprint, 2 unconfirmed |
| 13 | 11:10 | mode: 0 skip, 1 verify, 2 fix |
| 13 | 12 | built with the profile rolled out |
| 13 | 14:13 | RTSCTS observed (3 unread) |
| 13 | 23:16 | flags: 0x01 EEPROM saved, 0x02 rebooted, 0x04 data mode resumed with ATO, 0x08 forwarding entries, 0x10 skipped or stopped by the flight gate, 0x20 pending (waiting for the flight state or running), 0x40 needs attention |
| 13 | 31:24 | S5 observed (u8) |
| 15 | 15:0 / 31:16 | S3 / S6 observed |
| 22 | 15:0 / 23:16 / 31:24 | S11 / S2 / S4 observed |

u16 fields use 0xFFFF and u8 fields 0xFF for unread; 0xFE means 254 or more.
Values are those read after the last write and verification. GS decodes the
block (`rfd_modem::decode_rf_status`), shows it in Connectivity and raises a
notification for every finished RF check.

**Build cost.** Release RF image: flash 471,332 B of 474,624 B (unmodified
dev 465,968 B, +5.2 KiB), RAM 112,840 B of 114,688 B (+16 B static). The
check runs on the existing telemetry stack. The tightest combination,
`SEDS_FIRMWARE_SIM_TEST=1` with `--rfd-modem-check fix`, leaves 36 B of flash.

**Simulator.** `FirmwareSimulator` models the RFD command mode per radio
station (`FIRMWARE_SIM_RFD_MODEM`, see its `docs/rfd-async-mesh-model.md`).
In a linked RF + FC + GroundStation scene the RF board, booted with S6=20,
S11=1 and RTSCTS=1 in the 4.03 layout, received Idle from GS about 1 s after
boot, wrote S6, S11 and S19 (never S18 `LBT_RSSI`), saved, rebooted, verified
and reported `applied`, with the radio held for about 10 s; the link then
resumed. With S5=0 (not rolled out) it repaired S6, left S5 and reported
`mismatch`. A silent modem gave `no_response` after an 8 s hold, and an
Ascent flight state skipped the check without touching the radio.

**R4. Not needed.** The model gives no gain for RF ACK piggybacking, an RF
host backlog cap, or larger batches (see the model doc). Not implemented.

**SDK change (described, not made): per-side adaptive reliable retry.**
The model ranks RTT-derived retry as the only host change that removes
retry exhaustion under every fit, with about 5x fewer uplink copies.
`GS_RELIABLE_RETRANSMIT_MS` is global and also governs the fill link, so it
must not be used. The change needed is a per-side policy, for example
`RouterSide::with_reliable_retry(Adaptive { min_ms: 250, max_ms: 3000,
initial_ms: 1000, backoff: 1.5, deadline_ms: 15000 })`, with SRTT taken from
first-ACK samples on that side. GS would set it on the radio side only.
