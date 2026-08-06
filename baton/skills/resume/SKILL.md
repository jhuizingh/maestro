---
description: (Worker session) Pick up the task for the current git worktree — derive the leaf bead id from the branch name, load it from the active context's tracker, and start implementing. This is what a handed-off session runs to orient itself. No-ops cleanly outside a baton worktree.
allowed-tools: Bash(*), Read
---

## baton:resume

### Step 1 — Am I in a baton worktree?

Recompute the identity group from the branch, using the same helper `baton:start` minted it with:

```bash
BR="$(git rev-parse --abbrev-ref HEAD 2>/dev/null)" || { echo "Not a git repo — nothing to resume."; exit 0; }
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
ID="$("$IDENT" --branch "$BR" --format env)" || { echo "Branch '$BR' isn't a baton branch — nothing to resume."; exit 0; }
eval "$ID"          # capture first: `eval "$(cmd)"` would swallow cmd's exit status
echo "Branch: $BR  → leaf: $LEAF  session: $SESSION_NAME"
```

Beads ids look like `<prefix>-<hash>` (e.g. `personal-a3f2`), so a baton branch is
`<leaf-id>-<slug>` (the id may contain a dot for a dotted child bead). If the helper rejects the
branch, this isn't a baton worktree — say so and stop (no error).

### Step 2 — Resolve context + tracker

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
```

### Step 3 — Load the bead

```bash
bd show "$LEAF" --json
```
If not found, tell the user the branch doesn't map to a known bead and ask how to proceed (they
may want `baton:start` or a different id). Read `$GUIDE` and honor it.

### Step 4 — Fire worker.on_resume hooks

Run each action in `hooks.worker.on_resume` (from `$CTX`) here in the worktree. The identity
group (`$LEAF`, `$SLUG`, `$BR`, `$SESSION_NAME`, `$SESSION_TITLE`) is already exported from
Step 1 and is available to them.

### Step 5 — Orient + begin

Print a short summary (bead title, description, acceptance criteria, current status) and start
working through the acceptance criteria, beginning with the first unmet one. If the bead is
already claimed by someone else or closed, flag it and confirm before continuing.

Check `$LEAF`'s labels (already in the `bd show --json` output from Step 3) for
`autonomous-safe`. If present, say so and note the implication for the rest of this session: once
the acceptance criteria are met, run `baton:pr` and then `baton:finish` straight through —
those skills' autonomous paths will create the PR, wait for checks, merge, and signal cleanup
without pausing for confirmation. This does **not** relax hard gates that apply regardless of the
flag: failing `pre_pr`/`pre_finish` hooks and red CI still stop the flow for a human to look at.
If the label is absent, work proceeds as normal — human confirms at PR/merge/cleanup.

When the work is done and merged, run **`baton:finish`**.
