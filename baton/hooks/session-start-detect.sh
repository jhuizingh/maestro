#!/usr/bin/env bash
# baton SessionStart hook — zero-touch worker orientation.
#
# If this session is starting in a baton worktree (one whose identity resolves to a leaf bead
# in a registered context), inject a note telling the session to run baton:resume. Silent and
# harmless in every other case.
#
# Fast path first, cheap checks before touching the identity seam or beads, so non-baton
# sessions pay almost nothing.

# Never let a hook failure disrupt the session.
set -u

GD="$(git rev-parse --git-dir 2>/dev/null || true)"
[ -n "$GD" ] || exit 0

# Fast reject, with no subprocess and no knowledge of any naming scheme. A baton worktree is
# always a LINKED worktree, and a linked worktree's per-worktree git dir contains a `gitdir`
# file that a main worktree's `.git` does not. The identity carrier is accepted directly for
# the `in-place` work mode, where there is no linked worktree to detect.
#
# This deliberately does NOT re-implement the branch regex. It used to, with a comment asking
# that the copy be kept in sync with scripts/task-identity.sh — and it could not stay in sync
# once `naming.branch` made the branch free-form, because there is no longer a branch shape to
# match. Everything past this point goes through the one seam.
[ -f "$GD/gitdir" ] || [ -f "$GD/baton-identity" ] || exit 0

command -v jq >/dev/null 2>&1 || exit 0

BATON="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}"
[ -d "$BATON" ] || BATON="$HOME/code/maestro/baton"
RESOLVER="$BATON/scripts/resolve-context.sh"
IDENT="$BATON/scripts/task-identity.sh"
TRK="$BATON/scripts/tracker.sh"
[ -x "$RESOLVER" ] && [ -x "$IDENT" ] && [ -x "$TRK" ] || exit 0

CTX="$("$RESOLVER" 2>/dev/null || true)"
[ -n "$CTX" ] || exit 0

TRACKER="$(printf '%s' "$CTX" | jq -r '.task_tracking.dir // empty' 2>/dev/null)"
[ -n "$TRACKER" ] || exit 0

# The one seam: carrier, then the directory name, then the branch — backfilling the carrier
# whenever a fallback answered, so this worktree is authoritative from here on.
ID="$("$IDENT" --worktree "$PWD" --context - --format env 2>/dev/null <<<"$CTX" || true)"
[ -n "$ID" ] || exit 0
eval "$ID" || exit 0
[ -n "${LEAF:-}" ] || exit 0

# Confirm the task actually exists before saying anything. Through the tracker seam, so this
# hook works for whatever backend the context uses — it used to require `bd` on PATH and would
# silently no-op on a machine without it, which for a non-beads context would have been forever.
# The context is piped in rather than re-resolved: it is already in hand, and a hook should be
# cheap.
"$TRK" --context - get "$LEAF" >/dev/null 2>&1 <<<"$CTX" || exit 0

CTXNAME="$(printf '%s' "$CTX" | jq -r '.name // "?"' 2>/dev/null)"
MSG="This session is in a baton worktree (context: ${CTXNAME}) for bead ${LEAF} on branch ${BR}. Run the baton:resume skill to load the task and continue where it left off."

jq -cn --arg m "$MSG" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$m}}'
exit 0
