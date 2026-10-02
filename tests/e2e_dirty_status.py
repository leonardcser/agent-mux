"""Run after cargo build --release: python3 tests/e2e_dirty_status.py."""

import json
import os
import shlex
import subprocess
import tempfile
import time
from pathlib import Path


def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def main():
    binary = Path(__file__).resolve().parents[1] / "target/release/agent-mux"
    server = f"mux-e2e-{os.getpid()}"
    watcher = None
    refreshes = []
    samples = 0
    with tempfile.TemporaryDirectory(prefix="mux-e2e-") as directory:
        root = Path(directory).resolve()
        repo = root / "mux-fixture"
        repo.mkdir()
        run("git", "init", "--initial-branch=main", str(repo))
        tracked = repo / "tracked.txt"
        tracked.write_text("original\n")
        run("git", "-C", str(repo), "add", "tracked.txt")
        run(
            "git",
            "-C",
            str(repo),
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-m",
            "initial",
        )
        # A deterministic idle provider, without starting an actual AI session.
        provider = root / "smelt"
        run(
            "cc",
            "-x",
            "c",
            "-",
            "-o",
            str(provider),
            input="#include <unistd.h>\nint main(void) { sleep(120); return 0; }\n",
        )
        env = dict(os.environ, AGENT_MUX_STATE_DIR=str(root / "state"))

        def tmux(*args):
            return run("tmux", "-f", "/dev/null", "-L", server, *args)

        try:
            tmux(
                "new-session",
                "-d",
                "-s",
                "test",
                "-x",
                "160",
                "-y",
                "45",
                "-c",
                str(repo),
                f"{shlex.quote(str(provider))} 120",
            )
            env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
            watcher = subprocess.Popen(
                [str(binary), "watch"],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            pane = tmux(
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-c",
                str(repo),
                f"env AGENT_MUX_STATE_DIR={shlex.quote(env['AGENT_MUX_STATE_DIR'])} "
                f"{shlex.quote(str(binary))}",
            )
            snapshot_path = root / "state/snapshot.json"
            last_revision = 0

            def observe(expected):
                nonlocal last_revision
                assert watcher.poll() is None, "watcher exited unexpectedly"
                try:
                    snapshot = json.loads(snapshot_path.read_text())
                except FileNotFoundError:
                    return False
                assert snapshot["revision"] >= last_revision
                last_revision = snapshot["revision"]
                panes = [p for p in snapshot["panes"] if p["path"] == str(repo)]
                assert len(panes) <= 1
                if not panes:
                    return False
                status = panes[0]
                screen = tmux("capture-pane", "-p", "-t", pane)
                headers = [
                    line.split("│")[0].strip()
                    for line in screen.splitlines()
                    if line.lstrip().startswith("mux-fixture")
                ]
                return (
                    status.get("projectDirty", False) == expected
                    and status.get("gitDirty", False) == expected
                    and len(headers) == 1
                    and headers[0].endswith("main*" if expected else "main")
                )

            def check_phase(label, expected):
                nonlocal samples
                actual = bool(
                    run(
                        "git",
                        "-C",
                        str(repo),
                        "--no-optional-locks",
                        "status",
                        "--porcelain",
                    )
                )
                assert actual == expected, f"Git disagrees in {label}"
                deadline = time.monotonic() + 10
                while not observe(expected):
                    assert time.monotonic() < deadline, (
                        f"UI did not converge in {label}"
                    )
                    time.sleep(0.15)
                end = time.monotonic() + 4
                next_refresh = 0
                while time.monotonic() < end:
                    if time.monotonic() >= next_refresh:
                        refreshes.append(
                            subprocess.Popen(
                                [str(binary), "refresh"],
                                env=env,
                                stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE,
                            )
                        )
                        next_refresh = time.monotonic() + 0.8
                    assert observe(expected), f"dirty marker flickered in {label}"
                    samples += 1
                    time.sleep(0.15)
                print(f"{label}: snapshot and rendered marker match Git", flush=True)

            check_phase("clean", False)
            tracked.write_text("modified\n")
            check_phase("unstaged edit", True)
            tracked.write_text("original\n")
            check_phase("restored tracked file", False)
            untracked = repo / "mux-untracked.txt"
            untracked.write_text("new\n")
            check_phase("untracked file", True)
            untracked.unlink()
            check_phase("removed untracked file", False)
            for refresh in refreshes:
                assert refresh.wait(timeout=10) == 0, refresh.stderr.read().decode()
            print(
                f"Passed: {samples} rendered samples, {len(refreshes)} hook refreshes"
            )
        finally:
            for refresh in refreshes:
                if refresh.poll() is None:
                    refresh.terminate()
                refresh.wait(timeout=10)
            if watcher:
                watcher.terminate()
                watcher.wait(timeout=10)
            subprocess.run(
                ["tmux", "-L", server, "kill-server"],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )


if __name__ == "__main__":
    main()
