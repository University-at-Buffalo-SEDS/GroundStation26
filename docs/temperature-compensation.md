# Load-cell temperature offset calibration

Temperature is recorded with new live zero/span calibration captures. Compensation
uses measured unloaded drift, independently for KG1000 and KG50:

`reference_raw = raw - raw_per_c * (adc_temperature_c - reference_c)`

Mass calibration (including polynomial/tare) is applied to `reference_raw`.
Network KG1000/KG50 remain unchanged raw values. DAQ corrects SD calibrated
records; GroundStation corrects live derived values. Missing/stale temperature
makes compensated values unavailable, rather than silently reporting uncorrected
mass. A zero coefficient keeps legacy behavior.

## Calibration procedure

1. Update DAQ, GroundStation backend, and frontend together. Open Calibration.
2. Unload the cell and let the board and cell reach a stable temperature.
3. Click **Capture unloaded thermal point**. Repeat at another stabilized
   temperature at least 5 C away. Additional points use a least-squares line.
   Do not use loaded points to estimate zero drift.
4. Recapture ordinary zero and known-mass points, fit, and Save. Live captures
   require fresh ADC temperature and DAQ readings; the saved
   `temperature_captures` retains original raw, expected value, and temperature.
   Mass-fit inputs are raw values normalized to the thermal reference.
5. Check unloaded readings and known masses across heating AND cooling. Avoid
   extrapolating beyond the measured temperature range. If thermal coefficients
   change, repeat mass calibration before relying on calibrated outputs.

A single-temperature calibration cannot identify thermal drift. No generic
coefficient is assumed. Manual historical entries without temperature remain
legacy points; use live capture for temperature-aware calibration.

## Acquisition and wire contract

MCP3564R SCAN enables TEMP (bit 12, channel ID 12), CH1 and CH0. TEMP uses unity
gain internally. The first-order datasheet conversion is
`T = 0.00040096 * code * 2.4 - 269.13` (Celsius). Invalid/out-of-range readings
(outside -40..125 C), or readings older than 2 s, are unavailable. Each queued
load sample retains its acquisition-time temperature. Temperature measurements
are never queued as a load cell.

Adding TEMP to every scan reduces per-cell throughput to roughly two-thirds
of the previous two-channel scan, with unchanged OSR/auto-zero settings.
The 500 Hz broadcast setting is a ceiling, not guaranteed sample throughput.
Hardware timing and thermal performance still require bench validation.

- ID 140 `DAQ_ADC_TEMPERATURE`: one float32 Celsius, sent about once/second;
  NaN indicates unavailable. CAN route follows the two load-cell streams.
- ID 141 `DAQ_THERMAL_CALIBRATION`: retained four-float32 variable:
  `[KG1000 reference C, KG1000 raw/C, KG50 reference C, KG50 raw/C]`.
- Existing IDs and raw load-cell scales are unchanged. Firmware restores old
  4-float and 11-float calibration records with thermal compensation disabled.
- GroundStation matches temperature to the same sender and only preceding
  telemetry within 2 s. New fields default to empty for older calibration JSON.
- SD file headers include thermal coefficients; diagnostic temperature rows and
  network temperature packets permit replay. Test-fire CSV uses recorded
  temperature for KG1000 compensation and includes an ADC temperature column.
- The backend also provides `/api/calibration/thermal` and the authenticated
  `/api/calibration/capture_thermal_zero` endpoint. The normal dashboard has the
  same capture action; saved temperatures are preserved through UI edits.

## Research and limitations

[Microchip MCP3561/2/4R datasheet](https://download.mikroe.com/documents/datasheets/MCP3562_datasheet.pdf),
sections 5.1.2 and 5.15.3.2, specifies the die-temperature transfer function,
unity gain during TEMP scans, and uncalibrated sensor offset/gain. An absolute
thermometer calibration is not necessary for an empirical relative-drift fit,
but the sensor measures the ADC die, not the remote load cell.

[HBK temperature effects in strain gauges](https://cloud.hbkworld.com/pt/knowledge/resource-center/articles/strain-measurement-basics/strain-gauge-fundamentals/article-temperature-compensation-of-strain-gauges)
distinguishes temperature-induced zero errors and sensitivity effects.
[TI bridge-measurement guidance](https://www.ti.com/document-viewer/lit/html/SBAA290)
describes offset/drift in bridge measurement chains and electrical compensation.

This implementation addresses repeatable zero offset correlated with ADC die
temperature. It does not promise elimination of creep, hysteresis, load-dependent
sensitivity drift, thermal gradients or differing board/cell thermal lag. If
board temperature is a poor predictor, measure temperature at the load cell.

## Long unloaded capture and noise filtering

Open **Calibration → Long unloaded zero capture and noise filter**, select the
cell, confirm that it will remain unloaded, and start the capture. Default is
30 minutes; supported durations are 60 seconds to 24 hours. One capture runs at
a time. Closing the UI does not stop it. Stop and analyze early, or let the
duration expire. A backend restart interrupts acquisition; flushed CSV samples
remain available but sessions do not resume automatically.

The backend records each received raw load-cell sample with its timestamp,
`known_load_kg=0`, and a preceding fresh ADC temperature from sender DAQ. It
rejects missing/invalid temperature and duplicate/out-of-order timestamps.
A 4096-sample queue isolates disk writes from telemetry; dropped/rejected counts
are reported. Recordings are bounded to 10 million samples per capture and
flushed every second. Files are saved under
`backend/calibration/zero_captures/` (override with `GS_ZERO_CAPTURE_DIR`).
The CSV and JSON summary paths are shown in the panel. Back up these recordings;
starting another capture replaces the live status, not the files.

Analysis reports sample count, temperature coverage, zero at the reference
temperature, fitted raw/C slope, residual RMS noise, adjacent-difference noise
estimate, residual raw/hour trend, and temperature/time correlation. Moments
are accumulated in float64 with constant memory. The adjacent-difference
estimate is `sqrt(mean((delta_raw - slope*delta_temperature)^2)/2)`; correlated
noise changes this estimate. No time-based drift is silently subtracted.
A monotonic warm-up can confound temperature with creep/time drift: capture
heating AND cooling where possible and inspect the recorded residuals.

At least 1000 accepted samples spanning 60 seconds are required to apply a
result; drift fitting also requires 5 C of temperature coverage. Without that
coverage, uncheck **Apply fitted temperature drift** to apply zero and noise
filtering only. Applying establishes the observed unloaded baseline as zero,
saves the analysis-derived noise profile, and sends filter settings to DAQ.
Verify an unloaded zero and known mass afterwards; recapture mass points if
the temperature model changed.

Choose the smoothing time constant explicitly (0 disables, 0–2000 ms range).
100 ms is the initial UI suggestion. Both DAQ and GroundStation use:

`filtered += dt_ms / (tau_ms + dt_ms) * (temperature_corrected_raw - filtered)`

This low-pass filter preserves DC and small sustained loads; it has no zero
clamp/deadband. It trades high-frequency noise for latency (approximately 3*tau
to reach 95% of a step). It cannot eliminate drift, bias, or low-frequency noise.
Filtering occurs before the mass polynomial/tare. DAQ operates on individual
acquisitions; GS operates on network window averages, so numeric results need
not be identical despite sharing the same algorithm/time constant. States reset
after missing/invalid samples, gaps exceeding 2 s, and relevant calibration
changes. Raw network and raw SD measurements are never filtered.

ID 142 `DAQ_FILTER_CALIBRATION` carries two float32 time constants in milliseconds
`[KG1000, KG50]`. These settings persist across DAQ restarts and are included in
SD calibration headers. Older 15-float thermal records restore with filtering
disabled. GroundStation calibration JSON preserves `noise` profiles when edited
through the frontend. Test-fire CSV exports use the same smoothing calculation.

Every SD raw load-cell row additionally stores `adc_temperature_c` and
`adc_temperature_code`: the uncorrected Celsius measurement and signed raw TEMP
ADC conversion associated with that queued sample. These are independent of
thermal correction and smoothing. NaN Celsius marks invalid or stale temperature;
do not treat an accompanying retained ADC code as a fresh valid reading.
Slow temperature diagnostic rows and temperature telemetry are also recorded.
