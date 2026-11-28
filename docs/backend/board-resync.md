# Board Resync

Resync brings one board's application state back in line with the station's
current phase without resetting it to Startup or Idle. The typical case: the
station is in a fill phase or Armed, and the Actuator Board or Valve Board
rebooted or desynced and latched Abort. Resync puts that board back into the
station's phase. Review recovery returns a board to Idle (or Startup), and
Reset all boards sends everything to Startup and then Idle.

## What it changes

- **Only the board's state machine and phase metadata.** The board's flight
  state becomes the station phase, its Abort latch is cleared, and its
  recovery epoch advances by one. The durable flight-state record, recovery
  floor and bootstrap marker are written the same way as for any recovery.
- **No outputs move.** Resync never commands, replays or restores valves,
  servos, supplies, the igniter or the stepper. The board refuses unless its
  commanded outputs already read safe (Actuator Board: everything off and the
  stepper stopped and asleep; Valve Board: vent and dump open, pilot off). That
  is the state any board Abort leaves behind. After a resync, the operator
  commands outputs explicitly.
- The station's own phase is never changed or republished.

## When it is allowed

| Station phase | Resync |
|---|---|
| Pre Fill, Fill Test, Nitrogen Fill, Nitrous Fill, Armed | Allowed |
| Startup | Refused. The station moves to Idle on its own. |
| Idle | Refused. Use Review recovery (Recover to Idle). |
| Aborted | Refused. Use Review recovery or Reset all boards. |
| Launch and later | Refused. Nothing aborts, recovers or acts automatically after Launch, and the Flight Computer owns its flight states. |

It is also refused while:

- a tank overpressure is latched, at the station or freshly reported by the
  Valve Board. The board's Abort may still be protecting the tank. Acknowledge
  the overpressure when the pressure is below relief, then use Review recovery
  or Reset all boards. The Valve Board firmware enforces the same rule: a
  latched relief refuses any target past Idle;
- a GSE sequence is moving valves, cold flow is running, an operation or
  automatic abort is running, a manual Abort is being delivered, or an
  ABORT/VALVE/ACTUATOR/FLIGHT command is still waiting for an acknowledgement;
- the board's outputs do not read safe, the board needs commissioning, or it
  already matches the station with no Abort latched.

Each refusal is shown as plain text in the board card (`resync_blocked_reason`
in `/api/board-recovery/status`) and returned by PREPARE as
`resync_unavailable` with the same message.

## Boards

Resync is implemented for the **Actuator Board** and the **Valve Board**. A board
advertises it with bit 7 (`0x80`) of its recovery readback flags, and the station
shows Resync (`capabilities.resync`) only when the board's last readback carries
that bit.

The other boards are not resynced:

- **Flight Computer.** It owns its flight states, and its recovery endpoint
  supports only Startup.
- **Gateway, DAQ, RF and Power Boards.** Their cached phase is metadata that
  already follows the station's published `FLIGHT_STATE`, and their durable
  recovery history holds only Startup. Adding a phase target would change
  that durable format on four boards for no operational gain. Use Reset
  metadata to Startup.

## Safety flow

Resync uses the same coordinator as Review recovery (`board_recovery/coordinator.rs`,
`exchange.rs`), with the same proof rules.

1. **PREPARE** (`action: "resync"`) takes a fresh dedicated query of the board,
   freezes the board and the station phase, and returns a 30-second server
   ticket. The typed confirmation names both, for example
   `RESYNC AB TO NITROUSFILL`. The ticket includes `resync_target_state`,
   `resync_target_label` and `outputs_unchanged: true`.
2. **CONFIRM** rechecks the ticket, principal, mode, Stop epoch and history
   snapshot, and that the station is still in the frozen phase. It seals one
   RESYNC request: wire operation 2 with the phase in the desired byte, bound to
   the prepare query's boot generation, recovery epoch and issuer, and to the
   confirmation's network-UTC source time. As with recovery, no new query is
   sent between PREPARE and the board request, because the board accepts
   recovery only against the context from its most recent query.
3. The board checks boot generation, epoch, nonce, issuer and source time,
   writes its durable records, and completes only through its control owner.
   The station accepts completion only from a fresh final readback in the
   frozen phase, with the Abort latch clear, the epoch advanced by one and
   the same safe outputs.

The UI re-prepares when Confirm is clicked (`reconfirm_prepare`), so reading
time does not use up the proof window. It sends the confirmation only if the
fresh ticket has the same action, board and confirmation text, so a station
that changed phase in the meantime is refused.

Like every recovery, Resync is journaled (the sealed intent carries operation 2)
and advances the station's recovery generation, so automatic decisions in
flight are re-evaluated. Automatic-action inhibition returns to its earlier
setting afterwards.

## Compatibility

A station build older than this change rejects a readback with flag bit 7 set.
Update the station before flashing Actuator or Valve Board firmware that
advertises Resync. A new station works with old firmware: Resync stays hidden.
Older board firmware rejects a bootstrap marker whose stored target is past
Idle. A board downgraded after a resync therefore keeps controls inhibited
until it is reset.
