#!/usr/bin/env bash
# baton PreToolUse hook — one worktree, one writer.
#
# Blocks git commands that write history (commit, merge, push, …) when a session that belongs to
# one task runs them in another task's worktree.
#
# The session's own leaf is the identity carrier of the directory it was LAUNCHED in
# (CLAUDE_PROJECT_DIR), which `cd` does not change and subagents share. A home session is launched
# somewhere with no carrier, so it has no leaf and is never blocked.
#
# Allowed, deliberately:
#   - a target with no carrier (a primary clone, the workspace, a scratch worktree)
#   - a target with the session's own leaf (a task may have a worktree in several repos)
#   - BATON_FOREIGN_WORKTREE_OK=<the target's leaf>, set in the command itself
#
# Fails OPEN everywhere, like guard-primary-clone.sh; what it can't see is listed in
# scripts/git-invocations.sh.
#
# Contract: PreToolUse reads a JSON payload on stdin; exit 0 allows, exit 2 blocks and
# returns stderr to the model.

set -u

PAYLOAD="$(cat 2>/dev/null)" || exit 0

# Runs on every Bash call: no subprocess until the command could be a git write.
case "$PAYLOAD" in *git*) ;; *) exit 0 ;; esac
case "$PAYLOAD" in
  *commit*|*push*|*merge*|*rebase*|*revert*|*cherry-pick*|*\ am*) ;;
  *) exit 0 ;;
esac
[ -n "${CLAUDE_PROJECT_DIR:-}" ] && [ -d "$CLAUDE_PROJECT_DIR" ] || exit 0

# An inherited GIT_DIR would answer for every path asked about.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR

BATON="$(cd -P "$(dirname "$0")/.." 2>/dev/null && pwd)" || exit 0
IDENT="$BATON/scripts/task-identity.sh"
INVOCATIONS="$BATON/scripts/git-invocations.sh"
[ -x "$IDENT" ] && [ -x "$INVOCATIONS" ] || exit 0

OWN="$("$IDENT" --carrier-leaf "$CLAUDE_PROJECT_DIR" 2>/dev/null)" || exit 0
[ -n "$OWN" ] || exit 0

command -v jq >/dev/null 2>&1 || exit 0
CMD="$(jq -r 'select(.tool_name == "Bash") | .tool_input.command // empty' <<<"$PAYLOAD" 2>/dev/null)" || exit 0
[ -n "$CMD" ] || exit 0
CWD="$(jq -r '.cwd // empty' <<<"$PAYLOAD" 2>/dev/null)" || exit 0

LAST_DIR=""; LEAF=""
while IFS= read -r line; do
  SUB="${line%%$'\t'*}"; line="${line#*$'\t'}"
  TARGET="${line%%$'\t'*}"; OK="${line#*$'\t'}"
  case "$SUB" in commit|push|merge|rebase|revert|cherry-pick|am) ;; *) continue ;; esac
  [ -n "$TARGET" ] && [ -d "$TARGET" ] || continue
  if [ "$TARGET" != "$LAST_DIR" ]; then
    LAST_DIR="$TARGET"
    LEAF="$("$IDENT" --carrier-leaf "$TARGET" 2>/dev/null)" || LEAF=""
  fi
  [ -n "$LEAF" ] && [ "$LEAF" != "$OWN" ] && [ "$OK" != "$LEAF" ] || continue

  TOP="$(git -C "$TARGET" rev-parse --show-toplevel 2>/dev/null)" || TOP="$TARGET"
  cat >&2 <<EOF
baton: refusing 'git $SUB' — this session's task is $OWN, but the worktree belongs to $LEAF.

  $TOP

One worktree, one writer: this would land on $LEAF's branch, and its PR. Nothing in this command
has run.

If the change is part of $OWN, make it in this session's own worktree:

  $CLAUDE_PROJECT_DIR

If it is $LEAF's work, hand it over: that task's session or the home session should make it. Only
if the user has told you to write to $LEAF's worktree, say so in the command:

  BATON_FOREIGN_WORKTREE_OK=$LEAF git $SUB …
EOF
  exit 2
done < <(printf '%s' "$CMD" | "$INVOCATIONS" --cwd "$CWD" --var BATON_FOREIGN_WORKTREE_OK 2>/dev/null)
exit 0
