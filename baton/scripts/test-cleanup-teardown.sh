#!/usr/bin/env bash
# baton — tests for the seeded hooks.home.on_cleanup tmux teardown.
#
# The line lives in docs, not a script — baton:configure copies it into a user's context.yaml —
# but it runs in the HOME session, usually inside tmux, so a wrong line kills the session doing
# the cleanup. The bare `tmux kill-session -t "$SESSION_NAME"` it replaced did exactly that: tmux
# resolves an empty -t to the current session, so a reap that yielded an empty SESSION_NAME took
# the home session down (jbh-miql). These tests run the shipped line against a private tmux
# server and pin:
#
#   - an empty SESSION_NAME kills nothing (the self-kill)
#   - a SESSION_NAME naming the home session itself kills nothing (ambient leak)
#   - a legitimately reaped worker session is still killed, and only that one
#   - a name that is merely a prefix of a live session kills nothing (no fuzzy target match)
#   - outside tmux a reap still works
#
# It also checks every doc that quotes the line quotes it verbatim, so a fix to one copy can't
# leave a stale, unguarded copy for configure to seed. And it runs baton:doctor's detection jq
# against the old line and the new one.
#
# Hermetic: `tmux` is a wrapper pinned to a private socket (-L), and the hook runs with TMUX
# unset, so nothing here can reach the user's real tmux server. Skipped (exit 0) where tmux is
# absent. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
EXAMPLE="$ROOT/references/context.example.yaml"

pass=0; fail=0
ok()  { pass=$((pass + 1)); echo "  ✓ $1"; }
bad() { fail=$((fail + 1)); echo "  ✗ $1"; }

# The canonical line, read from the shipped example rather than restated here.
LINE="$(grep -E "^ +- '.*kill-session" "$EXAMPLE" | sed -E "s/^ +- '//; s/'\$//")"
[ -n "$LINE" ] || { echo "no on_cleanup teardown found in $EXAMPLE"; exit 1; }

echo "Docs quote the same line"
for f in references/hooks.md skills/configure/SKILL.md skills/cleanup-worktrees/SKILL.md skills/doctor/SKILL.md; do
  grep -qF -- "$LINE" "$ROOT/$f" && ok "$f" || bad "$f does not carry the shipped line verbatim"
done
# A seeded form — a YAML list item or a code line starting with the command — not prose that
# names the old line on purpose to explain why it was replaced.
UNGUARDED='^[[:space:]]*(- .)?tmux kill-session -t "\$SESSION_NAME'
if grep -rnE "$UNGUARDED" "$ROOT/references" "$ROOT/skills" >/dev/null; then
  bad "an unguarded kill-session line is still shipped:"; grep -rnE "$UNGUARDED" "$ROOT/references" "$ROOT/skills"
else
  ok "no unguarded kill-session line shipped"
fi

echo "baton:doctor detection"
DETECT='.hooks.home.on_cleanup // [] | .[] | select(test("kill-session") and (test("-t \"=\\$") | not))'
OLD='tmux kill-session -t "$SESSION_NAME" 2>/dev/null || true'
OLD_LEGACY='tmux kill-session -t "$SESSION_NAME_LEGACY" 2>/dev/null || true'
n="$(jq -n --arg a "$OLD" --arg b "$OLD_LEGACY" --arg c "$LINE" '{hooks:{home:{on_cleanup:[$a,$b,$c]}}}' | jq -r "$DETECT" | wc -l | tr -d ' ')"
[ "$n" = 2 ] && ok "flags both unguarded lines, passes the guarded one" || bad "expected 2 flagged lines, got $n"

REAL_TMUX="$(command -v tmux || true)"
if [ -z "$REAL_TMUX" ]; then
  echo "tmux not installed — skipping the live-server cases"
  echo; echo "$pass passed, $fail failed"; [ "$fail" -eq 0 ]; exit
fi

# Every case gets a fresh server on its own socket. Reusing one socket races: kill-server returns
# before the server has exited, and a new-session issued straight after can reach the dying
# server ("server exited unexpectedly") and never create the fixture.
TMP="$(mktemp -d)"; export BATON_TEST_SOCK=""
mkdir -p "$TMP/bin"
printf '#!/bin/sh\nexec %s -L "$BATON_TEST_SOCK" "$@"\n' "$REAL_TMUX" > "$TMP/bin/tmux"; chmod +x "$TMP/bin/tmux"
T="$TMP/bin/tmux"; N=0
fresh() { [ -n "$BATON_TEST_SOCK" ] && "$T" kill-server 2>/dev/null; N=$((N + 1)); BATON_TEST_SOCK="baton-test-$$-$N"; }
cleanup() { [ -n "$BATON_TEST_SOCK" ] && "$T" kill-server 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

sessions() { "$T" ls -F '#S' 2>/dev/null | sort | tr '\n' ' ' | sed 's/ $//'; }

# case <desc> <SESSION_NAME> <in-tmux: y|n> <expected sorted survivors>
case_() {
  fresh
  for s in home jbh-x-fix jbh-x-fix-tests; do "$T" new-session -d -s "$s" -x 80 -y 24; done
  [ "$(sessions)" = "home jbh-x-fix jbh-x-fix-tests" ] || { bad "$1 — fixture not created (got [$(sessions)])"; return; }
  local pane; pane="$("$T" display-message -p -t home '#{pane_id}')"
  if [ "$3" = y ]; then
    env -u TMUX PATH="$TMP/bin:$PATH" TMUX_PANE="$pane" SESSION_NAME="$2" sh -c "$LINE"
  else
    env -u TMUX -u TMUX_PANE PATH="$TMP/bin:$PATH" SESSION_NAME="$2" sh -c "$LINE"
  fi
  local rc=$? got; got="$(sessions)"
  [ "$rc" -eq 0 ] || { bad "$1 — exited $rc (a hook failure must not fail the cleanup)"; return; }
  [ "$got" = "$4" ] && ok "$1" || bad "$1 — expected [$4], got [$got]"
}

echo "Guarded teardown against a live tmux server"
case_ "empty SESSION_NAME, run from home: home survives"    ""                y "home jbh-x-fix jbh-x-fix-tests"
case_ "SESSION_NAME names home itself: home survives"        home              y "home jbh-x-fix jbh-x-fix-tests"
case_ "reaped worker session is killed, nothing else"        jbh-x-fix         y "home jbh-x-fix-tests"
case_ "prefix of a live session kills nothing"               jbh-x-fi          y "home jbh-x-fix jbh-x-fix-tests"
case_ "reap from outside tmux still kills the worker"        jbh-x-fix         n "home jbh-x-fix-tests"
case_ "empty SESSION_NAME outside tmux kills nothing"        ""                n "home jbh-x-fix jbh-x-fix-tests"

# And the regression itself: the old line, same setup, kills home. If this ever stops holding,
# the test above is no longer proving anything about tmux's empty-target behaviour.
fresh
for s in home jbh-x-fix; do "$T" new-session -d -s "$s" -x 80 -y 24; done
# A client attached to home is what makes "current session" resolve to it; TMUX points the
# unqualified tmux at our server the way a real home pane's environment does.
SP="$("$T" display-message -p -t home '#{socket_path}')"; SPID="$("$T" display-message -p -t home '#{pid}')"
PANE="$("$T" display-message -p -t home '#{pane_id}')"
TMUX="$SP,$SPID,0" TMUX_PANE="$PANE" SESSION_NAME="" sh -c "$OLD"
case " $(sessions) " in
  *" home "*) echo "  · (old line did not self-kill on this tmux — the empty-target case is unproven here)" ;;
  *)          ok "control: the old unguarded line kills home with an empty name" ;;
esac

echo; echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
