#!/usr/bin/env python3
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


def default_layout_path() -> Path:
    backend_dir = Path(__file__).resolve().parents[1]
    return backend_dir / "layout" / "layout.json"


def default_layout() -> dict:
    return {
        "version": 1,
        "connection_tab": {"sections": []},
        "actions_tab": {"actions": []},
        "data_tab": {"tabs": []},
        "state_tab": {"states": []},
    }


def validate_layout(data: dict) -> list[str]:
    errors: list[str] = []
    if not isinstance(data, dict):
        return ["Layout root must be an object."]

    for key in ("version", "connection_tab", "actions_tab", "data_tab", "state_tab"):
        if key not in data:
            errors.append(f"Missing top-level key: {key}")

    connection = data.get("connection_tab", {})
    if not isinstance(connection, dict) or not isinstance(connection.get("sections", []), list):
        errors.append("connection_tab.sections must be a list.")

    actions = data.get("actions_tab", {})
    if not isinstance(actions, dict) or not isinstance(actions.get("actions", []), list):
        errors.append("actions_tab.actions must be a list.")

    data_tab = data.get("data_tab", {})
    if not isinstance(data_tab, dict) or not isinstance(data_tab.get("tabs", []), list):
        errors.append("data_tab.tabs must be a list.")

    state_tab = data.get("state_tab", {})
    if not isinstance(state_tab, dict) or not isinstance(state_tab.get("states", []), list):
        errors.append("state_tab.states must be a list.")

    return errors


class LayoutEditor(tk.Tk):
    def __init__(self) -> None:
        super().__init__()
        self.title("GS26 Layout Editor")
        self.geometry("1100x720")

        self.path = default_layout_path()
        self.data = default_layout()

        toolbar = tk.Frame(self)
        toolbar.pack(fill=tk.X, padx=8, pady=8)

        tk.Button(toolbar, text="Load", command=self.load).pack(side=tk.LEFT, padx=4)
        tk.Button(toolbar, text="Save", command=self.save).pack(side=tk.LEFT, padx=4)
        tk.Button(toolbar, text="Save As", command=self.save_as).pack(side=tk.LEFT, padx=4)
        tk.Button(toolbar, text="Validate", command=self.validate).pack(side=tk.LEFT, padx=4)

        self.status = tk.StringVar(value=f"Layout path: {self.path}")
        tk.Label(self, textvariable=self.status, anchor="w").pack(fill=tk.X, padx=8)

        self.notebook = ttk.Notebook(self)
        self.notebook.pack(fill=tk.BOTH, expand=True, padx=8, pady=8)

        self.data_tab_frame = ttk.Frame(self.notebook)
        self.connection_tab_frame = ttk.Frame(self.notebook)
        self.actions_tab_frame = ttk.Frame(self.notebook)
        self.state_tab_frame = ttk.Frame(self.notebook)

        self.notebook.add(self.data_tab_frame, text="Data")
        self.notebook.add(self.connection_tab_frame, text="Connection")
        self.notebook.add(self.actions_tab_frame, text="Actions")
        self.notebook.add(self.state_tab_frame, text="State Layout")

        self._suspend_events = False
        self._section_title_loaded = ""
        self._build_data_tab()
        self._build_connection_tab()
        self._build_actions_tab()
        self._build_state_tab()

        self.load()
        self.after(100, self.focus_force)
        self.notebook.bind("<<NotebookTabChanged>>", self._on_tab_changed)

    # ------------------------
    # Data tab editor
    # ------------------------
    def _build_data_tab(self) -> None:
        frame = self.data_tab_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)
        self._data_selected_idx: int | None = None

        self.data_list = tk.Listbox(frame, height=20)
        self.data_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.data_list.bind("<<ListboxSelect>>", lambda _: self._on_data_select())

        form = ttk.Frame(frame)
        form.grid(row=0, column=1, sticky="nsew")
        form.columnconfigure(1, weight=1)

        self.data_id = self._entry(form, "ID", 0)
        self.data_label = self._entry(form, "Label", 1)
        self.data_channels = self._entry(form, "Channels (comma)", 2)
        self.data_chart = tk.BooleanVar(value=True)
        ttk.Checkbutton(form, text="Chart enabled", variable=self.data_chart).grid(
            row=3, column=1, sticky="w", pady=(6, 6)
        )
        self.data_is_valve = tk.BooleanVar(value=False)
        ttk.Checkbutton(form, text="Has labels", variable=self.data_is_valve, command=self._sync_data_bool_fields).grid(
            row=4, column=1, sticky="w", pady=(0, 6)
        )
        self.data_bool_true_label = ttk.Label(form, text="True label")
        self.data_bool_true_label.grid(row=5, column=0, sticky="w")
        self.data_bool_true = ttk.Entry(form)
        self.data_bool_true.grid(row=5, column=1, columnspan=2, sticky="ew", padx=6, pady=3)

        self.data_bool_false_label = ttk.Label(form, text="False label")
        self.data_bool_false_label.grid(row=6, column=0, sticky="w")
        self.data_bool_false = ttk.Entry(form)
        self.data_bool_false.grid(row=6, column=1, columnspan=2, sticky="ew", padx=6, pady=3)

        self.data_bool_unknown_label = ttk.Label(form, text="Unknown label")
        self.data_bool_unknown_label.grid(row=7, column=0, sticky="w")
        self.data_bool_unknown = ttk.Entry(form)
        self.data_bool_unknown.grid(row=7, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.data_bool_per_channel_label = ttk.Label(
            form, text="Per-channel labels (true,false,unknown | ...)"
        )
        self.data_bool_per_channel_label.grid(row=8, column=0, sticky="w")
        self.data_bool_per_channel = ttk.Entry(form)
        self.data_bool_per_channel.grid(row=8, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.data_bool_per_channel_hint = ttk.Label(
            form,
            text="Example: Open,Closed,Unknown | Installed,Removed,Unknown",
            foreground="#94a3b8",
        )
        self.data_bool_per_channel_hint.grid(row=9, column=1, columnspan=2, sticky="w", padx=6)

        btns = ttk.Frame(form)
        btns.grid(row=10, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_data_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Remove", command=self._remove_data_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Up", command=lambda: self._move_data_item(-1)).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Down", command=lambda: self._move_data_item(1)).pack(
            side=tk.LEFT, padx=4
        )
        self._sync_data_bool_fields()

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

        self.action_label = self._entry(form, "Label", 0)
        self.action_cmd = self._entry(form, "Command", 1)
        self.action_border = self._color_entry(form, "Border color", 2)
        self.action_bg = self._color_entry(form, "Background color", 3)
        self.action_fg = self._color_entry(form, "Text color", 4)

        btns = ttk.Frame(form)
        btns.grid(row=5, column=1, sticky="w", pady=8)
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
        ttk.Button(form, text="Remove State Entry", command=self._remove_state_entry).grid(
            row=1, column=3, padx=6, pady=4, sticky="w"
        )

        # Section row
        ttk.Label(form, text="Section title").grid(row=2, column=0, sticky="w")
        self.section_title = ttk.Entry(form)
        self.section_title.grid(row=2, column=1, columnspan=3, sticky="ew", padx=6, pady=3)
        self.section_title.bind("<KeyRelease>", lambda _: self._update_section_title_live())
        ttk.Button(form, text="Add Section", command=self._add_section).grid(
            row=3, column=1, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Remove Section", command=self._remove_section).grid(
            row=3, column=3, padx=6, pady=4, sticky="w"
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

        ttk.Label(form, text="Items (label:index,...)").grid(row=6, column=2, sticky="w")
        self.widget_items_label = form.grid_slaves(row=6, column=2)[0]
        self.widget_items = ttk.Entry(form)
        self.widget_items.grid(row=6, column=3, sticky="ew", padx=6, pady=3)

        self.widget_valves_label = ttk.Label(form, text="Valves (label:index,...)")
        self.widget_valves_label.grid(row=7, column=0, sticky="w")
        self.widget_valves = ttk.Entry(form)
        self.widget_valves.grid(row=7, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_true_label = ttk.Label(form, text="Valve true label")
        self.widget_valve_true_label.grid(row=8, column=0, sticky="w")
        self.widget_valve_true = ttk.Entry(form)
        self.widget_valve_true.grid(row=8, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_false_label = ttk.Label(form, text="Valve false label")
        self.widget_valve_false_label.grid(row=9, column=0, sticky="w")
        self.widget_valve_false = ttk.Entry(form)
        self.widget_valve_false.grid(row=9, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_unknown_label_text = ttk.Label(form, text="Valve unknown label")
        self.widget_valve_unknown_label_text.grid(row=10, column=0, sticky="w")
        self.widget_valve_unknown_text = ttk.Entry(form)
        self.widget_valve_unknown_text.grid(row=10, column=1, columnspan=3, sticky="ew", padx=6, pady=3)

        self.widget_valve_open_label = ttk.Label(form, text="Open colors (bg,border,fg)")
        self.widget_valve_open_label.grid(row=11, column=0, sticky="w")
        self.widget_valve_open = ttk.Entry(form)
        self.widget_valve_open.grid(row=11, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.widget_valve_open_btns = ttk.Frame(form)
        self.widget_valve_open_btns.grid(row=11, column=3, sticky="w")
        self._add_valve_color_buttons(self.widget_valve_open_btns, self.widget_valve_open)

        self.widget_valve_closed_label = ttk.Label(form, text="Closed colors (bg,border,fg)")
        self.widget_valve_closed_label.grid(row=12, column=0, sticky="w")
        self.widget_valve_closed = ttk.Entry(form)
        self.widget_valve_closed.grid(row=12, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.widget_valve_closed_btns = ttk.Frame(form)
        self.widget_valve_closed_btns.grid(row=12, column=3, sticky="w")
        self._add_valve_color_buttons(self.widget_valve_closed_btns, self.widget_valve_closed)

        self.widget_valve_unknown_label = ttk.Label(form, text="Unknown colors (bg,border,fg)")
        self.widget_valve_unknown_label.grid(row=13, column=0, sticky="w")
        self.widget_valve_unknown = ttk.Entry(form)
        self.widget_valve_unknown.grid(row=13, column=1, columnspan=2, sticky="ew", padx=6, pady=3)
        self.widget_valve_unknown_btns = ttk.Frame(form)
        self.widget_valve_unknown_btns.grid(row=13, column=3, sticky="w")
        self._add_valve_color_buttons(self.widget_valve_unknown_btns, self.widget_valve_unknown)

        self.widget_valve_labels_label = ttk.Label(
            form, text="Valve labels (true,false,unknown | ...)"
        )
        self.widget_valve_labels_label.grid(row=14, column=0, sticky="w")
        self.widget_valve_labels = ttk.Entry(form)
        self.widget_valve_labels.grid(row=14, column=1, columnspan=3, sticky="ew", padx=6, pady=3)
        self.widget_valve_labels_hint = ttk.Label(
            form,
            text="Example: Open,Closed,Unknown | Installed,Removed,Unknown",
            foreground="#94a3b8",
        )
        self.widget_valve_labels_hint.grid(row=15, column=1, columnspan=3, sticky="w", padx=6)

        self.widget_actions_available_label = ttk.Label(form, text="Available actions")
        self.widget_actions_available_label.grid(row=16, column=0, sticky="w")
        self.widget_actions_available = tk.Listbox(form, height=5, selectmode=tk.MULTIPLE)
        self.widget_actions_available.grid(
            row=17, column=0, columnspan=2, sticky="nsew", padx=6, pady=3
        )

        self.widget_actions_selected_label = ttk.Label(form, text="Selected actions")
        self.widget_actions_selected_label.grid(row=16, column=2, sticky="w")
        self.widget_actions_selected = tk.Listbox(form, height=5)
        self.widget_actions_selected.grid(
            row=17, column=2, columnspan=2, sticky="nsew", padx=6, pady=3
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
        action_btns.grid(row=18, column=2, columnspan=2, sticky="w", pady=4)
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

        ttk.Button(form, text="Add Widget", command=self._add_widget).grid(
            row=19, column=1, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Remove Widget", command=self._remove_widget).grid(
            row=19, column=3, padx=6, pady=4, sticky="w"
        )

        self.widget_kind.trace_add("write", lambda *_: self._sync_widget_fields())
        self._sync_widget_fields()
        self._set_actions_widget_visibility(False)
        self._set_valve_widget_visibility(False)

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

    def _pick_color(self, entry: ttk.Entry) -> None:
        initial = entry.get().strip() or None
        _, hex_color = colorchooser.askcolor(color=initial, parent=self)
        if hex_color:
            entry.delete(0, tk.END)
            entry.insert(0, hex_color)

    def _refresh_lists(self) -> None:
        self._with_suspended_events(self._refresh_lists_inner)

    def _refresh_lists_inner(self) -> None:
        self.data_list.delete(0, tk.END)
        for t in self.data["data_tab"]["tabs"]:
            self.data_list.insert(tk.END, t.get("label") or t.get("id") or "tab")

        self.conn_list.delete(0, tk.END)
        for s in self.data["connection_tab"]["sections"]:
            self.conn_list.insert(tk.END, f'{s.get("kind")} - {s.get("title","")}')

        self.actions_list.delete(0, tk.END)
        for a in self.data["actions_tab"]["actions"]:
            self.actions_list.insert(tk.END, a.get("label", "action"))

        self.state_entry_list.delete(0, tk.END)
        for entry in self.data["state_tab"]["states"]:
            states = ", ".join(entry.get("states", []))
            self.state_entry_list.insert(tk.END, states or "state entry")

        self.section_list.delete(0, tk.END)
        self.widget_list.delete(0, tk.END)

    # ------------------------
    # Load/save
    # ------------------------
    def load(self) -> None:
        if self.path.exists():
            raw = self.path.read_text(encoding="utf-8")
            self.data = json.loads(raw)
        else:
            self.data = default_layout()

        self.status.set(f"Layout path: {self.path}")
        self._refresh_lists()

    def save(self) -> None:
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
        errors = validate_layout(self.data)
        if errors:
            messagebox.showerror("Validation errors", "\n".join(errors))
        else:
            messagebox.showinfo("Validation", "Layout JSON looks good.")

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
        self.data_is_valve.set(bool(labels) or bool(channel_labels))
        self._sync_data_bool_fields()
        self.data_bool_true.insert(0, labels.get("true_label", ""))
        self.data_bool_false.insert(0, labels.get("false_label", ""))
        self.data_bool_unknown.insert(0, labels.get("unknown_label", ""))
        self.data_bool_per_channel.delete(0, tk.END)
        self.data_bool_per_channel.insert(0, self._format_valve_labels(channel_labels))

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
        }

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
        self.widget_items.delete(0, tk.END)
        self.widget_items.insert(0, self._format_items(widget.get("items", [])))
        self.widget_valves.delete(0, tk.END)
        self.widget_valves.insert(0, self._format_items(widget.get("valves", [])))
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
        section = {"title": self.section_title.get().strip(), "widgets": []}
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
            self._clear_widget_form()

    def _add_widget(self) -> None:
        e_idx = self._ensure_state_entry_selected()
        if e_idx is None:
            messagebox.showwarning("Selection required", "Create a state entry first.")
            return
        sections = self.data["state_tab"]["states"][e_idx]["sections"]
        s_idx = self._state_section_selected_idx
        if s_idx is None or s_idx >= len(sections):
            section = {"title": self.section_title.get().strip() or "Section", "widgets": []}
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
        if kind == "summary":
            items = self._parse_items(self.widget_items.get())
            if items:
                widget["items"] = items
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
        enable_items = kind == "summary"
        enable_chart = kind == "chart"
        enable_actions = kind == "actions" and self._state_widget_selected_idx is not None
        show_valves = kind == "valve_state"

        self._set_entry_state(self.widget_data_type, enable_data_type)
        self._set_entry_state(self.widget_items, enable_items)
        self._set_entry_state(self.widget_chart_title, enable_chart)
        self._set_entry_state(self.widget_width, enable_chart)
        self._set_entry_state(self.widget_height, enable_chart)
        self._set_widget_field_visibility(kind)
        self._set_valve_widget_visibility(show_valves)
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
        show_items = kind == "summary"
        show_chart = kind == "chart"

        self._set_widget_field_group(self.widget_data_type_label, self.widget_data_type, show_data_type)
        self._set_widget_field_group(self.widget_items_label, self.widget_items, show_items)
        self._set_widget_field_group(self.widget_chart_title_label, self.widget_chart_title, show_chart)
        self._set_widget_field_group(self.widget_width_label, self.widget_width, show_chart)
        self._set_widget_field_group(self.widget_height_label, self.widget_height, show_chart)

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

    def _clear_widget_form(self) -> None:
        self._state_widget_selected_idx = None
        self.widget_kind.set("summary")
        self.widget_data_type.delete(0, tk.END)
        self.widget_chart_title.delete(0, tk.END)
        self.widget_width.delete(0, tk.END)
        self.widget_height.delete(0, tk.END)
        self.widget_items.delete(0, tk.END)
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
            self._commit_data_form()
        elif current == 1:
            self._commit_conn_form()
        elif current == 2:
            self._commit_action_form()
        elif current == 3:
            self._commit_state_form()

    def _on_tab_changed(self, _event=None) -> None:
        if self._suspend_events:
            return
        self.after_idle(self._commit_current_tab)

    def _commit_data_form(self) -> None:
        idx = self._data_selected_idx
        if idx is None or idx >= len(self.data["data_tab"]["tabs"]):
            return
        self.data["data_tab"]["tabs"][idx] = {
            "id": self.data_id.get().strip(),
            "label": self.data_label.get().strip(),
            "channels": self._clean_channels(self._split_list(self.data_channels.get())),
            "chart": {"enabled": bool(self.data_chart.get())},
        }
        labels = self._boolean_labels_from_form()
        if labels:
            self.data["data_tab"]["tabs"][idx]["boolean_labels"] = labels
        channel_labels = self._channel_labels_from_form()
        if channel_labels:
            self.data["data_tab"]["tabs"][idx]["channel_boolean_labels"] = channel_labels

    def _commit_conn_form(self) -> None:
        idx = self._conn_selected_idx
        if idx is None or idx >= len(self.data["connection_tab"]["sections"]):
            return
        self.data["connection_tab"]["sections"][idx] = {
            "kind": self.conn_kind.get(),
            "title": self.conn_title.get().strip(),
        }

    def _commit_action_form(self) -> None:
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

            widgets = sections[s_idx].get("widgets", [])
            w_idx = self._state_widget_selected_idx
            if w_idx is not None and w_idx < len(widgets):
                widgets[w_idx] = self._widget_from_form()

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

    def _with_suspended_events(self, callback) -> None:
        prev = self._suspend_events
        self._suspend_events = True
        try:
            callback()
        finally:
            self._suspend_events = prev


if __name__ == "__main__":
    app = LayoutEditor()
    app.mainloop()
