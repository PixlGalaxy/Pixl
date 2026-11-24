#!/bin/sh
# Runs nginx (panel + /backend/ proxy) and the Lynx bot in one container, under tini.
# SIGTERM stops the bot first (it saves sessions and leaves voice channels), then nginx.
# If either process dies, the container exits so Docker can restart it.
set -eu

# nginx proxies /backend/ to 127.0.0.1:4000, so the bot's HTTP server must listen there whatever
# the env file says (HTTP_HOST / HTTP_PORT only matter when running the binary without Docker).
export HTTP_HOST=127.0.0.1 HTTP_PORT=4000
# Same for the admin listener: nginx serves the admin UI on 8081 and proxies only its /api/ to
# 127.0.0.1:4001. It is never bound to another interface inside the image.
export ADMIN_HTTP_HOST=127.0.0.1 ADMIN_HTTP_PORT=4001
unset ADMIN_WEB_DIR

nginx -c /etc/nginx/nginx.conf -g 'daemon off;' &
NGINX_PID=$!

/app/lynx &
LYNX_PID=$!

stop() {
    kill -TERM "$LYNX_PID" 2>/dev/null || true
    wait "$LYNX_PID" 2>/dev/null || true
    kill -TERM "$NGINX_PID" 2>/dev/null || true
    wait "$NGINX_PID" 2>/dev/null || true
}

trap 'stop; exit 0' TERM INT

while kill -0 "$LYNX_PID" 2>/dev/null && kill -0 "$NGINX_PID" 2>/dev/null; do
    sleep 1
done

echo "[entrypoint] a process exited unexpectedly, stopping the container" >&2
stop
exit 1
