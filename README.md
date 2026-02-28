# agent-mux

A TUI for multiplexing AI coding agent sessions in tmux.

Lists all active agent panes (Claude Code, Open Code, Gemini CLI, Codex CLI)
grouped by workspace,
with a live preview panel showing each session's output. Select a session and
press enter to jump to it.

## Requirements

- Go 1.25+
- tmux (must be run inside a tmux session)

## Install

```
go install
```

## Usage

From inside tmux:

```
agent-mux
```

### Background watcher

`agent-mux watch` runs a background daemon that polls session statuses every 2s
and writes them to disk. This means the TUI opens instantly with accurate
statuses instead of needing a few seconds to detect activity.

Add to your `~/.tmux.conf` to start the watcher automatically with tmux:

```tmux
run-shell -b "agent-mux watch"
```

### tmux binding

Add to your `~/.tmux.conf` to open agent-mux with `prefix + j`:

```tmux
bind j run-shell "tmux neww agent-mux"
```

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
