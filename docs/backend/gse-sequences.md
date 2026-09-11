# Ground support equipment sequences

The Actions tab contains a grouped **Reconfigure GSE** panel, a live equipment
scene, pressure/noise status, and sequence settings. Individual valves remain in
the **Manual GSE valves** group. Do not operate hardware from this software until
qualified personnel have reviewed the plumbing, command mapping, pressure limits,
and failure behavior, and completed an isolated-gas bench test. Software commands
and board acknowledgements are not independent valve-position feedback. Mechanical
pressure regulation, relief protection and a hardware emergency stop are required.

## Configuration

`GET /api/gse/config` reads settings; `POST` or `PUT` saves them (SendCommands
permission required). `GET /api/gse/status` returns the runtime state (ViewData).
Commands use the existing authenticated WebSocket command channel and action policy.
Configuration persists in `backend/config/gse_sequence.json`, overridable with
`GS_GSE_CONFIG`. Runtime calibration and nitrogen-pass status never survive restart.

Set `nitrogen_target_psi`, `pressure_ceiling_psi`, and `maximum_zero_offset_psi` in
the UI. The last two ship unset: automatic operations are unavailable until valid
limits are supplied. Target plus zero allowance must be below the pressure ceiling.
The ceiling is a software trip threshold, not a guarantee against overshoot.
Incomplete settings can be saved to select manual panel mode without enabling tests.

Calibration opens vent/dump, closes supplies/pilot, waits for acknowledgements, then
collects five seconds of fresh PT samples (at least ten). It records average, min,
max and maximum absolute deviation from the average. Zero is average ± that measured
deviation. Thus 0 ± 3 psi and 5 ± 2 psi are handled without a hardcoded noise floor.
The configured empty-PT allowance bounds the absolute extrema, not just the average;
the latter example requires at least 7 psi. Never enlarge this allowance to teach a
pressurized tank as empty. A PT reading alone cannot establish safe depressurization.

## Actions

| Action | Pilot | Vent | Dump | Nitrogen | Nitrous |
| --- | --- | --- | --- | --- | --- |
| Start fill (configuration first) | Closed | Open | Closed | Closed | Opens only after acknowledgements and zero PT |
| Pause fill | Closed | Closed | Closed | Closed | Closed |
| Cancel fill | Closed | Open | Open | Closed | Closed |
| Nitrogen test | Closed | Closed | Closed | Controlled | Closed |
| Self-test completion / nitrogen dump | Closed | Open | Open | Closed | Closed |

The nitrogen test calibrates zero, closes all valves, and raises pressure in 50 psi
steps, with a final smaller step to the configured target. Each step closes nitrogen,
allows at least two seconds to settle, and holds for five seconds. Rolling averages
filter isolated noisy samples. A drop greater than 3 psi plus the two-reading noise
envelope fails immediately; a sustained five-second mean loss greater than 3 psi
also fails (possibly inconclusive noise), never passes. This is not a certified leak
measurement. Following all holds, dump/vent open; at least a second of samples inside
the calibrated zero band is required before the passed notification and fill unlock.

Self-test requires explicit confirmation that gas supplies are isolated. It opens
one of the five valves, waits for acknowledgement, holds two seconds, then confirms
closure before moving to the next. Ignition and retraction are excluded. Confirmation
is runtime-only and consumed when a test starts. Once nitrogen testing or filling
starts, self-test stays locked for the process session. Restart requires a new
nitrogen test; it does not preserve a previous pass.

Manual valve changes invalidate nitrogen-pass status. Manual valve, launch and
retraction controls are locked while a sequence owns the valves; cancel first.
Missing/stale PT (2 s), invalid PT, pressure ceiling, lost interlock, command transport
errors, missing acknowledgements (5 s), and step/dump timeouts (60 s) stop sequencing.
Faults request supply closure and dump/vent opening while prelaunch. Once the vehicle
leaves prelaunch, only supply closure is requested. Verify physical state after any
fault; a network failure can prevent all requested recovery actions.

## Physical panel

`grouped_panel=true` maps the existing five-button cluster as below; LEDs follow the
mapped action policy. Relabel the panel before enabling this mode. Other buttons
retain their existing meanings. `grouped_panel=false` restores individual valves.

| Existing label / BCM input | Grouped action |
| --- | --- |
| Pilot / 13 | Valve self-test |
| Nitrogen / 23 | Nitrogen test |
| Nitrous / 17 | Start fill |
| Vent / 20 | Pause fill |
| Dump / 12 | Cancel fill |

## Models

![Equipment scene rendered in a standalone browser preview](gse-equipment.png)

The included unbranded GLB scene depicts nitrogen/nitrous tanks, manifold, plumbing,
launch tower and vehicle. Valve colors reflect last acknowledged state, not measured
flow. Animated `nitrogen-test`, `nitrous-fill`, and `dumping` clips indicate sequence
activity only. These illustrative models are not engineering CAD or tank-level data.
Regenerate with `node scripts/build_gse_models.mjs`. The Vehicle tab has a separate
minimal two-stage model and supports uploaded stage GLBs via `/media`.

The bundled model-viewer 4.3.1 renderer is Apache-2.0 licensed; see
`backend/assets/model-viewer.LICENSE`. Built-in uncompressed models require no CDN.
Uploaded models using compression may need additional decoder assets.
