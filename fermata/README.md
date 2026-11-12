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

```bash
cargo install --path .   # installs ~/.cargo/bin/fermata
```

Make sure `~/.cargo/bin` is on your `PATH`. Re-run the command after pulling changes.

## Keys

| Where | Keys |
|---|---|
| Everywhere | `t` tasks · `d` decisions · `s` sessions · `?` help · `R` full reload · `m` mouse capture (off by default) · `Esc`/`q` back · `q` quit (top views) |
| Tasks | `j`/`k` move · `Enter` detail · `o` ticket · `e` epic · `p` PR · `c` CI run · `w` tmux window in worktree · `:`/`i` input · `*` pin ticket · `+` pin epic · `r` reload |
| Detail | same open keys · `a`/`:` answer · `1`-`9` pre-fill option · `j`/`k` timeline · `Enter` open item · `r` refresh |
| Decisions | `j`/`k` · `1`-`9` pre-fill option · `a` answer · `Enter` detail |
| Sessions | `Enter` jump · `r` quick reply · `e` describe · `x` close (asks: `y` kill claude + pane, `k` claude only) · `X` closed list (`Enter` reopens, asks first) · `S` save snapshot · `L` restore snapshot (asks first) · `l` ccmon log · `v` transcript · `o` the session's task |
| Viewer | `j`/`k` · `Space`/`b` page · `g`/`G` · `Enter` expand tool call · `/` search · `n`/`N` |

Input goes to the task's live session pane, else to the orchestrator pane prefixed
`[fermata <id>]`, and is always recorded as an `ANSWER (fermata, …)` tracker note. A `working`
pane asks for confirmation first.

Environment: `FERMATA_CCMON_DIR` (default `~/code/claude-monitor`), `FERMATA_CACHE_DIR`,
`FERMATA_LOG`.

## Adapters

A context opts into ticket and progress panels with `<workspace>/fermata.yaml` (schema in
`DESIGN.md`). Without one, tasks, PRs, sessions and history still work.
