# Toolchain image used to build and test Lynx locally (no Rust needed on the host).
FROM rust:1-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake pkg-config libopus-dev ca-certificates python3 \
 && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy rustfmt
WORKDIR /app
