import fcntl
import hashlib
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import time
from pathlib import Path


def run(*args, timeout=5, **kwargs):
    result = subprocess.run(
        args,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        **kwargs,
    )
    if result.returncode:
        raise AssertionError(
            f"{shlex.join(map(str, args))} exited {result.returncode}\n"
            f"{result.stdout}\n{result.stderr}"
        )
    return result.stdout.rstrip("\n")


def wait_for(check, label, timeout=15):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (FileNotFoundError, ConnectionError, socket.timeout) as error:
            last_error = error
        time.sleep(0.05)
    raise AssertionError(
        f"Timed out after {timeout}s: {label}; last error: {last_error}"
    )


def read_json(path):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return {}


def fingerprint(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def atomic_write(path, text):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(text)
    temporary.replace(path)


class Sandbox:
    def __init__(self, root, source, provider):
        self.root = root
        self.source = source
        self.binary = root / "agent-mux"
        shutil.copy2(source, self.binary)
        shutil.copy2(provider, root / "smelt")
        self.state = root / "state"
        self.state.mkdir()
        self.config = root / "config/agent-mux/config.toml"
        self.config.parent.mkdir(parents=True)
        self.statuses = root / "statuses.json"
        self.statuses.write_text("[]")
        self.socket = root / "tmux.sock"
        self.processes = []
        self.env = dict(
            os.environ,
            HOME=str(root),
            SHELL="/bin/sh",
            XDG_CONFIG_HOME=str(root / "config"),
            AGENT_MUX_STATE_DIR=str(self.state),
            AGENT_MUX_TEST_STATUSES=str(self.statuses),
            PATH=f"{root}:{os.environ['PATH']}",
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL="/dev/null",
        )
        self.env.pop("TMUX", None)

    def start(self):
        self.tmux(
            "new-session",
            "-d",
            "-s",
            "browser",
            "-x",
            "160",
            "-y",
            "45",
            "exec sleep 600",
        )
        self.tmux("set-option", "-g", "default-shell", "/bin/sh")
        self.env["TMUX"] = self.tmux("display-message", "-p", "#{socket_path},#{pid},0")

    def tmux(self, *args):
        return run(
            "tmux", "-f", "/dev/null", "-S", str(self.socket), *args, env=self.env
        )

    def spawn(self, *args):
        with (self.root / f"child-{len(self.processes)}.log").open("w") as log:
            child = subprocess.Popen(
                args,
                env=self.env,
                stdin=subprocess.PIPE,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        self.processes.append(child)
        return child

    def attach(self, session="browser"):
        child = self.spawn(
            "tmux", "-S", str(self.socket), "-C", "attach-session", "-t", session
        )
        wait_for(
            lambda: (
                str(child.pid)
                in self.tmux("list-clients", "-F", "#{client_pid}").splitlines()
            ),
            "attach interactive tmux client",
        )

    def launch(self, executable=None):
        assignments = [
            f"{key}={self.env[key]}"
            for key in (
                "HOME",
                "XDG_CONFIG_HOME",
                "AGENT_MUX_STATE_DIR",
                "AGENT_MUX_TEST_STATUSES",
                "PATH",
            )
        ]
        command = shlex.join(["env", *assignments, str(executable or self.binary)])
        return self.tmux(
            "new-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "browser:",
            command,
        )

    def add_agent(self, name="agent", cwd=None, session="browser", output=None):
        command = [str(self.root / "smelt"), name]
        if output is not None:
            command.append(str(output))
        pane, pid = self.tmux(
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id} #{pane_pid}",
            "-t",
            f"{session}:",
            "-n",
            name,
            "-c",
            str(cwd or self.root),
            "exec " + shlex.join(command),
        ).split()
        expected = output.read_text() if output is not None else f"preview-{name}"
        wait_for(lambda: expected in self.screen(pane), "agent fixture ready")
        return pane, int(pid)

    def write_statuses(self, values):
        atomic_write(self.statuses, json.dumps(values))

    def screen(self, pane):
        return self.tmux("capture-pane", "-p", "-t", pane)

    def wait_screen(self, pane, check, label):
        return wait_for(lambda: check(self.screen(pane)), label)

    def send(self, pane, *keys):
        self.tmux("send-keys", "-t", pane, *keys)

    def click(self, pane, label):
        row = next(
            i
            for i, line in enumerate(self.screen(pane).splitlines())
            if label in line.split("│", 1)[0]
        )
        self.send(pane, "-l", f"\x1b[<0;3;{row + 1}M\x1b[<0;3;{row + 1}m")

    def ui_state(self):
        return read_json(self.state / "ui_state.json")

    def snapshot(self):
        return read_json(self.state / "snapshot.json")

    def pid(self):
        value = (self.state / "watch.lock").read_text().strip()
        return int(value) if value else None

    def request(self, kind):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(2 if kind == "SubscribeFresh" else 1)
            stream.connect(str(self.state / "daemon.sock"))
            stream.sendall((json.dumps({"type": kind}) + "\n").encode())
            with stream.makefile() as response:
                line = response.readline()
                if not line:
                    raise ConnectionError("watcher disconnected before its response")
                return json.loads(line)

    def ready(self, previous=None, timeout=15):
        def check():
            if self.request("Status").get("healthy"):
                pid = self.pid()
                return pid if pid != previous else None
            return None

        return wait_for(check, "healthy watcher", timeout=timeout)

    def replace_binary(self, suffix=b""):
        replacement = self.root / "replacement"
        shutil.copy2(self.source, replacement)
        with replacement.open("ab") as file:
            file.write(suffix)
        replacement.replace(self.binary)
        return fingerprint(self.binary)

    def capture_diagnostics(self):
        (self.root / "processes.txt").write_text(
            "\n".join(
                f"child-{i}: pid={child.pid} exit={child.poll()}"
                for i, child in enumerate(self.processes)
            )
        )
        try:
            (self.root / "panes.txt").write_text(
                self.tmux(
                    "list-panes",
                    "-a",
                    "-F",
                    "#{pane_id} dead=#{pane_dead} exit=#{pane_dead_status}",
                )
            )
            panes = self.tmux("list-panes", "-a", "-F", "#{pane_id}").splitlines()
            for pane in panes:
                (self.root / f"pane-{pane.removeprefix('%')}.txt").write_text(
                    self.screen(pane)
                )
        except (AssertionError, subprocess.TimeoutExpired) as error:
            (self.root / "capture-error.txt").write_text(str(error))

    def owned_watcher(self):
        # Never signal a PID from a lock file without verifying its command.
        try:
            with (self.state / "watch.lock").open("r+") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    return None
                except BlockingIOError:
                    pid = int(lock.read())
            command = shlex.split(run("ps", "-p", str(pid), "-o", "args="))
            if Path(command[0]).parent == self.root and command[1:] == ["watch"]:
                return pid
        except (OSError, ValueError, AssertionError, IndexError):
            pass
        return None

    def close(self):
        # Stop clients before the watcher so recovery cannot race teardown.
        subprocess.run(
            ["tmux", "-S", str(self.socket), "kill-server"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=5,
        )
        for child in self.processes:
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=3)
            if child.stdin:
                child.stdin.close()
        pid = self.owned_watcher()
        if pid:
            try:
                os.kill(pid, signal.SIGCONT)
                self.request("Shutdown")
            except (OSError, ValueError):
                pass
            deadline = time.monotonic() + 3
            while self.owned_watcher() == pid and time.monotonic() < deadline:
                time.sleep(0.05)
            if self.owned_watcher() == pid:
                os.kill(pid, signal.SIGKILL)
