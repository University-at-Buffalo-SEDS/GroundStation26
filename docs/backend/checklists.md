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
Changed item wording or sequence binding clears its old completion. Saving a stale template is rejected,
so concurrent crew progress cannot be overwritten by an administrator's older page.
Cancel and reopen management to reload after a conflict.

Launch enforcement is off initially. When enabled, every item in each selected
required checklist must be complete before either launch command is accepted.
Recovery is optional initially. This applies to the server command path and button
availability, adds no automatic abort after countdown begins, and never bypasses
existing safety interlocks. Checklist completion never replaces fresh sensor verification or hardware
qualification. Reset appropriate checks before a new operation.

API: GET `/api/checklists`, POST `/api/checklists/check` for an individual checkmark,
and admin-only POST `/api/checklists` for revision-checked template edits. Back up the
persistent data directory alongside calibration and user data.

## Automatic completion

In **Manage checklists**, choose **Complete after** for an item: valve self-test
passes, nitrogen test passes, or fill target reached. A successful sequence completes
every item linked to that result. New installations include a Sequence tests
checklist; existing templates are preserved and administrators can add these links.

Starting a test, a timeout, cancellation or a failed test does not complete its
items. Results are emitted only after the successful engine transition and durable
qualification save. A bounded persistent completion outbox retries after restart;
receipt IDs make retries idempotent, including after an operator clears an item.
A new successful run can complete a cleared item again. Progress and receipts use
the same atomic checklist transaction.

The UI distinguishes **Test passed · saved automatically** from a **Manual override**
when somebody checks a linked item themselves. A later actual pass replaces that
override with a test result. These checkmarks do not issue valve commands or bypass
backend interlocks. Explicit qualification/abort recovery resets clear linked items
and pending completions while preserving unrelated manual checks. HITL retains its
existing sequence behavior and does not emit operational qualification events.

This synchronizes clients of one station; GS/RRLS cross-station checklist merging
is a separate integration. Operational Fire consumes the nitrogen requirement
and clears only its linked checklist items through the same durable event/receipt
mechanism. The dedicated cold-flow override records override provenance in linked
items; simply checking a box never runs valves or changes operational qualification.
