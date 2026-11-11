import shlex
import time

import pytest
from harness import run, wait_for


def test_busy_parent_stays_working_while_nested_smelt_runs(mux):
    agent, pid = mux.add_agent("nested-parent")
    statuses = [{"pid": pid, "state": "busy"}]
    mux.write_statuses(statuses)
    pane = mux.launch()
    mux.wait_screen(pane, lambda frame: "working (1)" in frame, "parent starts working")

    for command, marker in [("s", "child-running"), ("x", "child-stopped")]:
        mux.send(agent, command, "Enter")
        mux.wait_screen(agent, lambda frame, marker=marker: marker in frame, marker)
        # Cross several polls with unchanged output to catch heuristic fallback.
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            assert "working (1)" in mux.screen(pane)
            time.sleep(0.1)


def test_sidebar_navigation_stash_restore_and_click(mux):
    mux.config.write_text(
        '[sidebar]\nworking = "section"\nselect = "last"\nsort = "creation"\n'
    )
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


def test_inline_last_keeps_busy_session_below_idle(mux, repo):
    mux.config.write_text(
        '[sidebar]\nworking = "inline-last"\nsort = "creation"\nselect = "first"\n'
    )
    _, busy_pid = mux.add_agent("busy", cwd=repo)
    _, idle_pid = mux.add_agent("idle", cwd=repo)
    mux.write_statuses(
        [{"pid": busy_pid, "state": "busy"}, {"pid": idle_pid, "state": "idle"}]
    )
    pane = mux.launch()
    mux.wait_screen(
        pane,
        lambda frame: "preview-idle" in frame,
        "select idle session before busy session",
    )
    sidebar = "\n".join(line.split("│", 1)[0] for line in mux.screen(pane).splitlines())
    assert "working (" not in sidebar
    assert sidebar.index("idle") < sidebar.index("busy")
    mux.send(pane, "j")
    mux.wait_screen(
        pane, lambda frame: "preview-busy" in frame, "navigate to busy session"
    )


@pytest.mark.parametrize("key", ["w", "\x1b[13;5u"], ids=["w", "ctrl-enter"])
def test_manual_working_parking_returns_after_completion(mux, repo, key):
    mux.config.write_text(
        '[sidebar]\nworking = "manual"\nsort = "creation"\nselect = "last"\n'
    )
    _, busy_pid = mux.add_agent("busy", cwd=repo)
    _, idle_pid = mux.add_agent("idle", cwd=repo)
    mux.write_statuses(
        [{"pid": busy_pid, "state": "busy"}, {"pid": idle_pid, "state": "idle"}]
    )
    pane = mux.launch()
    mux.wait_screen(pane, lambda frame: "preview-busy" in frame, "select busy agent")
    assert "working (" not in mux.screen(pane)
    mux.send(pane, "-l", key)
    mux.wait_screen(
        pane,
        lambda frame: "working (1)" in frame and "preview-idle" in frame,
        "park busy agent and select next agent",
    )
    mux.send(pane, "q")
    wait_for(
        lambda: (
            pane not in mux.tmux("list-panes", "-a", "-F", "#{pane_id}").splitlines()
        ),
        "close sidebar",
    )
    pane = mux.launch()
    mux.wait_screen(pane, lambda frame: "working (1)" in frame, "restore parking")
    mux.send(pane, "j")
    mux.wait_screen(pane, lambda frame: "preview-busy" in frame, "select parked agent")
    mux.send(pane, "-l", key)
    mux.wait_screen(pane, lambda frame: "working (" not in frame, "unpark early")
    mux.send(pane, "-l", key)
    mux.wait_screen(pane, lambda frame: "working (1)" in frame, "park again")
    mux.write_statuses(
        [{"pid": busy_pid, "state": "idle"}, {"pid": idle_pid, "state": "idle"}]
    )
    mux.wait_screen(
        pane, lambda frame: "working (" not in frame, "return finished agent"
    )
    sidebar = "\n".join(line.split("│", 1)[0] for line in mux.screen(pane).splitlines())
    assert sidebar.index("busy") < sidebar.index("idle")
    mux.write_statuses(
        [{"pid": busy_pid, "state": "busy"}, {"pid": idle_pid, "state": "idle"}]
    )
    wait_for(
        lambda: any(p.get("lastStatus") == 1 for p in mux.snapshot()["panes"]),
        "detect new busy period",
    )
    assert "working (" not in mux.screen(pane)


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


def add_worktree(mux, repo):
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
    return worktree


def launch_with_picker(mux, repo, key):
    worktree = add_worktree(mux, repo)
    mux.tmux("new-session", "-d", "-s", "agents", "exec sleep 600")
    mux.add_agent("selected", cwd=repo, session="agents")
    mux.attach()
    pane = mux.launch()
    mux.wait_screen(
        pane, lambda frame: "preview-selected" in frame, "preview selected project"
    )
    mux.send(pane, key)
    mux.wait_screen(
        pane,
        lambda frame: "Choose a worktree" in frame and "feature" in frame,
        "worktree picker lists both checkouts",
    )
    return pane, worktree


def test_worktree_picker_escape_keeps_the_sidebar_open(mux, repo):
    pane, _ = launch_with_picker(mux, repo, "A")
    mux.send(pane, "Escape")
    mux.wait_screen(
        pane, lambda frame: "Choose a worktree" not in frame, "picker closes"
    )
    assert pane in mux.tmux("list-panes", "-a", "-F", "#{pane_id}").splitlines()


def test_worktree_picker_opens_an_agent_window_in_the_chosen_worktree(mux, repo):
    pane, worktree = launch_with_picker(mux, repo, "A")
    before = mux.tmux("list-windows", "-t", "agents", "-F", "#{window_id}").splitlines()
    mux.send(pane, "-l", "feat")
    mux.wait_screen(pane, lambda frame: "> feat" in frame, "query typed")
    mux.send(pane, "Enter")
    wait_for(
        lambda: (
            len(
                mux.tmux(
                    "list-windows", "-t", "agents", "-F", "#{window_id}"
                ).splitlines()
            )
            == len(before) + 1
        ),
        "agent window added",
    )
    wait_for(
        lambda: (
            str(worktree)
            in mux.tmux(
                "list-panes", "-s", "-t", "agents", "-F", "#{pane_current_path}"
            ).splitlines()
        ),
        "new window starts in the worktree",
    )


def test_worktree_picker_opens_one_terminal_per_worktree(mux, repo):
    pane, worktree = launch_with_picker(mux, repo, "T")
    mux.send(pane, "-l", "feat")
    mux.wait_screen(pane, lambda frame: "> feat" in frame, "query typed")
    mux.send(pane, "Enter")
    wait_for(
        lambda: (
            str(worktree)
            in mux.tmux(
                "list-panes", "-s", "-t", "agents", "-F", "#{pane_current_path}"
            ).splitlines()
        ),
        "terminal window opens in the worktree",
    )
    tags = mux.tmux(
        "list-windows", "-t", "agents", "-F", "#{@agent-mux-terminal}"
    ).splitlines()
    assert f"terminal:{worktree}" in tags
    assert "terminal" not in tags


def test_waiting_session_is_unread_until_read(mux):
    _, idle_pid = mux.add_agent("idle")
    _, waiting_pid = mux.add_agent("waiting")
    mux.write_statuses(
        [
            {"pid": idle_pid, "state": "inactive"},
            {"pid": waiting_pid, "state": "waiting"},
        ]
    )
    pane = mux.launch()
    mux.wait_screen(
        pane,
        lambda frame: "preview-waiting" in frame,
        "startup selects the unread waiting session",
    )

    def icon_lines(icon):
        frame = mux.tmux("capture-pane", "-p", "-e", "-t", pane)
        return [line for line in frame.splitlines() if icon in line]

    violet = "38;2;155;155;245"
    orange = "38;2;217;119;6"
    assert any(violet in line for line in icon_lines("●"))
    mux.send(pane, "Space")
    wait_for(
        lambda: not any(violet in line for line in icon_lines("●"))
        and any(orange in line for line in icon_lines("○")),
        "reading turns the violet dot into an orange ring",
    )


def test_recreated_worktree_rejoins_its_project(mux, repo):
    worktree = add_worktree(mux, repo)
    mux.add_agent("selected", cwd=worktree)
    pane = mux.launch()
    mux.wait_screen(
        pane,
        lambda frame: "project with spaces" in frame,
        "session grouped under its project",
    )

    run(
        "git",
        "-C",
        str(repo),
        "worktree",
        "remove",
        "--force",
        str(worktree),
        env=mux.env,
    )
    mux.wait_screen(
        pane, lambda frame: "(deleted)" in frame, "deleted worktree is marked"
    )
    run(
        "git",
        "-C",
        str(repo),
        "worktree",
        "add",
        str(worktree),
        "feature",
        env=mux.env,
    )
    mux.wait_screen(
        pane,
        lambda frame: "(deleted)" not in frame and "project with spaces" in frame,
        "recreated worktree rejoins the project",
    )
