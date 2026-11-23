# Raw stream feed

A read-only feed of every live telemetry stream for broadcast overlays (OBS
browser sources), external dashboards and scripts. It carries the same decoded
and derived rows the live UI receives (`AppState::ws_tx`), one JSON object per
row, with field names, units, three timestamps and provenance.

Code: `backend/src/raw_stream.rs` (feed, catalog, token endpoints),
`backend/src/auth.rs` (stream viewer tokens). Schema id: `gs26.raw-stream.v1`.

## Endpoints

| Method and path | Who | What |
| --- | --- | --- |
| `GET /api/stream/raw/catalog` | stream token or any session with view data | Every stream seen since the backend started, with rate and latest values |
| `GET /api/stream/raw/ws` | same | WebSocket feed |
| `GET /api/stream/raw` | same | The same feed as NDJSON (`application/x-ndjson`) over one HTTP response |
| `POST /api/admin/stream-tokens` | application administrator | Issue a stream viewer token |
| `GET /api/admin/stream-tokens` | application administrator | List tokens (never the token itself) |
| `POST /api/admin/stream-tokens/{id}/revoke` | application administrator | Revoke one token |

The token goes in `Authorization: Bearer <token>` or, for browsers and OBS
(which cannot set WebSocket headers), in `?token=<token>`. The header wins
when both are present.

### Query parameters (feed and catalog)

| Parameter | Meaning | Default | Limits |
| --- | --- | --- | --- |
| `types` | Comma list of data types; an entry ending in `*` is a prefix (`LOADCELL_*`). Case-insensitive | all | 64 entries; `A-Z a-z 0-9 _ - . @ :` |
| `senders` | Comma list of senders (`DAQ`, `VB`, `AB`, `GW`, `FC`, `RF`, `PB`, `GS`…), prefixes allowed | all | 64 entries |
| `max_hz` | Rate cap per stream (one `data_type` + `sender` pair) | 50 | 0.1–1000; never above the token's `max_hz` |
| `max_rows_per_s` | Cap on all rows of the connection | 5000 | 1–20000 |

An invalid value is a `400` with a message. Unknown parameters are ignored
(cache-busting parameters in overlay URLs are harmless).

A token's own `types`/`senders` scope is intersected with the request: a row
must match both, so a request can only narrow what the token allows.

## Messages

### `hello` (first message)

```json
{"ty":"hello","schema":"gs26.raw-stream.v1","server_time_ms":1767225600000,
 "viewer":{"kind":"stream_token","id":"0123456789abcdef","label":"OBS","expires_at_ms":1769817600000},
 "filters":{"types":[],"senders":["DAQ"],"scope_types":["KG*"],"scope_senders":[]},
 "limits":{"max_hz":20.0,"max_rows_per_s":5000,"queue_rows":4096,"flush_ms":50,"status_ms":1000},
 "provenance":{"station":"live","igt_replay":false,"mode":"Normal"}}
```

`viewer.kind` is `stream_token` or `session` (no account name is sent).
`provenance.station` is `sim` when the backend runs on simulated firmware or
the internal flight simulator; `igt_replay` is true while the FC flies an
integrated-ground-test replay.

### Row

```json
{"ty":"row","seq":42,"data_type":"DAQ_LOADCELL_CHAMBER","sender":"DAQ",
 "received_ms":1767225600500,"source_ms":1767225600480,"network_ms":null,
 "values":[0.25,312.5,null],
 "fields":["kg1000_pin","chamber_pressure","chamber_connector"],
 "units":["V","psig","V"],"provenance":"live"}
```

| Field | Meaning |
| --- | --- |
| `seq` | Per connection, from 1. A gap means rows were dropped by backpressure (below) |
| `data_type`, `sender` | The stream. Sender ids are canonical board ids |
| `received_ms` | GS receipt: the host wall clock (Unix ms) when the packet worker handled the packet (`TelemetryRow.timestamp_ms`, `telemetry_task::get_current_timestamp_ms`, which is host system time, not network time). Same value the UI and recordings use |
| `source_ms` | The board's packet timestamp on SEDSnet network UTC, only when the GS has evidence it is UTC (`telemetry_provenance::source`); otherwise `null` |
| `network_ms` | The station's SEDSnet network clock (Unix ms) when the feed handled the row, `null` when the station has no network time. `network_ms - source_ms` approximates board-to-feed latency |
| `values` | Numbers or `null` (missing, withheld or non-finite) in the stream's channel order |
| `fields`, `units` | Channel names and units, same length as the documented channels; `[]` for a type without a table entry (raw payload rows such as `BOARD_HEALTH`) |
| `provenance` | `live`, `sim` (simulated station) or `replay` (FC replay data: `SIM_*` rows, and FC `FLIGHT_STATE` while the FC flies a replay) |

`values` may be shorter or longer than `fields` if a board sends a different
length; trust `values` and use `fields` as labels where they line up.

On the WebSocket rows arrive in batches, `{"ty":"rows","rows":[row,…]}`, about
every 50 ms. On NDJSON every row is its own line.

### `status` (every second; also the heartbeat)

```json
{"ty":"status","sent":1520,"dropped_rate_cap":310,"dropped_backpressure":0,"lagged":0,"queued":0,"server_time_ms":1767225601000}
```

Counters are cumulative for the connection.

### `bye` (last message)

```json
{"ty":"bye","reason":"revoked"}
```

Reasons: `revoked` (token revoked or session signed out), `expired`,
`issuer_removed` (the issuing administrator was disabled, deleted or lost the
administrator role), `server_error`. The WebSocket then closes; the NDJSON
response ends.

### Catalog

```json
{"schema":"gs26.raw-stream.v1","server_time_ms":1767225600000,"network_time_ms":1767225600002,
 "provenance":{"station":"live","igt_replay":false,"mode":"Normal"},
 "viewer":{"kind":"session"},
 "streams":[{"data_type":"FUEL_TANK_PRESSURE","sender":"VB","rows":15020,"rate_hz":50.0,
   "last_received_ms":1767225599990,"last_source_ms":1767225599975,"values":[512.3],
   "fields":["tank_pressure_raw"],"units":["psi"],"provenance":"live"}]}
```

`rate_hz` is rows per second over the last five whole seconds; `rows` counts
since the backend started. Streams are sorted by type, then sender, and are
filtered by the token scope and the request's `types`/`senders`.

## Limits, rate caps and backpressure

- **Per-stream cap (`max_hz`).** A row passes when at least `1/max_hz` has
  elapsed since that stream's last passed row. Otherwise it is held, replacing
  any older held row (the replaced row counts in `dropped_rate_cap`). The held
  row is sent as soon as the window opens, so the newest value of a capped
  stream is delayed by at most `1/max_hz`, never lost.
- **Total cap (`max_rows_per_s`).** A token bucket of `max_rows_per_s` rows,
  refilled continuously, burst one second. Rows beyond it count in
  `dropped_rate_cap`.
- **Backpressure.** Each connection has a queue of 4,096 rows. When the client
  reads too slowly the oldest queued row is dropped (`dropped_backpressure`,
  and a gap in `seq`). Ingest, the UI and other feed clients never wait for a
  slow feed client. A client that accepts nothing for 10 s is disconnected.
- **Fan-out lag.** If the feed task itself falls behind the station's
  telemetry broadcast, the rows it missed are counted in `lagged`.
- Status and auth: status every 1 s; the token is re-checked every 1 s, so a
  revocation ends an open feed within about a second.

## Stream viewer tokens

- Issued from the UI (the **Raw streams** tab, administrators only) or by
  `POST /api/admin/stream-tokens` with an administrator session:

  ```json
  {"label":"OBS lower third","ttl_hours":72,"types":["LOADCELL_*","PRESSURE_TRANSDUCER_CALIBRATED"],"senders":[],"max_hz":20}
  ```

  `label` 1–80 characters (required); `ttl_hours` 1–8760, default 720 (30
  days); `types`/`senders` as the query parameters (empty = all); `max_hz`
  0.1–1000 (optional cap). Reply (the token is shown only here):

  ```json
  {"id":"9f2c1d0e4b7a6c35","token":"gss1.…","label":"OBS lower third","issuer":"admin",
   "created_ms":1767225600000,"expires_at_ms":1767484800000,
   "types":["LOADCELL_*","PRESSURE_TRANSDUCER_CALIBRATED"],"senders":[],"max_hz":20.0}
  ```

- `GET /api/admin/stream-tokens` lists
  `{id,label,issuer,created_ms,expires_at_ms,types,senders,max_hz,last_used_ms}`
  (`last_used_ms` is in memory, `null` after a restart or before first use).
- `POST /api/admin/stream-tokens/{id}/revoke` replies `{"revoked":1}`.
- Format `gss1.` + 32 random bytes (URL-safe base64). Only a SHA-256 digest is
  stored (`auth_sessions`, row key `gss1h:<hex>`, `session_type`
  `stream|<id>`); the scope is in the same row.
- A token is valid while its row exists, it has not expired and its issuer is
  still an enabled application administrator. Resetting the issuer's password
  or deleting the account removes the issuer's sessions and with them its
  stream tokens. A stream token signing out (`POST /api/auth/logout` with it
  in the header) removes only its own row.
- **Scope.** A stream token reaches only the three feed endpoints and its own
  logout. Every other endpoint answers `403` (router layer
  `raw_stream::scope_stream_tokens`), and `auth::authorize_token` refuses it
  for every ordinary permission as well, so it can never send a command, open
  `/ws`, read `/api/recent`, media, voice, settings or admin endpoints.

## Privacy

The feed sends only decoded telemetry rows, their schema and counters. It
never sends secrets, tokens (the hello names a token by id and label), account
names, user data, command endpoints or command results. No device or person
location exists server side (the proximity lock receives near/far flags, never
coordinates); a denylist (`raw_stream::denied`: `GS_PROXIMITY_LOCK` and any
type containing `USER`, `DEVICE_LOCATION`, `PERSONNEL`, `PROXIMITY` or
`STATION_POSITION`) keeps such rows out if one is ever added. Vehicle
telemetry such as the rocket's `GPS_DATA` is included. Query-string tokens are
redacted from logged URLs (`log_redaction::redact_urls`); the feed never logs
a token.

## Examples

### OBS browser source

Add a Browser source with a local file or any static URL containing:

```html
<!doctype html>
<meta charset="utf-8">
<style>
  body { margin: 0; font: 600 48px system-ui; color: #fff; text-shadow: 0 2px 6px #000; background: transparent; }
  .stale { opacity: .45; }
</style>
<div id="tank">Tank — psi</div>
<div id="mass">Fill — kg</div>
<script>
const GS = "wss://groundstation.local";        // the station address
const TOKEN = "gss1.REPLACE_ME";                 // a stream viewer token
const url = `${GS}/api/stream/raw/ws?token=${encodeURIComponent(TOKEN)}` +
            `&types=PRESSURE_TRANSDUCER_CALIBRATED,LOADCELL_50_WEIGHT_KG&max_hz=5`;
const seen = {};
function connect() {
  const ws = new WebSocket(url);
  ws.onmessage = (event) => {
    const msg = JSON.parse(event.data);
    if (msg.ty === "rows") for (const row of msg.rows) {
      seen[row.data_type] = Date.now();
      const v = row.values[0];
      if (row.data_type === "PRESSURE_TRANSDUCER_CALIBRATED")
        tank.textContent = `Tank ${v == null ? "—" : v.toFixed(0)} ${row.units[0]}`;
      if (row.data_type === "LOADCELL_50_WEIGHT_KG")
        mass.textContent = `Fill ${v == null ? "—" : v.toFixed(2)} ${row.units[0]}`;
    }
    if (msg.ty === "bye") ws.close();
  };
  ws.onclose = () => setTimeout(connect, 2000);
}
setInterval(() => {
  tank.classList.toggle("stale", Date.now() - (seen.PRESSURE_TRANSDUCER_CALIBRATED || 0) > 5000);
  mass.classList.toggle("stale", Date.now() - (seen.LOADCELL_50_WEIGHT_KG || 0) > 5000);
}, 500);
connect();
</script>
```

A self-signed station certificate must be trusted by the machine running OBS
(or use `ws://` on port 3000 on a trusted network).

### curl and jq

```sh
TOKEN=gss1.REPLACE_ME
curl -sN -H "Authorization: Bearer $TOKEN" \
  "https://groundstation.local/api/stream/raw?types=KG*,LOADCELL_*&max_hz=10" -k \
  | jq -c 'select(.ty=="row") | {t: .received_ms, type: .data_type, v: .values[0], u: .units[0]}'

# Which streams exist and how fast
curl -s -k -H "Authorization: Bearer $TOKEN" https://groundstation.local/api/stream/raw/catalog \
  | jq -r '.streams[] | "\(.data_type)\t\(.sender)\t\(.rate_hz) Hz"'
```

### Python

```python
import json, requests

URL = "https://groundstation.local/api/stream/raw"
TOKEN = "gss1.REPLACE_ME"
params = {"types": "FUEL_TANK_PRESSURE,VALVE_STATE", "max_hz": "20"}
with requests.get(URL, params=params, stream=True, verify=False, timeout=30,
                  headers={"Authorization": f"Bearer {TOKEN}"}) as response:
    response.raise_for_status()
    for line in response.iter_lines():
        if not line:
            continue
        msg = json.loads(line)
        if msg["ty"] == "row":
            print(msg["received_ms"], msg["data_type"], dict(zip(msg["fields"], msg["values"])))
        elif msg["ty"] == "status" and msg["dropped_backpressure"]:
            print("reading too slowly:", msg["dropped_backpressure"], "rows dropped")
        elif msg["ty"] == "bye":
            print("feed ended:", msg["reason"])
            break
```

With the `websockets` package:

```python
import asyncio, json, ssl, websockets

async def main():
    ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    url = "wss://groundstation.local/api/stream/raw/ws?token=gss1.REPLACE_ME&senders=FC"
    async with websockets.connect(url, ssl=ctx) as ws:
        async for text in ws:
            msg = json.loads(text)
            if msg["ty"] == "rows":
                for row in msg["rows"]:
                    print(row["data_type"], row["values"], row["provenance"])

asyncio.run(main())
```

## Tests

`cargo test -p groundstation_backend raw_stream` covers token scope (403 on
commands, `/ws`, `/api/recent`, admin, media and voice with header and query
tokens), administrator-only issuing, revocation/expiry/issuer loss (including
an open NDJSON stream and an open WebSocket ending with `bye`), filters and
scope intersection, both rate caps, backpressure, the catalog, and exact
snapshots of the row and hello messages.
