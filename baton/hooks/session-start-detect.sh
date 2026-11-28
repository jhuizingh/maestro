#!/usr/bin/env bash
# baton SessionStart hook — zero-touch session orientation.
#
# - In a baton worktree (one whose identity resolves to a leaf bead in a registered context),
#   inject a note telling the session to run baton:resume.
# - In a context's home (the orchestrator), inject the reply convention
#   (references/orchestrator-replies.md).
#
# Silent and harmless in every other case.
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
ROLE=home
if [ -f "$GD/gitdir" ] || [ -f "$GD/baton-identity" ]; then ROLE=worker; fi

# A home session needs a registered context; without one, pay for nothing more.
if [ "$ROLE" = home ]; then
  [ -n "${BATON_WORKSPACES:-}" ] || [ -f "${BATON_REGISTRY:-$HOME/.config/baton/registry.yaml}" ] || exit 0
fi

command -v jq >/dev/null 2>&1 || exit 0

# This hook lives inside the plugin, so its own location IS the plugin root — the copy that is
# actually running, with no env var or cache lookup to get wrong (see references/locating-baton.md).
BATON="$(cd -P "$(dirname "$0")/.." 2>/dev/null && pwd)" || exit 0
[ -d "$BATON/scripts" ] || exit 0
RESOLVER="$BATON/scripts/resolve-context.sh"
IDENT="$BATON/scripts/task-identity.sh"
TRK="$BATON/scripts/tracker.sh"
[ -x "$RESOLVER" ] && [ -x "$IDENT" ] && [ -x "$TRK" ] || exit 0

CTX="$("$RESOLVER" 2>/dev/null || true)"
[ -n "$CTX" ] || exit 0

if [ "$ROLE" = home ]; then
  # The workspace or `home` itself: a member repo's clone and the default context do not count.
  HERE="$(pwd -P)"; TOP="$(git rev-parse --show-toplevel 2>/dev/null)"; IS_HOME=no
  [ -n "$TOP" ] || exit 0
  while IFS= read -r d; do
    case "$d" in
      "~") d="$HOME" ;;
      "~/"*) d="$HOME/${d#\~/}" ;;
    esac
    d="$(cd -P "$d" 2>/dev/null && pwd)" || continue
    # Same repository too: one cloned inside the workspace, or under a broad `home`, is not home.
    [ "$(git -C "$d" rev-parse --show-toplevel 2>/dev/null)" = "$TOP" ] || continue
    case "$HERE/" in "$d"/*) IS_HOME=yes ;; esac
  done < <(jq -r '._workspace, .home | strings | select(. != "")' <<<"$CTX" 2>/dev/null)
  [ "$IS_HOME" = yes ] || exit 0
  MSG="$(jq -r --arg ref "$BATON/references/orchestrator-replies.md" \
    --arg ask "$BATON/references/asking-questions.md" '
    "This is the home (orchestrator) session of baton context \(.name // "?"). "
    + "Head each part of a reply that is about a task with a line `### <task-id>`, unless the turn "
    + "answers a single `[fermata <task-id>] …` message. "
    + "Ask the user in plain text, never a modal picker such as AskUserQuestion: the question, "
    + "numbered options with the recommendation marked, then `Reply with a number, or your own "
    + "answer.` In the same turn, pose it with `tracker.sh decide <task-id> --question … --option …` "
    + "(one per question, never a free-text DECISION note; format: \($ask)), and "
    + "`tracker.sh resolve` it once acted on. "
    + "When you answer a `[fermata <task-id> <decision-id> question]` message, also record it with "
    + "`tracker.sh reply <task-id> <decision-id> --kind response`. "
    + "When you write to a task (note, state, decision, message to its worker), say so in its "
    + "section. Details: \($ref)"' <<<"$CTX" 2>/dev/null)"
  [ -n "$MSG" ] || exit 0
  jq -cn --arg m "$MSG" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$m}}'
  exit 0
fi

TRACKER="$(printf '%s' "$CTX" | jq -r '.task_tracking.dir // empty' 2>/dev/null)"
[ -n "$TRACKER" ] || exit 0

# The one seam: carrier, then the directory name, then the branch. This is the worker's own
# worktree, so a tracker-confirmed fallback is backfilled and the next read is authoritative.
ID="$("$IDENT" --worktree "$PWD" --context - --backfill --format env 2>/dev/null <<<"$CTX" || true)"
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
MSG="This session is in a baton worktree (context: ${CTXNAME}) for bead ${LEAF} on branch ${BR}. Run the baton:resume skill to load the task and continue where it left off. When you need the user to decide something, never open a modal picker such as AskUserQuestion: ask in plain text (the question, numbered options with the recommendation marked, then \`Reply with a number, or your own answer.\`) and, in the same turn, pose it with \`tracker.sh decide ${LEAF} --question … --option … --by session\`. Details: ${BATON}/references/asking-questions.md"

jq -cn --arg m "$MSG" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$m}}'
exit 0
