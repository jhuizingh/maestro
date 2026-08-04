#!/usr/bin/env bash
# baton PreToolUse hook — keep member repos' primary clones on their default branch.
#
# Blocks branch-creating git commands (checkout -b, switch -c, branch <name>) when they
# would run in the PRIMARY clone of a registered baton member repo. Work belongs in a
# worktree; the primary clone staying put is what makes git's "same branch checked out
# twice" guard meaningful — if the primary wanders off main, main becomes checkout-able
# elsewhere and a worktree can silently land on it with no error.
#
# Allowed, deliberately:
#   - anything inside a linked worktree (that's the sanctioned path)
#   - `git worktree add -b ...` anywhere (that's how worktrees get made)
#   - repos that aren't members of any registered context
#
# Fails OPEN everywhere: a missing tool, an unreadable registry, an unresolvable context,
# or any internal error allows the command. A guard that breaks ordinary work is worse
# than one that occasionally misses.
#
# Contract: PreToolUse reads a JSON payload on stdin; exit 0 allows, exit 2 blocks and
# returns stderr to the model.

set -u

# --- Fast path -------------------------------------------------------------------------
# Everything here must be cheap: the overwhelming majority of Bash calls are unrelated and
# should pay nothing beyond a JSON parse and a regex. No git, no resolver, until we know
# the command is branch-creating.

PAYLOAD="$(cat 2>/dev/null)" || exit 0
[ -n "$PAYLOAD" ] || exit 0

# Pure-bash pre-filter, no subprocess. This hook runs on EVERY Bash call, so the common
# case (a command with nothing branch-shaped in it) must not pay for a jq spawn — that
# alone measured ~35ms each, ~70ms per call, on every unrelated command.
case "$PAYLOAD" in
  *checkout*|*switch*|*branch*) ;;
  *) exit 0 ;;
esac

command -v jq >/dev/null 2>&1 || exit 0

# One jq spawn, not two.
read -r TOOL CMD_B64 <<EOF
$(printf '%s' "$PAYLOAD" | jq -r '[(.tool_name // ""), ((.tool_input.command // "") | @base64)] | join(" ")' 2>/dev/null)
EOF
[ "$TOOL" = "Bash" ] || exit 0
[ -n "${CMD_B64:-}" ] || exit 0
CMD="$(printf '%s' "$CMD_B64" | base64 --decode 2>/dev/null)" || exit 0
[ -n "$CMD" ] || exit 0

# `git worktree add -b <branch>` creates a branch, but that is exactly the sanctioned
# path — baton:start itself runs it. Never block it.
printf '%s' "$CMD" | grep -qE '\bgit\b[^|;&]*\bworktree\b' && exit 0

# Branch-creating forms. `git branch` only counts when followed by a non-flag token
# (`git branch`, `-a`, `-d foo`, `--show-current`, `--merged` all just read or delete).
printf '%s' "$CMD" | grep -qE '\bgit\b[^|;&]*(checkout[[:space:]]+(-b|-B)\b|switch[[:space:]]+(-c|-C|--create)\b|branch[[:space:]]+[^-[:space:]])' || exit 0

# --- Slow path -------------------------------------------------------------------------
# The command creates a branch. Now work out where it would land.

# Honor an explicit `git -C <path>`; otherwise use the session's cwd.
# No \b here: BSD sed (macOS) doesn't support it and silently matches nothing, which would
# quietly send the check at the session cwd instead of the -C target.
TARGET="$(printf '%s' "$CMD" | sed -nE 's/.*git[[:space:]]+-C[[:space:]]+"?([^"[:space:]]+)"?.*/\1/p' | head -1)"
if [ -z "$TARGET" ]; then
  TARGET="$(printf '%s' "$PAYLOAD" | jq -r '.cwd // ""' 2>/dev/null)"
fi
[ -n "$TARGET" ] || exit 0
TARGET="${TARGET/#\~/$HOME}"
[ -d "$TARGET" ] || exit 0

command -v git >/dev/null 2>&1 || exit 0

# A linked worktree has --git-dir under <primary>/.git/worktrees/<name>, while the primary
# clone has --git-dir == --git-common-dir. Anything in a worktree is fine by definition.
GIT_DIR="$(git -C "$TARGET" rev-parse --absolute-git-dir 2>/dev/null)" || exit 0
COMMON_DIR="$(git -C "$TARGET" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || exit 0
[ -n "$GIT_DIR" ] && [ -n "$COMMON_DIR" ] || exit 0
[ "$GIT_DIR" = "$COMMON_DIR" ] || exit 0   # in a worktree → allow

TOPLEVEL="$(git -C "$TARGET" rev-parse --show-toplevel 2>/dev/null)" || exit 0
[ -n "$TOPLEVEL" ] || exit 0

# Is this repo a member of any registered context? Only guard repos baton actually manages.
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || exit 0

CTX="$(cd "$TOPLEVEL" && "$RESOLVER" 2>/dev/null)" || exit 0
[ -n "$CTX" ] || exit 0
CTX_NAME="$(printf '%s' "$CTX" | jq -r '.name // ""' 2>/dev/null)" || exit 0
[ -n "$CTX_NAME" ] || exit 0

# The resolver falls back to the default context when cwd matches nothing, so confirm this
# repo really is one of that context's member repos before blocking.
REPO_NAME="$(basename "$TOPLEVEL")"
IS_MEMBER=no
while IFS= read -r pat; do
  [ -n "$pat" ] || continue
  pat="${pat/#\~/$HOME}"
  # shellcheck disable=SC2254
  case "$TOPLEVEL" in $pat) IS_MEMBER=yes; break ;; esac
done <<EOF
$(printf '%s' "$CTX" | jq -r '.member_repos[]?' 2>/dev/null)
EOF
[ "$IS_MEMBER" = yes ] || exit 0

# --- Block -----------------------------------------------------------------------------
WT_BASE="$(printf '%s' "$CTX" | jq -r '.worktree_base // "{code_root}/{repo}-worktrees"' 2>/dev/null)"
CODE_ROOT="$(printf '%s' "$CTX" | jq -r '.code_root // "~/code"' 2>/dev/null)"
WT_BASE="${WT_BASE//\{code_root\}/$CODE_ROOT}"
WT_BASE="${WT_BASE//\{repo\}/$REPO_NAME}"

cat >&2 <<EOF
baton: refusing to create a branch in the primary clone of '$REPO_NAME' (context: $CTX_NAME).

  $TOPLEVEL

The primary clone must stay on its default branch. git refuses to check out the same branch
in two worktrees at once, and that guard only works while the primary holds main — once the
primary wanders off, another worktree can silently land on main with no error.

Create a worktree instead:

  git -C "$TOPLEVEL" worktree add "$WT_BASE/<branch>" -b <branch> origin/main

or run baton:start, which does this for you and binds the worktree to a leaf bead.
EOF
exit 2
