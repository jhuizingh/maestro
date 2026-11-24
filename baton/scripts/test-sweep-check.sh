#!/usr/bin/env bash
# baton — tests for scripts/sweep-check.sh and the scripts/sessions.sh seam.
#
# Hermetic: the tracker is a read-only file-backed fake backend (`memo`) that fails any write verb
# and logs every call, sessions come from a fake backend, and `claude` is a fake on PATH. Nothing
# touches a real tracker, session or network. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }
command -v git >/dev/null 2>&1 || { echo "git is required" >&2; exit 2; }

TMP="$(mktemp -d)"; TMP="$(cd -P "$TMP" && pwd)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }
_has() { case "$2" in *"$3"*) _ok "$1" ;; *) _bad "$1" "'$2' does not contain '$3'" ;; esac; }
_hasnt() { case "$2" in *"$3"*) _bad "$1" "'$2' contains '$3'" ;; *) _ok "$1" ;; esac; }

unset CLAUDE_CODE_REMOTE BATON_TRACKER_RELAY BATON_RELAY_SESSION BEADS_DIR BATON_SESSIONS_TYPE
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

S="$TMP/scripts"; mkdir -p "$S/tracker" "$S/sessions"
cp "$HERE/tracker.sh" "$HERE/tracker-relay.sh" "$HERE/task-identity.sh" "$HERE/parked.sh" \
   "$HERE/sessions.sh" "$HERE/sweep-check.sh" "$S/"
cp "$HERE"/tracker/lib-*.sh "$S/tracker/"
cp "$HERE/sessions/claude-code.sh" "$S/sessions/"

# `memo`: task objects in tasks.json, decisions and registries in <id>.decisions / <id>.registry.
# `fail` lists verbs that exit 1; `whoami` answers from the file of that name, else exits 4.
cat >"$S/tracker/memo.sh" <<'EOF2'
#!/usr/bin/env bash
set -uo pipefail
D="${BATON_TRACKER_DIR:?}"
printf '%s\n' "$*" >>"$D/calls"
verb="$1"; shift
grep -qx -- "$verb" "$D/fail" 2>/dev/null && exit 1
grep -qx -- "$verb" "$D/unsupported" 2>/dev/null && exit 4
case "$verb" in
  list) [ "$*" = "--status active" ] || exit 1; jq -c 'map(select(.status != "closed"))' "$D/tasks.json" ;;
  get)  jq -ce --arg id "$1" '[.[] | select(.id == $id)][0] // empty' "$D/tasks.json" || exit 3 ;;
  whoami) [ -f "$D/whoami" ] || exit 4; cat "$D/whoami" ;;
  decisions) cat "$D/$1.decisions" 2>/dev/null || echo '[]' ;;
  list-branches) cat "$D/$1.registry" 2>/dev/null || echo '[]' ;;
  *) echo "memo: write verb $verb" >&2; exit 1 ;;
esac
EOF2
cat >"$S/sessions/fake.sh" <<EOF2
#!/usr/bin/env bash
[ -f "$TMP/sessions.json" ] || exit 1
cat "$TMP/sessions.json"
EOF2
chmod +x "$S/tracker/memo.sh" "$S/sessions/fake.sh"
export BATON_SESSIONS_TYPE=fake

DB="$TMP/tracker"; mkdir -p "$DB"
CTXF="$TMP/ctx.json"
jq -n --arg d "$DB" --arg w "$TMP" '{name:"t", _workspace:$w, task_tracking:{type:"memo", dir:$d}}' >"$CTXF"

NOW=2026-06-15T12:00:00Z
RUN() { "$S/sweep-check.sh" --context "$CTXF" --now "$NOW" --today 2026-06-15 "$@"; }
reset() { rm -f "$DB"/*; echo '[]' >"$TMP/sessions.json"; }
# tasks <json array of partial tasks>: fills in the fixed fields.
tasks() {
  jq -c 'map({title: ("Title " + .id), status: "open", labels: [], assignee: null, state: null,
              updated_at: "2026-06-15T11:00:00Z"} + .)' <<<"$1" >"$DB/tasks.json"
}
kinds() { RUN --format json "$@" | jq -r '[.[] | .kind + ":" + .id] | join(" ")'; }

# Real worktrees, with and without a carrier.
git init -q -b main "$TMP/repo"; git -C "$TMP/repo" commit -q --allow-empty -m x
wt() { git -C "$TMP/repo" worktree add -q -b "b-$1" "$TMP/wt-$1" 2>/dev/null
       "$S/task-identity.sh" --write-carrier "$TMP/wt-$1" --leaf "$1" --slug s --branch "b-$1" --context "$CTXF" >/dev/null 2>&1; }
wt t-1; wt t-closed; wt other-9
git -C "$TMP/repo" worktree add -q -b plain "$TMP/plain" 2>/dev/null
sess() { jq -n --arg c "$1" --arg s "$2" --arg w "${3:-}" \
  '{id:"s1", name:"n1", cwd:$c, status:$s, waiting_for:(if $w == "" then null else $w end)}'; }

echo "== nothing to report"
reset; tasks '[{"id":"t-1"}]'
OUT="$(RUN)"; _eq "silent" "$OUT" ""; _eq "exit 0" "$(RUN >/dev/null; echo $?)" 0
tasks '[]'; _eq "no tasks at all" "$(RUN; echo $?)" 0

echo "== answered"
reset; tasks '[{"id":"t-1","labels":["needs-decision"],"state":{"state":"waiting-on-me"}}]'
echo '[{"id":"d1aaaaa","status":"answered","answered_at":"2026-06-15T09:00:00Z","question":"Which\nway?"},
       {"id":"d2aaaaa","status":"open","question":"Other"}]' >"$DB/t-1.decisions"
OUT="$(RUN)"
_eq "one line" "$(wc -l <<<"$OUT" | tr -d ' ')" 1
_has "decision, age and one-line question" "$OUT" "answered t-1 decision d1aaaaa answered 3h ago, not resolved: Which way?"
_hasnt "an open decision is not a problem" "$OUT" d2aaaaa
tasks '[{"id":"t-1"}]'; _eq "only labelled tasks are asked" "$(RUN)" ""
tasks '[{"id":"t-1","labels":["needs-decision"],"status":"in_progress"}]'
echo '[]' >"$DB/t-1.decisions"; _eq "label without a record is unknown" "$(kinds)" "unknown:t-1"
echo decisions >"$DB/unsupported"; _eq "a backend without decisions is unknown" "$(kinds)" "unknown:t-1"
rm "$DB/unsupported"; echo decisions >"$DB/fail"
_eq "a failed read is unreadable" "$(kinds)" "unreadable:t-1"; RUN >/dev/null; _eq "and exits 2" "$?" 2
rm "$DB/fail"; echo 'garbage' >"$DB/t-1.decisions"; _eq "garbage is unreadable" "$(kinds)" "unreadable:t-1"

echo "== prompt"
reset; tasks '[{"id":"t-1","state":{"state":"working","at":"2026-06-15T11:00:00Z"}},{"id":"t-closed","status":"closed"}]'
jq -s . <(sess "$TMP/wt-t-1" waiting "permission prompt") >"$TMP/sessions.json"
OUT="$(RUN)"; _has "waiting session" "$OUT" "prompt t-1 session n1 is waiting at a prompt (permission prompt)"
mkdir -p "$TMP/wt-t-1/sub"; jq -s . <(sess "$TMP/wt-t-1/sub" waiting) >"$TMP/sessions.json"
_eq "from a subdirectory" "$(kinds)" "prompt:t-1"
jq -s . <(sess "$TMP/wt-t-1" idle) >"$TMP/sessions.json"; _eq "an idle session is not" "$(kinds)" ""
jq -s . <(sess "$TMP/wt-t-closed" waiting) >"$TMP/sessions.json"; _eq "a closed task's is" "$(kinds)" "prompt:t-closed"
jq -s . <(sess "$TMP/wt-other-9" waiting) >"$TMP/sessions.json"; _eq "another context's is not" "$(kinds)" ""
echo get >"$DB/fail"; _eq "unless its task could not be read" "$(kinds)" "unreadable:other-9"; rm "$DB/fail"
jq -s . <(sess "$TMP/plain" waiting) <(sess "$TMP/repo" waiting) <(sess "$TMP/gone" waiting) <(sess "" waiting) >"$TMP/sessions.json"
_eq "a session outside a task worktree is not" "$(kinds)" ""
tasks '[{"id":"t-1","assignee":"Sam"}]'; echo '{"user":"Pat"}' >"$DB/whoami"
jq -s . <(sess "$TMP/wt-t-1" waiting) >"$TMP/sessions.json"; _eq "somebody else's task still counts" "$(kinds)" "prompt:t-1"

echo "== overdue"
reset; tasks '[{"id":"a","state":{"state":"blocked","until":"2026-06-14"}},
               {"id":"b","state":{"state":"waiting-until","until":"2026-06-15"}},
               {"id":"c","state":{"state":"waiting-until","until":"2026-01-02"}},
               {"id":"d","state":{"state":"paused","until":"2026-01-02"}},
               {"id":"e","state":{"state":"blocked","until":"soon"}}]'
_eq "past only; today, paused and malformed are not" "$(kinds)" "overdue:a overdue:c"
tasks '[{"id":"p","labels":["keep-task-open"],"status":"in_progress"},{"id":"q","labels":["keep-task-open"]}]'
echo '[{"branch":"x","keep_task_open":"yes","waiting_on":"the vendor","until":"2026-06-01"}]' >"$DB/p.registry"
OUT="$(RUN)"; _has "parked past its date" "$OUT" "overdue p parked, waiting on the vendor (until 2026-06-01)"
_eq "only the overdue one" "$(kinds)" "overdue:p"
tasks '[{"id":"p","labels":["keep-task-open"],"state":{"state":"waiting-until","until":"2026-06-02"}}]'
_eq "declared and parked is one line" "$(kinds)" "overdue:p"
tasks '[{"id":"q","labels":["keep-task-open"]}]'; echo list-branches >"$DB/fail"
_eq "an unreadable registry" "$(kinds)" "unreadable:q"

echo "== stalled"
reset; tasks '[{"id":"t-1","state":{"state":"working","at":"2026-06-15T07:00:00Z"},"updated_at":"2026-06-15T07:30:00Z"}]'
_eq "no session, 4.5h" "$(kinds)" "stalled:t-1"
_eq "--stalled-after" "$(kinds --stalled-after 5)" ""
jq -s . <(sess "$TMP/wt-t-1" idle) >"$TMP/sessions.json"; _eq "a live session" "$(kinds)" ""
echo '[]' >"$TMP/sessions.json"
tasks '[{"id":"t-1","state":{"state":"working","at":"2026-06-15T07:00:00Z"},"updated_at":"2026-06-15T08:00:00.123Z"}]'
_eq "exactly 4h is not, fractions parse" "$(kinds)" ""
tasks '[{"id":"t-1","state":{"state":"working","at":"2026-06-15T11:59:00Z"},"updated_at":"2026-06-01T08:00:00Z"}]'
_eq "a fresh declaration counts" "$(kinds)" ""
tasks '[{"id":"t-1","state":{"state":"working"},"updated_at":"whenever"}]'
_eq "no readable time is stalled" "$(kinds)" "stalled:t-1"
tasks '[{"id":"t-1","state":{"state":"paused","at":"2026-01-01T00:00:00Z"},"updated_at":"2026-01-01T00:00:00Z"}]'
_eq "paused never stalls" "$(kinds)" ""

echo "== unknown"
reset; tasks '[{"id":"t-1","status":"in_progress"}]'
_eq "in progress, undeclared" "$(kinds)" "unknown:t-1"
jq -s . <(sess "$TMP/wt-t-1" working) >"$TMP/sessions.json"; _eq "with a session is not" "$(kinds)" ""
echo '[]' >"$TMP/sessions.json"
tasks '[{"id":"t-1","status":"in_progress","labels":["ready-for-worktree-delete"]}]'; _eq "parked is not" "$(kinds)" ""
tasks '[{"id":"t-1","state":{"state":"unknown"}}]'; _eq "unreadable state" "$(kinds)" "unknown:t-1"
tasks '[{"id":"t-1","status":"unknown"}]'; _eq "unreadable status" "$(kinds)" "unknown:t-1"
rm "$TMP/sessions.json"; tasks '[{"id":"t-1","status":"in_progress"}]'
_eq "no session list: said, and judged as none live" "$(kinds)" "unreadable:- unknown:t-1"
RUN >/dev/null; _eq "and exits 2" "$?" 2

echo "== backlog"
reset; tasks '[{"id":"a","updated_at":"2026-06-01T11:00:00Z"},{"id":"b","updated_at":"2026-06-01T12:00:00Z"},
  {"id":"c","updated_at":"2026-01-01T00:00:00Z","status":"blocked"},
  {"id":"d","updated_at":"2026-01-01T00:00:00Z","state":{"state":"paused"}},
  {"id":"e","updated_at":"2026-01-01T00:00:00Z","labels":["keep-task-open"]}]'
_eq "oldest first; 14d exactly, declared and parked are not" "$(kinds)" "backlog:c backlog:a"
_eq "--stale-after" "$(kinds --stale-after 60)" "backlog:c"
OUT="$(RUN --backlog summary)"; _eq "--backlog summary" "$OUT" "backlog - 2 tasks untouched too long; oldest: c, a"

echo "== whose tasks"
reset; tasks '[{"id":"a","assignee":"Sam","status":"in_progress"},{"id":"b","assignee":" pat ","status":"in_progress"},
  {"id":"c","status":"in_progress"},{"id":"d","assignee":"Sam","status":"in_progress","labels":["needs-decision"]}]'
_eq "whoami unknown: everyone's" "$(kinds)" "unknown:a unknown:b unknown:c unknown:d"
echo '{"user":"Pat"}' >"$DB/whoami"; _eq "mine, unassigned, needs-decision" "$(kinds)" "unknown:b unknown:c unknown:d"

echo "== order, hostile text, read-only"
reset; tasks '[{"id":"z","updated_at":"2026-01-01T00:00:00Z"},{"id":"y","status":"in_progress"},
  {"id":"x","state":{"state":"blocked","until":"2026-01-01"},"title":"a\nprompt evil \u001b[31m$(touch '"$TMP"'/pwned)"},
  {"id":"w","state":{"state":"working"},"updated_at":"2026-06-01T00:00:00Z"}]'
_eq "most pressing first" "$(kinds)" "overdue:x stalled:w unknown:y backlog:z"
OUT="$(RUN)"; _eq "a newline in a title adds no line" "$(wc -l <<<"$OUT" | tr -d ' ')" 4
_hasnt "no escape sequences" "$OUT" $'\033'
_eq "nothing executed" "$([ -e "$TMP/pwned" ] && echo yes || echo no)" no
_eq "read verbs only" "$(awk '{print $1}' "$DB/calls" | grep -vxE 'list|whoami|get|decisions|list-branches' | sort -u)" ""
# A worktree named the pre-carrier way, which an identity read may backfill.
git -C "$TMP/repo" worktree add -q -b t-7-old "$TMP/t-7-old" 2>/dev/null
tasks '[{"id":"t-7"}]'; jq -s . <(sess "$TMP/t-7-old" waiting) >"$TMP/sessions.json"
_eq "a legacy worktree resolves" "$(kinds)" "prompt:t-7"
_eq "and its carrier is not backfilled" "$(ls "$TMP"/repo/.git/worktrees/t-7-old/baton-identity 2>/dev/null)" ""

echo "== cannot run"
reset; echo list >"$DB/fail"; OUT="$(RUN)"; RC=$?
_has "a failed list is said" "$OUT" "unreadable - the task list could not be read"; _eq "exit 2" "$RC" 2
rm "$DB/fail"; echo '[{"status":"open"}]' >"$DB/tasks.json"; RUN >/dev/null 2>&1; _eq "tasks without ids are exit 2" "$?" 2
RUN --format xml >/dev/null 2>&1; _eq "bad format" "$?" 1
RUN --now yesterday >/dev/null 2>&1; _eq "bad --now" "$?" 1
"$S/sweep-check.sh" --context "$TMP/nope" >/dev/null 2>&1; _eq "no context" "$?" 1
tasks '[]'; _eq "json with nothing" "$(RUN --format json)" "[]"
_eq "context on stdin" "$("$S/sweep-check.sh" --context - <"$CTXF"; echo $?)" 0

echo "== sessions.sh"
mkdir -p "$TMP/bin"; printf '#!/bin/sh\n[ "$*" = "agents --json" ] || exit 9\ncat "%s/agents"\n' "$TMP" >"$TMP/bin/claude"; chmod +x "$TMP/bin/claude"
export BATON_SESSIONS_TYPE=claude-code
L() { PATH="$TMP/bin:$PATH" "$S/sessions.sh" list 2>/dev/null; }
cat >"$TMP/agents" <<'EOF2'
[{"sessionId":"1","cwd":"/a","name":"n","status":"waiting","waitingFor":"permission prompt"},
 {"sessionId":"2","cwd":"/a","status":"busy"},{"sessionId":"3","cwd":"/a","status":"idle"},
 {"id":"4","cwd":"/a","state":"blocked"},{"sessionId":"5","cwd":"/a","status":"new-thing"},{"sessionId":"6"}]
EOF2
_eq "status mapping" "$(L | jq -r '[.[].status] | join(" ")')" "waiting working idle waiting unknown unknown"
_eq "waiting_for" "$(L | jq -r '.[0].waiting_for')" "permission prompt"
_eq "a missing cwd is empty" "$(L | jq -r '.[5].cwd')" ""
echo '[]' >"$TMP/agents"; _eq "none live" "$(L)" "[]"
echo 'Not logged in' >"$TMP/agents"; OUT="$(L)"; _eq "not JSON fails" "$?$OUT" 1
rm "$TMP/agents"; OUT="$(L)"; _eq "a failing claude fails" "$?$OUT" 1
BATON_SESSIONS_TYPE=nope "$S/sessions.sh" list >/dev/null 2>&1; _eq "no such backend" "$?" 4
BATON_SESSIONS_TYPE=../x "$S/sessions.sh" list >/dev/null 2>&1; _eq "bad backend name" "$?" 1

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
