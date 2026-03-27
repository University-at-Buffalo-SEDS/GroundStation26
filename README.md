# Ground Station 2026

## Dependencies

- Rust: install from https://rustup.rs/
- `dioxus-cli`: install with `cargo install dioxus-cli`

The frontend uses Dioxus. No separate WASM toolchain workflow is needed beyond the Rust targets used by `build.py`.

## Configuration

- Set the device name in `.cargo/config.toml`.
- Backend runtime data lives under `backend/data/`.
- Loadcell calibration files live under `backend/calibration/`.
- Radio link selection lives in `backend/comms/coms.json` by default and can be overridden with `GS_RADIO_LINK_CONFIG`.
- Link interfaces can be configured as serial/UART, SPI, or CAN. The Linux backend supports all three; this covers
  Ubuntu and Raspberry Pi OS.
- Use `python3 backend/tools/radio_link_config_gui.py` to detect serial, SPI, and CAN candidates, assign the AV bay and
  fill box links, and save the JSON config.
- If no display is available, the same script falls back to a terminal UI automatically. You can also force modes with
  `--gui`, `--tui`, or `--cli`.

## Build

Build the default web frontend plus backend:

```bash
python3 build.py
```

Common local build modes:

```bash
python3 build.py testing
python3 build.py hitl-mode
python3 build.py backend_only
python3 build.py frontend_web
python3 build.py debug
```

Scoped build entry points:

```bash
python3 frontend/build.py frontend_web
python3 frontend/build.py macos
python3 backend/build.py
python3 backend/build.py testing
```

Platform-specific frontend bundles:

```bash
python3 build.py ios
python3 build.py ios_sim
python3 build.py macos
python3 build.py windows
python3 build.py android
python3 build.py linux
```

Docker images:

```bash
python3 build.py docker
python3 build.py docker pi_build
python3 build.py docker testing
```

Build output notes:

- Web builds write to `frontend/dist/public`.
- Native frontend bundles write to `frontend/dist/...`.
- Web and native builds no longer delete each other's output directories.
- `build.py` is now the compatibility wrapper; use `frontend/build.py` and `backend/build.py` when you only need one
  side.

## Documentation

- Documentation index: `docs/README.md`
- Frontend/backend API contract: `docs/frontend/api.md`
- Frontend architecture: `docs/frontend/architecture.md`
- Backend architecture: `docs/backend/architecture.md`
- Shared contracts: `docs/shared/contracts.md`
- System overview: `docs/system/overview.md`

## Run

Build the frontend, then run the backend:

```bash
python3 run_groundstation.py
```

Enable simulator/testing mode:

```bash
python3 run_groundstation.py --testing
```

Enable HITL mode:

```bash
python3 run_groundstation.py --hitl-mode
```

Legacy positional forms still work:

```bash
python3 run_groundstation.py testing
python3 run_groundstation.py hitl-mode
```

Mode notes:

- `testing` enables the flight simulator and uses `backend/calibration/loadcell_calibration_testing.json`.
- `hitl-mode` is for hardware-in-the-loop testing. It uses the HITL layout, ignores the key interlock, starts in
  `Startup`, and does not run the normal fill sequence state machine.

## Frontend / Backend Notes

- The frontend is served by the backend from `frontend/dist/public`.
- For geolocation to work correctly in browsers, the backend should be behind HTTPS.
- `docker-compose.yml` is set up for local TLS testing with a self-signed certificate.

## Map Data

Download map data with:

```bash
python3 download_map.py
```

This uses the `map_downloader/` crate and writes map data into `data/`.
