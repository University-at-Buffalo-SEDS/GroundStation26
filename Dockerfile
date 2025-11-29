FROM registry.gitlab.rylanswebsite.com/rylan-meilutis/rust-docker-builder:latest AS builder

LABEL authors="rylan"

WORKDIR /app

COPY . .

RUN ./download_map.py

RUN ./build.py



FROM debian:stable-slim

LABEL authors="rylan"

WORKDIR /app

COPY --from=builder /app/target/release/groundstation_backend /app

COPY --from=builder /app/frontend/dist /app/frontend/dist

COPY --from=builder /app/backend/data /app/backend/data

EXPOSE 3000

ENTRYPOINT ["/app/groundstation_backend"]