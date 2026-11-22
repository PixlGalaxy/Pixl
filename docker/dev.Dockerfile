# Toolchain image used to build and test Lynx locally (no Rust needed on the host).
FROM rust:1-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake pkg-config libopus-dev ca-certificates python3 curl unzip \
 && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy rustfmt
# yt-dlp + Deno, for the ignored network tests of the sources module.
RUN curl -fsSL https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip -o /tmp/deno.zip \
 && unzip /tmp/deno.zip -d /usr/local/bin && rm /tmp/deno.zip
RUN curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -o /usr/local/bin/yt-dlp \
 && chmod +x /usr/local/bin/yt-dlp
WORKDIR /app
