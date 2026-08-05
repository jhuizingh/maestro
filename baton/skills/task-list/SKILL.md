---
description: List tasks in the active context's tracker — ready work, in-progress, or a full tree — with optional filtering. Thin wrapper over beads, context-resolved so it reads the right database.
argument-hint: "[ready | in-progress | all | <bd list flags>]"
allowed-tools: Bash(*)
---

## baton:task-list

### Step 1 — Resolve context

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
echo "$CTX" | jq -r '"Context: \(.name)  tracker: \(.task_tracking.dir)"'
```

### Step 2 — List

Map `$ARGUMENTS` to a beads query:
- `ready` (default) → `bd ready`
- `in-progress` → `bd list --status in_progress`
- `all` → `bd list --status all`
- a parent id → `bd children <id> --pretty` (tree of subtasks)
- anything else → pass through as `bd list <args>`

Present the result compactly: id, title, status, and parent/child relationship where relevant.
Point out which items are ready to `baton:start`.
