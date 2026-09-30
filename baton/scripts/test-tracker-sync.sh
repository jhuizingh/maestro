#!/usr/bin/env bash
# baton — tests for scripts/tracker-sync.sh and the beads backend's `sync-status` verb.
#
# A tracker whose pushes silently never happen looks exactly like a healthy one from the machine
# that holds it: every read is right, every pull succeeds. So these tests attack the two places
# that silence can come from — a skip that should have warned, and a drift check that answers
# "in sync" when it does not know.
#
# Hermetic by default: `bd` (and `dolt`) are stubs, so CI needs neither. When a real `dolt` CLI is
# installed, one more section builds a real Dolt database and a file:// remote and checks the
# drift count against actual history, in both directions. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }
_has() { case "$2" in *"$3"*) _ok "$1" ;; *) _bad "$1" "'$2' does not contain '$3'" ;; esac; }
_hasnt() { case "$2" in *"$3"*) _bad "$1" "'$2' contains '$3'" ;; *) _ok "$1" ;; esac; }

# A private copy of the scripts, so a test-only backend can sit next to beads.sh.
S="$TMP/scripts"; mkdir -p "$S/tracker"
cp "$HERE/tracker.sh" "$HERE/tracker-sync.sh" "$S/"
cp "$HERE/tracker/beads.sh" "$HERE/tracker/lib-registry.sh" "$S/tracker/"
cat >"$S/tracker/nosync.sh" <<'EOF'
#!/usr/bin/env bash
exit 4
EOF
SYNC="$S/tracker-sync.sh"

# =============================================================================================
# Stub `bd` and `dolt`. Behaviour is driven by files under $ST; every bd call is logged to
# $ST/calls so a test can assert what was — and was not — run.
#   remote_out      what `bd dolt remote list` prints
#   push_fail       N: fail the next N pushes as non-fast-forward
#   pull_fail       exists: every pull fails
#   mode            server | embedded   (what `bd dolt show` reports)
#   ahead, behind   counts the SQL answers with; `noref` exists: the remote ref is missing
#   real            exists: push/pull/sql go to the real dolt CLI in $ST/data/$ST/db
# =============================================================================================
BIN="$TMP/bin"; mkdir -p "$BIN"
cat >"$BIN/_answer" <<'EOF'
#!/usr/bin/env bash
# Answers the four queries sync-status sends, CSV with a header, the way dolt does.
ST="${STUB_STATE:?}"; q="$1"
case "$q" in
  *"FROM dolt_remotes WHERE"*) printf 'url\nfile:///remote\n' ;;
  *"FROM dolt_remotes"*)       printf 'name\norigin\n' ;;
  *"active_branch()"*)         printf 'active_branch()\nmain\n' ;;
  *"dolt_log("*)
    [ -e "$ST/noref" ] && { echo "branch not found: remotes/origin/main" >&2; exit 1; }
    case "$q" in *"'main', '--not'"*) n="$(cat "$ST/ahead")" ;; *) n="$(cat "$ST/behind")" ;; esac
    printf 'COUNT(*)\n%s\n' "$n" ;;
  *) echo "unexpected query: $q" >&2; exit 1 ;;
esac
EOF
cat >"$BIN/bd" <<'EOF'
#!/usr/bin/env bash
ST="${STUB_STATE:?}"
echo "bd $*" >>"$ST/calls"
real() { ( cd "$(cat "$ST/data")/$(cat "$ST/db")" && dolt "$@" ); }
case "$1 ${2:-}" in
  "dolt remote") cat "$ST/remote_out" 2>/dev/null ;;
  "dolt push")
    if [ -e "$ST/real" ]; then real push origin main 2>&1; exit $?; fi
    n="$(cat "$ST/push_fail" 2>/dev/null || echo 0)"
    if [ "$n" -gt 0 ]; then
      echo $((n-1)) >"$ST/push_fail"
      echo "Error: failed to push: the tip of your current branch is behind (non-fast-forward)" >&2
      exit 1
    fi ;;
  "dolt pull")
    if [ -e "$ST/real" ]; then real pull origin main 2>&1; exit $?; fi
    [ -e "$ST/pull_fail" ] && { echo "Error: pull: authentication failed" >&2; exit 1; }
    : ;;
  "dolt show")
    printf 'Dolt Configuration\n  Database: %s\n  Mode:     %s\n  Data:     %s\n' \
      "$(cat "$ST/db" 2>/dev/null || echo tst)" "$(cat "$ST/mode")" "$(cat "$ST/data" 2>/dev/null || echo /nonexistent)" ;;
  "sql --csv")
    [ "$(cat "$ST/mode")" = server ] || { echo "Error: 'bd sql' is not yet supported in embedded mode" >&2; exit 1; }
    "$(dirname "$0")/_answer" "$3" ;;
  *) echo "stub bd: unhandled: $*" >&2; exit 1 ;;
esac
EOF
chmod +x "$BIN/bd" "$BIN/_answer"
# A stub `dolt` for embedded mode, shadowing any real one. The real-dolt section below restores
# the real binary for itself.
REAL_DOLT="$(command -v dolt 2>/dev/null || true)"
mkdir -p "$TMP/stubdolt"
cat >"$TMP/stubdolt/dolt" <<'EOF'
#!/usr/bin/env bash
# only `dolt sql -r csv -q <query>` is used by sync-status
[ "$1" = sql ] || { echo "stub dolt: unhandled: $*" >&2; exit 1; }
exec "$(dirname "$0")/../bin/_answer" "$5"
EOF
chmod +x "$TMP/stubdolt/dolt"

export STUB_STATE="$TMP/st"
BASEPATH="$BIN:$TMP/stubdolt:$PATH"

_reset() { # $1 = sync setting: "" (key absent) | true | false
  rm -rf "$STUB_STATE"; mkdir -p "$STUB_STATE" "$TMP/trk/.beads"
  : >"$STUB_STATE/calls"
  echo server >"$STUB_STATE/mode"; echo 0 >"$STUB_STATE/ahead"; echo 0 >"$STUB_STATE/behind"
  echo "origin               file:///remote" >"$STUB_STATE/remote_out"
  if [ -n "${1:-}" ]; then
    jq -n --arg d "$TMP/trk/.beads" --argjson s "$1" '{task_tracking:{type:"beads",dir:$d,sync:$s}}'
  else
    jq -n --arg d "$TMP/trk/.beads" '{task_tracking:{type:"beads",dir:$d}}'
  fi >"$TMP/ctx.json"
}
# run MODE -> sets OUT (stdout), ERR (stderr), RC
_run() {
  OUT="$(PATH="$BASEPATH" "$SYNC" --context "$TMP/ctx.json" "$1" 2>"$TMP/err")"; RC=$?
  ERR="$(cat "$TMP/err")"
}
_calls() { cat "$STUB_STATE/calls"; }

echo "opt-out: task_tracking.sync: false"
for m in pull push check; do
  _reset false; _run "$m"
  _eq  "$m: exit 0"                  "$RC" 0
  _eq  "$m: no output at all"        "$OUT$ERR" ""
  _eq  "$m: the tracker is never touched" "$(_calls)" ""
done
_reset true; _run push
_has "sync: true still pushes" "$(_calls)" "bd dolt push"

echo "push, remote configured"
_reset; _run push
_eq  "success: exit 0"              "$RC" 0
_eq  "success: silent"              "$OUT$ERR" ""
_eq  "success: one push, no pull"   "$(_calls | grep -c 'dolt pu')" 1

_reset; echo 1 >"$STUB_STATE/push_fail"; _run push
_eq  "rejected once: exit 0"        "$RC" 0
_has "rejected once: says it merged" "$OUT" "pulled, merged and pushed"
_eq  "rejected once: push, pull, push" "$(_calls | grep 'dolt pu' | awk '{print $3}' | tr '\n' ' ')" "push pull push "

_reset; echo 9 >"$STUB_STATE/push_fail"; _run push
_eq  "rejected always: still exit 0" "$RC" 0
_has "rejected always: reported"     "$ERR" "FAILED"
_has "rejected always: names remote" "$ERR" "file:///remote"
_has "rejected always: quotes the cause" "$ERR" "non-fast-forward"
_eq  "rejected always: exactly one retry" "$(_calls | grep -c 'dolt push')" 2

_reset; echo 1 >"$STUB_STATE/push_fail"; : >"$STUB_STATE/pull_fail"; _run push
_eq  "retry pull fails: exit 0"      "$RC" 0
_has "retry pull fails: reported with the pull's cause" "$ERR" "authentication failed"
_eq  "retry pull fails: no second push" "$(_calls | grep -c 'dolt push')" 1

_hasnt "never --force, in any case above" "$(_calls)" "force"

echo "no remote configured"
for m in pull push; do
  _reset; echo "No remotes configured." >"$STUB_STATE/remote_out"; _run "$m"
  _eq  "$m: exit 0"                  "$RC" 0
  _has "$m: WARNS, not a silent skip" "$ERR" "WARNING no sync remote"
  _has "$m: names the opt-out"       "$ERR" "task_tracking.sync: false"
  _hasnt "$m: does not sync"         "$(_calls)" "dolt pu"
done
_reset; echo "No remotes configured." >"$STUB_STATE/remote_out"; _run check
_eq  "check: unknown (3), not in sync" "$RC" 3

echo "backend with nothing to sync (exit 4)"
for m in pull push check; do
  _reset; jq '.task_tracking.type = "nosync"' "$TMP/ctx.json" >"$TMP/c2" && mv "$TMP/c2" "$TMP/ctx.json"
  _run "$m"
  _eq "$m: exit 0, silent" "$RC:$OUT$ERR" "0:"
done

echo "pull"
_reset; _run pull
_eq  "pull: exit 0"                 "$RC" 0
_has "pull: names the remote"       "$OUT" "pulled from file:///remote"
_reset; : >"$STUB_STATE/pull_fail"; _run pull
_eq  "pull failure: exit 0"         "$RC" 0
_has "pull failure: reported"       "$ERR" "authentication failed"

echo "no context / unreadable tracker"
_reset
OUT="$(PATH="$BASEPATH" "$SYNC" --context - push <<<'not json' 2>&1)"; RC=$?
_eq  "push with no context: exit 0" "$RC" 0
_has "push with no context: says so" "$OUT" "no context resolved"
_reset; jq '.task_tracking.dir = ""' "$TMP/ctx.json" >"$TMP/c2" && mv "$TMP/c2" "$TMP/ctx.json"
for m in pull push; do
  _run "$m"
  _eq  "$m, unreachable tracker: exit 0"   "$RC" 0
  _has "$m, unreachable tracker: reported" "$ERR" "could not read the tracker's remote"
done
_run check
_eq  "check, unreachable tracker: unknown (3)" "$RC" 3

echo "check (drift)"
for mode in server embedded; do
  _reset; echo "$mode" >"$STUB_STATE/mode"
  if [ "$mode" = embedded ]; then mkdir -p "$TMP/ed/tst/.dolt"; echo "$TMP/ed" >"$STUB_STATE/data"; fi
  _run check
  _eq  "$mode: in sync -> 0"          "$RC" 0
  _has "$mode: in sync -> says so"    "$OUT" "in sync with file:///remote"
  echo 263 >"$STUB_STATE/ahead"; _run check
  _eq  "$mode: local ahead -> 2"      "$RC" 2
  _has "$mode: local ahead -> count"  "$OUT" "263 commit(s) not on the remote"
  echo 0 >"$STUB_STATE/ahead"; echo 5 >"$STUB_STATE/behind"; _run check
  _eq  "$mode: remote ahead -> 2"     "$RC" 2
  _has "$mode: remote ahead -> count" "$OUT" "remote has 5 commit(s) not merged here"
  echo 3 >"$STUB_STATE/ahead"; _run check
  _has "$mode: both -> both reported" "$OUT" "3 commit(s) not on the remote; remote has 5"
  : >"$STUB_STATE/noref"; _run check
  _eq  "$mode: missing remote ref -> unknown (3), never in sync" "$RC" 3
  _has "$mode: missing remote ref -> says why" "$OUT" "ever pulled or pushed"
done
_reset; echo embedded >"$STUB_STATE/mode"; echo "$TMP/nowhere" >"$STUB_STATE/data"; _run check
_eq  "embedded, no database dir -> unknown (3)" "$RC" 3
_reset; echo embedded >"$STUB_STATE/mode"; mkdir -p "$TMP/ed/tst/.dolt"; echo "$TMP/ed" >"$STUB_STATE/data"
OUT="$(PATH="$BIN:$(dirname "$(command -v jq)"):/usr/bin:/bin" "$SYNC" --context "$TMP/ctx.json" check 2>/dev/null)"; RC=$?
if [ -z "$(PATH="$BIN:$(dirname "$(command -v jq)"):/usr/bin:/bin" command -v dolt)" ]; then
  _eq  "embedded without a dolt CLI -> unknown (3)" "$RC" 3
  _has "embedded without a dolt CLI -> says so" "$OUT" "dolt CLI is not installed"
fi

# =============================================================================================
# Real Dolt: a database, a file:// remote, and actual history on both sides of it.
# =============================================================================================
if [ -n "$REAL_DOLT" ]; then
  echo "real dolt: drift against actual history"
  export DOLT_ROOT_PATH="$TMP/doltroot"; mkdir -p "$DOLT_ROOT_PATH"
  D="$TMP/real"; mkdir -p "$D/data/tst" "$D/remote" "$D/other"
  RP="$BIN:$(dirname "$REAL_DOLT"):$PATH"
  PATH="$RP" dolt config --global --add user.name t >/dev/null
  PATH="$RP" dolt config --global --add user.email t@example.com >/dev/null
  (
    set -e; export PATH="$RP"
    cd "$D/data/tst"
    dolt init -b main >/dev/null
    dolt sql -q "CREATE TABLE t (i INT PRIMARY KEY)" >/dev/null
    dolt add . >/dev/null; dolt commit -m base >/dev/null
    dolt remote add origin "file://$D/remote" >/dev/null
    dolt push origin main >/dev/null 2>&1
    for i in 1 2 3; do
      dolt sql -q "INSERT INTO t VALUES ($i)" >/dev/null; dolt commit -am "local $i" >/dev/null
    done
  ) || _bad "real dolt: fixture setup" "dolt commands failed"
  _reset; echo embedded >"$STUB_STATE/mode"; echo "$D/data" >"$STUB_STATE/data"
  echo tst >"$STUB_STATE/db"; : >"$STUB_STATE/real"
  echo "origin file://$D/remote" >"$STUB_STATE/remote_out"
  _rrun() { OUT="$(PATH="$RP" "$SYNC" --context "$TMP/ctx.json" "$1" 2>"$TMP/err")"; RC=$?; ERR="$(cat "$TMP/err")"; }

  _rrun check
  _eq  "remote behind local: drift (2)"       "$RC" 2
  _has "remote behind local: exact count"     "$OUT" "3 commit(s) not on the remote"
  _rrun push
  _eq  "push: exit 0, silent"                 "$RC:$ERR" "0:"
  _rrun check
  _eq  "after push: in sync (0)"              "$RC" 0

  # Another clone pushes; after a fetch, this tracker is the one behind.
  ( set -e; export PATH="$RP"
    cd "$D/other" && dolt clone "file://$D/remote" c >/dev/null 2>&1 && cd c
    dolt sql -q "INSERT INTO t VALUES (100)" >/dev/null; dolt commit -am other >/dev/null
    dolt push origin main >/dev/null 2>&1
    cd "$D/data/tst" && dolt fetch origin >/dev/null 2>&1
  ) || _bad "real dolt: second clone" "dolt commands failed"
  _rrun check
  _eq  "local behind remote: drift (2)"       "$RC" 2
  _has "local behind remote: exact count"     "$OUT" "remote has 1 commit(s) not merged here"

  # Diverged: a local commit on top. The push is rejected, so the helper must pull (merge) and
  # push again — and end in sync, with nothing forced.
  ( export PATH="$RP"; cd "$D/data/tst"
    dolt sql -q "INSERT INTO t VALUES (4)" >/dev/null; dolt commit -am "local 4" >/dev/null )
  _rrun push
  _eq  "diverged push: exit 0"                "$RC" 0
  _has "diverged push: merged, then pushed"   "$OUT" "pulled, merged and pushed"
  _rrun check
  _eq  "diverged push: ends in sync"          "$RC:$OUT" "0:tracker: in sync with file://$D/remote"
  _eq  "diverged push: remote kept both rows" \
       "$(cd "$D/other/c" && PATH="$RP" dolt pull origin main >/dev/null 2>&1; PATH="$RP" dolt sql -r csv -q 'SELECT COUNT(*) FROM t' | sed -n 2p)" 5
else
  echo "real dolt: skipped (no dolt CLI)"
fi

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
