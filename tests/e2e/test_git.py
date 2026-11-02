import time

import pytest
from harness import run, wait_for


@pytest.fixture
def project(mux, repo):
    mux.add_agent(cwd=repo)
    return repo / "tracked.txt", mux.launch()


def observe(mux, pane, dirty):
    panes = mux.snapshot().get("panes", [])
    headers = [
        line.split("│")[0].strip()
        for line in mux.screen(pane).splitlines()
        if line.lstrip().startswith("project with spaces")
    ]
    return (
        len(panes) == 1
        and panes[0].get("projectDirty", False) == dirty
        and panes[0].get("gitDirty", False) == dirty
        and len(headers) == 1
        and headers[0].endswith("main*" if dirty else "main")
    )


def test_git_changes_reach_snapshot_and_sidebar(mux, project):
    tracked, pane = project
    for contents, dirty in [
        ("original\n", False),
        ("modified\n", True),
        ("original\n", False),
    ]:
        tracked.write_text(contents)
        run(str(mux.binary), "refresh", env=mux.env)
        wait_for(
            lambda dirty=dirty: observe(mux, pane, dirty), f"render Git dirty={dirty}"
        )


@pytest.mark.slow
def test_concurrent_refresh_does_not_flicker_dirty_marker(mux, project):
    tracked, pane = project
    for contents, dirty in [("original\n", False), ("modified\n", True)]:
        tracked.write_text(contents)
        wait_for(
            lambda dirty=dirty: observe(mux, pane, dirty), f"converge to dirty={dirty}"
        )
        end = time.monotonic() + 4
        next_refresh = 0
        children = []
        revision = 0
        while time.monotonic() < end:
            if time.monotonic() >= next_refresh:
                children.append(mux.spawn(str(mux.binary), "refresh"))
                next_refresh = time.monotonic() + 0.4
            snapshot = mux.snapshot()
            assert snapshot["revision"] >= revision
            revision = snapshot["revision"]
            assert observe(mux, pane, dirty), "dirty marker flickered during refresh"
            time.sleep(0.1)
        for child in children:
            assert child.wait(timeout=5) == 0
