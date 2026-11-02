import signal

import pytest
from harness import atomic_write, wait_for


@pytest.fixture
def source_pane(mux):
    output = mux.root / "output.txt"
    output.write_text("initial-preview")
    mux.tmux(
        "new-session", "-d", "-s", "agents", "-x", "100", "-y", "30", "exec sleep 600"
    )
    pane, pid = mux.add_agent("source", session="agents", output=output)
    mux.tmux("new-window", "-d", "-t", "agents:", "-n", "other", "exec sleep 600")
    mux.tmux("select-window", "-t", "agents:other")
    mux.write_statuses([{"pid": pid, "state": "idle"}])
    return pane, pid, output


def test_live_preview_updates_reconnects_and_resizes_without_focus(mux, source_pane):
    source, pid, output = source_pane
    sidebar = mux.launch()
    mux.wait_screen(
        sidebar, lambda frame: "initial-preview" in frame, "initial live preview"
    )
    assert (
        mux.tmux("display-message", "-p", "-t", source, "#{pane_width}x#{pane_height}")
        == "100x30"
    )
    assert (
        mux.tmux("display-message", "-p", "-t", "agents:", "#{window_name}") == "other"
    )

    first = mux.request("SubscribeFresh")["snapshot"]
    again = mux.request("SubscribeFresh")["snapshot"]
    assert first["generation"] == again["generation"]
    mux.write_statuses([{"pid": pid, "state": "busy"}])
    assert mux.request("SubscribeFresh")["snapshot"]["panes"][0]["lastStatus"] == 1
    mux.write_statuses([{"pid": pid, "state": "idle"}])
    assert mux.request("SubscribeFresh")["snapshot"]["panes"][0]["lastStatus"] == 3

    atomic_write(output, "live-update")
    mux.wait_screen(
        sidebar, lambda frame: "live-update" in frame, "output reaches preview"
    )
    assert mux.snapshot()["panes"][0]["lastStatus"] == 3, (
        "observer must not mark completion read"
    )
    observer, flags = mux.tmux(
        "list-clients", "-F", "#{client_name}\t#{client_flags}"
    ).split("\t")
    assert "read-only" in flags and "ignore-size" in flags
    mux.tmux("detach-client", "-t", observer)
    atomic_write(output, "reconnected-preview")
    mux.wait_screen(
        sidebar,
        lambda frame: "reconnected-preview" in frame,
        "preview observer reconnects",
    )

    atomic_write(output, "A" * 85 + "resize-tail")
    mux.wait_screen(
        sidebar, lambda frame: "resize-tail" in frame, "source line before reflow"
    )
    mux.tmux("resize-window", "-t", source, "-x", "90", "-y", "28")
    mux.wait_screen(
        sidebar,
        lambda frame: "resize-tail" not in frame and "e-tail" in frame,
        "source reflow reaches preview without new application output",
    )
    mux.tmux("resize-window", "-t", sidebar, "-x", "130", "-y", "38")
    mux.wait_screen(
        sidebar, lambda frame: "e-tail" in frame, "preview survives UI resize"
    )
    mux.send(sidebar, "q")
    wait_for(lambda: not mux.tmux("list-clients"), "preview observer exits with TUI")
    mux.attach("agents")
    mux.tmux("select-window", "-t", source)
    wait_for(
        lambda: mux.snapshot()["panes"][0]["lastStatus"] == 0,
        "interactive control client still marks session read",
    )


@pytest.fixture
def watcher(mux):
    process = mux.spawn(str(mux.binary), "watch")
    try:
        mux.ready()
        yield process
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGCONT)


def test_empty_startup_loads_until_live_scan_confirms_no_sessions(mux, watcher):
    watcher.send_signal(signal.SIGSTOP)
    sidebar = mux.launch()
    mux.wait_screen(
        sidebar, lambda frame: "Loading sessions..." in frame, "delayed loading"
    )
    frame = mux.screen(sidebar)
    assert frame.count("Loading sessions...") == 1
    assert "No active sessions" not in frame
    assert "Refreshing session status" not in frame
    assert "Error:" not in frame

    watcher.send_signal(signal.SIGCONT)
    mux.wait_screen(
        sidebar,
        lambda frame: "No active sessions" in frame and "Loading sessions" not in frame,
        "live empty scan ends loading",
    )


def test_startup_shows_cache_then_silently_refreshes(mux, source_pane, watcher):
    source, pid, _ = source_pane
    wait_for(lambda: len(mux.snapshot().get("panes", [])) == 1, "cached idle snapshot")
    watcher.send_signal(signal.SIGSTOP)
    mux.write_statuses([{"pid": pid, "state": "busy"}])
    sidebar = mux.launch()
    wait_for(
        lambda: "initial-preview" in mux.screen(sidebar),
        "cached session and live preview appear before fresh scan",
        timeout=1,
    )
    frame = mux.screen(sidebar)
    assert "source" in frame and "working (1)" not in frame
    assert "Refreshing session status" not in frame
    assert "Loading sessions" not in frame

    watcher.send_signal(signal.SIGCONT)
    mux.wait_screen(
        sidebar,
        lambda frame: "working (1)" in frame and "initial-preview" in frame,
        "fresh status moves selected session into Working without losing preview",
    )
    assert "Refreshing session status" not in mux.screen(sidebar)

    watcher.send_signal(signal.SIGSTOP)
    mux.attach()
    mux.tmux("kill-pane", "-t", source)
    mux.send(sidebar, "Enter")
    mux.wait_screen(
        sidebar,
        lambda frame: "Error:" in frame and "source" in frame,
        "opening closed cached session keeps sidebar visible with error",
    )
    watcher.send_signal(signal.SIGCONT)
    mux.wait_screen(
        sidebar,
        lambda frame: "No active sessions" in frame and "source" not in frame,
        "fresh state removes closed cached session",
    )
