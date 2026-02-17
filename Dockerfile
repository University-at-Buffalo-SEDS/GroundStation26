FROM registry.gitlab.rylanswebsite.com/rylan-meilutis/rust-docker-builder:latest AS builder
ARG PI_BUILD=""
ARG TESTING=""
ARG BINARYEN_VERSION=117

LABEL authors="rylan"

# Install dependencies for build script and map downloader
RUN set -e; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl xz-utils; \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/*

# Directory creation
WORKDIR /app
RUN mkdir -p backend/sr
RUN mkdir -p map_downloader/src
RUN mkdir -p frontend/dist
RUN mkdir -p shared/src

# Backend crate (no data/)
COPY backend/Cargo.toml backend/
COPY backend/src backend/src
COPY backend/layout backend/layout

# Map downloader crate
COPY map_downloader/Cargo.toml map_downloader/
COPY map_downloader/src map_downloader/src

# Frontend
COPY frontend/src frontend/src
COPY frontend/build.rs frontend/
COPY frontend/Cargo.toml frontend/
COPY frontend/assets frontend/assets
COPY frontend/platform frontend/platform
COPY frontend/scripts frontend/scripts
COPY frontend/static frontend/static
COPY frontend/Dioxus.toml frontend/

# Shared
COPY shared/Cargo.toml shared/
COPY shared/src shared/src

# Top-level workspace manifest and main build script
COPY Cargo.toml ./
COPY build.py ./
COPY entrypoint.sh ./

# Run all builds for the workspace
RUN chmod +x entrypoint.sh

RUN cargo update && cargo fetch

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


FROM debian:stable-slim

LABEL authors="rylan"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/*

WORKDIR /app

COPY --from=builder /app/backend/layout /app/backend/layout/
COPY --from=builder /app/target/release/groundstation_backend /app/
COPY --from=builder /app/target/release/map_downloader /app/map_downloader/
COPY --from=builder /app/frontend/dist /app/frontend/dist/
COPY --from=builder /app/frontend/static /app/frontend/static/
COPY --from=builder /app/frontend/assets /app/frontend/assets/
COPY --from=builder /app/entrypoint.sh /app/

EXPOSE 3000
ENTRYPOINT ["/app/entrypoint.sh"]
