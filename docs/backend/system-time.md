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
the current user can set time. `POST` requires that permission and accepts either
`{"source":"client","utc_ms":1790000000000}` or `{"source":"network"}`.
Network uptime and ordinary packet timestamps are not treated as UTC.

## Linux host setup

The backend invokes `/usr/bin/timedatectl --no-ask-password set-time @SECONDS`
without a shell. The service account needs polkit authorization for
`org.freedesktop.timedate1.set-time`. Do not run the backend as root or grant it
unrestricted sudo. An administrator can install a root-owned rule such as
`/etc/polkit-1/rules.d/49-groundstation-time.rules`, replacing the example account
with the dedicated, noninteractive GroundStation service account:

```javascript
polkit.addRule(function(action, subject) {
    if (action.id === "org.freedesktop.timedate1.set-time" &&
        subject.user === "groundstation") {
        return polkit.Result.YES;
    }
});
```

This OS permission covers processes running as that account; the application
enforces the separate per-user permission. Do not use a shared interactive account.
The rule is an example, **not installed automatically**. No host clock is changed
by builds or tests. macOS/non-systemd hosts report unsupported status.

If automatic NTP is enabled, systemd may reject manual changes. The API reports
that error; it does not turn NTP off. The administrator must choose how to manage
NTP on the offline Pi. See [systemd time service documentation](https://github.com/systemd/systemd/blob/main/man/org.freedesktop.timedate1.xml).

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
