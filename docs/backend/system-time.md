# System date/time and local data exports

The Data tab can export a UTC interval across all `groundstation_recording_*.db`
files in the backend's recording directory. Selection uses **row receive time**,
not file names or modification times. Start is inclusive; end is exclusive.
The CSV includes the originating database and row ID; files are grouped by name
and rows by ID, not globally time-sorted. Duplicate measurements are preserved.
Each file has a finite ID snapshot, and output streams in 512-row pages.
No matching rows returns a clear 404. A corrupt database fails the scan rather
than silently omitting its data. Individual-session downloads remain available.

## Per-user clock authority

Network UTC is also available in Actions and Settings in every flight state.
`POST /api/network_time` accepts `{"source":"client","utc_ms":1793052000000}`
with the same explicit `set_system_time` account permission. This updates the
primary station's SEDSnet UTC source, without invoking systemd or changing the
host wall clock. The selected client source remains anchored to SEDSnet's
monotonic clock; periodic host-clock fallback refreshes do not overwrite it.
The API reports admission failures instead of claiming success. SEDSnet decides
how the network clock follows the new UTC: more than 1 s off, it **steps** to it
at once and starts a new time-base epoch, which every board adopts within one or
two time sync exchanges; within 1 s, or while the station's step policy
inhibits steps, it slews at 20 ppm (boards track it). Steps are allowed in
Startup/Idle or before any board is locked, never from Armed through Recovery.
A correction made while steps are inhibited keeps slewing and steps as soon as
the policy allows it again (the next one-second host refresh or operator set).
Packets carry their epoch, so a command or report stamped just before a step is
judged by its true age and ordered correctly on both sides of it; nothing has to
be re-sent after a step. Network time labels use the router's synchronized UTC
when available. RRLS must receive its time from
the primary; direct network-clock writes on RRLS return a conflict response.

AV-bay underglow, FC buzzer, and T-clock reset are available in normal, HITL,
and test-fire modes in every flight state. Account command permissions and
an explicit software-control disable still apply. T-clock reset changes the
displayed launch clock without resetting flight state or board sequences.

Add `"set_system_time": true` under a user's `permissions` in `users.json`, or
use **Set System Date/Time** in `backend/tools/users_config_gui.py` (GUI/TUI).
The CLI also supports `--set-system-time` / `--no-set-system-time`.
This permission defaults to false, is independent of command authority, and is
never available anonymously. It is reread on each request, so revocation affects
existing sessions. No existing user is granted this permission automatically.

The UI offers the client clock, a manually selected UTC date/time, or a complete
SEDSNet UTC reading (RF supplies this after valid GPS time). These are explicit
authorized operations, not unsolicited clock changes from telemetry.

`GET /api/system/time` returns current OS UTC, available network UTC, and whether
the current user has clock permission, whether the host can perform an OS
clock update, and an explanation when unavailable. `network_time_base` reports
the network clock's epoch, the newest epoch heard on the network (above
`epoch` the station is re-anchoring), `holding`, `steps_allowed`, the step
count and `last_step` (`delta_ms`, `reason`, `from_epoch`, `to_epoch`,
`age_ms`); the topology message's station time carries the same fields as
`time_base`. `POST` requires that permission and accepts either
`{"source":"client","utc_ms":1790000000000}` or `{"source":"network"}`.
Network uptime and ordinary packet timestamps are not treated as UTC.

## Linux host setup

Settings → Maintenance → Station system date/time provides an explicit
**Set OS + network from this device** action. Date/time and network-source
changes are also permission-gated at the backend. OS success is followed by a
checked SDK source update; if SDK admission fails, the response explicitly says
that the OS clock changed but the network source was not accepted. The network
clock then steps or slews as described above, and the one-second host-clock
refresh resumes from the corrected OS clock. An RRLS never steps the network
from its own host clock: its corrections slew.
This action does not reset discovery, monotonic clocks, or launch deadlines.

The Linux backend uses `clock_settime(CLOCK_REALTIME)` directly. It needs
**CAP_SYS_TIME** in its effective capability set. The native Pi service runs as
root; if its systemd unit restricts `CapabilityBoundingSet`, include
`CAP_SYS_TIME` in the existing list without removing its other required
capabilities. A dedicated non-root service can receive `CAP_SYS_TIME` through
`AmbientCapabilities`; keep it in the bounding set too. Hardened units using
`ProtectClock=yes` or a syscall filter that rejects clock changes must permit
this explicit function. No service or host setting is changed automatically.

`GET /api/system/time` distinguishes application permission from Linux/kernel
capability. macOS and other unsupported hosts report the reason and do not offer
an OS update button. Kernel rejection is returned truthfully even when capability
inspection initially looked usable. Builds and tests never set the host clock;
tests inject a mock setter. The backend does not invoke a shell, sudo, polkit, or
`timedatectl` for this operation.

NTP configuration remains unchanged. An active time synchronization service may
later correct the wall clock again; choose its policy separately for an offline
Pi. Only an explicit authenticated UI/API request changes the OS clock.

Correct the clock **before active operations/recording**: wall-clock jumps can
expire authentication sessions, alter freshness displays, and affect wall-clock
based application timers. Sign in again after a large correction. Future OS and
application log timestamps then use the corrected clock; old rows are never
rewritten. With no battery-backed RTC or network time on a subsequent power-up,
sync again—the last saved time cannot reveal how long the machine was off.

Range API: `GET /api/recordings/csv?start_ms=...&end_ms=...`, with ViewData access.
An older recording made under an incorrect date still needs its original date
range or an individual-session export; its actual date cannot be reconstructed
without an external reference.
