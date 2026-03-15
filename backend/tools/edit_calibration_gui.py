#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import math
import os
import socket
import sys
from collections import deque
from pathlib import Path
from typing import Any

import pyqtgraph as pg
from PyQt6 import QtCore, QtWidgets

DEFAULT_FULL_MASS_KG = 10.0
ALL_CALIBRATION_FIT_MODES = [
    "best",
    "linear",
    "linear_zero",
    "parabolic",
    "parabolic_zero",
    "cubic",
    "cubic_zero",
]


def _default_calibration_path() -> Path:
    env = os.environ.get("GS_LOADCELL_CALIBRATION_PATH", "").strip()
    if env:
        return Path(env)
    backend_dir = Path(__file__).resolve().parents[1]
    testing_path = backend_dir / "calibration" / "loadcell_calibration_testing.json"
    if testing_path.exists():
        return testing_path
    return backend_dir / "data" / "loadcell_calibration.json"


def _read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def _write_json(path: Path, data: dict[str, Any]) -> None:
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def _get_float(text: str) -> float | None:
    s = text.strip()
    if not s:
        return None
    return float(s)


class _PointValueDialog(QtWidgets.QDialog):
    def __init__(self, parent: QtWidgets.QWidget, *, title: str, weight: float = 0.0, raw: float = 0.0, show_raw: bool = True) -> None:
        super().__init__(parent)
        self.setWindowTitle(title)
        self.setModal(True)
        self.resize(340, 130 if show_raw else 110)

        layout = QtWidgets.QVBoxLayout(self)
        form = QtWidgets.QFormLayout()
        layout.addLayout(form)

        self.weight_spin = QtWidgets.QDoubleSpinBox()
        self.weight_spin.setRange(0.0, 10000.0)
        self.weight_spin.setDecimals(3)
        self.weight_spin.setSingleStep(0.1)
        self.weight_spin.setValue(float(weight))
        form.addRow("Expected kg:", self.weight_spin)

        self.raw_spin: QtWidgets.QDoubleSpinBox | None = None
        if show_raw:
            raw_spin = QtWidgets.QDoubleSpinBox()
            raw_spin.setRange(-1e12, 1e12)
            raw_spin.setDecimals(6)
            raw_spin.setSingleStep(0.1)
            raw_spin.setValue(float(raw))
            form.addRow("Raw value:", raw_spin)
            self.raw_spin = raw_spin

        buttons = QtWidgets.QDialogButtonBox(
            QtWidgets.QDialogButtonBox.StandardButton.Ok |
            QtWidgets.QDialogButtonBox.StandardButton.Cancel
        )
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        layout.addWidget(buttons)

    def values(self) -> tuple[float, float | None]:
        raw_v = float(self.raw_spin.value()) if self.raw_spin is not None else None
        return float(self.weight_spin.value()), raw_v


class _PointsSequenceDialog(QtWidgets.QDialog):
    def __init__(self, editor: "CalibrationEditor") -> None:
        super().__init__(editor)
        self._editor = editor
        self.setWindowTitle("Calibration Points / Sequence")
        self.setModal(True)
        self.resize(620, 420)

        layout = QtWidgets.QVBoxLayout(self)

        top = QtWidgets.QHBoxLayout()
        layout.addLayout(top)
        top.addWidget(QtWidgets.QLabel("Channel:"))
        self.channel_combo = QtWidgets.QComboBox()
        self.channel_combo.addItems(["50kg", "1000kg", "Tank Pressure"])
        self.channel_combo.currentIndexChanged.connect(self._reload_list)
        top.addWidget(self.channel_combo)
        self.sequence_lbl = QtWidgets.QLabel("Sequence: —")
        top.addWidget(self.sequence_lbl)
        top.addStretch(1)

        self.points_list = QtWidgets.QListWidget()
        layout.addWidget(self.points_list, stretch=1)

        btns = QtWidgets.QHBoxLayout()
        layout.addLayout(btns)
        self.add_btn = QtWidgets.QPushButton("Add point")
        self.add_btn.clicked.connect(self._on_add)
        btns.addWidget(self.add_btn)
        self.edit_btn = QtWidgets.QPushButton("Edit selected")
        self.edit_btn.clicked.connect(self._on_edit)
        btns.addWidget(self.edit_btn)
        self.remove_btn = QtWidgets.QPushButton("Remove selected")
        self.remove_btn.clicked.connect(self._on_remove)
        btns.addWidget(self.remove_btn)
        self.reset_btn = QtWidgets.QPushButton("Reset channel")
        self.reset_btn.clicked.connect(self._on_reset)
        btns.addWidget(self.reset_btn)

        seq_btns = QtWidgets.QHBoxLayout()
        layout.addLayout(seq_btns)
        self.continue_btn = QtWidgets.QPushButton("Continue sequence")
        self.continue_btn.clicked.connect(self._on_continue_sequence)
        seq_btns.addWidget(self.continue_btn)
        self.new_btn = QtWidgets.QPushButton("Start new sequence")
        self.new_btn.clicked.connect(self._on_new_sequence)
        seq_btns.addWidget(self.new_btn)
        seq_btns.addStretch(1)

        close_row = QtWidgets.QHBoxLayout()
        layout.addLayout(close_row)
        close_row.addStretch(1)
        close_btn = QtWidgets.QPushButton("Close")
        close_btn.clicked.connect(self.accept)
        close_row.addWidget(close_btn)

        self.status_lbl = QtWidgets.QLabel("Status: —")
        layout.addWidget(self.status_lbl)

        self._reload_list()

    def _channel(self) -> int:
        return int(self.channel_combo.currentIndex())

    def _channel_name(self, ch: int) -> str:
        return "50kg" if ch == 0 else ("1000kg" if ch == 1 else "Tank Pressure")

    def _reload_list(self) -> None:
        ch = self._channel()
        pts = self._editor._points_for_channel(ch)
        self.points_list.clear()
        if pts:
            for raw, kg in pts:
                self.points_list.addItem(f"{kg:g} kg -> {raw:.6g}")
        else:
            self.points_list.addItem("(no points)")
        started = self._editor._sequence_started.get(ch, False)
        self.sequence_lbl.setText(f"Sequence: {self._channel_name(ch)} {'started' if started else 'not started'}")

    def _on_add(self) -> None:
        dlg = _PointValueDialog(self, title="Add calibration point", weight=0.0, raw=0.0, show_raw=True)
        if dlg.exec() != int(QtWidgets.QDialog.DialogCode.Accepted):
            return
        kg, raw = dlg.values()
        if raw is None:
            return
        self._editor.add_manual_point(self._channel(), kg, raw)
        self.status_lbl.setText("Status: point added")
        self._reload_list()

    def _on_edit(self) -> None:
        ch = self._channel()
        row = self.points_list.currentRow()
        pts = self._editor._points_for_channel(ch)
        if row < 0 or row >= len(pts):
            self.status_lbl.setText("Status: select a point first")
            return
        raw, kg = pts[row]
        dlg = _PointValueDialog(self, title="Edit calibration point", weight=kg, raw=raw, show_raw=True)
        if dlg.exec() != int(QtWidgets.QDialog.DialogCode.Accepted):
            return
        kg_new, raw_new = dlg.values()
        if raw_new is None:
            return
        self._editor.update_manual_point(ch, row, kg_new, raw_new)
        self.status_lbl.setText("Status: point updated")
        self._reload_list()

    def _on_remove(self) -> None:
        ch = self._channel()
        row = self.points_list.currentRow()
        if not self._editor.remove_point(ch, row):
            self.status_lbl.setText("Status: select a point first")
            return
        self.status_lbl.setText("Status: point removed")
        self._reload_list()

    def _on_reset(self) -> None:
        ch = self._channel()
        if QtWidgets.QMessageBox.question(
            self,
            "Reset channel",
            f"Reset {self._channel_name(ch)} points and sequence?",
            QtWidgets.QMessageBox.StandardButton.Yes | QtWidgets.QMessageBox.StandardButton.No,
            QtWidgets.QMessageBox.StandardButton.No,
        ) != QtWidgets.QMessageBox.StandardButton.Yes:
            return
        self._editor.reset_channel_points(ch)
        self.status_lbl.setText(f"Status: {self._channel_name(ch)} reset")
        self._reload_list()

    def _on_continue_sequence(self) -> None:
        dlg = _PointValueDialog(self, title="Continue sequence", weight=1.0, show_raw=False)
        if dlg.exec() != int(QtWidgets.QDialog.DialogCode.Accepted):
            return
        kg, _ = dlg.values()
        if self._editor.start_sequence_capture(self._channel(), kg, start_new=False):
            self.status_lbl.setText(f"Status: capture started at {kg:g} kg")
        else:
            self.status_lbl.setText(self._editor.status_lbl.text())

    def _on_new_sequence(self) -> None:
        dlg = _PointValueDialog(self, title="Start new sequence", weight=0.0, show_raw=False)
        if dlg.exec() != int(QtWidgets.QDialog.DialogCode.Accepted):
            return
        kg, _ = dlg.values()
        if self._editor.start_sequence_capture(self._channel(), kg, start_new=True):
            self.status_lbl.setText("Status: new sequence started")
        else:
            self.status_lbl.setText(self._editor.status_lbl.text())


class CalibrationEditor(QtWidgets.QWidget):
    def __init__(self, initial_file: str | None = None, raw_port: int | None = None) -> None:
        super().__init__()
        self.setWindowTitle("Load Cell Calibration Editor")
        self.resize(1500, 980)

        self._data: dict[str, Any] | None = None
        self._path: Path | None = None
        self._points0_xy: list[tuple[float, float]] = []
        self._points1_xy: list[tuple[float, float]] = []
        self._points2_xy: list[tuple[float, float]] = []
        self._ch0_fit_meta: dict[str, Any] | None = None
        self._ch1_fit_meta: dict[str, Any] | None = None
        self._iadc_fit_meta: dict[str, Any] | None = None
        self._raw_port = raw_port
        self._raw_sock: socket.socket | None = None
        self._raw_timer: QtCore.QTimer | None = None
        self._raw_recent: deque[tuple[float, float, float]] = deque(maxlen=512)
        self._live_raw_ch0 = 0.0
        self._live_raw_ch1 = 0.0
        self._live_raw_iadc = 0.0
        self._capture_active = False
        self._capture_channel = 0
        self._capture_weight = 0.0
        self._capture_target = 200
        self._capture_vals: list[float] = []
        self._capture_mode = "manual"
        self._sequence_started: dict[int, bool] = {0: False, 1: False, 2: False}

        layout = QtWidgets.QVBoxLayout(self)

        file_row = QtWidgets.QHBoxLayout()
        layout.addLayout(file_row)
        file_row.addWidget(QtWidgets.QLabel("Calibration file:"))
        self.file_edit = QtWidgets.QLineEdit(initial_file or str(_default_calibration_path()))
        file_row.addWidget(self.file_edit, stretch=1)
        self.browse_btn = QtWidgets.QPushButton("Browse")
        self.browse_btn.clicked.connect(self._browse)
        file_row.addWidget(self.browse_btn)
        self.load_btn = QtWidgets.QPushButton("Load")
        self.load_btn.clicked.connect(self._load)
        file_row.addWidget(self.load_btn)

        grid = QtWidgets.QGridLayout()
        layout.addLayout(grid)

        grid.addWidget(QtWidgets.QLabel("Channel"), 0, 0)
        grid.addWidget(QtWidgets.QLabel("Slope (m)"), 0, 1)
        grid.addWidget(QtWidgets.QLabel("Intercept (b)"), 0, 2)

        grid.addWidget(QtWidgets.QLabel("50kg"), 1, 0)
        self.ch0_m = QtWidgets.QLineEdit()
        self.ch0_b = QtWidgets.QLineEdit()
        grid.addWidget(self.ch0_m, 1, 1)
        grid.addWidget(self.ch0_b, 1, 2)

        grid.addWidget(QtWidgets.QLabel("1000kg"), 2, 0)
        self.ch1_m = QtWidgets.QLineEdit()
        self.ch1_b = QtWidgets.QLineEdit()
        grid.addWidget(self.ch1_m, 2, 1)
        grid.addWidget(self.ch1_b, 2, 2)

        grid.addWidget(QtWidgets.QLabel("Tank Pressure"), 3, 0)
        self.iadc_m = QtWidgets.QLineEdit()
        self.iadc_b = QtWidgets.QLineEdit()
        grid.addWidget(self.iadc_m, 3, 1)
        grid.addWidget(self.iadc_b, 3, 2)

        zero_row = QtWidgets.QHBoxLayout()
        layout.addLayout(zero_row)
        zero_row.addWidget(QtWidgets.QLabel("Zero raw 50kg:"))
        self.ch0_zero = QtWidgets.QLineEdit()
        self.ch0_zero.setFixedWidth(160)
        zero_row.addWidget(self.ch0_zero)
        zero_row.addSpacing(20)
        zero_row.addWidget(QtWidgets.QLabel("Zero raw 1000kg:"))
        self.ch1_zero = QtWidgets.QLineEdit()
        self.ch1_zero.setFixedWidth(160)
        zero_row.addWidget(self.ch1_zero)
        zero_row.addSpacing(20)
        zero_row.addWidget(QtWidgets.QLabel("Zero raw TankP:"))
        self.iadc_zero = QtWidgets.QLineEdit()
        self.iadc_zero.setFixedWidth(160)
        zero_row.addWidget(self.iadc_zero)
        zero_row.addStretch(1)

        info_row = QtWidgets.QHBoxLayout()
        layout.addLayout(info_row)
        self.weights_lbl = QtWidgets.QLabel("Weights: —")
        info_row.addWidget(self.weights_lbl)
        info_row.addStretch(1)

        points_row = QtWidgets.QHBoxLayout()
        layout.addLayout(points_row)
        self.points_ch0_lbl = QtWidgets.QLabel("50kg points: —")
        self.points_ch1_lbl = QtWidgets.QLabel("1000kg points: —")
        self.points_iadc_lbl = QtWidgets.QLabel("Tank pressure points: —")
        points_row.addWidget(self.points_ch0_lbl)
        points_row.addWidget(self.points_ch1_lbl)
        points_row.addWidget(self.points_iadc_lbl)
        points_row.addStretch(1)

        fit_row = QtWidgets.QHBoxLayout()
        layout.addLayout(fit_row)
        self.ch0_fit_lbl = QtWidgets.QLabel("50kg fit: —")
        fit_row.addWidget(self.ch0_fit_lbl)
        self.ch1_fit_lbl = QtWidgets.QLabel("1000kg fit: —")
        fit_row.addWidget(self.ch1_fit_lbl)
        self.iadc_fit_lbl = QtWidgets.QLabel("Tank pressure fit: —")
        fit_row.addWidget(self.iadc_fit_lbl)
        fit_row.addStretch(1)

        fit_mode_row = QtWidgets.QHBoxLayout()
        layout.addLayout(fit_mode_row)
        fit_mode_row.addWidget(QtWidgets.QLabel("50kg mode:"))
        self.ch0_fit_mode = QtWidgets.QComboBox()
        self.ch0_fit_mode.addItems(ALL_CALIBRATION_FIT_MODES)
        fit_mode_row.addWidget(self.ch0_fit_mode)
        fit_mode_row.addSpacing(12)
        fit_mode_row.addWidget(QtWidgets.QLabel("1000kg mode:"))
        self.ch1_fit_mode = QtWidgets.QComboBox()
        self.ch1_fit_mode.addItems(ALL_CALIBRATION_FIT_MODES)
        fit_mode_row.addWidget(self.ch1_fit_mode)
        fit_mode_row.addSpacing(12)
        fit_mode_row.addWidget(QtWidgets.QLabel("TankP mode:"))
        self.iadc_fit_mode = QtWidgets.QComboBox()
        self.iadc_fit_mode.addItems(ALL_CALIBRATION_FIT_MODES)
        fit_mode_row.addWidget(self.iadc_fit_mode)
        fit_mode_row.addStretch(1)

        ops_row = QtWidgets.QHBoxLayout()
        layout.addLayout(ops_row)
        self.points_modal_btn = QtWidgets.QPushButton("Edit points / sequence")
        self.points_modal_btn.clicked.connect(self._open_points_modal)
        ops_row.addWidget(self.points_modal_btn)
        self.refit_btn = QtWidgets.QPushButton("Refit from points")
        self.refit_btn.clicked.connect(self._refit_from_points)
        ops_row.addWidget(self.refit_btn)
        self.seq_state_lbl = QtWidgets.QLabel("Sequence: 50kg not started, 1000kg not started, TankP not started")
        ops_row.addWidget(self.seq_state_lbl)
        ops_row.addStretch(1)

        plots_row = QtWidgets.QHBoxLayout()
        layout.addLayout(plots_row)
        self.plot_widget = pg.GraphicsLayoutWidget()
        plots_row.addWidget(self.plot_widget, stretch=1)

        self.p0 = self.plot_widget.addPlot(row=0, col=0, title="50kg Calibration")
        self.p1 = self.plot_widget.addPlot(row=0, col=1, title="1000kg Calibration")
        self.p2 = self.plot_widget.addPlot(row=0, col=2, title="Tank Pressure Calibration")
        for p in (self.p0, self.p1, self.p2):
            p.showGrid(x=True, y=True)
            p.setLabel("bottom", "Raw")
            p.setLabel("left", "Expected (kg)")
        self.p0_points = self.p0.plot([], [], pen=None, symbol="o", symbolSize=7)
        self.p0_fit = self.p0.plot([], [], pen=pg.mkPen(width=2))
        self.p1_points = self.p1.plot([], [], pen=None, symbol="o", symbolSize=7)
        self.p1_fit = self.p1.plot([], [], pen=pg.mkPen(width=2))
        self.p2_points = self.p2.plot([], [], pen=None, symbol="o", symbolSize=7)
        self.p2_fit = self.p2.plot([], [], pen=pg.mkPen(width=2))

        opts_row = QtWidgets.QHBoxLayout()
        layout.addLayout(opts_row)
        self.backup_chk = QtWidgets.QCheckBox("Write .bak backup")
        self.backup_chk.setChecked(True)
        opts_row.addWidget(self.backup_chk)
        opts_row.addStretch(1)

        action_row = QtWidgets.QHBoxLayout()
        layout.addLayout(action_row)
        self.save_btn = QtWidgets.QPushButton("Save")
        self.save_btn.clicked.connect(self._save)
        action_row.addWidget(self.save_btn)
        action_row.addStretch(1)
        self.status_lbl = QtWidgets.QLabel("Status: —")
        action_row.addWidget(self.status_lbl)

        live_row = QtWidgets.QHBoxLayout()
        layout.addLayout(live_row)
        self.live_lbl = QtWidgets.QLabel("Live raw: —")
        live_row.addWidget(self.live_lbl)
        live_row.addStretch(1)

        for w in (self.ch0_m, self.ch0_b, self.ch1_m, self.ch1_b):
            w.textChanged.connect(self._update_regression_plots)

        if self._raw_port is not None:
            try:
                self._raw_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                self._raw_sock.bind(("127.0.0.1", int(self._raw_port)))
                self._raw_sock.setblocking(False)
                self._raw_timer = QtCore.QTimer(self)
                self._raw_timer.setInterval(10)
                self._raw_timer.timeout.connect(self._poll_raw_stream)
                self._raw_timer.start()
                self.live_lbl.setText(f"Live raw: listening on 127.0.0.1:{int(self._raw_port)}")
            except Exception as e:
                self.live_lbl.setText(f"Live raw: socket error ({e})")

        self._update_sequence_status_label()
        # Auto-load default file on startup (best-effort)
        QtCore.QTimer.singleShot(0, self._load)

    def _selected_fit_mode(self, channel: int) -> str:
        if channel == 0:
            return self.ch0_fit_mode.currentText()
        if channel == 1:
            return self.ch1_fit_mode.currentText()
        return self.iadc_fit_mode.currentText()

    def _set_selected_fit_mode(self, channel: int, mode: str | None) -> None:
        combo = self.ch0_fit_mode if channel == 0 else (self.ch1_fit_mode if channel == 1 else self.iadc_fit_mode)
        normalized = {
            None: "best",
            "quadratic": "parabolic",
            "quadratic_zero": "parabolic_zero",
            "poly2": "parabolic",
            "poly2_zero": "parabolic_zero",
            "poly3": "cubic",
            "poly3_zero": "cubic_zero",
        }.get(mode, mode)
        idx = combo.findText(normalized)
        combo.setCurrentIndex(idx if idx >= 0 else 0)

    def _format_fit_label(self, prefix: str, meta: dict[str, Any] | None) -> str:
        if not isinstance(meta, dict):
            return f"{prefix}: —"
        fit_type = str(meta.get("type") or "—")
        if fit_type == "poly2":
            return f"{prefix}: poly2 (a={meta.get('a')}, b={meta.get('b')}, c={meta.get('c')}, x0={meta.get('x0')})"
        if fit_type == "poly3":
            return f"{prefix}: poly3 (a={meta.get('a')}, b={meta.get('b')}, c={meta.get('c')}, d={meta.get('d')}, x0={meta.get('x0')})"
        if fit_type == "linear":
            return f"{prefix}: linear (a={meta.get('a')}, b={meta.get('b')}, x0={meta.get('x0')})"
        return f"{prefix}: {fit_type}"

    def _predict_with_meta(self, meta: dict[str, Any] | None, x: float) -> float | None:
        if not isinstance(meta, dict):
            return None
        fit_type = meta.get("type")
        x0 = meta.get("x0")
        x_eff = x - float(x0) if x0 is not None else x
        if fit_type == "linear":
            a = meta.get("a")
            if a is None:
                return None
            intercept = 0.0 if x0 is not None else float(meta.get("b") or 0.0)
            return float(a) * x_eff + intercept
        if fit_type == "poly2":
            a = meta.get("a")
            b = meta.get("b")
            if a is None or b is None:
                return None
            c = 0.0 if x0 is not None else float(meta.get("c") or 0.0)
            return float(a) * x_eff * x_eff + float(b) * x_eff + c
        if fit_type == "poly3":
            a = meta.get("a")
            b = meta.get("b")
            c = meta.get("c")
            if a is None or b is None or c is None:
                return None
            d = 0.0 if x0 is not None else float(meta.get("d") or 0.0)
            return float(a) * x_eff * x_eff * x_eff + float(b) * x_eff * x_eff + float(c) * x_eff + d
        return None

    def _fit_points_backend(
        self, points_xy: list[tuple[float, float]], mode_name: str
    ) -> tuple[float, float, float | None, dict[str, Any]]:
        if len(points_xy) < 2:
            raise ValueError("need at least 2 points")
        xs = [float(x) for x, _ in points_xy]
        ys = [float(y) for _, y in points_xy]
        zero_candidates = [x for x, y in points_xy if abs(y) < 1e-9]
        zero_hint = float(zero_candidates[0]) if zero_candidates else None

        def fit_line(xs_v: list[float], ys_v: list[float]) -> tuple[float, float]:
            n = float(len(xs_v))
            sx = sum(xs_v)
            sy = sum(ys_v)
            sxx = sum(x * x for x in xs_v)
            sxy = sum(x * y for x, y in zip(xs_v, ys_v))
            denom = (n * sxx) - (sx * sx)
            if abs(denom) < 1e-18:
                raise ValueError("degenerate points")
            return (n * sxy - sx * sy) / denom, (sy - ((n * sxy - sx * sy) / denom) * sx) / n

        def fit_line_zero(xs_v: list[float], ys_v: list[float]) -> float:
            denom = sum(x * x for x in xs_v)
            if abs(denom) < 1e-18:
                raise ValueError("degenerate points")
            return sum(x * y for x, y in zip(xs_v, ys_v)) / denom

        def fit_poly2(xs_v: list[float], ys_v: list[float]) -> tuple[float, float, float]:
            n = float(len(xs_v))
            sx = sum(xs_v)
            sx2 = sum(x * x for x in xs_v)
            sx3 = sum(x * x * x for x in xs_v)
            sx4 = sum(x * x * x * x for x in xs_v)
            sy = sum(ys_v)
            sxy = sum(x * y for x, y in zip(xs_v, ys_v))
            sx2y = sum((x * x) * y for x, y in zip(xs_v, ys_v))
            a11, a12, a13 = sx4, sx3, sx2
            a21, a22, a23 = sx3, sx2, sx
            a31, a32, a33 = sx2, sx, n
            b1, b2, b3 = sx2y, sxy, sy
            if abs(a11) < 1e-18:
                raise ValueError("degenerate points")
            f21 = a21 / a11
            f31 = a31 / a11
            a22 -= f21 * a12
            a23 -= f21 * a13
            b2 -= f21 * b1
            a32 -= f31 * a12
            a33 -= f31 * a13
            b3 -= f31 * b1
            if abs(a22) < 1e-18:
                raise ValueError("degenerate points")
            f32 = a32 / a22
            a33 -= f32 * a23
            b3 -= f32 * b2
            if abs(a33) < 1e-18:
                raise ValueError("degenerate points")
            c = b3 / a33
            b = (b2 - a23 * c) / a22
            a = (b1 - a12 * b - a13 * c) / a11
            return a, b, c

        def fit_poly2_zero(xs_v: list[float], ys_v: list[float]) -> tuple[float, float]:
            sx2 = sum(x * x for x in xs_v)
            sx3 = sum(x * x * x for x in xs_v)
            sx4 = sum(x * x * x * x for x in xs_v)
            sxy = sum(x * y for x, y in zip(xs_v, ys_v))
            sx2y = sum((x * x) * y for x, y in zip(xs_v, ys_v))
            det = sx4 * sx2 - sx3 * sx3
            if abs(det) < 1e-18:
                raise ValueError("degenerate points")
            return (sx2y * sx2 - sxy * sx3) / det, (sx4 * sxy - sx3 * sx2y) / det

        def solve_linear_system(a: list[list[float]], b: list[float]) -> list[float]:
            n = len(a)
            for i in range(n):
                pivot = max(range(i, n), key=lambda r: abs(a[r][i]))
                if abs(a[pivot][i]) < 1e-18:
                    raise ValueError("degenerate points")
                if pivot != i:
                    a[i], a[pivot] = a[pivot], a[i]
                    b[i], b[pivot] = b[pivot], b[i]
                pivot_val = a[i][i]
                for j in range(i, n):
                    a[i][j] /= pivot_val
                b[i] /= pivot_val
                for r in range(n):
                    if r == i:
                        continue
                    factor = a[r][i]
                    if abs(factor) < 1e-18:
                        continue
                    for j in range(i, n):
                        a[r][j] -= factor * a[i][j]
                    b[r] -= factor * b[i]
            return b

        def fit_poly3(xs_v: list[float], ys_v: list[float]) -> tuple[float, float, float, float]:
            sx = sum(xs_v)
            sx2 = sum(x * x for x in xs_v)
            sx3 = sum(x * x * x for x in xs_v)
            sx4 = sum(x * x * x * x for x in xs_v)
            sx5 = sum(x * x * x * x * x for x in xs_v)
            sx6 = sum(x * x * x * x * x * x for x in xs_v)
            sy = sum(ys_v)
            sxy = sum(x * y for x, y in zip(xs_v, ys_v))
            sx2y = sum((x * x) * y for x, y in zip(xs_v, ys_v))
            sx3y = sum((x * x * x) * y for x, y in zip(xs_v, ys_v))
            sol = solve_linear_system(
                [
                    [sx6, sx5, sx4, sx3],
                    [sx5, sx4, sx3, sx2],
                    [sx4, sx3, sx2, sx],
                    [sx3, sx2, sx, float(len(xs_v))],
                ],
                [sx3y, sx2y, sxy, sy],
            )
            return sol[0], sol[1], sol[2], sol[3]

        def fit_poly3_zero(xs_v: list[float], ys_v: list[float]) -> tuple[float, float, float]:
            sx2 = sum(x * x for x in xs_v)
            sx3 = sum(x * x * x for x in xs_v)
            sx4 = sum(x * x * x * x for x in xs_v)
            sx5 = sum(x * x * x * x * x for x in xs_v)
            sx6 = sum(x * x * x * x * x * x for x in xs_v)
            sxy = sum(x * y for x, y in zip(xs_v, ys_v))
            sx2y = sum((x * x) * y for x, y in zip(xs_v, ys_v))
            sx3y = sum((x * x * x) * y for x, y in zip(xs_v, ys_v))
            sol = solve_linear_system(
                [
                    [sx6, sx5, sx4],
                    [sx5, sx4, sx3],
                    [sx4, sx3, sx2],
                ],
                [sx3y, sx2y, sxy],
            )
            return sol[0], sol[1], sol[2]

        def sse_line(xs_v: list[float], ys_v: list[float], m: float, b: float) -> float:
            return sum((y - (m * x + b)) ** 2 for x, y in zip(xs_v, ys_v))

        def sse_poly2(xs_v: list[float], ys_v: list[float], a: float, b: float, c: float) -> float:
            return sum((y - (a * x * x + b * x + c)) ** 2 for x, y in zip(xs_v, ys_v))

        def sse_poly3(xs_v: list[float], ys_v: list[float], a: float, b: float, c: float, d: float) -> float:
            return sum((y - (a * x * x * x + b * x * x + c * x + d)) ** 2 for x, y in zip(xs_v, ys_v))

        def aic(sse: float, n: int, k: int) -> float:
            if n <= 0:
                return float("inf")
            s = max(sse, 1e-18)
            return n * math.log(s / n) + (2 * k)

        candidates: list[tuple[str, float, dict[str, Any], float, float, float | None]] = []
        lin_m, lin_b = fit_line(xs, ys)
        candidates.append(("linear", aic(sse_line(xs, ys, lin_m, lin_b), len(xs), 2), {"type": "linear", "a": lin_m, "b": lin_b, "x0": None}, lin_m, lin_b, None))

        if zero_hint is not None:
            xs_shift = [x - zero_hint for x in xs]
            lin0_m = fit_line_zero(xs_shift, ys)
            candidates.append(("linear_zero", aic(sse_line(xs_shift, ys, lin0_m, 0.0), len(xs_shift), 1), {"type": "linear", "a": lin0_m, "b": 0.0, "x0": zero_hint}, lin0_m, -(lin0_m * zero_hint), zero_hint))

        if len(xs) >= 3:
            a2, b2, c2 = fit_poly2(xs, ys)
            candidates.append(("parabolic", aic(sse_poly2(xs, ys, a2, b2, c2), len(xs), 3), {"type": "poly2", "a": a2, "b": b2, "c": c2, "x0": None}, lin_m, lin_b, None))

        if zero_hint is not None and len(xs) >= 2:
            xs_shift = [x - zero_hint for x in xs]
            a2z, b2z = fit_poly2_zero(xs_shift, ys)
            candidates.append(("parabolic_zero", aic(sse_poly2(xs_shift, ys, a2z, b2z, 0.0), len(xs_shift), 2), {"type": "poly2", "a": a2z, "b": b2z, "c": 0.0, "x0": zero_hint}, lin_m, lin_b, zero_hint))

        if len(xs) >= 4:
            a3, b3, c3, d3 = fit_poly3(xs, ys)
            candidates.append(("cubic", aic(sse_poly3(xs, ys, a3, b3, c3, d3), len(xs), 4), {"type": "poly3", "a": a3, "b": b3, "c": c3, "d": d3, "x0": None}, lin_m, lin_b, None))

        if zero_hint is not None and len(xs) >= 3:
            xs_shift = [x - zero_hint for x in xs]
            a3z, b3z, c3z = fit_poly3_zero(xs_shift, ys)
            candidates.append(("cubic_zero", aic(sse_poly3(xs_shift, ys, a3z, b3z, c3z, 0.0), len(xs_shift), 3), {"type": "poly3", "a": a3z, "b": b3z, "c": c3z, "d": 0.0, "x0": zero_hint}, lin_m, lin_b, zero_hint))

        if mode_name == "best":
            selected = min(candidates, key=lambda entry: entry[1])
        else:
            selected = next((entry for entry in candidates if entry[0] == mode_name), None)
            if selected is None:
                raise ValueError(f"fit mode '{mode_name}' unavailable for current points")
        _, _, meta, m, b, x0 = selected
        return m, b, x0, meta

    def _browse(self) -> None:
        path, _ = QtWidgets.QFileDialog.getOpenFileName(
            self, "Select calibration file", "", "JSON Files (*.json);;All Files (*)"
        )
        if path:
            self.file_edit.setText(path)

    def _load(self) -> None:
        path = Path(self.file_edit.text().strip())
        if not path.exists():
            self.status_lbl.setText(f"Status: file not found: {path}")
            return
        try:
            data = _read_json(path)
        except Exception as e:
            self.status_lbl.setText(f"Status: load failed ({e})")
            return

        self._data = data
        self._path = path
        self._data.setdefault("version", 1)
        self._data.setdefault("full_mass_kg", DEFAULT_FULL_MASS_KG)

        ch0 = data.get("ch0", {})
        ch1 = data.get("ch1", {})

        self.ch0_m.setText("" if "m" not in ch0 or ch0["m"] is None else str(ch0["m"]))
        self.ch0_b.setText("" if "b" not in ch0 or ch0["b"] is None else str(ch0["b"]))
        self.ch1_m.setText("" if "m" not in ch1 or ch1["m"] is None else str(ch1["m"]))
        self.ch1_b.setText("" if "b" not in ch1 or ch1["b"] is None else str(ch1["b"]))
        iadc = data.get("iadc", {})
        self.iadc_m.setText("" if "m" not in iadc or iadc["m"] is None else str(iadc["m"]))
        self.iadc_b.setText("" if "b" not in iadc or iadc["b"] is None else str(iadc["b"]))

        self.ch0_zero.setText("" if data.get("ch0_zero_raw") is None else str(data.get("ch0_zero_raw")))
        self.ch1_zero.setText("" if data.get("ch1_zero_raw") is None else str(data.get("ch1_zero_raw")))
        self.iadc_zero.setText("" if data.get("iadc_zero_raw") is None else str(data.get("iadc_zero_raw")))
        self._sequence_started[0] = data.get("ch0_zero_raw") is not None
        self._sequence_started[1] = data.get("ch1_zero_raw") is not None
        self._sequence_started[2] = data.get("iadc_zero_raw") is not None
        self._update_sequence_status_label()

        pts0 = data.get("points", [])
        pts1 = data.get("points_ch1", [])
        pts2 = data.get("points_iadc", [])
        ch1_fit = data.get("ch1_fit", {})
        ch0_fit = data.get("ch0_fit", {})
        iadc_fit = data.get("iadc_fit", {})
        ch0_m = _get_float(self.ch0_m.text())
        ch0_b = _get_float(self.ch0_b.text())
        ch1_m = _get_float(self.ch1_m.text())
        ch1_b = _get_float(self.ch1_b.text())
        iadc_m = _get_float(self.iadc_m.text())
        iadc_b = _get_float(self.iadc_b.text())
        self._ch0_fit_meta = ch0_fit if isinstance(ch0_fit, dict) else None
        self._ch1_fit_meta = ch1_fit if isinstance(ch1_fit, dict) else None
        self._iadc_fit_meta = iadc_fit if isinstance(iadc_fit, dict) else None
        if self._ch0_fit_meta is None and ch0_m is not None and ch0_b is not None:
            self._ch0_fit_meta = {"type": "linear", "a": ch0_m, "b": ch0_b, "x0": _get_float(self.ch0_zero.text())}
        if self._ch1_fit_meta is None and ch1_m is not None and ch1_b is not None:
            self._ch1_fit_meta = {"type": "linear", "a": ch1_m, "b": ch1_b, "x0": _get_float(self.ch1_zero.text())}
        if self._iadc_fit_meta is None and iadc_m is not None and iadc_b is not None:
            self._iadc_fit_meta = {"type": "linear", "a": iadc_m, "b": iadc_b, "x0": _get_float(self.iadc_zero.text())}
        self.ch0_fit_lbl.setText(self._format_fit_label("50kg fit", self._ch0_fit_meta))
        self.ch1_fit_lbl.setText(self._format_fit_label("1000kg fit", self._ch1_fit_meta))
        self.iadc_fit_lbl.setText(self._format_fit_label("Tank pressure fit", self._iadc_fit_meta))
        self._set_selected_fit_mode(0, "best" if self._ch0_fit_meta is None else str(self._ch0_fit_meta.get("type")))
        self._set_selected_fit_mode(1, "best" if self._ch1_fit_meta is None else str(self._ch1_fit_meta.get("type")))
        self._set_selected_fit_mode(2, "best" if self._iadc_fit_meta is None else str(self._iadc_fit_meta.get("type")))

        self._points0_xy = []
        self._points1_xy = []
        self._points2_xy = []
        if isinstance(pts0, list) and pts0:
            for p in pts0:
                try:
                    raw = p.get("ch0_raw")
                    kg = p.get("kg")
                    if raw is not None and kg is not None:
                        self._points0_xy.append((float(raw), float(kg)))
                except Exception:
                    pass

        if isinstance(pts1, list) and pts1:
            for p in pts1:
                try:
                    raw = p.get("ch1_raw")
                    kg = p.get("kg")
                    if raw is not None and kg is not None:
                        self._points1_xy.append((float(raw), float(kg)))
                except Exception:
                    pass
        if isinstance(pts2, list) and pts2:
            for p in pts2:
                try:
                    raw = p.get("iadc_raw")
                    expected = p.get("expected")
                    if expected is None:
                        expected = p.get("kg")
                    if raw is not None and expected is not None:
                        self._points2_xy.append((float(raw), float(expected)))
                except Exception:
                    pass

        self._refresh_points_lists()
        self._update_regression_plots()
        self.status_lbl.setText(f"Status: loaded {path}")

    def _refresh_points_lists(self) -> None:
        self.points_ch0_lbl.setText(f"50kg points: {len(self._points0_xy)}")
        self.points_ch1_lbl.setText(f"1000kg points: {len(self._points1_xy)}")
        self.points_iadc_lbl.setText(f"Tank pressure points: {len(self._points2_xy)}")
        all_w = sorted({kg for _, kg in self._points0_xy} | {kg for _, kg in self._points1_xy} | {v for _, v in self._points2_xy})
        self.weights_lbl.setText(f"Weights: {all_w}" if all_w else "Weights: —")
        self._update_sequence_status_label()
        self._update_regression_plots()

    def _update_sequence_status_label(self) -> None:
        ch0 = "started" if self._sequence_started.get(0, False) else "not started"
        ch1 = "started" if self._sequence_started.get(1, False) else "not started"
        ch2 = "started" if self._sequence_started.get(2, False) else "not started"
        self.seq_state_lbl.setText(f"Sequence: 50kg {ch0}, 1000kg {ch1}, TankP {ch2}")

    def _open_points_modal(self) -> None:
        dlg = _PointsSequenceDialog(self)
        dlg.exec()

    def _points_for_channel(self, channel: int) -> list[tuple[float, float]]:
        ch = int(channel)
        if ch == 0:
            return self._points0_xy
        if ch == 1:
            return self._points1_xy
        return self._points2_xy

    def _channel_name(self, channel: int) -> str:
        ch = int(channel)
        return "50kg" if ch == 0 else ("1000kg" if ch == 1 else "Tank Pressure")

    def _upsert_point(self, channel: int, kg: float, raw: float) -> str:
        pts = self._points_for_channel(channel)
        for i, (_raw_i, kg_i) in enumerate(pts):
            if abs(kg_i - kg) < 1e-9:
                pts[i] = (raw, kg)
                return "updated"
        pts.append((raw, kg))
        return "added"

    def _begin_capture(self, channel: int, weight: float, mode: str) -> None:
        if self._raw_sock is None:
            self.status_lbl.setText("Status: no live stream connected")
            return
        if self._capture_active:
            self.status_lbl.setText("Status: capture already in progress")
            return
        self._capture_channel = int(channel)
        self._capture_weight = float(weight)
        self._capture_mode = mode
        self._capture_vals.clear()
        self._capture_active = True
        self.status_lbl.setText(
            f"Status: capturing {self._channel_name(self._capture_channel)} 0/{self._capture_target} at {self._capture_weight:g}..."
        )

    def add_manual_point(self, channel: int, kg: float, raw: float) -> None:
        pts = self._points_for_channel(channel)
        pts.append((float(raw), float(kg)))
        self.status_lbl.setText(f"Status: added {self._channel_name(channel)} point ({kg:g}, {raw:.6g})")
        self._refresh_points_lists()

    def update_manual_point(self, channel: int, index: int, kg: float, raw: float) -> bool:
        pts = self._points_for_channel(channel)
        if index < 0 or index >= len(pts):
            return False
        pts[index] = (float(raw), float(kg))
        self.status_lbl.setText(f"Status: updated {self._channel_name(channel)} point")
        self._refresh_points_lists()
        return True

    def remove_point(self, channel: int, index: int) -> bool:
        pts = self._points_for_channel(channel)
        if index < 0 or index >= len(pts):
            return False
        del pts[index]
        self.status_lbl.setText(f"Status: removed {self._channel_name(channel)} point")
        self._refresh_points_lists()
        return True

    def reset_channel_points(self, channel: int) -> None:
        ch = int(channel)
        if ch == 0:
            self._points0_xy.clear()
            self.ch0_zero.setText("")
        elif ch == 1:
            self._points1_xy.clear()
            self.ch1_zero.setText("")
        else:
            self._points2_xy.clear()
            self.iadc_zero.setText("")
        self._sequence_started[ch] = False
        self.status_lbl.setText(f"Status: reset {self._channel_name(ch)} points and sequence")
        self._refresh_points_lists()

    def start_sequence_capture(self, channel: int, kg: float, *, start_new: bool) -> bool:
        ch = int(channel)
        if start_new:
            if abs(float(kg)) > 1e-9:
                self.status_lbl.setText("Status: new sequence must start at 0 kg")
                return False
            self.reset_channel_points(ch)
            self._begin_capture(ch, 0.0, "sequence_zero")
            return True
        if not self._sequence_started.get(ch, False):
            self.status_lbl.setText(f"Status: start {self._channel_name(ch)} sequence first (0)")
            return False
        if float(kg) <= 0.0:
            self.status_lbl.setText("Status: expected kg must be > 0 for sequence points")
            return False
        self._begin_capture(ch, float(kg), "sequence_point")
        return True

    def _poll_raw_stream(self) -> None:
        if self._raw_sock is None:
            return
        received = False
        while True:
            try:
                payload, _addr = self._raw_sock.recvfrom(256)
            except BlockingIOError:
                break
            except Exception:
                break
            try:
                parts = payload.decode("ascii", errors="ignore").strip().split(",")
                if len(parts) < 3:
                    continue
                t_s, c0_s, c1_s = parts[0], parts[1], parts[2]
                t_mono = float(t_s)
                ch0 = float(c0_s)
                ch1 = float(c1_s)
                iadc = float(parts[3]) if len(parts) >= 4 else self._live_raw_iadc
            except Exception:
                continue
            received = True
            self._raw_recent.append((t_mono, ch0, ch1))
            self._live_raw_ch0 = ch0
            self._live_raw_ch1 = ch1
            self._live_raw_iadc = iadc
            if self._capture_active:
                if self._capture_channel == 0:
                    v = ch0
                elif self._capture_channel == 1:
                    v = ch1
                else:
                    v = iadc
                self._capture_vals.append(v)
                n = len(self._capture_vals)
                self.status_lbl.setText(
                    f"Status: capturing {self._channel_name(self._capture_channel)} {n}/{self._capture_target} at {self._capture_weight:g}..."
                )
                if n >= self._capture_target:
                    avg = sum(self._capture_vals) / float(n)
                    action = self._upsert_point(self._capture_channel, self._capture_weight, avg)
                    if self._capture_mode == "sequence_zero":
                        if self._capture_channel == 0:
                            self.ch0_zero.setText(str(avg))
                        elif self._capture_channel == 1:
                            self.ch1_zero.setText(str(avg))
                        else:
                            self.iadc_zero.setText(str(avg))
                        self._sequence_started[self._capture_channel] = True
                    self._capture_active = False
                    self._capture_vals.clear()
                    self._refresh_points_lists()
                    self.status_lbl.setText(
                        f"Status: captured avg raw={avg:.6g} at {self._capture_weight:g} on {self._channel_name(self._capture_channel)} ({action})"
                    )
        if received and not self._capture_active:
            self.live_lbl.setText(
                f"Live raw: 50kg={self._live_raw_ch0:.6g}  1000kg={self._live_raw_ch1:.6g}  TankP={self._live_raw_iadc:.6g}"
            )

    def _refit_from_points(self) -> None:
        try:
            if len(self._points0_xy) >= 2:
                m0, b0, x0, fit0 = self._fit_points_backend(self._points0_xy, self._selected_fit_mode(0))
                self.ch0_m.setText(str(m0))
                self.ch0_b.setText(str(b0))
                self.ch0_zero.setText("" if x0 is None else str(x0))
                self._ch0_fit_meta = fit0
                self.ch0_fit_lbl.setText(self._format_fit_label("50kg fit", fit0))
            if len(self._points1_xy) >= 2:
                m1, b1, x1, fit1 = self._fit_points_backend(self._points1_xy, self._selected_fit_mode(1))
                self.ch1_m.setText(str(m1))
                self.ch1_b.setText(str(b1))
                self.ch1_zero.setText("" if x1 is None else str(x1))
                self._ch1_fit_meta = fit1
                self.ch1_fit_lbl.setText(self._format_fit_label("1000kg fit", fit1))
            if len(self._points2_xy) >= 2:
                m2, b2, x2, fit2 = self._fit_points_backend(self._points2_xy, self._selected_fit_mode(2))
                self.iadc_m.setText(str(m2))
                self.iadc_b.setText(str(b2))
                self.iadc_zero.setText("" if x2 is None else str(x2))
                self.iadc_fit_lbl.setText(self._format_fit_label("Tank pressure fit", fit2))
                self._iadc_fit_meta = fit2
        except Exception as e:
            self.status_lbl.setText(f"Status: refit failed ({e})")
            return

        self._update_regression_plots()
        self.status_lbl.setText("Status: regression updated from points")

    def _update_regression_plots(self) -> None:
        def set_channel(points_xy: list[tuple[float, float]], m_edit: QtWidgets.QLineEdit, b_edit: QtWidgets.QLineEdit, pts_curve, fit_curve, meta: dict[str, Any] | None) -> None:
            if points_xy:
                xs = [x for x, _ in points_xy]
                ys = [y for _, y in points_xy]
                pts_curve.setData(xs, ys)
            else:
                xs = []
                pts_curve.setData([], [])

            try:
                m = _get_float(m_edit.text())
                b = _get_float(b_edit.text())
            except Exception:
                fit_curve.setData([], [])
                return

            if m is None or b is None:
                fit_curve.setData([], [])
                return

            if not xs:
                fit_curve.setData([], [])
                return

            x_min = min(xs)
            x_max = max(xs)
            if x_max <= x_min:
                x_fit = [x_min - 1.0, x_min + 1.0]
            else:
                span = x_max - x_min
                x_fit = [x_min - 0.05 * span + (0.10 * span * i / 63.0) for i in range(64)]
            y_fit = [self._predict_with_meta(meta, x) if meta else None for x in x_fit]
            if any(v is None for v in y_fit):
                y_fit = [(m * x) + b for x in x_fit]
            else:
                y_fit = [float(v) for v in y_fit if v is not None]
            fit_curve.setData(x_fit, y_fit)

        set_channel(self._points0_xy, self.ch0_m, self.ch0_b, self.p0_points, self.p0_fit, self._ch0_fit_meta)
        set_channel(self._points1_xy, self.ch1_m, self.ch1_b, self.p1_points, self.p1_fit, self._ch1_fit_meta)
        set_channel(self._points2_xy, self.iadc_m, self.iadc_b, self.p2_points, self.p2_fit, self._iadc_fit_meta)

    def _save(self) -> None:
        if self._data is None or self._path is None:
            self.status_lbl.setText("Status: load a file first")
            return

        try:
            ch0_m = _get_float(self.ch0_m.text())
            ch0_b = _get_float(self.ch0_b.text())
            ch1_m = _get_float(self.ch1_m.text())
            ch1_b = _get_float(self.ch1_b.text())
            iadc_m = _get_float(self.iadc_m.text())
            iadc_b = _get_float(self.iadc_b.text())
            iadc_zero = _get_float(self.iadc_zero.text())
            ch0_zero = _get_float(self.ch0_zero.text())
            ch1_zero = _get_float(self.ch1_zero.text())
        except Exception as e:
            self.status_lbl.setText(f"Status: invalid number ({e})")
            return

        self._data.setdefault("ch0", {})
        self._data.setdefault("ch1", {})
        self._data.setdefault("version", 1)
        self._data["full_mass_kg"] = self._data.get("full_mass_kg", DEFAULT_FULL_MASS_KG)
        self._data["ch0"]["m"] = ch0_m
        self._data["ch0"]["b"] = ch0_b
        self._data["ch1"]["m"] = ch1_m
        self._data["ch1"]["b"] = ch1_b
        self._data.setdefault("iadc", {})
        self._data["iadc"]["m"] = iadc_m
        self._data["iadc"]["b"] = iadc_b
        self._data["iadc_zero_raw"] = iadc_zero
        self._data["ch0_zero_raw"] = ch0_zero
        self._data["ch1_zero_raw"] = ch1_zero
        self._data["points"] = [{"kg": kg, "ch0_raw": raw} for raw, kg in self._points0_xy]
        self._data["points_ch1"] = [{"kg": kg, "ch1_raw": raw} for raw, kg in self._points1_xy]
        self._data["points_iadc"] = [{"expected": v, "iadc_raw": raw} for raw, v in self._points2_xy]
        self._data["weights_kg"] = sorted(
            {kg for _, kg in self._points0_xy} | {kg for _, kg in self._points1_xy} | {v for _, v in self._points2_xy}
        )
        if isinstance(self._ch0_fit_meta, dict):
            self._data["ch0_fit"] = self._ch0_fit_meta
        if isinstance(self._ch1_fit_meta, dict):
            self._data["ch1_fit"] = self._ch1_fit_meta
        if isinstance(self._iadc_fit_meta, dict):
            self._data["iadc_fit"] = self._iadc_fit_meta

        try:
            if self.backup_chk.isChecked():
                backup_path = self._path.with_suffix(self._path.suffix + ".bak")
                backup_path.write_text(self._path.read_text(encoding="utf-8"), encoding="utf-8")
            _write_json(self._path, self._data)
        except Exception as e:
            self.status_lbl.setText(f"Status: save failed ({e})")
            return

        self.status_lbl.setText(f"Status: saved {self._path}")

    def closeEvent(self, event) -> None:  # type: ignore[override]
        try:
            if self._raw_timer is not None:
                self._raw_timer.stop()
        except Exception:
            pass
        try:
            if self._raw_sock is not None:
                self._raw_sock.close()
        except Exception:
            pass
        super().closeEvent(event)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("calibration_file", nargs="?", default=None)
    parser.add_argument("--raw-port", type=int, default=None)
    args = parser.parse_args()

    app = QtWidgets.QApplication(sys.argv)
    win = CalibrationEditor(initial_file=args.calibration_file, raw_port=args.raw_port)
    win.show()
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
