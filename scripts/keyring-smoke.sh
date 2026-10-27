#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --offline --manifest-path src-tauri/Cargo.toml --example ssh_smoke --features integration-test
fixture="$(mktemp -d /tmp/syncui-keyring-XXXXXX)"
mkdir -p "$fixture/data" "$fixture/runtime"
chmod 700 "$fixture/runtime"
export SYNCUI_ISOLATED_KEYRING=1 XDG_DATA_HOME="$fixture/data" XDG_RUNTIME_DIR="$fixture/runtime"
dbus-run-session -- sh -c 'printf "%s" fixture-keyring-password | gnome-keyring-daemon --unlock --components=secrets >/dev/null; exec src-tauri/target/debug/examples/ssh_smoke --keyring'

SYNCUI_KEYRING_UNAVAILABLE=1 DBUS_SESSION_BUS_ADDRESS="unix:path=$fixture/nonexistent-bus" src-tauri/target/debug/examples/ssh_smoke --keyring
