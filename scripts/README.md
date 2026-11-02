# Demo recording

Requires Python 3, Bun, ffmpeg, Git, tmux 3.6+, and a smelt checkout. Prepare
smelt once:

```sh
cd /path/to/smelt
cargo build --locked --bin smelt
cd docs/shots
bun install --frozen-lockfile
bunx playwright-core install chromium
bun src/main.ts wallpaper
```

From agent-mux:

```sh
export SMELT_REPO=/path/to/smelt
export SMELT_BIN="$SMELT_REPO/target/debug/smelt"
scripts/demo                                  # Write assets/demo.gif
scripts/demo --stills /tmp/demo                # Inspect without replacing GIF
scripts/demo --replay target/demo/replay.json  # Render saved recording
```

Eight real smelt sessions, two projects, offline scripted replies. Uses an
isolated tmux server and smelt's docs renderer; leaves user sessions and config
unchanged.
