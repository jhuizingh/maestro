---
description: List tasks in the active context's tracker — ready work, in-progress, or a full tree — with optional filtering. Thin wrapper over the tracker seam, context-resolved so it reads the right backend and database.
argument-hint: "[ready | in-progress | all | --status <s> | --label <l>]"
allowed-tools: Bash(*)
---

## baton:task-list

### Step 1 — Resolve context

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
TRK="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/tracker.sh"
[ -x "$TRK" ] || TRK="$HOME/code/maestro/baton/scripts/tracker.sh"
echo "$CTX" | jq -r '"Context: \(.name)  tracker: \(.task_tracking.dir)  type: \(.task_tracking.type // "beads")"'
```

`tracker.sh` is the one seam to the task tracker — `task_tracking.type` picks the backend behind
it, so this skill works the same whether that is beads, Jira or anything else. Never call `bd`
here. The verb set is documented in `../../references/tracker.md`.

### Step 2 — List

Map `$ARGUMENTS` to a verb:
- `ready` (default) → `"$TRK" ready`
- `in-progress` → `"$TRK" list --status in_progress`
- `all` → `"$TRK" list --status all`
- a parent id → `"$TRK" children <id>` (subtasks)
- anything else → the flags `list` accepts: `--status <s>`, `--label <l>`, `--limit <n>`

Every one of those returns a JSON **array of task objects** (`id`, `title`, `status`, `labels`,
`parent`, …) — the same shape from every backend, so format it with `jq` rather than reading a
backend's own rendering. Present it compactly: id, title, status, and parent/child relationship
where relevant. Point out which items are ready to `baton:start`.
