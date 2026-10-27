#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
sshd_bin="${1:-/usr/sbin/sshd}"
export SYNCUI_SMOKE_ROOT
SYNCUI_SMOKE_ROOT="$(mktemp -d /tmp/syncui-smoke-XXXXXX)"
export SYNCUI_SMOKE_PORT
SYNCUI_SMOKE_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
mkdir "$SYNCUI_SMOKE_ROOT/home"
ssh-keygen -q -t ed25519 -N fixture-passphrase -f "$SYNCUI_SMOKE_ROOT/encrypted_key"
ssh-keygen -q -t ed25519 -N '' -f "$SYNCUI_SMOKE_ROOT/host_key"
ssh-keygen -q -t ed25519 -N '' -f "$SYNCUI_SMOKE_ROOT/client_key"
cat "$SYNCUI_SMOKE_ROOT/encrypted_key.pub" >> "$SYNCUI_SMOKE_ROOT/client_key.pub"
cat > "$SYNCUI_SMOKE_ROOT/sshd.conf" <<CONFIG
ListenAddress 127.0.0.1
Port $SYNCUI_SMOKE_PORT
HostKey $SYNCUI_SMOKE_ROOT/host_key
AuthorizedKeysFile $SYNCUI_SMOKE_ROOT/client_key.pub
PidFile $SYNCUI_SMOKE_ROOT/sshd.pid
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
SetEnv HOME=$SYNCUI_SMOKE_ROOT/home
LogLevel VERBOSE
CONFIG
"$sshd_bin" -D -e -f "$SYNCUI_SMOKE_ROOT/sshd.conf" > "$SYNCUI_SMOKE_ROOT/sshd.log" 2>&1 &
sshd_pid=$!
password_pid=""
trap 'kill "$sshd_pid" ${password_pid:+"$password_pid"} 2>/dev/null || true' EXIT
if python3 -c 'import paramiko' 2>/dev/null; then
  python3 scripts/password-fixture.py > "$SYNCUI_SMOKE_ROOT/password-server.log" 2>&1 &
  password_pid=$!
  for attempt in {1..50}; do
    if [[ -f "$SYNCUI_SMOKE_ROOT/password-port" ]]; then break; fi
    sleep 0.1
  done
  export SYNCUI_SMOKE_PASSWORD_PORT="$(cat "$SYNCUI_SMOKE_ROOT/password-port")"
fi
printf 'Isolated SSH fixture: %s\n' "$SYNCUI_SMOKE_ROOT"
cargo run --offline --manifest-path src-tauri/Cargo.toml --example ssh_smoke --features integration-test
