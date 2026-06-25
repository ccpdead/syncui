#!/usr/bin/env bash
# Launch SyncUI in development mode (hot-reload frontend + Tauri window).
# Uses the isolated conda env so the host system stays clean.
set -e

# Activate the conda environment that holds rust/node/webkit deps.
source "$(conda info --base)/etc/profile.d/conda.sh"
conda activate sync_ui

# The conda-forge LLVM segfaults under parallel codegen on some machines;
# a larger stack avoids it. (jobs=1 is pinned in .cargo/config.toml.)
export RUST_MIN_STACK=16777216

# WebKitGTK on Linux often renders a blank window due to its DMABUF/GPU
# compositing path failing with certain drivers. Disabling them forces a
# stable software/normal path and fixes the blank-screen issue.
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export WEBKIT_DISABLE_COMPOSITING_MODE=1

exec npm run tauri dev
