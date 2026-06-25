#!/usr/bin/env bash
# Build distributable bundles (.deb / .AppImage on Linux).
set -e

source "$(conda info --base)/etc/profile.d/conda.sh"
conda activate sync_ui

export RUST_MIN_STACK=16777216

npm run tauri build
