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

## Integrated ground test controls

The integrated ground test (`docs/integrated-ground-test.md`) adds four FC
commands on the same one-byte protocol. They exist only in the
`integrated_ground_test` station mode (Actions > Ground test) and are refused
and not offered in every other mode. A flight FC image ignores them.

| Control | Command | Wire byte | FC compat name | Station path |
| --- | --- | --- | --- | --- |
| Enter ground test | `IgtEnter` | 29 | `Compat_IGT_Enter` | review only: `POST /api/igt/prepare` then `/api/igt/confirm` |
| LIVE PYRO | `IgtLivePyro` | 30 | `Compat_IGT_Live_Pyro` | review only, Admin, typed `LIVE PYRO` |
| Dry-run | `IgtDryRun` | 31 | `Compat_IGT_Dry_Run` | plain command |
| Exit ground test | `IgtExit` | 32 | `Compat_IGT_Exit` | plain command |

The plain command path (`POST /api/command`, the RRLS proxy) refuses
`IgtEnter` and `IgtLivePyro`.
