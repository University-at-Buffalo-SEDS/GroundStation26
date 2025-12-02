FROM registry.gitlab.rylanswebsite.com/rylan-meilutis/rust-docker-builder:latest AS builder

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

# Frontend (adjust these to match your actual structure)
RUN mkdir -p frontend/dist
COPY frontend/src frontend/src
COPY frontend/build.rs frontend/
COPY frontend/Cargo.toml frontend/
COPY frontend/dist/favicon.png frontend/dist
COPY frontend/dist/index.html frontend/dist
COPY frontend/dist/LICENSE frontend/dist

# Other needed files
COPY entrypoint.sh build.py ./

RUN chmod +x entrypoint.sh

RUN ./build.py

RUN cargo build --release -p map_downloader


FROM debian:stable-slim

LABEL authors="rylan"

WORKDIR /app

COPY --from=builder /app/target/release/groundstation_backend /app

COPY --from=builder /app/target/release/map_downloader /app

COPY --from=builder /app/frontend/dist /app/frontend/dist

COPY --from=builder /app/entrypoint.sh /app


EXPOSE 3000

ENTRYPOINT ["/app/entrypoint.sh"]