#!/usr/bin/env bash
# Build distributable bundles (.deb / .AppImage on Linux).
set -e

# Same WebKit workarounds as run-dev.sh (harmless during headless build).
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

npm run tauri build
