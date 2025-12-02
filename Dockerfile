FROM registry.gitlab.rylanswebsite.com/rylan-meilutis/rust-docker-builder:latest AS builder

LABEL authors="rylan"

WORKDIR /app

COPY . .

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