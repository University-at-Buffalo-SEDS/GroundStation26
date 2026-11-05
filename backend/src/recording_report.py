"""Headless recording preview/report worker. JSON request on stdin, result on stdout.

CSV/preview need only Python's standard library. XLSX uses openpyxl; PDF uses
matplotlib's Agg/PdfPages backend, never a display server or browser window.
"""
import csv
import io
import json
import math
import re
import sqlite3
import sys
import struct
from collections import defaultdict
from contextlib import closing
from pathlib import Path

MAX_ROWS = 200_000
MAX_SERIES = 128
RAW_TYPES = {"LOADCELL_WEIGHT_KG": "KG1000", "LOADCELL_50_WEIGHT_KG": "KG50",
             "PRESSURE_TRANSDUCER_CALIBRATED": "FUEL_TANK_PRESSURE"}


def read_rows(request):
    root = Path(request["root"]).resolve(strict=True)
    recording = request.get("recording", "")
    paths = []
    for path in root.iterdir():
        if not re.fullmatch(r"groundstation_recording_[0-9_-]+\.db", path.name):
            continue
        if recording and path.name != recording:
            continue
        if path.is_symlink() or not path.is_file() or path.resolve().parent != root:
            continue
        paths.append(path)
    if recording and not paths:
        raise ValueError("Recording not found. Refresh the recording list.")
    start, end = request.get("start_ms"), request.get("end_ms")
    if start is not None and end is not None and end <= start:
        raise ValueError("End time must be after start time.")
    result, history, provenance = [], defaultdict(list), {}
    for path in sorted(paths):
        with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=5)) as db:
            db.execute("BEGIN")
            resolved = db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sd_replay_telemetry'").fetchone()
            if resolved:
                # Explicit derived recording only. main.telemetry is unchanged;
                # its receipt time must never be confused with this replay axis.
                db.execute("CREATE TEMP VIEW telemetry AS SELECT id,timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json,provenance_json FROM main.sd_replay_telemetry")
            if db.execute("SELECT 1 FROM sqlite_master WHERE name='calibration_history'").fetchone():
                for t, sender, sensor, config in db.execute("SELECT timestamp_ms,sender_id,sensor_id,config_json FROM calibration_history ORDER BY timestamp_ms"):
                    entries = history[(path.name, sender, sensor)]
                    snapshot = json.loads(config)
                    if not entries or entries[-1][1] != snapshot:
                        entries.append((t, snapshot))
            upper = db.execute("SELECT COALESCE(MAX(id),0) FROM telemetry").fetchone()[0]
            sql = "SELECT id,timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json" + (",provenance_json" if resolved else "") + " FROM telemetry WHERE id<=?"
            args = [upper]
            if start is not None:
                sql += " AND timestamp_ms>=?"
                args.append(start)
            if end is not None:
                sql += " AND timestamp_ms<?"
                args.append(end)
            for row in db.execute(sql + " ORDER BY id", args):
                if resolved:
                    info=json.loads(row[-1])
                    if not isinstance(info,dict) or info.get("origin") not in ("sd","network"):
                        raise ValueError("Resolved recording has invalid source provenance; no export was produced.")
                    provenance[(path.name,row[0])]=info
                    row=row[:-1]
                result.append((path.name,) + row)
                if len(result) > MAX_ROWS:
                    raise ValueError("Selection exceeds 200,000 recorded rows. Choose a recording or narrower time window; no rows were silently dropped.")
    if not result:
        raise ValueError("No recorded samples in this selection.")
    return sorted(result, key=lambda row: (row[2], row[0], row[1])), history, provenance


def channels(rows):
    result = defaultdict(list)
    count = 0
    for recording, row_id, received, source, sender, kind, values, payload in rows:
        for index, value in enumerate(json.loads(values or "[]")):
            count += 1
            if count > MAX_ROWS:
                raise ValueError("Selection exceeds 200,000 channel samples. Choose a narrower time window; no data was silently dropped.")
            if value is not None and (not isinstance(value, (float, int)) or not math.isfinite(value)):
                value = None
            key = json.dumps([sender or "", kind, index], separators=(",", ":"))
            result[key].append((received, value, source, recording, row_id))
            if len(result) > MAX_SERIES:
                raise ValueError("Too many channels for one report. Select a smaller recording.")
    return dict(result)


def regression(key, samples, streams, history=None):
    sender, kind, index = json.loads(key)
    sensor = {**RAW_TYPES, "LOADCELL_FILL_PERCENT": "KG50"}.get(kind)
    if sensor == "FUEL_TANK_PRESSURE":
        sensor = "IADC"
    if sensor and history:
        import bisect
        epochs, missing = {}, 0
        indexes = {}
        for t, _, _, recording, _ in samples:
            if recording not in indexes:
                records = history.get((recording, sender, sensor), [])
                if kind == "LOADCELL_FILL_PERCENT":
                    records = sorted((entry for source in ("KG50", "KG1000")
                        for entry in history.get((recording, sender, source), [])
                        if source == ("KG1000" if entry[1].get("fill_targets", {}).get("fill_source") == "kg1000_absolute" else "KG50")), key=lambda entry: entry[0])
                indexes[recording] = (records, [entry[0] for entry in records])
            records, stamps = indexes[recording]
            i = bisect.bisect_right(stamps, t)-1
            if i < 0:
                missing += 1
                continue
            stamp, snapshot = records[i]
            cfg = snapshot["calibration"]
            sample_sensor = sensor
            if kind == "LOADCELL_FILL_PERCENT":
                sample_sensor = "KG1000" if snapshot.get("fill_targets", {}).get("fill_source") == "kg1000_absolute" else "KG50"
            channel_name = {"KG1000":"ch1", "KG50":"kg50", "IADC":"iadc"}[sample_sensor]
            if channel_name == "kg50":
                channel = cfg.get("extra_channels", {}).get("kg50", {"linear":{"m":1,"b":0}})
            else:
                channel = {"linear":cfg[channel_name], "fit":cfg.get(channel_name+"_fit"),
                           "zero_raw":cfg.get(channel_name+"_zero_raw"),
                           "points":cfg.get("points_"+channel_name, [])}
            signature = json.dumps([recording, stamp, channel], sort_keys=True)
            entry = epochs.setdefault(signature, {"recording":recording,"first_sample_ms":t,"last_sample_ms":t,
                "channel":channel,"equation":"y = f(raw) - f(zero_raw) if tare exists, else f(raw); linear: f(x)=m*x+b; polyN: descending a..e coefficients evaluated at (x-x0)",
                "samples":0})
            entry["last_sample_ms"] = t
            entry["samples"] += 1
            if kind == "LOADCELL_FILL_PERCENT":
                entry["fill_targets"] = snapshot.get("fill_targets")
                entry["flight_state"] = snapshot.get("flight_state")
                entry["fill_source"] = sample_sensor
                entry["postprocess"] = "clamp(100 * " + ("abs(calibrated_kg)" if sample_sensor == "KG1000" else "calibrated_kg") + " / active_target_kg, 0, 100)"
        if epochs:
            return {"status":"recorded_calibration" if missing==0 else "partially_recorded_calibration",
                    "epochs":list(epochs.values()), "samples_without_metadata":missing}
    if kind not in RAW_TYPES:
        if "CALIBRAT" in kind or kind == "LOADCELL_FILL_PERCENT":
            return {"status": "unavailable", "reason": "No historical regression recorded for this derived channel."}
        return {"status": "not_applicable"}
    raw = streams.get(json.dumps([sender, RAW_TYPES[kind], index], separators=(",", ":")), [])
    lookup = {(t, src, recording): value for t, value, src, recording, _ in raw}
    pairs = [(lookup[(t, src, rec)], value) for t, value, src, rec, _ in samples
             if value is not None and lookup.get((t, src, rec)) is not None]
    if len(pairs) < 3:
        return {"status": "unavailable", "reason": "Insufficient matched raw/calibrated samples; current calibration is not substituted."}
    mx = sum(x for x, _ in pairs) / len(pairs)
    my = sum(y for _, y in pairs) / len(pairs)
    xx = sum((x-mx)**2 for x, _ in pairs)
    if xx < 1e-24:
        return {"status": "unavailable", "reason": "Raw samples have insufficient variation."}
    slope = sum((x-mx)*(y-my) for x, y in pairs) / xx
    intercept = my-slope*mx
    residual = max(abs(y-(slope*x+intercept)) for x, y in pairs)
    tolerance = max(1e-5, max(abs(y) for _, y in pairs)*2e-6)
    if residual > tolerance:
        return {"status": "unavailable", "reason": "Samples do not support one linear regression (possibly calibration changes or a nonlinear fit). Select a narrower interval."}
    return {"status": "inferred_from_recorded_pairs", "equation": "y = slope * x + intercept",
            "raw_type": RAW_TYPES[kind], "slope": slope, "intercept": intercept,
            "pair_count": len(pairs), "max_residual": residual,
            "note": "Reconstructed mapping, not a saved original calibration."}


def preview_points(samples, limit=600):
    # Keep extrema rather than averaging away brief pressure/loadcell peaks.
    width = max(1, math.ceil(len(samples)/(limit/5)))
    points = []
    for offset in range(0, len(samples), width):
        group = samples[offset:offset+width]
        valid = [(i, s) for i, s in enumerate(group) if s[1] is not None]
        indexes = {0, len(group)-1}
        missing = next((i for i,s in enumerate(group) if s[1] is None), None)
        if missing is not None:
            indexes.add(missing)
        if valid:
            indexes.add(min(valid, key=lambda p:p[1][1])[0])
            indexes.add(max(valid, key=lambda p:p[1][1])[0])
        points.extend([[group[i][0], group[i][1]] for i in sorted(indexes)])
    return points


def selected_streams(request, streams):
    selected = request.get("channels")
    if selected is None:
        return streams
    if not isinstance(selected, list) or not selected:
        raise ValueError("Select at least one channel, or choose all data.")
    unknown = set(selected)-set(streams)
    if unknown:
        raise ValueError("A selected channel has no data in this interval. Refresh the preview or change the selection.")
    return {key: streams[key] for key in selected}


VALVE_CHANNELS = ["Pilot valve", "Normally-open valve", "Dump valve", "Igniter", "Nitrogen valve", "Nitrous valve", "Plumbing"]

def channel_label(key):
    sender, kind, index = json.loads(key)
    if kind == "VALVE_STATE" and 0 <= index < len(VALVE_CHANNELS):
        return VALVE_CHANNELS[index]
    return f"{sender} / {kind} / {index}"

def state_label(key, value, mapping=None):
    _, kind, index = json.loads(key)
    if kind != "VALVE_STATE" or index >= len(VALVE_CHANNELS):
        return ""
    if value not in (0, 1):
        return (mapping or {}).get("unknown_label") or "Unknown"
    labels = ("Off", "On") if index == 3 else (("Extended", "Retracted") if index == 6 else ("Closed", "Open"))
    return (mapping or {}).get("true_label" if value else "false_label") or labels[int(value)]


def export_csv(streams, regressions, provenance=None):
    if sum(len(json.dumps(regressions[key]))*len(samples) for key,samples in streams.items()) > 48_000_000:
        raise ValueError("Regression metadata makes this CSV too large. Narrow the selection or use Excel, which stores regressions separately.")
    out = io.StringIO(newline="")
    writer = csv.writer(out)
    writer.writerow(["recording", "row_id", "received_timestamp_ms", "source_timestamp_ms",
                     "sender", "data_type", "channel", "value", "regression_json", "channel_label", "state_label"])
    for key, samples in streams.items():
        sender, kind, index = json.loads(key)
        metadata = json.dumps(regressions[key], separators=(",", ":"))
        for t, value, source, recording, row_id in samples:
            # Spreadsheet formula protection for untrusted textual identities.
            safe = lambda s: "'"+s if s.startswith(("=", "+", "-", "@")) else s
            origin=(provenance or {}).get((recording,row_id))
            received=origin.get("received_timestamp_ms") if origin else t
            sample_metadata=metadata
            if origin:
                # Do not leak unselected raw/vector values in provenance.
                clean_origin={k:v for k,v in origin.items() if k!="fields"}
                sample_metadata=json.dumps({**regressions[key],"sample_provenance":clean_origin},separators=(",",":"))
            writer.writerow([recording, row_id, received, source, safe(sender), safe(kind), index, value, sample_metadata, safe(regressions[key].get("channel_label",channel_label(key))), safe(state_label(key,value,regressions[key].get("boolean_labels")))])
    return out.getvalue().encode()


# Exact firmware column order from DAQ-Board/Core/Inc/sd_record_format.h.
DAQ_COLUMNS = ("timestamp_ms", "monotonic_ms", "sensor", "value", "raw_adc_code",
               "raw_value", "calibrated_value", "time_source", "adc_temperature_c",
               "adc_temperature_code", "uptime_ticks", "uptime_frequency_hz", "record_sequence")


def daq_float(value):
    """Firmware's nine-significant-digit binary32 representation, without
    quantizing a higher-precision recording value down to binary32."""
    if isinstance(value, int):
        return str(value)
    try:
        binary32 = struct.unpack("<f", struct.pack("<f", value))[0]
    except (OverflowError, struct.error):
        return repr(value)
    if binary32 != value:
        return repr(value)
    sign = "-" if math.copysign(1.0, value) < 0 else ""
    if value == 0:
        return sign + "0"
    normalized, exponent = abs(value), 0
    while normalized >= 10:
        normalized /= 10
        exponent += 1
    while normalized < 1:
        normalized *= 10
        exponent -= 1
    digits = int(normalized * 100000000 + 0.5)
    if digits == 1000000000:
        digits, exponent = 100000000, exponent + 1
    return f"{sign}{digits // 100000000}.{digits % 100000000:08d}e{'-' if exponent < 0 else '+'}{abs(exponent):02d}"


def daq_cell(value, delimiter=",", force_quote=False):
    text = "" if value is None else str(value)
    if force_quote or any(c in text for c in (delimiter, '"', "\r", "\n")):
        return '"' + text.replace('"', '""') + '"'
    return text


def daq_record(fields, delimiter=","):
    # Value is always quoted, as in sd_format_csv_record. Imported original
    # raw/value strings remain strings; no new calibration or ADC decoding.
    raw_adc = fields.get("raw_adc_code") not in (None, "")
    return delimiter.join(daq_cell(fields.get(name), delimiter,
                          (name == "value" and not raw_adc) or
                          (name in ("raw_value", "calibrated_value") and not raw_adc and fields.get(name) not in (None, "")))
                          for name in DAQ_COLUMNS) + "\r\n"


def data_kind(key):
    _, kind, _ = json.loads(key)
    if kind in RAW_TYPES or kind == "LOADCELL_FILL_PERCENT":
        return "recorded_derived"
    if kind in set(RAW_TYPES.values()) | {"IADC", "COMBUSTION_CHAMBER_VOLTAGE"}:
        return "recorded_raw_input"
    if kind == "VALVE_STATE":
        return "recorded_state"
    return "recorded_unclassified"


def recorded_diagnostic_info(key, request, samples):
    sender, kind, index = json.loads(key)
    if not kind.startswith("REC_") or not samples:
        return None
    versions = request.get("_recorded_diagnostic_versions", {})
    if not all(versions.get(json.dumps([rec, row_id])) == 1 for _, _, _, rec, row_id in samples):
        return None
    schema = request.get("recording_diagnostic_streams", {})
    if schema.get("version") != 1:
        return None
    stream = next((s for s in schema.get("streams", []) if s.get("data_type") == kind), None)
    if not stream or not 0 <= index < len(stream.get("channels", [])):
        return None
    units = stream.get("units", [])
    unit = units[index] if index < len(units) else stream.get("unit")
    return {"channel_label": stream["channels"][index], "unit": unit,
            "unit_source": "recorded_diagnostic_schema_v1", "precision": "counter CSV integers exact; graph projections approximate"}


def channel_info(key, request, provenance=None, samples=None):
    sender, kind, index = json.loads(key)
    entries = [entry for entry in request.get("channel_metadata", [])
               if entry.get("data_type") == kind and entry.get("index") == index
               and entry.get("sender_id") in (None, sender)]
    # Conflicting export-time configuration is unknown, never a guessed unit.
    units = {entry.get("unit") for entry in entries if entry.get("unit")}
    recorded_units=set()
    for _,_,_,recording,row_id in samples or []:
        origin=(provenance or {}).get((recording,row_id),{})
        if origin.get("origin")=="sd" and isinstance(origin.get("mapping",{}).get("units"),str):
            recorded_units.add(origin["mapping"]["units"])
    chosen_units=recorded_units or units
    diagnostic = recorded_diagnostic_info(key, request, samples)
    return {"sender": sender, "data_type": kind, "index": index,
            "unit": next(iter(chosen_units)) if len(chosen_units) == 1 else None,
            "unit_source": ("recorded_sd_manifest" if recorded_units else "export_station_layout") if len(chosen_units) == 1 else "unknown",
            "value_kind": data_kind(key), **(diagnostic or {})}


def export_daq(streams, metadata, request, delimiter=",", provenance=None, all_streams=None):
    out = [delimiter.join(DAQ_COLUMNS) + "\r\n"]
    out.append("# export=" + json.dumps({"layout": "seds-sd-csv-13-v1",
        "origin": "groundstation_recording", "clock_policy": "source UTC only when calendar-valid; otherwise unknown",
        "unavailable_fields": ["monotonic_ms", "raw_adc_code", "raw_value", "calibrated_value", "adc_temperature_c",
                               "adc_temperature_code", "uptime_ticks", "uptime_frequency_hz", "record_sequence"],
        "precision": "firmware nine-digit binary32; higher recorded precision retained",
        "missing_values": "omitted; counts retained in channel metadata"}, separators=(",", ":")) + "\r\n")
    selected = []
    for key, samples in streams.items():
        info = {**channel_info(key, request, provenance, samples), "key": key, "samples": len(samples),
                "missing_samples": sum(value is None for _, value, *_ in samples),
                "regression": metadata[key]}
        out.append("# channel=" + json.dumps(info, separators=(",", ":")) + "\r\n")
        for received, value, source, recording, row_id in samples:
            if value is not None:
                selected.append((received, recording, row_id, key, value, source))
    # One selected saved channel sample produces exactly one row, even if
    # unrelated sources share a numeric value or timestamp. No interpolation.
    seen_sd=set()
    physical_channels=defaultdict(set)
    if provenance:
        for candidate,samples in (all_streams or streams).items():
            for _,_,_,rec,rid in samples:
                original=provenance.get((rec,rid),{})
                if original.get("origin")=="sd":
                    physical_channels[(rec,original.get("sd_sample_id"))].add(candidate)
    for received, recording, row_id, key, value, source in sorted(selected):
        origin=(provenance or {}).get((recording,row_id))
        if origin and origin.get("origin")=="sd":
            sample_id=origin.get("sd_sample_id")
            if sample_id is None or not isinstance(origin.get("fields"),dict):
                raise ValueError("Resolved SD recording lacks its original thirteen fields; no substitute was produced.")
            identity=(recording,sample_id)
            required=physical_channels[identity]
            if not required.issubset(streams):
                raise ValueError("DAQ SD layout preserves whole physical SD records. Select all channels mapped from the chosen SD record, or use selected-channel CSV.")
            if identity not in seen_sd:
                seen_sd.add(identity)
                out.append(export_daq_sd_rows([origin],delimiter).decode().split("\r\n",1)[1])
            continue
        sender, kind, index = json.loads(key)
        utc = source if isinstance(source, int) and 315532800000 <= source < 4354819200000 else None
        sensor = "kg1000_network" if (sender, kind, index) == ("DAQ", "KG1000", 0) else key
        labels = metadata[key].get("boolean_labels")
        provenance = {"recording": recording, "row_id": row_id, "channel": key,
                      "received_timestamp_ms": received, "source_timestamp_ms": source,
                      "source_age_ms": received-source if utc is not None and 315532800000 <= received < 4354819200000 else None,
                      "state_label": state_label(key, value, labels)}
        out.append("# recorded_sample=" + json.dumps(provenance, separators=(",", ":")) + "\r\n")
        out.append(daq_record({"timestamp_ms": utc, "sensor": sensor, "value": daq_float(value),
                               "time_source": "network" if utc is not None else "unknown"}, delimiter))
    return "".join(out).encode()


def export_daq_sd_rows(samples, delimiter=","):
    """Adapter contract with recording_sd_import.iter_sd_rows: preserve actual
    thirteen original cells, including raw nan/vectors/clock labels, verbatim.
    SD/telemetry overlap resolution belongs to that provenance adapter."""
    out = [delimiter.join(DAQ_COLUMNS) + "\r\n"]
    for sample in samples:
        fields = sample.get("fields")
        if not isinstance(fields, dict) or set(fields) != set(DAQ_COLUMNS):
            raise ValueError("Imported SD sample does not contain the actual thirteen-column schema.")
        provenance = {key: value for key, value in sample.items() if key != "fields"}
        out.append("# sd_sample=" + json.dumps(provenance, separators=(",", ":")) + "\r\n")
        out.append(daq_record(fields, delimiter))
    return "".join(out).encode()


def export_xlsx(streams, regressions, provenance=None):
    from openpyxl import Workbook
    from openpyxl.chart import ScatterChart, Reference, Series
    book = Workbook()
    meta = book.active
    meta.title = "Regression metadata"
    meta.append(["Channel", "Regression / provenance"])
    for number, (key, samples) in enumerate(streams.items(), 1):
        metadata = json.dumps(regressions[key])
        for offset in range(0, len(metadata), 30000):
            meta.append([key, metadata[offset:offset+30000]])
            meta.cell(meta.max_row, 1).data_type = "s"
        sheet = book.create_sheet(f"Channel {number}")
        has_sd=any((provenance or {}).get((rec,rid),{}).get("origin")=="sd" for _,_,_,rec,rid in samples)
        sheet.append((["Timeline ms", "Value", "Source ms", "Recording", "Row ID", "Channel", "Received ms"] if has_sd else ["Received ms", "Value", "Source ms", "Recording", "Row ID", "Channel"]))
        for row in samples:
            t, value, source, recording, row_id = row
            row=[t,value,source,recording,row_id,key]
            if has_sd:
                origin=(provenance or {}).get((recording,row_id))
                row.append(origin.get("received_timestamp_ms") if origin else t)
            sheet.append(row)
            sheet.cell(sheet.max_row, 6).data_type = "s"
        chart = ScatterChart()
        chart.title = key
        chart.x_axis.title = "Canonical replay timeline (ms UTC)" if has_sd else "Received timestamp (ms UTC)"
        chart.y_axis.title = "Recorded value"
        chart.series.append(Series(Reference(sheet, min_col=2, min_row=2, max_row=sheet.max_row),
                                   Reference(sheet, min_col=1, min_row=2, max_row=sheet.max_row)))
        chart.width, chart.height = 25, 12
        sheet.add_chart(chart, "H2")
        sheet.freeze_panes = "A2"
    out = io.BytesIO()
    book.save(out)
    book.close()
    return out.getvalue()


def export_pdf(streams, regressions, provenance=None):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.backends.backend_pdf import PdfPages
    import textwrap
    # A raw-data PDF can be enormous. Reject instead of truncating the appendix.
    if sum(len(v) for v in streams.values()) > 20_000:
        raise ValueError("PDF raw-data appendix exceeds 20,000 samples. Narrow the selection or use CSV/Excel.")
    out = io.BytesIO()
    with PdfPages(out) as pdf:
        for key, samples in streams.items():
            fig, ax = plt.subplots(figsize=(11.7, 8.3))
            start = samples[0][0]
            ax.plot([(s[0]-start)/1000 for s in samples], [s[1] for s in samples], linewidth=.7)
            has_sd=any((provenance or {}).get((rec,rid),{}).get("origin")=="sd" for _,_,_,rec,rid in samples)
            ax.set(title=key, xlabel=f"Seconds after {'canonical replay' if has_sd else 'receive'} UTC epoch {start} ms", ylabel="Recorded value")
            ax.grid(True, alpha=.3)
            pdf.savefig(fig)
            plt.close(fig)
            lines = textwrap.wrap(json.dumps(regressions[key]), 110)
            for offset in range(0,len(lines),48):
                fig, ax = plt.subplots(figsize=(11.7,8.3))
                ax.axis("off")
                ax.set_title(key+" — regression / provenance",fontsize=9)
                ax.text(0,1,"\n".join(lines[offset:offset+48]),va="top",fontsize=8, family="monospace")
                pdf.savefig(fig)
                plt.close(fig)
            for offset in range(0, len(samples), 35):
                fig, ax = plt.subplots(figsize=(11.7, 8.3))
                ax.axis("off")
                ax.set_title(f"{key} — recorded data {offset+1}–{min(offset+35,len(samples))}", fontsize=9)
                cells = [[str(v) if v is not None else "" for v in row] for row in samples[offset:offset+35]]
                table = ax.table(cellText=cells, colLabels=["Timeline ms" if has_sd else "Received ms", "Value", "Source ms", "Recording", "Row ID"], loc="center")
                table.auto_set_font_size(False)
                table.set_fontsize(5)
                table.scale(1, 1.35)
                pdf.savefig(fig)
                plt.close(fig)
    return out.getvalue()


def run(request):
    rows, history, provenance = read_rows(request)
    request = dict(request)
    versions = {}
    for rec, row_id, received, source, sender, kind, values, payload in rows:
        if kind.startswith("REC_"):
            try:
                decoded = json.loads(payload or "null")
                context = decoded.get("context", decoded)
                versions[json.dumps([rec, row_id])] = context.get("version")
            except (TypeError, ValueError, AttributeError):
                pass
    request["_recorded_diagnostic_versions"] = versions
    all_streams = channels(rows)
    streams = selected_streams(request, all_streams)
    metadata = {key: regression(key, samples, all_streams, history) for key, samples in streams.items()}
    for key, info in metadata.items():
        _,kind,index=json.loads(key)
        info.update(recorded_diagnostic_info(key, request, streams[key]) or {})
        if kind=="VALVE_STATE":
            mapping=next((m for m in request.get("valve_mappings",[]) if isinstance(m,dict) and m.get("index")==index),{})
            info["channel_label"]=mapping.get("label") or channel_label(key)
            info["boolean_labels"]={name:mapping[name] for name in ("true_label","false_label","unknown_label") if isinstance(mapping.get(name),str)}
    fmt = request.get("format", "preview")
    if fmt == "preview":
        return json.dumps({"start_ms": rows[0][2], "end_ms": rows[-1][2]+1,
            "recorded_rows": len(rows), "channels": [{"key": key, "label": metadata[key].get("channel_label",channel_label(key)), "count": len(samples),
                "points": preview_points(samples), "regression": metadata[key],
                "finite_count": sum(value is not None for _, value, *_ in samples),
                **channel_info(key, request, provenance, samples)}
                for key, samples in streams.items()]}).encode()
    if fmt == "csv":
        return export_csv(streams, metadata, provenance)
    if fmt in ("daq_csv", "daq_tsv"):
        if not request.get("recording"):
            raise ValueError("Select one recording before choosing DAQ SD layout.")
        return export_daq(streams, metadata, request, chr(9) if fmt == "daq_tsv" else ",", provenance, all_streams)
    if fmt == "xlsx":
        return export_xlsx(streams, metadata, provenance)
    if fmt == "pdf":
        return export_pdf(streams, metadata, provenance)
    raise ValueError("Choose CSV, DAQ SD CSV/TSV, Excel or PDF.")


if __name__ == "__main__":
    try:
        sys.stdout.buffer.write(run(json.load(sys.stdin)))
    except ImportError as exc:
        sys.stderr.write(f"Report dependency unavailable: {exc}. Install openpyxl and matplotlib in the backend report Python environment; CSV/preview remain available.")
        sys.exit(2)
    except Exception as exc:
        sys.stderr.write(str(exc))
        sys.exit(1)
