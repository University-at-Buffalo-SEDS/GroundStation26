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
