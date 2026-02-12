FROM registry.gitlab.rylanswebsite.com/rylan-meilutis/rust-docker-builder:latest AS builder
ARG PI_BUILD=""
ARG TESTING=""
ARG BINARYEN_VERSION=117

LABEL authors="rylan"

WORKDIR /app

# wasm-opt for dx bundle --release (binaryen). Install from GitHub release.
RUN set -e; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl xz-utils; \
    arch="$(uname -m)"; \
    case "$arch" in \
        x86_64) bin_arch="x86_64" ;; \
        aarch64|arm64) bin_arch="aarch64" ;; \
        *) echo "Unsupported arch for wasm-opt: $arch" >&2; exit 1 ;; \
    esac; \
    url="https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/binaryen-version_${BINARYEN_VERSION}-${bin_arch}-linux.tar.gz"; \
    curl -fsSL "$url" -o /tmp/binaryen.tar.gz; \
    mkdir -p /opt/binaryen; \
    tar -xzf /tmp/binaryen.tar.gz -C /opt/binaryen --strip-components=1; \
    ln -sf /opt/binaryen/bin/wasm-opt /usr/local/bin/wasm-opt; \
    rm -rf /tmp/binaryen*; \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/*
ENV WASM_OPT=/usr/local/bin/wasm-opt

# Top-level workspace manifests
COPY Cargo.toml Cargo.lock ./

# Backend crate (no data/)
RUN mkdir -p backend/src
COPY backend/Cargo.toml backend/
COPY backend/src backend/src
COPY backend/layout backend/layout

# Map downloader crate
RUN mkdir -p map_downloader/src
COPY map_downloader/Cargo.toml map_downloader/
COPY map_downloader/src map_downloader/src

# Frontend
RUN mkdir -p frontend/dist
COPY frontend/src frontend/src
COPY frontend/build.rs frontend/
COPY frontend/Cargo.toml frontend/
COPY frontend/assets frontend/assets
COPY frontend/platform frontend/platform
COPY frontend/scripts frontend/scripts
COPY frontend/static frontend/static
COPY frontend/Dioxus.toml frontend/

# Shared
RUN mkdir -p shared/src
COPY shared/Cargo.toml shared/
COPY shared/src shared/src
RUN cd frontend && cargo update && cargo fetch && cd ..

COPY build.py ./

# Build args:
# - PI_BUILD="pi_build" -> ./build.py pi_build
# - TESTING="testing"  -> ./build.py testing
# - both set           -> ./build.py pi_build testing
RUN set -e; \
    args=""; \
    if [ -n "${PI_BUILD}" ] && [ "${PI_BUILD}" = "TRUE" ]; then \
        echo "PI_BUILD='${PI_BUILD}' → enabling pi_build"; \
        args="$args pi_build"; \
    else \
        echo "PI_BUILD not set to 'TRUE'"; \
    fi; \
    if [ -n "${TESTING}" ] && [ "${TESTING}" = "TRUE" ]; then \
        echo "TESTING='${TESTING}' → enabling testing"; \
        args="$args testing"; \
    else \
        echo "TESTING not set to 'TRUE'"; \
    fi; \
    if [ -n "$args" ]; then \
        echo "→ ./build.py$args"; \
        GROUNDSTATION_NO_PARALLEL=1 ./build.py $args; \
    else \
        echo "→ ./build.py"; \
        GROUNDSTATION_NO_PARALLEL=1 ./build.py; \
    fi

RUN cargo build --release -p map_downloader

COPY entrypoint.sh ./
RUN chmod +x entrypoint.sh

FROM debian:stable-slim

LABEL authors="rylan"

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/*

COPY --from=builder /app/backend/layout /app/backend/layout/
COPY --from=builder /app/target/release/groundstation_backend /app/
COPY --from=builder /app/target/release/map_downloader /app/map_downloader/
COPY --from=builder /app/frontend/dist /app/frontend/dist/
COPY --from=builder /app/frontend/static /app/frontend/static/
COPY --from=builder /app/frontend/assets /app/frontend/assets/
COPY --from=builder /app/entrypoint.sh /app/

EXPOSE 3000
ENTRYPOINT ["/app/entrypoint.sh"]
