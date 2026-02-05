#!/usr/bin/env python3
import json
import sys
from pathlib import Path

try:
    import tkinter as tk
    from tkinter import filedialog, messagebox, ttk
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

        self._build_data_tab()
        self._build_connection_tab()
        self._build_actions_tab()
        self._build_state_tab()

        self.load()
        self.after(100, self.focus_force)

    # ------------------------
    # Data tab editor
    # ------------------------
    def _build_data_tab(self) -> None:
        frame = self.data_tab_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)

        self.data_list = tk.Listbox(frame, height=20)
        self.data_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.data_list.bind("<<ListboxSelect>>", lambda _: self._load_data_item())

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

        btns = ttk.Frame(form)
        btns.grid(row=4, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_data_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Update", command=self._update_data_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Remove", command=self._remove_data_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Up", command=lambda: self._move_data_item(-1)).pack(
            side=tk.LEFT, padx=4
        )
        ttk.Button(btns, text="Down", command=lambda: self._move_data_item(1)).pack(
            side=tk.LEFT, padx=4
        )

    # ------------------------
    # Connection tab editor
    # ------------------------
    def _build_connection_tab(self) -> None:
        frame = self.connection_tab_frame
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(0, weight=1)

        self.conn_list = tk.Listbox(frame, height=20)
        self.conn_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.conn_list.bind("<<ListboxSelect>>", lambda _: self._load_conn_item())

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
        ttk.Button(btns, text="Update", command=self._update_conn_item).pack(
            side=tk.LEFT, padx=4
        )
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

        self.actions_list = tk.Listbox(frame, height=20)
        self.actions_list.grid(row=0, column=0, sticky="ns", padx=(0, 10), pady=5)
        self.actions_list.bind("<<ListboxSelect>>", lambda _: self._load_action_item())

        form = ttk.Frame(frame)
        form.grid(row=0, column=1, sticky="nsew")
        form.columnconfigure(1, weight=1)

        self.action_label = self._entry(form, "Label", 0)
        self.action_cmd = self._entry(form, "Command", 1)
        self.action_border = self._entry(form, "Border color", 2)
        self.action_bg = self._entry(form, "Background color", 3)
        self.action_fg = self._entry(form, "Text color", 4)

        btns = ttk.Frame(form)
        btns.grid(row=5, column=1, sticky="w", pady=8)
        ttk.Button(btns, text="Add", command=self._add_action_item).pack(side=tk.LEFT, padx=4)
        ttk.Button(btns, text="Update", command=self._update_action_item).pack(
            side=tk.LEFT, padx=4
        )
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
        self.state_entry_list.bind("<<ListboxSelect>>", lambda _: self._load_state_entry())

        # Sections
        ttk.Label(frame, text="Sections").grid(row=0, column=1, sticky="w")
        self.section_list = tk.Listbox(frame, height=18)
        self.section_list.grid(row=1, column=1, sticky="ns", padx=(0, 10))
        self.section_list.bind("<<ListboxSelect>>", lambda _: self._load_section())

        # Widgets
        ttk.Label(frame, text="Widgets").grid(row=0, column=2, sticky="w")
        self.widget_list = tk.Listbox(frame, height=18)
        self.widget_list.grid(row=1, column=2, sticky="nsew")
        self.widget_list.bind("<<ListboxSelect>>", lambda _: self._load_widget())

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
        ttk.Button(form, text="Update State Entry", command=self._update_state_entry).grid(
            row=1, column=2, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Remove State Entry", command=self._remove_state_entry).grid(
            row=1, column=3, padx=6, pady=4, sticky="w"
        )

        # Section row
        ttk.Label(form, text="Section title").grid(row=2, column=0, sticky="w")
        self.section_title = ttk.Entry(form)
        self.section_title.grid(row=2, column=1, columnspan=3, sticky="ew", padx=6, pady=3)
        ttk.Button(form, text="Add Section", command=self._add_section).grid(
            row=3, column=1, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Update Section", command=self._update_section).grid(
            row=3, column=2, padx=6, pady=4, sticky="w"
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
        self.widget_data_type = ttk.Entry(form)
        self.widget_data_type.grid(row=4, column=3, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Chart title").grid(row=5, column=0, sticky="w")
        self.widget_chart_title = ttk.Entry(form)
        self.widget_chart_title.grid(row=5, column=1, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Width").grid(row=5, column=2, sticky="w")
        self.widget_width = ttk.Entry(form)
        self.widget_width.grid(row=5, column=3, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Height").grid(row=6, column=0, sticky="w")
        self.widget_height = ttk.Entry(form)
        self.widget_height.grid(row=6, column=1, sticky="ew", padx=6, pady=3)

        ttk.Label(form, text="Items (label:index,...)").grid(row=6, column=2, sticky="w")
        self.widget_items = ttk.Entry(form)
        self.widget_items.grid(row=6, column=3, sticky="ew", padx=6, pady=3)

        ttk.Button(form, text="Add Widget", command=self._add_widget).grid(
            row=7, column=1, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Update Widget", command=self._update_widget).grid(
            row=7, column=2, padx=6, pady=4, sticky="w"
        )
        ttk.Button(form, text="Remove Widget", command=self._remove_widget).grid(
            row=7, column=3, padx=6, pady=4, sticky="w"
        )

    # ------------------------
    # Helpers
    # ------------------------
    def _entry(self, parent: ttk.Frame, label: str, row: int, col: int = 0, col_span: int = 1) -> tk.Entry:
        ttk.Label(parent, text=label).grid(row=row, column=col, sticky="w")
        entry = ttk.Entry(parent)
        entry.grid(row=row, column=col + 1, columnspan=col_span, sticky="ew", padx=6, pady=3)
        parent.columnconfigure(col + 1, weight=1)
        return entry

    def _refresh_lists(self) -> None:
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
        item = self.data["data_tab"]["tabs"][idx]
        self.data_id.delete(0, tk.END)
        self.data_id.insert(0, item.get("id", ""))
        self.data_label.delete(0, tk.END)
        self.data_label.insert(0, item.get("label", ""))
        self.data_channels.delete(0, tk.END)
        self.data_channels.insert(0, ", ".join(item.get("channels", [])))
        chart = item.get("chart", {})
        self.data_chart.set(chart.get("enabled", True))

    def _add_data_item(self) -> None:
        item = {
            "id": self.data_id.get().strip(),
            "label": self.data_label.get().strip(),
            "channels": self._split_list(self.data_channels.get()),
            "chart": {"enabled": bool(self.data_chart.get())},
        }
        self.data["data_tab"]["tabs"].append(item)
        self._refresh_lists()

    def _update_data_item(self) -> None:
        idx = self._selected_index(self.data_list)
        if idx is None:
            return
        self.data["data_tab"]["tabs"][idx] = {
            "id": self.data_id.get().strip(),
            "label": self.data_label.get().strip(),
            "channels": self._split_list(self.data_channels.get()),
            "chart": {"enabled": bool(self.data_chart.get())},
        }
        self._refresh_lists()
        self.data_list.selection_set(idx)

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
        item = self.data["connection_tab"]["sections"][idx]
        self.conn_kind.set(item.get("kind", "board_status"))
        self.conn_title.delete(0, tk.END)
        self.conn_title.insert(0, item.get("title", ""))

    def _add_conn_item(self) -> None:
        self.data["connection_tab"]["sections"].append(
            {"kind": self.conn_kind.get(), "title": self.conn_title.get().strip()}
        )
        self._refresh_lists()

    def _update_conn_item(self) -> None:
        idx = self._selected_index(self.conn_list)
        if idx is None:
            return
        self.data["connection_tab"]["sections"][idx] = {
            "kind": self.conn_kind.get(),
            "title": self.conn_title.get().strip(),
        }
        self._refresh_lists()
        self.conn_list.selection_set(idx)

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
        self._refresh_lists()

    def _update_action_item(self) -> None:
        idx = self._selected_index(self.actions_list)
        if idx is None:
            return
        self.data["actions_tab"]["actions"][idx] = self._action_from_form()
        self._refresh_lists()
        self.actions_list.selection_set(idx)

    def _remove_action_item(self) -> None:
        idx = self._selected_index(self.actions_list)
        if idx is None:
            return
        self.data["actions_tab"]["actions"].pop(idx)
        self._refresh_lists()

    def _move_action_item(self, delta: int) -> None:
        self._move_list_item(self.data["actions_tab"]["actions"], self.actions_list, delta)

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
        self.state_states.delete(0, tk.END)
        self.state_states.insert(0, ", ".join(entry.get("states", [])))
        self.section_list.delete(0, tk.END)
        for s in entry.get("sections", []):
            self.section_list.insert(tk.END, s.get("title", "Section"))
        self.widget_list.delete(0, tk.END)
        if entry.get("sections"):
            self.section_list.selection_set(0)
            self._load_section()

    def _load_section(self) -> None:
        s_idx = self._selected_index(self.section_list)
        e_idx = self._selected_index(self.state_entry_list)
        if e_idx is None or s_idx is None:
            return
        section = self.data["state_tab"]["states"][e_idx]["sections"][s_idx]
        self.section_title.delete(0, tk.END)
        self.section_title.insert(0, section.get("title", ""))
        self.widget_list.delete(0, tk.END)
        for w in section.get("widgets", []):
            label = w.get("kind", "widget")
            if w.get("data_type"):
                label = f"{label} ({w.get('data_type')})"
            self.widget_list.insert(tk.END, label)
        if section.get("widgets"):
            self.widget_list.selection_set(0)
            self._load_widget()

    def _load_widget(self) -> None:
        e_idx = self._selected_index(self.state_entry_list)
        s_idx = self._selected_index(self.section_list)
        w_idx = self._selected_index(self.widget_list)
        if None in (e_idx, s_idx, w_idx):
            return
        widget = self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["widgets"][w_idx]
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

    def _add_state_entry(self) -> None:
        entry = {"states": self._split_list(self.state_states.get()), "sections": []}
        self.data["state_tab"]["states"].append(entry)
        self._refresh_lists()

    def _update_state_entry(self) -> None:
        idx = self._selected_index(self.state_entry_list)
        if idx is None:
            return
        self.data["state_tab"]["states"][idx]["states"] = self._split_list(
            self.state_states.get()
        )
        self._refresh_lists()
        self.state_entry_list.selection_set(idx)

    def _remove_state_entry(self) -> None:
        idx = self._selected_index(self.state_entry_list)
        if idx is None:
            return
        self.data["state_tab"]["states"].pop(idx)
        self._refresh_lists()

    def _add_section(self) -> None:
        e_idx = self._selected_index(self.state_entry_list)
        if e_idx is None:
            messagebox.showwarning("Selection required", "Select a state entry first.")
            return
        section = {"title": self.section_title.get().strip(), "widgets": []}
        self.data["state_tab"]["states"][e_idx]["sections"].append(section)
        self._load_state_entry()

    def _update_section(self) -> None:
        e_idx = self._selected_index(self.state_entry_list)
        s_idx = self._selected_index(self.section_list)
        if None in (e_idx, s_idx):
            messagebox.showwarning("Selection required", "Select a state entry and section first.")
            return
        self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["title"] = (
            self.section_title.get().strip()
        )
        self._load_state_entry()
        self.section_list.selection_set(s_idx)

    def _remove_section(self) -> None:
        e_idx = self._selected_index(self.state_entry_list)
        s_idx = self._selected_index(self.section_list)
        if None in (e_idx, s_idx):
            messagebox.showwarning("Selection required", "Select a state entry and section first.")
            return
        self.data["state_tab"]["states"][e_idx]["sections"].pop(s_idx)
        self._load_state_entry()

    def _add_widget(self) -> None:
        e_idx = self._selected_index(self.state_entry_list)
        s_idx = self._selected_index(self.section_list)
        if None in (e_idx, s_idx):
            messagebox.showwarning("Selection required", "Select a state entry and section first.")
            return
        widget = self._widget_from_form()
        self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["widgets"].append(widget)
        self._load_section()

    def _update_widget(self) -> None:
        e_idx = self._selected_index(self.state_entry_list)
        s_idx = self._selected_index(self.section_list)
        w_idx = self._selected_index(self.widget_list)
        if None in (e_idx, s_idx, w_idx):
            messagebox.showwarning(
                "Selection required", "Select a state entry, section, and widget first."
            )
            return
        self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["widgets"][w_idx] = (
            self._widget_from_form()
        )
        self._load_section()
        self.widget_list.selection_set(w_idx)

    def _remove_widget(self) -> None:
        e_idx = self._selected_index(self.state_entry_list)
        s_idx = self._selected_index(self.section_list)
        w_idx = self._selected_index(self.widget_list)
        if None in (e_idx, s_idx, w_idx):
            messagebox.showwarning(
                "Selection required", "Select a state entry, section, and widget first."
            )
            return
        self.data["state_tab"]["states"][e_idx]["sections"][s_idx]["widgets"].pop(w_idx)
        self._load_section()

    # ------------------------
    # Utility
    # ------------------------
    def _selected_index(self, listbox: tk.Listbox) -> int | None:
        sel = listbox.curselection()
        return sel[0] if sel else None

    def _split_list(self, text: str) -> list[str]:
        return [s.strip() for s in text.split(",") if s.strip()]

    def _format_items(self, items: list[dict]) -> str:
        parts = []
        for item in items:
            label = item.get("label", "").strip()
            idx = item.get("index", "")
            if label:
                parts.append(f"{label}:{idx}")
        return ", ".join(parts)

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
        widget = {"kind": self.widget_kind.get()}
        if self.widget_data_type.get().strip():
            widget["data_type"] = self.widget_data_type.get().strip()
        if self.widget_chart_title.get().strip():
            widget["chart_title"] = self.widget_chart_title.get().strip()
        if self.widget_width.get().strip():
            try:
                widget["width"] = float(self.widget_width.get().strip())
            except ValueError:
                pass
        if self.widget_height.get().strip():
            try:
                widget["height"] = float(self.widget_height.get().strip())
            except ValueError:
                pass
        items = self._parse_items(self.widget_items.get())
        if items:
            widget["items"] = items
        return widget

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


if __name__ == "__main__":
    app = LayoutEditor()
    app.mainloop()
