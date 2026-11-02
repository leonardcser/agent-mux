import json
import shutil
import tempfile
from pathlib import Path

import pytest
from harness import Sandbox, run


@pytest.fixture(scope="session")
def binary():
    for tool in ("cargo", "tmux", "cc", "git"):
        if not shutil.which(tool):
            pytest.fail(f"E2E tests require {tool}", pytrace=False)
    root = Path(__file__).resolve().parents[2]
    run("cargo", "build", "--locked", "--release", cwd=root, timeout=300)
    metadata = json.loads(
        run(
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--no-deps",
            cwd=root,
        )
    )
    return Path(metadata["target_directory"]) / "release/agent-mux"


@pytest.fixture(scope="session")
def provider(tmp_path_factory):
    path = tmp_path_factory.mktemp("provider") / "smelt"
    source = Path(__file__).resolve().parents[1] / "fixtures/provider.c"
    run("cc", "-Wall", "-Werror", str(source), "-o", str(path))
    return path


@pytest.fixture
def repo(mux):
    path = mux.root / "project with spaces"
    path.mkdir()
    run("git", "init", "--initial-branch=main", str(path), env=mux.env)
    (path / "tracked.txt").write_text("original\n")
    run("git", "-C", str(path), "add", "tracked.txt", env=mux.env)
    run(
        "git",
        "-C",
        str(path),
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
        env=mux.env,
    )
    return path


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item):
    report = (yield).get_result()
    if report.failed:
        item.test_failed = True


@pytest.fixture
def mux(request, binary, provider):
    # Short paths also keep Unix socket names below the OS length limit.
    root = Path(tempfile.mkdtemp(prefix="am-test-"))
    sandbox = Sandbox(root, binary, provider)
    try:
        sandbox.start()
        yield sandbox
    finally:
        failed = getattr(request.node, "test_failed", False)
        try:
            if failed:
                sandbox.capture_diagnostics()
        finally:
            sandbox.close()
            if failed:
                print(f"\nE2E artifacts retained: {root}")
            else:
                shutil.rmtree(root)
