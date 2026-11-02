import os
import signal
import sys
import time

import pytest
from harness import fingerprint, wait_for


def launch_with_preview(mux):
    mux.add_agent("cached")
    pane = mux.launch()
    mux.wait_screen(
        pane, lambda frame: "preview-cached" in frame, "render cached session"
    )
    return pane, mux.ready()


def test_upgrade_preserves_state_with_multiple_clients(mux):
    pane, first = launch_with_preview(mux)
    mux.send(pane, "o")
    wait_for(
        lambda: mux.ui_state().get("sortMode") == "order", "persist sort preference"
    )
    other = mux.launch()
    mux.wait_screen(other, lambda frame: "preview-cached" in frame, "second TUI ready")
    expected = mux.replace_binary(b"upgrade-fixture")
    wait_for(
        lambda: mux.request("Status").get("build_id") == expected,
        "open TUIs apply atomic executable upgrade",
    )
    second = mux.ready()
    assert second != first
    assert mux.ui_state()["sortMode"] == "order"
    # Cross a full reconciliation interval to detect competing TUI upgrades.
    time.sleep(3.2)
    assert mux.ready() == second


def test_open_tui_recovers_crashed_watcher(mux):
    pane, first = launch_with_preview(mux)
    os.kill(first, signal.SIGKILL)
    assert mux.ready(previous=first) != first
    mux.wait_screen(
        pane, lambda frame: "preview-cached" in frame, "preview survives recovery"
    )


def test_rejected_upgrade_preserves_healthy_watcher(mux):
    pane, first = launch_with_preview(mux)
    replacement = mux.root / "invalid"
    replacement.write_text("invalid executable\n")
    replacement.chmod(0o755)
    replacement.replace(mux.binary)
    mux.wait_screen(
        pane,
        lambda frame: "validate installed executable" in frame,
        "show failed upgrade without hiding session list",
    )
    assert mux.ready() == first
    assert "cached" in mux.screen(pane) and "preview-cached" in mux.screen(pane)
    mux.replace_binary()
    mux.wait_screen(pane, lambda frame: "Error:" not in frame, "reinstall clears error")
    assert mux.ready() == first


@pytest.mark.slow
def test_open_tui_recovers_stopped_watcher(mux):
    pane, first = launch_with_preview(mux)
    os.kill(first, signal.SIGSTOP)
    assert mux.ready(previous=first, timeout=25) != first
    mux.wait_screen(
        pane, lambda frame: "preview-cached" in frame, "cached preview stays usable"
    )


@pytest.mark.slow
def test_stale_lock_never_signals_unrelated_process(mux):
    lock = mux.state / "watch.lock"
    lock.touch()
    unrelated = mux.spawn(
        sys.executable,
        "-c",
        "import fcntl, os, sys, time; "
        "f=open(sys.argv[1], 'r+'); fcntl.flock(f, fcntl.LOCK_EX); "
        "f.seek(0); f.truncate(); f.write(str(os.getpid())); f.flush(); "
        "time.sleep(600)",
        str(lock),
    )
    wait_for(lambda: lock.read_text() == str(unrelated.pid), "fixture owns stale lock")
    pane = mux.launch()
    mux.wait_screen(
        pane,
        lambda frame: "refusing to stop unverified" in frame,
        "refuse to signal unrelated lock owner",
    )
    assert unrelated.poll() is None
    unrelated.terminate()
    unrelated.wait(timeout=5)
    assert mux.ready() != unrelated.pid


@pytest.mark.slow
def test_symlink_upgrade_follows_selected_installation(mux):
    link = mux.root / "stable-mux"
    link.symlink_to(mux.binary)
    pane = mux.launch(link)
    first = mux.ready()
    # An older TUI must follow the selected installation, not downgrade it.
    old_client = mux.launch()
    mux.wait_screen(
        old_client, lambda frame: "No active sessions" in frame, "older TUI ready"
    )
    mux.tmux("kill-pane", "-t", pane)
    pane = mux.launch(link)
    wait_for(
        lambda: (
            (mux.state / "daemon-executable.json").read_text().strip() == f'"{link}"'
        ),
        "select stable symlink path",
    )
    image = mux.root / "next-version"
    image.write_bytes(mux.source.read_bytes() + b"symlink-upgrade")
    image.chmod(0o755)
    temporary = mux.root / "next-link"
    temporary.symlink_to(image)
    temporary.replace(link)
    wait_for(
        lambda: mux.request("Status").get("build_id") == fingerprint(image),
        "symlink replacement upgrades watcher",
    )
    second = mux.ready()
    assert second != first
    time.sleep(3.2)
    assert mux.ready() == second
