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
| `a` / `A`                 | New agent in project / in a worktree |
| `t` / `T`                 | Terminal of project / of a worktree |
| `r`                       | Rerun last command in the terminal |
| `space` / `R`             | Toggle read/unread / mark all read |
| `dd`                      | Kill session                       |
| `o`                       | Toggle recent/creation order       |
| `H` / `L` or drag divider | Resize sidebar                     |
| `?`                       | Full key help                      |
| `q` / `Esc`               | Quit                               |

### Sidebar

Sessions are grouped by project. Busy sessions appear under **Working**.
Sessions that wait for input leave **Working** and show an outline circle:
violet until you read them, then orange. In `manual` mode, park them back with
Ctrl+Enter or `w`. Use `s` to move sessions to **Stashed**. Click a section title to expand or
collapse it. Sessions that finish in the background become unread until opened.

### Projects

Every git project that has run a session stays in the sidebar after its last
session ends, with its branch, a `*` for uncommitted changes, and `↑n`/`↓n` for
commits to push or pull. Select it to see details and available actions. Press
`a` to open an agent there, or `dd` to forget it. A new session in the project
lists it again.

### Actions

Every key besides the built-in navigation keys is an action. Actions run in the
selected session's directory, or in the project root for a project without
sessions. This works with any agent. These actions exist without any config:

| Key | Action                                                   |
| --- | -------------------------------------------------------- |
| `a` | New window that starts `new_window_command` (default: a shell) |
| `A` | The same, in a worktree you pick                         |
| `t` | Go to the project's terminal window                      |
| `T` | The same, in a worktree you pick                         |
| `r` | Rerun the last command in the terminal, without leaving  |

`Enter` on a project row runs the action bound to `a`. `a` and `t` work on the
project's main checkout, also when the selected session runs in a worktree.

Add your own, or rebind a default by using its key:

```toml
[actions.P]
cmd = "git push"
mode = "run"            # popup (default) | window | run | agent | terminal
description = "push"

[actions.v]
cmd = "lazygit"         # popup: modal tmux popup, closes when the command exits
pick = "worktree"       # ask which worktree to run in

[actions.m]
mode = "agent"
cmd = "claude"          # start this agent instead of new_window_command
```

#### Worktrees

`pick = "worktree"` works with every mode. It lists the project's git
worktrees, with the current one selected. Type to filter by branch or path
(fuzzy, best match first) and press `Enter`. Move with Up/Down, `Ctrl-n`/`Ctrl-p` or `Ctrl-j`/`Ctrl-k`.
`Esc` clears the query, then closes the list. The chosen worktree becomes
`{path}` and `{branch}`. A project with only one checkout skips the list.

#### Project terminal

`mode = "terminal"` uses one reusable shell window per checkout, for commands
you rerun often such as a build. The main checkout and each worktree get a
window of their own. The window is created when missing (and the session too,
for a project without one) and is never replaced, so its history and
scrollback stay.

```toml
[actions.b]
mode = "terminal"
send = "cargo build"         # type and run this
pick = "worktree"

[actions.R]
mode = "terminal"
send = "last"                # recall and run the previous command
focus = false                # stay in the list
```

- `send = "last"` presses Up and Enter. Any other `send` text is typed and
  run, and may use `{path}`, `{root}` and `{branch}`.
- `tag` (default `terminal`) marks the window, so it is found whatever it is
  called; use different tags for more than one terminal per checkout. The
  window keeps tmux's own name (`zsh` while idle, the command while one runs).
  A new terminal is created as the first window of the session and shifts the
  others right, so windows added with `a` always follow it at the end. An idle
  single-shell window that already sits in the checkout's directory is adopted
  where it is.
- A terminal that runs a program other than a shell is left alone and the list
  shows an error.
- `mode = "window"`, `mode = "agent"` and a focusing terminal action close the
  list after they switch windows.

`{path}`, `{root}` and `{branch}` expand to shell-quoted values: the selected
directory, its project root and its branch. Keys are a single character or
`ctrl-<character>`; keys the sidebar already uses are rejected.

## Configuration

Optional file: `~/.config/agent-mux/config.toml` (or
`$XDG_CONFIG_HOME/agent-mux/config.toml` if set).

Defaults:

```toml
new_window_command = "" # shell | smelt | opencode | codex | claude

# [actions.<key>]
# cmd = "..."           # required, except for agent and terminal
# mode = "popup"        # popup | window | run | agent | terminal
# pick = "worktree"     # ask which worktree to run in
# description = "..."   # shown in the project details

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
