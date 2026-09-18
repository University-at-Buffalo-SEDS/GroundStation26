# GSE pressure limits and connection status

Automatic nitrogen checkout uses the GSE nitrogen target plus **100 psi** as
its failure threshold. Nitrous filling uses the nitrous pressure target from
the fill-target settings plus **50 psi**. Nitrous pressure is captured when a
new fill starts, and retained while paused/resumed. A configured hardware
pressure ceiling can only lower either threshold. Reaching the threshold
requests supply closure and dump/vent opening; it is not a substitute for
independent hardware pressure protection.

The optional hardware ceiling is no longer required just to enable automation.
The maximum acceptable empty-tank pressure-transducer offset still must be
configured by the operator. Fresh pressure, valve confirmations, interlocks,
dry-self-test confirmation and nitrogen-test completion remain enforced.
The GSE settings panel exposes the offset and hardware ceiling. Action-panel
diagnostics show configuration errors and the last rejection reason.

Software command duplicate suppression uses monotonic elapsed time, independent
of GPS/network time corrections. Rejected duplicate clicks do not extend the
window. Different command payloads, including explicit close commands, have
separate keys, and Abort is never suppressed by this check.

Cached topology is not evidence that downstream boards remain powered. Board
status uses actual sender observations and aged discovery announcers, not
repeated reads of connection names. All seven remote boards expire using the
same liveness timeout. The graph retains known offline nodes and displays their
last-seen timestamp and age; these are not connection-session uptime values.

Tests cover target boundaries, lower hardware caps, invalid targets, stale
pressure, missing valve acknowledgements, command debounce, and expiration of
every remote board. Hardware actuation and Raspberry Pi UI validation remain
separate deployment checks.
