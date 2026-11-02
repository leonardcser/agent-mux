"""Run after cargo build --release: python3 tests/e2e_daemon.py."""

import hashlib
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def wait_for(check, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.1)
    raise AssertionError(label)


def main():
    source = Path(__file__).resolve().parents[1] / "target/release/agent-mux"
    server = f"mux-lifecycle-{os.getpid()}"
    with tempfile.TemporaryDirectory(prefix="mux-lifecycle-") as directory:
        root = Path(directory)
        binary = root / "agent-mux"
        shutil.copy2(source, binary)
        probe_env = dict(os.environ)
        probe_env.pop("TMUX", None)
        info = json.loads(subprocess.check_output(
            [str(binary), "--daemon-info"], env=probe_env, text=True,
        ))
        assert info["build_id"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        state = root / "state"
        lock = state / "watch.lock"
        unrelated = None
        legacy = None

        def tmux(*args):
            return subprocess.check_output(
                ["tmux", "-f", "/dev/null", "-L", server, *args], text=True
            ).strip()

        def launch(executable=binary):
            return tmux(
                "new-window", "-d", "-P", "-F", "#{pane_id}",
                f"env AGENT_MUX_STATE_DIR={shlex.quote(str(state))} "
                f"XDG_CONFIG_HOME={shlex.quote(str(root / 'config'))} "
                f"PATH={shlex.quote(str(root / 'shadow') + ':' + str(root) + ':' + os.environ['PATH'])} "
                f"AGENT_MUX_CLIENT_BINARY={shlex.quote(str(binary))} "
                f"{shlex.quote(str(executable))}",
            )

        def pid():
            return int(lock.read_text())

        def request(kind):
            with socket.socket(socket.AF_UNIX) as stream:
                stream.settimeout(1)
                stream.connect(str(state / "daemon.sock"))
                stream.sendall((json.dumps({"type": kind}) + "\n").encode())
                return json.loads(stream.makefile().readline())

        provider = root / "smelt"
        subprocess.run(
            ["cc", "-x", "c", "-", "-o", str(provider)],
            input='#include <stdio.h>\n#include <unistd.h>\n'
                  'int main(int argc, char **argv) { if (argc > 1) { puts("[]"); return 0; } '
                  'puts("cached-preview"); fflush(stdout); sleep(300); return 0; }\n',
            text=True, check=True,
        )
        try:
            tmux("new-session", "-d", "-s", "test", "-x", "100", "-y", "30")
            cached = tmux("new-window", "-d", "-P", "-F", "#{pane_id}",
                          "-n", "cached", "-c", str(root), str(provider))
            wait_for(lambda: "cached-preview" in tmux("capture-pane", "-p", "-t", cached),
                     "provider fixture has visible output")
            if os.environ.get("AGENT_MUX_LEGACY_BINARY"):
                shutil.copy2(os.environ["AGENT_MUX_LEGACY_BINARY"], binary)
                env = dict(os.environ, AGENT_MUX_STATE_DIR=str(state),
                           TMUX=tmux("display-message", "-p", "#{socket_path},#{pid},0"))
                legacy = subprocess.Popen([str(binary), "watch"], env=env)
                old = wait_for(pid, "legacy watcher starts")
                replacement = root / "replacement"
                shutil.copy2(source, replacement)
                replacement.replace(binary)
                pane = launch()
                wait_for(lambda: pid() != old, "new TUI replaces legacy watcher")
                assert legacy.wait(timeout=10) == -signal.SIGTERM
                print("Legacy watcher: automatic migration succeeds", flush=True)
            else:
                pane = launch()
            first = wait_for(pid, "initial watcher starts")
            wait_for(lambda: (state / "snapshot.json").exists(), "first snapshot")
            tmux("send-keys", "-t", pane, "o")
            ui_state = state / "ui_state.json"
            wait_for(lambda: json.loads(ui_state.read_text()).get("sortMode") == "order",
                     "sort preference is saved")
            wait_for(lambda: "cached-preview" in tmux("capture-pane", "-p", "-t", pane),
                     "preview renders after startup key press")
            replacement = root / "replacement"
            shutil.copy2(source, replacement)
            # A distinct executable build, retaining the same package version.
            with replacement.open("ab") as file:
                file.write(b"lifecycle-upgrade-fixture")
            replacement.replace(binary)
            second = wait_for(lambda: pid() if pid() != first else None,
                              "open TUI must replace outdated watcher")
            expected = hashlib.sha256(binary.read_bytes()).hexdigest()
            wait_for(lambda: request("Status").get("build_id") == expected,
                     "watcher must run installed build")
            assert json.loads(ui_state.read_text())["sortMode"] == "order"
            print("Atomic upgrade: open TUI replaces watcher and preserves UI state", flush=True)

            panes = [launch() for _ in range(3)]
            time.sleep(5)
            assert pid() == second, "concurrent TUIs must not bounce watcher"
            print("Concurrent TUIs: watcher remains stable", flush=True)

            os.kill(second, signal.SIGKILL)
            third = wait_for(lambda: pid() if pid() != second else None,
                             "open TUI must recover crashed watcher")
            wait_for(lambda: request("Status").get("healthy"), "recovered watcher healthy")
            assert third != second
            print("Crash recovery: watcher restarts automatically", flush=True)

            os.kill(third, signal.SIGSTOP)
            fourth = wait_for(lambda: pid() if pid() != third else None,
                              "open TUI must recover unresponsive watcher", timeout=25)
            wait_for(lambda: request("Status").get("healthy"), "replacement healthy")
            assert fourth != third
            tmux("send-keys", "-t", pane, "?")
            wait_for(lambda: "Keybindings" in tmux("capture-pane", "-p", "-t", pane),
                     "help appears")
            assert "reload watch" not in tmux("capture-pane", "-p", "-t", pane)
            tmux("send-keys", "-t", pane, "?", "R")
            time.sleep(1)
            assert pid() == fourth, "R must not restart watcher"
            for other in panes:
                tmux("kill-pane", "-t", other)
            print("Unresponsive watcher: bounded recovery succeeds", flush=True)

            def screen_contains(text):
                screen = tmux("capture-pane", "-p", "-t", pane)
                sidebar = "".join(line.split("│")[0].rstrip() for line in screen.splitlines())
                return text.replace(" ", "") in sidebar.replace(" ", "")

            missing = root / "missing"
            binary.rename(missing)
            wait_for(lambda: screen_contains("open installed executable"),
                     "missing binary error is visible")
            assert pid() == fourth, "missing update must not stop healthy watcher"
            assert screen_contains("cached"), "recovery error must not hide cached session"
            screen = tmux("capture-pane", "-p", "-t", pane)
            assert "cached-preview" in screen, (screen, tmux("capture-pane", "-p", "-t", cached))
            missing.rename(binary)
            wait_for(lambda: not screen_contains("Error:"), "restored binary clears error")
            print("Missing binary: healthy watcher stays running", flush=True)

            replacement.write_text("invalid executable\n")
            replacement.chmod(0o755)
            replacement.replace(binary)
            wait_for(lambda: screen_contains("validate installed executable"),
                     "invalid replacement error is visible")
            assert pid() == fourth, "invalid update must preserve healthy watcher"
            assert request("Status")["healthy"], "previous watcher stays usable"
            assert screen_contains("cached"), "invalid update must not hide session list"

            replacement.write_text("#!/bin/sh\nexec sleep 30\n")
            replacement.chmod(0o755)
            replacement.replace(binary)
            wait_for(lambda: screen_contains("validation timed out"),
                     "hung replacement validation is bounded")
            assert pid() == fourth, "hung update must preserve healthy watcher"
            assert request("Status")["healthy"]
            replacement.write_text("#!/bin/sh\nsleep 5 &\nprintf '{}\\n'\n")
            replacement.chmod(0o755)
            replacement.replace(binary)
            wait_for(lambda: screen_contains("read replacement validation output"),
                     "inherited validation stdout cannot block recovery")
            assert pid() == fourth, "blocked validation output must preserve healthy watcher"
            assert request("Status")["healthy"]
            print("Preflight: broken binaries, hangs, and inherited stdout stay bounded", flush=True)
            shutil.copy2(source, replacement)
            replacement.replace(binary)
            fifth = wait_for(lambda: pid() if pid() != fourth else None,
                             "valid reinstall recovers failed startup")
            wait_for(lambda: request("Status").get("healthy"), "reinstalled watcher healthy")
            print("Failed update: error is visible, reinstall recovers", flush=True)

            request("Shutdown")
            fixture = (
                "import fcntl, os, sys, time; "
                "f=open(sys.argv[1], 'r+'); fcntl.flock(f, fcntl.LOCK_EX); "
                "f.seek(0); f.truncate(); f.write(str(os.getpid())); f.flush(); "
                "time.sleep(60)"
            )
            unrelated = subprocess.Popen([sys.executable, "-c", fixture, str(lock)])
            wait_for(lambda: pid() == unrelated.pid, "unrelated process holds stale lock")
            wait_for(lambda: screen_contains("refusing to stop unverified"),
                     "unsafe PID is rejected")
            assert unrelated.poll() is None, "unrelated process must not be signalled"
            unrelated.terminate()
            unrelated.wait(timeout=10)
            sixth = wait_for(lambda: pid() if pid() not in (fifth, unrelated.pid) else None,
                             "watcher recovers after lock is released")
            wait_for(lambda: request("Status").get("healthy"), "watcher healthy after stale PID")
            print("Stale PID: unrelated process is not stopped", flush=True)

            tmux("kill-pane", "-t", pane)
            shutil.copy2(source, replacement)
            with replacement.open("ab") as file:
                file.write(b"next-launch-fixture")
            replacement.replace(binary)
            time.sleep(4)
            assert pid() == sixth, "no background upgrade is needed with no TUI"
            pane = launch()
            wait_for(lambda: pid() != sixth, "next launch applies pending upgrade")
            print("Closed TUIs: next launch applies upgrade", flush=True)

            alternate = root / "alternate-mux"
            shutil.copy2(source, alternate)
            launch(alternate)
            alternate_hash = hashlib.sha256(alternate.read_bytes()).hexdigest()
            wait_for(lambda: request("Status").get("build_id") == alternate_hash,
                     "new launch selects alternate installation")
            selected = pid()
            time.sleep(7)
            assert pid() == selected, "older TUIs must follow selected installation"
            print("Different installations: older TUIs do not downgrade watcher", flush=True)

            link = root / "stable-mux"
            link.symlink_to(alternate)
            shadow = root / "shadow"
            shadow.mkdir()
            (shadow / link.name).write_text("not executable\n")
            launch(link.name)
            wait_for(lambda: json.loads((state / "daemon-executable.json").read_text()) == str(link),
                     "stable launch path is selected")
            for version in (1, 2):
                previous = pid()
                image = root / f"version-{version}"
                shutil.copy2(source, image)
                with image.open("ab") as file:
                    file.write(f"symlink-version-{version}".encode())
                expected = hashlib.sha256(image.read_bytes()).hexdigest()
                if version == 2:
                    os.kill(previous, signal.SIGSTOP)
                temporary = root / "new-link"
                temporary.symlink_to(image)
                temporary.replace(link)
                wait_for(lambda: request("Status").get("build_id") == expected,
                         "symlink upgrade must follow stable launch path", timeout=25)
                assert pid() != previous
            print("Symlink upgrades: responsive and stopped watchers recover", flush=True)

            future = root / "future-mux"
            fixture = Path(__file__).with_name("future_daemon.py").read_text()
            future.write_text(f"#!{sys.executable}\n" + fixture)
            future.chmod(0o755)
            future_pane = launch(future)
            future_hash = hashlib.sha256(future.read_bytes()).hexdigest()
            wait_for(lambda: request("Status").get("build_id") == future_hash,
                     "newer-protocol watcher starts")
            wait_for(lambda: "reopen" in tmux("capture-pane", "-p", "-t", future_pane),
                     "incompatible TUI requests reopen")
            incompatible = pid()
            (state / "future-unhealthy").touch()
            wait_for(lambda: pid() != incompatible,
                     "older TUI must recover unhealthy newer-protocol watcher")
            wait_for(lambda: request("Status").get("healthy"), "newer watcher recovers")
            assert request("Status")["build_id"] == future_hash
            assert not (state / "unexpected-client-request").exists()
            print("Protocol mismatch: watcher recovery stays active without client requests", flush=True)
        finally:
            if unrelated and unrelated.poll() is None:
                unrelated.terminate()
                unrelated.wait(timeout=10)
            if legacy and legacy.poll() is None:
                legacy.terminate()
                legacy.wait(timeout=10)
            # Only stop the watcher in this test's private state directory.
            if lock.exists():
                try:
                    os.kill(pid(), signal.SIGKILL)
                except (OSError, ValueError):
                    pass
            subprocess.run(["tmux", "-L", server, "kill-server"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
