"""Run after cargo build --release: python3 tests/e2e_live_preview.py."""

import json
import os
import shlex
import shutil
import signal
import socket
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def wait_for(check, label, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.005)
    raise AssertionError(f"Timed out: {label}")


def main():
    source = Path(
        os.environ.get(
            "AGENT_MUX_TEST_BINARY",
            Path(__file__).resolve().parents[1] / "target/release/agent-mux",
        )
    )
    server = f"mux-live-{os.getpid()}"
    watcher = None
    with tempfile.TemporaryDirectory(prefix="mux-live-") as directory:
        root = Path(directory)
        binary = root / "agent-mux"
        shutil.copy2(source, binary)
        provider = root / "smelt"
        statuses = root / "statuses.json"
        output = root / "output"
        statuses.write_text("[]")
        output.write_text("initial-preview")
        run(
            "cc",
            "-x",
            "c",
            "-",
            "-o",
            str(provider),
            input=r"""
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "status") == 0) {
        FILE *f = fopen(getenv("LIVE_STATUSES"), "r");
        if (!f) return 1;
        int c;
        while ((c = fgetc(f)) != EOF) putchar(c);
        fclose(f);
        return 0;
    }
    char previous[4096] = "", text[4096];
    for (;;) {
        FILE *f = fopen(getenv("LIVE_OUTPUT"), "r");
        if (f) {
            size_t n = fread(text, 1, sizeof(text) - 1, f);
            text[n] = 0;
            fclose(f);
            if (strcmp(text, previous) != 0) {
                printf("\033[2J\033[H%s", text);
                fflush(stdout);
                strcpy(previous, text);
            }
        }
        usleep(1000);
    }
}
""",
        )
        env = dict(
            os.environ,
            AGENT_MUX_STATE_DIR=str(root / "state"),
            XDG_CONFIG_HOME=str(root / "config"),
            LIVE_STATUSES=str(statuses),
            LIVE_OUTPUT=str(output),
            PATH=f"{root}:{os.environ['PATH']}",
        )

        def tmux(*args):
            return run("tmux", "-f", "/dev/null", "-L", server, *args)

        def write_status(state, pid):
            temp = statuses.with_suffix(".tmp")
            temp.write_text(json.dumps([{"pid": pid, "state": state}]))
            temp.replace(statuses)

        def fresh_state():
            with socket.socket(socket.AF_UNIX) as stream:
                stream.settimeout(2)
                stream.connect(str(root / "state/daemon.sock"))
                stream.sendall(b'{"type":"SubscribeFresh"}\n')
                return json.loads(stream.makefile().readline())

        def snapshot():
            try:
                return json.loads((root / "state/snapshot.json").read_text())
            except FileNotFoundError:
                return {}

        try:
            tmux(
                "new-session", "-d", "-s", "test", "-x", "140", "-y", "40", "sleep 120"
            )
            env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
            # A separate session catches accidental window selection and size changes.
            pane, pid = tmux(
                "new-session",
                "-d",
                "-s",
                "agent",
                "-n",
                "source-fixture",
                "-x",
                "100",
                "-y",
                "30",
                "-P",
                "-F",
                "#{pane_id} #{pane_pid}",
                "env "
                + " ".join(
                    f"{k}={shlex.quote(env[k])}"
                    for k in ["LIVE_OUTPUT", "LIVE_STATUSES"]
                )
                + " "
                + shlex.quote(str(provider)),
            ).split()
            pid = int(pid)
            write_status("idle", pid)
            tmux("new-window", "-d", "-t", "agent:", "-n", "source-other", "sleep 120")
            tmux("select-window", "-t", "agent:source-other")
            watcher = subprocess.Popen(
                [str(binary), "watch"],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            wait_for(
                lambda: len(snapshot().get("panes", [])) == 1, "initial idle snapshot"
            )
            watcher.send_signal(signal.SIGSTOP)
            write_status("busy", pid)
            launch = (
                "env "
                + " ".join(
                    f"{k}={shlex.quote(env[k])}"
                    for k in [
                        "AGENT_MUX_STATE_DIR",
                        "XDG_CONFIG_HOME",
                        "LIVE_OUTPUT",
                        "LIVE_STATUSES",
                        "PATH",
                    ]
                )
                + " "
                + shlex.quote(str(binary))
            )
            sidebar = tmux(
                "new-window", "-t", "test:", "-P", "-F", "#{pane_id}", launch
            )

            def screen():
                return tmux("capture-pane", "-p", "-t", sidebar)

            # Keep the watcher unavailable briefly. A fresh startup must not expose the idle list.
            frames = []
            end = time.monotonic() + 0.3
            while time.monotonic() < end:
                frames.append(screen())
                time.sleep(0.01)
            watcher.send_signal(signal.SIGCONT)
            stale = [
                frame
                for frame in frames
                if "source-fixture" in frame and "working (1)" not in frame
            ]
            assert not stale, (
                "startup displayed stale idle list before refreshing status"
            )
            wait_for(lambda: "working (1)" in screen(), "fresh busy layout")
            wait_for(lambda: "initial-preview" in screen(), "initial preview")
            assert (
                tmux(
                    "display-message", "-p", "-t", pane, "#{pane_width}x#{pane_height}"
                )
                == "100x30"
            )
            assert (
                tmux("display-message", "-p", "-t", "agent:", "#{window_name}")
                == "source-other"
            )

            # A fresh subscription must run a scan, even after an unchanged scan.
            unchanged = fresh_state()
            again = fresh_state()
            assert (
                unchanged["snapshot"]["generation"] == again["snapshot"]["generation"]
            )
            write_status("idle", pid)
            refreshed = fresh_state()
            assert refreshed["snapshot"]["panes"][0]["lastStatus"] == 3
            # An observer must not count as user focus and clear unread completion.
            wait_for(
                lambda: snapshot()["panes"][0]["lastStatus"] == 3,
                "preview observer does not mark completed agent read",
            )
            latencies = []
            for index in range(20):
                marker = f"live-frame-{index:03d}"
                start = time.monotonic()
                temporary = output.with_suffix(".tmp")
                temporary.write_text(marker)
                temporary.replace(output)
                wait_for(lambda marker=marker: marker in screen(), marker)
                latencies.append((time.monotonic() - start) * 1000)
                time.sleep(0.023)
            print(
                f"Preview latency: median={statistics.median(latencies):.1f} ms, max={max(latencies):.1f} ms"
            )
            for index in range(300):
                temporary = output.with_suffix(".tmp")
                temporary.write_text(f"burst-{index:03d}")
                temporary.replace(output)
                time.sleep(0.002)
            wait_for(lambda: "burst-299" in screen(), "latest burst frame")

            client = tmux("list-clients", "-F", "#{client_name}")
            tmux("detach-client", "-t", client)
            output.write_text("reconnected-preview")
            wait_for(
                lambda: "reconnected-preview" in screen(), "control client reconnects"
            )
            output.write_text("A" * 85 + "resize-tail")
            wait_for(lambda: "resize-tail" in screen(), "unwrapped source line")
            tmux("resize-window", "-t", pane, "-x", "90", "-y", "28")
            wait_for(
                lambda: "resize-tail" not in screen() and "e-tail" in screen(),
                "preview follows source reflow without new application output",
            )
            tmux("resize-window", "-t", sidebar, "-x", "130", "-y", "38")
            wait_for(lambda: "e-tail" in screen(), "preview survives UI resize")
            tmux("send-keys", "-t", sidebar, "q")
            wait_for(
                lambda: not tmux("list-clients", "-F", "#{client_control_mode}"),
                "preview control client exits",
            )

            watcher.send_signal(signal.SIGSTOP)
            sidebar = tmux(
                "new-window", "-t", "test:", "-P", "-F", "#{pane_id}", launch
            )
            wait_for(
                lambda: (
                    "source-fixture" in screen()
                    and "Refreshing session status" in screen()
                ),
                "bounded startup falls back to cached list",
                timeout=3.5,
            )
            watcher.send_signal(signal.SIGCONT)
            wait_for(
                lambda: "Refreshing session status" not in screen(),
                "fresh status replaces fallback",
            )
            tmux("send-keys", "-t", sidebar, "q")
            wait_for(
                lambda: not tmux("list-clients", "-F", "#{client_control_mode}"),
                "fallback preview client exits",
            )
            interactive = subprocess.Popen(
                ["tmux", "-L", server, "-C", "attach-session", "-t", "agent"],
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            try:
                tmux("select-window", "-t", pane)
                wait_for(
                    lambda: snapshot()["panes"][0]["lastStatus"] == 0,
                    "interactive control client still counts as user focus",
                )
            finally:
                interactive.terminate()
                interactive.wait(timeout=5)
            print(
                "PASS: fresh startup, bounded fallback, live preview, coalescing, reconnect, resize, focus, cleanup"
            )
        finally:
            if watcher is not None:
                watcher.send_signal(signal.SIGCONT)
                watcher.terminate()
                try:
                    watcher.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    watcher.kill()
                    watcher.wait()
            subprocess.run(
                ["tmux", "-L", server, "kill-server"],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )


if __name__ == "__main__":
    main()
