#!/usr/bin/env bash
# Launch SyncUI in development mode (hot-reload frontend + Tauri window).
set -e

# WebKitGTK on Linux often renders a blank window due to its DMABUF/GPU
# compositing path failing with certain drivers. Disabling them forces a
# stable software/normal path and fixes the blank-screen issue.
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export WEBKIT_DISABLE_COMPOSITING_MODE=1

if ! command -v npm >/dev/null 2>&1; then
  echo "error: npm not found. Install Node.js >= 20 first (see README)." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust via rustup first (see README)." >&2
  exit 1
fi

if [[ ! -d node_modules ]]; then
  echo "node_modules missing — running npm install..."
  npm install
fi

exec npm run tauri dev
