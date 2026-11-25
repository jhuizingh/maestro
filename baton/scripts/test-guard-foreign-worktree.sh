#!/usr/bin/env bash
# baton — tests for hooks/guard-foreign-worktree.sh: a task's session may not write to another
# task's worktree, and nobody else is affected.
#
# Hermetic: throwaway repos under a temp dir. Needs git and jq. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
HOOK="$HERE/../hooks/guard-foreign-worktree.sh"
IDENT="$HERE/task-identity.sh"
[ -x "$HOOK" ] || { echo "not found: $HOOK" >&2; exit 2; }

TMP="$(mktemp -d)" || { echo "mktemp -d failed" >&2; exit 2; }
[ -n "$TMP" ] && [ -d "$TMP" ] || { echo "mktemp -d gave no directory" >&2; exit 2; }
TMP="$(cd "$TMP" && pwd -P)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

unset BATON_CONTEXT BATON_FOREIGN_WORKTREE_OK CLAUDE_PROJECT_DIR
export HOME="$TMP"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com

for r in app lib ws; do git init -q "$TMP/$r" && git -C "$TMP/$r" commit -q --allow-empty -m x; done
git -C "$TMP/app" worktree add -q "$TMP/app-a" -b a
git -C "$TMP/app" worktree add -q "$TMP/app-b" -b b
git -C "$TMP/app" worktree add -q "$TMP/loose" -b loose
git -C "$TMP/lib" worktree add -q "$TMP/lib-a" -b a
mkdir "$TMP/app-a/sub" "$TMP/app-b/sub"
"$IDENT" --write-carrier "$TMP/app-a" --leaf t-1 --slug one >/dev/null
"$IDENT" --write-carrier "$TMP/lib-a" --leaf t-1 --slug one >/dev/null
"$IDENT" --write-carrier "$TMP/app-b" --leaf t-2.3 --slug two >/dev/null

PROJECT="$TMP/app-a"; CWD="$TMP/app-a"; ERR=""
_run() { # $1 = command; sets RC and ERR
  ERR="$(jq -n --arg c "$1" --arg d "$CWD" '{tool_name:"Bash",cwd:$d,tool_input:{command:$c}}' \
    | CLAUDE_PROJECT_DIR="$PROJECT" "$HOOK" 2>&1)"; RC=$?
}
_check() { # $1 = what, $2 = wanted rc, $3 = command
  _run "$3"
  if [ "$RC" = "$2" ] && { [ "$2" = 2 ] || [ -z "$ERR" ]; }; then PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"
  else FAIL=$((FAIL+1)); printf '  ❌ %s\n     rc %s, want %s: %s\n' "$1" "$RC" "$2" "$ERR"; fi
}
_blocks() { _check "$1" 2 "$2"; }
_allows() { _check "$1" 0 "$2"; }

echo "a worker writing to another task's worktree"
_blocks "cd, add, commit"   "cd $TMP/app-b && git add . && git commit -m x"
_blocks "cd, push"          "cd $TMP/app-b; git push"
_blocks "a subdirectory"    "cd $TMP/app-b/sub && git commit -m x"
_blocks "a relative cd"     "cd ../app-b && git commit -m x"
_blocks "git -C"            "git -C $TMP/app-b commit -m x"
_blocks "a variable"        "WT=$TMP/app-b; git -C \"\$WT\" push"
for s in merge rebase revert cherry-pick am; do _blocks "$s" "git -C $TMP/app-b $s x"; done
_blocks "after its own"     "git commit -m x; git -C $TMP/app-b push"
CWD="$TMP/app-b"
_blocks "from an earlier cd" "git commit -m x"
CWD="$TMP/app-a"; PROJECT="$TMP/app-a/sub"
_blocks "launched in a subdirectory" "git -C $TMP/app-b commit -m x"
PROJECT="$TMP/app-a"
_run "git -C $TMP/app-b commit -m x"
for want in "t-1" "t-2.3" "$TMP/app-b" "BATON_FOREIGN_WORKTREE_OK=t-2.3 git commit"; do
  case "$ERR" in *"$want"*) PASS=$((PASS+1)); echo "  ✅ the message names $want" ;;
    *) FAIL=$((FAIL+1)); echo "  ❌ the message names $want"; echo "     $ERR" ;; esac
done

echo "the override"
_allows "named leaf"          "cd $TMP/app-b && BATON_FOREIGN_WORKTREE_OK=t-2.3 git commit -m x"
_allows "exported"            "export BATON_FOREIGN_WORKTREE_OK=t-2.3; git -C $TMP/app-b commit; git -C $TMP/app-b push"
_blocks "another leaf"        "BATON_FOREIGN_WORKTREE_OK=t-9 git -C $TMP/app-b commit"
_blocks "just 1"              "BATON_FOREIGN_WORKTREE_OK=1 git -C $TMP/app-b commit"
_blocks "empty"               "BATON_FOREIGN_WORKTREE_OK= git -C $TMP/app-b commit"
_blocks "on one command only" "cd $TMP/app-b; BATON_FOREIGN_WORKTREE_OK=t-2.3 git commit; git push"
_blocks "in the message"      "git -C $TMP/app-b commit -m BATON_FOREIGN_WORKTREE_OK=t-2.3"
export BATON_FOREIGN_WORKTREE_OK=t-2.3
_blocks "in the environment"  "git -C $TMP/app-b commit"
unset BATON_FOREIGN_WORKTREE_OK

echo "a worker, otherwise"
_allows "its own worktree"     "git commit -m x && git push"
_allows "its own subdirectory" "cd sub && git commit -m x"
_allows "its leaf, other repo" "git -C $TMP/lib-a commit -m x"
_allows "a primary clone"      "git -C $TMP/ws commit -m x"
_allows "a worktree, no task"  "git -C $TMP/loose commit -m x"
_allows "reading"              "cd $TMP/app-b && git status && git log && git diff && git fetch"
_allows "not a repository"     "cd / && git commit"
_allows "a missing directory"  "cd $TMP/none && git commit"
_allows "an unknown directory" "cd \"\$NO_SUCH_VAR_X\" && git commit"
_allows "prose"                "bd create -d 'cd $TMP/app-b && git commit'"
_allows "prose in a heredoc"   "cat > f <<'EOF'
cd $TMP/app-b && git push
EOF
git commit -m \"it's in app-b\""

echo "sessions with no leaf"
CWD="$TMP/app-b"
PROJECT="$TMP/ws";    _allows "home"                        "git commit -m x && git push"
PROJECT="$TMP/app";   _allows "a primary clone"             "git commit -m x"
PROJECT="$TMP/loose"; _allows "a worktree with no task"     "git commit -m x"
PROJECT="";           _allows "no project dir"              "git commit -m x"
PROJECT="$TMP/none";  _allows "a project dir that is gone"  "git commit -m x"

echo "inputs that must not break a call"
PROJECT="$TMP/app-a"
for p in "" "not json git commit" '{"tool_name":"Write","tool_input":{"command":"git commit"}}' '{"tool_name":"Bash","x":"git commit"}'; do
  CLAUDE_PROJECT_DIR="$PROJECT" "$HOOK" <<<"$p" >/dev/null 2>&1; RC=$?
  if [ "$RC" = 0 ]; then PASS=$((PASS+1)); echo "  ✅ payload '$p'"; else FAIL=$((FAIL+1)); echo "  ❌ payload '$p': rc $RC"; fi
done
export GIT_DIR="$TMP/app/.git"
_blocks "an inherited GIT_DIR" "git commit -m x"
unset GIT_DIR

if jq -e '[.hooks.PreToolUse[] | select(.matcher == "Bash") | .hooks[].command] | any(endswith("guard-foreign-worktree.sh"))' \
     "$HERE/../hooks/hooks.json" >/dev/null; then PASS=$((PASS+1)); echo "  ✅ registered"
else FAIL=$((FAIL+1)); echo "  ❌ registered in hooks.json"; fi

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
