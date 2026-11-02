"""Run after cargo build --release: python3 tests/e2e_sidebar.py."""

import json
import os
import shlex
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def wait_for(check, label):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.1)
    raise AssertionError(f"Timed out: {label}")


def read_json(path):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return {}


def main():
    binary = Path(__file__).resolve().parents[1] / "target/release/agent-mux"
    server = f"mux-sidebar-{os.getpid()}"
    watcher = None
    client = None
    with tempfile.TemporaryDirectory(prefix="mux-sidebar-") as directory:
        root = Path(directory)
        source = binary
        binary = root / "agent-mux"
        shutil.copy2(source, binary)
        provider = root / "smelt"
        statuses = root / "statuses.json"
        statuses.write_text("[]")
        run(
            "cc",
            "-x",
            "c",
            "-",
            "-o",
            str(provider),
            input="""#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "status") == 0) {
        FILE *f = fopen(getenv("SIDEBAR_STATUSES"), "r");
        if (!f) return 1;
        int c;
        while ((c = fgetc(f)) != EOF) putchar(c);
        fclose(f);
        return 0;
    }
    printf("preview-fixture-%s\\n", argv[1]);
    fflush(stdout);
    sleep(120);
    return 0;
}
""",
        )
        env = dict(
            os.environ,
            AGENT_MUX_STATE_DIR=str(root / "state"),
            XDG_CONFIG_HOME=str(root / "config"),
            SIDEBAR_STATUSES=str(statuses),
            PATH=f"{root}:{os.environ['PATH']}",
        )

        def tmux(*args):
            return run("tmux", "-f", "/dev/null", "-L", server, *args)

        def write_statuses(values):
            temporary = statuses.with_suffix(".tmp")
            temporary.write_text(json.dumps(values))
            temporary.replace(statuses)

        def ui_state():
            return read_json(root / "state/ui_state.json")

        try:
            tmux(
                "new-session", "-d", "-s", "test", "-x", "160", "-y", "45", "sleep 120"
            )
            env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
            agents = []
            for name in ["busy-fixture", "idle-fixture", "stash-fixture"]:
                pane_id, pid = tmux(
                    "new-window",
                    "-d",
                    "-P",
                    "-F",
                    "#{pane_id} #{pane_pid}",
                    "-n",
                    name,
                    "-c",
                    str(root),
                    f"{shlex.quote(str(provider))} {len(agents)}",
                ).split()
                agents.append((pane_id, int(pid)))
            values = [
                {"pid": pid, "state": "busy" if i == 0 else "idle"}
                for i, (_, pid) in enumerate(agents)
            ]
            write_statuses(values)
            watcher = subprocess.Popen(
                [str(binary), "watch"],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            launch = (
                "env "
                + " ".join(
                    f"{key}={shlex.quote(env[key])}"
                    for key in [
                        "AGENT_MUX_STATE_DIR",
                        "XDG_CONFIG_HOME",
                        "SIDEBAR_STATUSES",
                        "PATH",
                    ]
                )
                + " "
                + shlex.quote(str(binary))
            )

            def open_sidebar():
                return tmux(
                    "new-window", "-P", "-F", "#{pane_id}", "-n", "sidebar", launch
                )

            snapshot_path = root / "state/snapshot.json"
            wait_for(
                lambda: len(read_json(snapshot_path).get("panes", [])) == 3,
                "watcher discovers fixture sessions",
            )
            sidebar = open_sidebar()

            def screen():
                assert watcher.poll() is None, "watcher exited"
                return tmux("capture-pane", "-p", "-t", sidebar)

            def send(key):
                tmux("send-keys", "-t", sidebar, key)

            labels = ["busy-fixture", "idle-fixture", "stash-fixture"]

            def hover(label):
                row = next(
                    i
                    for i, line in enumerate(screen().splitlines())
                    if label in line.split("│", 1)[0]
                )
                tmux("send-keys", "-t", sidebar, "-l", f"\x1b[<35;3;{row + 1}M")

            def wait_for_screen(check, label):
                wait_for(lambda: check(screen()), label)

            def preview(index, frame):
                return f"preview-fixture-{index}" in frame

            def visible_sessions(frame):
                return [
                    name
                    for line in frame.splitlines()
                    for name in labels
                    if name in line.split("│", 1)[0]
                ]

            def first_session_preview(frame):
                visible = visible_sessions(frame)
                return bool(visible) and preview(labels.index(visible[0]), frame)

            def status(index):
                panes = read_json(snapshot_path).get("panes", [])
                return next(
                    (
                        p.get("lastStatus", 0)
                        for p in panes
                        if p["paneID"] == agents[index][0]
                    ),
                    None,
                )

            wait_for_screen(
                lambda frame: (
                    "working (1)" in frame
                    and "idle-fixture" in frame
                    and "stash-fixture" in frame
                    and "busy-fixture" not in frame
                ),
                "automatic collapsed Working",
            )
            wait_for_screen(first_session_preview, "startup focuses first session")
            hover("stash-fixture")
            wait_for_screen(
                lambda frame: preview(2, frame), "hover selects session preview"
            )
            send("s")
            wait_for_screen(
                lambda frame: "stashed (1)" in frame and "stash-fixture" not in frame,
                "collapsed Stashed",
            )
            hover("stashed (1)")
            wait_for_screen(
                lambda frame: "stash-fixture" in frame and preview(2, frame),
                "hover expands Stashed",
            )
            hover("stashed (1)")
            hover("working (1)")
            wait_for_screen(
                lambda frame: (
                    "busy-fixture" in frame
                    and "stash-fixture" not in frame
                    and preview(0, frame)
                ),
                "hover selects Working and collapses Stashed",
            )
            send("q")
            sidebar = open_sidebar()
            wait_for_screen(
                lambda frame: "busy-fixture" in frame and preview(0, frame),
                "restore selection inside expanded Working",
            )
            hover("idle-fixture")
            wait_for_screen(
                lambda frame: preview(1, frame), "hover updates main session preview"
            )
            send("s")
            wait_for_screen(
                lambda frame: "busy-fixture" in frame and preview(0, frame),
                "stashing last main session selects first Working session",
            )
            hover("stashed (2)")
            wait_for_screen(
                lambda frame: "idle-fixture" in frame,
                "expand Stashed to restore main session",
            )
            hover("idle-fixture")
            wait_for_screen(
                lambda frame: preview(1, frame), "select stashed main session"
            )
            send("u")
            wait_for_screen(
                lambda frame: "stashed (1)" in frame, "unstash main session"
            )
            assert tmux("display-message", "-p", "-t", "test", "#{pane_id}") == sidebar

            values[2]["state"] = "busy"
            write_statuses(values)
            wait_for(lambda: status(2) == 1, "stashed agent becomes busy")
            frame = screen()
            assert "working (1)" in frame and "stash-fixture" not in frame
            hover("stashed (1)")
            wait_for_screen(
                lambda frame: "stash-fixture" in frame and preview(2, frame),
                "busy stashed session stays in Stashed",
            )
            send("q")
            sidebar = open_sidebar()
            wait_for_screen(
                lambda frame: "stash-fixture" in frame and preview(2, frame),
                "restore selection inside expanded Stashed",
            )
            hover("idle-fixture")
            wait_for_screen(
                lambda frame: "stash-fixture" not in frame, "collapse busy Stashed"
            )
            for _ in range(40):
                frame = screen()
                assert "working (1)" in frame and "busy-fixture" not in frame
                time.sleep(0.1)

            send("q")
            values[0]["state"] = "needs_attention"
            write_statuses(values)
            wait_for(
                lambda: status(0) in (0, 3), "agent completes while sidebar is closed"
            )
            sidebar = open_sidebar()
            wait_for_screen(
                lambda frame: "busy-fixture" in frame and "working (" not in frame,
                "completed session returns to main list",
            )
            wait_for_screen(
                lambda frame: preview(0, frame),
                "startup selects completed unread session",
            )
            frame = screen()
            assert "stashed (1)" in frame and "stash-fixture" not in frame

            values[1]["state"] = "busy"
            write_statuses(values)
            wait_for(lambda: status(1) == 1, "another session starts working")
            values[1]["state"] = "idle"
            write_statuses(values)
            wait_for(lambda: status(1) == 3, "another session completes unread")
            send("q")
            sidebar = open_sidebar()
            wait_for_screen(
                lambda frame: preview(1, frame),
                "startup selects first unread in recent order",
            )

            config_path = Path(env["XDG_CONFIG_HOME"]) / "agent-mux/config.toml"
            config_path.parent.mkdir(parents=True, exist_ok=True)

            def write_startup_config(select, sort=None):
                source = f'[sidebar]\nselect = "{select}"\n'
                if sort is not None:
                    source += f'sort = "{sort}"\n'
                config_path.write_text(source)

            for select, sort, saved, expected in [
                ("last", "recent", 0, 0),
                ("first", "creation", 1, 0),
                ("first", "recent", 0, 1),
            ]:
                hover(labels[saved])
                wait_for_screen(
                    lambda frame, saved=saved: preview(saved, frame),
                    "save startup selection",
                )
                write_startup_config(select, sort)
                send("q")
                sidebar = open_sidebar()
                expected_order = (
                    labels[:2] if sort == "creation" else list(reversed(labels[:2]))
                )
                wait_for_screen(
                    lambda frame, expected=expected, order=expected_order: (
                        preview(expected, frame) and visible_sessions(frame) == order
                    ),
                    "configured startup selection and sort apply",
                )

            send("o")
            wait_for(
                lambda: ui_state().get("sortMode") == "order", "sort toggle is saved"
            )
            send("q")
            sidebar = open_sidebar()
            wait_for_screen(
                lambda frame: (
                    preview(1, frame)
                    and visible_sessions(frame) == list(reversed(labels[:2]))
                ),
                "explicit sort overrides saved toggle on startup",
            )
            send("o")
            write_startup_config("first")
            send("q")
            sidebar = open_sidebar()
            wait_for_screen(
                lambda frame: (
                    preview(0, frame) and visible_sessions(frame) == labels[:2]
                ),
                "omitted sort restores saved creation order",
            )
            send("o")
            config_path.unlink()

            for index, label in enumerate(labels[:2]):
                pane_id = agents[index][0]
                hover(label)
                wait_for_screen(
                    lambda frame, index=index: preview(index, frame),
                    "select session to stash",
                )
                send("s")
                wait_for(
                    lambda pane_id=pane_id: (
                        ui_state().get("panes", {}).get(pane_id, {}).get("stashed")
                    ),
                    "persist stash",
                )
            send("q")
            sidebar = open_sidebar()
            wait_for_screen(
                lambda frame: (
                    "stashed (3)" in frame and all(label in frame for label in labels)
                ),
                "startup expands only Stashed",
            )
            wait_for_screen(
                first_session_preview, "startup selects first stashed session"
            )
            values[0]["state"] = "busy"
            write_statuses(values)
            wait_for(lambda: status(0) == 1, "stashed session becomes busy")
            hover("busy-fixture")
            wait_for_screen(
                lambda frame: preview(0, frame), "select busy stashed session"
            )
            send("u")
            wait_for_screen(
                lambda frame: "working (1)" in frame,
                "unstash busy session into Working",
            )
            hover("stashed (2)")
            wait_for_screen(
                lambda frame: "idle-fixture" in frame, "expand remaining Stashed"
            )
            hover("idle-fixture")
            wait_for_screen(
                lambda frame: preview(1, frame), "select idle stashed session"
            )
            send("u")
            wait_for_screen(
                lambda frame: "stashed (1)" in frame,
                "unstash idle session into main list",
            )

            config_path = Path(env["XDG_CONFIG_HOME"]) / "agent-mux/config.toml"
            config_path.parent.mkdir(parents=True, exist_ok=True)

            def write_config(working, stashed):
                config_path.write_text(
                    f'[sidebar]\nworking = "{working}"\nstashed = "{stashed}"\n'
                )

            for working, stashed in [
                ("expand", "collapse"),
                ("collapse", "expand"),
                ("expand", "expand"),
            ]:
                write_config(working, stashed)
                send("q")
                sidebar = open_sidebar()
                wait_for_screen(
                    lambda frame, working=working, stashed=stashed: (
                        preview(1, frame)
                        and ("busy-fixture" in frame) == (working == "expand")
                        and ("stash-fixture" in frame) == (stashed == "expand")
                    ),
                    "section configuration applies on reopen",
                )
                hover("working (1)")
                wait_for_screen(
                    lambda frame: preview(0, frame), "enter configured Working"
                )
                hover("stashed (1)")
                wait_for_screen(
                    lambda frame: preview(2, frame), "enter configured Stashed"
                )
                hover("idle-fixture")
                wait_for_screen(
                    lambda frame, working=working, stashed=stashed: (
                        preview(1, frame)
                        and ("busy-fixture" in frame) == (working == "expand")
                        and ("stash-fixture" in frame) == (stashed == "expand")
                    ),
                    "configured expansion persists when focus leaves",
                )

            config_path.write_text("[sidebar]\nworking = 'automatic'\n")
            invalid = subprocess.run(
                [str(binary)],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            assert invalid.returncode != 0
            assert str(config_path) in invalid.stderr
            assert "working" in invalid.stderr
            write_config("expand", "expand")

            client = subprocess.Popen(
                ["tmux", "-L", server, "-C", "attach-session", "-t", "test"],
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            wait_for(
                lambda: bool(tmux("list-clients", "-F", "#{client_name}")),
                "attach test client",
            )
            row = next(
                i
                for i, line in enumerate(screen().splitlines())
                if "busy-fixture" in line.split("│", 1)[0]
            )
            tmux("send-keys", "-t", sidebar, "-l", f"\x1b[<0;3;{row + 1}M")
            wait_for(
                lambda: (
                    tmux("display-message", "-p", "-t", "test", "#{pane_id}")
                    == agents[0][0]
                ),
                "click opens session",
            )
            print(
                "PASS: automatic Working, hover previews, stash priority, first-session startup, click to open"
            )
        finally:
            for process in (client, watcher):
                if process is not None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            subprocess.run(["tmux", "-L", server, "kill-server"], check=False)


if __name__ == "__main__":
    main()
