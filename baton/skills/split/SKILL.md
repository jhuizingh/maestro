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
TRK="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/tracker.sh"
[ -x "$TRK" ] || TRK="$HOME/code/maestro/baton/scripts/tracker.sh"
```

`tracker.sh` is the one seam to the task tracker — `task_tracking.type` picks the backend behind
it. Never call `bd` here; the verb set is documented in `../../references/tracker.md`.

`PARENT` = `$ARGUMENTS` if given; else this worktree's leaf id (`task-identity.sh --worktree
"$PWD"`, as `baton:resume` does — never parsed out of the branch name); else ask.
`"$TRK" get "$PARENT"`.

### Step 2 — Define the children

Ask the user for the child tasks (titles, optionally descriptions). For each:

```bash
CHILD="$("$TRK" create "<child title>" --parent "$PARENT")"   # inherits parent labels
```

If the children have an order, link them: `"$TRK" link <later> <earlier> --type blocks` so
`baton:start` warns when a prerequisite isn't done.

If `$PARENT` carries the `autonomous-safe` label, each child inherits it automatically (unless
you pass `--no-inherit-labels`) — flag this to the user, since it means every child worker will
run end-to-end (PR, merge, cleanup) without pausing for confirmation. If the parent isn't
autonomous-safe, ask per child (or once, for "all of these") whether any are low-impact/easy
enough to mark that way, and if so `"$TRK" label-add <child> autonomous-safe`. Don't infer it
from the task text — only apply it on explicit confirmation.

### Step 3 — The parent is gated by its children already

Don't try to set an explicit "wait for all children" flag on the parent. Earlier versions of this
skill prescribed `bd update --waits-for-gate all-children`; **no such flag exists** (checked
against bd 1.1.0), so that line never did anything but fail. The verb set has no equivalent
either, on purpose — see `../../references/tracker.md`.

What actually holds, and is enough:

- The parent/child edge is the structure. `baton:start` refuses to create a worktree for a parent
  that still has open children — it drops into the child picker instead — so a parent cannot be
  worked as if it were a leaf.
- `"$TRK" deps <child>` reports the `parent-child` edge, and `"$TRK" children <parent>` reports
  the other direction, so any skill that needs the relationship can read it.
- Closing the parent once its children are done stays a decision someone makes. `baton:finish`
  already notes when a leaf was its parent's last open child.

Say this to the user if they ask for a gate, rather than silently doing nothing.

### Step 4 — The no-re-home rule

Explain and honor: if you're currently in a worktree for a bead that just became a parent, that
worktree **keeps finishing its own bead** — do not rebind its branch, or rewrite its identity
carrier, to a child id. Each new
child gets its **own** worktree via `baton:start`. (For work you discovered rather than planned,
use `discovered-from` instead of `--parent`.)

### Step 5 — Offer to start a child

List the new children and offer to `baton:start <child>` on one (opening its own worktree),
or leave them in the backlog for later.
