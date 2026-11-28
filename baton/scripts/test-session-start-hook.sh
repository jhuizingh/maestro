#!/usr/bin/env bash
# baton — tests for hooks/session-start-detect.sh: who gets the orchestrator's reply convention.
#
# Hermetic: throwaway repos and workspaces under a temp dir. Needs git, yq and jq. Exit 0 = all
# passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
HOOK="$HERE/../hooks/session-start-detect.sh"
[ -x "$HOOK" ] || { echo "not found: $HOOK" >&2; exit 2; }

TMP="$(mktemp -d)" || { echo "mktemp -d failed" >&2; exit 2; }
[ -n "$TMP" ] && [ -d "$TMP" ] || { echo "mktemp -d gave no directory" >&2; exit 2; }
TMP="$(cd "$TMP" && pwd -P)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }

unset BATON_CONTEXT
export HOME="$TMP" BATON_WORKSPACES="$TMP/ws"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com

mkdir -p "$TMP/ws/sub" "$TMP/ws/nested" "$TMP/home dir" "$TMP/app" "$TMP/else" "$TMP/plain"
for d in "$TMP/ws" "$TMP/ws/nested" "$TMP/home dir" "$TMP/app" "$TMP/else"; do
  git -C "$d" init -q && git -C "$d" commit -q --allow-empty -m x
done
git -C "$TMP/ws" worktree add -q "$TMP/ws-worktrees/loose" -b loose
ln -s "$TMP/ws" "$TMP/link"
cat >"$TMP/ws/context.yaml" <<YAML
name: one
default: true
home: ~/home dir
task_tracking:
  type: beads
  dir: $TMP/tracker
member_repos:
  - $TMP/app
guidance: |
  a line with a \\n in it
  another
YAML

_run() { ( cd "$1" && "$HOOK" </dev/null ); }
_says() { # $1 = what, $2 = dir
  local out; out="$(_run "$2")"
  if jq -e '.hookSpecificOutput | .hookEventName == "SessionStart"
            and (.additionalContext | contains("context one") and contains("### <task-id>"))' \
       <<<"$out" >/dev/null 2>&1; then _ok "$1"; else _bad "$1" "got '$out'"; fi
}
_silent() { # $1 = what, $2 = dir
  local out rc; out="$(_run "$2" 2>&1)"; rc=$?
  if [ -z "$out" ] && [ "$rc" = 0 ]; then _ok "$1"; else _bad "$1" "rc $rc, got '$out'"; fi
}

_says   "the workspace dir"                    "$TMP/ws"
_says   "a directory inside it"                "$TMP/ws/sub"
_says   "the workspace through a symlink"      "$TMP/link"
_says   "a home that is not the workspace"     "$TMP/home dir"
_silent "another repo cloned inside it"        "$TMP/ws/nested"
_silent "a member repo's primary clone"        "$TMP/app"
_silent "another repo, under a default context" "$TMP/else"
_silent "a directory that is not a repo"       "$TMP/plain"
_silent "a linked worktree with no task"       "$TMP/ws-worktrees/loose"
if _run "$TMP/ws" | jq -e '.hookSpecificOutput.additionalContext | contains("tracker.sh decide")' >/dev/null
then _ok "it says to ask with decide"; else _bad "it says to ask with decide" "no decide in the message"; fi
if _run "$TMP/ws" | jq -e '.hookSpecificOutput.additionalContext
     | contains("never a modal picker") and contains("Reply with a number, or your own answer.")' >/dev/null
then _ok "it says to ask in numbered plain text"; else _bad "it says to ask in numbered plain text" "no plain-text rule in the message"; fi
ASK="$(_run "$TMP/ws" | jq -r '.hookSpecificOutput.additionalContext' | sed -n 's/.*format: \([^)]*\)).*/\1/p')"
if [ -f "$ASK" ]; then _ok "the question format it names exists"; else _bad "the question format it names exists" "'$ASK'"; fi
REF="$(_run "$TMP/ws" | jq -r '.hookSpecificOutput.additionalContext' | sed -n 's/.*Details: //p')"
if [ -f "$REF" ]; then _ok "the reference it names exists"; else _bad "the reference it names exists" "'$REF'"; fi
if grep -q '"matcher"' <<<"$(jq -c '.hooks.SessionStart' "$HERE/../hooks/hooks.json")"; then
  _bad "the hook also runs on resume and compaction" "SessionStart has a matcher"
else _ok "the hook also runs on resume and compaction"; fi
sed -i.bak 's|^home: .*|home: "~"|' "$TMP/ws/context.yaml"
_silent "a repo under a home of ~"             "$TMP/else"
_says   "the workspace, with a home of ~"      "$TMP/ws"
unset BATON_WORKSPACES
_silent "no registry at all"                   "$TMP/ws"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
