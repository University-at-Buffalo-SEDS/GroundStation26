# Offline load-cell and pressure export

`export_loadcell_excel.py` is a standalone utility for an existing GroundStation
recording CSV. It does not change calibration or the native application's UI.

Create a virtual environment and install `matplotlib` and `openpyxl`, then run:

```sh
python export_loadcell_excel.py recording.csv --kg-mode recorded
```

Drag over a plot to select the time window and click **Export selection**.
For a headless export, use `--all` or both `--start` and `--end` with ISO dates.
Dates without an explicit offset are UTC. Use `-o result.xlsx` to select a new
output file; existing files are never overwritten.

The default kg mode is `absolute`: it infers a consistent linear mapping from
timestamp-matched raw KG1000 and recorded kilogram pairs, then plots absolute
recalculated mass while retaining the signed/original data. This reconstructs
the recorded conversion, not an independent physical calibration. Nonlinear,
clipped, changing, or insufficient calibration data is rejected. Use
`--kg-mode recorded` to export unchanged readings, or specify a known
`--kg-slope` and `--kg-intercept` for `signed` or `absolute` mode.

The workbook includes raw/converted values, received and source timestamps,
sender-separated charts, extrema, and the regression coefficients and supporting
pairs when recalculation is enabled. Selection uses the common received clock;
device clocks are retained separately. This utility covers KG1000 and pressure
channels, not the KG50 channel.

Run its tests with the same environment:

```sh
python -m unittest discover -s tests -p test_export_loadcell_excel.py -v
```
