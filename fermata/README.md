# fermata

A terminal dashboard for one-orchestrator, many-workers Claude Code work: every in-flight task,
what is waiting on you, and a place to answer it. See `DESIGN.md`.

## Build and run

```bash
cargo build --release
target/release/fermata              # TUI
target/release/fermata --dry-run    # TUI; input is logged, never sent or noted
target/release/fermata --context work   # TUI, showing only the context named "work"
target/release/fermata --dump       # joined task list as JSON
target/release/fermata --dump --task ctx-123   # one task's detail + timeline as JSON
target/release/fermata --doctor     # load once, report each source's health; exit 1 if degraded
target/release/fermata --version    # version, git commit and build time
target/release/fermata goto         # jump to the running fermata, or start it in tmux session "fermata"
target/release/fermata cleanup      # end records whose claude process is gone (--dry-run to preview)
target/release/fermata snapshot save|restore|list   # save running sessions; relaunch them later
```

For a tmux key that jumps to fermata: `bind-key C-g run-shell 'fermata goto'` (use the full path
if tmux's `run-shell` does not have `~/.cargo/bin` on its `PATH`).

Requires baton (located like baton's own scripts do), `jq`/`yq`, `gh`, and `tmux` on PATH.

## Install

fermata is a Rust binary plus a small Claude Code plugin (also named `fermata`) whose hooks call
that binary to keep a record of every Claude session. Installing the baton plugin installs
neither. On a new machine:

1. **Rust 1.91 or newer** (its dependencies need it; `Cargo.toml` declares it): `brew install rust`, or
   [rustup](https://rustup.rs).
2. **Build and install** from the repo, with no clone needed:

   ```bash
   cargo install --locked --git https://github.com/jhuizingh/maestro fermata
   ```

   That repo is the one canonical source. From a clone of it, `cargo install --locked --path
   fermata` does the same. `--locked` builds
   against the committed `Cargo.lock`. Either way the binary lands in `~/.cargo/bin/fermata`.
3. **Put `~/.cargo/bin` on your `PATH`** if it is not already, e.g. in `~/.zshrc`:
   `export PATH="$HOME/.cargo/bin:$PATH"`.
4. **Runtime dependencies**, which fermata calls but does not bundle:
   - **baton**: the plugin from this marketplace (see the top-level README's Install section),
     with at least one context configured. fermata finds it the same way baton's own scripts do.
   - `jq`, `yq`, `gh` (logged in) and `tmux` on `PATH`.
5. **Install the plugin** for the session views, after the binary (the hooks need a binary that
   has `fermata hook`):

   ```bash
   claude plugins install fermata@maestro
   ```

   Or in Claude Code: `/plugin install fermata --marketplace <owner>/<repo>`. Its hooks write one
   record per session under `FERMATA_DATA_DIR`; see "Session records and hooks" in `DESIGN.md`.
   Without it, tasks, PRs and history still work. If you ran ccmon's Python hooks before, remove
   them from `~/.claude/settings.json` when you install it, or both will write records. The two
   changes reach running sessions differently: hooks removed from `settings.json` stop at once,
   while a newly installed plugin's hooks load only when a session starts. Sessions already
   running when you switch go untracked until they restart.
6. **Check it:** `fermata --version` names the commit it was built from, `fermata --doctor`
   reports each source fermata reads (tracker, ticket adapter, review rank, `gh`, sessions, tmux)
   with its last success, last error and data age, and `fermata --dump-sessions` prints the
   session records once a Claude session has started with the plugin on.

**Updating.** Re-run the install command with `--force` (or `git pull` and re-run the `--path`
form), and `claude plugins update fermata@maestro`. Updating a plugin does not rebuild fermata,
so update both when a release changes the branch registry: fermata reads its fields directly,
and an older baton simply leaves the newer ones out. The mine/everyone filter needs baton 0.19.0 or newer (its `whoami` verb); with
an older baton, fermata cannot tell who you are and shows everyone, saying so in the header.

## Keys

| Where | Keys |
|---|---|
| Everywhere | `t` tasks · `d` decisions · `s` sessions · `H` health (also when the header shows `⚠ N sources degraded`) · `?` help · `R` full reload · `m` mouse capture (off by default) · `Esc`/`q` back · `q` quit (top views) |
| Tasks, Detail, Decisions, Sessions | `M` all contexts ↔ one context · `C` pick the context · `[`/`]` previous/next context |
| Context picker | `j`/`k` · `Enter` show only that context · `Esc` back |
| Tasks, Detail | `u` mine ↔ everyone. Mine (the default) is tasks assigned to you, unassigned ones, and anything waiting on you; the header says how many it hides |
| Tasks | `j`/`k` move · `Enter` detail · `o` ticket · `e` epic · `p` PR · `c` CI run · `w` go to the task's session (else a new window) · `W` new tmux window in worktree · `:`/`i` input · `*` pin ticket · `+` pin epic · `r` reload |
| Detail | same open keys · `a`/`:` answer · `1`-`9` pre-fill option · `j`/`k` timeline · `J`/`K` scroll the highlighted item's full text · `Enter` open item · `r` refresh |
| Detail, 140+ columns | the list stays as a left column: `j`/`k` move it and the detail follows · `Tab`/`Enter` focus the timeline · `Tab` back to the list |
| Decisions | `j`/`k` · `1`-`9` pre-fill option · `a` answer · `Enter` detail |
| Sessions | `Enter` jump · `0`–`9`, `M-a`…`M-z` jump to the row with that key (`M-` needs Option sent as Meta) · `r` quick reply · `e` describe · `x` close (asks: `y` kill claude + pane, `k` claude only) · `X` closed list (`Enter` reopens, asks first) · `S` save snapshot · `L` restore snapshot (asks first) · `l` session log · `v` transcript · `o` the session's task |
| Tasks, Decisions, Sessions | `/` filter as you type (k9s-style) · `Enter` lock it · `Esc` (typing or locked) back to the whole list |
| Viewer | `j`/`k` · `Space`/`b` page · `g`/`G` · `Enter` expand tool call · `/` search · `n`/`N` |

Input goes to the task's live session pane, else to the orchestrator pane prefixed
`[fermata <id>]`, and is always recorded as an `ANSWER (fermata, …)` tracker note. That note
takes an answered decision out of the Decisions view until a new `DECISION` is posed. A busy
(`working`) pane asks for confirmation first, unless it runs Claude Code, which queues input.
Panes are found by the session's claude process, not by the recorded tmux address, which goes
stale as panes move. See "Pane resolution" in `DESIGN.md`.

Environment: `FERMATA_DATA_DIR` (session records; default `fermata` under the OS data dir,
`~/Library/Application Support/fermata` on macOS), `FERMATA_CCMON_DIR` (ccmon's older records,
still read; default `~/code/claude-monitor`), `FERMATA_BIN` (the binary the plugin's hooks run;
default `fermata` on `PATH`, then `~/.cargo/bin/fermata`), `FERMATA_SESSION` (the tmux session
`fermata goto` starts fermata in; default `fermata`), `FERMATA_RELAUNCH` (how reopen and snapshot
restore start a session whose tmux session is gone: a command line with `{session}`, `{worktree}`
and `{session_id}`, run through `sh -c`; default a new detached tmux session running
`claude --resume`), `FERMATA_CACHE_DIR`, `FERMATA_LOG`.

While it runs, the TUI ends every minute the records of sessions whose claude process is gone
(crashed or killed sessions never fire the session-end hook), archiving named ones to the closed
list. `fermata cleanup` does the same once.

## Contexts: one or all

Every list shows either **all contexts**, each in its own block under a `── <context> ──`
heading (never interleaved), or **one context** only. `M` switches between the two, `C` picks
the context, and the header always says which you are looking at. In one-context mode the header
adds `+N elsewhere` when tasks in other contexts need you. See "Information architecture" in
`DESIGN.md`.

## Adapters

A context opts into ticket and progress panels with `<workspace>/fermata.yaml` (schema in
`DESIGN.md`). Without one, tasks, PRs, sessions and history still work.
`examples/ticket-info.sh` is a portable starting point for the `info` command: it shows how to
report "not found" versus "source unavailable" and how to stay inside fermata's time budget.
