# Flight-computer test controls

HITL controls use FC's existing one-byte compatibility protocol. The recovery
threshold buttons configure sensor-failure thresholds; they are not countdowns.

| Control | Wire byte |
| --- | --- |
| Abort threshold 40 | 23 |
| Abort threshold 100 | 24 |
| Abort threshold 250 | 25 |
| Reinit threshold 15 | 26 |
| Reinit threshold 30 | 27 |
| Reinit threshold 50 | 28 |

These values match `remote_cmd_compat` and `extmap` in FlightComputer26.
Earlier GroundStation layouts exposed unsupported 15/70 abort and 12/26/44
reinit labels; five requests were accepted but never dispatched. The shipped
layout and backend now use the actual FC values. No FC firmware change or
new command IDs are required.

Custom layouts and per-command account grants using those old request names
must be reviewed in the admin/configuration tools. They are not automatically
converted to different thresholds or granted permission to the replacement
commands. Existing deployment, launch and HITL interlock behavior is unchanged.

Tests cover every visible flight command's payload and discovered destination
using captured packets only. They do not trigger hardware outputs.
