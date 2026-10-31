# Checklist review and sign-off

Completing individual items and approving a checklist are separate actions.
Every checklist needs one authorized reviewer to sign off. Application admins
inherit this permission; an admin can grant **Sign off checklists** to another
signed-in account with data access. Command permission alone does not grant it.

The server records the authenticated username, server UTC timestamp in
milliseconds, reviewed document revision and SHA-256 digest of the exact
checklist contents and item completion records. Clients cannot supply reviewer
identity, time or approvals through template editing. A stale revision or any
incomplete item rejects approval. Authorization is checked again on each request.

When launch enforcement is enabled, every required checklist must have all items
complete and a current sign-off. Optional checklists do not block launch, but
still display their approval state. Existing installations retain their launch
enforcement choice and load existing documents as unsigned; old item completion
records remain intact. Signing never runs a sequence or changes physical outputs.

Changing a checklist title, required flag, item definition, item order or item
completion record invalidates its existing sign-off. Removing the checklist also
invalidates it. The prior reviewer and time remain in the audit history with an
invalidation time and reason. Restoring previous text or rechecking an item does
not reactivate a prior sign-off: the updated checklist must be reviewed again.
Unrelated checklists retain their approvals. Failed persistence leaves the
in-memory version unchanged. Sign-off uses the same file fsync, atomic rename
and directory fsync as checklist progress.

Paired stations keep approvals local to the station where the review occurred;
remote progress or template synchronization cannot grant local operational
qualification. Local approval history survives paired projection updates and
restart, and shared content/progress changes invalidate it. The current station
must be durably synchronized before signing. Export each station's audit when
collecting reviews from multiple stations. Automatic cross-station approval
replication is not part of this change.

The Checklists view provides **Export checklists** (full JSON including items and
approval audit history) and **Export sign-offs CSV** (reviewers, timestamps,
reviewed revisions/digests, unsigned checklists and invalidated approvals).
Exports use the authenticated data-viewing session and escape CSV text for
spreadsheet use. These are checklist exports; telemetry recording exports retain
their existing format.

Authenticated API:

- `POST /api/checklists/sign`: `{"checklist_id":"setup","revision":42}`.
- `GET /api/checklists/export`: full document JSON attachment.
- `GET /api/checklists/export.csv`: sign-off audit CSV attachment.

`permissions.sign_checklists` defaults to false for non-admin accounts. Old
configuration and session JSON remain readable without a database migration.
Audit history is bounded to 1,024 approvals and the existing 1 MiB document limit.
When full, new approvals fail rather than silently dropping old audit records;
export and archive the document before starting a fresh operational checklist.
