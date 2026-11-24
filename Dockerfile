# --- Panel (React Activity) -------------------------------------------------
FROM node:24-bookworm-slim AS web
WORKDIR /web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
COPY locales/ /locales/
RUN pnpm build

# --- Admin panel UI (admin/web, its own app) ----------------------------------
FROM node:24-bookworm-slim AS admin-web
WORKDIR /admin-web
RUN corepack enable
COPY admin/web/package.json admin/web/pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile
COPY admin/web/ ./
RUN pnpm build

# --- Bot (Rust) -------------------------------------------------------------
FROM rust:1-bookworm AS bot
RUN apt-get update && apt-get install -y --no-install-recommends cmake pkg-config libopus-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
# Admin backend sources (compiled into the bot: src/main.rs includes admin/server/mod.rs).
COPY admin/server/ admin/server/
COPY migrations/ migrations/
COPY assets/ assets/
COPY locales/ locales/
# Commit shown by /about (the build context has no .git). Empty: no commit is shown.
ARG LYNX_COMMIT=
RUN cargo build --release --locked

# --- Runtime ----------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini libopus0 curl unzip nginx-light \
 && rm -rf /var/lib/apt/lists/* \
 && rm -f /etc/nginx/sites-enabled/default
# Deno: JS runtime yt-dlp needs for YouTube challenges.
RUN curl -fsSL https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip -o /tmp/deno.zip \
 && unzip /tmp/deno.zip -d /usr/local/bin && rm /tmp/deno.zip
# Baseline yt-dlp; the bot keeps an updated copy in $APPDATA_DIR/bin.
RUN curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -o /usr/local/bin/yt-dlp \
 && chmod +x /usr/local/bin/yt-dlp
# /appdata: persistent app data (SQLite, yt-dlp updates). /data: heavy data (stream fragments, caches).
RUN useradd --system --uid 10001 --home /appdata lynx \
 && mkdir -p /appdata /data \
 && chown lynx /appdata /data
WORKDIR /app
COPY --from=bot /app/target/release/lynx /app/lynx
COPY --from=web /web/dist /app/web
# Admin UI (served by nginx on 8081 only; see nginx.conf).
COPY --from=admin-web /admin-web/dist /app/admin
COPY nginx.conf /etc/nginx/nginx.conf
COPY docker/entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh
# nginx listens on 8080 (panel at /, backend at /backend/); the bot's HTTP server stays internal.
ENV APPDATA_DIR=/appdata DATA_DIR=/data HTTP_HOST=127.0.0.1 HTTP_PORT=4000
USER lynx
VOLUME ["/appdata", "/data"]
EXPOSE 8080
# Admin panel (status, activity, logs, notices, tweaks). Off unless ADMIN_USERNAME and a password are set; keep it LAN-only.
EXPOSE 8081
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s CMD curl -fsS http://127.0.0.1:8080/backend/healthz || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/app/entrypoint.sh"]
