# Per-account client settings

`GET /api/account/client-settings/{namespace}` and `POST` to the same path
store a small display-preference document for the signed-in account. The only
namespace is `watch` (the Apple Watch / Wear OS companion layout, cycle items,
compass options and update rate; see the frontend `docs/watch-companion.md`).

* Requires a signed-in, non-anonymous session with telemetry view access.
  Anonymous sessions get HTTP 403; unknown namespaces 404.
* An account reads and replaces only its own document. The backend treats the
  document as opaque client data: a JSON object, at most 8 KiB, nesting depth
  at most 8. It is never interpreted and has no effect on telemetry, commands,
  radio links, launch state or other accounts.
* `GET` returns `{"revision":N,"settings":{...}|null,"updated_at_ms":...|null}`
  (`revision` 0 and `settings` null when nothing is stored).
* `POST {"expected_revision":N,"settings":{...}}` replaces the document when
  `N` is the current revision and returns the new document. A stale revision
  returns HTTP 409 with the current document so the client can reconcile.
* Stored atomically in `backend/data/client_settings.json` (or
  `GS_CLIENT_SETTINGS_FILE`), at most 512 accounts. The file is runtime data and
  is not committed. GroundStation and RRLS keep separate copies: the phone
  syncs with whichever station it is signed in to.
