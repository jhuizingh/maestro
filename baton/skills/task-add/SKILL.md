---
description: Quick-capture a task into the active context's tracker. Optionally make it a child of a parent (to pre-decompose a ticket into per-worktree subtasks), add labels, or mark it autonomous-safe. Thin wrapper over the tracker seam, context-resolved so it hits the right backend and database.
argument-hint: "<task text> [--parent <id>] [--label <l>] [--autonomous-safe]"
allowed-tools: Bash(*)
---

## baton:task-add

### Step 1 — Resolve context

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
TRK="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/tracker.sh"
[ -x "$TRK" ] || TRK="$HOME/code/maestro/baton/scripts/tracker.sh"
echo "$CTX" | jq -r '"Adding to context: \(.name)"'
```

`tracker.sh` is the one seam to the task tracker — `task_tracking.type` picks the backend behind
it. Never call `bd` here; the verb set is documented in `../../references/tracker.md`.

### Step 2 — Create the task

`create` prints **only the new id**, so it is safe to capture directly.

```bash
NEW="$("$TRK" create "<task text>")"                                  # simple capture
NEW="$("$TRK" create "<task text>" --parent <id>)"                    # a child leaf under a parent
NEW="$("$TRK" create "<task text>" --labels "a,b" --description "…")" # with labels / body
```

- A `repo-<name>` label routes the task to that member repo when `baton:start` runs.
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
