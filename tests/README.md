# Testing

Requires Rust with rustfmt/Clippy, [uv](https://docs.astral.sh/uv/), Python 3.11+,
tmux, Git, and a C compiler. Run from the repository root:

```sh
scripts/check                        # Format, lint, Rust tests, fast E2Es
scripts/check --all                   # Include slow recovery/stability checks
cargo test                           # Rust only; no Python or tmux needed
uv --no-config run --locked pytest -k add_window  # Focused E2E
```

pytest builds the current release executable once, honoring Cargo's target
directory, and runs all E2Es unless filtered. uv installs locked dependencies
without user-level resolver settings. Linux CI runs `scripts/check --all`;
macOS CI runs Rust checks and builds.

## Failures

Each E2E uses a private tmux server, executable, configuration, and state, with
a fake provider compiled from `fixtures/provider.c`. Processes are cleaned up
on success or failure. Failed sandboxes remain at the printed `/tmp/am-test-*`
path with pane captures, logs, and state. CI uploads these diagnostics.

## Coverage

- **Rust behavior tests:** state transitions, configuration, rendering, scheduling.
- **Rust component tests:** real files, locks, Git, sockets, executable validation.
- **Python E2Es:** terminal input, discovery, tmux actions, persistence, recovery.

Reproduce bugs in the real application, then test at the lowest layer that
proves the behavior. Add E2Es for new boundaries or journeys, not every rule.
Wait for observable conditions; use fixed windows only for stability checks.
Do not hide failures with retries.
