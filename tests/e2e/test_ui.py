import shlex

import pytest
from harness import run, wait_for


def test_sidebar_navigation_stash_restore_and_click(mux):
    mux.config.write_text('[sidebar]\nselect = "last"\nsort = "creation"\n')
    idle, idle_pid = mux.add_agent("idle")
    _, busy_pid = mux.add_agent("busy")
    mux.write_statuses(
        [
            {"pid": idle_pid, "state": "idle"},
            {"pid": busy_pid, "state": "busy"},
        ]
    )
    pane = mux.launch()
    mux.wait_screen(
        pane,
        lambda frame: "working (1)" in frame and "preview-idle" in frame,
        "discover agents and preview first idle session",
    )
    mux.send(pane, "j")
    mux.wait_screen(
        pane, lambda frame: "preview-busy" in frame, "keyboard previews Working"
    )
    mux.send(pane, "s")
    mux.wait_screen(
        pane,
        lambda frame: "stashed (1)" in frame and "preview-idle" in frame,
        "stash busy session and select idle session",
    )
    mux.click(pane, "stashed (1)")
    mux.wait_screen(
        pane, lambda frame: "preview-busy" in frame, "title click expands Stashed"
    )
    mux.send(pane, "q")
    wait_for(
        lambda: (
            pane not in mux.tmux("list-panes", "-a", "-F", "#{pane_id}").splitlines()
        ),
        "close sidebar after saving selection",
    )
    pane = mux.launch()
    mux.wait_screen(
        pane, lambda frame: "preview-busy" in frame, "restore saved stashed selection"
    )
    assert any(state.get("stashed") for state in mux.ui_state()["panes"].values())
    mux.send(pane, "u")
    mux.wait_screen(
        pane,
        lambda frame: "working (1)" in frame and "stashed (" not in frame,
        "unstash busy session into Working",
    )
    mux.attach()
    mux.click(pane, "idle")
    wait_for(
        lambda: (
            mux.tmux("display-message", "-p", "-t", "browser", "#{pane_id}") == idle
        ),
        "session click switches tmux to selected agent",
    )


def test_config_error_stays_visible_scrolls_and_quits(mux):
    mux.tmux("set-option", "-g", "remain-on-exit", "on")
    mux.config.write_text("[sidebar]\nworking = 'automatic'\n")
    pane = mux.launch()
    mux.wait_screen(pane, lambda frame: "automatic" in frame, "show invalid config")
    frame = mux.screen(pane)
    assert "Configuration error" in frame and str(mux.config) in frame
    assert "q / Esc: quit" in frame
    assert mux.tmux("display-message", "-p", "-t", pane, "#{pane_dead}") == "0"
    mux.tmux("resize-window", "-t", pane, "-x", "50", "-y", "10")
    # The footer is present before resize too; wait for the wrapped diagnostic.
    first_line = "  " + f"parse config {mux.config}"[:46]
    mux.wait_screen(
        pane,
        lambda frame: frame.splitlines()[3:4] == [first_line],
        "resize diagnostic",
    )
    first_page = mux.screen(pane)
    mux.send(pane, "j")
    mux.wait_screen(pane, lambda frame: frame != first_page, "scroll down")
    mux.send(pane, "k")
    mux.wait_screen(pane, lambda frame: frame == first_page, "scroll up")
    mux.send(pane, "Escape")
    wait_for(
        lambda: mux.tmux("display-message", "-p", "-t", pane, "#{pane_dead}") == "1",
        "quit diagnostic",
    )
    assert mux.tmux("display-message", "-p", "-t", pane, "#{pane_dead_status}") == "0"


@pytest.mark.parametrize("configured", [False, True], ids=["shell", "command"])
def test_add_window_uses_project_root_from_worktree(mux, repo, configured):
    worktree = mux.root / "worktree with spaces"
    run(
        "git",
        "-C",
        str(repo),
        "worktree",
        "add",
        "-b",
        "feature",
        str(worktree),
        env=mux.env,
    )
    mux.tmux("new-session", "-d", "-s", "agents", "exec sleep 600")
    mux.add_agent("selected", cwd=worktree, session="agents")
    mux.tmux("new-window", "-d", "-t", "agents:7", "exec sleep 600")
    marker = mux.root / "command result.txt"
    if configured:
        command = f"pwd > {shlex.quote(str(marker))}; exec sleep 600"
        mux.config.write_text('new_window_command = """' + command + '"""\n')
    mux.attach()
    pane = mux.launch()
    mux.wait_screen(
        pane, lambda frame: "preview-selected" in frame, "preview selected project"
    )
    before = mux.tmux(
        "list-windows", "-t", "agents", "-F", "#{window_index}"
    ).splitlines()
    mux.send(pane, "a")
    wait_for(
        lambda: (
            len(
                mux.tmux(
                    "list-windows", "-t", "agents", "-F", "#{window_index}"
                ).splitlines()
            )
            == len(before) + 1
        ),
        "add window in agent session",
    )
    indexes = mux.tmux(
        "list-windows", "-t", "agents", "-F", "#{window_index}"
    ).splitlines()
    assert indexes[:-1] == before
    assert int(indexes[-1]) == int(before[-1]) + 1
    target = f"agents:{indexes[-1]}"
    wait_for(
        lambda: mux.tmux("list-clients", "-F", "#{session_name}") == "agents",
        "switch client",
    )
    wait_for(
        lambda: (
            mux.tmux("display-message", "-p", "-t", target, "#{pane_current_path}")
            == str(repo)
        ),
        "new process enters selected project",
    )
    if not configured:
        mux.send(target, f"pwd > {shlex.quote(str(marker))}", "Enter")
    wait_for(marker.exists, "shell command executes")
    assert marker.read_text().strip() == str(repo)
    wait_for(
        lambda: (
            pane not in mux.tmux("list-panes", "-a", "-F", "#{pane_id}").splitlines()
        ),
        "browser closes after opening window",
    )
