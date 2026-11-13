# fermata

A terminal dashboard for one-orchestrator, many-workers Claude Code work: every in-flight task,
what is waiting on you, and a place to answer it. See `DESIGN.md`.

## Build and run

```bash
cargo build --release
target/release/fermata              # TUI
target/release/fermata --dry-run    # TUI; input is logged, never sent or noted
target/release/fermata --dump       # joined task list as JSON
target/release/fermata --dump --task ctx-123   # one task's detail + timeline as JSON
```

Requires baton (located like baton's own scripts do), `jq`/`yq`, `gh`, and `tmux` on PATH.

## Install

fermata is a standalone binary, not a Claude Code plugin, so installing the baton plugin does not
install it. On a new machine:

1. **Rust 1.91 or newer** (its dependencies need it; `Cargo.toml` declares it): `brew install rust`, or
   [rustup](https://rustup.rs).
2. **Build and install** from the repo, with no clone needed:

   ```bash
   cargo install --locked --git https://github.com/<you>/maestro fermata
   ```

   From a clone, `cargo install --locked --path fermata` does the same. `--locked` builds
   against the committed `Cargo.lock`. Either way the binary lands in `~/.cargo/bin/fermata`.
3. **Put `~/.cargo/bin` on your `PATH`** if it is not already, e.g. in `~/.zshrc`:
   `export PATH="$HOME/.cargo/bin:$PATH"`.
4. **Runtime dependencies**, which fermata calls but does not bundle:
   - **baton**: the plugin from this marketplace (see the top-level README's Install section),
     with at least one context configured. fermata finds it the same way baton's own scripts do.
   - `jq`, `yq`, `gh` (logged in) and `tmux` on `PATH`.
   - **ccmon** for the session views: `FERMATA_CCMON_DIR`, default `~/code/claude-monitor`.
     Without it, tasks, PRs and history still work.
5. **Check it:** `fermata --dump` prints the joined task list as JSON.

**Updating.** Re-run the install command with `--force` (or `git pull` and re-run the `--path`
form). Updating the baton plugin does not rebuild fermata, so update both when a release changes
the branch registry: fermata reads its fields directly, and an older baton simply leaves the
newer ones out.

## Keys

| Where | Keys |
|---|---|
| Everywhere | `t` tasks · `d` decisions · `s` sessions · `?` help · `R` full reload · `m` mouse capture (off by default) · `Esc`/`q` back · `q` quit (top views) |
| Tasks | `j`/`k` move · `Enter` detail · `o` ticket · `e` epic · `p` PR · `c` CI run · `w` tmux window in worktree · `:`/`i` input · `*` pin ticket · `+` pin epic · `r` reload |
| Detail | same open keys · `a`/`:` answer · `1`-`9` pre-fill option · `j`/`k` timeline · `Enter` open item · `r` refresh |
| Decisions | `j`/`k` · `1`-`9` pre-fill option · `a` answer · `Enter` detail |
| Sessions | `Enter` jump · `r` quick reply · `e` describe · `x` close (asks: `y` kill claude + pane, `k` claude only) · `X` closed list (`Enter` reopens, asks first) · `S` save snapshot · `L` restore snapshot (asks first) · `l` ccmon log · `v` transcript · `o` the session's task |
| Tasks, Decisions, Sessions | `/` filter as you type (k9s-style) · `Enter` lock it · `Esc` (typing or locked) back to the whole list |
| Viewer | `j`/`k` · `Space`/`b` page · `g`/`G` · `Enter` expand tool call · `/` search · `n`/`N` |

Input goes to the task's live session pane, else to the orchestrator pane prefixed
`[fermata <id>]`, and is always recorded as an `ANSWER (fermata, …)` tracker note. A busy
(`working`) pane asks for confirmation first, unless it runs Claude Code, which queues input.
Panes are found by the session's claude process, not by the address ccmon recorded, which goes
stale as panes move. See "Pane resolution" in `DESIGN.md`.

Environment: `FERMATA_CCMON_DIR` (default `~/code/claude-monitor`), `FERMATA_CACHE_DIR`,
`FERMATA_LOG`.

## Adapters

A context opts into ticket and progress panels with `<workspace>/fermata.yaml` (schema in
`DESIGN.md`). Without one, tasks, PRs, sessions and history still work.
