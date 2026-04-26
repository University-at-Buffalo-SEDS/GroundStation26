#!/usr/bin/env python3
import argparse
import json
import sys
from pathlib import Path

try:
    import tkinter as tk
    from tkinter import colorchooser, filedialog, messagebox, ttk
except ImportError:
    print(
        "Error: Tkinter is not available in this Python environment.\n"
        "On macOS, install a Python build that includes Tk support (e.g., python.org).",
        file=sys.stderr,
    )
    sys.exit(1)

EXPECTED_BOARD_OPTIONS = [
    ("Flight Computer (FC)", "FC"),
    ("RF Board (RF)", "RF"),
    ("Power Board (PB)", "PB"),
    ("Valve Board (VB)", "VB"),
    ("Gateway Board (GB)", "GB"),
    ("Actuator Board (AB)", "AB"),
    ("DAQ Board (DAQ)", "DAQ"),
]


def known_layout_paths() -> dict[str, Path]:
    backend_dir = Path(__file__).resolve().parents[1]
    layout_dir = backend_dir / "layout"
    return {
        "default": layout_dir / "layout.json",
        "hitl": layout_dir / "layout_hitl.json",
        "test_fire": layout_dir / "layout_test_fire.json",
    }


def default_layout_path() -> Path:
    return known_layout_paths()["default"]


def default_fill_targets_path() -> Path:
    backend_dir = Path(__file__).resolve().parents[1]
    return backend_dir / "config" / "fill_targets.json"


def default_fill_targets() -> dict:
    return {
        "version": 1,
        "nitrogen": {
            "target_mass_kg": 10.0,
            "target_pressure_psi": 120.0,
        },
        "nitrous": {
            "target_mass_kg": 10.0,
            "target_pressure_psi": 745.0,
        },
    }


def default_layout() -> dict:
    return {
        "version": 1,
        "branding": {
            "app_name": None,
            "dashboard_title": None,
            "tab_labels": {},
        },
        "theme": {
            "app_background": "#020617",
            "panel_background": "#0b1220",
            "panel_background_alt": "#0f172a",
            "overlay_background": "#020617ee",
            "border": "#334155",
            "border_strong": "#4b5563",
            "border_soft": "#1f2937",
            "text_primary": "#e5e7eb",
            "text_secondary": "#cbd5e1",
            "text_muted": "#94a3b8",
            "text_soft": "#9ca3af",
            "button_background": "#111827",
            "button_border": "#334155",
            "button_text": "#e5e7eb",
            "tab_shell_background": "#020617ee",
            "tab_shell_border": "#4b5563",
            "info_accent": "#60a5fa",
            "info_background": "#0b1a33",
            "info_text": "#bfdbfe",
            "success_text": "#22c55e",
            "warning_background": "#451a03",
            "warning_border": "#f59e0b",
            "warning_text": "#fde68a",
            "error_background": "#450a0a",
            "error_border": "#ef4444",
            "error_text": "#fecaca",
            "notification_background": "#0b1f4d",
            "notification_border": "#2563eb",
            "notification_text": "#bfdbfe",
            "main_tab_accents": {
                "state": "#38bdf8",
                "connection-status": "#06b6d4",
                "detailed": "#0ea5e9",
                "map": "#22c55e",
                "actions": "#a78bfa",
                "calibration": "#14b8a6",
                "notifications": "#3b82f6",
                "warnings": "#facc15",
                "errors": "#ef4444",
                "data": "#f97316",
                "network-topology": "#8b5cf6"
            }
        },
        "main_tabs": [
            "state",
            "connection-status",
            "map",
            "actions",
            "calibration",
            "notifications",
            "warnings",
            "errors",
            "data",
            "detailed",
            "network-topology",
        ],
        "connection_tab": {"sections": []},
        "network_tab": {
            "enabled": False,
            "title": "SEDSprintf Network",
            "expected_boards": ["FC", "RF", "PB", "VB", "GB", "AB", "DAQ"],
        },
        "actions_tab": {
            "disable_actions_by_default": False,
            "show_flight_setup": True,
            "show_fill_targets": True,
            "fill_targets_require_actions_enabled": True,
            "actions": [],
        },
        "data_tab": {"sender_split_data_types": [], "tabs": []},
        "state_tab": {"states": []},
        "battery": {
            "estimator": {"window_seconds": 300, "min_drop_rate_v_per_min": 0.005},
            "sources": [],
        },
    }


def validate_layout(data: dict) -> list[str]:
    errors: list[str] = []
    if not isinstance(data, dict):
        return ["Layout root must be an object."]

    for key in (
            "version",
            "main_tabs",
            "connection_tab",
            "network_tab",
            "actions_tab",
            "data_tab",
            "state_tab",
    ):
        if key not in data:
            errors.append(f"Missing top-level key: {key}")

    main_tabs = data.get("main_tabs", [])
    if not isinstance(main_tabs, list):
        errors.append("main_tabs must be a list.")

    network = data.get("network_tab", {})
    if not isinstance(network, dict):
        errors.append("network_tab must be an object.")
    elif not isinstance(network.get("expected_boards", []), list):
        errors.append("network_tab.expected_boards must be a list.")

    connection = data.get("connection_tab", {})
    if not isinstance(connection, dict) or not isinstance(connection.get("sections", []), list):
        errors.append("connection_tab.sections must be a list.")

    actions = data.get("actions_tab", {})
    if not isinstance(actions, dict) or not isinstance(actions.get("actions", []), list):
        errors.append("actions_tab.actions must be a list.")

    data_tab = data.get("data_tab", {})
    if not isinstance(data_tab, dict) or not isinstance(data_tab.get("tabs", []), list):
        errors.append("data_tab.tabs must be a list.")
    elif not isinstance(data_tab.get("sender_split_data_types", []), list):
        errors.append("data_tab.sender_split_data_types must be a list.")

    state_tab = data.get("state_tab", {})
    if not isinstance(state_tab, dict) or not isinstance(state_tab.get("states", []), list):
        errors.append("state_tab.states must be a list.")

    battery = data.get("battery", {})
    if battery and (not isinstance(battery, dict) or not isinstance(battery.get("sources", []), list)):
        errors.append("battery.sources must be a list.")

    return errors


class LayoutEditor(tk.Tk):
    def __init__(self, initial_path: Path | None = None) -> None:
        super().__init__()
        self.title("GS26 Layout Editor")
        self.geometry("1100x720")

        self.layout_presets = known_layout_paths()
        self.path = initial_path or default_layout_path()
        self.fill_targets_path = default_fill_targets_path()
        self.data = default_layout()
        self.fill_targets_data = default_fill_targets()

        toolbar = tk.Frame(self)
        toolbar.pack(fill=tk.X, padx=8, pady=8)

        tk.Button(toolbar, text="Load", command=self.load).pack(side=tk.LEFT, padx=4)
        tk.Button(toolbar, text="Save", command=self.save).pack(side=tk.LEFT, padx=4)
        tk.Button(toolbar, text="Save As", command=self.save_as).pack(side=tk.LEFT, padx=4)
        tk.Button(toolbar, text="Validate", command=self.validate).pack(side=tk.LEFT, padx=4)
        ttk.Label(toolbar, text="Preset").pack(side=tk.LEFT, padx=(16, 4))
        self.layout_preset_var = tk.StringVar(value=self._preset_name_for_path(self.path))
        preset_choices = list(self.layout_presets.keys())
        ttk.OptionMenu(
            toolbar,
            self.layout_preset_var,
            self.layout_preset_var.get(),
            *preset_choices,
            command=self._switch_layout_preset,
        ).pack(side=tk.LEFT, padx=4)
        tk.Button(toolbar, text="Browse Layout…", command=self._browse_layout).pack(side=tk.LEFT, padx=4)

        self.status = tk.StringVar(value=f"Layout path: {self.path}")
        tk.Label(self, textvariable=self.status, anchor="w").pack(fill=tk.X, padx=8)

        self.notebook = ttk.Notebook(self)
        self.notebook.pack(fill=tk.BOTH, expand=True, padx=8, pady=8)

        self.main_tabs_frame = ttk.Frame(self.notebook)
        self.data_tab_frame = ttk.Frame(self.notebook)
        self.connection_tab_frame = ttk.Frame(self.notebook)
        self.network_tab_frame = ttk.Frame(self.notebook)
        self.actions_tab_frame = ttk.Frame(self.notebook)
        self.fill_targets_tab_frame = ttk.Frame(self.notebook)
        self.state_tab_frame = ttk.Frame(self.notebook)
        self.battery_tab_frame = ttk.Frame(self.notebook)

        self.notebook.add(self.main_tabs_frame, text="Dashboard Tabs")
        self.notebook.add(self.data_tab_frame, text="Data")
        self.notebook.add(self.connection_tab_frame, text="Connection")
        self.notebook.add(self.network_tab_frame, text="Network")
        self.notebook.add(self.actions_tab_frame, text="Actions")
        self.notebook.add(self.fill_targets_tab_frame, text="Fill Targets")
        self.notebook.add(self.state_tab_frame, text="State Layout")
        self.notebook.add(self.battery_tab_frame, text="Battery")

        self._suspend_events = False
        self._section_title_loaded = ""
        self._build_main_tabs_tab()
        self._build_data_tab()
        self._build_connection_tab()
        self._build_network_tab()
        self._build_actions_tab()
        self._build_fill_targets_tab()
        self._build_state_tab()
        self._build_battery_tab()

        self.load()
        self.after(100, self.focus_force)
        self.notebook.bind("<<NotebookTabChanged>>", self._on_tab_changed)

    def _preset_name_for_path(self, path: Path) -> str:
        resolved = path.resolve()
        for name, preset_path in self.layout_presets.items():
            try:
                if resolved == preset_path.resolve():
                    return name
            except FileNotFoundError:
                if str(resolved) == str(preset_path):
                    return name
        return "default"

    def _switch_layout_preset(self, preset_name: str) -> None:
        preset_path = self.layout_presets.get(preset_name)
        if preset_path is None:
            return
        self._commit_current_tab()
        self.path = preset_path
        self.status.set(f"Layout path: {self.path}")
        self.load()

    def _browse_layout(self) -> None:
        filename = filedialog.askopenfilename(
            title="Open layout JSON",
            filetypes=[("JSON files", "*.json")],
            initialdir=str(default_layout_path().parent),
        )
        if not filename:
            return
        self._commit_current_tab()
        self.path = Path(filename)
        self.layout_preset_var.set(self._preset_name_for_path(self.path))
        self.load()

    # ------------------------
    # Dashboard tabs editor
    # ------------------------
    def _build_main_tabs_tab(self) -> None:
        frame = self.main_tabs_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)
        self._main_tabs_selected_idx: int | None = None
        self.available_main_tabs = [
            "state",
            "connection-status",
            "map",
            "actions",
            "calibration",
            "notifications",
            "warnings",
            "errors",
            "data",
            "detailed",
            "network-topology",
        ]

        self.main_tabs_list = tk.Listbox(frame, height=16)
        self.main_tabs_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.main_tabs_list.bind("<<ListboxSelect>>", lambda _: self._on_main_tab_select())

        form = ttk.Frame(frame)
        form.grid(row=0, column=1, sticky="nsew")
        form.columnconfigure(1, weight=1)

        ttk.Label(form, text="Available dashboard tab").grid(row=0, column=0, sticky="w")
        self.main_tab_choice = tk.StringVar(value=self.available_main_tabs[0])
        ttk.OptionMenu(form, self.main_tab_choice, self.available_main_tabs[0], *self.available_main_tabs).grid(
            row=0, column=1, sticky="w", padx=6, pady=3
        )

        ttk.Label(
            form,
            text="The runtime dashboard nav follows this list order.",
            foreground="#94a3b8",
        ).grid(row=1, column=0, columnspan=2, sticky="w", pady=(0, 8))

        self.layout_version = self._entry(form, "Layout version", 2)
        self.branding_app_name = self._entry(form, "App name", 3)
        self.branding_dashboard_title = self._entry(form, "Dashboard title", 4)

        ttk.Label(form, text="Tab labels").grid(row=5, column=0, sticky="nw")
        self.tab_label_entries: dict[str, ttk.Entry] = {}
        tab_labels_frame = ttk.Frame(form)
        tab_labels_frame.grid(row=5, column=1, sticky="ew", padx=6, pady=3)
        tab_labels_frame.columnconfigure(1, weight=1)
        for idx, tab_id in enumerate(self.available_main_tabs):
            ttk.Label(tab_labels_frame, text=tab_id).grid(row=idx, column=0, sticky="w")
            entry = ttk.Entry(tab_labels_frame)
            entry.grid(row=idx, column=1, sticky="ew", padx=6, pady=2)
            self.tab_label_entries[tab_id] = entry

        btns = ttk.Frame(form)
        btns.grid(row=6, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_main_tab_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Remove", command=self._remove_main_tab_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Up", command=lambda: self._move_main_tab_item(-1)).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Down", command=lambda: self._move_main_tab_item(1)).pack(side=tk.LEFT, padx=4)

        self.theme_entries: dict[str, ttk.Entry] = {}
        theme_fields = [
            ("App background", "app_background"),
            ("Panel background", "panel_background"),
            ("Panel alt background", "panel_background_alt"),
            ("Overlay background", "overlay_background"),
            ("Border", "border"),
            ("Strong border", "border_strong"),
            ("Soft border", "border_soft"),
            ("Primary text", "text_primary"),
            ("Secondary text", "text_secondary"),
            ("Muted text", "text_muted"),
            ("Soft text", "text_soft"),
            ("Button background", "button_background"),
            ("Button border", "button_border"),
            ("Button text", "button_text"),
            ("Tab shell background", "tab_shell_background"),
            ("Tab shell border", "tab_shell_border"),
            ("Info accent", "info_accent"),
            ("Info background", "info_background"),
            ("Info text", "info_text"),
            ("Success text", "success_text"),
            ("Warning background", "warning_background"),
            ("Warning border", "warning_border"),
            ("Warning text", "warning_text"),
            ("Error background", "error_background"),
            ("Error border", "error_border"),
            ("Error text", "error_text"),
            ("Notification background", "notification_background"),
            ("Notification border", "notification_border"),
            ("Notification text", "notification_text"),
        ]
        self.theme_frame = ttk.LabelFrame(form, text="Theme")
        self.theme_frame.grid(row=7, column=0, columnspan=2, sticky="nsew", pady=8)
        for col in range(6):
            self.theme_frame.columnconfigure(col, weight=1)
        for idx, (label, key) in enumerate(theme_fields):
            row = idx // 2
            col = (idx % 2) * 2
            entry = self._color_entry_at(self.theme_frame, label, row, col * 2)
            self.theme_entries[key] = entry

        ttk.Label(self.theme_frame, text="Main tab accents").grid(row=15, column=0, sticky="nw")
        self.theme_tab_accent_entries: dict[str, ttk.Entry] = {}
        accents_frame = ttk.Frame(self.theme_frame)
        accents_frame.grid(row=15, column=1, columnspan=3, sticky="ew", padx=6, pady=3)
        accents_frame.columnconfigure(1, weight=1)
        for idx, tab_id in enumerate(self.available_main_tabs):
            ttk.Label(accents_frame, text=tab_id).grid(row=idx, column=0, sticky="w")
            entry = ttk.Entry(accents_frame)
            entry.grid(row=idx, column=1, sticky="ew", padx=6, pady=2)
            ttk.Button(
                accents_frame, text="Pick", command=lambda e=entry: self._pick_color(e)
            ).grid(row=idx, column=2, sticky="w", padx=(0, 6))
            self.theme_tab_accent_entries[tab_id] = entry

    # ------------------------
    # Data tab editor
    # ------------------------
    def _build_data_tab(self) -> None:
        frame = self.data_tab_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)
        self._data_selected_idx: int | None = None
        self._data_subtab_selected_idx: int | None = None
        self._data_chart_group_selected_idx: int | None = None
        self._data_summary_item_selected_idx: int | None = None

        self.data_list = tk.Listbox(frame, height=20)
        self.data_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.data_list.bind("<<ListboxSelect>>", lambda _: self._on_data_select())

        form = ttk.Frame(frame)
        form.grid(row=0, column=1, sticky="nsew")
        form.columnconfigure(1, weight=1)

        self.data_id = self._entry(form, "ID", 0)
        self.data_label = self._entry(form, "Label", 1)
        self.data_channels = self._entry(form, "Channels (comma)", 2)
        self.data_sender_split_types = self._entry(form, "Sender-split data types", 3)
        self.data_chart = tk.BooleanVar(value=True)
        ttk.Checkbutton(form, text="Chart enabled", variable=self.data_chart).grid(
            row=4, column=1, sticky="w", pady=(6, 6)
        )
        self.data_is_valve = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Has labels", variable=self.data_is_valve, command=self._sync_data_bool_fields).grid(
            row=5, column=1, sticky="w", pady=(0, 6)
        )
        self.data_bool_true_label = ttk.Label(form, text="True label")
        self.data_bool_true_label.grid(row=6, column=0, sticky="w")
        self.data_bool_true = ttk.Entry(form)
        self.data_bool_true.grid(row=6, column=1, columnspan=2, sticky="ew", padx=6, pady=3)

        self.data_bool_false_label = ttk.Label(form, text="False label")
        self.data_bool_false_label.grid(row=7, column=0, sticky="w")
        self.data_bool_false = ttk.Entry(form)
        self.data_bool_false.grid(row=7, column=1, columnspan=2, sticky="ew", padx=6, pady=3)

        self.data_bool_unknown_label = ttk.Label(form, text="Unknown label")
        self.data_bool_unknown_label.grid(row=8, column=0, sticky="w")
        self.data_bool_unknown = ttk.Entry(form)
        self.data_bool_unknown.grid(row=8, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.data_bool_per_channel_label = ttk.Label(
            form, text="Per-channel labels (true,false,unknown | ...)"
        )
        self.data_bool_per_channel_label.grid(row=9, column=0, sticky="w")
        self.data_bool_per_channel = ttk.Entry(form)
        self.data_bool_per_channel.grid(row=9, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.data_bool_per_channel_hint = ttk.Label(
            form,
            text="Example: Open,Closed,Unknown | Installed,Removed,Unknown",
            foreground="#94a3b8",
        )
        self.data_bool_per_channel_hint.grid(row=10, column=1, columnspan=2, sticky="w", padx=6)

        self.data_channel_formatters_frame = ttk.LabelFrame(form, text="Channel formatters")
        self.data_channel_formatters_frame.grid(row=11, column=0, columnspan=3, sticky="ew", padx=6, pady=6)
        for col in range(4):
            self.data_channel_formatters_frame.columnconfigure(col, weight=1)
        self.data_formatter_channels = tk.Listbox(self.data_channel_formatters_frame, height=5)
        self.data_formatter_channels.grid(row=0, column=0, rowspan=5, sticky="nsew", padx=6, pady=3)
        self.data_formatter_channels.bind("<<ListboxSelect>>", lambda _: self._on_data_formatter_select())
        ttk.Label(self.data_channel_formatters_frame, text="Format kind").grid(row=0, column=1, sticky="w")
        self.data_formatter_kind = tk.StringVar(value="")
        ttk.OptionMenu(
            self.data_channel_formatters_frame,
            self.data_formatter_kind,
            "",
            "",
            "number",
            "integer",
        ).grid(row=0, column=2, sticky="w", padx=6, pady=3)
        self.data_formatter_precision = self._entry(self.data_channel_formatters_frame, "Precision", 1, col=1)
        self.data_formatter_prefix = self._entry(self.data_channel_formatters_frame, "Prefix", 2, col=1)
        self.data_formatter_suffix = self._entry(self.data_channel_formatters_frame, "Suffix", 3, col=1)
        data_formatter_btns = ttk.Frame(self.data_channel_formatters_frame)
        data_formatter_btns.grid(row=4, column=1, columnspan=3, sticky="w", pady=4)
        ttk.Button(data_formatter_btns, text="Apply", command=self._apply_data_formatter).pack(side=tk.LEFT, padx=4)
        ttk.Button(data_formatter_btns, text="Clear", command=self._clear_data_formatter).pack(side=tk.LEFT, padx=4)
        ttk.Button(data_formatter_btns, text="Sync Channels", command=self._sync_data_formatter_channels).pack(
            side=tk.LEFT, padx=4)
        self._data_channel_formatters: list[dict | None] = []
        self._data_formatter_selected_idx: int | None = None

        btns = ttk.Frame(form)
        btns.grid(row=12, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_data_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Remove", command=self._remove_data_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Up", command=lambda: self._move_data_item(-1)).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Down", command=lambda: self._move_data_item(1)).pack(
            side=tk.LEFT, padx=4
        )

        self.data_subtabs_frame = ttk.LabelFrame(form, text="Subtabs")
        self.data_subtabs_frame.grid(row=13, column=0, columnspan=3, sticky="ew", padx=6, pady=(8, 4))
        for col in range(4):
            self.data_subtabs_frame.columnconfigure(col, weight=1)
        self.data_subtabs_list = tk.Listbox(self.data_subtabs_frame, height=5)
        self.data_subtabs_list.grid(row=0, column=0, rowspan=7, sticky="nsew", padx=6, pady=3)
        self.data_subtabs_list.bind("<<ListboxSelect>>", lambda _: self._on_data_subtab_select())
        self.data_subtab_id = self._entry(self.data_subtabs_frame, "Subtab ID", 0, col=1)
        self.data_subtab_label = self._entry(self.data_subtabs_frame, "Subtab Label", 1, col=1)
        self.data_subtab_data_type = self._entry(self.data_subtabs_frame, "Data type", 2, col=1)
        self.data_subtab_sender_id = self._entry(self.data_subtabs_frame, "Sender ID", 3, col=1)
        self.data_subtab_channels = self._entry(self.data_subtabs_frame, "Channels (comma)", 4, col=1)
        self.data_subtab_chart = tk.BooleanVar(value=True)
        ttk.Checkbutton(self.data_subtabs_frame, text="Chart enabled", variable=self.data_subtab_chart).grid(
            row=5, column=1, sticky="w", padx=6, pady=3
        )
        self.data_subtab_has_labels = tk.BooleanVar(value=False)
        ttk.Checkbutton(
            self.data_subtabs_frame,
            text="Has labels",
            variable=self.data_subtab_has_labels,
            command=self._sync_data_subtab_bool_fields,
        ).grid(row=5, column=2, sticky="w", padx=6, pady=3)
        self.data_subtab_bool_true_label = ttk.Label(self.data_subtabs_frame, text="True label")
        self.data_subtab_bool_true_label.grid(row=6, column=1, sticky="w")
        self.data_subtab_bool_true = ttk.Entry(self.data_subtabs_frame)
        self.data_subtab_bool_true.grid(row=6, column=2, sticky="ew", padx=6, pady=3)
        self.data_subtab_bool_false_label = ttk.Label(self.data_subtabs_frame, text="False label")
        self.data_subtab_bool_false_label.grid(row=7, column=1, sticky="w")
        self.data_subtab_bool_false = ttk.Entry(self.data_subtabs_frame)
        self.data_subtab_bool_false.grid(row=7, column=2, sticky="ew", padx=6, pady=3)
        self.data_subtab_bool_unknown_label = ttk.Label(self.data_subtabs_frame, text="Unknown label")
        self.data_subtab_bool_unknown_label.grid(row=8, column=1, sticky="w")
        self.data_subtab_bool_unknown = ttk.Entry(self.data_subtabs_frame)
        self.data_subtab_bool_unknown.grid(row=8, column=2, sticky="ew", padx=6, pady=3)
        self.data_subtab_bool_per_channel_label = ttk.Label(
            self.data_subtabs_frame, text="Per-channel labels (true,false,unknown | ...)"
        )
        self.data_subtab_bool_per_channel_label.grid(row=9, column=1, sticky="w")
        self.data_subtab_bool_per_channel = ttk.Entry(self.data_subtabs_frame)
        self.data_subtab_bool_per_channel.grid(row=9, column=2, sticky="ew", padx=6, pady=3)
        self.data_subtab_bool_per_channel_hint = ttk.Label(
            self.data_subtabs_frame,
            text="Example: Open,Closed,Unknown | Installed,Removed,Unknown",
            foreground="#94a3b8",
        )
        self.data_subtab_bool_per_channel_hint.grid(row=10, column=2, sticky="w", padx=6)

        self.data_subtab_channel_formatters_frame = ttk.LabelFrame(self.data_subtabs_frame, text="Channel formatters")
        self.data_subtab_channel_formatters_frame.grid(row=11, column=1, columnspan=3, sticky="ew", padx=6, pady=6)
        for col in range(4):
            self.data_subtab_channel_formatters_frame.columnconfigure(col, weight=1)
        self.data_subtab_formatter_channels = tk.Listbox(self.data_subtab_channel_formatters_frame, height=4)
        self.data_subtab_formatter_channels.grid(row=0, column=0, rowspan=5, sticky="nsew", padx=6, pady=3)
        self.data_subtab_formatter_channels.bind(
            "<<ListboxSelect>>", lambda _: self._on_data_subtab_formatter_select()
        )
        ttk.Label(self.data_subtab_channel_formatters_frame, text="Format kind").grid(row=0, column=1, sticky="w")
        self.data_subtab_formatter_kind = tk.StringVar(value="")
        ttk.OptionMenu(
            self.data_subtab_channel_formatters_frame,
            self.data_subtab_formatter_kind,
            "",
            "",
            "number",
            "integer",
        ).grid(row=0, column=2, sticky="w", padx=6, pady=3)
        self.data_subtab_formatter_precision = self._entry(
            self.data_subtab_channel_formatters_frame, "Precision", 1, col=1
        )
        self.data_subtab_formatter_prefix = self._entry(
            self.data_subtab_channel_formatters_frame, "Prefix", 2, col=1
        )
        self.data_subtab_formatter_suffix = self._entry(
            self.data_subtab_channel_formatters_frame, "Suffix", 3, col=1
        )
        data_subtab_formatter_btns = ttk.Frame(self.data_subtab_channel_formatters_frame)
        data_subtab_formatter_btns.grid(row=4, column=1, columnspan=3, sticky="w", pady=4)
        ttk.Button(
            data_subtab_formatter_btns, text="Apply", command=self._apply_data_subtab_formatter
        ).pack(side=tk.LEFT, padx=4)
        ttk.Button(
            data_subtab_formatter_btns, text="Clear", command=self._clear_data_subtab_formatter
        ).pack(side=tk.LEFT, padx=4)
        ttk.Button(
            data_subtab_formatter_btns, text="Sync Channels", command=self._sync_data_subtab_formatter_channels
        ).pack(side=tk.LEFT, padx=4)
        self._data_subtab_channel_formatters: list[dict | None] = []
        self._data_subtab_formatter_selected_idx: int | None = None
        subtab_btns = ttk.Frame(self.data_subtabs_frame)
        subtab_btns.grid(row=12, column=1, columnspan=3, sticky="w", pady=4)
        ttk.Button(subtab_btns, text="Add", command=self._add_data_subtab).pack(side=tk.LEFT, padx=4)
        ttk.Button(subtab_btns, text="Update", command=self._update_data_subtab).pack(side=tk.LEFT, padx=4)
        ttk.Button(subtab_btns, text="Remove", command=self._remove_data_subtab).pack(side=tk.LEFT, padx=4)
        ttk.Button(subtab_btns, text="Up", command=lambda: self._move_data_subtab(-1)).pack(side=tk.LEFT, padx=4)
        ttk.Button(subtab_btns, text="Down", command=lambda: self._move_data_subtab(1)).pack(side=tk.LEFT, padx=4)
        ttk.Button(subtab_btns, text="Clear Editor", command=self._clear_data_subtab_editor).pack(side=tk.LEFT, padx=4)

        self.data_chart_groups_frame = ttk.LabelFrame(form, text="Chart groups")
        self.data_chart_groups_frame.grid(row=14, column=0, columnspan=3, sticky="ew", padx=6, pady=(8, 4))
        for col in range(4):
            self.data_chart_groups_frame.columnconfigure(col, weight=1)
        self.data_chart_group_scope = tk.StringVar(value="Tab")
        ttk.Label(self.data_chart_groups_frame, textvariable=self.data_chart_group_scope).grid(
            row=0, column=0, columnspan=2, sticky="w", padx=6
        )
        self.data_chart_groups_list = tk.Listbox(self.data_chart_groups_frame, height=5)
        self.data_chart_groups_list.grid(row=1, column=0, rowspan=7, sticky="nsew", padx=6, pady=3)
        self.data_chart_groups_list.bind("<<ListboxSelect>>", lambda _: self._on_data_chart_group_select())
        self.data_chart_group_title = self._entry(self.data_chart_groups_frame, "Title", 1, col=1)
        self.data_chart_group_data_type = self._entry(self.data_chart_groups_frame, "Data type", 2, col=1)
        self.data_chart_group_sender_id = self._entry(self.data_chart_groups_frame, "Sender ID", 3, col=1)
        self.data_chart_group_labels = self._entry(self.data_chart_groups_frame, "Labels (comma)", 4, col=1)
        self.data_chart_group_channels = self._entry(self.data_chart_groups_frame, "Channels (comma)", 5, col=1)
        ttk.Label(self.data_chart_groups_frame, text="Scale mode").grid(row=6, column=1, sticky="w")
        self.data_chart_group_scale_mode = tk.StringVar(value="")
        ttk.OptionMenu(
            self.data_chart_groups_frame,
            self.data_chart_group_scale_mode,
            "",
            "",
            "shared",
            "per_series",
        ).grid(row=6, column=2, sticky="w", padx=6, pady=3)
        chart_group_btns = ttk.Frame(self.data_chart_groups_frame)
        chart_group_btns.grid(row=7, column=1, columnspan=3, sticky="w", pady=4)
        ttk.Button(chart_group_btns, text="Add", command=self._add_data_chart_group).pack(side=tk.LEFT, padx=4)
        ttk.Button(chart_group_btns, text="Update", command=self._update_data_chart_group).pack(side=tk.LEFT, padx=4)
        ttk.Button(chart_group_btns, text="Remove", command=self._remove_data_chart_group).pack(side=tk.LEFT, padx=4)
        ttk.Button(chart_group_btns, text="Up", command=lambda: self._move_data_chart_group(-1)).pack(side=tk.LEFT,
                                                                                                      padx=4)
        ttk.Button(chart_group_btns, text="Down", command=lambda: self._move_data_chart_group(1)).pack(side=tk.LEFT,
                                                                                                       padx=4)
        ttk.Button(chart_group_btns, text="Clear Editor", command=self._clear_data_chart_group_editor).pack(
            side=tk.LEFT, padx=4)

        self.data_summary_items_frame = ttk.LabelFrame(form, text="Summary items")
        self.data_summary_items_frame.grid(row=15, column=0, columnspan=3, sticky="ew", padx=6, pady=(8, 4))
        for col in range(4):
            self.data_summary_items_frame.columnconfigure(col, weight=1)
        self.data_summary_item_scope = tk.StringVar(value="Select a subtab to edit summary items")
        ttk.Label(self.data_summary_items_frame, textvariable=self.data_summary_item_scope).grid(
            row=0, column=0, columnspan=2, sticky="w", padx=6
        )
        self.data_summary_items_list = tk.Listbox(self.data_summary_items_frame, height=5)
        self.data_summary_items_list.grid(row=1, column=0, rowspan=8, sticky="nsew", padx=6, pady=3)
        self.data_summary_items_list.bind("<<ListboxSelect>>", lambda _: self._on_data_summary_item_select())
        self.data_summary_item_label = self._entry(self.data_summary_items_frame, "Label", 1, col=1)
        self.data_summary_item_data_type = self._entry(self.data_summary_items_frame, "Data type", 2, col=1)
        self.data_summary_item_index = self._entry(self.data_summary_items_frame, "Index", 3, col=1)
        self.data_summary_item_sender_id = self._entry(self.data_summary_items_frame, "Sender ID", 4, col=1)
        self.data_summary_item_true = self._entry(self.data_summary_items_frame, "True label", 5, col=1)
        self.data_summary_item_false = self._entry(self.data_summary_items_frame, "False label", 6, col=1)
        self.data_summary_item_unknown = self._entry(self.data_summary_items_frame, "Unknown label", 7, col=1)
        ttk.Label(self.data_summary_items_frame, text="Format kind").grid(row=1, column=2, sticky="w")
        self.data_summary_item_format_kind = tk.StringVar(value="")
        ttk.OptionMenu(
            self.data_summary_items_frame,
            self.data_summary_item_format_kind,
            "",
            "",
            "number",
            "integer",
        ).grid(row=1, column=3, sticky="w", padx=6, pady=3)
        self.data_summary_item_precision = self._entry(self.data_summary_items_frame, "Precision", 2, col=2)
        self.data_summary_item_prefix = self._entry(self.data_summary_items_frame, "Prefix", 3, col=2)
        self.data_summary_item_suffix = self._entry(self.data_summary_items_frame, "Suffix", 4, col=2)
        data_summary_btns = ttk.Frame(self.data_summary_items_frame)
        data_summary_btns.grid(row=8, column=1, columnspan=3, sticky="w", pady=4)
        ttk.Button(data_summary_btns, text="Add", command=self._add_data_summary_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(data_summary_btns, text="Update", command=self._update_data_summary_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(data_summary_btns, text="Remove", command=self._remove_data_summary_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(data_summary_btns, text="Up", command=lambda: self._move_data_summary_item(-1)).pack(side=tk.LEFT,
                                                                                                        padx=4)
        ttk.Button(data_summary_btns, text="Down", command=lambda: self._move_data_summary_item(1)).pack(side=tk.LEFT,
                                                                                                         padx=4)
        ttk.Button(data_summary_btns, text="Clear Editor", command=self._clear_data_summary_item_editor).pack(
            side=tk.LEFT, padx=4)
        self._sync_data_bool_fields()
        self._sync_data_subtab_bool_fields()
        self._sync_data_subtab_formatter_channels()

    # ------------------------
    # Connection tab editor
    # ------------------------
    def _build_connection_tab(self) -> None:
        frame = self.connection_tab_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)
        self._conn_selected_idx: int | None = None

        self.conn_list = tk.Listbox(frame, height=20)
        self.conn_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.conn_list.bind("<<ListboxSelect>>", lambda _: self._on_conn_select())

        form = ttk.Frame(frame)
        form.grid(row=0, column=1, sticky="nsew")
        form.columnconfigure(1, weight=1)

        self.conn_kind = tk.StringVar(value="board_status")
        ttk.Label(form, text="Kind").grid(row=0, column=0, sticky="w")
        ttk.OptionMenu(
            form, self.conn_kind, "board_status", "board_status", "latency"
        ).grid(row=0, column=1, sticky="w")
        self.conn_title = self._entry(form, "Title", 1)

        btns = ttk.Frame(form)
        btns.grid(row=2, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_conn_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Remove", command=self._remove_conn_item).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Up", command=lambda: self._move_conn_item(-1)).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Down", command=lambda: self._move_conn_item(1)).pack(
            side=tk.LEFT, padx=4
        )

    # ------------------------
    # Network tab editor
    # ------------------------
    def _build_network_tab(self) -> None:
        frame = self.network_tab_frame
        frame.columnconfigure(1, weight=1)

        self.network_enabled = tk.BooleanVar(value=False)
        ttk.Checkbutton(frame, text="Enable network tab", variable=self.network_enabled).grid(
            row=0, column=0, columnspan=2, sticky="w", padx=6, pady=(8, 6)
        )
        self.network_title = self._entry(frame, "Title", 1)
        ttk.Label(frame, text="Expected boards").grid(row=2, column=0, sticky="nw", padx=6, pady=(8, 4))
        expected_frame = ttk.Frame(frame)
        expected_frame.grid(row=2, column=1, sticky="w", padx=6, pady=(8, 4))
        self.network_expected_board_vars: dict[str, tk.BooleanVar] = {}
        for idx, (label, sender_id) in enumerate(EXPECTED_BOARD_OPTIONS):
            var = tk.BooleanVar(value=False)
            self.network_expected_board_vars[sender_id] = var
            ttk.Checkbutton(expected_frame, text=label, variable=var).grid(
                row=idx // 2, column=idx % 2, sticky="w", padx=(0, 16), pady=2
            )

    # ------------------------
    # Actions tab editor
    # ------------------------
    def _build_actions_tab(self) -> None:
        frame = self.actions_tab_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)
        self._actions_selected_idx: int | None = None
        self._state_entry_selected_idx: int | None = None
        self._state_section_selected_idx: int | None = None
        self._state_widget_selected_idx: int | None = None

        self.actions_list = tk.Listbox(frame, height=20)
        self.actions_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.actions_list.bind("<<ListboxSelect>>", lambda _: self._on_action_select())

        form = ttk.Frame(frame)
        form.grid(row=0, column=1, sticky="nsew")
        form.columnconfigure(1, weight=1)

        self.disable_actions_by_default = tk.BooleanVar(value=False)
        ttk.Checkbutton(
            form,
            text="Disable actions by default",
            variable=self.disable_actions_by_default,
            command=self._store_actions_defaults,
        ).grid(row=0, column=1, sticky="w", pady=(0, 8))
        self.show_flight_setup = tk.BooleanVar(value=True)
        ttk.Checkbutton(
            form,
            text="Show flight setup controls",
            variable=self.show_flight_setup,
            command=self._store_actions_defaults,
        ).grid(row=1, column=1, sticky="w", pady=(0, 4))
        self.show_fill_targets = tk.BooleanVar(value=True)
        ttk.Checkbutton(
            form,
            text="Show fill targets controls",
            variable=self.show_fill_targets,
            command=self._store_actions_defaults,
        ).grid(row=2, column=1, sticky="w", pady=(0, 4))
        self.fill_targets_require_actions_enabled = tk.BooleanVar(value=True)
        ttk.Checkbutton(
            form,
            text="Require actions enabled for fill targets",
            variable=self.fill_targets_require_actions_enabled,
            command=self._store_actions_defaults,
        ).grid(row=3, column=1, sticky="w", pady=(0, 8))

        self.action_label = self._entry(form, "Label", 4)
        self.action_cmd = self._entry(form, "Command", 5)
        self.action_border = self._color_entry(form, "Border color", 6)
        self.action_bg = self._color_entry(form, "Background color", 7)
        self.action_fg = self._color_entry(form, "Text color", 8)
        self.action_illuminated = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Illuminated", variable=self.action_illuminated).grid(
            row=9, column=1, sticky="w", pady=(0, 4)
        )
        self.action_spacer_before = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Spacer before", variable=self.action_spacer_before).grid(
            row=10, column=1, sticky="w", pady=(0, 4)
        )
        self.action_spacer_after = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Spacer after", variable=self.action_spacer_after).grid(
            row=10, column=2, sticky="w", pady=(0, 4)
        )
        self.action_new_row_before = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="New row before", variable=self.action_new_row_before).grid(
            row=11, column=1, sticky="w", pady=(0, 4)
        )
        self.action_new_row_after = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="New row after", variable=self.action_new_row_after).grid(
            row=11, column=2, sticky="w", pady=(0, 4)
        )
        self.action_spacer_row_before = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Spacer row before", variable=self.action_spacer_row_before).grid(
            row=12, column=1, sticky="w", pady=(0, 4)
        )
        self.action_spacer_row_after = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Spacer row after", variable=self.action_spacer_row_after).grid(
            row=12, column=2, sticky="w", pady=(0, 4)
        )

        btns = ttk.Frame(form)
        btns.grid(row=13, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_action_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Remove", command=self._remove_action_item).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Up", command=lambda: self._move_action_item(-1)).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Down", command=lambda: self._move_action_item(1)).pack(
            side=tk.LEFT, padx=4
        )

    # ------------------------
    # Fill targets editor
    # ------------------------
    def _build_fill_targets_tab(self) -> None:
        frame = self.fill_targets_tab_frame
        frame.columnconfigure(0, weight=1)

        self.fill_targets_status = tk.StringVar(value=f"Fill target path: {self.fill_targets_path}")
        ttk.Label(frame, textvariable=self.fill_targets_status, anchor="w").grid(
            row=0, column=0, sticky="ew", padx=8, pady=(8, 12)
        )

        form = ttk.Frame(frame)
        form.grid(row=1, column=0, sticky="nsew", padx=8)
        for col in range(3):
            form.columnconfigure(col, weight=1)

        ttk.Label(form, text="Fluid").grid(row=0, column=0, sticky="w", padx=6, pady=3)
        ttk.Label(form, text="Target mass (kg)").grid(row=0, column=1, sticky="w", padx=6, pady=3)
        ttk.Label(form, text="Target pressure (psi)").grid(row=0, column=2, sticky="w", padx=6, pady=3)

        ttk.Label(form, text="Nitrogen").grid(row=1, column=0, sticky="w", padx=6, pady=3)
        self.nitrogen_target_mass = ttk.Entry(form)
        self.nitrogen_target_mass.grid(row=1, column=1, sticky="ew", padx=6, pady=3)
        self.nitrogen_target_pressure = ttk.Entry(form)
        self.nitrogen_target_pressure.grid(row=1, column=2, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Nitrous").grid(row=2, column=0, sticky="w", padx=6, pady=3)
        self.nitrous_target_mass = ttk.Entry(form)
        self.nitrous_target_mass.grid(row=2, column=1, sticky="ew", padx=6, pady=3)
        self.nitrous_target_pressure = ttk.Entry(form)
        self.nitrous_target_pressure.grid(row=2, column=2, sticky="ew", padx=6, pady=3)

        btns = ttk.Frame(form)
        btns.grid(row=3, column=1, columnspan=2, sticky="w", pady=8)
        ttk.Button(btns, text="Reload Fill Targets", command=self._load_fill_targets_file).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Save Fill Targets", command=self._save_fill_targets_file).pack(
            side=tk.LEFT, padx=4
        )

    # ------------------------
    # State tab editor
    # ------------------------
    def _build_state_tab(self) -> None:
        frame = self.state_tab_frame
        frame.columnconfigure(2, weight=1)
        frame.rowconfigure(1, weight=1)

        # State entries
        ttk.Label(frame, text="State entries").grid(row=0, column=0, sticky="w")
        self.state_entry_list = tk.Listbox(frame, height=18)
        self.state_entry_list.grid(row=1, column=0, sticky="ns", padx=(0, 10))
        self.state_entry_list.bind("<<ListboxSelect>>", lambda _: self._on_state_entry_select())

        # Sections
        ttk.Label(frame, text="Sections").grid(row=0, column=1, sticky="w")
        self.section_list = tk.Listbox(frame, height=18)
        self.section_list.grid(row=1, column=1, sticky="ns", padx=(0, 10))
        self.section_list.bind("<<ListboxSelect>>", lambda _: self._on_section_select())
        self.section_list.bind("<ButtonRelease-1>", lambda _: self._on_section_select())
        self.section_list.bind("<KeyRelease-Up>", lambda _: self._on_section_select())
        self.section_list.bind("<KeyRelease-Down>", lambda _: self._on_section_select())

        # Widgets
        ttk.Label(frame, text="Widgets").grid(row=0, column=2, sticky="w")
        self.widget_list = tk.Listbox(frame, height=18)
        self.widget_list.grid(row=1, column=2, sticky="nsew")
        self.widget_list.bind("<<ListboxSelect>>", lambda _: self._on_widget_select())
        self.widget_list.bind("<ButtonRelease-1>", lambda _: self._on_widget_select())
        self.widget_list.bind("<KeyRelease-Up>", lambda _: self._on_widget_select())
        self.widget_list.bind("<KeyRelease-Down>", lambda _: self._on_widget_select())

        form = ttk.Frame(frame)
        form.grid(row=2, column=0, columnspan=3, sticky="ew", pady=10)
        for col in range(6):
            form.columnconfigure(col, weight=1)

        # State entry row
        ttk.Label(form, text="States (comma)").grid(row=0, column=0, sticky="w")
        self.state_states = ttk.Entry(form)
        self.state_states.grid(row=0, column=1, columnspan=3, sticky="ew", padx=6, pady=3)
        ttk.Button(form, text="Add State Entry", command=self._add_state_entry).grid(
            row=1, column=1, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Up", command=lambda: self._move_state_entry(-1)).grid(
            row=1, column=2, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Remove State Entry", command=self._remove_state_entry).grid(
            row=1, column=3, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Down", command=lambda: self._move_state_entry(1)).grid(
            row=1, column=4, padx=6, pady=4, sticky="w"
        )

        # Section row
        ttk.Label(form, text="Section title").grid(row=2, column=0, sticky="w")
        self.section_title = ttk.Entry(form)
        self.section_title.grid(row=2, column=1, columnspan=3, sticky="ew", padx=6, pady=3)
        self.section_title.bind("<KeyRelease>", lambda _: self._update_section_title_live())
        ttk.Label(form, text="Value layout").grid(row=2, column=4, sticky="w")
        self.section_value_layout = tk.StringVar(value="auto")
        ttk.OptionMenu(
            form,
            self.section_value_layout,
            "auto",
            "auto",
            "horizontal",
            "vertical",
            command=lambda _: self._update_section_title_live(),
        ).grid(row=2, column=5, sticky="ew", padx=6, pady=3)
        self.section_style_frame = ttk.LabelFrame(form, text="Section style")
        self.section_style_frame.grid(row=3, column=4, rowspan=1, columnspan=2, sticky="nsew", padx=6, pady=3)
        self.section_style_frame.columnconfigure(1, weight=1)
        self.section_bg = self._color_entry(self.section_style_frame, "Background", 0)
        self.section_border = self._color_entry(self.section_style_frame, "Border", 1)
        self.section_title_color = self._color_entry(self.section_style_frame, "Title color", 2)
        ttk.Button(form, text="Add Section", command=self._add_section).grid(
            row=3, column=1, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Up", command=lambda: self._move_section(-1)).grid(
            row=3, column=2, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Remove Section", command=self._remove_section).grid(
            row=3, column=3, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Down", command=lambda: self._move_section(1)).grid(
            row=3, column=4, padx=6, pady=4, sticky="w"
        )

        # Widget row
        ttk.Label(form, text="Widget kind").grid(row=4, column=0, sticky="w")
        self.widget_kind = tk.StringVar(value="summary")
        ttk.OptionMenu(
            form,
            self.widget_kind,
            "summary",
            "board_status",
            "summary",
            "chart",
            "valve_state",
            "map",
            "actions",
        ).grid(row=4, column=1, sticky="w", padx=6, pady=3)

        ttk.Label(form, text="Data type").grid(row=4, column=2, sticky="w")
        self.widget_data_type_label = form.grid_slaves(row=4, column=2)[0]
        self.widget_data_type = ttk.Entry(form)
        self.widget_data_type.grid(row=4, column=3, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Chart title").grid(row=5, column=0, sticky="w")
        self.widget_chart_title_label = form.grid_slaves(row=5, column=0)[0]
        self.widget_chart_title = ttk.Entry(form)
        self.widget_chart_title.grid(row=5, column=1, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Width").grid(row=5, column=2, sticky="w")
        self.widget_width_label = form.grid_slaves(row=5, column=2)[0]
        self.widget_width = ttk.Entry(form)
        self.widget_width.grid(row=5, column=3, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Height").grid(row=6, column=0, sticky="w")
        self.widget_height_label = form.grid_slaves(row=6, column=0)[0]
        self.widget_height = ttk.Entry(form)
        self.widget_height.grid(row=6, column=1, sticky="ew", padx=6, pady=3)
        self.widget_full_width = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Full row", variable=self.widget_full_width).grid(
            row=6, column=2, sticky="w", padx=6, pady=3
        )
        ttk.Label(form, text="Width fraction").grid(row=6, column=3, sticky="w")
        self.widget_width_fraction_label = form.grid_slaves(row=6, column=3)[0]
        self.widget_width_fraction = ttk.Entry(form)
        self.widget_width_fraction.grid(row=6, column=4, sticky="ew", padx=6, pady=3)

        self.chart_series_frame = ttk.LabelFrame(form, text="Chart series")
        self.chart_series_frame.grid(row=7, column=0, columnspan=6, sticky="ew", padx=6, pady=(6, 4))
        for col in range(4):
            self.chart_series_frame.columnconfigure(col, weight=1)
        self.chart_series_list = tk.Listbox(self.chart_series_frame, height=4)
        self.chart_series_list.grid(row=0, column=0, rowspan=5, columnspan=2, sticky="nsew", padx=6, pady=3)
        self.chart_series_list.bind("<<ListboxSelect>>", lambda _: self._on_chart_series_select())
        self.chart_series_data_type = self._entry(self.chart_series_frame, "Data type", 0, col=2)
        self.chart_series_index = self._entry(self.chart_series_frame, "Index", 1, col=2)
        self.chart_series_label = self._entry(self.chart_series_frame, "Legend label", 2, col=2)
        chart_series_btns = ttk.Frame(self.chart_series_frame)
        chart_series_btns.grid(row=5, column=0, columnspan=4, sticky="w", pady=4)
        ttk.Button(chart_series_btns, text="Add", command=self._add_chart_series).pack(side=tk.LEFT, padx=4)
        ttk.Button(chart_series_btns, text="Update", command=self._update_chart_series).pack(side=tk.LEFT, padx=4)
        ttk.Button(chart_series_btns, text="Remove", command=self._remove_chart_series).pack(side=tk.LEFT, padx=4)
        ttk.Button(chart_series_btns, text="Up", command=lambda: self._move_chart_series(-1)).pack(side=tk.LEFT, padx=4)
        ttk.Button(chart_series_btns, text="Down", command=lambda: self._move_chart_series(1)).pack(side=tk.LEFT,
                                                                                                    padx=4)
        ttk.Button(chart_series_btns, text="Clear Editor", command=self._clear_chart_series_editor).pack(side=tk.LEFT,
                                                                                                         padx=4)
        self._chart_series: list[dict] = []
        self._chart_series_selected_idx: int | None = None

        self.summary_style_frame = ttk.LabelFrame(form, text="Summary card style")
        self.summary_style_frame.grid(row=4, column=4, rowspan=4, columnspan=2, sticky="nsew", padx=6, pady=3)
        self.summary_style_frame.columnconfigure(1, weight=1)
        self.summary_bg = self._color_entry(self.summary_style_frame, "Background", 0)
        self.summary_border = self._color_entry(self.summary_style_frame, "Border", 1)
        self.summary_label_color = self._color_entry(self.summary_style_frame, "Label color", 2)
        self.summary_value_color = self._color_entry(self.summary_style_frame, "Value color", 3)

        self.widget_valves_label = ttk.Label(form, text="Valves (label:index,...)")
        self.widget_valves_label.grid(row=8, column=0, sticky="w")
        self.widget_valves = ttk.Entry(form)
        self.widget_valves.grid(row=8, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_true_label = ttk.Label(form, text="Valve true label")
        self.widget_valve_true_label.grid(row=9, column=0, sticky="w")
        self.widget_valve_true = ttk.Entry(form)
        self.widget_valve_true.grid(row=9, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_false_label = ttk.Label(form, text="Valve false label")
        self.widget_valve_false_label.grid(row=10, column=0, sticky="w")
        self.widget_valve_false = ttk.Entry(form)
        self.widget_valve_false.grid(row=10, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_unknown_label_text = ttk.Label(form, text="Valve unknown label")
        self.widget_valve_unknown_label_text.grid(row=11, column=0, sticky="w")
        self.widget_valve_unknown_text = ttk.Entry(form)
        self.widget_valve_unknown_text.grid(row=11, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_open_label = ttk.Label(form, text="Open colors (bg,border,fg)")
        self.widget_valve_open_label.grid(row=12, column=0, sticky="w")
        self.widget_valve_open = ttk.Entry(form)
        self.widget_valve_open.grid(row=12, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.widget_valve_open_btns = ttk.Frame(form)
        self.widget_valve_open_btns.grid(row=12, column=3, sticky="w")
        self._add_valve_color_buttons(self.widget_valve_open_btns, self.widget_valve_open)

        self.widget_valve_closed_label = ttk.Label(form, text="Closed colors (bg,border,fg)")
        self.widget_valve_closed_label.grid(row=13, column=0, sticky="w")
        self.widget_valve_closed = ttk.Entry(form)
        self.widget_valve_closed.grid(row=13, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.widget_valve_closed_btns = ttk.Frame(form)
        self.widget_valve_closed_btns.grid(row=13, column=3, sticky="w")
        self._add_valve_color_buttons(self.widget_valve_closed_btns, self.widget_valve_closed)

        self.widget_valve_unknown_label = ttk.Label(form, text="Unknown colors (bg,border,fg)")
        self.widget_valve_unknown_label.grid(row=14, column=0, sticky="w")
        self.widget_valve_unknown = ttk.Entry(form)
        self.widget_valve_unknown.grid(row=14, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.widget_valve_unknown_btns = ttk.Frame(form)
        self.widget_valve_unknown_btns.grid(row=14, column=3, sticky="w")
        self._add_valve_color_buttons(self.widget_valve_unknown_btns, self.widget_valve_unknown)

        self.widget_valve_labels_label = ttk.Label(
            form, text="Valve labels (true,false,unknown | ...)"
        )
        self.widget_valve_labels_label.grid(row=15, column=0, sticky="w")
        self.widget_valve_labels = ttk.Entry(form)
        self.widget_valve_labels.grid(row=15, column=1, columnspan=3, sticky="ew", padx=6, pady=3)
        self.widget_valve_labels_hint = ttk.Label(
            form,
            text="Example: Open,Closed,Unknown | Installed,Removed,Unknown",
            foreground="#94a3b8",
        )
        self.widget_valve_labels_hint.grid(row=16, column=1, columnspan=3, sticky="w", padx=6)

        self.widget_actions_available_label = ttk.Label(form, text="Available actions")
        self.widget_actions_available_label.grid(row=17, column=0, sticky="w")
        self.widget_actions_available = tk.Listbox(form, height=5, selectmode=tk.MULTIPLE)
        self.widget_actions_available.grid(
            row=18, column=0, columnspan=2, sticky="nsew", padx=6, pady=3
        )

        self.widget_actions_selected_label = ttk.Label(form, text="Selected actions")
        self.widget_actions_selected_label.grid(row=17, column=2, sticky="w")
        self.widget_actions_selected = tk.Listbox(form, height=5)
        self.widget_actions_selected.grid(
            row=18, column=2, columnspan=2, sticky="nsew", padx=6, pady=3
        )
        self.widget_actions_labels = [
            self.widget_actions_available_label,
            self.widget_actions_selected_label,
        ]
        self.widget_actions_lists = [self.widget_actions_available, self.widget_actions_selected]
        self._widget_actions_available_cmds: list[str] = []
        self._widget_actions_selected_cmds: list[str] = []

        self.widget_actions_buttons: list[ttk.Button] = []
        action_btns = ttk.Frame(form)
        action_btns.grid(row=19, column=2, columnspan=2, sticky="w", pady=4)
        self.widget_actions_buttons_row = action_btns
        self.widget_actions_buttons.append(
            ttk.Button(action_btns, text="Add →", command=self._add_widget_actions)
        )
        self.widget_actions_buttons[-1].pack(side=tk.LEFT, padx=4)
        self.widget_actions_buttons.append(
            ttk.Button(action_btns, text="Remove", command=self._remove_widget_actions)
        )
        self.widget_actions_buttons[-1].pack(side=tk.LEFT, padx=4)
        self.widget_actions_buttons.append(
            ttk.Button(action_btns, text="Up", command=lambda: self._move_widget_action(-1))
        )
        self.widget_actions_buttons[-1].pack(side=tk.LEFT, padx=4)
        self.widget_actions_buttons.append(
            ttk.Button(action_btns, text="Down", command=lambda: self._move_widget_action(1))
        )
        self.widget_actions_buttons[-1].pack(side=tk.LEFT, padx=4)

        self.summary_items_frame = ttk.LabelFrame(form, text="Summary items")
        self.summary_items_frame.grid(row=20, column=0, columnspan=6, sticky="ew", padx=6, pady=(8, 4))
        for col in range(4):
            self.summary_items_frame.columnconfigure(col, weight=1)
        self.summary_item_list = tk.Listbox(self.summary_items_frame, height=5)
        self.summary_item_list.grid(row=0, column=0, rowspan=6, columnspan=2, sticky="nsew", padx=6, pady=3)
        self.summary_item_list.bind("<<ListboxSelect>>", lambda _: self._on_summary_item_select())
        self.summary_item_label = self._entry(self.summary_items_frame, "Label", 0, col=2)
        self.summary_item_index = self._entry(self.summary_items_frame, "Index", 1, col=2)
        ttk.Label(self.summary_items_frame, text="Format kind").grid(row=2, column=2, sticky="w")
        self.summary_item_format_kind = tk.StringVar(value="")
        ttk.OptionMenu(
            self.summary_items_frame,
            self.summary_item_format_kind,
            "",
            "",
            "number",
            "integer",
        ).grid(row=2, column=3, sticky="w", padx=6, pady=3)
        self.summary_item_precision = self._entry(self.summary_items_frame, "Precision", 3, col=2)
        self.summary_item_prefix = self._entry(self.summary_items_frame, "Prefix", 4, col=2)
        self.summary_item_suffix = self._entry(self.summary_items_frame, "Suffix", 5, col=2)
        summary_item_btns = ttk.Frame(self.summary_items_frame)
        summary_item_btns.grid(row=6, column=0, columnspan=4, sticky="w", pady=4)
        ttk.Button(summary_item_btns, text="Add", command=self._add_summary_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(summary_item_btns, text="Update", command=self._update_summary_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(summary_item_btns, text="Remove", command=self._remove_summary_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(summary_item_btns, text="Up", command=lambda: self._move_summary_item(-1)).pack(side=tk.LEFT, padx=4)
        ttk.Button(summary_item_btns, text="Down", command=lambda: self._move_summary_item(1)).pack(side=tk.LEFT,
                                                                                                    padx=4)
        ttk.Button(summary_item_btns, text="Clear Editor", command=self._clear_summary_item_editor).pack(side=tk.LEFT,
                                                                                                         padx=4)
        self._summary_items: list[dict] = []
        self._summary_item_selected_idx: int | None = None

        ttk.Button(form, text="Add Widget", command=self._add_widget).grid(
            row=21, column=1, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Up", command=lambda: self._move_widget(-1)).grid(
            row=21, column=2, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Remove Widget", command=self._remove_widget).grid(
            row=21, column=3, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Down", command=lambda: self._move_widget(1)).grid(
            row=21, column=4, padx=6, pady=4, sticky="w"
        )

        self.widget_kind.trace_add("write", lambda *_: self._sync_widget_fields())
        self._sync_widget_fields()
        self._set_actions_widget_visibility(False)
        self._set_valve_widget_visibility(False)
        self._set_summary_widget_visibility(False)

    # ------------------------
    # Battery tab editor
    # ------------------------
    def _build_battery_tab(self) -> None:
        frame = self.battery_tab_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)
        self._battery_selected_idx: int | None = None

        self.battery_list = tk.Listbox(frame, height=20)
        self.battery_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.battery_list.bind("<<ListboxSelect>>", lambda _: self._on_battery_select())

        form = ttk.Frame(frame)
        form.grid(row=0, column=1, sticky="nsew")
        form.columnconfigure(1, weight=1)

        self.battery_window_seconds = self._entry(form, "Window seconds", 0)
        self.battery_min_drop = self._entry(form, "Min drop rate (V/min)", 1)
        self.battery_id = self._entry(form, "Source ID", 2)
        self.battery_label = self._entry(form, "Label", 3)
        self.battery_sender_id = self._entry(form, "Sender ID", 4)
        self.battery_input_data_type = self._entry(form, "Input data type", 5)
        self.battery_percent_data_type = self._entry(form, "Percent data type", 6)
        self.battery_drop_rate_data_type = self._entry(form, "Drop-rate data type", 7)
        self.battery_remaining_data_type = self._entry(form, "Remaining-min data type", 8)
        self.battery_empty_voltage = self._entry(form, "Min voltage (empty)", 9)
        self.battery_nominal_voltage = self._entry(form, "Nominal voltage (optional)", 10)
        self.battery_full_voltage = self._entry(form, "Max charged voltage (full)", 11)
        self.battery_curve_exponent = self._entry(form, "Curve exponent", 12)

        btns = ttk.Frame(form)
        btns.grid(row=13, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_battery_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Remove", command=self._remove_battery_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Up", command=lambda: self._move_battery_item(-1)).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Down", command=lambda: self._move_battery_item(1)).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Add AV Bay Preset", command=self._add_av_bay_battery_preset).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Add Valve Board Preset", command=self._add_valve_board_battery_preset).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(
            btns,
            text="Add Fill Box Preset",
            command=self._add_fill_box_battery_preset,
        ).pack(side=tk.LEFT, padx=4)

    # ------------------------
    # Helpers
    # ------------------------
    def _entry(
            self, parent: ttk.Frame, label: str, row: int, col: int = 0, col_span: int = 1
    ) -> tk.Entry:
        ttk.Label(parent, text=label).grid(row=row, column=col, sticky="w")
        entry = ttk.Entry(parent)
        entry.grid(row=row, column=col + 1, columnspan=col_span, sticky="ew", padx=6, pady=3)
        parent.columnconfigure(col + 1, weight=1)
        return entry

    def _color_entry(self, parent: ttk.Frame, label: str, row: int) -> ttk.Entry:
        ttk.Label(parent, text=label).grid(row=row, column=0, sticky="w")
        entry = ttk.Entry(parent)
        entry.grid(row=row, column=1, sticky="ew", padx=6, pady=3)
        ttk.Button(parent, text="Pick", command=lambda: self._pick_color(entry)).grid(
            row=row, column=2, sticky="w", padx=(0, 6)
        )
        parent.columnconfigure(1, weight=1)
        return entry

    def _color_entry_at(self, parent: ttk.Frame, label: str, row: int, col: int) -> ttk.Entry:
        ttk.Label(parent, text=label).grid(row=row, column=col, sticky="w")
        entry = ttk.Entry(parent)
        entry.grid(row=row, column=col + 1, sticky="ew", padx=6, pady=3)
        ttk.Button(parent, text="Pick", command=lambda: self._pick_color(entry)).grid(
            row=row, column=col + 2, sticky="w", padx=(0, 6)
        )
        parent.columnconfigure(col + 1, weight=1)
        return entry

    def _pick_color(self, entry: ttk.Entry) -> None:
        initial = entry.get().strip() or None
        _, hex_color = colorchooser.askcolor(color=initial, parent=self)
        if hex_color:
            entry.delete(0, tk.END)
            entry.insert(0, hex_color)

    def _refresh_lists(self) -> None:
        self._with_suspended_events(self._refresh_lists_inner)

    def _refresh_lists_inner(self) -> None:
        if hasattr(self, "main_tabs_list"):
            self.main_tabs_list.delete(0, tk.END)
            for tab_id in self.data.get("main_tabs", []):
                self.main_tabs_list.insert(tk.END, tab_id)
            self._load_main_tabs_form()

        self.data_list.delete(0, tk.END)
        for t in self.data["data_tab"]["tabs"]:
            self.data_list.insert(tk.END, t.get("label") or t.get("id") or "tab")
        if hasattr(self, "data_sender_split_types"):
            split_types = self.data.get("data_tab", {}).get("sender_split_data_types", [])
            self.data_sender_split_types.delete(0, tk.END)
            self.data_sender_split_types.insert(0, ", ".join(split_types))

        self.conn_list.delete(0, tk.END)
        for s in self.data["connection_tab"]["sections"]:
            self.conn_list.insert(tk.END, f'{s.get("kind")} - {s.get("title", "")}')

        self.actions_list.delete(0, tk.END)
        for a in self.data["actions_tab"]["actions"]:
            self.actions_list.insert(tk.END, a.get("label", "action"))
        if hasattr(self, "disable_actions_by_default"):
            actions_tab = self.data.get("actions_tab", {})
            self.disable_actions_by_default.set(
                bool(
                    actions_tab.get("disable_actions_by_default", False)
                )
            )
            self.show_flight_setup.set(bool(actions_tab.get("show_flight_setup", True)))
            self.show_fill_targets.set(bool(actions_tab.get("show_fill_targets", True)))
            self.fill_targets_require_actions_enabled.set(
                bool(actions_tab.get("fill_targets_require_actions_enabled", True))
            )

        self.state_entry_list.delete(0, tk.END)
        for entry in self.data["state_tab"]["states"]:
            states = ", ".join(entry.get("states", []))
            self.state_entry_list.insert(tk.END, states or "state entry")

        if hasattr(self, "battery_list"):
            self.battery_list.delete(0, tk.END)
            for source in self.data.get("battery", {}).get("sources", []):
                label = source.get("label") or source.get("id") or "battery source"
                self.battery_list.insert(tk.END, label)

        self.section_list.delete(0, tk.END)
        self.widget_list.delete(0, tk.END)

        if hasattr(self, "nitrogen_target_mass"):
            self._load_fill_targets_form()

    def _load_fill_targets_file(self) -> None:
        if self.fill_targets_path.exists():
            raw = self.fill_targets_path.read_text(encoding="utf-8")
            self.fill_targets_data = json.loads(raw)
        else:
            self.fill_targets_data = default_fill_targets()
        if hasattr(self, "fill_targets_status"):
            self.fill_targets_status.set(f"Fill target path: {self.fill_targets_path}")
        self._load_fill_targets_form()

    def _load_fill_targets_form(self) -> None:
        targets = self.fill_targets_data or default_fill_targets()
        nitrogen = targets.get("nitrogen", {})
        nitrous = targets.get("nitrous", {})
        for entry, value in (
                (self.nitrogen_target_mass, nitrogen.get("target_mass_kg", 10.0)),
                (self.nitrogen_target_pressure, nitrogen.get("target_pressure_psi", 120.0)),
                (self.nitrous_target_mass, nitrous.get("target_mass_kg", 10.0)),
                (self.nitrous_target_pressure, nitrous.get("target_pressure_psi", 745.0)),
        ):
            entry.delete(0, tk.END)
            entry.insert(0, str(value))

    def _commit_fill_targets_form(self) -> None:
        def number(entry: ttk.Entry, label: str, minimum: float) -> float:
            try:
                value = float(entry.get().strip())
            except ValueError as exc:
                raise ValueError(f"{label} must be a number") from exc
            if value < minimum:
                raise ValueError(f"{label} must be at least {minimum:g}")
            return value

        self.fill_targets_data = {
            "version": int(self.fill_targets_data.get("version", 1) or 1),
            "nitrogen": {
                "target_mass_kg": number(self.nitrogen_target_mass, "Nitrogen target mass", 0.01),
                "target_pressure_psi": number(
                    self.nitrogen_target_pressure,
                    "Nitrogen target pressure",
                    0.0,
                ),
            },
            "nitrous": {
                "target_mass_kg": number(self.nitrous_target_mass, "Nitrous target mass", 0.01),
                "target_pressure_psi": number(
                    self.nitrous_target_pressure,
                    "Nitrous target pressure",
                    0.0,
                ),
            },
        }

    def _save_fill_targets_file(self) -> bool:
        try:
            self._commit_fill_targets_form()
        except ValueError as exc:
            messagebox.showerror("Fill target error", str(exc))
            return False
        self.fill_targets_path.parent.mkdir(parents=True, exist_ok=True)
        self.fill_targets_path.write_text(
            json.dumps(self.fill_targets_data, indent=2) + "\n",
            encoding="utf-8",
        )
        if hasattr(self, "fill_targets_status"):
            self.fill_targets_status.set(f"Fill target path: {self.fill_targets_path}")
        return True

    # ------------------------
    # Load/save
    # ------------------------
    def load(self) -> None:
        if self.path.exists():
            raw = self.path.read_text(encoding="utf-8")
            self.data = json.loads(raw)
        else:
            self.data = default_layout()
        self._load_fill_targets_file()
        self._ensure_layout_shape()

        self.status.set(f"Layout path: {self.path}")
        self._refresh_lists()
        if hasattr(self, "network_title"):
            self._load_network_tab()

    def save(self) -> None:
        self._commit_current_tab()
        if not self._save_fill_targets_file():
            return
        errors = validate_layout(self.data)
        if errors:
            messagebox.showerror("Validation errors", "\n".join(errors))
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(json.dumps(self.data, indent=2) + "\n", encoding="utf-8")
        self.status.set(f"Layout path: {self.path}")

    def save_as(self) -> None:
        filename = filedialog.asksaveasfilename(
            title="Save layout as",
            defaultextension=".json",
            filetypes=[("JSON files", "*.json")],
        )
        if not filename:
            return
        self.path = Path(filename)
        self.save()

    def validate(self) -> None:
        self._commit_current_tab()
        errors = validate_layout(self.data)
        if errors:
            messagebox.showerror("Validation errors", "\n".join(errors))
        else:
            messagebox.showinfo("Validation", "Layout JSON looks good.")

    # ------------------------
    # Dashboard tab actions
    # ------------------------
    def _on_main_tab_select(self) -> None:
        if self._suspend_events:
            return
        self._main_tabs_selected_idx = self._selected_index(self.main_tabs_list)

    def _add_main_tab_item(self) -> None:
        self.data.setdefault("main_tabs", []).append(self.main_tab_choice.get())
        self._refresh_lists()

    def _remove_main_tab_item(self) -> None:
        idx = self._selected_index(self.main_tabs_list)
        if idx is None:
            return
        self.data["main_tabs"].pop(idx)
        self._refresh_lists()

    def _move_main_tab_item(self, delta: int) -> None:
        self._move_list_item(self.data["main_tabs"], self.main_tabs_list, delta)

    # ------------------------
    # Data tab actions
    # ------------------------
    def _load_data_item(self) -> None:
        idx = self._selected_index(self.data_list)
        if idx is None:
            return
        self._data_selected_idx = idx
        item = self.data["data_tab"]["tabs"][idx]
        self.data_id.delete(0, tk.END)
        self.data_id.insert(0, item.get("id", ""))
        self.data_label.delete(0, tk.END)
        self.data_label.insert(0, item.get("label", ""))
        self.data_channels.delete(0, tk.END)
        channels = self._clean_channels(item.get("channels", []))
        self.data_channels.insert(0, ", ".join(channels))
        chart = item.get("chart", {})
        self.data_chart.set(chart.get("enabled", True))
        self.data_bool_true.delete(0, tk.END)
        self.data_bool_false.delete(0, tk.END)
        self.data_bool_unknown.delete(0, tk.END)
        labels = item.get("boolean_labels", {}) or {}
        channel_labels = item.get("channel_boolean_labels", []) or []
        channel_formatters = item.get("channel_formatters", []) or []
        self.data_is_valve.set(bool(labels) or bool(channel_labels))
        self._sync_data_bool_fields()
        self.data_bool_true.insert(0, labels.get("true_label", ""))
        self.data_bool_false.insert(0, labels.get("false_label", ""))
        self.data_bool_unknown.insert(0, labels.get("unknown_label", ""))
        self.data_bool_per_channel.delete(0, tk.END)
        self.data_bool_per_channel.insert(0, self._format_valve_labels(channel_labels))
        self._data_channel_formatters = [dict(fmt) if isinstance(fmt, dict) else None for fmt in channel_formatters]
        self._sync_data_formatter_channels()
        self._data_subtab_selected_idx = None
        self._refresh_data_subtab_list()
        self._refresh_data_chart_group_list()
        self._refresh_data_summary_item_list()
        self._clear_data_subtab_editor()
        self._clear_data_chart_group_editor()
        self._clear_data_summary_item_editor()

    def _add_data_item(self) -> None:
        item = {
            "id": self.data_id.get().strip(),
            "label": self.data_label.get().strip(),
            "channels": self._clean_channels(self._split_list(self.data_channels.get())),
            "chart": {"enabled": bool(self.data_chart.get())},
        }
        labels = self._boolean_labels_from_form()
        if labels:
            item["boolean_labels"] = labels
        channel_labels = self._channel_labels_from_form()
        if channel_labels:
            item["channel_boolean_labels"] = channel_labels
        self._sync_data_formatter_channels()
        channel_formatters = [fmt for fmt in self._data_channel_formatters if fmt]
        if channel_formatters:
            item["channel_formatters"] = [
                self._data_channel_formatters[i] or {}
                for i in range(len(self._data_channel_formatters))
            ]
        self.data["data_tab"]["tabs"].append(item)
        self._refresh_lists()

    def _remove_data_item(self) -> None:
        idx = self._selected_index(self.data_list)
        if idx is None:
            return
        self.data["data_tab"]["tabs"].pop(idx)
        self._refresh_lists()

    def _move_data_item(self, delta: int) -> None:
        self._move_list_item(self.data["data_tab"]["tabs"], self.data_list, delta)

    def _current_data_tab(self) -> dict | None:
        idx = self._data_selected_idx
        tabs = self.data.get("data_tab", {}).get("tabs", [])
        if idx is None or idx >= len(tabs):
            return None
        return tabs[idx]

    def _current_data_subtabs(self) -> list[dict]:
        tab = self._current_data_tab()
        if tab is None:
            return []
        return tab.setdefault("subtabs", [])

    def _current_data_subtab(self) -> dict | None:
        subtabs = self._current_data_subtabs()
        idx = self._data_subtab_selected_idx
        if idx is None or idx >= len(subtabs):
            return None
        return subtabs[idx]

    def _data_chart_group_target(self) -> dict | None:
        return self._current_data_subtab() or self._current_data_tab()

    def _data_summary_items_target(self) -> dict | None:
        return self._current_data_subtab()

    def _data_subtab_label(self, item: dict) -> str:
        return item.get("label") or item.get("id") or item.get("data_type") or "subtab"

    def _data_chart_group_label(self, item: dict) -> str:
        title = str(item.get("title", "")).strip()
        channels = ",".join(str(v) for v in item.get("channels", []) or [])
        scale_mode = str(item.get("scale_mode", "")).strip()
        parts = [part for part in (title or channels, scale_mode) if part]
        return " | ".join(parts) if parts else "chart group"

    def _data_summary_item_label_text(self, item: dict) -> str:
        label = str(item.get("label", "")).strip()
        data_type = str(item.get("data_type", "")).strip()
        index = item.get("index")
        base = label or data_type or "summary item"
        if data_type and index is not None:
            return f"{base} [{data_type}:{index}]"
        return base

    def _refresh_data_subtab_list(self) -> None:
        self.data_subtabs_list.delete(0, tk.END)
        for item in self._current_data_subtabs():
            self.data_subtabs_list.insert(tk.END, self._data_subtab_label(item))

    def _refresh_data_chart_group_list(self) -> None:
        self.data_chart_groups_list.delete(0, tk.END)
        target = self._data_chart_group_target()
        scope = self._current_data_subtab()
        self.data_chart_group_scope.set(
            f"Editing {'subtab' if scope is not None else 'tab'} chart groups"
        )
        for item in (target.get("chart_groups", []) if target else []):
            self.data_chart_groups_list.insert(tk.END, self._data_chart_group_label(item))

    def _refresh_data_summary_item_list(self) -> None:
        self.data_summary_items_list.delete(0, tk.END)
        target = self._data_summary_items_target()
        self.data_summary_item_scope.set(
            f"Editing subtab summary items"
            if target is not None
            else "Select a subtab to edit summary items"
        )
        for item in (target.get("summary_items", []) if target else []):
            self.data_summary_items_list.insert(tk.END, self._data_summary_item_label_text(item))

    def _subtab_from_form(self) -> dict:
        item: dict[str, object] = {
            "id": self.data_subtab_id.get().strip(),
            "label": self.data_subtab_label.get().strip(),
            "chart": {"enabled": bool(self.data_subtab_chart.get())},
        }
        data_type = self.data_subtab_data_type.get().strip()
        if data_type:
            item["data_type"] = data_type
        sender_id = self.data_subtab_sender_id.get().strip()
        if sender_id:
            item["sender_id"] = sender_id
        channels = self._clean_channels(self._split_list(self.data_subtab_channels.get()))
        if channels:
            item["channels"] = channels
        existing = self._current_data_subtab() or {}
        labels = self._data_subtab_boolean_labels_from_form()
        if labels:
            item["boolean_labels"] = labels
        channel_labels = self._data_subtab_channel_labels_from_form()
        if channel_labels:
            item["channel_boolean_labels"] = channel_labels
        self._sync_data_subtab_formatter_channels()
        if any(self._data_subtab_channel_formatters):
            item["channel_formatters"] = [
                formatter or {} for formatter in self._data_subtab_channel_formatters
            ]
        for key in ("chart_groups", "summary_items"):
            if key in existing and key not in item:
                item[key] = existing[key]
        return item

    def _load_data_subtab(self, item: dict) -> None:
        self.data_subtab_id.delete(0, tk.END)
        self.data_subtab_id.insert(0, item.get("id", "") or "")
        self.data_subtab_label.delete(0, tk.END)
        self.data_subtab_label.insert(0, item.get("label", "") or "")
        self.data_subtab_data_type.delete(0, tk.END)
        self.data_subtab_data_type.insert(0, item.get("data_type", "") or "")
        self.data_subtab_sender_id.delete(0, tk.END)
        self.data_subtab_sender_id.insert(0, item.get("sender_id", "") or "")
        self.data_subtab_channels.delete(0, tk.END)
        self.data_subtab_channels.insert(0, ", ".join(self._clean_channels(item.get("channels", []) or [])))
        self.data_subtab_chart.set(bool((item.get("chart", {}) or {}).get("enabled", True)))
        labels = item.get("boolean_labels", {}) or {}
        channel_labels = item.get("channel_boolean_labels", []) or []
        channel_formatters = item.get("channel_formatters", []) or []
        self.data_subtab_has_labels.set(bool(labels) or bool(channel_labels))
        self._sync_data_subtab_bool_fields()
        self.data_subtab_bool_true.delete(0, tk.END)
        self.data_subtab_bool_true.insert(0, labels.get("true_label", "") or "")
        self.data_subtab_bool_false.delete(0, tk.END)
        self.data_subtab_bool_false.insert(0, labels.get("false_label", "") or "")
        self.data_subtab_bool_unknown.delete(0, tk.END)
        self.data_subtab_bool_unknown.insert(0, labels.get("unknown_label", "") or "")
        self.data_subtab_bool_per_channel.delete(0, tk.END)
        self.data_subtab_bool_per_channel.insert(0, self._format_valve_labels(channel_labels))
        self._data_subtab_channel_formatters = [
            dict(fmt) if isinstance(fmt, dict) else None for fmt in channel_formatters
        ]
        self._sync_data_subtab_formatter_channels()

    def _clear_data_subtab_editor(self) -> None:
        self._data_subtab_selected_idx = None
        for entry in (
                self.data_subtab_id,
                self.data_subtab_label,
                self.data_subtab_data_type,
                self.data_subtab_sender_id,
                self.data_subtab_channels,
                self.data_subtab_bool_true,
                self.data_subtab_bool_false,
                self.data_subtab_bool_unknown,
                self.data_subtab_bool_per_channel,
        ):
            entry.delete(0, tk.END)
        self.data_subtab_chart.set(True)
        self.data_subtab_has_labels.set(False)
        self._sync_data_subtab_bool_fields()
        self._data_subtab_channel_formatters = []
        self._sync_data_subtab_formatter_channels()
        self.data_subtabs_list.selection_clear(0, tk.END)

    def _commit_current_data_subtab(self) -> None:
        subtabs = self._current_data_subtabs()
        idx = self._data_subtab_selected_idx
        if idx is None or idx >= len(subtabs):
            return
        subtabs[idx] = self._subtab_from_form()

    def _on_data_subtab_select(self) -> None:
        if self._suspend_events:
            return
        self._commit_current_data_subtab()
        idx = self._selected_index(self.data_subtabs_list)
        self._data_subtab_selected_idx = idx
        if idx is None:
            self._clear_data_subtab_editor()
        else:
            subtabs = self._current_data_subtabs()
            if idx < len(subtabs):
                self._load_data_subtab(subtabs[idx])
        self._data_chart_group_selected_idx = None
        self._data_summary_item_selected_idx = None
        self._refresh_data_chart_group_list()
        self._refresh_data_summary_item_list()
        self._clear_data_chart_group_editor()
        self._clear_data_summary_item_editor()

    def _add_data_subtab(self) -> None:
        subtabs = self._current_data_subtabs()
        subtabs.append(self._subtab_from_form())
        self._refresh_data_subtab_list()
        idx = len(subtabs) - 1
        self._with_suspended_events(lambda: self.data_subtabs_list.selection_set(idx))
        self._data_subtab_selected_idx = idx
        self._load_data_subtab(subtabs[idx])
        self._refresh_data_chart_group_list()
        self._refresh_data_summary_item_list()

    def _update_data_subtab(self) -> None:
        subtabs = self._current_data_subtabs()
        idx = self._selected_index(self.data_subtabs_list)
        if idx is None or idx >= len(subtabs):
            messagebox.showwarning("Subtab", "Select a subtab first.")
            return
        subtabs[idx] = self._subtab_from_form()
        self._refresh_data_subtab_list()
        self._with_suspended_events(lambda: self.data_subtabs_list.selection_set(idx))
        self._data_subtab_selected_idx = idx
        self._load_data_subtab(subtabs[idx])

    def _remove_data_subtab(self) -> None:
        subtabs = self._current_data_subtabs()
        idx = self._selected_index(self.data_subtabs_list)
        if idx is None or idx >= len(subtabs):
            return
        subtabs.pop(idx)
        if not subtabs:
            self._current_data_tab().pop("subtabs", None)
        self._refresh_data_subtab_list()
        self._clear_data_subtab_editor()
        self._refresh_data_chart_group_list()
        self._refresh_data_summary_item_list()

    def _move_data_subtab(self, delta: int) -> None:
        subtabs = self._current_data_subtabs()
        idx = self._selected_index(self.data_subtabs_list)
        if idx is None:
            return
        new_idx = idx + delta
        if new_idx < 0 or new_idx >= len(subtabs):
            return
        subtabs[idx], subtabs[new_idx] = subtabs[new_idx], subtabs[idx]
        self._refresh_data_subtab_list()
        self._with_suspended_events(lambda: self.data_subtabs_list.selection_set(new_idx))
        self._data_subtab_selected_idx = new_idx
        self._load_data_subtab(subtabs[new_idx])

    def _data_chart_group_from_form(self) -> dict | None:
        channels_raw = self._split_list(self.data_chart_group_channels.get())
        channels: list[int] = []
        for value in channels_raw:
            try:
                channels.append(int(value))
            except ValueError:
                continue
        if not channels:
            return None
        item: dict[str, object] = {"channels": channels}
        title = self.data_chart_group_title.get().strip()
        if title:
            item["title"] = title
        data_type = self.data_chart_group_data_type.get().strip()
        if data_type:
            item["data_type"] = data_type
        sender_id = self.data_chart_group_sender_id.get().strip()
        if sender_id:
            item["sender_id"] = sender_id
        labels = self._clean_channels(self._split_list(self.data_chart_group_labels.get()))
        if labels:
            item["labels"] = labels
        scale_mode = self.data_chart_group_scale_mode.get().strip()
        if scale_mode:
            item["scale_mode"] = scale_mode
        return item

    def _load_data_chart_group(self, item: dict) -> None:
        self.data_chart_group_title.delete(0, tk.END)
        self.data_chart_group_title.insert(0, item.get("title", "") or "")
        self.data_chart_group_data_type.delete(0, tk.END)
        self.data_chart_group_data_type.insert(0, item.get("data_type", "") or "")
        self.data_chart_group_sender_id.delete(0, tk.END)
        self.data_chart_group_sender_id.insert(0, item.get("sender_id", "") or "")
        self.data_chart_group_labels.delete(0, tk.END)
        self.data_chart_group_labels.insert(0, ", ".join(self._clean_channels(item.get("labels", []) or [])))
        self.data_chart_group_channels.delete(0, tk.END)
        self.data_chart_group_channels.insert(0, ", ".join(str(v) for v in item.get("channels", []) or []))
        self.data_chart_group_scale_mode.set(str(item.get("scale_mode", "")).strip())

    def _clear_data_chart_group_editor(self) -> None:
        self._data_chart_group_selected_idx = None
        for entry in (
                self.data_chart_group_title,
                self.data_chart_group_data_type,
                self.data_chart_group_sender_id,
                self.data_chart_group_labels,
                self.data_chart_group_channels,
        ):
            entry.delete(0, tk.END)
        self.data_chart_group_scale_mode.set("")
        self.data_chart_groups_list.selection_clear(0, tk.END)

    def _commit_current_data_chart_group(self) -> None:
        target = self._data_chart_group_target()
        idx = self._data_chart_group_selected_idx
        if target is None or idx is None:
            return
        groups = target.setdefault("chart_groups", [])
        if idx >= len(groups):
            return
        item = self._data_chart_group_from_form()
        if item is not None:
            groups[idx] = item

    def _on_data_chart_group_select(self) -> None:
        if self._suspend_events:
            return
        self._commit_current_data_chart_group()
        idx = self._selected_index(self.data_chart_groups_list)
        self._data_chart_group_selected_idx = idx
        target = self._data_chart_group_target()
        groups = target.get("chart_groups", []) if target else []
        if idx is None or idx >= len(groups):
            self._clear_data_chart_group_editor()
            return
        self._load_data_chart_group(groups[idx])

    def _add_data_chart_group(self) -> None:
        target = self._data_chart_group_target()
        if target is None:
            return
        item = self._data_chart_group_from_form()
        if item is None:
            messagebox.showwarning("Chart group", "Enter at least one numeric channel index.")
            return
        groups = target.setdefault("chart_groups", [])
        groups.append(item)
        self._refresh_data_chart_group_list()
        idx = len(groups) - 1
        self._with_suspended_events(lambda: self.data_chart_groups_list.selection_set(idx))
        self._data_chart_group_selected_idx = idx
        self._load_data_chart_group(item)

    def _update_data_chart_group(self) -> None:
        target = self._data_chart_group_target()
        idx = self._selected_index(self.data_chart_groups_list)
        groups = target.get("chart_groups", []) if target else []
        if idx is None or idx >= len(groups):
            messagebox.showwarning("Chart group", "Select a chart group first.")
            return
        item = self._data_chart_group_from_form()
        if item is None:
            return
        groups[idx] = item
        self._refresh_data_chart_group_list()
        self._with_suspended_events(lambda: self.data_chart_groups_list.selection_set(idx))
        self._data_chart_group_selected_idx = idx
        self._load_data_chart_group(item)

    def _remove_data_chart_group(self) -> None:
        target = self._data_chart_group_target()
        idx = self._selected_index(self.data_chart_groups_list)
        groups = target.get("chart_groups", []) if target else []
        if idx is None or idx >= len(groups):
            return
        groups.pop(idx)
        if not groups and target is not None:
            target.pop("chart_groups", None)
        self._refresh_data_chart_group_list()
        self._clear_data_chart_group_editor()

    def _move_data_chart_group(self, delta: int) -> None:
        target = self._data_chart_group_target()
        if target is None:
            return
        groups = target.setdefault("chart_groups", [])
        idx = self._selected_index(self.data_chart_groups_list)
        if idx is None:
            return
        new_idx = idx + delta
        if new_idx < 0 or new_idx >= len(groups):
            return
        groups[idx], groups[new_idx] = groups[new_idx], groups[idx]
        self._refresh_data_chart_group_list()
        self._with_suspended_events(lambda: self.data_chart_groups_list.selection_set(new_idx))
        self._data_chart_group_selected_idx = new_idx
        self._load_data_chart_group(groups[new_idx])

    def _data_summary_item_from_form(self) -> dict | None:
        label = self.data_summary_item_label.get().strip()
        data_type = self.data_summary_item_data_type.get().strip()
        if not label or not data_type:
            return None
        try:
            index = int(self.data_summary_item_index.get().strip())
        except ValueError:
            return None
        item: dict[str, object] = {"label": label, "data_type": data_type, "index": index}
        sender_id = self.data_summary_item_sender_id.get().strip()
        if sender_id:
            item["sender_id"] = sender_id
        formatter = {}
        kind = self.data_summary_item_format_kind.get().strip()
        if kind:
            formatter["kind"] = kind
        precision_raw = self.data_summary_item_precision.get().strip()
        if precision_raw:
            try:
                formatter["precision"] = int(precision_raw)
            except ValueError:
                pass
        prefix = self.data_summary_item_prefix.get().strip()
        if prefix:
            formatter["prefix"] = prefix
        suffix = self.data_summary_item_suffix.get().strip()
        if suffix:
            formatter["suffix"] = suffix
        if formatter:
            item["formatter"] = formatter
        true_label = self.data_summary_item_true.get().strip()
        false_label = self.data_summary_item_false.get().strip()
        unknown_label = self.data_summary_item_unknown.get().strip()
        if true_label or false_label or unknown_label:
            labels = {
                "true_label": true_label or "True",
                "false_label": false_label or "False",
            }
            if unknown_label:
                labels["unknown_label"] = unknown_label
            item["boolean_labels"] = labels
        return item

    def _load_data_summary_item(self, item: dict) -> None:
        self.data_summary_item_label.delete(0, tk.END)
        self.data_summary_item_label.insert(0, item.get("label", "") or "")
        self.data_summary_item_data_type.delete(0, tk.END)
        self.data_summary_item_data_type.insert(0, item.get("data_type", "") or "")
        self.data_summary_item_index.delete(0, tk.END)
        self.data_summary_item_index.insert(0, str(item.get("index", "")))
        self.data_summary_item_sender_id.delete(0, tk.END)
        self.data_summary_item_sender_id.insert(0, item.get("sender_id", "") or "")
        boolean_labels = item.get("boolean_labels", {}) or {}
        self.data_summary_item_true.delete(0, tk.END)
        self.data_summary_item_true.insert(0, boolean_labels.get("true_label", "") or "")
        self.data_summary_item_false.delete(0, tk.END)
        self.data_summary_item_false.insert(0, boolean_labels.get("false_label", "") or "")
        self.data_summary_item_unknown.delete(0, tk.END)
        self.data_summary_item_unknown.insert(0, boolean_labels.get("unknown_label", "") or "")
        formatter = item.get("formatter", {}) or {}
        self.data_summary_item_format_kind.set(str(formatter.get("kind", "")).strip())
        self.data_summary_item_precision.delete(0, tk.END)
        if formatter.get("precision") is not None:
            self.data_summary_item_precision.insert(0, str(formatter.get("precision")))
        self.data_summary_item_prefix.delete(0, tk.END)
        self.data_summary_item_prefix.insert(0, formatter.get("prefix", "") or "")
        self.data_summary_item_suffix.delete(0, tk.END)
        self.data_summary_item_suffix.insert(0, formatter.get("suffix", "") or "")

    def _clear_data_summary_item_editor(self) -> None:
        self._data_summary_item_selected_idx = None
        for entry in (
                self.data_summary_item_label,
                self.data_summary_item_data_type,
                self.data_summary_item_index,
                self.data_summary_item_sender_id,
                self.data_summary_item_true,
                self.data_summary_item_false,
                self.data_summary_item_unknown,
                self.data_summary_item_precision,
                self.data_summary_item_prefix,
                self.data_summary_item_suffix,
        ):
            entry.delete(0, tk.END)
        self.data_summary_item_format_kind.set("")
        self.data_summary_items_list.selection_clear(0, tk.END)

    def _commit_current_data_summary_item(self) -> None:
        target = self._data_summary_items_target()
        idx = self._data_summary_item_selected_idx
        if target is None or idx is None:
            return
        items = target.setdefault("summary_items", [])
        if idx >= len(items):
            return
        item = self._data_summary_item_from_form()
        if item is not None:
            items[idx] = item

    def _on_data_summary_item_select(self) -> None:
        if self._suspend_events:
            return
        self._commit_current_data_summary_item()
        target = self._data_summary_items_target()
        items = target.get("summary_items", []) if target else []
        idx = self._selected_index(self.data_summary_items_list)
        self._data_summary_item_selected_idx = idx
        if idx is None or idx >= len(items):
            self._clear_data_summary_item_editor()
            return
        self._load_data_summary_item(items[idx])

    def _add_data_summary_item(self) -> None:
        target = self._data_summary_items_target()
        if target is None:
            messagebox.showwarning("Summary item", "Select a subtab first.")
            return
        item = self._data_summary_item_from_form()
        if item is None:
            messagebox.showwarning("Summary item", "Enter label, data type, and numeric index.")
            return
        items = target.setdefault("summary_items", [])
        items.append(item)
        self._refresh_data_summary_item_list()
        idx = len(items) - 1
        self._with_suspended_events(lambda: self.data_summary_items_list.selection_set(idx))
        self._data_summary_item_selected_idx = idx
        self._load_data_summary_item(item)

    def _update_data_summary_item(self) -> None:
        target = self._data_summary_items_target()
        items = target.get("summary_items", []) if target else []
        idx = self._selected_index(self.data_summary_items_list)
        if idx is None or idx >= len(items):
            messagebox.showwarning("Summary item", "Select a summary item first.")
            return
        item = self._data_summary_item_from_form()
        if item is None:
            return
        items[idx] = item
        self._refresh_data_summary_item_list()
        self._with_suspended_events(lambda: self.data_summary_items_list.selection_set(idx))
        self._data_summary_item_selected_idx = idx
        self._load_data_summary_item(item)

    def _remove_data_summary_item(self) -> None:
        target = self._data_summary_items_target()
        items = target.get("summary_items", []) if target else []
        idx = self._selected_index(self.data_summary_items_list)
        if idx is None or idx >= len(items):
            return
        items.pop(idx)
        if not items and target is not None:
            target.pop("summary_items", None)
        self._refresh_data_summary_item_list()
        self._clear_data_summary_item_editor()

    def _move_data_summary_item(self, delta: int) -> None:
        target = self._data_summary_items_target()
        if target is None:
            return
        items = target.setdefault("summary_items", [])
        idx = self._selected_index(self.data_summary_items_list)
        if idx is None:
            return
        new_idx = idx + delta
        if new_idx < 0 or new_idx >= len(items):
            return
        items[idx], items[new_idx] = items[new_idx], items[idx]
        self._refresh_data_summary_item_list()
        self._with_suspended_events(lambda: self.data_summary_items_list.selection_set(new_idx))
        self._data_summary_item_selected_idx = new_idx
        self._load_data_summary_item(items[new_idx])

    # ------------------------
    # Battery tab actions
    # ------------------------
    def _ensure_layout_shape(self) -> None:
        self.data.setdefault(
            "main_tabs",
            [
                "state",
                "connection-status",
                "map",
                "actions",
                "calibration",
                "notifications",
                "warnings",
                "errors",
                "data",
                "detailed",
                "network-topology",
            ],
        )
        branding = self.data.setdefault("branding", {})
        branding.setdefault("app_name", None)
        branding.setdefault("dashboard_title", None)
        branding.setdefault("tab_labels", {})
        theme = self.data.setdefault("theme", {})
        for key, value in default_layout()["theme"].items():
            if isinstance(value, dict):
                theme.setdefault(key, {}).update(
                    {k: v for k, v in value.items() if k not in theme.get(key, {})}
                )
            else:
                theme.setdefault(key, value)
        self.data.setdefault("connection_tab", {}).setdefault("sections", [])
        network = self.data.setdefault("network_tab", {})
        network.setdefault("enabled", False)
        network.setdefault("title", "SEDSprintf Network")
        network.setdefault("expected_boards", ["FC", "RF", "PB", "VB", "GB", "AB", "DAQ"])
        actions_tab = self.data.setdefault("actions_tab", {})
        actions_tab.setdefault("disable_actions_by_default", False)
        actions_tab.setdefault("show_flight_setup", True)
        actions_tab.setdefault("show_fill_targets", True)
        actions_tab.setdefault("fill_targets_require_actions_enabled", True)
        actions_tab.setdefault("actions", [])
        data_tab = self.data.setdefault("data_tab", {})
        data_tab.setdefault("sender_split_data_types", [])
        data_tab.setdefault("tabs", [])
        self.data.setdefault("state_tab", {}).setdefault("states", [])
        for entry in self.data["state_tab"]["states"]:
            for section in entry.get("sections", []) or []:
                section.setdefault("value_layout", "auto")
        battery = self.data.setdefault("battery", {})
        battery.setdefault("estimator", {})
        battery.setdefault("sources", [])
        estimator = battery["estimator"]
        estimator.setdefault("window_seconds", 300)
        estimator.setdefault("min_drop_rate_v_per_min", 0.005)

    def _load_network_tab(self) -> None:
        network = self.data.get("network_tab", {})
        self.network_enabled.set(bool(network.get("enabled", False)))
        self.network_title.delete(0, tk.END)
        self.network_title.insert(0, network.get("title", "") or "")
        expected = set(network.get("expected_boards", []) or [])
        for sender_id, var in self.network_expected_board_vars.items():
            var.set(sender_id in expected)

    def _load_main_tabs_form(self) -> None:
        if not hasattr(self, "layout_version"):
            return
        self.layout_version.delete(0, tk.END)
        self.layout_version.insert(0, str(self.data.get("version", 1)))
        branding = self.data.get("branding", {}) or {}
        self.branding_app_name.delete(0, tk.END)
        self.branding_app_name.insert(0, branding.get("app_name", "") or "")
        self.branding_dashboard_title.delete(0, tk.END)
        self.branding_dashboard_title.insert(0, branding.get("dashboard_title", "") or "")
        tab_labels = branding.get("tab_labels", {}) or {}
        for tab_id, entry in self.tab_label_entries.items():
            entry.delete(0, tk.END)
            entry.insert(0, tab_labels.get(tab_id, "") or "")
        theme = self.data.get("theme", {}) or {}
        for key, entry in self.theme_entries.items():
            entry.delete(0, tk.END)
            entry.insert(0, theme.get(key, "") or "")
        accents = theme.get("main_tab_accents", {}) or {}
        for tab_id, entry in self.theme_tab_accent_entries.items():
            entry.delete(0, tk.END)
            entry.insert(0, accents.get(tab_id, "") or "")

    def _load_battery_item(self) -> None:
        idx = self._selected_index(self.battery_list)
        if idx is None:
            return
        self._battery_selected_idx = idx
        battery = self.data.get("battery", {})
        estimator = battery.get("estimator", {})
        source = battery.get("sources", [])[idx]
        self.battery_window_seconds.delete(0, tk.END)
        self.battery_window_seconds.insert(0, str(estimator.get("window_seconds", 300)))
        self.battery_min_drop.delete(0, tk.END)
        self.battery_min_drop.insert(0, str(estimator.get("min_drop_rate_v_per_min", 0.005)))
        self.battery_id.delete(0, tk.END)
        self.battery_id.insert(0, source.get("id", ""))
        self.battery_label.delete(0, tk.END)
        self.battery_label.insert(0, source.get("label", ""))
        self.battery_sender_id.delete(0, tk.END)
        self.battery_sender_id.insert(0, source.get("sender_id", ""))
        self.battery_input_data_type.delete(0, tk.END)
        self.battery_input_data_type.insert(0, source.get("input_data_type", "BATTERY_VOLTAGE"))
        self.battery_percent_data_type.delete(0, tk.END)
        self.battery_percent_data_type.insert(0, source.get("percent_data_type", ""))
        self.battery_drop_rate_data_type.delete(0, tk.END)
        self.battery_drop_rate_data_type.insert(0, source.get("drop_rate_data_type", ""))
        self.battery_remaining_data_type.delete(0, tk.END)
        self.battery_remaining_data_type.insert(0, source.get("remaining_minutes_data_type", ""))
        self.battery_empty_voltage.delete(0, tk.END)
        self.battery_empty_voltage.insert(0, str(source.get("empty_voltage", "")))
        self.battery_nominal_voltage.delete(0, tk.END)
        self.battery_nominal_voltage.insert(0, str(source.get("nominal_voltage", "")))
        self.battery_full_voltage.delete(0, tk.END)
        self.battery_full_voltage.insert(0, str(source.get("full_voltage", "")))
        self.battery_curve_exponent.delete(0, tk.END)
        self.battery_curve_exponent.insert(0, str(source.get("curve_exponent", 1.0)))

    def _battery_from_form(self) -> dict:
        def _f(v: str, default: float) -> float:
            try:
                return float(v.strip())
            except ValueError:
                return default

        source = {
            "id": self.battery_id.get().strip(),
            "label": self.battery_label.get().strip(),
            "sender_id": self.battery_sender_id.get().strip(),
            "input_data_type": self.battery_input_data_type.get().strip() or "BATTERY_VOLTAGE",
            "percent_data_type": self.battery_percent_data_type.get().strip(),
            "drop_rate_data_type": self.battery_drop_rate_data_type.get().strip(),
            "remaining_minutes_data_type": self.battery_remaining_data_type.get().strip(),
            "empty_voltage": _f(self.battery_empty_voltage.get(), 6.3),
            "full_voltage": _f(self.battery_full_voltage.get(), 8.4),
            "curve_exponent": _f(self.battery_curve_exponent.get(), 1.0),
        }
        nominal_raw = self.battery_nominal_voltage.get().strip()
        if nominal_raw:
            try:
                source["nominal_voltage"] = float(nominal_raw)
            except ValueError:
                pass
        return source

    def _add_battery_item(self) -> None:
        self.data["battery"]["sources"].append(self._battery_from_form())
        self._refresh_lists()

    def _add_av_bay_battery_preset(self) -> None:
        self.data["battery"]["sources"].append(
            {
                "id": "av_bay",
                "label": "AV Bay Battery",
                "sender_id": "PB",
                "input_data_type": "BATTERY_VOLTAGE",
                "percent_data_type": "AV_BAY_BATTERY_PERCENT",
                "drop_rate_data_type": "AV_BAY_BATTERY_DROP_RATE_V_PER_MIN",
                "remaining_minutes_data_type": "AV_BAY_BATTERY_REMAINING_MINUTES",
                "empty_voltage": 6.3,
                "nominal_voltage": 7.4,
                "full_voltage": 8.4,
                "curve_exponent": 1.0,
            }
        )
        self._refresh_lists()

    def _add_fill_box_battery_preset(self) -> None:
        self.data["battery"]["sources"].append(
            {
                "id": "fill_box_power",
                "label": "Fill Box Power",
                "sender_id": "GB",
                "input_data_type": "BATTERY_VOLTAGE",
                "percent_data_type": "FILL_BOX_POWER_PERCENT",
                "drop_rate_data_type": "FILL_BOX_POWER_DROP_RATE_V_PER_MIN",
                "remaining_minutes_data_type": "FILL_BOX_POWER_REMAINING_MINUTES",
                "empty_voltage": 13.3,
                "nominal_voltage": 14.0,
                "full_voltage": 15.5,
                "curve_exponent": 1.0,
            }
        )
        self._refresh_lists()

    def _add_valve_board_battery_preset(self) -> None:
        self.data["battery"]["sources"].append(
            {
                "id": "valve_board",
                "label": "Valve Board Battery",
                "sender_id": "VB",
                "input_data_type": "BATTERY_VOLTAGE",
                "percent_data_type": "VALVE_BOARD_BATTERY_PERCENT",
                "drop_rate_data_type": "VALVE_BOARD_BATTERY_DROP_RATE_V_PER_MIN",
                "remaining_minutes_data_type": "VALVE_BOARD_BATTERY_REMAINING_MINUTES",
                "empty_voltage": 6.3,
                "nominal_voltage": 7.4,
                "full_voltage": 8.4,
                "curve_exponent": 1.0,
            }
        )
        self._refresh_lists()

    def _remove_battery_item(self) -> None:
        idx = self._selected_index(self.battery_list)
        if idx is None:
            return
        self.data["battery"]["sources"].pop(idx)
        self._refresh_lists()

    def _move_battery_item(self, delta: int) -> None:
        self._move_list_item(self.data["battery"]["sources"], self.battery_list, delta)

    # ------------------------
    # Connection tab actions
    # ------------------------
    def _load_conn_item(self) -> None:
        idx = self._selected_index(self.conn_list)
        if idx is None:
            return
        self._conn_selected_idx = idx
        item = self.data["connection_tab"]["sections"][idx]
        self.conn_kind.set(item.get("kind", "board_status"))
        self.conn_title.delete(0, tk.END)
        self.conn_title.insert(0, item.get("title", ""))

    def _add_conn_item(self) -> None:
        self.data["connection_tab"]["sections"].append(
            {"kind": self.conn_kind.get(), "title": self.conn_title.get().strip()}
        )
        self._refresh_lists()

    def _remove_conn_item(self) -> None:
        idx = self._selected_index(self.conn_list)
        if idx is None:
            return
        self.data["connection_tab"]["sections"].pop(idx)
        self._refresh_lists()

    def _move_conn_item(self, delta: int) -> None:
        self._move_list_item(self.data["connection_tab"]["sections"], self.conn_list, delta)

    # ------------------------
    # Actions tab actions
    # ------------------------
    def _load_action_item(self) -> None:
        idx = self._selected_index(self.actions_list)
        if idx is None:
            return
        self._actions_selected_idx = idx
        item = self.data["actions_tab"]["actions"][idx]
        self.action_label.delete(0, tk.END)
        self.action_label.insert(0, item.get("label", ""))
        self.action_cmd.delete(0, tk.END)
        self.action_cmd.insert(0, item.get("cmd", ""))
        self.action_border.delete(0, tk.END)
        self.action_border.insert(0, item.get("border", ""))
        self.action_bg.delete(0, tk.END)
        self.action_bg.insert(0, item.get("bg", ""))
        self.action_fg.delete(0, tk.END)
        self.action_fg.insert(0, item.get("fg", ""))
        self.action_illuminated.set(bool(item.get("illuminated", False)))
        self.action_spacer_before.set(bool(item.get("spacer_before", False)))
        self.action_spacer_after.set(bool(item.get("spacer_after", False)))
        self.action_new_row_before.set(bool(item.get("new_row_before", False)))
        self.action_new_row_after.set(bool(item.get("new_row_after", False)))
        self.action_spacer_row_before.set(bool(item.get("spacer_row_before", False)))
        self.action_spacer_row_after.set(bool(item.get("spacer_row_after", False)))

    def _add_action_item(self) -> None:
        self.data["actions_tab"]["actions"].append(self._action_from_form())
        self._refresh_widget_actions(self._widget_actions_selected_cmds)
        self._refresh_lists()

    def _remove_action_item(self) -> None:
        idx = self._selected_index(self.actions_list)
        if idx is None:
            return
        self.data["actions_tab"]["actions"].pop(idx)
        self._refresh_widget_actions(self._widget_actions_selected_cmds)
        self._refresh_lists()

    def _move_action_item(self, delta: int) -> None:
        self._move_list_item(self.data["actions_tab"]["actions"], self.actions_list, delta)
        self._refresh_widget_actions(self._widget_actions_selected_cmds)

    def _action_from_form(self) -> dict:
        return {
            "label": self.action_label.get().strip(),
            "cmd": self.action_cmd.get().strip(),
            "border": self.action_border.get().strip(),
            "bg": self.action_bg.get().strip(),
            "fg": self.action_fg.get().strip(),
            "illuminated": bool(self.action_illuminated.get()),
            "spacer_before": bool(self.action_spacer_before.get()),
            "spacer_after": bool(self.action_spacer_after.get()),
            "new_row_before": bool(self.action_new_row_before.get()),
            "new_row_after": bool(self.action_new_row_after.get()),
            "spacer_row_before": bool(self.action_spacer_row_before.get()),
            "spacer_row_after": bool(self.action_spacer_row_after.get()),
        }

    def _store_actions_defaults(self) -> None:
        actions_tab = self.data.setdefault("actions_tab", {})
        actions_tab["disable_actions_by_default"] = bool(self.disable_actions_by_default.get())
        actions_tab["show_flight_setup"] = bool(self.show_flight_setup.get())
        actions_tab["show_fill_targets"] = bool(self.show_fill_targets.get())
        actions_tab["fill_targets_require_actions_enabled"] = bool(
            self.fill_targets_require_actions_enabled.get()
        )

    # ------------------------
    # State layout actions
    # ------------------------
    def _load_state_entry(self) -> None:
        idx = self._selected_index(self.state_entry_list)
        if idx is None:
            return
        entry = self.data["state_tab"]["states"][idx]
        self._state_entry_selected_idx = idx
        self._state_section_selected_idx = None
        self._state_widget_selected_idx = None
        self.state_states.delete(0, tk.END)
        self.state_states.insert(0, ", ".join(entry.get("states", [])))
        self.section_list.delete(0, tk.END)
        for s in entry.get("sections", []):
            self.section_list.insert(tk.END, s.get("title", "Section"))
        self.widget_list.delete(0, tk.END)
        if entry.get("sections"):
            def _select_first() -> None:
                self.section_list.selection_clear(0, tk.END)
                self.section_list.selection_set(0)
                self.section_list.activate(0)
                self.section_list.see(0)
                self._load_section_for(idx, 0)

            self._with_suspended_events(_select_first)
        else:
            self._clear_section_form()
            self._clear_widget_form()

    def _load_section_from_selection(self) -> None:
        s_idx = self._selected_index(self.section_list)
        e_idx = self._state_entry_selected_idx
        if e_idx is None:
            e_idx = self._selected_index(self.state_entry_list)
        if e_idx is None or s_idx is None:
            self._clear_widget_form()
            return
        self._load_section_for(e_idx, s_idx)

    def _load_section_for(self, e_idx: int, s_idx: int) -> None:
        section = self.data["state_tab"]["states"][e_idx]["sections"][s_idx]
        self._state_section_selected_idx = s_idx
        self._state_widget_selected_idx = None
        self.section_title.delete(0, tk.END)
        self.section_title.insert(0, section.get("title", ""))
        self._section_title_loaded = section.get("title", "")
        self.section_value_layout.set(section.get("value_layout", "auto") or "auto")
        section_style = section.get("style", {}) or {}
        self.section_bg.delete(0, tk.END)
        self.section_bg.insert(0, section_style.get("background", "") or "")
        self.section_border.delete(0, tk.END)
        self.section_border.insert(0, section_style.get("border", "") or "")
        self.section_title_color.delete(0, tk.END)
        self.section_title_color.insert(0, section_style.get("title_color", "") or "")
        self.widget_list.delete(0, tk.END)
        widgets = section.get("widgets", [])
        if not isinstance(widgets, list):
            widgets = []
            section["widgets"] = widgets
        for w in widgets:
            self.widget_list.insert(tk.END, self._widget_label(w))
        self._clear_widget_form()

    def _load_widget_from_selection(self) -> None:
        e_idx = self._state_entry_selected_idx
        s_idx = self._state_section_selected_idx
        w_idx = self._selected_index(self.widget_list)
        if None in (e_idx, s_idx, w_idx):
            self._clear_widget_form()
            return
        self._load_widget_for(e_idx, s_idx, w_idx)

    def _load_widget_for(self, e_idx: int, s_idx: int, w_idx: int) -> None:
        widget = self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["widgets"][w_idx]
        self._state_widget_selected_idx = w_idx
        self.widget_kind.set(widget.get("kind", "summary"))
        self.widget_data_type.delete(0, tk.END)
        self.widget_data_type.insert(0, widget.get("data_type", ""))
        self.widget_chart_title.delete(0, tk.END)
        self.widget_chart_title.insert(0, widget.get("chart_title", ""))
        self.widget_width.delete(0, tk.END)
        self.widget_width.insert(0, str(widget.get("width", "")))
        self.widget_height.delete(0, tk.END)
        self.widget_height.insert(0, str(widget.get("height", "")))
        self.widget_full_width.set(bool(widget.get("full_width", False)))
        self.widget_width_fraction.delete(0, tk.END)
        self.widget_width_fraction.insert(0, str(widget.get("width_fraction", "")))
        self._chart_series = [dict(item) for item in widget.get("chart_series", []) or []]
        self._refresh_chart_series_list()
        self._clear_chart_series_editor()
        self._summary_items = [dict(item) for item in widget.get("items", []) or []]
        self._refresh_summary_item_list()
        self._clear_summary_item_editor()
        self.widget_valves.delete(0, tk.END)
        self.widget_valves.insert(0, self._format_items(widget.get("valves", [])))
        summary_style = widget.get("summary_style", {}) or {}
        self.summary_bg.delete(0, tk.END)
        self.summary_bg.insert(0, summary_style.get("background", "") or "")
        self.summary_border.delete(0, tk.END)
        self.summary_border.insert(0, summary_style.get("border", "") or "")
        self.summary_label_color.delete(0, tk.END)
        self.summary_label_color.insert(0, summary_style.get("label_color", "") or "")
        self.summary_value_color.delete(0, tk.END)
        self.summary_value_color.insert(0, summary_style.get("value_color", "") or "")
        self.widget_valve_true.delete(0, tk.END)
        self.widget_valve_false.delete(0, tk.END)
        self.widget_valve_unknown_text.delete(0, tk.END)
        bool_labels = widget.get("boolean_labels", {}) or {}
        self.widget_valve_true.insert(0, bool_labels.get("true_label", ""))
        self.widget_valve_false.insert(0, bool_labels.get("false_label", ""))
        self.widget_valve_unknown_text.insert(0, bool_labels.get("unknown_label", ""))
        self.widget_valve_open.delete(0, tk.END)
        self.widget_valve_open.insert(0, self._format_color_triplet(widget.get("valve_colors", {}).get("open")))
        self.widget_valve_closed.delete(0, tk.END)
        self.widget_valve_closed.insert(0, self._format_color_triplet(widget.get("valve_colors", {}).get("closed")))
        self.widget_valve_unknown.delete(0, tk.END)
        self.widget_valve_unknown.insert(0, self._format_color_triplet(widget.get("valve_colors", {}).get("unknown")))
        self.widget_valve_labels.delete(0, tk.END)
        self.widget_valve_labels.insert(0, self._format_valve_labels(widget.get("valve_labels", [])))
        self._refresh_widget_actions(widget.get("actions", []))
        self._set_actions_widget_visibility(widget.get("kind") == "actions")
        self._sync_widget_fields()

    def _add_state_entry(self) -> None:
        entry = {"states": self._split_list(self.state_states.get()), "sections": []}
        self.data["state_tab"]["states"].append(entry)
        self._refresh_lists()
        new_idx = len(self.data["state_tab"]["states"]) - 1
        if new_idx >= 0:
            self._with_suspended_events(lambda: self.state_entry_list.selection_set(new_idx))
            self._state_entry_selected_idx = new_idx
            self._load_state_entry()

    def _remove_state_entry(self) -> None:
        idx = self._selected_index(self.state_entry_list)
        if idx is None:
            return
        self.data["state_tab"]["states"].pop(idx)
        self._refresh_lists()

    def _move_state_entry(self, delta: int) -> None:
        self._move_list_item(self.data["state_tab"]["states"], self.state_entry_list, delta)

    def _add_section(self) -> None:
        e_idx = self._ensure_state_entry_selected()
        if e_idx is None:
            messagebox.showwarning("Selection required", "Create a state entry first.")
            return
        s_idx = self._state_section_selected_idx
        sections = self.data["state_tab"]["states"][e_idx]["sections"]
        if s_idx is not None and s_idx < len(sections):
            sections[s_idx]["title"] = self._section_title_loaded or ""
            if s_idx < self.section_list.size():
                def _restore_title() -> None:
                    self.section_list.delete(s_idx)
                    self.section_list.insert(s_idx, self._section_title_loaded or "Section")
                    self.section_list.selection_set(s_idx)

                self._with_suspended_events(_restore_title)
        section = {
            "title": self.section_title.get().strip(),
            "widgets": [],
            "value_layout": self.section_value_layout.get() or "auto",
        }
        style = self._style_dict(
            [
                ("background", self.section_bg),
                ("border", self.section_border),
                ("title_color", self.section_title_color),
            ]
        )
        if style:
            section["style"] = style
        sections.append(section)
        new_idx = len(sections) - 1
        if new_idx >= 0:
            def _select_new() -> None:
                self.section_list.insert(tk.END, section.get("title") or "Section")
                self.section_list.selection_clear(0, tk.END)
                self.section_list.selection_set(new_idx)
                self.section_list.activate(new_idx)
                self.section_list.see(new_idx)

            self._with_suspended_events(_select_new)
            self._state_section_selected_idx = new_idx
            self._load_section_for(e_idx, new_idx)

    def _remove_section(self) -> None:
        e_idx = self._state_entry_selected_idx
        s_idx = self._selected_index(self.section_list)
        if None in (e_idx, s_idx):
            messagebox.showwarning("Selection required", "Select a state entry and section first.")
            return
        self.data["state_tab"]["states"][e_idx]["sections"].pop(s_idx)
        if s_idx < self.section_list.size():
            self.section_list.delete(s_idx)
        self._state_section_selected_idx = None
        if self.section_list.size() > 0:
            next_idx = min(s_idx, self.section_list.size() - 1)

            def _select_next() -> None:
                self.section_list.selection_set(next_idx)
                self.section_list.activate(next_idx)
                self.section_list.see(next_idx)

            self._with_suspended_events(_select_next)
            self._state_section_selected_idx = next_idx
            self._load_section_for(e_idx, next_idx)
        else:
            self._clear_section_form()
            self._clear_widget_form()

    def _move_section(self, delta: int) -> None:
        e_idx = self._state_entry_selected_idx
        s_idx = self._selected_index(self.section_list)
        if e_idx is None or s_idx is None:
            return
        sections = self.data["state_tab"]["states"][e_idx]["sections"]
        new_idx = s_idx + delta
        if new_idx < 0 or new_idx >= len(sections):
            return
        sections[s_idx], sections[new_idx] = sections[new_idx], sections[s_idx]
        self._load_state_entry()
        self._with_suspended_events(lambda: self.section_list.selection_set(new_idx))
        self._load_section_for(e_idx, new_idx)

    def _add_widget(self) -> None:
        e_idx = self._ensure_state_entry_selected()
        if e_idx is None:
            messagebox.showwarning("Selection required", "Create a state entry first.")
            return
        sections = self.data["state_tab"]["states"][e_idx]["sections"]
        s_idx = self._state_section_selected_idx
        if s_idx is None or s_idx >= len(sections):
            section = {
                "title": self.section_title.get().strip() or "Section",
                "widgets": [],
                "value_layout": self.section_value_layout.get() or "auto",
            }
            style = self._style_dict(
                [
                    ("background", self.section_bg),
                    ("border", self.section_border),
                    ("title_color", self.section_title_color),
                ]
            )
            if style:
                section["style"] = style
            sections.append(section)
            s_idx = len(sections) - 1
            self._with_suspended_events(lambda: self.section_list.selection_set(s_idx))
            self._state_section_selected_idx = s_idx
            self._load_section_for(e_idx, s_idx)
        widget = self._widget_from_form()
        sections[s_idx]["widgets"].append(widget)
        w_idx = len(sections[s_idx]["widgets"]) - 1
        self._load_section_for(e_idx, s_idx)
        if w_idx >= 0:
            def _select_new() -> None:
                self.widget_list.selection_clear(0, tk.END)
                self.widget_list.selection_set(w_idx)
                self.widget_list.activate(w_idx)
                self.widget_list.see(w_idx)

            self._with_suspended_events(_select_new)
            self._load_widget_for(e_idx, s_idx, w_idx)

    def _remove_widget(self) -> None:
        e_idx = self._state_entry_selected_idx
        s_idx = self._state_section_selected_idx
        w_idx = self._selected_index(self.widget_list)
        if None in (e_idx, s_idx, w_idx):
            messagebox.showwarning(
                "Selection required", "Select a state entry, section, and widget first."
            )
            return
        self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["widgets"].pop(w_idx)
        self._load_section_for(e_idx, s_idx)

    def _move_widget(self, delta: int) -> None:
        e_idx = self._state_entry_selected_idx
        s_idx = self._state_section_selected_idx
        w_idx = self._selected_index(self.widget_list)
        if None in (e_idx, s_idx, w_idx):
            return
        widgets = self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["widgets"]
        new_idx = w_idx + delta
        if new_idx < 0 or new_idx >= len(widgets):
            return
        widgets[w_idx], widgets[new_idx] = widgets[new_idx], widgets[w_idx]
        self._load_section_for(e_idx, s_idx)
        self._with_suspended_events(lambda: self.widget_list.selection_set(new_idx))
        self._load_widget_for(e_idx, s_idx, new_idx)

    # ------------------------
    # Utility
    # ------------------------
    def _selected_index(self, listbox: tk.Listbox) -> int | None:
        sel = listbox.curselection()
        return sel[0] if sel else None

    def _ensure_state_entry_selected(self) -> int | None:
        idx = self._state_entry_selected_idx
        if idx is not None and idx < len(self.data["state_tab"]["states"]):
            return idx
        idx = self._selected_index(self.state_entry_list)
        if idx is not None:
            self._state_entry_selected_idx = idx
            return idx
        if self.state_entry_list.size() > 0:
            def _select_first() -> None:
                self.state_entry_list.selection_clear(0, tk.END)
                self.state_entry_list.selection_set(0)
                self.state_entry_list.activate(0)
                self.state_entry_list.see(0)

            self._with_suspended_events(_select_first)
            self._state_entry_selected_idx = 0
            self._load_state_entry()
            return 0
        return None

    def _ensure_section_selected(self, e_idx: int) -> int | None:
        sections = self.data["state_tab"]["states"][e_idx]["sections"]
        idx = self._state_section_selected_idx
        if idx is not None and idx < len(sections):
            return idx
        idx = self._selected_index(self.section_list)
        if idx is not None:
            self._state_section_selected_idx = idx
            return idx
        if self.section_list.size() > 0:
            def _select_first() -> None:
                self.section_list.selection_clear(0, tk.END)
                self.section_list.selection_set(0)
                self.section_list.activate(0)
                self.section_list.see(0)

            self._with_suspended_events(_select_first)
            self._state_section_selected_idx = 0
            self._load_section_for(e_idx, 0)
            return 0
        return None

    def _clear_section_form(self) -> None:
        self.section_title.delete(0, tk.END)
        self.section_value_layout.set("auto")
        self.section_bg.delete(0, tk.END)
        self.section_border.delete(0, tk.END)
        self.section_title_color.delete(0, tk.END)
        self._section_title_loaded = ""

    def _update_section_title_live(self) -> None:
        if self._suspend_events:
            return
        e_idx = self._state_entry_selected_idx
        if e_idx is None or e_idx >= len(self.data["state_tab"]["states"]):
            return
        s_idx = self._state_section_selected_idx
        if s_idx is None or s_idx >= len(self.data["state_tab"]["states"][e_idx]["sections"]):
            return
        title = self.section_title.get().strip()
        self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["title"] = title
        self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["value_layout"] = (
                self.section_value_layout.get() or "auto"
        )
        if s_idx < self.section_list.size():
            def _update() -> None:
                self.section_list.delete(s_idx)
                self.section_list.insert(s_idx, title or "Section")
                self.section_list.selection_set(s_idx)

            self._with_suspended_events(_update)

    def _widget_label(self, widget: dict) -> str:
        label = widget.get("kind", "widget")
        if widget.get("data_type"):
            label = f"{label} ({widget.get('data_type')})"
        elif widget.get("chart_series"):
            label = f"{label} (combined)"
        return label

    def _action_pairs(self) -> list[tuple[str, str]]:
        actions = self.data.get("actions_tab", {}).get("actions", [])
        pairs: list[tuple[str, str]] = []
        for action in actions:
            cmd = str(action.get("cmd", "")).strip()
            label = str(action.get("label", "")).strip() or cmd
            if cmd:
                pairs.append((label, cmd))
        return pairs

    def _refresh_widget_actions(self, selected_cmds: list[str]) -> None:
        self._widget_actions_available_cmds = []
        self._widget_actions_selected_cmds = []
        self.widget_actions_available.delete(0, tk.END)
        self.widget_actions_selected.delete(0, tk.END)

        pairs = self._action_pairs()
        for label, cmd in pairs:
            self._widget_actions_available_cmds.append(cmd)
            self.widget_actions_available.insert(tk.END, label)

        for cmd in selected_cmds or []:
            label = next((lbl for lbl, c in pairs if c == cmd), cmd)
            self._widget_actions_selected_cmds.append(cmd)
            self.widget_actions_selected.insert(tk.END, label)

    def _add_widget_actions(self) -> None:
        if self.widget_actions_available.cget("state") == "disabled":
            return
        selected = list(self.widget_actions_available.curselection())
        if not selected:
            return
        for idx in selected:
            cmd = self._widget_actions_available_cmds[idx]
            if cmd not in self._widget_actions_selected_cmds:
                self._widget_actions_selected_cmds.append(cmd)
                label = self.widget_actions_available.get(idx)
                self.widget_actions_selected.insert(tk.END, label)

    def _remove_widget_actions(self) -> None:
        if self.widget_actions_selected.cget("state") == "disabled":
            return
        selected = list(self.widget_actions_selected.curselection())
        if not selected:
            return
        for idx in reversed(selected):
            self.widget_actions_selected.delete(idx)
            self._widget_actions_selected_cmds.pop(idx)

    def _move_widget_action(self, delta: int) -> None:
        if self.widget_actions_selected.cget("state") == "disabled":
            return
        sel = self.widget_actions_selected.curselection()
        if not sel:
            return
        idx = sel[0]
        new_idx = idx + delta
        if new_idx < 0 or new_idx >= len(self._widget_actions_selected_cmds):
            return
        cmds = self._widget_actions_selected_cmds
        labels = list(self.widget_actions_selected.get(0, tk.END))
        cmds[idx], cmds[new_idx] = cmds[new_idx], cmds[idx]
        labels[idx], labels[new_idx] = labels[new_idx], labels[idx]
        self.widget_actions_selected.delete(0, tk.END)
        for label in labels:
            self.widget_actions_selected.insert(tk.END, label)
        self.widget_actions_selected.selection_set(new_idx)

    def _split_list(self, text: str) -> list[str]:
        return [s.strip() for s in text.split(",") if s.strip()]

    def _clean_channels(self, channels: list[str]) -> list[str]:
        return [c.strip() for c in channels if str(c).strip()]

    def _format_items(self, items: list[dict]) -> str:
        parts = []
        for item in items:
            label = item.get("label", "").strip()
            idx = item.get("index", "")
            if label:
                parts.append(f"{label}:{idx}")
        return ", ".join(parts)

    def _chart_series_label(self, item: dict) -> str:
        data_type = str(item.get("data_type", "")).strip() or "series"
        idx = item.get("index", "?")
        label = str(item.get("label", "")).strip()
        return f"{data_type}:{idx}" if not label else f"{data_type}:{idx} [{label}]"

    def _refresh_chart_series_list(self) -> None:
        self.chart_series_list.delete(0, tk.END)
        for item in self._chart_series:
            self.chart_series_list.insert(tk.END, self._chart_series_label(item))

    def _chart_series_from_form(self) -> dict | None:
        data_type = self.chart_series_data_type.get().strip()
        if not data_type:
            return None
        try:
            idx = int(self.chart_series_index.get().strip())
        except ValueError:
            return None
        item = {"data_type": data_type, "index": idx}
        label = self.chart_series_label.get().strip()
        if label:
            item["label"] = label
        return item

    def _load_chart_series_editor(self, item: dict) -> None:
        self.chart_series_data_type.delete(0, tk.END)
        self.chart_series_data_type.insert(0, item.get("data_type", ""))
        self.chart_series_index.delete(0, tk.END)
        self.chart_series_index.insert(0, str(item.get("index", "")))
        self.chart_series_label.delete(0, tk.END)
        self.chart_series_label.insert(0, item.get("label", "") or "")

    def _clear_chart_series_editor(self) -> None:
        self._chart_series_selected_idx = None
        self.chart_series_data_type.delete(0, tk.END)
        self.chart_series_index.delete(0, tk.END)
        self.chart_series_label.delete(0, tk.END)
        if hasattr(self, "chart_series_list"):
            self.chart_series_list.selection_clear(0, tk.END)

    def _on_chart_series_select(self) -> None:
        if self._suspend_events:
            return
        idx = self._selected_index(self.chart_series_list)
        if idx is None or idx >= len(self._chart_series):
            self._clear_chart_series_editor()
            return
        self._chart_series_selected_idx = idx
        self._load_chart_series_editor(self._chart_series[idx])

    def _add_chart_series(self) -> None:
        item = self._chart_series_from_form()
        if item is None:
            messagebox.showwarning("Chart series", "Enter a data type and valid index.")
            return
        self._chart_series.append(item)
        self._refresh_chart_series_list()
        idx = len(self._chart_series) - 1
        self._with_suspended_events(lambda: self.chart_series_list.selection_set(idx))
        self._chart_series_selected_idx = idx
        self._load_chart_series_editor(item)

    def _update_chart_series(self) -> None:
        idx = self._selected_index(self.chart_series_list)
        if idx is None or idx >= len(self._chart_series):
            messagebox.showwarning("Chart series", "Select a chart series first.")
            return
        item = self._chart_series_from_form()
        if item is None:
            messagebox.showwarning("Chart series", "Enter a data type and valid index.")
            return
        self._chart_series[idx] = item
        self._refresh_chart_series_list()
        self._with_suspended_events(lambda: self.chart_series_list.selection_set(idx))
        self._chart_series_selected_idx = idx

    def _remove_chart_series(self) -> None:
        idx = self._selected_index(self.chart_series_list)
        if idx is None or idx >= len(self._chart_series):
            return
        self._chart_series.pop(idx)
        self._refresh_chart_series_list()
        self._clear_chart_series_editor()

    def _move_chart_series(self, delta: int) -> None:
        idx = self._selected_index(self.chart_series_list)
        if idx is None:
            return
        new_idx = idx + delta
        if new_idx < 0 or new_idx >= len(self._chart_series):
            return
        self._chart_series[idx], self._chart_series[new_idx] = (
            self._chart_series[new_idx],
            self._chart_series[idx],
        )
        self._refresh_chart_series_list()
        self._with_suspended_events(lambda: self.chart_series_list.selection_set(new_idx))
        self._chart_series_selected_idx = new_idx
        self._load_chart_series_editor(self._chart_series[new_idx])

    def _summary_item_label(self, item: dict) -> str:
        label = str(item.get("label", "")).strip() or "item"
        idx = item.get("index", "?")
        formatter = item.get("formatter", {}) or {}
        fmt_kind = str(formatter.get("kind", "")).strip()
        prefix = str(formatter.get("prefix", "")).strip()
        suffix = str(formatter.get("suffix", "")).strip()
        extras = [part for part in (fmt_kind, prefix, suffix) if part]
        extra_text = f" [{' | '.join(extras)}]" if extras else ""
        return f"{label}:{idx}{extra_text}"

    def _refresh_summary_item_list(self) -> None:
        self.summary_item_list.delete(0, tk.END)
        for item in self._summary_items:
            self.summary_item_list.insert(tk.END, self._summary_item_label(item))

    def _summary_item_from_form(self) -> dict | None:
        label = self.summary_item_label.get().strip()
        if not label:
            return None
        try:
            idx = int(self.summary_item_index.get().strip())
        except ValueError:
            return None
        item = {"label": label, "index": idx}
        formatter: dict[str, object] = {}
        kind = self.summary_item_format_kind.get().strip()
        if kind:
            formatter["kind"] = kind
        precision_raw = self.summary_item_precision.get().strip()
        if precision_raw:
            try:
                formatter["precision"] = int(precision_raw)
            except ValueError:
                pass
        prefix = self.summary_item_prefix.get().strip()
        if prefix:
            formatter["prefix"] = prefix
        suffix = self.summary_item_suffix.get().strip()
        if suffix:
            formatter["suffix"] = suffix
        if formatter:
            item["formatter"] = formatter
        return item

    def _load_summary_item_editor(self, item: dict) -> None:
        self.summary_item_label.delete(0, tk.END)
        self.summary_item_label.insert(0, item.get("label", ""))
        self.summary_item_index.delete(0, tk.END)
        self.summary_item_index.insert(0, str(item.get("index", "")))
        formatter = item.get("formatter", {}) or {}
        self.summary_item_format_kind.set(str(formatter.get("kind", "")).strip())
        self.summary_item_precision.delete(0, tk.END)
        if formatter.get("precision") is not None:
            self.summary_item_precision.insert(0, str(formatter.get("precision")))
        self.summary_item_prefix.delete(0, tk.END)
        self.summary_item_prefix.insert(0, formatter.get("prefix", "") or "")
        self.summary_item_suffix.delete(0, tk.END)
        self.summary_item_suffix.insert(0, formatter.get("suffix", "") or "")

    def _clear_summary_item_editor(self) -> None:
        self._summary_item_selected_idx = None
        self.summary_item_label.delete(0, tk.END)
        self.summary_item_index.delete(0, tk.END)
        self.summary_item_format_kind.set("")
        self.summary_item_precision.delete(0, tk.END)
        self.summary_item_prefix.delete(0, tk.END)
        self.summary_item_suffix.delete(0, tk.END)
        if hasattr(self, "summary_item_list"):
            self.summary_item_list.selection_clear(0, tk.END)

    def _on_summary_item_select(self) -> None:
        if self._suspend_events:
            return
        idx = self._selected_index(self.summary_item_list)
        if (
                self._summary_item_selected_idx is not None
                and self._summary_item_selected_idx < len(self._summary_items)
        ):
            updated = self._summary_item_from_form()
            if updated is not None:
                self._summary_items[self._summary_item_selected_idx] = updated
                self._refresh_summary_item_list()
                if idx is not None and idx < len(self._summary_items):
                    self._with_suspended_events(lambda: self.summary_item_list.selection_set(idx))
        if idx is None or idx >= len(self._summary_items):
            self._clear_summary_item_editor()
            return
        self._summary_item_selected_idx = idx
        self._load_summary_item_editor(self._summary_items[idx])

    def _add_summary_item(self) -> None:
        item = self._summary_item_from_form()
        if item is None:
            messagebox.showwarning("Summary item", "Enter a label and valid index.")
            return
        self._summary_items.append(item)
        self._refresh_summary_item_list()
        idx = len(self._summary_items) - 1
        self._with_suspended_events(lambda: self.summary_item_list.selection_set(idx))
        self._summary_item_selected_idx = idx
        self._load_summary_item_editor(item)

    def _update_summary_item(self) -> None:
        idx = self._selected_index(self.summary_item_list)
        if idx is None or idx >= len(self._summary_items):
            messagebox.showwarning("Summary item", "Select a summary item first.")
            return
        item = self._summary_item_from_form()
        if item is None:
            messagebox.showwarning("Summary item", "Enter a label and valid index.")
            return
        self._summary_items[idx] = item
        self._refresh_summary_item_list()
        self._with_suspended_events(lambda: self.summary_item_list.selection_set(idx))
        self._summary_item_selected_idx = idx

    def _remove_summary_item(self) -> None:
        idx = self._selected_index(self.summary_item_list)
        if idx is None or idx >= len(self._summary_items):
            return
        self._summary_items.pop(idx)
        self._refresh_summary_item_list()
        self._clear_summary_item_editor()

    def _move_summary_item(self, delta: int) -> None:
        idx = self._selected_index(self.summary_item_list)
        if idx is None:
            return
        new_idx = idx + delta
        if new_idx < 0 or new_idx >= len(self._summary_items):
            return
        self._summary_items[idx], self._summary_items[new_idx] = (
            self._summary_items[new_idx],
            self._summary_items[idx],
        )
        self._refresh_summary_item_list()
        self._with_suspended_events(lambda: self.summary_item_list.selection_set(new_idx))
        self._summary_item_selected_idx = new_idx
        self._load_summary_item_editor(self._summary_items[new_idx])

    def _style_dict(self, mapping: list[tuple[str, ttk.Entry]]) -> dict | None:
        result = {
            key: entry.get().strip()
            for key, entry in mapping
            if entry.get().strip()
        }
        return result or None

    def _format_color_triplet(self, colors: dict | None) -> str:
        if not isinstance(colors, dict):
            return ""
        bg = str(colors.get("bg", "")).strip()
        border = str(colors.get("border", "")).strip()
        fg = str(colors.get("fg", "")).strip()
        if not any([bg, border, fg]):
            return ""
        return f"{bg},{border},{fg}"

    def _format_valve_labels(self, labels: list[dict]) -> str:
        parts = []
        for item in labels:
            if not isinstance(item, dict):
                continue
            t = str(item.get("true_label", "")).strip()
            f = str(item.get("false_label", "")).strip()
            u = str(item.get("unknown_label", "")).strip()
            parts.append(",".join([t, f, u]))
        return " | ".join(parts)

    def _parse_valve_labels(self, text: str) -> list[dict]:
        entries = []
        for chunk in text.split("|"):
            chunk = chunk.strip()
            if not chunk:
                continue
            parts = [p.strip() for p in chunk.split(",")]
            if len(parts) < 2:
                continue
            true_label = parts[0] or "Open"
            false_label = parts[1] or "Closed"
            unknown_label = parts[2] if len(parts) > 2 and parts[2] else None
            item = {"true_label": true_label, "false_label": false_label}
            if unknown_label:
                item["unknown_label"] = unknown_label
            entries.append(item)
        return entries

    def _parse_color_triplet(self, text: str) -> dict | None:
        parts = [p.strip() for p in text.split(",") if p.strip()]
        if len(parts) != 3:
            return None
        return {"bg": parts[0], "border": parts[1], "fg": parts[2]}

    def _parse_valve_colors(self) -> dict | None:
        open_c = self._parse_color_triplet(self.widget_valve_open.get())
        closed_c = self._parse_color_triplet(self.widget_valve_closed.get())
        unknown_c = self._parse_color_triplet(self.widget_valve_unknown.get())
        if not any([open_c, closed_c, unknown_c]):
            return None
        result: dict = {}
        if open_c:
            result["open"] = open_c
        if closed_c:
            result["closed"] = closed_c
        if unknown_c:
            result["unknown"] = unknown_c
        return result

    def _boolean_labels_for_valves(self) -> dict | None:
        true_label = self.widget_valve_true.get().strip()
        false_label = self.widget_valve_false.get().strip()
        unknown_label = self.widget_valve_unknown_text.get().strip()
        if not true_label and not false_label and not unknown_label:
            return None
        if not true_label:
            true_label = "Open"
        if not false_label:
            false_label = "Closed"
        result = {"true_label": true_label, "false_label": false_label}
        if unknown_label:
            result["unknown_label"] = unknown_label
        return result

    def _boolean_labels_from_form(self) -> dict | None:
        if not self.data_is_valve.get():
            return None
        true_label = self.data_bool_true.get().strip()
        false_label = self.data_bool_false.get().strip()
        unknown_label = self.data_bool_unknown.get().strip()
        if not true_label and not false_label and not unknown_label:
            return None
        if not true_label:
            true_label = "True"
        if not false_label:
            false_label = "False"
        result = {"true_label": true_label, "false_label": false_label}
        if unknown_label:
            result["unknown_label"] = unknown_label
        return result

    def _channel_labels_from_form(self) -> list[dict] | None:
        if not self.data_is_valve.get():
            return None
        labels = self._parse_valve_labels(self.data_bool_per_channel.get())
        return labels or None

    def _data_subtab_boolean_labels_from_form(self) -> dict | None:
        if not self.data_subtab_has_labels.get():
            return None
        true_label = self.data_subtab_bool_true.get().strip()
        false_label = self.data_subtab_bool_false.get().strip()
        unknown_label = self.data_subtab_bool_unknown.get().strip()
        if not true_label and not false_label and not unknown_label:
            return None
        if not true_label:
            true_label = "True"
        if not false_label:
            false_label = "False"
        result = {"true_label": true_label, "false_label": false_label}
        if unknown_label:
            result["unknown_label"] = unknown_label
        return result

    def _data_subtab_channel_labels_from_form(self) -> list[dict] | None:
        if not self.data_subtab_has_labels.get():
            return None
        labels = self._parse_valve_labels(self.data_subtab_bool_per_channel.get())
        return labels or None

    def _sync_data_formatter_channels(self) -> None:
        channels = self._clean_channels(self._split_list(self.data_channels.get()))
        existing = list(self._data_channel_formatters)
        self._data_channel_formatters = []
        for idx, _channel in enumerate(channels):
            self._data_channel_formatters.append(existing[idx] if idx < len(existing) else None)
        self.data_formatter_channels.delete(0, tk.END)
        for idx, channel in enumerate(channels):
            formatter = self._data_channel_formatters[idx] or {}
            kind = str(formatter.get("kind", "")).strip()
            label = f"{channel} [{kind}]" if kind else channel
            self.data_formatter_channels.insert(tk.END, label)
        self._clear_data_formatter_editor()

    def _load_data_formatter_editor(self, formatter: dict | None) -> None:
        formatter = formatter or {}
        self.data_formatter_kind.set(str(formatter.get("kind", "")).strip())
        self.data_formatter_precision.delete(0, tk.END)
        if formatter.get("precision") is not None:
            self.data_formatter_precision.insert(0, str(formatter.get("precision")))
        self.data_formatter_prefix.delete(0, tk.END)
        self.data_formatter_prefix.insert(0, formatter.get("prefix", "") or "")
        self.data_formatter_suffix.delete(0, tk.END)
        self.data_formatter_suffix.insert(0, formatter.get("suffix", "") or "")

    def _clear_data_formatter_editor(self) -> None:
        self._data_formatter_selected_idx = None
        self.data_formatter_kind.set("")
        self.data_formatter_precision.delete(0, tk.END)
        self.data_formatter_prefix.delete(0, tk.END)
        self.data_formatter_suffix.delete(0, tk.END)
        if hasattr(self, "data_formatter_channels"):
            self.data_formatter_channels.selection_clear(0, tk.END)

    def _data_formatter_from_form(self) -> dict | None:
        formatter: dict[str, object] = {}
        kind = self.data_formatter_kind.get().strip()
        if kind:
            formatter["kind"] = kind
        precision_raw = self.data_formatter_precision.get().strip()
        if precision_raw:
            try:
                formatter["precision"] = int(precision_raw)
            except ValueError:
                pass
        prefix = self.data_formatter_prefix.get().strip()
        if prefix:
            formatter["prefix"] = prefix
        suffix = self.data_formatter_suffix.get().strip()
        if suffix:
            formatter["suffix"] = suffix
        return formatter or None

    def _on_data_formatter_select(self) -> None:
        if self._suspend_events:
            return
        idx = self._selected_index(self.data_formatter_channels)
        if idx is None or idx >= len(self._data_channel_formatters):
            self._clear_data_formatter_editor()
            return
        self._data_formatter_selected_idx = idx
        self._load_data_formatter_editor(self._data_channel_formatters[idx])

    def _apply_data_formatter(self) -> None:
        idx = self._selected_index(self.data_formatter_channels)
        if idx is None or idx >= len(self._data_channel_formatters):
            messagebox.showwarning("Channel formatter", "Select a channel first.")
            return
        self._data_channel_formatters[idx] = self._data_formatter_from_form()
        self._sync_data_formatter_channels()
        self._with_suspended_events(lambda: self.data_formatter_channels.selection_set(idx))
        self._data_formatter_selected_idx = idx
        self._load_data_formatter_editor(self._data_channel_formatters[idx])

    def _clear_data_formatter(self) -> None:
        idx = self._selected_index(self.data_formatter_channels)
        if idx is None or idx >= len(self._data_channel_formatters):
            self._clear_data_formatter_editor()
            return
        self._data_channel_formatters[idx] = None
        self._sync_data_formatter_channels()

    def _sync_data_subtab_formatter_channels(self) -> None:
        channels = self._clean_channels(self._split_list(self.data_subtab_channels.get()))
        existing = list(self._data_subtab_channel_formatters)
        self._data_subtab_channel_formatters = []
        for idx, _channel in enumerate(channels):
            self._data_subtab_channel_formatters.append(existing[idx] if idx < len(existing) else None)
        self.data_subtab_formatter_channels.delete(0, tk.END)
        for idx, channel in enumerate(channels):
            formatter = self._data_subtab_channel_formatters[idx] or {}
            kind = str(formatter.get("kind", "")).strip()
            label = f"{channel} [{kind}]" if kind else channel
            self.data_subtab_formatter_channels.insert(tk.END, label)
        self._clear_data_subtab_formatter_editor()

    def _load_data_subtab_formatter_editor(self, formatter: dict | None) -> None:
        formatter = formatter or {}
        self.data_subtab_formatter_kind.set(str(formatter.get("kind", "")).strip())
        self.data_subtab_formatter_precision.delete(0, tk.END)
        if formatter.get("precision") is not None:
            self.data_subtab_formatter_precision.insert(0, str(formatter.get("precision")))
        self.data_subtab_formatter_prefix.delete(0, tk.END)
        self.data_subtab_formatter_prefix.insert(0, formatter.get("prefix", "") or "")
        self.data_subtab_formatter_suffix.delete(0, tk.END)
        self.data_subtab_formatter_suffix.insert(0, formatter.get("suffix", "") or "")

    def _clear_data_subtab_formatter_editor(self) -> None:
        self._data_subtab_formatter_selected_idx = None
        self.data_subtab_formatter_kind.set("")
        self.data_subtab_formatter_precision.delete(0, tk.END)
        self.data_subtab_formatter_prefix.delete(0, tk.END)
        self.data_subtab_formatter_suffix.delete(0, tk.END)
        if hasattr(self, "data_subtab_formatter_channels"):
            self.data_subtab_formatter_channels.selection_clear(0, tk.END)

    def _data_subtab_formatter_from_form(self) -> dict | None:
        formatter: dict[str, object] = {}
        kind = self.data_subtab_formatter_kind.get().strip()
        if kind:
            formatter["kind"] = kind
        precision_raw = self.data_subtab_formatter_precision.get().strip()
        if precision_raw:
            try:
                formatter["precision"] = int(precision_raw)
            except ValueError:
                pass
        prefix = self.data_subtab_formatter_prefix.get().strip()
        if prefix:
            formatter["prefix"] = prefix
        suffix = self.data_subtab_formatter_suffix.get().strip()
        if suffix:
            formatter["suffix"] = suffix
        return formatter or None

    def _on_data_subtab_formatter_select(self) -> None:
        if self._suspend_events:
            return
        idx = self._selected_index(self.data_subtab_formatter_channels)
        if idx is None or idx >= len(self._data_subtab_channel_formatters):
            self._clear_data_subtab_formatter_editor()
            return
        self._data_subtab_formatter_selected_idx = idx
        self._load_data_subtab_formatter_editor(self._data_subtab_channel_formatters[idx])

    def _apply_data_subtab_formatter(self) -> None:
        idx = self._selected_index(self.data_subtab_formatter_channels)
        if idx is None or idx >= len(self._data_subtab_channel_formatters):
            messagebox.showwarning("Subtab channel formatter", "Select a channel first.")
            return
        self._data_subtab_channel_formatters[idx] = self._data_subtab_formatter_from_form()
        self._sync_data_subtab_formatter_channels()
        self._with_suspended_events(lambda: self.data_subtab_formatter_channels.selection_set(idx))
        self._data_subtab_formatter_selected_idx = idx
        self._load_data_subtab_formatter_editor(self._data_subtab_channel_formatters[idx])

    def _clear_data_subtab_formatter(self) -> None:
        idx = self._selected_index(self.data_subtab_formatter_channels)
        if idx is None or idx >= len(self._data_subtab_channel_formatters):
            self._clear_data_subtab_formatter_editor()
            return
        self._data_subtab_channel_formatters[idx] = None
        self._sync_data_subtab_formatter_channels()

    def _sync_data_bool_fields(self) -> None:
        enabled = bool(self.data_is_valve.get())
        state = "normal" if enabled else "disabled"
        for entry in (self.data_bool_true, self.data_bool_false, self.data_bool_unknown):
            entry.configure(state=state)
        self.data_bool_per_channel.configure(state=state)
        if not enabled:
            self.data_bool_true.delete(0, tk.END)
            self.data_bool_false.delete(0, tk.END)
            self.data_bool_unknown.delete(0, tk.END)
            self.data_bool_per_channel.delete(0, tk.END)
            self.data_bool_true_label.grid_remove()
            self.data_bool_false_label.grid_remove()
            self.data_bool_unknown_label.grid_remove()
            self.data_bool_true.grid_remove()
            self.data_bool_false.grid_remove()
            self.data_bool_unknown.grid_remove()
            self.data_bool_per_channel_label.grid_remove()
            self.data_bool_per_channel_hint.grid_remove()
            self.data_bool_per_channel.grid_remove()
        else:
            self.data_bool_true_label.grid()
            self.data_bool_false_label.grid()
            self.data_bool_unknown_label.grid()
            self.data_bool_true.grid()
            self.data_bool_false.grid()
            self.data_bool_unknown.grid()
            self.data_bool_per_channel_label.grid()
            self.data_bool_per_channel.grid()
            self.data_bool_per_channel_hint.grid()

    def _sync_data_subtab_bool_fields(self) -> None:
        enabled = bool(self.data_subtab_has_labels.get())
        state = "normal" if enabled else "disabled"
        for entry in (
                self.data_subtab_bool_true,
                self.data_subtab_bool_false,
                self.data_subtab_bool_unknown,
        ):
            entry.configure(state=state)
        self.data_subtab_bool_per_channel.configure(state=state)
        if not enabled:
            self.data_subtab_bool_true.delete(0, tk.END)
            self.data_subtab_bool_false.delete(0, tk.END)
            self.data_subtab_bool_unknown.delete(0, tk.END)
            self.data_subtab_bool_per_channel.delete(0, tk.END)
            self.data_subtab_bool_true_label.grid_remove()
            self.data_subtab_bool_false_label.grid_remove()
            self.data_subtab_bool_unknown_label.grid_remove()
            self.data_subtab_bool_true.grid_remove()
            self.data_subtab_bool_false.grid_remove()
            self.data_subtab_bool_unknown.grid_remove()
            self.data_subtab_bool_per_channel_label.grid_remove()
            self.data_subtab_bool_per_channel_hint.grid_remove()
            self.data_subtab_bool_per_channel.grid_remove()
        else:
            self.data_subtab_bool_true_label.grid()
            self.data_subtab_bool_false_label.grid()
            self.data_subtab_bool_unknown_label.grid()
            self.data_subtab_bool_true.grid()
            self.data_subtab_bool_false.grid()
            self.data_subtab_bool_unknown.grid()
            self.data_subtab_bool_per_channel_label.grid()
            self.data_subtab_bool_per_channel.grid()
            self.data_subtab_bool_per_channel_hint.grid()

    def _parse_items(self, text: str) -> list[dict]:
        items: list[dict] = []
        for part in text.split(","):
            part = part.strip()
            if not part:
                continue
            if ":" not in part:
                continue
            label, idx = part.split(":", 1)
            label = label.strip()
            try:
                idx_val = int(idx.strip())
            except ValueError:
                continue
            items.append({"label": label, "index": idx_val})
        return items

    def _widget_from_form(self) -> dict:
        kind = self.widget_kind.get()
        widget = {"kind": kind}
        if kind in ("summary", "chart") and self.widget_data_type.get().strip():
            widget["data_type"] = self.widget_data_type.get().strip()
        if kind == "chart" and self.widget_chart_title.get().strip():
            widget["chart_title"] = self.widget_chart_title.get().strip()
        if kind == "chart" and self.widget_width.get().strip():
            try:
                widget["width"] = float(self.widget_width.get().strip())
            except ValueError:
                pass
        if kind == "chart" and self.widget_height.get().strip():
            try:
                widget["height"] = float(self.widget_height.get().strip())
            except ValueError:
                pass
        if self.widget_full_width.get():
            widget["full_width"] = True
        if self.widget_width_fraction.get().strip():
            try:
                widget["width_fraction"] = float(self.widget_width_fraction.get().strip())
            except ValueError:
                pass
        if kind == "chart":
            if self._chart_series_selected_idx is not None and self._chart_series_selected_idx < len(
                    self._chart_series):
                item = self._chart_series_from_form()
                if item is not None:
                    self._chart_series[self._chart_series_selected_idx] = item
            if self._chart_series:
                widget["chart_series"] = list(self._chart_series)
        if kind == "summary":
            if self._summary_item_selected_idx is not None and self._summary_item_selected_idx < len(
                    self._summary_items):
                item = self._summary_item_from_form()
                if item is not None:
                    self._summary_items[self._summary_item_selected_idx] = item
            if self._summary_items:
                widget["items"] = list(self._summary_items)
            summary_style = self._style_dict(
                [
                    ("background", self.summary_bg),
                    ("border", self.summary_border),
                    ("label_color", self.summary_label_color),
                    ("value_color", self.summary_value_color),
                ]
            )
            if summary_style:
                widget["summary_style"] = summary_style
        if kind == "valve_state":
            valves = self._parse_items(self.widget_valves.get())
            if valves:
                widget["valves"] = valves
            valve_colors = self._parse_valve_colors()
            if valve_colors:
                widget["valve_colors"] = valve_colors
        valve_labels = self._boolean_labels_for_valves()
        if valve_labels:
            widget["boolean_labels"] = valve_labels
        valve_labels_list = self._parse_valve_labels(self.widget_valve_labels.get())
        if valve_labels_list:
            widget["valve_labels"] = valve_labels_list
        if kind == "actions" and getattr(self, "_widget_actions_selected_cmds", None):
            widget["actions"] = list(self._widget_actions_selected_cmds)
        return widget

    def _sync_widget_fields(self) -> None:
        kind = self.widget_kind.get()
        enable_data_type = kind in ("summary", "chart")
        enable_chart = kind == "chart"
        enable_actions = kind == "actions"
        show_valves = kind == "valve_state"
        show_summary = kind == "summary"

        self._set_entry_state(self.widget_data_type, enable_data_type)
        self._set_entry_state(self.widget_chart_title, enable_chart)
        self._set_entry_state(self.widget_width, enable_chart)
        self._set_entry_state(self.widget_height, enable_chart)
        self._set_entry_state(self.widget_width_fraction, enable_chart)
        self._set_widget_field_visibility(kind)
        self._set_valve_widget_visibility(show_valves)
        self._set_summary_widget_visibility(show_summary)
        if enable_chart:
            self.chart_series_frame.grid()
        else:
            self.chart_series_frame.grid_remove()
        self._set_listbox_state(self.widget_actions_available, enable_actions)
        self._set_listbox_state(self.widget_actions_selected, enable_actions)
        for btn in self.widget_actions_buttons:
            btn.configure(state="normal" if enable_actions else "disabled")
        self._set_actions_widget_visibility(enable_actions)
        if enable_actions:
            if self._state_widget_selected_idx is None:
                self._refresh_widget_actions([])
            elif self.widget_actions_available.size() == 0:
                self._refresh_widget_actions(self._widget_actions_selected_cmds)

    def _set_entry_state(self, entry: ttk.Entry, enabled: bool) -> None:
        entry.configure(state="normal" if enabled else "disabled")

    def _set_listbox_state(self, listbox: tk.Listbox, enabled: bool) -> None:
        listbox.configure(state="normal" if enabled else "disabled")

    def _set_widget_field_visibility(self, kind: str) -> None:
        show_data_type = kind in ("summary", "chart")
        show_chart = kind == "chart"

        self._set_widget_field_group(self.widget_data_type_label, self.widget_data_type, show_data_type)
        self._set_widget_field_group(self.widget_chart_title_label, self.widget_chart_title, show_chart)
        self._set_widget_field_group(self.widget_width_label, self.widget_width, show_chart)
        self._set_widget_field_group(self.widget_height_label, self.widget_height, show_chart)
        self._set_widget_field_group(self.widget_width_fraction_label, self.widget_width_fraction, show_chart)

    def _set_widget_field_group(self, label: ttk.Label, entry: ttk.Entry, show: bool) -> None:
        if show:
            label.grid()
            entry.grid()
        else:
            label.grid_remove()
            entry.grid_remove()

    def _set_valve_widget_visibility(self, show: bool) -> None:
        fields = [
            (self.widget_valves_label, self.widget_valves),
            (self.widget_valve_true_label, self.widget_valve_true),
            (self.widget_valve_false_label, self.widget_valve_false),
            (self.widget_valve_unknown_label_text, self.widget_valve_unknown_text),
            (self.widget_valve_open_label, self.widget_valve_open),
            (self.widget_valve_closed_label, self.widget_valve_closed),
            (self.widget_valve_unknown_label, self.widget_valve_unknown),
            (self.widget_valve_labels_label, self.widget_valve_labels),
        ]
        for label, entry in fields:
            if show:
                label.grid()
                entry.grid()
            else:
                label.grid_remove()
                entry.grid_remove()
        if show:
            self.widget_valve_open_btns.grid()
            self.widget_valve_closed_btns.grid()
            self.widget_valve_unknown_btns.grid()
        else:
            self.widget_valve_open_btns.grid_remove()
            self.widget_valve_closed_btns.grid_remove()
            self.widget_valve_unknown_btns.grid_remove()

    def _set_summary_widget_visibility(self, show: bool) -> None:
        if show:
            self.summary_style_frame.grid()
            self.summary_items_frame.grid()
        else:
            self.summary_style_frame.grid_remove()
            self.summary_items_frame.grid_remove()

    def _clear_widget_form(self) -> None:
        self._state_widget_selected_idx = None
        self.widget_kind.set("summary")
        self.widget_data_type.delete(0, tk.END)
        self.widget_chart_title.delete(0, tk.END)
        self.widget_width.delete(0, tk.END)
        self.widget_height.delete(0, tk.END)
        self.widget_full_width.set(False)
        self.widget_width_fraction.delete(0, tk.END)
        self._chart_series = []
        self._refresh_chart_series_list()
        self._clear_chart_series_editor()
        self._summary_items = []
        self._refresh_summary_item_list()
        self._clear_summary_item_editor()
        self.summary_bg.delete(0, tk.END)
        self.summary_border.delete(0, tk.END)
        self.summary_label_color.delete(0, tk.END)
        self.summary_value_color.delete(0, tk.END)
        self.widget_valves.delete(0, tk.END)
        self.widget_valve_true.delete(0, tk.END)
        self.widget_valve_false.delete(0, tk.END)
        self.widget_valve_unknown_text.delete(0, tk.END)
        self.widget_valve_open.delete(0, tk.END)
        self.widget_valve_closed.delete(0, tk.END)
        self.widget_valve_unknown.delete(0, tk.END)
        self.widget_valve_labels.delete(0, tk.END)
        self._widget_actions_selected_cmds = []
        self._refresh_widget_actions([])
        self._set_actions_widget_visibility(False)
        self._set_valve_widget_visibility(False)
        self._set_summary_widget_visibility(False)
        self.chart_series_frame.grid_remove()

    def _set_actions_widget_visibility(self, visible: bool) -> None:
        if visible:
            for label in self.widget_actions_labels:
                label.grid()
            for lst in self.widget_actions_lists:
                lst.grid()
            self.widget_actions_buttons_row.grid()
        else:
            for label in self.widget_actions_labels:
                label.grid_remove()
            for lst in self.widget_actions_lists:
                lst.grid_remove()
            self.widget_actions_buttons_row.grid_remove()

    def _add_valve_color_buttons(self, parent: ttk.Frame, entry: ttk.Entry) -> None:
        ttk.Button(
            parent,
            text="Bg",
            command=lambda: self._pick_valve_color(entry, 0),
        ).pack(side=tk.LEFT, padx=2)
        ttk.Button(
            parent,
            text="Bd",
            command=lambda: self._pick_valve_color(entry, 1),
        ).pack(side=tk.LEFT, padx=2)
        ttk.Button(
            parent,
            text="Fg",
            command=lambda: self._pick_valve_color(entry, 2),
        ).pack(side=tk.LEFT, padx=2)

    def _pick_valve_color(self, entry: ttk.Entry, index: int) -> None:
        initial = entry.get().strip()
        parts = [p.strip() for p in initial.split(",")]
        while len(parts) < 3:
            parts.append("")
        _, hex_color = colorchooser.askcolor(color=parts[index] or None, parent=self)
        if not hex_color:
            return
        parts[index] = hex_color
        entry.delete(0, tk.END)
        entry.insert(0, ",".join(parts))

    def _move_list_item(self, items: list, listbox: tk.Listbox, delta: int) -> None:
        idx = self._selected_index(listbox)
        if idx is None:
            return
        new_idx = idx + delta
        if new_idx < 0 or new_idx >= len(items):
            return
        items[idx], items[new_idx] = items[new_idx], items[idx]
        self._refresh_lists()
        listbox.selection_set(new_idx)

    # ------------------------
    # Auto-commit (no Update buttons)
    # ------------------------
    def _commit_current_tab(self) -> None:
        current = self.notebook.index(self.notebook.select())
        if current == 0:
            self._commit_main_tabs_form()
        elif current == 1:
            self._commit_data_form()
        elif current == 2:
            self._commit_conn_form()
        elif current == 3:
            self._commit_network_form()
        elif current == 4:
            self._commit_action_form()
        elif current == 5:
            try:
                self._commit_fill_targets_form()
            except ValueError:
                pass
        elif current == 6:
            self._commit_state_form()
        elif current == 7:
            self._commit_battery_form()

    def _commit_main_tabs_form(self) -> None:
        self.data["main_tabs"] = [tab for tab in self.data.get("main_tabs", []) if str(tab).strip()]
        try:
            self.data["version"] = max(1, int(self.layout_version.get().strip()))
        except ValueError:
            self.data["version"] = 1
        branding = self.data.setdefault("branding", {})
        branding["app_name"] = self.branding_app_name.get().strip() or None
        branding["dashboard_title"] = self.branding_dashboard_title.get().strip() or None
        tab_labels = {
            tab_id: entry.get().strip()
            for tab_id, entry in self.tab_label_entries.items()
            if entry.get().strip()
        }
        branding["tab_labels"] = tab_labels
        theme = self.data.setdefault("theme", {})
        for key, entry in self.theme_entries.items():
            theme[key] = entry.get().strip() or default_layout()["theme"][key]
        theme["main_tab_accents"] = {
            tab_id: entry.get().strip()
            for tab_id, entry in self.theme_tab_accent_entries.items()
            if entry.get().strip()
        }

    def _commit_network_form(self) -> None:
        self.data["network_tab"] = {
            "enabled": bool(self.network_enabled.get()),
            "title": self.network_title.get().strip() or None,
            "expected_boards": [
                sender_id
                for _label, sender_id in EXPECTED_BOARD_OPTIONS
                if self.network_expected_board_vars[sender_id].get()
            ],
        }

    def _on_tab_changed(self, _event=None) -> None:
        if self._suspend_events:
            return
        self.after_idle(self._commit_current_tab)

    def _commit_data_form(self) -> None:
        data_tab = self.data.setdefault("data_tab", {})
        data_tab["sender_split_data_types"] = self._clean_channels(
            self._split_list(self.data_sender_split_types.get())
        )
        idx = self._data_selected_idx
        if idx is None or idx >= len(self.data["data_tab"]["tabs"]):
            return
        self._commit_current_data_subtab()
        self._commit_current_data_chart_group()
        self._commit_current_data_summary_item()
        self._sync_data_formatter_channels()
        existing = self.data["data_tab"]["tabs"][idx]
        updated = {
            "id": self.data_id.get().strip(),
            "label": self.data_label.get().strip(),
            "channels": self._clean_channels(self._split_list(self.data_channels.get())),
            "chart": {"enabled": bool(self.data_chart.get())},
        }
        for key in ("subtabs", "chart_groups"):
            if key in existing:
                updated[key] = existing[key]
        labels = self._boolean_labels_from_form()
        if labels:
            updated["boolean_labels"] = labels
        channel_labels = self._channel_labels_from_form()
        if channel_labels:
            updated["channel_boolean_labels"] = channel_labels
        if any(self._data_channel_formatters):
            updated["channel_formatters"] = [
                formatter or {} for formatter in self._data_channel_formatters
            ]
        self.data["data_tab"]["tabs"][idx] = updated

    def _commit_conn_form(self) -> None:
        idx = self._conn_selected_idx
        if idx is None or idx >= len(self.data["connection_tab"]["sections"]):
            return
        self.data["connection_tab"]["sections"][idx] = {
            "kind": self.conn_kind.get(),
            "title": self.conn_title.get().strip(),
        }

    def _commit_action_form(self) -> None:
        self._store_actions_defaults()
        idx = self._actions_selected_idx
        if idx is None or idx >= len(self.data["actions_tab"]["actions"]):
            return
        self.data["actions_tab"]["actions"][idx] = self._action_from_form()
        if self.widget_actions_available.size() > 0:
            self._refresh_widget_actions(self._widget_actions_selected_cmds)

    def _commit_state_form(self) -> None:
        e_idx = self._state_entry_selected_idx
        if e_idx is None or e_idx >= len(self.data["state_tab"]["states"]):
            return
        states = self._split_list(self.state_states.get())
        self.data["state_tab"]["states"][e_idx]["states"] = states

        sections = self.data["state_tab"]["states"][e_idx]["sections"]
        s_idx = self._state_section_selected_idx
        if s_idx is not None and s_idx < len(sections):
            sections[s_idx]["title"] = self.section_title.get().strip()
            sections[s_idx]["value_layout"] = self.section_value_layout.get() or "auto"
            style = self._style_dict(
                [
                    ("background", self.section_bg),
                    ("border", self.section_border),
                    ("title_color", self.section_title_color),
                ]
            )
            if style:
                sections[s_idx]["style"] = style
            else:
                sections[s_idx].pop("style", None)

            widgets = sections[s_idx].get("widgets", [])
            w_idx = self._state_widget_selected_idx
            if w_idx is not None and w_idx < len(widgets):
                widgets[w_idx] = self._widget_from_form()

    def _commit_battery_form(self) -> None:
        battery = self.data.setdefault("battery", {})
        estimator = battery.setdefault("estimator", {})
        try:
            estimator["window_seconds"] = max(30, int(self.battery_window_seconds.get().strip()))
        except ValueError:
            estimator["window_seconds"] = 300
        try:
            estimator["min_drop_rate_v_per_min"] = max(0.0, float(self.battery_min_drop.get().strip()))
        except ValueError:
            estimator["min_drop_rate_v_per_min"] = 0.005
        idx = self._battery_selected_idx
        sources = battery.setdefault("sources", [])
        if idx is None or idx >= len(sources):
            return
        sources[idx] = self._battery_from_form()

    def _on_data_select(self) -> None:
        if self._suspend_events:
            return
        new_idx = self._selected_index(self.data_list)
        self._commit_data_form()
        if new_idx is None:
            return
        self._data_selected_idx = new_idx
        self._load_data_item()

    def _on_conn_select(self) -> None:
        if self._suspend_events:
            return
        new_idx = self._selected_index(self.conn_list)
        self._commit_conn_form()
        if new_idx is None:
            return
        self._conn_selected_idx = new_idx
        self._load_conn_item()

    def _on_action_select(self) -> None:
        if self._suspend_events:
            return
        new_idx = self._selected_index(self.actions_list)
        self._commit_action_form()
        if new_idx is None:
            return
        self._actions_selected_idx = new_idx
        self._load_action_item()

    def _on_state_entry_select(self) -> None:
        if self._suspend_events:
            return
        new_idx = self._selected_index(self.state_entry_list)
        self._commit_state_form()
        if new_idx is None:
            return
        self._state_entry_selected_idx = new_idx
        self._load_state_entry()

    def _on_section_select(self) -> None:
        if self._suspend_events:
            return
        new_idx = self._selected_index(self.section_list)
        self._commit_state_form()
        if new_idx is None:
            return
        e_idx = self._state_entry_selected_idx
        if e_idx is None:
            e_idx = self._selected_index(self.state_entry_list)
        if e_idx is None:
            return
        self._state_section_selected_idx = new_idx
        self._load_section_for(e_idx, new_idx)

    def _on_widget_select(self) -> None:
        if self._suspend_events:
            return
        new_idx = self._selected_index(self.widget_list)
        self._commit_state_form()
        if new_idx is None:
            return
        self._state_widget_selected_idx = new_idx
        self._load_widget_from_selection()

    def _on_battery_select(self) -> None:
        if self._suspend_events:
            return
        new_idx = self._selected_index(self.battery_list)
        self._commit_battery_form()
        if new_idx is None:
            return
        self._battery_selected_idx = new_idx
        self._load_battery_item()

    def _with_suspended_events(self, callback) -> None:
        prev = self._suspend_events
        self._suspend_events = True
        try:
            callback()
        finally:
            self._suspend_events = prev


if __name__ == "__main__":
    try:
        parser = argparse.ArgumentParser(description="GroundStation layout JSON editor.")
        parser.add_argument(
            "--layout",
            type=Path,
            default=default_layout_path(),
            help="Path to layout JSON file (default: backend/layout/layout.json).",
        )
        args = parser.parse_args()

        app = LayoutEditor(initial_path=args.layout)
        app.mainloop()
    except KeyboardInterrupt:
        print("\nLayout editor interrupted.", file=sys.stderr)
        sys.exit(130)
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print(f"Error: Required file not found: {missing}", file=sys.stderr)
        print("Hint: verify backend/layout/layout.json exists and paths are correct.", file=sys.stderr)
        sys.exit(1)
    except PermissionError as e:
        print(f"Error: Permission denied: {e}", file=sys.stderr)
        print("Hint: check read/write permissions for the selected layout file.", file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(f"Error: layout_gui failed unexpectedly: {e}", file=sys.stderr)
        print("Hint: try validating your JSON layout file and retry.", file=sys.stderr)
        sys.exit(1)
