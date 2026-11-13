# Radio frame recovery qualification — 2026-10-20

A corrupted UART length can be numerically valid yet longer than the available
bytes. GroundStation previously waited indefinitely and withheld an intact frame
behind it. The new regression reproduces that failure before the fix. The decoder
now discards one sync byte after a bounded partial-frame wait, then resumes sync
search while retaining following bytes. Empty/invalid headers are skipped in the
same parsing pass rather than waiting for another serial read.

The normal bound is two seconds, matching RF firmware. It increases for UART
rates too slow to carry a maximum frame within that interval. Simulator builds
use virtual time, so slow host execution does not expire legitimate frames.
Tests cover silent recovery, split valid payloads containing frame-looking bytes,
maximum payload boundaries, invalid headers, and slow emulation.

This bounds damage amplification; it does not repair corrupted bytes or prove
that radio latency is resolved. The live RF health report showed no UART drops,
CAN drops, allocation failures or admission refusals in the captured image, while
GroundStation still logged side-transport CRC errors.

The separate FTDI spare reports RFD ASYNC 4.03 on RFD900X2, 115200 UART, 64 kbit/s
air rate, MAX_DATA 3968, MAX_RETRIES 10, GLOBAL_RETRIES 10, broadcast destination
32768, and RTSCTS 0. Those settings are evidence from the spare, not a verified
capture of the active pair. Do not apply peer-to-peer register examples to async
firmware. Even async register indices differ: the spare reports RTSCTS at S19;
the older async manual lists S18. Read and match parameter names before writing.

High broadcast retry counts can consume the airtime budget assumed by firmware.
A repeat/queue test and readback of both active modems are needed before calling
this the physical-link root cause. No modem settings were changed in this work.

## Live three-radio observation — 2026-10-26

An eight-second passive trace of the running Pi backend at revision `b7d8e03`
contained 12 valid recovery READY frames and 72 valid relayed data frames. READY
replies matching heartbeats emitted during the capture arrived 2.477–5.629 seconds
later. Multiple old replies were read in one burst. This exceeds the two-second
command-readiness lease and explains rejection of those replies; it does not
establish where the delay accumulated or verify the reported 40-second maximum.
The source on this Pi uses RFD capacity backpressure without LoRa turn windows.
Current RF source defaults `RFBOARD_LORA_SCHEDULER=OFF`; the flashed image's build
flag still needs confirmation. No modem configuration was changed.

The simulator's finite UART and airtime model is not an implementation of RFD
firmware retries, addressing, or mesh arbitration. Passing that model cannot
qualify the active modem configuration. Read back the active radios before
assigning the delay to retries or changing the relay lease.

A follow-up ARM64 `ppoll` trace found repeated normal 50 ms UART timeouts during
multi-second receive gaps, followed by reads completing in tens of microseconds.
It did not show a stalled host receive loop during those gaps. This narrows that
observation to data availability upstream of the GS read loop, without proving
which modem, RF arbitration, or sender-side queue caused it. The user confirmed
that the reported 3–40 second delay concerns command response, so a simultaneous
command-send/receipt capture is still needed to locate the full delay.

## Reproduced packetization stall and bounded write batches

The production UART capacity gate previously admitted small frames continuously
without a bound on packetization delay. A deterministic simulator regression
uses that exact Rust gate to schedule the captured 42-byte write size. The actual
`SedsRadioMedium` source delivers it within 17.2 ms under the earlier 2 ms /
256-byte profile, but takes 5.11 seconds with a 50 ms idle threshold and a
3,968-byte packet limit. No fixed multi-second delay is injected. The latter is
an explicit configuration envelope; the active hardware's registers remain
unverified.

GS/RRLS and RF now allow bounded 100 ms batches and then preserve 55 ms of serial
silence after actual UART completion. Average airtime admission remains a
separate constraint. This prevents a stream of short frames from postponing the
packet boundary indefinitely, without adding LoRa turn windows or changing
command retries/recovery leases. A per-frame gap was rejected because it caused
a reliable-delivery regression; the bounded batch passes the unchanged campaign.

The final raw-byte fixture measures 173.7 ms with no loss or reordering. It also
passes on Renode's real virtual clock, and disabling only the new boundary still
reproduces 5.11 seconds. Startup logs show the active host burst/gap values.
`GS_RADIO_PACKET_BURST_MS` and `GS_RADIO_PACKET_GAP_MS` control these bounds;
the baud rate comes from the selected serial link. RF needs the matching firmware
build. Hardware confirmation still requires modem readback and physical command
receipt timing; no active station configuration or firmware was changed here.


## Persistent application profile

Raw-UART GS and RRLS starts load the checked-in `backend/comms/rfd-profile.json`
by default, including rebuilt binaries. The compatible profile retains a 55ms
UART idle gap for the existing 50ms modem serial-break setting, while budgeting
32kbit/s usable capacity rather than assuming all 64kbit/s is available. Hardware
flow control remains disabled unless explicitly enabled on a verified wired link.
This profile loads application settings; it never writes modem registers.

A serial link can persist a `rfd_profile` path in its communications configuration.
Relative paths resolve beside that communications JSON. `GS_RADIO_PROFILE` can
select another file for a particular launch; conflicting per-setting environment
overrides are rejected. RF firmware uses its checked-in `config/rfd-profile.json`
on default builds and CMake rebuilds, preventing stale shorter-gap cache values.

The optional `config/radio/rfd-async-64k-low-latency.json` profile requires matched
modem changes on GS, RF and RRLS before selection. Its 15ms application gap is
incompatible with an unchanged 50ms modem serial-break setting. Collect public
settings and forwarding-table readbacks from all three radios before using it;
no automatic modem retuning is performed. CTS on RRLS alone cannot protect
unwired GS/RF modem ingress or shared airtime capacity.

### Broadcast retry settings

The [RFDesign ASYNC manual](https://files.rfdesign.com.au/Files/documents/RFD900x%20Asynchronous%20User%20Manual%20V1.1.pdf)
defines S4 as the maximum retry count and S5 as the maximum broadcast retry
count. DESTID 32768 selects broadcast on newer ASYNC firmware. A high broadcast
retry limit can consume airtime in addition to application-level reliable
retries. The manual does not specify a deterministic number or timing of copies
for every packet; do not infer an exact airtime multiplier from these settings.

Read back all three modems before changing a profile. Verify unique node IDs,
the same network/air/framing settings, and the forwarding table (`AT+F?`): fixed
modem forwarding and application RRLS forwarding are separate mechanisms.
The RF repository's `scripts/rfd_profile.py --profile <profile.json>
--readback gs=<public.json> --readback rf=<public.json>
--readback rrls=<public.json>` checks saved public readbacks without opening a
serial port or writing settings. Selecting an application profile does not
configure its corresponding modem retry or serial-break settings.
