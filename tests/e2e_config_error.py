"""Run after cargo build --release: python3 tests/e2e_config_error.py."""

import os
import shlex
import subprocess
import tempfile
import time
from pathlib import Path

from e2e_sidebar import run, wait_for


def main():
    binary = Path(__file__).resolve().parents[1] / "target/release/agent-mux"
    server = f"mux-config-error-{os.getpid()}"
    with tempfile.TemporaryDirectory(prefix="mux-config-error-") as directory:
        root = Path(directory)
        config = root / "config/agent-mux/config.toml"
        config.parent.mkdir(parents=True)

        def tmux(*args):
            return run("tmux", "-f", "/dev/null", "-L", server, *args)

        try:
            tmux("new-session", "-d", "-s", "test", "-x", "120", "-y", "35")
            tmux("set-option", "-g", "remain-on-exit", "on")
            for source, detail in [
                ("unknown_option = true\n", "unknown_option"),
                ("[sidebar]\nworking = 'automatic'\n", "automatic"),
                ("[sidebar\n", "expected"),
            ]:
                config.write_text(source)
                launch = (
                    f"env XDG_CONFIG_HOME={shlex.quote(str(root / 'config'))} "
                    f"AGENT_MUX_STATE_DIR={shlex.quote(str(root / 'state'))} "
                    + shlex.quote(str(binary))
                )
                pane = tmux("new-window", "-P", "-F", "#{pane_id}", launch)

                def screen():
                    return tmux("capture-pane", "-p", "-t", pane)

                wait_for(lambda: detail in screen(), "show config diagnostic")
                assert tmux("display-message", "-p", "-t", pane, "#{pane_dead}") == "0", screen()
                assert "Configuration error" in screen(), screen()
                assert str(config) in screen(), screen()
                assert "q / Esc: quit" in screen(), screen()
                time.sleep(1)
                assert tmux("display-message", "-p", "-t", pane, "#{pane_dead}") == "0"
                tmux("resize-window", "-t", pane, "-x", "50", "-y", "10")
                wait_for(lambda: "j / k: scroll" in screen(), "resize error screen")
                first_page = screen()
                tmux("send-keys", "-t", pane, "j")
                wait_for(lambda: screen() != first_page, "scroll diagnostic down")
                tmux("send-keys", "-t", pane, "k")
                wait_for(lambda: screen() == first_page, "scroll diagnostic up")
                tmux("resize-window", "-t", pane, "-x", "120", "-y", "35")
                wait_for(lambda: str(config) in screen(), "restore full diagnostic")
                key = "Escape" if detail == "automatic" else "q"
                tmux("send-keys", "-t", pane, key)
                wait_for(
                    lambda: tmux("display-message", "-p", "-t", pane, "#{pane_dead}") == "1",
                    "quit error screen",
                )
                assert tmux("display-message", "-p", "-t", pane, "#{pane_dead_status}") == "0"
            print("PASS: unknown option, invalid value, broken TOML stay visible until quit")
        finally:
            subprocess.run(["tmux", "-L", server, "kill-server"], check=False)


if __name__ == "__main__":
    main()
