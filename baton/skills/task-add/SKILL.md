---
description: Quick-capture a task into the active context's tracker. Optionally make it a child of a parent (to pre-decompose a ticket into per-worktree subtasks), add labels, or mark it autonomous-safe. Thin wrapper over beads, context-resolved so it hits the right database.
argument-hint: "<task text> [--parent <id>] [--label <l>] [--autonomous-safe]"
allowed-tools: Bash(*)
---

## baton:task-add

### Step 1 — Resolve context

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
echo "$CTX" | jq -r '"Adding to context: \(.name)"'
```

### Step 2 — Create the bead

- Simple capture: `bd q "<task text>"` → prints the new id.
- With a parent (a child leaf under a parent): `bd create "<task text>" --parent <id> --json`.
- With labels: add `--labels <a,b>`; a `repo-<name>` label routes it to that member repo when
  `baton:start` runs.
- `--autonomous-safe` is shorthand for `--labels autonomous-safe` (merged with any other
  `--label`s given). Only offer/use it when the user says the task is low-impact and easy
  enough that a worker session can go all the way through implementation, PR, merge, and
  `baton:finish` cleanup without waiting on human review at any of those gates — don't infer
  it from task text alone. `baton:start` surfaces the label when dispatching, and
  `baton:resume`/`baton:pr`/`baton:finish` honor it; see those skills for what it actually
  changes.

Echo the resulting id and title (and, if applicable, that it's marked autonomous-safe). If the
user gave multiple tasks, create each. Confirm and, if they want, offer `baton:start <id>` to
begin one now.
