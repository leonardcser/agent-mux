# agent-mux

A TUI for multiplexing AI coding agent sessions in tmux.

Lists all active agent panes (Claude Code, Open Code) grouped by workspace,
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
| `enter`          | Switch to session    |
| `dd`             | Kill session         |
| `q` / `esc`      | Quit                 |
