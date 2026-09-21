# DAQ SD sessions and Flight Computer OTA

GroundStation retains `DAQ_LOG_CLOCK` with its other network variables in
`backend/data/network_variables.json` (or `GS_NETWORK_VARIABLE_CACHE`). It is
published to `SD_CARD`, the fill-system logger endpoint, as two little-endian
u64 values: session ID and absolute network-UTC close deadline. Zeroes reset
normal recording. This endpoint does not represent storage on Actuator.

Launch acceptance starts a DAQ launch file. Valve's sequence confirmation and
Pilot-open T0 correct the deadline without changing the session ID. The deadline
is T+120 seconds, not two minutes after pressing Launch. Persisting the absolute
deadline prevents a GroundStation service restart from extending the run.
DAQ subscribes using SEDSNet network variables, so reconnects retrieve the cached
value. Duplicate values do not create another file. Update DAQ firmware as well
as GroundStation to use this protocol.

Flight Computer is now an available OTA target, but first needs the new combined
bootloader/application image installed over a wired programmer. Its staging area
is only 16 KiB; only deltas which fit can be sent through the OTA UI. A full-image
`.seds` fallback is not a live OTA upload. Use a wired factory flash when a delta
does not fit, or when the installed base image differs.

Validation: backend HITL tests exercise retained launch-clock delivery across
router recreation, disk-cache restart, T-minus/T-plus deadline conversion and OTA
target eligibility. Firmware tests cover the receiver and real LaunchCore delta
installer independently. Physical SD power loss and hardware OTA still require
hardware qualification.
