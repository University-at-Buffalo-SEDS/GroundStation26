# Shared checklists

Open the Checklists tab. Every signed-in crew member can check or uncheck items.
GroundStation saves each change atomically in `backend/data/checklists.json`
(`GS_CHECKLISTS_FILE` overrides this), including the completing account and timestamp.
Open tabs refresh from the backend every second. Different-item updates merge on the
server; the latest accepted update wins when two people change the same item.
There is no browser-owned checklist state.

Application admins can manage multiple checklists, names, individual items and the
required-before-launch setting. Setup, Pre-launch, Launch and Recovery are initial
editable templates, not a fixed workflow. Add up to 32 checklists and 1024 total items.
Changed item wording clears its old completion. Saving a stale template is rejected,
so concurrent crew progress cannot be overwritten by an administrator's older page.
Cancel and reopen management to reload after a conflict.

Launch enforcement is off initially. When enabled, every item in each selected
required checklist must be complete before either launch command is accepted.
Recovery is optional initially. This applies to the server command path and button
availability, adds no automatic abort after countdown begins, and never bypasses
existing safety interlocks. Checklists are operator acknowledgements, not sensor
verification or hardware qualification. Reset appropriate checks before a new operation.

API: GET `/api/checklists`, POST `/api/checklists/check` for an individual checkmark,
and admin-only POST `/api/checklists` for revision-checked template edits. Back up the
persistent data directory alongside calibration and user data.
