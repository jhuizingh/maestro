---
description: Start work on a task in the active context — resolve a single leaf bead (pick a child of a parent, use a leaf directly, or create one), create its git worktree and branch, and hand off per the context's work mode (new tmux session by default). One worktree ⇔ one leaf bead.
argument-hint: "[bead-id | free-text task]"
allowed-tools: Bash(*), Read
---

## baton:start

### Step 1 — Resolve context + guidance

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
echo "$CTX" | jq -r '"Context: \(.name)  tracker: \(.task_tracking.dir)  mode: \(.work_mode.default)"'
```

Read `$GUIDE` if it exists and honor its preferences for the rest of this skill.

### Step 2 — Resolve the LEAF bead (invariant: one worktree ⇔ one leaf)

Determine `LEAF` (a leaf bead id):

- **`$ARGUMENTS` is a bead id** → fetch it: `bd show <id> --json`.
  - If it has open children (`bd children <id>`), it's a **parent** — list the open children and
    ask which to work, or offer to create a new child (Step 3). Do **not** worktree a parent.
  - If it's a **leaf** (no open children) → `LEAF=<id>`.
- **`$ARGUMENTS` is free text** → create a leaf: `bd q "<text>"` (capture the id).
- **`$ARGUMENTS` empty** → show ready work (`bd ready` or `bd list --status open`), let the user
  pick a leaf; if they pick a parent, drop into the child picker; if they want something ad-hoc,
  create a stub (`bd q "<text>"`). Every worktree gets a unique leaf — never proceed without one.

Confirm: "Starting `<LEAF>` — <title>." If `bd show <LEAF> --json` carries the `autonomous-safe`
label, say so explicitly — e.g. "marked autonomous-safe: this worker will go through PR,
merge, and `baton:finish` cleanup without pausing for confirmation at those gates" — since
that's a meaningfully different hand-off than the default human-gated flow.

### Step 3 — (When decomposing) create a child leaf

If the user is carving a chunk off a parent:
```bash
bd create "<child title>" --parent <PARENT> --json   # inherits parent labels
```
Use the new child id as `LEAF`. (For deeper splitting mid-work, see `baton:split`.)

### Step 4 — Check dependencies

If `LEAF` has an unmet `blocked-by` dependency (an open blocker), warn the user and confirm
before proceeding.

### Step 5 — Compute branch + target repo

- Slugify the title: lowercase → strip non-alphanumeric/space → spaces to `-` → truncate to 40
  chars → trim trailing `-`. **Branch = `<LEAF>-<slug>`** (load-bearing: `baton:resume` and
  `baton:cleanup-worktrees` parse this).
- Target repo: if the bead carries a `repo-<name>` label, map `<name>` to `<code_root>/<name>`;
  otherwise ask which member repo this work belongs to (offer the context's `member_repos`).
- Worktree base: expand `worktree_base` (`{code_root}` and `{repo}` substituted), e.g.
  `~/code/<repo>-worktrees`.

### Step 6 — Claim + create the worktree

```bash
bd update <LEAF> --claim                      # assign to me + in_progress
REPO=<code_root>/<repo>; WT_BASE=<expanded worktree_base>; BR=<LEAF>-<slug>
git -C "$REPO" fetch --all --prune
mkdir -p "$WT_BASE"
git -C "$REPO" worktree add "$WT_BASE/$BR" -b "$BR" origin/main    # or without -b if branch exists
```
If the repo has a `package.json` (or other obvious deps), install them in the worktree.

### Step 7 — Fire home.on_dispatch hooks

Run each action in `hooks.home.on_dispatch` (from `$CTX`) — shell command or natural-language
step — in the home/orchestrator session (cwd = workspace).

### Step 8 — Hand off per work_mode

- **`worktree-new-session`** (default): open a fresh session in the worktree. **Write no file** —
  the worker discovers its bead from the branch.

  Read `handoff.launcher` from `$CTX`. **If set**, it's a command taking the worktree path:
  ```bash
  "$LAUNCHER" "$WT_BASE/$BR"
  ```
  **If unset (the default)**, use plain tmux — no tmuxinator or wrapper required:
  ```bash
  SESSION="baton-${BR//[^A-Za-z0-9_-]/_}"   # tmux treats . and : as target syntax; sanitize
  CLAUDE_ARGS=""                            # add --dangerously-skip-permissions per handoff.dangerous
  tmux new-session -d -s "$SESSION" -c "$WT_BASE/$BR" "claude $CLAUDE_ARGS"
  if [ -n "$TMUX" ]; then tmux switch-client -t "$SESSION"; else tmux attach -t "$SESSION"; fi
  ```
  Use that exact `SESSION` formula — `hooks.home.on_cleanup` recomputes it from `$BR` to tear the
  session down, so the two must agree. If `tmux` isn't available, say so and fall back to
  `worktree-same-session` rather than failing the dispatch.

  The new session runs `baton:resume` (via the SessionStart hook / launcher prompt).
- **`worktree-same-session`**: `cd "$WT_BASE/$BR"` and continue here; then run `baton:resume`
  yourself to orient and begin.
- **`in-place`**: skip the worktree entirely (Step 6 already ran? — for in-place, do NOT create a
  worktree; just work on a branch in the member repo). Orient and begin.

### Step 9 — Orient

Print a short summary: bead id + title, repo, branch, worktree path, whether it's
autonomous-safe, and the first acceptance criterion / next action. Then begin (or, for
new-session handoff, tell the user the worker session is opening).
