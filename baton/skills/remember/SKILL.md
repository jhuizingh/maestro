---
description: Save a decision, preference, or convention into the active context's guidance.md so future baton runs honor it — without editing the plugin. This is the write path the retrospective uses; call it anytime you want the workflow to remember something for this context.
argument-hint: "<the thing to remember>"
allowed-tools: Bash(*), Read, Edit
---

## baton:remember

### Step 1 — Resolve context + guidance file

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
RESOLVER="$BATON/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
echo "Guidance: $GUIDE"
```

### Step 2 — Decide where it belongs

Most preferences → a bullet in `guidance.md`. But if the thing to remember is really:
- a **repeatable action at a lifecycle point** → propose adding it to `hooks.home.*` /
  `hooks.worker.*` in `context.yaml` (e.g. "always run the test suite before finishing" →
  `hooks.worker.pre_finish`). Consult `../../references/hooks.md` (resolve relative to
  the baton root `$BATON` — `../../references/locating-baton.md` shows how it is found)
  to pick the right one of the six and to
  check the action only uses variables that hook actually gets;
- a **structural setting** (a new member repo, a changed work mode, a required tool) → propose the
  corresponding `context.yaml` change.

Say which target you're proposing and why.

### Step 3 — Write it (after confirmation)

- Guidance: append a dated, concise bullet under the relevant heading in `$GUIDE` (create the
  file from the standard header if missing).
- Hook / config: edit `context.yaml` with the specific change.

Confirm the exact change with the user before writing. Never edit the plugin — everything lands
in the (user-owned) workspace repo. Report what changed and where.
