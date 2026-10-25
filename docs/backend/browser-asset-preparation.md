# Generated frontend assets in backend-only builds

The backend embeds the browser assets declared by the selected frontend's
`station_ui/manifest.json`. A clean checkout may not contain generated MapLibre
CSS/JS. The `gs-browser-assets` build script now runs that frontend's
`scripts/browser_assets.py` before discovering exports, so building only the
backend does not require building the UI first. GroundStation's source selection
also prepares dependencies before calculating its asset digest/receipt.

The frontend owns preparation, version pins and cache policy. Python 3.10+ is
required; Cargo tries `python3`, `python`, then the Windows `py -3` launcher.
`GS_PYTHON` selects a particular executable. The Docker builder explicitly
installs Python. This is a build-time requirement, not a runtime dependency of
the GroundStation/RRLS binary.

Prepare dependencies directly from the selected frontend with:

```sh
python3 build.py --prepare-browser-assets
```

Use `GS_BROWSER_ASSETS_CACHE` for an offline or container cache. MapLibre 4.7.1
CSS and JS are pinned to hashes verified against the official npm package and
its published SHA512 integrity. Explicit `MAPLIBRE_GL_VERSION` overrides retain
hash receipts for cache validation. Invalid/missing assets cause an actionable
build failure; no placeholder is compiled into the backend. All generated files
stay in the frontend checkout, which remains the asset authority.

Qualification includes synthetic HTTP cold/placeholder/corrupt-cache/failure
cases, parallel and interrupted processes, the lightweight frontend flag with no
Rust tools on PATH, selected-source preparation before digesting, and a backend
Cargo test against a clean selected frontend lacking generated assets and a
frontend Cargo manifest. That last test uses a verified cache with external
network access disabled; it must not invoke any UI compilation.
