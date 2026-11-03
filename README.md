# agent-mux

Browse AI coding agents in tmux, preview their output, and jump between
sessions. Supports Smelt, Claude Code, Open Code, Gemini CLI, Codex CLI, and
Kimi CLI.

<p align="center">
  <img src="assets/demo.gif" alt="agent-mux demo" />
</p>

## Install

Requires Rust 1.88+ and tmux on Linux or macOS. On Windows, use WSL.

```sh
cargo install --git https://github.com/leonardcser/agent-mux --locked
```

Or, from a local checkout:

```sh
cargo install --path . --locked
```

Rerun the same command to upgrade.

## Use

Run inside tmux:

```sh
agent-mux
```

Use Up/Down to preview a session, then click or press Enter to open it.

Optional shortcut in `~/.tmux.conf`:

```tmux
bind j new-window agent-mux
```

Reload with `tmux source-file ~/.tmux.conf`, then use `prefix + j`.

### Keys

| Key                       | Action                             |
| ------------------------- | ---------------------------------- |
| `j` / `k` or Up/Down      | Move down/up                       |
| `h` / Left                | Collapse current section           |
| `gg` / `G`                | First/last session                 |
| `Enter`                   | Open session                       |
| `s` / `u`                 | Stash/unstash                      |
| `Ctrl+Enter` / `w`        | Park/unpark in manual mode         |
| `a`                       | Add window in selected project     |
| `space` / `A`             | Toggle read/unread / mark all read |
| `dd`                      | Kill session                       |
| `o`                       | Toggle recent/creation order       |
| `H` / `L` or drag divider | Resize sidebar                     |
| `?`                       | Full key help                      |
| `q` / `Esc`               | Quit                               |

### Sidebar

Sessions are grouped by project. Busy sessions appear under **Working**.
Sessions that wait for input show an orange outline circle and leave
**Working**; in `manual` mode, park them back with Ctrl+Enter or `w`. Use `s` to move sessions to **Stashed**. Click a section title to expand or
collapse it. Sessions that finish in the background become unread until opened.

## Configuration

Optional file: `~/.config/agent-mux/config.toml` (or
`$XDG_CONFIG_HOME/agent-mux/config.toml` if set).

Defaults:

```toml
new_window_command = "" # shell | smelt | opencode | codex | claude

[sidebar]
sort = "recent"              # recent | creation; unset: saved choice
select = "unread"            # unread | last | first
working = "manual"           # manual | section | inline | inline-last
working_section = "collapse" # collapse | expand
stashed_section = "collapse" # collapse | expand
```

`working` modes:

- `manual`: use Ctrl+Enter or `w` to park busy or waiting sessions in Working.
- `section`: move busy sessions to Working automatically.
- `inline`: keep busy sessions in place.
- `inline-last`: show busy sessions last in each project.

Reopen agent-mux to apply changes.

## Development

```sh
cargo run --release
scripts/check
```

See [Testing](tests/README.md) for setup, full-suite checks, and focused runs.

See [Demo recording](scripts/README.md) to update the GIF.
