# --- Panel (React Activity) -------------------------------------------------
FROM node:24-bookworm-slim AS web
WORKDIR /web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
COPY locales/ /locales/
RUN pnpm build

# --- Bot (Rust) -------------------------------------------------------------
FROM rust:1-bookworm AS bot
RUN apt-get update && apt-get install -y --no-install-recommends cmake pkg-config libopus-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY migrations/ migrations/
COPY locales/ locales/
RUN cargo build --release --locked

# --- Runtime ----------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini libopus0 curl unzip \
 && rm -rf /var/lib/apt/lists/*
# Deno: JS runtime yt-dlp needs for YouTube challenges.
RUN curl -fsSL https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip -o /tmp/deno.zip \
 && unzip /tmp/deno.zip -d /usr/local/bin && rm /tmp/deno.zip
# Baseline yt-dlp; the bot keeps an updated copy in $DATA_DIR/bin.
RUN curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -o /usr/local/bin/yt-dlp \
 && chmod +x /usr/local/bin/yt-dlp
RUN useradd --system --uid 10001 --home /data lynx && mkdir -p /data && chown lynx /data
WORKDIR /app
COPY --from=bot /app/target/release/lynx /app/lynx
COPY --from=web /web/dist /app/web
ENV DATA_DIR=/data WEB_DIR=/app/web HTTP_PORT=8080
USER lynx
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/app/lynx"]
