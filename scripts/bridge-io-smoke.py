#!/usr/bin/env python3
"""Exercise the real bridge and rsync locally; replace only the SSH transport.

Run scripts/build-tools.sh first, then python3 scripts/bridge-io-smoke.py.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bridge", type=Path, default=Path(__file__).resolve().parents[1] / "src-tauri/resources/syncui-bridge")
    args = parser.parse_args()
    bridge = args.bridge.resolve()
    with tempfile.TemporaryDirectory(prefix="syncui-bridge-io-") as directory:
        root = Path(directory)
        (root / "bin").mkdir()
        (root / "source").mkdir()
        # PATH substitutes SSH with a local rsync server: no network connection is made.
        ssh = root / "bin/ssh"
        ssh.write_text("""#!/usr/bin/python3
import os, shlex, sys
args = shlex.split(sys.argv[-1])
assert args[0] == 'rsync'
os.execv('/usr/bin/rsync', args)
""")
        ssh.chmod(0o755)
        payload = os.urandom(16 * 1024 * 1024)
        (root / "source/payload.bin").write_bytes(payload)
        expected = hashlib.sha256(payload).digest()
        config = root / "bridge.json"
        config.write_text(json.dumps({"socket": str(root / "unused.socket"), "token": "local-test", "ssh_args": [], "host": "mock.invalid"}))
        env = {**os.environ, "PATH": f"{root / 'bin'}:/usr/bin:/bin", "SYNCUI_BRIDGE_CONFIG": str(config)}
        for direction in ["download", "upload"]:
            for blocking in [False, True]:
                destination = root / f"{direction}-{blocking}"
                destination.mkdir()
                command = ["/usr/bin/rsync", "--times", "--perms", "--relative", "--dirs", "--no-recursive",
                           "--omit-dir-times", "--no-implied-dirs", "--from0", "--files-from=-", "--ignore-times",
                           "--protect-args", "--bwlimit=4096", "-e", shlex.quote(str(bridge)) + " rsync-rsh"]
                if blocking:
                    command.append("--blocking-io")
                source = str(root / "source") + "/"
                target = str(destination) + "/"
                if direction == "download":
                    source = "syncui-peer:" + source
                else:
                    target = "syncui-peer:" + target
                result = subprocess.run(command + ["--", source, target], input=b"payload.bin\0", capture_output=True, env=env, timeout=30)
                assert result.returncode == 0, result.stderr.decode(errors="replace")
                assert hashlib.sha256((destination / "payload.bin").read_bytes()).digest() == expected
                print(f"PASS {direction} blocking={blocking}: 16 MiB, exit=0, SHA256 matches", flush=True)


if __name__ == "__main__":
    main()
