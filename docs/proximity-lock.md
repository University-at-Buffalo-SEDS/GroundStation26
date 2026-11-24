# Personnel proximity lock

An optional lock that refuses arming and pressurizing while people are near
the rocket. Each participating phone or computer works out **on the device**
whether it is within a set distance of the rocket, and tells the station only
"in range" or "out of range". The station never receives or stores a
location.

The frontend side (permission prompts, background reporting per platform,
the device test checklist) is in the UI repository's `docs/proximity-lock.md`.

The proximity lock never sends a location. The separate recovery search
([backend/recovery-search.md](backend/recovery-search.md)) is the only
feature that shares a track. It needs its own explicit opt-in on each device,
only while an operator runs a search, and the proximity lock does not use it.

## Summary

| | |
| --- | --- |
| Default | Off. An administrator opts the station in (Settings > Safety). |
| Who takes part | Every signed-in device, once the station is opted in, unless its user opts out on that device. A denied location permission counts as opting out. |
| What a device sends | `participating`, `in_range` (true / false / null = cannot tell), an accuracy class, the kind of reference, a heartbeat counter. Never a latitude, longitude, distance or bearing. |
| What is refused | New **arming**, **fill sequence** and **launch sequence** commands (and, if an admin opts in, single manual valve opens), per class as configured. |
| What is never refused | Valve self-test, Abort, Stop, vent and dump (either direction), supply closes, PILOT close, igniter off, Pause and Cancel fill, Rewind, Reset, Recover, and every FC evaluation/abort command. |
| Already running | Continues. Nothing is actuated because someone enters range; a warning is raised. |
| Devices that stop reporting | "Unknown" after the heartbeat timeout (default 60 s). Policy: **warn** (default) or **block**. |
| Override | Administrator only, 1–30 min, with a reason and a confirmation; logged and shown on every view; ends early on request. |

## Settings

`GET /api/proximity` (ViewData) returns the status, including the settings.
`PUT /api/proximity/settings` (administrator; partial; unknown fields
refused) changes them. They persist in `proximity_lock.json` in the station's
data directory (next to `igt_settings.json`). Each station has its own
settings: configure the primary and the RRLS.

| Field | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | false | | The lock is on. Turning it off forgets every device and ends an override. |
| `radius_m` | 150 | 25–2000 | Lockout radius around the rocket. |
| `exit_margin_m` | 30 | 5–500 | Hysteresis: a device in range leaves only beyond radius + margin (see below). |
| `blocks.arming` | true | | Postinit, operator Advance (AdvanceFlightState / phase step), LIVE PYRO confirm, and the automatic Fill → Armed publication (which waits). |
| `blocks.fill` | true | | Fill sequences: StartFill (and the recorded Fill), ContinueFillSequence, NitrogenTest, Turbocharge. |
| `blocks.manual_valves` | false | | A single Nitrogen, Nitrous or PILOT toggle that would open. Off by default so pad crew can work valves by hand. |
| `blocks.launch` | true | | Launch, GroundStationLaunch, IgniterSequence, Igniter that would turn on, Pilot that would open. Launch also needs Armed. |
| `stale_policy` | `warn` | `warn` / `block` | What an unknown device does. |
| `heartbeat_timeout_s` | 60 | 30–600 | A participating device silent this long is unknown. Devices report every 15 s. |

A toggle (Nitrogen, Nitrous, Pilot, Igniter) is refused only in its opening
direction: the station judges the direction from the requested or confirmed
output state, and an unknown state counts as opening. The explicit closes
(`NitrogenClose`, `NitrousClose`) are never refused.

Every change is an audit event (see Audit).

## Reports (privacy model)

`POST /api/proximity/report`, signed in (a session, or a watch token: the
watch-token scope allows this one path). Exactly these fields; anything else
is a 400 (`deny_unknown_fields`):

```json
{"v": 1, "device_id": "prox-…", "device_name": "Alex's iPhone", "platform": "ios",
 "participating": true, "in_range": true, "accuracy": "high", "reference": "rocket",
 "background": true, "opt_out": null, "seq": 42, "sent_ms": 1763600000000}
```

- `accuracy` is a class (`high` ≤ 20 m, `medium` ≤ 100 m, `low`), not metres.
- `reference`: `rocket` (the rocket's fresh GPS fix, which the device already
  receives) or `pad` (the configured FC launch site, when there is no fresh
  rocket fix). With neither, the device sends `in_range: null`.
- `opt_out` (when not participating): `user`, `permission_denied`, `unsupported`.
- Freshness is the station's receive time; `sent_ms` is informational.

The station keeps, in memory only, per account and device: the name, the
platform, the flags above and the last receive time. A restart forgets every
device. Nothing is kept while the lock is off. The status lists device names
and states; the recording (`GS_PROXIMITY_LOCK` rows) logs only the lock state
and the counts. The only coordinates in the status are the configured launch
site's (`pad`), so devices can fall back to it; that is the rocket's place,
never a person's.

Tests: `proximity_lock::tests::reports_with_any_location_field_are_refused_and_nothing_is_stored`
(GS) and `the_serialized_report_carries_no_coordinates_distance_or_bearing` (UI).

### On the device

The device evaluates against the reference:

- **Conservative accuracy**: if the fix's accuracy circle reaches the radius
  (`distance − accuracy ≤ radius`) the device is in range. An unknown accuracy
  counts as 100 m.
- **Hysteresis**: a device in range leaves only when `distance − accuracy >
  radius + exit_margin`, so it must be beyond the radius by at least the margin
  or its accuracy, whichever is larger, and does not flap at the edge.
- A fix older than 5 minutes, or no reference, gives `in_range: null`.

### Rate limits

At most 12 reports per 30 s per device (429 beyond), 8 devices per account,
256 devices per station. Devices send every 15 s and at once on a change;
opted-out devices repeat every 5 minutes.

## Enforcement (safety model)

While the lock is on and at least one participating device with a fresh report
is in range (or, under `block`, is unknown), the station refuses the blocked
commands with **409**:

```
StartFill refused. Proximity lock: 1 person within 150 m of the rocket (Alex's iPhone).
```

Where it applies (one decision, `proximity_lock::refusal`):

- `POST /api/command` and the websocket command path (`web.rs`), including
  commands an RRLS forwards (they are admitted through the same path);
- the physical panel: the controls are disabled in the action policy, which
  the panel's buttons and LEDs follow; a press says why;
- the ground test LIVE PYRO confirm (`/api/igt/confirm`);
- the automatic Fill → Armed publication (`operational_policy`), which waits
  and says so once;
- the action policy: blocked controls are disabled with the reason as their
  tooltip; a control already disabled for another reason keeps it, with
  "Also: Proximity lock: …" added.

The same reason is in the status (`reason`), the websocket frame
`ProximityLock`, the About safety catalog (section "Personnel proximity
lock", with the live state), and the banner on every view.

### Mid-fill: warn, do not actuate

The lock refuses **new** arming and pressurizing commands only. A fill,
nitrogen test, turbocharge or launch sequence already admitted continues
under operator control, and the station does not close, vent or abort
anything because someone entered range. Reasons:

- an automatic action triggered by a phone's location is a new failure mode:
  a GPS glitch could close valves or vent mid-fill and leave the vehicle in a
  worse state than letting the operator decide;
- the operator still has every safing control, none of which the lock ever
  refuses.

When a device enters range while the vehicle may hold pressure (PreFill,
FillTest, NitrogenFill, NitrousFill, Armed, or Turbocharge active) the station
raises a **warning** (alert sound and banner) and a notification naming the
device. In other phases it is a notification. A participating device that
becomes unknown is a notification (a warning during a fill under `block`).

### Stale or missing flags

A participating device that stops reporting (app killed, phone off, no
signal) is unknown after `heartbeat_timeout_s`. Opted-out devices are never
unknown.

- **warn** (default): unknown devices are listed prominently in the banner and
  the Actions tab, and do not block. A dead phone or a dropped connection
  does not stop the operation; the trade-off is that a person whose phone
  stopped reporting near the pad is not caught.
- **block**: unknown devices block like a device in range until they report
  again, an administrator removes them (`POST /api/proximity/dismiss
  {"id"}`, listed by name in Settings), or 12 h pass. Safest, but one dead
  phone stops arming and filling for everyone until someone acts.

Recommendation: **warn**, with a pre-operation check that every pad-crew
phone shows "reporting" and the crew briefed that the lock is a backstop, not
a substitute for the range-safety roll call. Use **block** when the pad is
not visible from the control point.

A station restart forgets every device; with `block`, the lock is therefore
open until the devices report again (within one heartbeat, 15 s).

### Override

`POST /api/proximity/override {"minutes": 1–30, "reason": "…", "confirm": true}`
(administrator; the UI shows a confirm dialog). While active the lock does
not refuse, the condition is still evaluated and shown ("PROXIMITY LOCK
OVERRIDDEN by admin (8:30 left): pad crew filling"), and the override is in
the status, the banner on every view and the safety catalog.
`POST /api/proximity/override/end` (anyone with command permission) ends it
early; otherwise it expires by itself and the lock applies again at once.
Start, end and expiry are audit events and a warning/notification. Turning
the lock off ends it.

## RRLS and relay setups

- Devices report to the station they are connected to.
- **Primary admission**: every blocked command is admitted on the primary,
  including those an RRLS user sends (the RRLS forwards them over the radio
  and the primary admits them through the same path). The primary's lock
  (its devices) applies to all of them.
- **RRLS**: an RRLS evaluates the devices that report to it (with its own
  settings). It refuses forwarding a blocked command while its own lock is
  active (409, before anything is sent), and adds its reason to the primary's
  mirrored action policy. A forwarded command therefore passes both locks.
- **RRLS takeover**: while the primary is out of range the RRLS admits only
  AV-bay recovery commands (parachute, evaluation, reinit); fill, launch and
  ignition are unavailable anyway, so the lock has nothing more to refuse.
- **Gap**: devices connected only to the RRLS do not block a command sent
  from the primary, because the flags are not relayed over the radio. In
  practice the RRLS network is at the recovery site; pad crew phones should be
  on the primary's network. Relaying a one-byte summary over the mesh is an
  open question (radio budget).

## Audit

`GET /api/proximity/audit` (ViewData) returns the last 200 events; each is
also a station message (persisted with the recording database's messages) and
a log line. Events: settings changed (who, each field before → after), a
device entered range / left range / became unknown / opted out (and why) /
opted back in, a device removed (by whom), override started / ended /
expired, and every refused command (command, actor, reason). Device names and
accounts only; never a place.

## Status

```json
{"settings": {…}, "heartbeat_interval_s": 15, "active": true, "condition": true,
 "reason": "Proximity lock: 1 person within 150 m of the rocket (Alex's iPhone)",
 "warning": null,
 "summary": "Proximity lock: 4 devices reporting, 1 in range (Alex's iPhone), 2 opted out",
 "counts": {"reporting": 4, "in_range": 1, "unknown": 0, "opted_out": 2},
 "devices": [{"id": "3fa2…", "name": "Alex's iPhone", "user": "alex", "platform": "ios",
              "state": "in_range", "accuracy": "high", "reference": "rocket",
              "background": true, "last_report_ms": 1763600000000}],
 "override": null, "pad": {"lat_deg": 42.0, "lon_deg": -78.0},
 "blocked_commands": ["Postinit", "AdvanceFlightState", …]}
```

`active` is true while the lock refuses; `condition` is true while someone is
in range (or unknown under `block`), overridden or not.

## Platform limits (summary)

| Platform | Reports |
| --- | --- |
| iOS | In the background with "Always" (background location updates, region monitoring around the rocket, significant-change monitoring). With "While Using", while the app runs (iOS keeps updates started in the foreground going until the app is suspended or killed). |
| Android | In the background through a location foreground service (a notification shows it) and a Geofencing API fence, with "Allow all the time"; with "While using the app", while the app is open. |
| macOS, Windows, Linux | While the app is open; best effort. |
| Web | While the page is open and visible (browser Geolocation API). |
| Watches | Do not take part. |

When a device cannot report (app killed, suspended, page closed), its
heartbeats stop and the stale policy applies.

## Code

- `backend/src/proximity_lock.rs`: settings, reports, evaluation, override,
  audit, routes; `proximity_lock/tests.rs`.
- Admission hooks: `web.rs` (`dispatch_command_inner`, websocket commands),
  `command_proxy::submit_with_origins` (RRLS forward), `igt.rs`
  (`post_confirm`), `gse/operational_policy.rs` (automatic Armed),
  `state.rs` (`action_policy_snapshot`), `gpio_panel.rs` (press notice).
- `safety_catalog_rules.rs`: section `proximity`.
