"""Incompatible watcher fixture used by e2e_daemon.py."""

import fcntl
import hashlib
import json
import os
import socket
import sys
from pathlib import Path


def main():
    info = {
        "build_id": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "protocol_version": 999,
    }
    if sys.argv[1:] == ["--daemon-info"]:
        print(json.dumps(info))
        return
    if sys.argv[1:] != ["watch"]:
        # Run a real, older TUI while retaining this fixture's installation path.
        os.execv(os.environ["AGENT_MUX_CLIENT_BINARY"], [sys.argv[0]])

    state = Path(os.environ["AGENT_MUX_STATE_DIR"])
    with (state / "watch.lock").open("r+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        lock.seek(0)
        lock.truncate()
        lock.write(str(os.getpid()))
        lock.flush()
        unhealthy = state / "future-unhealthy"
        unhealthy.unlink(missing_ok=True)
        path = state / "daemon.sock"
        path.unlink(missing_ok=True)
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(path))
            listener.listen()
            while True:
                stream, _ = listener.accept()
                with stream:
                    request = json.loads(stream.makefile().readline())
                    if request["type"] == "Status":
                        response = dict(info, type="Status", healthy=not unhealthy.exists())
                    elif request["type"] == "Shutdown":
                        stream.sendall(b'{"type":"Stopping"}\n')
                        return
                    else:
                        (state / "unexpected-client-request").write_text(request["type"])
                        response = dict(type="Error", message="unsupported client protocol")
                    stream.sendall((json.dumps(response) + "\n").encode())


if __name__ == "__main__":
    main()
