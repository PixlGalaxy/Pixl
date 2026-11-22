#!/usr/bin/env bash
# Run cargo inside the dev container. Usage: scripts/cargo.sh check | test | clippy ...
# LYNX_TARGET_VOLUME lets parallel worktrees keep separate build caches.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_VOLUME="${LYNX_TARGET_VOLUME:-lynx-target}"
docker image inspect lynx-dev >/dev/null 2>&1 || docker build -t lynx-dev -f "$ROOT/docker/dev.Dockerfile" "$ROOT/docker"
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "$ROOT:/app" \
  -v lynx-cargo-registry:/usr/local/cargo/registry \
  -v "$TARGET_VOLUME:/app/target" \
  -e CARGO_TERM_COLOR=never \
  -w /app lynx-dev cargo "$@"
