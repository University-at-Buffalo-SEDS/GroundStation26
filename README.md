# Ground Station 2026

## Required Dependencies,

- Rust

get it from https://rustup.rs/

- Dioxus (frontend framework; no separate install required)

- dioxus-cli

install with `cargo install dioxus-cli`


## Setting up the DEVICE_IDENTIFIER
- The device name can be set in the `.cargo/config.toml` file in the root directory of the project.

## Usage
The frontend is built with Dioxus (no direct wasm workflow required).
After building, the backend needs to be behind a reverse proxy with ssl enabled for geolocation to work properly.
If using docker compose,
the provided `docker-compose.yml` file already generates a self-signed certificate for testing purposes.

## Building
Use `build.py` to build the frontend and backend:
```bash
python3 build.py
```

Platform-specific frontend builds:
```bash
python3 build.py ios|macos|windows|android|linux
```

Docker images:
```bash
python3 build.py docker
python3 build.py docker pi_build
```

## Running
Build the frontend and run the backend:
```bash
python3 run_groundstation.py
```

Enable testing features:
```bash
python3 run_groundstation.py testing
```

To download the map data run the provided python script:
```bash
python3 download_map.py
```
This uses the `map_downloader/` crate to fetch map data into `data/` for the UI; rerun to refresh or replace the data.
