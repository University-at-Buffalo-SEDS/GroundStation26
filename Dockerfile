FROM registry.gitlab.rylanswebsite.com/rylan-meilutis/rust-docker-builder:latest AS builder

LABEL authors="rylan"

WORKDIR /app

RUN curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh

COPY . .

RUN ./build.py

FROM debian:stable-slim

LABEL authors="rylan"

WORKDIR /app

COPY --from=builder /app/target/release/groundstation_backend /app

COPY --from=builder /app/frontend/ /app/frontend/

EXPOSE 3000

ENTRYPOINT ["/app/groundstation_backend"]