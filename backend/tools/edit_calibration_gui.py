#!/usr/bin/env python3
from __future__ import annotations

import argparse
import copy
import json
import math
import os
import sys
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, messagebox, ttk
from typing import Any

DEFAULT_FULL_MASS_KG = 10.0
PLOT_WIDTH = 700
PLOT_HEIGHT = 260
PLOT_PAD_LEFT = 56
PLOT_PAD_RIGHT = 20
PLOT_PAD_TOP = 16
PLOT_PAD_BOTTOM = 30

CHANNEL_SPECS = {
    "ch1": {
        "label": "1000kg",
        "points_key": "points_ch1",
        "raw_key": "ch1_raw",
        "linear_key": "ch1",
        "zero_key": "ch1_zero_raw",
        "fit_key": "ch1_fit",
        "color": "#22d3ee",
    },
    "iadc": {
        "label": "Tank Pressure",
        "points_key": "points_iadc",
        "raw_key": "iadc_raw",
        "linear_key": "iadc",
        "zero_key": "iadc_zero_raw",
        "fit_key": "iadc_fit",
        "color": "#a78bfa",
    },
}


def _default_calibration_path() -> Path:
    env = os.environ.get("GS_LOADCELL_CALIBRATION_PATH", "").strip()
    if env:
        return Path(env)
    backend_dir = Path(__file__).resolve().parents[1]
    candidates = [
        backend_dir / "data" / "loadcell_calibration.json",
        backend_dir / "calibration" / "loadcell_calibration.json",
        backend_dir / "calibration" / "loadcell_calibration_testing.json",
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    return candidates[0]


def _read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _write_json(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def _get_float(text: str) -> float | None:
    s = text.strip()
    if not s:
        return None
    return float(s)


def _fmt_float(value: float | None, precision: int = 6) -> str:
    if value is None:
        return ""
    return f"{value:.{precision}f}".rstrip("0").rstrip(".")


def _default_channel_linear() -> dict[str, float]:
    return {"m": 1.0, "b": 0.0}


def _normalize_data(data: dict[str, Any]) -> dict[str, Any]:
    normalized = copy.deepcopy(data)
    normalized.setdefault("version", 1)
    normalized.setdefault("full_mass_kg", DEFAULT_FULL_MASS_KG)
    normalized.setdefault("weights_kg", [])
    for channel_name, spec in CHANNEL_SPECS.items():
        linear = normalized.setdefault(spec["linear_key"], {})
        if not isinstance(linear, dict):
            linear = {}
            normalized[spec["linear_key"]] = linear
        linear.setdefault("m", 1.0)
        linear.setdefault("b", 0.0)
        normalized.setdefault(spec["zero_key"], None)
        normalized.setdefault(spec["fit_key"], None)
        points = normalized.setdefault(spec["points_key"], [])
        if not isinstance(points, list):
            normalized[spec["points_key"]] = []
    return normalized


def _points_for_channel(data: dict[str, Any], channel_name: str) -> list[tuple[float, float]]:
    spec = CHANNEL_SPECS[channel_name]
    points = []
    for point in data.get(spec["points_key"], []):
        if not isinstance(point, dict):
            continue
        expected = point.get("kg")
        if channel_name == "iadc":
            expected = point.get("expected")
        raw = point.get(spec["raw_key"])
        if expected is None or raw is None:
            continue
        points.append((float(raw), float(expected)))
    points.sort(key=lambda item: item[0])
    return points


def _write_points_for_channel(
        data: dict[str, Any], channel_name: str, points: list[tuple[float, float]]
) -> None:
    spec = CHANNEL_SPECS[channel_name]
    out = []
    for raw, expected in points:
        if channel_name == "iadc":
            out.append({"expected": float(expected), spec["raw_key"]: float(raw)})
        else:
            out.append({"kg": float(expected), spec["raw_key"]: float(raw)})
    data[spec["points_key"]] = out


def _fit_meta_for_channel(data: dict[str, Any], channel_name: str) -> dict[str, Any] | None:
    spec = CHANNEL_SPECS[channel_name]
    fit = data.get(spec["fit_key"])
    return fit if isinstance(fit, dict) else None


def _linear_for_channel(data: dict[str, Any], channel_name: str) -> dict[str, Any]:
    spec = CHANNEL_SPECS[channel_name]
    linear = data.get(spec["linear_key"])
    if not isinstance(linear, dict):
        linear = _default_channel_linear()
        data[spec["linear_key"]] = linear
    return linear


def _eval_fit(data: dict[str, Any], channel_name: str, raw: float) -> float | None:
    linear = _linear_for_channel(data, channel_name)
    fit = _fit_meta_for_channel(data, channel_name) or {}
    fit_type = fit.get("type")
    x0 = fit.get("x0") or 0.0

    if fit_type == "poly3":
        a = fit.get("a")
        b = fit.get("b")
        c = fit.get("c")
        d = fit.get("d")
        if None not in (a, b, c, d):
            x = raw - x0
            return float(a) * x ** 3 + float(b) * x ** 2 + float(c) * x + float(d)

    if fit_type == "poly2":
        a = fit.get("a")
        b = fit.get("b")
        c = fit.get("c")
        if None not in (a, b, c):
            x = raw - x0
            return float(a) * x ** 2 + float(b) * x + float(c)

    m = linear.get("m")
    if m is None:
        return None
    b = linear.get("b") or 0.0
    return float(m) * raw + float(b)


def _fit_summary(data: dict[str, Any], channel_name: str) -> str:
    linear = _linear_for_channel(data, channel_name)
    fit = _fit_meta_for_channel(data, channel_name) or {}
    fit_type = fit.get("type") or "linear"
    x0 = fit.get("x0")

    if fit_type == "poly3":
        return (
            f"type=poly3  a={_fmt_float(fit.get('a'), 4)}  b={_fmt_float(fit.get('b'), 4)}  "
            f"c={_fmt_float(fit.get('c'), 4)}  d={_fmt_float(fit.get('d'), 4)}  x0={_fmt_float(x0, 4)}"
        )
    if fit_type == "poly2":
        return (
            f"type=poly2  a={_fmt_float(fit.get('a'), 4)}  b={_fmt_float(fit.get('b'), 4)}  "
            f"c={_fmt_float(fit.get('c'), 4)}  x0={_fmt_float(x0, 4)}"
        )
    return (
        f"type={fit_type}  m={_fmt_float(linear.get('m'), 4)}  "
        f"b={_fmt_float(linear.get('b'), 4)}  x0={_fmt_float(x0, 4)}"
    )


class PointDialog(tk.Toplevel):
    def __init__(
            self,
            parent: tk.Misc,
            *,
            title: str,
            expected_label: str,
            expected_value: float = 0.0,
            raw_value: float = 0.0,
    ) -> None:
        super().__init__(parent)
        self.title(title)
        self.transient(parent)
        self.grab_set()
        self.result: tuple[float, float] | None = None

        frame = ttk.Frame(self, padding=12)
        frame.pack(fill="both", expand=True)

        ttk.Label(frame, text=expected_label).grid(row=0, column=0, sticky="w", padx=(0, 8), pady=(0, 8))
        self.expected_var = tk.StringVar(value=_fmt_float(expected_value, 4))
        ttk.Entry(frame, textvariable=self.expected_var, width=18).grid(row=0, column=1, sticky="ew", pady=(0, 8))

        ttk.Label(frame, text="Raw value").grid(row=1, column=0, sticky="w", padx=(0, 8))
        self.raw_var = tk.StringVar(value=_fmt_float(raw_value, 6))
        ttk.Entry(frame, textvariable=self.raw_var, width=18).grid(row=1, column=1, sticky="ew")

        frame.columnconfigure(1, weight=1)

        buttons = ttk.Frame(frame)
        buttons.grid(row=2, column=0, columnspan=2, sticky="e", pady=(12, 0))
        ttk.Button(buttons, text="Cancel", command=self.destroy).pack(side="right")
        ttk.Button(buttons, text="OK", command=self._accept).pack(side="right", padx=(0, 8))

    def _accept(self) -> None:
        try:
            expected = _get_float(self.expected_var.get())
            raw = _get_float(self.raw_var.get())
        except ValueError as exc:
            messagebox.showerror("Invalid value", str(exc), parent=self)
            return
        if expected is None or raw is None:
            messagebox.showerror("Invalid value", "Both values are required.", parent=self)
            return
        self.result = (expected, raw)
        self.destroy()


class ChannelPane(ttk.Frame):
    def __init__(self, master: tk.Misc, editor: "CalibrationEditor", channel_name: str) -> None:
        super().__init__(master, padding=10)
        self.editor = editor
        self.channel_name = channel_name
        self.spec = CHANNEL_SPECS[channel_name]

        self.fit_var = tk.StringVar(value="type=linear")
        ttk.Label(self, textvariable=self.fit_var).pack(anchor="w", pady=(0, 8))

        self.canvas = tk.Canvas(
            self,
            width=PLOT_WIDTH,
            height=PLOT_HEIGHT,
            background="#020617",
            highlightthickness=1,
            highlightbackground="#334155",
        )
        self.canvas.pack(fill="x", expand=False)

        body = ttk.Frame(self)
        body.pack(fill="both", expand=True, pady=(10, 0))

        left = ttk.Frame(body)
        left.pack(side="left", fill="both", expand=True)
        right = ttk.Frame(body)
        right.pack(side="left", fill="y", padx=(12, 0))

        self.points_list = tk.Listbox(left, height=10, exportselection=False)
        self.points_list.pack(fill="both", expand=True)

        ttk.Button(right, text="Add", command=self.add_point).pack(fill="x")
        ttk.Button(right, text="Edit", command=self.edit_point).pack(fill="x", pady=(6, 0))
        ttk.Button(right, text="Remove", command=self.remove_point).pack(fill="x", pady=(6, 0))
        ttk.Button(right, text="Reset", command=self.reset_points).pack(fill="x", pady=(6, 0))

    def expected_label(self) -> str:
        return "Expected value" if self.channel_name == "iadc" else "Expected kg"

    def refresh(self) -> None:
        points = _points_for_channel(self.editor.data, self.channel_name)
        self.points_list.delete(0, "end")
        if points:
            for raw, expected in points:
                self.points_list.insert("end", f"{expected:g} -> raw {raw:.6g}")
        else:
            self.points_list.insert("end", "(no points)")
        self.fit_var.set(_fit_summary(self.editor.data, self.channel_name))
        self._draw_plot(points)

    def selected_index(self) -> int | None:
        selection = self.points_list.curselection()
        if not selection:
            return None
        index = int(selection[0])
        if not _points_for_channel(self.editor.data, self.channel_name):
            return None
        return index

    def add_point(self) -> None:
        dialog = PointDialog(
            self,
            title=f"Add {self.spec['label']} point",
            expected_label=self.expected_label(),
        )
        self.wait_window(dialog)
        if dialog.result is None:
            return
        expected, raw = dialog.result
        points = _points_for_channel(self.editor.data, self.channel_name)
        points.append((raw, expected))
        points.sort(key=lambda item: item[0])
        _write_points_for_channel(self.editor.data, self.channel_name, points)
        self.editor.set_status(f"Added {self.spec['label']} point")
        self.editor.refresh_all()

    def edit_point(self) -> None:
        index = self.selected_index()
        points = _points_for_channel(self.editor.data, self.channel_name)
        if index is None or index >= len(points):
            self.editor.set_status("Select a point first")
            return
        raw, expected = points[index]
        dialog = PointDialog(
            self,
            title=f"Edit {self.spec['label']} point",
            expected_label=self.expected_label(),
            expected_value=expected,
            raw_value=raw,
        )
        self.wait_window(dialog)
        if dialog.result is None:
            return
        new_expected, new_raw = dialog.result
        points[index] = (new_raw, new_expected)
        points.sort(key=lambda item: item[0])
        _write_points_for_channel(self.editor.data, self.channel_name, points)
        self.editor.set_status(f"Updated {self.spec['label']} point")
        self.editor.refresh_all()

    def remove_point(self) -> None:
        index = self.selected_index()
        points = _points_for_channel(self.editor.data, self.channel_name)
        if index is None or index >= len(points):
            self.editor.set_status("Select a point first")
            return
        del points[index]
        _write_points_for_channel(self.editor.data, self.channel_name, points)
        self.editor.set_status(f"Removed {self.spec['label']} point")
        self.editor.refresh_all()

    def reset_points(self) -> None:
        if not messagebox.askyesno(
                "Reset points",
                f"Reset all {self.spec['label']} points?",
                parent=self,
        ):
            return
        _write_points_for_channel(self.editor.data, self.channel_name, [])
        self.editor.set_status(f"Reset {self.spec['label']} points")
        self.editor.refresh_all()

    def _draw_plot(self, points: list[tuple[float, float]]) -> None:
        self.canvas.delete("all")
        w = PLOT_WIDTH
        h = PLOT_HEIGHT
        left = PLOT_PAD_LEFT
        right = w - PLOT_PAD_RIGHT
        top = PLOT_PAD_TOP
        bottom = h - PLOT_PAD_BOTTOM
        color = self.spec["color"]

        self.canvas.create_line(left, top, left, bottom, fill="#334155")
        self.canvas.create_line(left, bottom, right, bottom, fill="#334155")

        if points:
            x_values = [raw for raw, _ in points]
            y_values = [expected for _, expected in points]
        else:
            x_values = [0.0, 1.0]
            y_values = [0.0, 1.0]

        x_min = min(x_values)
        x_max = max(x_values)
        y_min = min(y_values)
        y_max = max(y_values)
        if abs(x_max - x_min) < 1e-9:
            x_min -= 1.0
            x_max += 1.0
        if abs(y_max - y_min) < 1e-9:
            y_min -= 1.0
            y_max += 1.0
        x_pad = (x_max - x_min) * 0.1
        y_pad = (y_max - y_min) * 0.15
        x_min -= x_pad
        x_max += x_pad
        y_min -= y_pad
        y_max += y_pad

        def sx(raw: float) -> float:
            return left + (raw - x_min) / (x_max - x_min) * (right - left)

        def sy(expected: float) -> float:
            return top + (1.0 - (expected - y_min) / (y_max - y_min)) * (bottom - top)

        fit_points = []
        for step in range(80):
            raw = x_min + (x_max - x_min) * step / 79.0
            expected = _eval_fit(self.editor.data, self.channel_name, raw)
            if expected is None or math.isnan(expected) or math.isinf(expected):
                continue
            fit_points.extend((sx(raw), sy(expected)))
        if len(fit_points) >= 4:
            self.canvas.create_line(*fit_points, fill=color, width=2.5, smooth=True)

        for raw, expected in points:
            x = sx(raw)
            y = sy(expected)
            self.canvas.create_oval(x - 4, y - 4, x + 4, y + 4, fill="#f8fafc", outline=color, width=1.5)

        self.canvas.create_text(8, 8, anchor="nw", fill="#94a3b8", text=f"y max {y_max:.3f}", font=("TkDefaultFont", 9))
        self.canvas.create_text(8, bottom - 2, anchor="sw", fill="#94a3b8", text=f"y min {y_min:.3f}",
                                font=("TkDefaultFont", 9))
        self.canvas.create_text(left, h - 8, anchor="sw", fill="#94a3b8", text=f"x min {x_min:.3f}",
                                font=("TkDefaultFont", 9))
        self.canvas.create_text(right, h - 8, anchor="se", fill="#94a3b8", text=f"x max {x_max:.3f}",
                                font=("TkDefaultFont", 9))


class CalibrationEditor(tk.Tk):
    def __init__(self, initial_file: str | None = None) -> None:
        super().__init__()
        self.title("Load Cell Calibration Editor")
        self.geometry("1250x900")
        self.minsize(1100, 760)

        self.path_var = tk.StringVar(value=initial_file or str(_default_calibration_path()))
        self.status_var = tk.StringVar(value="Ready.")
        self.backup_var = tk.BooleanVar(value=True)
        self.data: dict[str, Any] = _normalize_data({})

        self.full_mass_var = tk.StringVar()
        self.channel_vars: dict[str, dict[str, tk.StringVar]] = {}
        self.channel_fit_vars: dict[str, tk.StringVar] = {}
        self.channel_panes: dict[str, ChannelPane] = {}

        self._build_ui()
        self.load_file(Path(self.path_var.get()))

    def _build_ui(self) -> None:
        root = ttk.Frame(self, padding=12)
        root.pack(fill="both", expand=True)

        file_row = ttk.Frame(root)
        file_row.pack(fill="x")
        ttk.Label(file_row, text="Calibration file").pack(side="left")
        ttk.Entry(file_row, textvariable=self.path_var).pack(side="left", fill="x", expand=True, padx=8)
        ttk.Button(file_row, text="Browse", command=self.browse_file).pack(side="left")
        ttk.Button(file_row, text="Load", command=self.load_from_ui).pack(side="left", padx=(8, 0))
        ttk.Button(file_row, text="Save", command=self.save_file).pack(side="left", padx=(8, 0))

        main = ttk.Panedwindow(root, orient="horizontal")
        main.pack(fill="both", expand=True, pady=(12, 0))

        left = ttk.Frame(main, padding=10)
        right = ttk.Frame(main, padding=10)
        main.add(left, weight=1)
        main.add(right, weight=3)

        general = ttk.LabelFrame(left, text="General", padding=10)
        general.pack(fill="x")
        ttk.Label(general, text="Full mass kg").grid(row=0, column=0, sticky="w")
        ttk.Entry(general, textvariable=self.full_mass_var).grid(row=0, column=1, sticky="ew", padx=(8, 0))
        general.columnconfigure(1, weight=1)

        for row_idx, (channel_name, spec) in enumerate(CHANNEL_SPECS.items(), start=1):
            section = ttk.LabelFrame(left, text=spec["label"], padding=10)
            section.pack(fill="x", pady=(10, 0))
            vars_for_channel = {
                "m": tk.StringVar(),
                "b": tk.StringVar(),
                "zero": tk.StringVar(),
            }
            self.channel_vars[channel_name] = vars_for_channel
            self.channel_fit_vars[channel_name] = tk.StringVar(value="type=linear")
            ttk.Label(section, text="Slope (m)").grid(row=0, column=0, sticky="w")
            ttk.Entry(section, textvariable=vars_for_channel["m"]).grid(row=0, column=1, sticky="ew", padx=(8, 0))
            ttk.Label(section, text="Intercept (b)").grid(row=1, column=0, sticky="w", pady=(6, 0))
            ttk.Entry(section, textvariable=vars_for_channel["b"]).grid(row=1, column=1, sticky="ew", padx=(8, 0),
                                                                        pady=(6, 0))
            ttk.Label(section, text="Zero raw").grid(row=2, column=0, sticky="w", pady=(6, 0))
            ttk.Entry(section, textvariable=vars_for_channel["zero"]).grid(row=2, column=1, sticky="ew", padx=(8, 0),
                                                                           pady=(6, 0))
            ttk.Label(section, text="Fit").grid(row=3, column=0, sticky="nw", pady=(6, 0))
            ttk.Label(
                section,
                textvariable=self.channel_fit_vars[channel_name],
                foreground=spec["color"],
                wraplength=260,
                justify="left",
            ).grid(row=3, column=1, sticky="w", padx=(8, 0), pady=(6, 0))
            section.columnconfigure(1, weight=1)

        save_options = ttk.Frame(left)
        save_options.pack(fill="x", pady=(12, 0))
        ttk.Checkbutton(save_options, text="Write .bak backup", variable=self.backup_var).pack(side="left")

        notebook = ttk.Notebook(right)
        notebook.pack(fill="both", expand=True)
        for channel_name, spec in CHANNEL_SPECS.items():
            pane = ChannelPane(notebook, self, channel_name)
            self.channel_panes[channel_name] = pane
            notebook.add(pane, text=spec["label"])

        status = ttk.Label(root, textvariable=self.status_var)
        status.pack(fill="x", pady=(10, 0))

    def set_status(self, message: str) -> None:
        self.status_var.set(message)

    def browse_file(self) -> None:
        chosen = filedialog.askopenfilename(
            title="Open calibration file",
            filetypes=[("JSON files", "*.json"), ("All files", "*.*")],
            initialfile=Path(self.path_var.get()).name if self.path_var.get().strip() else None,
        )
        if chosen:
            self.path_var.set(chosen)

    def load_from_ui(self) -> None:
        self.load_file(Path(self.path_var.get()))

    def load_file(self, path: Path) -> None:
        try:
            self.data = _normalize_data(_read_json(path))
        except FileNotFoundError:
            if not messagebox.askyesno(
                    "File not found",
                    f"{path} does not exist.\n\nCreate a new calibration file there?",
                    parent=self,
            ):
                return
            self.data = _normalize_data({})
        except Exception as exc:
            messagebox.showerror("Load failed", str(exc), parent=self)
            return

        self.path_var.set(str(path))
        self.refresh_all()
        self.set_status(f"Loaded {path}")

    def refresh_all(self) -> None:
        self.full_mass_var.set(_fmt_float(self.data.get("full_mass_kg"), 4))
        for channel_name, spec in CHANNEL_SPECS.items():
            linear = _linear_for_channel(self.data, channel_name)
            self.channel_vars[channel_name]["m"].set(_fmt_float(linear.get("m"), 6))
            self.channel_vars[channel_name]["b"].set(_fmt_float(linear.get("b"), 6))
            self.channel_vars[channel_name]["zero"].set(_fmt_float(self.data.get(spec["zero_key"]), 6))
            self.channel_fit_vars[channel_name].set(_fit_summary(self.data, channel_name))
            self.channel_panes[channel_name].refresh()

    def _collect_form(self) -> bool:
        try:
            self.data["full_mass_kg"] = _get_float(self.full_mass_var.get()) or DEFAULT_FULL_MASS_KG
            for channel_name, spec in CHANNEL_SPECS.items():
                linear = _linear_for_channel(self.data, channel_name)
                linear["m"] = _get_float(self.channel_vars[channel_name]["m"].get())
                linear["b"] = _get_float(self.channel_vars[channel_name]["b"].get())
                self.data[spec["zero_key"]] = _get_float(self.channel_vars[channel_name]["zero"].get())
        except ValueError as exc:
            messagebox.showerror("Invalid value", str(exc), parent=self)
            return False
        return True

    def save_file(self) -> None:
        if not self._collect_form():
            return
        path = Path(self.path_var.get())
        try:
            if self.backup_var.get() and path.exists():
                backup_path = path.with_suffix(path.suffix + ".bak")
                backup_path.write_text(path.read_text(encoding="utf-8"), encoding="utf-8")
            _write_json(path, self.data)
        except Exception as exc:
            messagebox.showerror("Save failed", str(exc), parent=self)
            self.set_status(f"Save failed: {exc}")
            return
        self.set_status(f"Saved {path}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("calibration_file", nargs="?", default=None)
    args = parser.parse_args()

    editor = CalibrationEditor(initial_file=args.calibration_file)
    editor.mainloop()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("\nCalibration editor interrupted.", file=sys.stderr)
        raise SystemExit(130)
