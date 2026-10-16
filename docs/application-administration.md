# Application administration

The application `admin` role controls accounts and opens the administration
area from the dashboard. Stream-only administrators cannot manage accounts.
An application administrator does not automatically receive hardware command
or calibration permission.

Bootstrap an existing account locally, using the same users configuration
file as the backend:

```sh
python3 backend/tools/users_config_gui.py --help
python3 backend/tools/users_config_gui.py set-role USER admin grant
```

Check the tool's `--file` option when the service uses a nondefault path.
This grants data viewing and the administration role, not hardware control.
No account is created or elevated automatically by an upgrade.

The administration page supports account creation, disabling, permissions,
command restrictions, stream roles and password replacement. New passwords
must contain 12–128 characters. Password replacement invalidates existing
sessions; permission revocation also affects existing sessions. The API never
returns password hashes. At least one enabled administrator with data access
must remain.

Broadcast, dashboard-view editing and voice controls are linked from the
administration area and retain their own permissions. Creating an account
starts with viewing permission only; grant command access deliberately.
