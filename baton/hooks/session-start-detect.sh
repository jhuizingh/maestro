#!/usr/bin/env bash
# baton SessionStart hook — zero-touch worker orientation.
#
# If this session is starting in a baton worktree (its branch resolves to a leaf
# bead in a registered context), inject a note telling the session to run
# baton:resume. Silent and harmless in every other case.
#
# Fast path first, cheap checks before touching beads, so non-baton sessions pay
# almost nothing.

# Never let a hook failure disrupt the session.
set -u

BR="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
[ -n "$BR" ] || exit 0

# Branch must look like <prefix>-<hash>-<slug> (beads id + slug). The hash segment
# may itself contain a dot for dotted child-bead ids (e.g. abc-8z1.3).
printf '%s' "$BR" | grep -qE '^[a-z0-9]+-[a-z0-9.]+-' || exit 0
LEAF="$(printf '%s' "$BR" | sed -E 's/^([a-z0-9]+-[a-z0-9.]+)-.*/\1/')"

command -v jq >/dev/null 2>&1 || exit 0
command -v bd >/dev/null 2>&1 || exit 0

RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || exit 0

CTX="$("$RESOLVER" 2>/dev/null || true)"
[ -n "$CTX" ] || exit 0

TRACKER="$(printf '%s' "$CTX" | jq -r '.task_tracking.dir // empty' 2>/dev/null)"
[ -n "$TRACKER" ] || exit 0
TRACKER="${TRACKER/#\~/$HOME}"

# Confirm the bead actually exists before saying anything.
BEADS_DIR="$TRACKER" bd show "$LEAF" --json >/dev/null 2>&1 || exit 0

CTXNAME="$(printf '%s' "$CTX" | jq -r '.name // "?"' 2>/dev/null)"
MSG="This session is in a baton worktree (context: ${CTXNAME}) for bead ${LEAF} on branch ${BR}. Run the baton:resume skill to load the task and continue where it left off."

jq -cn --arg m "$MSG" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$m}}'
exit 0
