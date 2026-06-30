# agent-mux

A TUI for multiplexing AI coding agent sessions in tmux.

Lists all active agent panes (Claude Code, Open Code, Gemini CLI, Codex CLI, Kimi CLI)
grouped by workspace, with a live preview panel showing each session's output.
Select a session and press enter to jump to it.

<p align="center">
  <img src="assets/demo.gif" alt="agent-mux demo" />
</p>

## Requirements

- Rust 1.85+
- tmux (must be run inside a tmux session)

## Setup

### Install

```
cargo install --path .
```

### Run from source

```
cargo run --release
```

Run the watcher directly:

```
cargo run --release -- watch
```

### Configure tmux

Add to your `~/.tmux.conf` to start the background watcher, keep its snapshot
fresh on tmux topology changes, and set up a key binding. This assumes
`agent-mux` resolves to the Rust binary in tmux's `PATH`.

```tmux
run-shell -b "agent-mux watch"
bind j run-shell "tmux neww agent-mux"

set-hook -g after-kill-pane 'run-shell -b "agent-mux refresh"'
set-hook -g window-unlinked 'run-shell -b "agent-mux refresh"'
set-hook -g session-closed 'run-shell -b "agent-mux refresh"'
set-hook -g after-new-window 'run-shell -b "agent-mux refresh"'
set-hook -g after-split-window 'run-shell -b "agent-mux refresh"'
```

The watcher owns the canonical pane snapshot used by the TUI. It polls session
statuses every 500ms, while the hooks trigger an immediate refresh when panes,
windows, or sessions are created or removed.

Reload tmux: `tmux source-file ~/.tmux.conf`

## Usage

From inside tmux:

```
agent-mux
```

Or use the key binding: `prefix + j`

### Keys

| Key              | Action               |
| ---------------- | -------------------- |
| `j` / `k`        | Navigate up/down     |
| `[count]j` / `k` | Move N sessions      |
| `gg`             | Go to first session  |
| `G`              | Go to last session   |
| `space`          | Toggle attention     |
| `s` / `u`        | Stash/unstash        |
| `enter`          | Switch to session    |
| `dd`             | Kill session         |
| `R`              | Reload watch process |
| `H` / `L`        | Resize sidebar       |
| `?`              | Toggle help          |
| `q` / `esc`      | Quit                 |

The sidebar separator can also be dragged with the mouse.
