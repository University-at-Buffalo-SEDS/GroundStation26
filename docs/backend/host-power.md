# Power off and reboot the station

Administrators can cleanly power off or reboot the station host from
**Settings › Administration › Station power**. It applies only to the station
the app is connected to (the primary, or an RRLS for its own Pi); the panel
names that station (`GS_STATION_NAME`, default "Ground Station" / "RRLS").

## Refusals

| State | Refused | FORCE allowed |
| --- | --- | --- |
| Armed through landing (Launch, Ascent, …, InFlight) | yes | no |
| A launch capture still recording | yes | yes |
| Fill states (PreFill … NitrousFill) | yes | yes |
| Any station sequence running | yes | yes |
| Integrated ground test active | yes | yes |
| Aborted | yes | yes |

FORCE needs the operator to tick an explicit override in the confirm dialog;
the request is logged at error level and broadcast as a warning with the
administrator's name. Every request, admitted or refused, is logged with who
and when.

## What happens

1. Every client is told the station is powering off / rebooting.
2. Every open event capture is ended now and saved with its manifest.
3. The current rolling segment is checkpointed and closed (a new one opens,
   which the normal shutdown closes again).
4. Pending state is persisted (settings are saved when changed).
5. The host is asked to power off or reboot.

If the host request fails the station says so and keeps recording.

## Host mechanism (one-time setup)

`GS_HOST_POWER` selects how the host is asked:

* `auto` (default): `path-unit` when the helper below is installed, otherwise
  the panel shows "Not supported here" and nothing is attempted (development
  machines).
* `path-unit`: the backend writes `data/host-power/request` (`poweroff` or
  `reboot`). A host systemd path unit runs `systemctl poweroff|reboot`. This
  works from the Docker container with no extra privileges or mounts.
* `systemctl`: the backend runs `systemctl poweroff|reboot` itself (a native
  service running as root, like `rrls.service`, or a polkit rule for its user).
* `off`: always unsupported.

Install the helper once on the Pi (as root):

```sh
sudo deploy/host-power/install.sh /path/to/GroundStation26
```

It installs `/usr/local/sbin/gs-host-power.sh`, `gs-host-power.path` and
`gs-host-power.service`, enables the path unit and writes the
`data/host-power/helper-installed` marker the backend looks for. The helper
removes each request before acting (no reboot loop) and ignores requests
older than two minutes.
