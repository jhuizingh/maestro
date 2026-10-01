---
description: Quick-capture a task into the active context's tracker. Optionally make it a child of a parent (to pre-decompose a ticket into per-worktree subtasks), add labels, or mark it autonomous-safe. Thin wrapper over the tracker seam, context-resolved so it hits the right backend and database.
argument-hint: "<task text> [--parent <id>] [--label <l>] [--autonomous-safe]"
allowed-tools: Bash(*)
---

## baton:task-add

### Step 1 — Resolve context

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
RESOLVER="$BATON/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
TRK="$BATON/scripts/tracker.sh"
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

### Step 3 — Push the tracker

Once, after the last `create`:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
TS="$BATON/scripts/tracker-sync.sh"
[ -x "$TS" ] && "$TS" push
```

It never fails this skill: a skipped push (`task_tracking.sync: false`) is silent, and a missing
remote or failed push is one line — relay that line to the user as-is. See "Keeping a tracker in
step with its remote" in `../../references/tracker.md`.
