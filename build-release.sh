#!/usr/bin/env bash
# Build distributable bundles (.deb / .AppImage on Linux).
set -e

# Same WebKit workarounds as run-dev.sh (harmless during headless build).
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export WEBKIT_DISABLE_COMPOSITING_MODE=1

# Cap parallel rustc jobs. Release builds with LTO / codegen-units=1 are very
# memory-hungry; default (= nproc, often 32) can trigger "rustc-LLVM ERROR: out of memory".
if [[ -z "${CARGO_BUILD_JOBS:-}" ]]; then
  cpus="$(nproc 2>/dev/null || echo 4)"
  if (( cpus > 8 )); then
    export CARGO_BUILD_JOBS=8
  else
    export CARGO_BUILD_JOBS="$cpus"
  fi
fi

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

echo "Building with CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}..."
npm run tauri build
