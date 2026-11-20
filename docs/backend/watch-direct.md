# Watch direct link

The Apple Watch and Wear OS companions normally get their data from the phone
app. iOS suspends the phone app in the background, so the watch can also talk
to the station itself over its own network (Wi-Fi, LTE, or the system's
Bluetooth proxy through the phone). Code: `backend/src/watch.rs`, watch-token
handling in `backend/src/auth.rs`. Client side: the frontend
`docs/watch-companion.md`.

## Watch token

`POST /api/auth/watch-token` `{"device_id":"<phone install id>","commands":["Abort","Dump"],"voice":true}`
with a signed-in phone session returns
`{"token":"gsw1.…","device_id":…,"expires_at_ms":…,"commands":[granted],"voice":true|false}`.

* Issued only to a named, non-anonymous session; never to a watch token.
* `commands` is narrowed to the commands the account may send now **and**
  that are watch-eligible (below), deduplicated, at most 16. A viewer gets a
  read-only token.
* `voice` (optional, default false) asks for crew voice. It is granted only
  when the account may transmit on crew voice now (`voice_transmit`, or the
  admin role), and stored with the commands
  (`allowed_commands_json` = `{"commands":[…],"voice":true}`; a plain array
  means no voice).
* One token per account and `device_id`: issuing again replaces the old
  one. The token never outlives the issuing session (and at most 30 days).
* Only a SHA-256 digest is stored in `auth_sessions`
  (`session_type = watch|<device>|<issuing-session fingerprint>`).
* Revocation: `POST /api/auth/watch-token/revoke` `{"device_id":…}` (or `{}`
  for every watch token of the account); signing the issuing phone session
  out (`/api/auth/logout`) revokes its watch tokens; the watch may sign
  itself out with its own token; disabling or deleting the account stops
  the token at once.

A watch token resolves to a narrow principal: no roles (so no administrator
authority), view data, and only the granted commands that are still
watch-eligible and still allowed for the account; crew voice only when it
was granted and the account still has voice permission; no calibration,
checklist, camera, stream-management or clock permissions. The router
additionally limits watch tokens to `GET /api/watch/status`,
`POST /api/command`, `POST /api/auth/logout`, `POST /api/proximity/report`,
`GET /api/voice/status`, `GET /api/voice/ws` and
`GET /tiles/{z}/{x}/{y}[.jpg]` (decimal indices only); every other request
with a watch token (including `/ws?token=` and any query-string token) gets
HTTP 403. The voice socket authenticates inside the connection, so
`voice.rs` itself refuses a watch token without the voice grant, and the
audience commentary socket refuses every watch token. See
`docs/backend/crew-voice.md` "Watches" for the codec and the phone/watch
echo rule.

The tile endpoint is read-only: it serves the station's offline map bundle
(the same tiles the map tab shows) and no telemetry. The watch uses it for
the compass map background in direct mode, fetching only the few tiles of
its current view that the phone has not already sent it (at most six at a
time, a missing tile not again for a minute).

## Watch-eligible actions

Watch pages may carry at most two action buttons each, chosen on the phone
from `GET /api/watch/actions` (signed-in phone session):

```json
{"actions":[{"cmd":"Abort","label":"Abort","section":"safety","abort":true,"enabled":true},
            {"cmd":"Dump","label":"Dump","section":"fill_system","abort":false,"enabled":false,"reason":"…"}],
 "excluded":[{"cmd":"Launch","reason":"Launch-class actions are not offered on the watch."}],
 "max_per_page":2}
```

The list holds the current mode's Actions-tab commands the account may send,
minus the excluded ones:

* Launch class: `Launch`, `GroundStationLaunch`, `ResetLaunchLatch`,
  `TogglePhysicalLaunchMode`, `ToggleLaunchInterlock`;
* pyro: `Igniter`, `IgniterSequence`, `DeployParachute`, `ExpandParachute`;
* `RecoverStation` (an HTTP action with its own flow) and the HITL bench
  (Testing section) controls.

`watch::WATCH_SAFE_OVERRIDES` is the explicit list that would mark one of
those watch-safe; it is empty. A press is an ordinary `POST /api/command`
with the watch token, so authorization, the action policy, mode gating and
admission are exactly those of a phone press. Abort is always pressable.

## Status

`GET /api/watch/status?units=metric|imperial&actions=Abort,Dump&stale_after_s=30`
(watch token or any session with view access) returns the phone snapshot's
data keys in a few KiB:

```json
{"v":1,"seq":<server ms>,"mode":"live","source":"direct",
 "station":{"name":"GroundStation","role":"primary","link":"connected","address":"","telemetry_age_ms":350},
 "items":{"altitude":{"label":"Altitude","text":"1234","value":1234.0,"unit":"m","age_ms":200,"stale":false,"level":"normal"}},
 "targets":[{"id":"rocket","label":"Rocket","lat":…,"lon":…,"alt_m":…,"age_ms":3000,"stale":false,"source":"Rocket GPS","fallback":false}],
 "actions":{"Abort":{"enabled":true},"Dump":{"enabled":false,"reason":"…"}},
 "link_timeout_s":15}
```

Read-only; it changes nothing. The item list mirrors the phone registry
(`watch.rs ITEMS`; keep it in sync with the frontend
`watch_companion/registry.rs`). `actions` reports only commands the caller
may send. The watch takes pages, settings and the catalog from the phone's
last snapshot.
