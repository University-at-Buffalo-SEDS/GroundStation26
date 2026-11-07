# DAQ launch log clock

The primary GroundStation publishes retained `DAQ_LOG_CLOCK` to `SD_CARD` with
reliable ordered delivery. Its 16-byte payload is two little-endian UInt64
values: a nonzero recording-session ID and the absolute network-UTC close
instant. Reset publishes `[0,0]`. A durable, monotonic revision is carried in the
packet timestamp; it is independent of UTC corrections.

The close instant is **T0 + 20 seconds**, not 20 seconds after pressing Launch:

- Countdown: T0 is the countdown anchor plus its duration (normally ten seconds).
- Observed pilot opening/T+: T0 is the first accepted T+ anchor. A correction
  changes the deadline/revision while retaining the recording-session ID.
- HITL/test-fire command acceptance prepares the DAQ recording session before
  Valve's sequence-start confirmation starts the visible countdown.

`AppState::set_launch_clock` saves the primary clock and calls
`publish_daq_launch_clock` → `network_variables::set_daq_log_clock`. The literal
pair and revision are saved before SDK submission. If local SDK admission
fails, the newest desired pair remains pending and is retried on the existing
heartbeat cadence (default 500 ms; configurable). Retry preserves the original
session, deadline and revision. A newer T0/reset supersedes older pending work;
completion of an old attempt cannot clear it. After successful admission there
is no periodic DAQ-clock refresh: SDK reliable delivery and managed-variable
reconnect replay own the accepted value. Startup seeds the saved literal pair
and revision instead of calculating a new deadline. RRLS never originates this
publication or retry.

SDK admission is not proof that the physical DAQ has received or written the
recording. Firmware uses the session to rotate launch records into its special
file and enforces the close instant against its mapped monotonic clock. Queued
records retain their acquisition time/session: a pre-cutoff record may finish
writing after T+20, while a later record belongs to the ordinary log. The close
window is a capture boundary, not a requirement that the card has flushed by
that exact instant.

Valid UTC is required to date this window. A Pi with an invalid wall clock and
no usable network UTC receives an explicit publication error; GroundStation
does not invent a UTC anchor or add a new launch interlock. Router initialization
precedes operational request handling, so a missing router is a pre-start/test
case. Confirm the station clock before a test fire without AV-bay GPS time.

An already expired/closed session must not reopen from retained replays. Severe
loss or delay of the pilot/T0 confirmation beyond the estimated close window
can therefore prevent that same session from collecting a newly extended
launch window. The ordinary log and protected pressure recording remain the
fallback; this change does not relax the closed-session rule or launch policy.

Host regression tests exercise actual AppState publication, SDK SD-side packets
and a receiving managed-variable callback, including first-admission rejection,
retry, corrected T0, superseding reset, late old revisions and literal restart
seeding. They do not constitute a physical card or hardware qualification.

DAQ firmware with `DAQ_LAUNCH_LOG_STATUS` reports application-level acceptance
separately from transport delivery. Every 5 seconds it emits seven UInt32 words:
actual SD session low/high, actual SD deadline UTC low/high, receiver-accepted
revision low/high, then flags. Flag bits are 0 services initialized, 1 SD ready,
2 deadline anchored, 3 cutoff reached, 4 current raw launch file open, 5 current
telemetry launch file open, and 6 final flush pending. Integer values retain their
full 64-bit precision; base `BOARD_HEALTH` and `DAQ_SD_HEALTH` formats are unchanged.

`/api/network_topology` exposes this as DAQ diagnostics `launch_log`, including
`session`, `deadline_utc_ms`, `revision`, `flags`, and independent
`received_age_ms`. For valid network UTC captures this age includes transport
queueing delay already present on arrival, then advances monotonically; a report
captured 30 seconds ago cannot appear fresh when delivered now. Captures more
than 100 ms ahead of selected SDK network UTC are rejected; a smaller lead is
clamped to zero age. Unsynchronized boot timestamps retain receipt-time fallback.
Optional `expected_session`, `expected_deadline_utc_ms`, and
`expected_revision` are the exact persisted station intent. `matches_expected`
compares the complete pair and revision, including an explicit reset; it is null
when no persisted intent exists. A matching stale report is still stale, and
ordinary/base/SD reports never refresh its receipt time. UI confirmation requires
a fresh matching report, not merely an SDK ACK. Files-open/ready/cutoff/flush flags
remain separate observations: acceptance is not proof of a successful card write,
lossless capture, or completion of all queued flushes. Older firmware provides no
application-level confirmation. This status adds no automatic launch gate.
