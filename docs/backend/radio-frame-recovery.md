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
