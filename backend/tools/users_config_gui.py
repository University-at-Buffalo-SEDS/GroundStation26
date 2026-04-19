#!/usr/bin/env python3
from __future__ import annotations

import argparse
import base64
import getpass
import hashlib
import json
import os
import re
import secrets
import sys
from pathlib import Path
from typing import Any

DEFAULT_ITERATIONS = 120_000
DEFAULT_SESSION_TTL_SECONDS = 14 * 24 * 60 * 60
DEFAULT_USERS_PATH = Path(__file__).resolve().parents[1] / "users" / "users.json"
DEFAULT_COMMANDS_SOURCE = Path(__file__).resolve().parents[1] / "src" / "sequences.rs"


def normalize_permissions(value: dict[str, Any] | None) -> dict[str, bool]:
    value = dict(value or {})
    perms = {
        "view_data": bool(value.get("view_data", False)),
        "send_commands": bool(value.get("send_commands", False)),
    }
    if perms["send_commands"]:
        perms["view_data"] = True
    return perms


def hash_password(password: str, *, iterations: int = DEFAULT_ITERATIONS) -> dict[str, Any]:
    salt = secrets.token_bytes(16)
    derived = hashlib.pbkdf2_hmac("sha256", password.encode("utf-8"), salt, iterations, dklen=32)
    return {
        "algorithm": "pbkdf2_sha256",
        "iterations": iterations,
        "salt_b64": base64.b64encode(salt).decode("ascii"),
        "hash_b64": base64.b64encode(derived).decode("ascii"),
    }


def normalize_command_access(value: dict[str, Any] | None) -> dict[str, list[str]]:
    value = dict(value or {})
    allowed = []
    for item in value.get("allowed_commands", []):
        text = str(item).strip()
        if text:
            allowed.append(text)
    allowed = sorted(set(allowed))
    return {"allowed_commands": allowed}


def normalize_calibration_access(value: dict[str, Any] | None) -> dict[str, bool]:
    value = dict(value or {})
    access = {
        "view": bool(value.get("view", False)),
        "edit": bool(value.get("edit", False)),
    }
    if access["edit"]:
        access["view"] = True
    return access


def parse_command_csv(raw: str) -> list[str]:
    return normalize_command_access(
        {"allowed_commands": [part.strip() for part in raw.split(",")]}
    )["allowed_commands"]


def available_commands(source_path: Path = DEFAULT_COMMANDS_SOURCE) -> list[str]:
    try:
        raw = source_path.read_text(encoding="utf-8")
    except OSError:
        return []

    commands: list[str] = []
    for match in re.finditer(
            r"pub fn all_command_names\(\) -> Vec<&'static str>\s*\{\s*vec!\[(?P<body>.*?)\]\s*\}",
            raw,
            re.DOTALL,
    ):
        body = match.group("body")
        commands.extend(re.findall(r'"([^"]+)"', body))
    return sorted(set(commands))


def default_config() -> dict[str, Any]:
    return {
        "version": 1,
        "session_ttl_seconds": DEFAULT_SESSION_TTL_SECONDS,
        "anonymous": normalize_permissions({}),
        "anonymous_command_access": normalize_command_access({}),
        "anonymous_calibration_access": normalize_calibration_access({}),
        "users": [],
    }


def load_config(path: Path) -> dict[str, Any]:
    if not path.exists():
        cfg = default_config()
        save_config(path, cfg)
        return cfg
    with path.open("r", encoding="utf-8") as handle:
        cfg = json.load(handle)
    cfg.setdefault("version", 1)
    cfg["session_ttl_seconds"] = int(cfg.get("session_ttl_seconds", DEFAULT_SESSION_TTL_SECONDS))
    cfg["anonymous"] = normalize_permissions(cfg.get("anonymous"))
    cfg["anonymous_command_access"] = normalize_command_access(cfg.get("anonymous_command_access"))
    cfg["anonymous_calibration_access"] = normalize_calibration_access(
        cfg.get("anonymous_calibration_access")
    )
    users = []
    for raw_user in cfg.get("users", []):
        user = dict(raw_user)
        user["disabled"] = bool(user.get("disabled", False))
        user["permissions"] = normalize_permissions(user.get("permissions"))
        user["command_access"] = normalize_command_access(user.get("command_access"))
        user["calibration_access"] = normalize_calibration_access(user.get("calibration_access"))
        users.append(user)
    cfg["users"] = users
    return cfg


def save_config(path: Path, cfg: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        json.dump(cfg, handle, indent=2, sort_keys=False)
        handle.write("\n")


def upsert_user(
        cfg: dict[str, Any],
        username: str,
        *,
        password: str | None,
        view_data: bool,
        send_commands: bool,
        calibration_view: bool,
        calibration_edit: bool,
        disabled: bool,
        allowed_commands: list[str] | None,
) -> None:
    username = username.strip()
    if not username:
        raise ValueError("username is required")
    permissions = normalize_permissions(
        {"view_data": view_data, "send_commands": send_commands}
    )
    command_access = normalize_command_access({"allowed_commands": allowed_commands or []})
    calibration_access = normalize_calibration_access(
        {"view": calibration_view, "edit": calibration_edit}
    )
    existing = next((user for user in cfg["users"] if user["username"] == username), None)
    if existing is None:
        if password is None:
            raise ValueError("password is required when creating a user")
        cfg["users"].append(
            {
                "username": username,
                "password": hash_password(password),
                "permissions": permissions,
                "command_access": command_access,
                "calibration_access": calibration_access,
                "disabled": disabled,
            }
        )
        cfg["users"].sort(key=lambda item: item["username"].lower())
        return

    existing["permissions"] = permissions
    existing["command_access"] = command_access
    existing["calibration_access"] = calibration_access
    existing["disabled"] = disabled
    if password:
        existing["password"] = hash_password(password)


def remove_user(cfg: dict[str, Any], username: str) -> bool:
    before = len(cfg["users"])
    cfg["users"] = [user for user in cfg["users"] if user["username"] != username]
    return len(cfg["users"]) != before


def print_summary(cfg: dict[str, Any]) -> None:
    print(f"Session TTL: {cfg['session_ttl_seconds']} seconds")
    anon = cfg["anonymous"]
    print(
        f"Anonymous permissions: view_data={anon['view_data']} send_commands={anon['send_commands']}"
    )
    print(
        "Anonymous command access: "
        + (", ".join(cfg["anonymous_command_access"]["allowed_commands"]) or "all commands")
    )
    anon_cal = cfg["anonymous_calibration_access"]
    print(
        f"Anonymous calibration: view={anon_cal['view']} edit={anon_cal['edit']}"
    )
    if not cfg["users"]:
        print("No configured users.")
        return
    print("Users:")
    for user in cfg["users"]:
        perms = user["permissions"]
        print(
            f"  - {user['username']}: view_data={perms['view_data']} "
            f"send_commands={perms['send_commands']} disabled={user['disabled']} "
            f"allowed_commands={','.join(user['command_access']['allowed_commands']) or 'all'} "
            f"calibration_view={user['calibration_access']['view']} "
            f"calibration_edit={user['calibration_access']['edit']}"
        )


def cli_mode(args: argparse.Namespace) -> int:
    cfg = load_config(args.file)
    commands_catalog = available_commands()
    changed = False

    if args.command == "list":
        print_summary(cfg)
        return 0

    if args.command == "set-anonymous":
        cfg["anonymous"] = normalize_permissions(
            {"view_data": args.view_data, "send_commands": args.send_commands}
        )
        cfg["anonymous_command_access"] = normalize_command_access(
            {
                "allowed_commands": commands_catalog
                if args.select_all_commands
                else parse_command_csv(args.allowed_commands or "")
            }
        )
        cfg["anonymous_calibration_access"] = normalize_calibration_access(
            {"view": args.calibration_view, "edit": args.calibration_edit}
        )
        changed = True
    elif args.command == "set-ttl":
        cfg["session_ttl_seconds"] = max(1, int(args.seconds))
        changed = True
    elif args.command == "remove-user":
        if not remove_user(cfg, args.username):
            print(f"user not found: {args.username}", file=sys.stderr)
            return 1
        changed = True
    elif args.command in {"add-user", "edit-user"}:
        password = args.password
        if password is None and args.prompt_password:
            password = getpass.getpass("Password: ")
        upsert_user(
            cfg,
            args.username,
            password=password,
            view_data=args.view_data,
            send_commands=args.send_commands,
            calibration_view=args.calibration_view,
            calibration_edit=args.calibration_edit,
            disabled=args.disabled,
            allowed_commands=commands_catalog
            if args.select_all_commands
            else parse_command_csv(args.allowed_commands or ""),
        )
        changed = True
    else:
        raise ValueError(f"unsupported CLI command: {args.command}")

    if changed:
        save_config(args.file, cfg)
        print(f"Updated {args.file}")
    return 0


def tui_mode(path: Path) -> int:
    cfg = load_config(path)
    commands_catalog = available_commands()
    while True:
        print("\nUsers Config")
        print("1. List users")
        print("2. Add user")
        print("3. Edit user")
        print("4. Remove user")
        print("5. Set anonymous permissions")
        print("6. Set session TTL")
        print("7. Save and exit")
        print("8. Exit without saving")
        choice = input("> ").strip()

        try:
            if choice == "1":
                print_summary(cfg)
            elif choice == "2":
                username = input("Username: ").strip()
                password = getpass.getpass("Password: ")
                view_data = prompt_bool("Allow view data", True)
                send_commands = prompt_bool("Allow send commands", False)
                calibration_view = prompt_bool("Allow calibration view", False)
                calibration_edit = prompt_bool("Allow calibration edit", False)
                disabled = prompt_bool("Disabled", False)
                allowed_commands = prompt_command_selection(
                    "Allowed commands",
                    commands_catalog,
                    [],
                )
                upsert_user(
                    cfg,
                    username,
                    password=password,
                    view_data=view_data,
                    send_commands=send_commands,
                    calibration_view=calibration_view,
                    calibration_edit=calibration_edit,
                    disabled=disabled,
                    allowed_commands=allowed_commands,
                )
            elif choice == "3":
                username = input("Username to edit: ").strip()
                user = next((user for user in cfg["users"] if user["username"] == username), None)
                if user is None:
                    print("User not found.")
                    continue
                password = getpass.getpass("New password (leave blank to keep current): ").strip()
                perms = user["permissions"]
                calibration = user["calibration_access"]
                view_data = prompt_bool("Allow view data", perms["view_data"])
                send_commands = prompt_bool("Allow send commands", perms["send_commands"])
                calibration_view = prompt_bool(
                    "Allow calibration view", calibration["view"]
                )
                calibration_edit = prompt_bool(
                    "Allow calibration edit", calibration["edit"]
                )
                disabled = prompt_bool("Disabled", bool(user.get("disabled", False)))
                allowed_commands = prompt_command_selection(
                    "Allowed commands",
                    commands_catalog,
                    user["command_access"]["allowed_commands"],
                )
                upsert_user(
                    cfg,
                    username,
                    password=password or None,
                    view_data=view_data,
                    send_commands=send_commands,
                    calibration_view=calibration_view,
                    calibration_edit=calibration_edit,
                    disabled=disabled,
                    allowed_commands=allowed_commands,
                )
            elif choice == "4":
                username = input("Username to remove: ").strip()
                if not remove_user(cfg, username):
                    print("User not found.")
            elif choice == "5":
                anon = cfg["anonymous"]
                cfg["anonymous"] = normalize_permissions(
                    {
                        "view_data": prompt_bool("Anonymous view data", anon["view_data"]),
                        "send_commands": prompt_bool(
                            "Anonymous send commands", anon["send_commands"]
                        ),
                    }
                )
                cfg["anonymous_command_access"] = normalize_command_access(
                    {
                        "allowed_commands": prompt_command_selection(
                            "Anonymous allowed commands",
                            commands_catalog,
                            cfg["anonymous_command_access"]["allowed_commands"],
                        )
                    }
                )
                anon_cal = cfg["anonymous_calibration_access"]
                cfg["anonymous_calibration_access"] = normalize_calibration_access(
                    {
                        "view": prompt_bool(
                            "Anonymous calibration view", anon_cal["view"]
                        ),
                        "edit": prompt_bool(
                            "Anonymous calibration edit", anon_cal["edit"]
                        ),
                    }
                )
            elif choice == "6":
                raw = input(
                    f"Session TTL seconds [{cfg['session_ttl_seconds']}]: "
                ).strip() or str(cfg["session_ttl_seconds"])
                cfg["session_ttl_seconds"] = max(1, int(raw))
            elif choice == "7":
                save_config(path, cfg)
                print(f"Saved {path}")
                return 0
            elif choice == "8":
                return 0
        except (ValueError, KeyboardInterrupt) as exc:
            print(f"Error: {exc}")


def prompt_bool(label: str, default: bool) -> bool:
    suffix = "Y/n" if default else "y/N"
    raw = input(f"{label} [{suffix}]: ").strip().lower()
    if not raw:
        return default
    return raw in {"y", "yes", "1", "true", "on"}


def prompt_command_selection(
        label: str,
        commands_catalog: list[str],
        current: list[str],
) -> list[str]:
    if not commands_catalog:
        raw = input(
            f"{label} CSV (blank = all commands) [{','.join(current)}]: "
        ).strip()
        return parse_command_csv(raw or ",".join(current))

    print(f"{label}:")
    for idx, command in enumerate(commands_catalog, start=1):
        marker = "*" if command in current else " "
        print(f"  {idx:>2}. [{marker}] {command}")
    raw = input(
        "Choose command numbers separated by commas, 'all' for every command, 'none' for no commands,"
        " or press Enter to keep the current selection"
        f" [{','.join(current) or 'all'}]: "
    ).strip()
    if not raw:
        return list(current)
    lowered = raw.lower()
    if lowered == "all":
        return commands_catalog.copy()
    if lowered == "none":
        return []

    indexes = []
    for part in raw.split(","):
        text = part.strip()
        if not text:
            continue
        idx = int(text)
        if idx < 1 or idx > len(commands_catalog):
            raise ValueError(f"command selection out of range: {idx}")
        indexes.append(idx - 1)
    return [commands_catalog[idx] for idx in sorted(set(indexes))]


def selected_commands(listbox: Any) -> list[str]:
    return [listbox.get(i) for i in listbox.curselection()]


def set_selected_commands(listbox: Any, commands: list[str]) -> None:
    wanted = set(commands)
    listbox.selection_clear(0, "end")
    for idx in range(listbox.size()):
        if listbox.get(idx) in wanted:
            listbox.selection_set(idx)


def select_all_commands(listbox: Any) -> None:
    if listbox.size() > 0:
        listbox.selection_set(0, "end")


def gui_mode(path: Path) -> int:
    import tkinter as tk
    from tkinter import messagebox, ttk

    cfg = load_config(path)
    commands_catalog = available_commands()

    root = tk.Tk()
    root.title("Groundstation Users")
    root.geometry("860x520")

    users_var = tk.StringVar(value=[user["username"] for user in cfg["users"]])
    selected_username = tk.StringVar()
    username_var = tk.StringVar()
    password_var = tk.StringVar()
    ttl_var = tk.StringVar(value=str(cfg["session_ttl_seconds"]))
    anon_view_var = tk.BooleanVar(value=cfg["anonymous"]["view_data"])
    anon_send_var = tk.BooleanVar(value=cfg["anonymous"]["send_commands"])
    anon_calibration_view_var = tk.BooleanVar(
        value=cfg["anonymous_calibration_access"]["view"]
    )
    anon_calibration_edit_var = tk.BooleanVar(
        value=cfg["anonymous_calibration_access"]["edit"]
    )
    user_view_var = tk.BooleanVar(value=True)
    user_send_var = tk.BooleanVar(value=False)
    user_calibration_view_var = tk.BooleanVar(value=False)
    user_calibration_edit_var = tk.BooleanVar(value=False)
    user_disabled_var = tk.BooleanVar(value=False)

    def refresh_user_list() -> None:
        users_var.set([user["username"] for user in cfg["users"]])

    def on_select(_event: object | None = None) -> None:
        selection = listbox.curselection()
        if not selection:
            return
        username = cfg["users"][selection[0]]["username"]
        selected_username.set(username)
        user = next(user for user in cfg["users"] if user["username"] == username)
        username_var.set(user["username"])
        password_var.set("")
        user_view_var.set(bool(user["permissions"]["view_data"]))
        user_send_var.set(bool(user["permissions"]["send_commands"]))
        user_calibration_view_var.set(bool(user["calibration_access"]["view"]))
        user_calibration_edit_var.set(bool(user["calibration_access"]["edit"]))
        user_disabled_var.set(bool(user.get("disabled", False)))
        set_selected_commands(user_commands_listbox, user["command_access"]["allowed_commands"])

    def save_all() -> None:
        try:
            cfg["session_ttl_seconds"] = max(1, int(ttl_var.get().strip()))
        except ValueError:
            messagebox.showerror("Invalid TTL", "Session TTL must be an integer.")
            return
        cfg["anonymous"] = normalize_permissions(
            {"view_data": anon_view_var.get(), "send_commands": anon_send_var.get()}
        )
        cfg["anonymous_command_access"] = normalize_command_access(
            {"allowed_commands": selected_commands(anon_commands_listbox)}
        )
        cfg["anonymous_calibration_access"] = normalize_calibration_access(
            {
                "view": anon_calibration_view_var.get(),
                "edit": anon_calibration_edit_var.get(),
            }
        )
        save_config(path, cfg)
        messagebox.showinfo("Saved", f"Saved {path}")

    def save_all_event(_event: object | None = None) -> str:
        save_all()
        return "break"

    def save_user() -> None:
        username = username_var.get().strip()
        password = password_var.get()
        try:
            upsert_user(
                cfg,
                username,
                password=password or None,
                view_data=user_view_var.get(),
                send_commands=user_send_var.get(),
                calibration_view=user_calibration_view_var.get(),
                calibration_edit=user_calibration_edit_var.get(),
                disabled=user_disabled_var.get(),
                allowed_commands=selected_commands(user_commands_listbox),
            )
        except ValueError as exc:
            messagebox.showerror("User Error", str(exc))
            return
        refresh_user_list()
        password_var.set("")
        selected_username.set(username)

    def save_user_event(_event: object | None = None) -> str:
        save_user()
        return "break"

    def new_user() -> None:
        selected_username.set("")
        username_var.set("")
        password_var.set("")
        user_view_var.set(True)
        user_send_var.set(False)
        user_calibration_view_var.set(False)
        user_calibration_edit_var.set(False)
        user_disabled_var.set(False)
        user_commands_listbox.selection_clear(0, tk.END)
        listbox.selection_clear(0, tk.END)

    def delete_user() -> None:
        username = username_var.get().strip() or selected_username.get().strip()
        if not username:
            return
        if not messagebox.askyesno("Delete User", f"Delete user '{username}'?"):
            return
        remove_user(cfg, username)
        refresh_user_list()
        new_user()

    root.columnconfigure(1, weight=1)
    root.rowconfigure(0, weight=1)

    left = ttk.Frame(root, padding=12)
    left.grid(row=0, column=0, sticky="ns")
    ttk.Label(left, text="Users").pack(anchor="w")
    listbox = tk.Listbox(left, listvariable=users_var, height=18, width=24)
    listbox.pack(fill="y", expand=True, pady=(8, 8))
    listbox.bind("<<ListboxSelect>>", on_select)
    ttk.Button(left, text="New User", command=new_user).pack(fill="x", pady=(0, 6))
    ttk.Button(left, text="Delete User", command=delete_user).pack(fill="x")

    right_shell = ttk.Frame(root, padding=12)
    right_shell.grid(row=0, column=1, sticky="nsew")
    right_shell.columnconfigure(0, weight=1)
    right_shell.rowconfigure(0, weight=1)

    right_canvas = tk.Canvas(right_shell, highlightthickness=0, borderwidth=0)
    right_scrollbar = ttk.Scrollbar(
        right_shell, orient="vertical", command=right_canvas.yview
    )
    right_canvas.configure(yscrollcommand=right_scrollbar.set)
    right_canvas.grid(row=0, column=0, sticky="nsew")
    right_scrollbar.grid(row=0, column=1, sticky="ns")

    right = ttk.Frame(right_canvas, padding=12)
    right.columnconfigure(1, weight=1)
    right_window = right_canvas.create_window((0, 0), window=right, anchor="nw")

    def sync_right_scrollregion(_event: object | None = None) -> None:
        right_canvas.configure(scrollregion=right_canvas.bbox("all"))

    def sync_right_width(event: object) -> None:
        width = getattr(event, "width", None)
        if width is not None:
            right_canvas.itemconfigure(right_window, width=width)

    def on_mousewheel(event: object) -> str:
        delta = getattr(event, "delta", 0)
        if delta:
            right_canvas.yview_scroll(int(-delta / 120), "units")
        return "break"

    def on_linux_scroll_up(_event: object) -> str:
        right_canvas.yview_scroll(-1, "units")
        return "break"

    def on_linux_scroll_down(_event: object) -> str:
        right_canvas.yview_scroll(1, "units")
        return "break"

    right.bind("<Configure>", sync_right_scrollregion)
    right_canvas.bind("<Configure>", sync_right_width)
    right_canvas.bind_all("<MouseWheel>", on_mousewheel)
    right_canvas.bind_all("<Button-4>", on_linux_scroll_up)
    right_canvas.bind_all("<Button-5>", on_linux_scroll_down)

    ttk.Label(right, text="Username").grid(row=0, column=0, sticky="w")
    ttk.Entry(right, textvariable=username_var).grid(row=0, column=1, sticky="ew", pady=(0, 8))
    ttk.Label(right, text="Password").grid(row=1, column=0, sticky="w")
    ttk.Entry(right, textvariable=password_var, show="*").grid(
        row=1, column=1, sticky="ew", pady=(0, 8)
    )
    ttk.Checkbutton(right, text="View Data", variable=user_view_var).grid(
        row=2, column=0, sticky="w"
    )
    ttk.Checkbutton(right, text="Send Commands", variable=user_send_var).grid(
        row=2, column=1, sticky="w"
    )
    ttk.Checkbutton(right, text="Calibration View", variable=user_calibration_view_var).grid(
        row=3, column=0, sticky="w"
    )
    ttk.Checkbutton(right, text="Calibration Edit", variable=user_calibration_edit_var).grid(
        row=3, column=1, sticky="w"
    )
    ttk.Checkbutton(right, text="Disabled", variable=user_disabled_var).grid(
        row=4, column=0, sticky="w", pady=(0, 10)
    )
    ttk.Label(right, text="Allowed Commands").grid(row=5, column=0, sticky="nw")
    user_commands_listbox = tk.Listbox(
        right,
        selectmode=tk.MULTIPLE,
        exportselection=False,
        height=min(max(len(commands_catalog), 6), 12),
    )
    user_commands_listbox.grid(row=5, column=1, sticky="ew", pady=(0, 8))
    for command in commands_catalog:
        user_commands_listbox.insert(tk.END, command)
    user_cmd_buttons = ttk.Frame(right)
    user_cmd_buttons.grid(row=6, column=1, sticky="w", pady=(0, 8))
    ttk.Button(
        user_cmd_buttons,
        text="Select All",
        command=lambda: select_all_commands(user_commands_listbox),
    ).pack(side=tk.LEFT)
    ttk.Button(
        user_cmd_buttons,
        text="Clear",
        command=lambda: user_commands_listbox.selection_clear(0, tk.END),
    ).pack(side=tk.LEFT, padx=(8, 0))
    ttk.Button(right, text="Save User", command=save_user).grid(
        row=7, column=0, columnspan=2, sticky="ew", pady=(0, 18)
    )

    ttk.Separator(right, orient="horizontal").grid(
        row=8, column=0, columnspan=2, sticky="ew", pady=(0, 12)
    )

    ttk.Label(right, text="Anonymous Permissions").grid(row=9, column=0, sticky="w")
    ttk.Checkbutton(right, text="View Data", variable=anon_view_var).grid(
        row=10, column=0, sticky="w"
    )
    ttk.Checkbutton(right, text="Send Commands", variable=anon_send_var).grid(
        row=10, column=1, sticky="w"
    )
    ttk.Checkbutton(
        right, text="Calibration View", variable=anon_calibration_view_var
    ).grid(row=11, column=0, sticky="w")
    ttk.Checkbutton(
        right, text="Calibration Edit", variable=anon_calibration_edit_var
    ).grid(row=11, column=1, sticky="w")
    ttk.Label(right, text="Anonymous Allowed Commands").grid(row=12, column=0, sticky="nw")
    anon_commands_listbox = tk.Listbox(
        right,
        selectmode=tk.MULTIPLE,
        exportselection=False,
        height=min(max(len(commands_catalog), 6), 12),
    )
    anon_commands_listbox.grid(row=12, column=1, sticky="ew", pady=(0, 8))
    for command in commands_catalog:
        anon_commands_listbox.insert(tk.END, command)
    set_selected_commands(
        anon_commands_listbox, cfg["anonymous_command_access"]["allowed_commands"]
    )
    anon_cmd_buttons = ttk.Frame(right)
    anon_cmd_buttons.grid(row=13, column=1, sticky="w", pady=(0, 8))
    ttk.Button(
        anon_cmd_buttons,
        text="Select All",
        command=lambda: select_all_commands(anon_commands_listbox),
    ).pack(side=tk.LEFT)
    ttk.Button(
        anon_cmd_buttons,
        text="Clear",
        command=lambda: anon_commands_listbox.selection_clear(0, tk.END),
    ).pack(side=tk.LEFT, padx=(8, 0))
    ttk.Label(right, text="Session TTL Seconds").grid(row=14, column=0, sticky="w", pady=(10, 0))
    ttk.Entry(right, textvariable=ttl_var).grid(row=14, column=1, sticky="ew", pady=(10, 0))
    ttk.Button(right, text="Save Config", command=save_all).grid(
        row=15, column=0, columnspan=2, sticky="ew", pady=(16, 0)
    )

    root.bind_all("<Control-s>", save_all_event)
    root.bind_all("<Command-s>", save_all_event)
    root.bind_all("<Control-S>", save_user_event)
    root.bind_all("<Command-S>", save_user_event)

    root.mainloop()
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Manage backend users.json with GUI, TUI, or CLI modes."
    )
    parser.add_argument("--file", type=Path, default=DEFAULT_USERS_PATH, help="Path to users.json")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--gui", action="store_true", help="Force Tk GUI mode")
    mode.add_argument("--tui", action="store_true", help="Force terminal UI mode")
    mode.add_argument("--cli", action="store_true", help="Force CLI subcommand mode")

    sub = parser.add_subparsers(dest="command")
    sub.add_parser("list", help="Print current config")

    anon = sub.add_parser("set-anonymous", help="Set anonymous permissions")
    anon.add_argument("--view-data", action="store_true")
    anon.add_argument("--send-commands", action="store_true")
    anon.add_argument("--calibration-view", action="store_true")
    anon.add_argument("--calibration-edit", action="store_true")
    anon.add_argument("--allowed-commands", help="Comma-separated allowed commands")
    anon.add_argument(
        "--select-all-commands",
        action="store_true",
        help="Allow every parsed backend command",
    )

    ttl = sub.add_parser("set-ttl", help="Set session TTL in seconds")
    ttl.add_argument("seconds", type=int)

    remove = sub.add_parser("remove-user", help="Delete a user")
    remove.add_argument("username")

    for name in ("add-user", "edit-user"):
        cmd = sub.add_parser(name, help=f"{name.replace('-', ' ').title()}")
        cmd.add_argument("username")
        cmd.add_argument("--password")
        cmd.add_argument("--prompt-password", action="store_true")
        cmd.add_argument("--view-data", action="store_true")
        cmd.add_argument("--send-commands", action="store_true")
        cmd.add_argument("--calibration-view", action="store_true")
        cmd.add_argument("--calibration-edit", action="store_true")
        cmd.add_argument("--disabled", action="store_true")
        cmd.add_argument("--allowed-commands", help="Comma-separated allowed commands")
        cmd.add_argument(
            "--select-all-commands",
            action="store_true",
            help="Allow every parsed backend command",
        )

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    if args.cli or args.command:
        return cli_mode(args)
    if args.tui:
        return tui_mode(args.file)
    if args.gui:
        return gui_mode(args.file)

    has_display = bool(os.environ.get("DISPLAY") or sys.platform in {"win32", "darwin"})
    if has_display:
        try:
            return gui_mode(args.file)
        except Exception as exc:  # pragma: no cover - fallback path
            print(f"GUI failed ({exc}); falling back to TUI.", file=sys.stderr)
    return tui_mode(args.file)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("\nUsers config interrupted.", file=sys.stderr)
        raise SystemExit(130)
