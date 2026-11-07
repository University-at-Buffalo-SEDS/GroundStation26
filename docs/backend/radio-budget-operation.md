# RFD application airtime allocation

The default raw-UART profile budgets 32 kbit/s of usable application capacity
on a 64 kbit/s modem link. UART remains 115200 baud, with a maximum 100 ms
transmit batch followed by 55 ms of serial silence. CTS remains disabled.
These are application settings; loading the profile does not change modem
registers or verify their live values. There are no LoRa uplink/downlink turn
windows in this RFD configuration.

Deploy matching GroundStation, RRLS and RF firmware together. The RF profile
uses a 35% application share and supplies a read-only profile response. The
GroundStation/RRLS pair negotiates the remaining allocation:

| State | GroundStation | RF | RRLS |
| --- | ---: | ---: | ---: |
| Startup or unconfirmed peer | 15% | 35% | 35% |
| Direct path, no RRLS heard for 30 s | 25% | 35% | 35% |
| Confirmed direct path | 25% | 35% | 35% |
| Confirmed relay path | 15% | 35% | 50% |

Without an RRLS the GroundStation no longer stays at 15%: after 30 s on the
direct path with no RRLS budget message (Offer, HighAck or BaseAck), it uses
25%, the same share as a confirmed direct path (25 + 35 + RRLS base 35 = 95%).
Any RRLS budget message returns it to 15% until the normal handshake confirms.
30 s is far longer than the 2 s relay lease, so an RRLS that once held 50% has
already dropped to its base share. With `GS_RRLS_ROLE=off` nothing changes.

RRLS enters the higher allocation only after matching RF application-profile
confirmation and a current GroundStation grant. Returning to the direct path
requires RRLS to reduce its allocation before acknowledging the change.
Allocation changes preserve outstanding byte debt and the UART quiet interval.
Budget/profile exchanges consume the same capacity as other traffic. Setup exchanges
can run between complete physical frames of a discovery transfer, while queued
commands, delivery receipts and incomplete physical writes retain priority.
An unreserved revocation refers to a fully drained current health report; once
reserved, its bytes remain fixed through refusal or partial-write retries.

Older peers without this exchange remain at conservative allocations. They
have not been qualified for the same timing bounds. An incompatible
effective UART or application pacing override pauses radio transmission and
exposes the mismatch in `/api/rrls/status` under `recovery_budget`. Check
`effective_transport_compatible`, `outbound_paused`, `fault_reason`,
`expected_tuple` and `actual_effective_tuple`. The reported
`requested_share_percent` is a policy request, not a modem readback.

The recovery envelopes use application-private kinds 13–15; these are not new
SEDSnet reserved message IDs. Host qualification used public crates.io SEDSnet
4.3.4 under the version 4 requirement; this is not a permanent exact version pin.
The separately qualified MCU cohort uses scoped discovery recovery from SEDSnet
4.3.5. Default board builds select SDK `main`, currently 4.3.4. The MCU changes
must be merged and released on SDK `main` before a default build contains them;
existing caches and resulting board artifacts still need source verification.
Host and MCU SDKs need not have identical versions, but their wire compatibility
and source provenance must be checked.

## Framing and retry ownership

GroundStation validates complete CRC-protected recovery commands before
consuming their declared length. Otherwise a truncated command can absorb a
following valid frame. The GroundStation parser searches for a fully validated recovery frame only
inside an already rejected span, using its bounded recovery scan. Valid or
incomplete ordinary parent frames do not grant this search permission.

Failed physical writes retain their original owners for retry. An unrelated
successful write cannot release them; an unstarted SDK-retired write can be
discarded through the normal retirement accounting.

These changes address application framing and admission. Hardware validation
still requires matched modem readbacks and command/acknowledgment timing; a
simulator pass does not establish that a physical RF link is loss-free.
