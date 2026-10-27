#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Self-contained musl targets require no remote runtime or C cross compiler.
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  arch="${target%%-*}"
  RUSTFLAGS='-C linker=rust-lld' cargo build --locked --release --manifest-path crates/sync-tools/Cargo.toml --target "$target"
  mkdir -p "src-tauri/resources/agents/$arch"
  install -m 755 "crates/sync-tools/target/$target/release/syncui-agent" "src-tauri/resources/agents/$arch/syncui-agent"
  (cd "src-tauri/resources/agents/$arch" && sha256sum syncui-agent > SHA256SUMS)
  mkdir -p src-tauri/target/release/bundle/agents
  tar -czf "src-tauri/target/release/bundle/agents/syncui-agent-0.2.0-$arch.tar.gz" -C "src-tauri/resources/agents/$arch" syncui-agent SHA256SUMS
done
host_arch="$(uname -m)"
install -m 755 "crates/sync-tools/target/$host_arch-unknown-linux-musl/release/syncui-bridge" src-tauri/resources/syncui-bridge
