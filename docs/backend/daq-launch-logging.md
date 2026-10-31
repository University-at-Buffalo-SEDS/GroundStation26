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
