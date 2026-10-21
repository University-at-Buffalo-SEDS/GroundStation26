# Application administration

The application `admin` role controls accounts and opens the administration
area from the dashboard. Stream-only administrators cannot manage accounts.
Application administrators inherit every application permission, including commands,
calibration, voice and stream management. Hardware interlocks still apply.

Bootstrap an existing account locally, using the same users configuration
file as the backend:

```sh
python3 backend/tools/users_config_gui.py --help
python3 backend/tools/users_config_gui.py set-role USER admin grant
```

Check the tool's `--file` option when the service uses a nondefault path.
This grants the administration role and its full application access.
No account is created or elevated automatically by an upgrade.

The administration page supports account creation, disabling, permissions,
command restrictions, stream roles and password replacement. New passwords
must contain 12–128 characters. Password replacement invalidates existing
sessions; permission revocation also affects existing sessions. The API never
returns password hashes. At least one enabled administrator with data access
must remain.

Broadcast, dashboard-view editing and voice controls are linked from the
administration area. Other roles retain their explicit permissions. Creating an account
starts with viewing permission only; grant command access deliberately.

## Terminal management over SSH

No graphical desktop or Tk installation is needed:

```sh
python3 backend/tools/users_config_tui.py
# A custom service configuration:
python3 backend/tools/users_config_tui.py --file /path/to/users.json
```

Use `ssh -t` for an interactive terminal. The existing
`python3 backend/tools/users_config_gui.py --tui` entry point also works.
The menu supports accounts, passwords, permissions, role assignment, command
restrictions, calibration access and session lifetime. Password entry is hidden.
Menu **9** edits roles; **7** saves, **8** exits without saving, and **10** reloads.
Changes are local until saved. A stale save is rejected if another administrator
changed the file. Saves replace the file atomically; a new file is owner-only.

The list shows effective permission flags for application admins. Stored flags
remain available when an admin role is revoked. The terminal and CLI tools refuse
to delete, disable or demote the last enabled application admin. Bootstrap a first
admin with `set-role USER admin grant`; granting command access alone does not
make an account an admin. Role and permission changes apply to existing sessions;
reload the dashboard to refresh the controls it displays. Use local `sudo` only
when required by the service users-file ownership.
