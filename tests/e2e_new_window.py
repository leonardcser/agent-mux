"""Run after cargo build --release: python3 tests/e2e_new_window.py."""

import os
import shlex
import shutil
import subprocess
import tempfile
from pathlib import Path

from e2e_sidebar import read_json, run, wait_for


def main():
    source = Path(__file__).resolve().parents[1] / "target/release/agent-mux"
    server = f"mux-new-window-{os.getpid()}"
    watcher = client = None
    with tempfile.TemporaryDirectory(prefix="mux-new-window-") as directory:
        root = Path(directory)
        binary = root / "agent-mux"
        shutil.copy2(source, binary)
        project = root / "project with spaces"
        project.mkdir()
        provider = root / "claude"
        run(
            "cc", "-x", "c", "-", "-o", str(provider),
            input='#include <unistd.h>\nint main(void) { sleep(120); }\n',
        )
        env = dict(
            os.environ,
            AGENT_MUX_STATE_DIR=str(root / "state"),
            XDG_CONFIG_HOME=str(root / "config"),
        )
        config = root / "config/agent-mux/config.toml"
        config.parent.mkdir(parents=True)

        def tmux(*args):
            return run("tmux", "-f", "/dev/null", "-L", server, *args)

        def windows(session):
            return tmux("list-windows", "-t", session, "-F", "#{window_index}").splitlines()

        try:
            tmux("new-session", "-d", "-s", "browser", "-x", "120", "-y", "40")
            # The selected project belongs to another session, not the browser.
            tmux("new-session", "-d", "-s", "agents", "-n", "agent-fixture", "-c", str(project), str(provider))
            tmux("new-window", "-d", "-t", "agents:7", "sleep 120")
            env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
            watcher = subprocess.Popen(
                [str(binary), "watch"], env=env,
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )
            wait_for(
                lambda: len(read_json(root / "state/snapshot.json").get("panes", [])) == 1,
                "discover selected project",
            )
            client = subprocess.Popen(
                ["tmux", "-L", server, "-C", "attach-session", "-t", "browser"],
                stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )
            wait_for(lambda: bool(tmux("list-clients")), "attach client")
            launch = "env " + " ".join(
                f"{key}={shlex.quote(env[key])}"
                for key in ("AGENT_MUX_STATE_DIR", "XDG_CONFIG_HOME")
            ) + " " + shlex.quote(str(binary))

            def add_window():
                tmux("switch-client", "-t", "browser")
                sidebar = tmux(
                    "new-window", "-t", "browser:", "-P", "-F", "#{pane_id}", launch
                )
                wait_for(
                    lambda: "agent-fixture" in tmux("capture-pane", "-p", "-t", sidebar),
                    "render selected agent",
                )
                before = windows("agents")
                browser_before = windows("browser")
                tmux("send-keys", "-t", sidebar, "a")
                wait_for(
                    lambda: len(windows("agents")) == len(before) + 1,
                    "add window with a",
                )
                indexes = windows("agents")
                assert indexes[:-1] == before, indexes
                assert int(indexes[-1]) == int(before[-1]) + 1, indexes
                wait_for(
                    lambda: tmux("list-clients", "-F", "#{session_name}") == "agents",
                    "switch client to new window",
                )
                target = f"agents:{indexes[-1]}"
                assert tmux("display-message", "-p", "-t", target, "#{window_active}") == "1"
                wait_for(
                    lambda: tmux("display-message", "-p", "-t", target, "#{pane_current_path}") == str(project),
                    "new process enters project directory",
                )
                wait_for(
                    lambda: len(windows("browser")) == len(browser_before) - 1,
                    "close browser after opening window",
                )
                return target

            shell = add_window()
            shell_marker = root / "default shell.txt"
            tmux("send-keys", "-t", shell, f"pwd > {shlex.quote(str(shell_marker))}", "Enter")
            wait_for(shell_marker.exists, "default shell accepts input")
            assert shell_marker.read_text().strip() == str(project)
            marker = root / "command result.txt"
            command = f"pwd > {shlex.quote(str(marker))}; exec sleep 120"
            # Preserve shell quoting and spaces in the configured command.
            config.write_text('new_window_command = """' + command + '"""\n')
            add_window()
            wait_for(marker.exists, "run configured shell command")
            assert marker.read_text().strip() == str(project)

            config.write_text('new_window_command = "   "\n')
            add_window()
            run("git", "init", str(project))
            run("git", "-C", str(project), "-c", "user.name=Test", "-c", "user.email=test@example.com",
                "commit", "--allow-empty", "-m", "Initial commit")
            worktree = root / "worktree with spaces"
            run("git", "-C", str(project), "worktree", "add", "-b", "feature", str(worktree))
            tmux("respawn-pane", "-k", "-t", "agents:agent-fixture", "-c", str(worktree), str(provider))
            wait_for(
                lambda: any(
                    pane.get("path") == str(worktree) and pane.get("projectRoot") == str(project)
                    for pane in read_json(root / "state/snapshot.json").get("panes", [])
                ),
                "discover worktree project group",
            )
            add_window()
            marker.unlink()
            config.write_text('new_window_command = """' + command + '"""\n')
            add_window()
            wait_for(marker.exists, "run configured command from worktree group")
            assert marker.read_text().strip() == str(project)
            print("PASS: add key, default shell, configured command, project/worktree cwd, session, append with gaps")
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
