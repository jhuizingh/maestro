---
description: Decompose a bead into child leaves mid-work — turn a task into a parent by creating children (labels inherited), optionally gate the parent on all children, and offer to start a child in its own worktree. Never re-homes the current worktree onto a different id.
argument-hint: "[parent-bead-id]"
allowed-tools: Bash(*), Read
---

## baton:split

Carve a bead into smaller units of work, each destined for its own worktree.

### Step 1 — Resolve context + the parent

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
```

`PARENT` = `$ARGUMENTS` if given; else the current branch's leaf id; else ask. `bd show "$PARENT"`.

### Step 2 — Define the children

Ask the user for the child tasks (titles, optionally descriptions). For each:

```bash
bd create "<child title>" --parent "$PARENT" --json    # inherits parent labels
```

If the children have an order, link them: `bd link <later> <earlier> --type blocks` so
`baton:start` warns when a prerequisite isn't done.

If `$PARENT` carries the `autonomous-safe` label, each child inherits it automatically (unless
you pass `--no-inherit-labels`) — flag this to the user, since it means every child worker will
run end-to-end (PR, merge, cleanup) without pausing for confirmation. If the parent isn't
autonomous-safe, ask per child (or once, for "all of these") whether any are low-impact/easy
enough to mark that way, and if so `bd label add <child> autonomous-safe`. Don't infer it from
the task text — only apply it on explicit confirmation.

### Step 3 — Optional: gate the parent

Offer to make the parent wait for all its children:
```bash
bd update "$PARENT" --waits-for-gate all-children
```
So the parent won't be considered done until every child closes.

### Step 4 — The no-re-home rule

Explain and honor: if you're currently in a worktree for a bead that just became a parent, that
worktree **keeps finishing its own bead** — do not rebind its branch to a child id. Each new
child gets its **own** worktree via `baton:start`. (For work you discovered rather than planned,
use `discovered-from` instead of `--parent`.)

### Step 5 — Offer to start a child

List the new children and offer to `baton:start <child>` on one (opening its own worktree),
or leave them in the backlog for later.
