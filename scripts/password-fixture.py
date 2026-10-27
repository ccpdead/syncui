"""Disposable loopback SSH password server; test dependency: paramiko.
No OS account/password changes. Commands are only accepted after fixture auth.
"""
import os, socket, subprocess, threading, time
from pathlib import Path
import paramiko
root = Path(os.environ["SYNCUI_SMOKE_ROOT"])
assert str(root).startswith("/tmp/syncui-smoke-")
key = paramiko.RSAKey.generate(2048)
class Server(paramiko.ServerInterface):
    def get_allowed_auths(self, username): return "password"
    def check_auth_password(self, username, password):
        return paramiko.AUTH_SUCCESSFUL if password == "fixture-passphrase" else paramiko.AUTH_FAILED
    def check_channel_request(self, kind, chanid):
        return paramiko.OPEN_SUCCEEDED if kind == "session" else paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED
    def check_channel_exec_request(self, channel, command):
        def execute():
            env = dict(os.environ, HOME=str(root / "password-home"))
            p = subprocess.run(["/bin/sh", "-c", command.decode()], env=env, capture_output=True)
            if p.stdout: channel.sendall(p.stdout)
            if p.stderr: channel.sendall_stderr(p.stderr)
            channel.send_exit_status(p.returncode)
            channel.close()
        threading.Thread(target=execute, daemon=True).start()
        return True
server = socket.socket(); server.bind(("127.0.0.1", 0)); server.listen()
(root / "password-port").write_text(str(server.getsockname()[1]))
(root / "password-home").mkdir()
def serve(sock):
    transport = paramiko.Transport(sock); transport.add_server_key(key)
    try:
        transport.start_server(server=Server())
        # Keep the transport/control master alive; callbacks handle session exec.
        channels = []
        while transport.is_active():
            channel = transport.accept(0.2)
            if channel is not None: channels.append(channel)
    except (EOFError, OSError): pass
    finally: transport.close()
while True:
    sock, _ = server.accept()
    threading.Thread(target=serve, args=(sock,), daemon=True).start()
