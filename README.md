# Ground Station 2026

## Required Dependencies,

- Rust

get it from https://rustup.rs/

- wasm-pack

get it by running `cargo install wasm-pack`


## Setting up the DEVICE_IDENTIFIER
- The device name can be set in the `.cargo/config.toml` file in the root directory of the project.

## Usage
After building, the backend needs to be behind a reverse proxy with ssl enabled for geolocation to work properly.
If using docker compose,
the provided `docker-compose.yml` file already generates a self-signed certificate for testing purposes.

To download the map data run the provided python script:
```bash
python3 download_map.py
```


