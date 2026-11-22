#!/usr/bin/env bash
# Format only the given files (rustfmt, edition 2021, repo rustfmt.toml). Usage: scripts/fmt-files.sh src/a.rs src/b.rs
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MSYS_NO_PATHCONV=1 docker run --rm -v "$ROOT:/app" -w /app lynx-dev rustfmt --edition 2021 "$@"
