# Ground Station 2026

Live multi-camera video, the Raspberry Pi sender daemon, and rocket stage model
storage are documented in [Video and stage models](docs/backend/video-and-models.md).
Grouped valve actions, nitrogen testing, pressure calibration and the fill-equipment
scene are documented in [GSE sequences](docs/backend/gse-sequences.md).
Stream-manager roles and delayed audience video are covered in [Broadcast studio](docs/backend/broadcast-studio.md);
named-part animation is covered in [Model animations](docs/backend/model-animations.md).

The default Dashboard is the single-stage model with cycling backend-defined data.
Settings → General → Viewing mode selects Ground Station view or delayed Streamer
mode. Deploy matching frontend/backend `dev` builds (`--frontend-dev`); see the
[contract examples](docs/frontend/examples/README.md) and
[backend presentation profile](docs/backend/presentation.example.json).

## Dependencies

- Rust: install from https://rustup.rs/
- `dioxus-cli`: install with `cargo install dioxus-cli`
- SEDSNet v4.0.27 from crates.io. The untracked workspace lockfile is local
  build state; releases are qualified against the current stable `4` series.

The frontend uses Dioxus. No separate WASM toolchain workflow is needed beyond the Rust targets used by `build.py`.

## Configuration

- Set the device name in `.cargo/config.toml`.
- Backend runtime data lives under `backend/data/`.
- Loadcell calibration files live under `backend/calibration/`.
- Radio link selection lives in `backend/comms/coms.json` by default and can be overridden with `GS_RADIO_LINK_CONFIG`.
- Link interfaces can be configured as serial/UART, SPI, or CAN. The Linux backend supports all three; this covers
  Ubuntu and Raspberry Pi OS.
- Set `GS_DEBUG_PRINTS=1` to enable backend debug/status prints that are muted by default.
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
python3 build.py --frontend-dev
```

`--frontend-dev` checks out the frontend's `dev` branch; the default frontend
build uses its stable release branch. The cached checkout is fetched and reset
to the selected remote branch, so an existing local branch is not assumed to
have valid tracking metadata.

Scoped build entry points:

```bash
python3 backend/build.py
python3 backend/build.py testing
```

Docker images:

```bash
python3 build.py docker
python3 build.py docker pi_build
python3 build.py docker testing
python3 build.py docker --frontend-dev
```

## Documentation

- Documentation index: `docs/README.md`
- Frontend/backend API contract: `docs/frontend/api.md`
- Frontend architecture: `docs/frontend/architecture.md`
- Backend architecture: `docs/backend/architecture.md`
- Backend I2C transport: `docs/backend/i2c.md`
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

The backend is a normal SEDSNet endpoint and discovers subscribers and routes by
schema data type. It does not broadcast every value to both physical links.
Managed underglow, startup-buzzer, flight-state, RF/FC telemetry-rate, and DAQ
calibration values are cached on disk;
their last authoritative values are restored when GroundStation restarts and
are resynchronized when boards join or reboot.

## Frontend / Backend Notes

- For geolocation to work correctly in browsers, the backend should be behind HTTPS.
- `docker-compose.yml` is set up for local TLS testing with a self-signed certificate.
- The three bundled layouts (`layout.json`, `layout_hitl.json`, and `layout_test_fire.json`) define UI-only data display filter defaults. The frontend presents these as groundstation defaults and lets operators override filter kinds and tuning values locally. Loadcell display data is time-averaged while GPS and valve-state telemetry stay raw so saved telemetry remains unmodified.

## Map Data

Download map data with:

```bash
python3 download_map.py
```

This uses the `map_downloader/` crate and writes map data into `data/`.
