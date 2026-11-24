# Base images are pinned by digest (Dependabot's docker updates bump tag and digest together);
# Deno and the baseline yt-dlp are pinned by version and SHA-256.

# --- Panel (React Activity) -------------------------------------------------
FROM node:24-bookworm-slim@sha256:190bcf5115694c9d3c4d654a1aa88177d77600a06d67972fd7bf4cf224f7692c AS web
WORKDIR /web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
COPY locales/ /locales/
RUN pnpm build

# --- Admin panel UI (admin/web, its own app) ----------------------------------
FROM node:24-bookworm-slim@sha256:190bcf5115694c9d3c4d654a1aa88177d77600a06d67972fd7bf4cf224f7692c AS admin-web
WORKDIR /admin-web
RUN corepack enable
COPY admin/web/package.json admin/web/pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile
COPY admin/web/ ./
RUN pnpm build

# --- Bot (Rust) -------------------------------------------------------------
FROM rust:1-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea AS bot
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
FROM debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini libopus0 curl unzip nginx-light \
 && rm -rf /var/lib/apt/lists/* \
 && rm -f /etc/nginx/sites-enabled/default
# Deno: JS runtime yt-dlp needs for YouTube challenges. To update: the new version and the SHA-256
# from that release's deno-x86_64-unknown-linux-gnu.zip.sha256sum.
ARG DENO_VERSION=v2.9.8
ARG DENO_SHA256=e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609
RUN curl -fsSL "https://github.com/denoland/deno/releases/download/${DENO_VERSION}/deno-x86_64-unknown-linux-gnu.zip" -o /tmp/deno.zip \
 && echo "${DENO_SHA256}  /tmp/deno.zip" | sha256sum -c - \
 && unzip /tmp/deno.zip -d /usr/local/bin && rm /tmp/deno.zip
# Baseline yt-dlp; the bot keeps an updated copy in $APPDATA_DIR/bin (each update is checked against
# the release's SHA2-256SUMS). To update: the new version and its yt-dlp_linux line in SHA2-256SUMS.
ARG YTDLP_VERSION=2026.11.23
ARG YTDLP_SHA256=249ddb058234276bc449b9cd9492aad9f133bda5923c61a9e39983ae114ef17d
RUN curl -fsSL "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_linux" -o /usr/local/bin/yt-dlp \
 && echo "${YTDLP_SHA256}  /usr/local/bin/yt-dlp" | sha256sum -c - \
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
COPY docker/nginx-headers.conf /etc/nginx/lynx-headers.conf
COPY docker/nginx-backend-headers.conf /etc/nginx/lynx-backend-headers.conf
COPY docker/nginx-admin-headers.conf /etc/nginx/lynx-admin-headers.conf
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
