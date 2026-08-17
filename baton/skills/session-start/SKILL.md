---
description: "Run the active context's startup_tasks, in order, every time — the routine that <name>-start and the home session invoke. Defaults are align (pull + plugin update + tracker sync), doctor (tool check), cleanup (worktree review), and status. Custom tasks run too. From a generic home session it can iterate all contexts."
allowed-tools: Bash(*), Read
---

## baton:session-start

### Step 1 — Resolve context(s)

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER" 2>/dev/null || true)"
```

- If a context resolved (normal `<name>-start` case, or cwd inside a member repo) → run its
  startup tasks (Step 2), scoped to that context only — never touch another context's repos or
  tracker here.
- If nothing resolved and you're at a generic home base (e.g. `~/work`) → iterate **all**
  registered contexts (`yq -r '.workspaces[]' "$REG"`), running each one's startup tasks and
  printing a per-context section. This is the one legitimate "all contexts" default in baton:
  it only fires when there's genuinely no active context to scope to (no cwd match, no
  `default:true`), not as a convenience default while a specific context IS resolvable. Compare
  `baton:worktrees`, which defaults to the active context and requires an explicit
  `--all-contexts` to widen — do not weaken that skill's scoping by analogy to this one.

Read the resolved context's `guidance.md` and honor it.

### Step 2 — Run startup_tasks in order

Read `startup_tasks` from the context and execute each, in order. Built-in task keywords:

- **`align`** — keep things current, failing soft (never let one pull break the rest):
  - For the workspace repo and the `maestro` plugin repo (if present locally): if on a clean
    `main` and behind `origin/main`, `git pull`.
  - For every other member repo (and any of its worktrees) that exists locally: if it's on
    `main` (or the repo's default branch), has no local changes (`git status --porcelain` empty),
    and is behind `origin/main`, `git pull --ff-only`. Skip anything on a feature branch, with
    uncommitted changes, or with no upstream — never touch those.
  - **Plugin freshness.** Run the update *unconditionally*, then report what happened — never
    decide whether an update is needed by comparing versions yourself, and never read a version
    out of a developer clone (`~/code/maestro`) or a listing of the plugin cache directory.
    Both readings produce confident false passes; see the header of `plugin-freshness.sh` and
    `jbh-a6w` for how each one failed in practice. (The `$HOME/code/maestro` fallback below only
    *locates the helper script*, the same way every other baton skill does — no version is ever
    read from it.)

    ```bash
    claude plugins marketplace update maestro 2>&1 | tail -3   # refresh the channel
    claude plugins update baton@maestro       2>&1 | tail -3   # unconditional — it knows
    PF="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/plugin-freshness.sh"
    [ -x "$PF" ] || PF="$HOME/code/maestro/baton/scripts/plugin-freshness.sh"
    F=""; [ -x "$PF" ] && { F="$("$PF" --format env)" || F=""; }   # capture first, as elsewhere
    eval "$F"   # PLUGIN_STATE PLUGIN_ACTION PLUGIN_COMMAND RUNNING_VERSION INSTALLED_VERSION
                # LATEST_VERSION MARKETPLACE_BRANCH PLUGIN_WARNINGS
    [ -n "${PLUGIN_STATE:-}" ] || PLUGIN_STATE=unknown   # plugin cache too old to have the helper
    ```

    Report on `$PLUGIN_STATE`, and print every line of `$PLUGIN_WARNINGS` regardless of it —
    warnings are independent of the state, so a `current` plugin can still have something worth
    saying (a marketplace clone parked on a feature branch, for instance):
    - `current` — one line, no action.
    - `restart-needed` — the update landed; say the session is still running `$RUNNING_VERSION`
      while `$INSTALLED_VERSION` is installed, and that a restart picks it up. This is the case
      a two-value check cannot see at all.
    - `update-available` — the update ran a moment ago and *did not take*. Surface it as a
      problem with the update, quoting both versions; don't just re-run the command.
    - `not-installed` / `unknown` — say which value was missing and why (`$PLUGIN_WARNINGS`
      names it). Never round an unknown down to "up to date".

    Fail soft, like the rest of `align`: if the helper is missing (an old plugin cache) or the
    update command errors, say so and carry on with the remaining tasks.
  - Sync the context's tracker, best-effort: `git -C <tracker-repo> pull` for git-tracked files
    (e.g. `interactions.jsonl`), **and** `BEADS_DIR=<tracker-repo>/.beads bd dolt pull` for actual
    issue state. These are independent syncs — issue data lives in Dolt's own `refs/dolt/data`
    ref, not in the git-tracked files, so a clean `git pull` can succeed while `bd show`/`bd ready`
    still return stale (e.g. already-closed-elsewhere) status. Do the same `bd dolt pull` for any
    repo-local self-hosted tracker (any member repo carrying its own `.beads/` directory)
    before trusting its status during this run.
    Before the first `bd dolt pull` of the session against a given tracker, confirm its remote
    with `BEADS_DIR=<tracker-repo>/.beads bd dolt remote list` — don't trust a comment in
    `config.yaml`, which can silently disagree with the actual configured value (see
    `baton:beads`). If it's missing or points somewhere unexpected, skip the pull, warn instead of
    failing the rest of `align`, and surface it in the status summary.
- **`doctor`** — invoke `baton:doctor` (tool check; offers fixes).
- **`cleanup`** — invoke `baton:cleanup-worktrees` (review mode; asks before removing anything).
- **`status`** — print a short status: in-progress beads (`bd list --status in_progress`), ready
  work (`bd ready`), open PRs awaiting your review (`gh pr list` across member repos), and a
  one-line suggestion for what to pick up next.
- **Any other entry** — if it looks like a shell command, run it (echo it first); otherwise treat
  it as a natural-language instruction and carry it out.

Set `BEADS_DIR` to the context's `task_tracking.dir` before any `bd` calls.

### Step 3 — Wrap up

End with the `status` summary (if not already last) and a suggested next action (`baton:start`,
resume an in-progress worktree, or review a PR). Keep the whole thing concise — this runs on
every start.
