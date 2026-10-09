#!/usr/bin/env bash
# Build distributable bundles (.deb / .AppImage on Linux).
set -e

# Same WebKit workarounds as run-dev.sh (harmless during headless build).
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export WEBKIT_DISABLE_COMPOSITING_MODE=1

# rustc/LLVM can overflow its default worker stack on this toolchain (SIGILL).
export RUST_MIN_STACK="${RUST_MIN_STACK:-16777216}"

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
build_marker="$(mktemp)"
trap 'rm -f "$build_marker"' EXIT
npm run tauri build

# The .deb must never be named "sync-ui": that is Ubuntu's SyncEvolution GUI,
# and unattended-upgrades would silently replace our app with it.
if command -v dpkg-deb >/dev/null 2>&1; then
  mapfile -t debs < <(find src-tauri/target/release/bundle/deb -maxdepth 1 -type f -name '*.deb' -newer "$build_marker")
  if (( ${#debs[@]} == 0 )); then
    echo "error: no freshly built .deb found." >&2
    exit 1
  fi
  for deb in "${debs[@]}"; do
    pkg="$(dpkg-deb -f "$deb" Package)"
    if [[ "$pkg" != "syncui" ]]; then
      echo "error: $deb declares Package: $pkg (expected syncui)." >&2
      exit 1
    fi
    echo "Verified $deb: Package: syncui"
  done
fi
