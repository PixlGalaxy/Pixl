#!/bin/sh
# Runs nginx (panel + /backend/ proxy) and the Lynx bot in one container, under tini.
# SIGTERM stops the bot first (it saves sessions and leaves voice channels), then nginx.
# If either process dies, the container exits so Docker can restart it.
set -eu

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
