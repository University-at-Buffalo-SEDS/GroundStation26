# Repository Guidelines

## Project Structure & Module Organization

- `backend/` is the Axum backend (`backend/src`). Runtime data lives under `backend/data`, and calibration files live
  under `backend/calibration`.
- `frontend/` is the Dioxus UI (`frontend/src`), with assets in `frontend/assets` and `frontend/static`. Bundled output
  goes to `frontend/dist`.
- `shared/` contains Rust crates shared by frontend and backend.
- `map_downloader/` holds the Rust crate for map-related utilities; `download_map.py` fetches map data into `data/`.
- Root tooling includes `build.py`, `run_groundstation.py`, and `docker-compose.yml` for orchestration.

## Build, Test, and Development Commands

- `python3 build.py` builds the frontend web bundle and backend in parallel.
- `python3 build.py testing|hitl-mode|debug|backend_only|frontend_web` enables common local build modes.
- `python3 build.py docker` builds Docker images; add `pi_build` for Raspberry Pi (`python3 build.py docker pi_build`).
- `python3 build.py ios|ios_sim|macos|windows|android|linux` builds a platform-specific frontend bundle.
- `python3 run_groundstation.py [testing|hitl-mode]` builds the frontend and runs the backend.
- `python3 run_groundstation.py --testing|--hitl-mode` is the preferred explicit form.
- `python3 download_map.py` downloads map data needed by the UI.
- `cargo build -p groundstation_backend --release` builds the backend only.
- `cargo test` runs workspace tests (add `-p <crate>` for a single crate).

## Coding Style & Naming Conventions

- Rust uses 4-space indentation; format with `cargo fmt` (rustfmt defaults).
- Naming: `snake_case` for modules/functions, `CamelCase` for types, `SCREAMING_SNAKE_CASE` for constants.
- Keep feature flags explicit in `Cargo.toml` and avoid platform-specific code paths outside guarded modules.

## Testing Guidelines

- Use `cargo test` from the repo root or a crate directory.
- Place unit tests in `#[cfg(test)]` modules or add integration tests under `tests/`.
- There is no stated coverage target; add tests alongside new logic when feasible.

## Commit & Pull Request Guidelines

- Commit history favors short, imperative, lower-case messages (e.g., "fix build script").
- PRs should include a concise description, any linked issues, and screenshots for UI changes.
- Call out new data files or config changes explicitly.

## Configuration & Runtime Notes

- Set the device identifier in `.cargo/config.toml`.
- The backend expects a reverse proxy with TLS for geolocation; `docker-compose.yml` provides a self-signed setup for
  local testing.
