FROM registry.gitlab.rylanswebsite.com/rylan-meilutis/rust-docker-builder:latest AS builder
ARG PI_BUILD=""
ARG TESTING=""

LABEL authors="rylan"

WORKDIR /app

# Top-level workspace manifests
COPY Cargo.toml Cargo.lock ./

# Backend crate (no data/)
RUN mkdir -p backend/src
COPY backend/Cargo.toml backend/
COPY backend/src backend/src

# Map downloader crate
RUN mkdir -p map_downloader/src
COPY map_downloader/Cargo.toml map_downloader/
COPY map_downloader/src map_downloader/src

# Frontend
RUN mkdir -p frontend/dist
COPY frontend/src frontend/src
COPY frontend/build.rs frontend/
COPY frontend/Cargo.toml frontend/
COPY frontend/assets frontend/
COPY frontend/platform frontend/
COPY frontend/scripts frontend/
COPY frontend/static frontend/
COPY frontend/Dioxus.toml frontend/

# Shared
RUN mkdir -p shared/src
COPY shared/Cargo.toml shared/
COPY shared/src shared/src

COPY build.py ./

# Build args:
# - PI_BUILD="pi_build" -> ./build.py pi_build
# - TESTING="testing"  -> ./build.py testing
# - both set           -> ./build.py pi_build testing
RUN set -e; \
    args=""; \
    if [ -n "${PI_BUILD}" ] && [ "${PI_BUILD}" = "pi_build" ]; then \
        echo "PI_BUILD='${PI_BUILD}' → enabling pi_build"; \
        args="$args pi_build"; \
    else \
        echo "PI_BUILD not set to 'pi_build'"; \
    fi; \
    if [ -n "${TESTING}" ] && [ "${TESTING}" = "testing" ]; then \
        echo "TESTING='${TESTING}' → enabling testing"; \
        args="$args testing"; \
    else \
        echo "TESTING not set to 'testing'"; \
    fi; \
    if [ -n "$args" ]; then \
        echo "→ ./build.py$args"; \
        ./build.py $args; \
    else \
        echo "→ ./build.py"; \
        ./build.py; \
    fi

RUN cargo build --release -p map_downloader

COPY entrypoint.sh ./
RUN chmod +x entrypoint.sh

FROM debian:stable-slim

LABEL authors="rylan"

WORKDIR /app

COPY --from=builder /app/target/release/groundstation_backend /app/
COPY --from=builder /app/target/release/map_downloader /app/map_downloader/
COPY --from=builder /app/frontend/dist /app/frontend/dist/
COPY --from=builder /app/frontend/web /app/frontend/web
COPY --from=builder /app/entrypoint.sh /app/

EXPOSE 3000
ENTRYPOINT ["/app/entrypoint.sh"]
