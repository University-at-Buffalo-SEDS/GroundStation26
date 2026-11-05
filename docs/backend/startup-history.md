# Durable startup history paths

The provided `run_groundstation.py` and `run_rrls.py` launchers run the backend with the repository root as its working directory. With no `GS_BOARD_ABORT_HISTORY_DIR` override, startup verifies that layout and creates only the immediate `backend/data` directory when missing. Existing directory permissions and all journal contents are preserved. The history Store provisions a missing journal with all seven boards uncertain; a fresh directory is never proof of nominal hardware state.

Direct `cargo run` should also run from the repository root (`cargo run -p groundstation_backend`). Running it from `backend/` with default paths is rejected rather than creating `backend/backend/data` and a second history. Startup does not change the process working directory.

`GS_BOARD_ABORT_HISTORY_DIR` remains an exact administrator-selected path. Its parent must already exist; a misspelled hierarchy is not created or rewritten. For a custom service working directory, explicitly configure both it and `GS_LAUNCH_CLOCK_STATE` to retain stable journal locations. Containers and installed services should mount/create their selected parent directory before launch. This is configuration, not permission to copy, delete or reset an existing journal.

The supplied RRLS service currently uses its repository root as `WorkingDirectory`. Its `StateDirectory=rrls` alone does not move history into `/var/lib/rrls`; the abort and launch journal paths are independent of `GS_USERS_FILE`. Existing service upgrades keep their current history location. Any future migration into the state directory requires a separate explicit migration plan.

Malformed, incomplete, tombstoned or identity-mismatched history remains an error. This startup helper protects its default-base creation with fd-relative no-follow directory operations and syncs the data directory and its parent. It does not claim to remove all replacement races from the unchanged history Store implementation.
