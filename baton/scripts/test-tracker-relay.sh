#!/usr/bin/env bash
# baton — tests for scripts/tracker-relay.sh: relaying tracker writes from a session that cannot
# push its tracker, through a git branch, to a machine that can.
#
# Hermetic: the tracker is a file-backed fake backend (`memo`) dropped next to a private copy of
# the scripts, and the "tracker repo" is a pair of clones of a local bare remote — one playing the
# cloud session, one the Mac. Nothing touches a real tracker or the network. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }
command -v git >/dev/null 2>&1 || { echo "git is required" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }
_has() { case "$2" in *"$3"*) _ok "$1" ;; *) _bad "$1" "'$2' does not contain '$3'" ;; esac; }
_hasnt() { case "$2" in *"$3"*) _bad "$1" "'$2' contains '$3'" ;; *) _ok "$1" ;; esac; }

unset CLAUDE_CODE_REMOTE BATON_TRACKER_RELAY BATON_RELAY_SESSION BEADS_DIR
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

S="$TMP/scripts"; mkdir -p "$S/tracker"
cp "$HERE/tracker.sh" "$HERE/tracker-sync.sh" "$HERE/tracker-relay.sh" "$S/"
cp "$HERE/tracker/lib-registry.sh" "$S/tracker/"

# =============================================================================================
# `memo`: a fake backend. One JSON file per task under $BATON_TRACKER_DIR/tasks. Like a real
# tool it refuses what it cannot do: closing a closed task, an unknown id, an unknown flag. Every
# call is logged to $BATON_TRACKER_DIR/calls.
# =============================================================================================
cat >"$S/tracker/memo.sh" <<'EOF'
#!/usr/bin/env bash
set -uo pipefail
D="${BATON_TRACKER_DIR:?}"; T="$D/tasks"; mkdir -p "$T"
printf '%s\n' "$*" >>"$D/calls"
die() { echo "memo: $*" >&2; exit 1; }
verb="$1"; shift
f() { printf '%s/%s.json' "$T" "$1"; }
need() { [ -f "$(f "$1")" ] || die "no such task $1"; }
put() { jq -c "$2" "$(f "$1")" >"$(f "$1").t" && mv "$(f "$1").t" "$(f "$1")"; }
case "$verb" in
  get)  [ -f "$(f "${1:-}")" ] || exit 3; cat "$(f "$1")" ;;
  list) cat "$T"/*.json 2>/dev/null | jq -s . ;;
  create)
    title="$1"; shift; desc=""; parent=""; labels=""
    while [ $# -gt 0 ]; do case "$1" in
      --description) desc="$2" ;; --parent) parent="$2"; need "$2" ;; --labels) labels="$2" ;;
      --priority) ;; *) die "create: unknown flag $1" ;; esac; shift 2; done
    n=$(( $(cat "$D/counter" 2>/dev/null || echo 0) + 1 )); echo "$n" >"$D/counter"
    id="$(cat "$D/prefix")-$n"
    jq -n --arg id "$id" --arg t "$title" --arg d "$desc" --arg p "$parent" --arg l "$labels" \
      '{id:$id,title:$t,description:$d,notes:"",status:"open",assignee:null,
        labels:($l|split(",")|map(select(length>0))),parent:(if $p=="" then null else $p end),
        close_reason:null,links:[],branches:[]}' >"$(f "$id")"
    echo "$id" ;;
  update)
    id="$1"; need "$id"; shift
    while [ $# -gt 0 ]; do case "$1" in
      --status) put "$id" ".status=\"$2\"" ;; --title) put "$id" ".title=$(jq -Rn --arg v "$2" '$v')" ;;
      --description) put "$id" ".description=$(jq -n --arg v "$2" '$v')" ;; --priority) ;;
      *) die "update: unknown flag $1" ;; esac; shift 2; done ;;
  claim) need "$1"; put "$1" ".status=\"in_progress\" | .assignee=\"${MEMO_USER:-me}\"" ;;
  close)
    need "$1"; [ "$(jq -r .status "$(f "$1")")" != closed ] || die "$1 is already closed"
    [ "${2:-}" = --reason ] || die "close needs --reason"
    put "$1" ".status=\"closed\" | .close_reason=$(jq -n --arg v "$3" '$v')" ;;
  reopen) need "$1"; put "$1" '.status="open" | .close_reason=null' ;;
  label-add) need "$1"; put "$1" ".labels=(.labels + [$(jq -n --arg v "$2" '$v')] | unique)" ;;
  label-remove) need "$1"; put "$1" ".labels=(.labels - [$(jq -n --arg v "$2" '$v')])" ;;
  link) need "$1"; need "$2"; put "$1" ".links += [{to:\"$2\",type:\"${4:-blocks}\"}]" ;;
  note)
    need "$1"; case "$2" in -*) die "note text looks like a flag" ;; esac
    put "$1" ".notes = (if .notes == \"\" then \"\" else .notes + \"\n\" end) + $(jq -n --arg v "$2" '$v')" ;;
  record-branch|update-branch) need "$1"; id="$1"; shift; put "$id" ".branches += [$(jq -cn '$ARGS.positional' --args -- "$@")]" ;;
  remote) echo '{"configured":true,"remote":"file:///memo"}' ;;
  sync) echo "synced $*" ;;
  sync-status) echo '{"ahead":0,"behind":0}' ;;
  *) exit 4 ;;
esac
EOF

TRK="$S/tracker.sh"; RELAY="$S/tracker-relay.sh"; SYNC="$S/tracker-sync.sh"

# A bare "tracker repo" remote, and two clones of it. Each clone's .trk is that machine's tracker.
git init -q --bare "$TMP/remote.git"
git clone -q "$TMP/remote.git" "$TMP/seed" 2>/dev/null
git -C "$TMP/seed" commit -q --allow-empty -m init && git -C "$TMP/seed" push -q origin HEAD:main 2>/dev/null
git -C "$TMP/remote.git" symbolic-ref HEAD refs/heads/main

_machine() { # $1 = name, $2 = id prefix
  git clone -q "$TMP/remote.git" "$TMP/$1" 2>/dev/null
  mkdir -p "$TMP/$1/.trk/tasks"; echo "$2" >"$TMP/$1/.trk/prefix"
  jq -n --arg d "$TMP/$1/.trk" '{task_tracking:{type:"memo",dir:$d}}' >"$TMP/$1.ctx"
}
_seed() { # $1 = machine, $2 = id, $3 = status, [$4 = labels csv], [$5 = assignee]
  jq -n --arg id "$2" --arg s "$3" --arg l "${4:-}" --arg a "${5:-}" \
    '{id:$id,title:"task \($id)",description:"",notes:"",status:$s,assignee:(if $a=="" then null else $a end),
      labels:($l|split(",")|map(select(length>0))),parent:null,close_reason:(if $s=="closed" then "earlier" else null end),
      links:[],branches:[]}' >"$TMP/$1/.trk/tasks/$2.json"
}
_task()  { cat "$TMP/$1/.trk/tasks/$2.json"; }
_field() { jq -r "$3" "$TMP/$1/.trk/tasks/$2.json"; }
_snap()  { cat "$TMP/$1/.trk/tasks/"*.json; }
_remote_branches() { git -C "$TMP/remote.git" for-each-ref --format='%(refname:short)' 'refs/heads/tracker-relay/'; }

# The cloud session: relay mode via the real trigger, CLAUDE_CODE_REMOTE.
_cloud() { CLAUDE_CODE_REMOTE=true BATON_RELAY_SESSION="${SID:-sess1}" "$TRK" --context "$TMP/cloud.ctx" "$@"; }
_apply() { OUT="$("$RELAY" apply --context "$TMP/mac.ctx" 2>"$TMP/err")"; RC=$?; ERR="$(cat "$TMP/err")"; }
_relay_ops() { git -C "$TMP/$1" cat-file blob "refs/heads/tracker-relay/$2:relay.jsonl"; }

_machine cloud c; _machine mac m
for m in cloud mac; do
  _seed "$m" A open; _seed "$m" C open x; _seed "$m" D open old; _seed "$m" E open
done
_seed cloud B open; _seed mac B closed       # B was closed elsewhere while the cloud worked
_seed mac E in_progress "" someone-else     # …and E claimed by someone else

# =============================================================================================
echo "relay mode detection"
_eq "off by default"                      "$("$RELAY" active; echo $?)" 1
_eq "on with CLAUDE_CODE_REMOTE=true"     "$(CLAUDE_CODE_REMOTE=true "$RELAY" active; echo $?)" 0
_eq "BATON_TRACKER_RELAY=off overrides"   "$(CLAUDE_CODE_REMOTE=true BATON_TRACKER_RELAY=off "$RELAY" active; echo $?)" 1
_eq "BATON_TRACKER_RELAY=on forces it"    "$(BATON_TRACKER_RELAY=on "$RELAY" active; echo $?)" 0

echo "no cloud, no relay"
"$TRK" --context "$TMP/mac.ctx" label-add D nocloud
_eq "a write outside relay mode records nothing" \
    "$(git -C "$TMP/mac" for-each-ref 'refs/heads/tracker-relay/' | wc -l | tr -d ' ')" 0
_apply
_eq "apply with no relay branches: exit 0" "$RC" 0
_eq "apply with no relay branches: silent" "$OUT$ERR" ""
"$TRK" --context "$TMP/mac.ctx" label-remove D nocloud

# =============================================================================================
echo "cloud side: writes apply locally AND are recorded"
printf 'Rewritten\nbody\n' >"$TMP/desc.txt"
_cloud claim A
_cloud note A "cloud note"
_cloud label-add A foo
NEW="$(_cloud create "Child of A" --parent A --labels bar)"
_eq  "create still prints the local id"   "$NEW" "c-1"
_cloud label-add "$NEW" baz
_cloud link "$NEW" C --type relates-to
_cloud close B --reason "done in cloud"
_cloud close C --reason "finished"
_cloud update A --description-file "$TMP/desc.txt"
_cloud label-remove D old
_cloud claim E
_eq  "local apply: A claimed in the cloud's own tracker" "$(_field cloud A .status)" in_progress
_eq  "local apply: the child exists in the cloud's tracker" "$(_field cloud c-1 .parent)" A

OPS="$(_relay_ops cloud sess1)"
_eq  "one op per write"                   "$(grep -c . <<<"$OPS")" 11
_eq  "op ids are session/seq"             "$(jq -r .id <<<"$OPS" | tr '\n' ' ')" \
     "sess1/1 sess1/2 sess1/3 sess1/4 sess1/5 sess1/6 sess1/7 sess1/8 sess1/9 sess1/10 sess1/11 "
_eq  "create carries a handle and its local id" \
     "$(jq -rs '.[3] | "\(.handle) \(.local_id)"' <<<"$OPS")" "relay:4 c-1"
_eq  "a later op names the create by handle, not local id" \
     "$(jq -rs '.[4].args[0]' <<<"$OPS")" "relay:4"
_eq  "…in link's first position too"      "$(jq -rs '.[5].args[0:2] | join(" ")' <<<"$OPS")" "relay:4 C"
_eq  "--description-file is recorded as its text" \
     "$(jq -rs '.[8].args | join("|")' <<<"$OPS")" "A|--description|Rewritten"$'\n'"body"
_eq  "the recorded file passes its own validation" \
     "$("$RELAY" validate --session sess1 - <<<"$OPS" >/dev/null; echo $?)" 0

echo "cloud side: a write that cannot be relayed says so"
ERR="$(_cloud label-add A "-flaglike" 2>&1 >/dev/null)"
_has "warned that it was not relayed"     "$ERR" "NOT relayed"
_eq  "…and nothing was recorded"          "$(_relay_ops cloud sess1 | grep -c .)" 11

echo "cloud side: push sends the relay branch, not the tracker"
: >"$TMP/cloud/.trk/calls"
OUT="$(CLAUDE_CODE_REMOTE=true BATON_RELAY_SESSION=sess1 "$SYNC" --context "$TMP/cloud.ctx" push 2>&1)"; RC=$?
_eq  "push: exit 0"                       "$RC" 0
_has "push: reports the relay"            "$OUT" "relayed 11 write(s) on tracker-relay/sess1"
_eq  "the branch is on the remote"        "$(_remote_branches)" "tracker-relay/sess1"
_hasnt "the tracker's own push is not attempted" "$(cat "$TMP/cloud/.trk/calls")" "sync"

# =============================================================================================
echo "mac side: apply"
_apply
_eq  "exit 0"                             "$RC" 0
_has "one line, with counts"              "$OUT" "tracker relay sess1: 9 applied, 2 skipped"
_has "the closed-elsewhere close is skipped with a reason" "$OUT" "sess1/7 close: B is already closed"
_has "the claimed-elsewhere claim is skipped with a reason" "$OUT" "sess1/11 claim: E is already claimed by someone-else"
_eq  "claim landed"                       "$(_field mac A '.status + " " + .assignee')" "in_progress me"
_has "note landed"                        "$(_field mac A .notes)" "cloud note"
_has "label landed"                       "$(_field mac A '.labels | join(",")')" "foo"
_eq  "description landed"                 "$(_field mac A .description)" "Rewritten"$'\n'"body"
_eq  "close landed"                       "$(_field mac C '.status + " " + .close_reason')" "closed finished"
_eq  "label-remove landed"                "$(_field mac D '.labels | join(",")')" ""
_eq  "conflicting close left B alone"     "$(_field mac B .close_reason)" "earlier"
_eq  "conflicting claim left E alone"     "$(_field mac E .assignee)" "someone-else"
CHILD="$(jq -r 'select(.title == "Child of A") | .id' "$TMP/mac/.trk/tasks/"*.json)"
_eq  "create landed with a Mac id"        "$CHILD" "m-1"
_eq  "…under its parent"                  "$(_field mac m-1 .parent)" A
_eq  "…and the handle resolved for later ops" "$(_field mac m-1 '.labels | join(",")')" "bar,baz"
_eq  "…link included"                     "$(_field mac m-1 '.links[0].to')" C
_has "provenance names the cloud session" "$(_field mac A .notes)" "Relayed from cloud session sess1"
_has "provenance on the created task"     "$(_field mac m-1 .notes)" "[relay-op:sess1/4]"
_has "provenance on a closed task"        "$(_field mac C .notes)" "[relay-op:sess1/8]"
_eq  "a relayed note carries its marker in the same write" \
     "$(_field mac A .notes | grep -c 'cloud note')" 1
_hasnt "a skipped op leaves no marker"    "$(_field mac B .notes)" "relay-op"
_eq  "the relay branch is gone"           "$(_remote_branches)" ""
_eq  "no pending ref left behind"         "$(git -C "$TMP/mac" for-each-ref refs/baton-relay/ | wc -l | tr -d ' ')" 0

echo "idempotency: the same relay again changes nothing"
BEFORE="$(_snap mac)"
git -C "$TMP/cloud" push -q origin refs/heads/tracker-relay/sess1:refs/heads/tracker-relay/sess1
_apply
_has "every op is skipped"                "$OUT" "0 applied, 11 skipped"
_has "…as already applied"                "$OUT" "sess1/1 claim: already applied"
_has "…including the create"              "$OUT" "sess1/4 create: already applied (as m-1)"
_eq  "the tracker is byte-for-byte unchanged" "$(_snap mac)" "$BEFORE"
_eq  "the branch is gone again"           "$(_remote_branches)" ""

echo "a run that died part-way resumes from its pending ref"
SID=sess2 _cloud label-add A resumed
git -C "$TMP/mac" fetch -q "$TMP/cloud" "refs/heads/tracker-relay/sess2:refs/baton-relay/pending/sess2"
_apply
_has "the pending relay is applied"       "$OUT" "tracker relay sess2 (resumed): 1 applied, 0 skipped"
_has "…for real"                          "$(_field mac A '.labels | join(",")')" "resumed"
_eq  "…and its ref removed"               "$(git -C "$TMP/mac" for-each-ref refs/baton-relay/ | wc -l | tr -d ' ')" 0

echo "dependents of a create that did not apply are skipped, the rest still apply"
SID=sess3 _cloud create "Orphan" --parent A >/dev/null
SID=sess3 _cloud label-add c-2 orphaned
SID=sess3 _cloud label-add A fine
rm "$TMP/mac/.trk/tasks/A.json.bak" 2>/dev/null
mv "$TMP/mac/.trk/tasks/A.json" "$TMP/A.hold"         # the parent is gone on the Mac
SID=sess3 CLAUDE_CODE_REMOTE=true BATON_RELAY_SESSION=sess3 "$SYNC" --context "$TMP/cloud.ctx" push >/dev/null 2>&1
_apply
mv "$TMP/A.hold" "$TMP/mac/.trk/tasks/A.json"
_has "the create is skipped"              "$OUT" "sess3/1 create: no such parent A"
_has "its dependent is skipped, naming the handle" "$OUT" "sess3/2 label-add: depends on relay:1"
_has "an op on a missing task is skipped" "$OUT" "sess3/3 label-add: no such task A"

# =============================================================================================
echo "malformed relays are reported and left alone; session-start carries on"
_bad_relay() { # $1 = session, $2 = file content
  local blob tree c
  blob="$(printf '%s' "$2" | git -C "$TMP/seed" hash-object -w --stdin)"
  tree="$(printf '100644 blob %s\trelay.jsonl\n' "$blob" | git -C "$TMP/seed" mktree)"
  c="$(git -C "$TMP/seed" commit-tree "$tree" -m bad)"
  git -C "$TMP/seed" push -q origin "$c:refs/heads/tracker-relay/$1"
}
_bad_relay junk 'this is not json'
_bad_relay evil '{"v":1,"id":"evil/1","ts":"t","session":"evil","verb":"exec","args":["rm","-rf","/"]}'
SID=sess4 _cloud label-add C good
SID=sess4 CLAUDE_CODE_REMOTE=true BATON_RELAY_SESSION=sess4 "$SYNC" --context "$TMP/cloud.ctx" push >/dev/null 2>&1
BEFORE="$(_snap mac)"
_apply
_eq  "exit 0"                             "$RC" 0
_has "garbage is reported"                "$OUT" "tracker relay junk: MALFORMED"
_has "a non-whitelisted verb is reported" "$OUT" 'verb "exec" is not a relayable tracker write'
_has "a good relay in the same run still applies" "$OUT" "tracker relay sess4: 1 applied"
_eq  "malformed branches are left in place" "$(_remote_branches | tr '\n' ' ')" "tracker-relay/evil tracker-relay/junk "
_eq  "nothing but the good op changed the tracker" \
     "$(diff <(printf '%s\n' "$BEFORE") <(_snap mac) | grep -c '^>')" 1
_apply
_has "reported again on the next run"     "$OUT" "tracker relay junk: MALFORMED"
git -C "$TMP/remote.git" update-ref -d refs/heads/tracker-relay/junk
git -C "$TMP/remote.git" update-ref -d refs/heads/tracker-relay/evil

echo "a relay that grows between fetch and claim is not taken"
SID=sess6 _cloud label-add C first
CLAUDE_CODE_REMOTE=true BATON_RELAY_SESSION=sess6 "$SYNC" --context "$TMP/cloud.ctx" push >/dev/null 2>&1
SID=sess6 _cloud label-add C second
GROWN="$(git -C "$TMP/cloud" rev-parse refs/heads/tracker-relay/sess6)"
git -C "$TMP/cloud" push -q "$TMP/remote.git" "$GROWN:refs/heads/tracker-relay/sess6-grown"
# A git wrapper that lets the cloud's next push land just before apply's claim.
mkdir -p "$TMP/racebin"; REAL_GIT="$(command -v git)"
cat >"$TMP/racebin/git" <<EOF
#!/usr/bin/env bash
case "\$*" in *force-with-lease*sess6*) "$REAL_GIT" -C "$TMP/remote.git" update-ref refs/heads/tracker-relay/sess6 $GROWN ;; esac
exec "$REAL_GIT" "\$@"
EOF
chmod +x "$TMP/racebin/git"
git -C "$TMP/remote.git" update-ref -d refs/heads/tracker-relay/sess6-grown
BEFORE="$(_snap mac)"
OUT="$(PATH="$TMP/racebin:$PATH" "$RELAY" apply --context "$TMP/mac.ctx" 2>&1)"
_has "the claim is refused and reported"  "$OUT" "tracker relay sess6: could not take the branch"
_eq  "nothing from it was applied"        "$(_snap mac)" "$BEFORE"
_eq  "the grown branch is still there"    "$(git -C "$TMP/remote.git" rev-parse refs/heads/tracker-relay/sess6)" "$GROWN"
_apply
_has "the next run applies all of it"     "$OUT" "tracker relay sess6: 2 applied"

echo "a remote that refuses the claim leaves the relay unapplied"
SID=sess7 _cloud label-add C refused
CLAUDE_CODE_REMOTE=true BATON_RELAY_SESSION=sess7 "$SYNC" --context "$TMP/cloud.ctx" push >/dev/null 2>&1
printf '#!/bin/sh\nwhile read o n r; do case "$n" in 0000000000000000000000000000000000000000) exit 1;; esac; done\n' \
  >"$TMP/remote.git/hooks/pre-receive"; chmod +x "$TMP/remote.git/hooks/pre-receive"
BEFORE="$(_snap mac)"
_apply
_has "reported"                           "$OUT" "tracker relay sess7: could not take the branch"
_eq  "nothing applied"                    "$(_snap mac)" "$BEFORE"
rm "$TMP/remote.git/hooks/pre-receive"
_apply
_has "applied once the remote allows it"  "$OUT" "tracker relay sess7: 1 applied"

echo "apply never blocks"
git -C "$TMP/mac" remote set-url origin "$TMP/nonexistent.git"
_apply
_eq  "unreachable remote: exit 0"         "$RC" 0
_has "…and says so"                       "$ERR" "could not list relay branches"
git -C "$TMP/mac" remote set-url origin "$TMP/remote.git"
OUT="$("$RELAY" apply --context - <<<'not json' 2>&1)"; RC=$?
_eq  "no context: exit 0"                 "$RC" 0
SID=sess5 _cloud label-add C later
CLAUDE_CODE_REMOTE=true BATON_RELAY_SESSION=sess5 "$SYNC" --context "$TMP/cloud.ctx" push >/dev/null 2>&1
OUT="$(CLAUDE_CODE_REMOTE=true "$RELAY" apply --context "$TMP/mac.ctx" 2>&1)"
_eq  "a relaying session never applies"   "$OUT|$(_remote_branches)" "|tracker-relay/sess5"

# =============================================================================================
echo "validation: op parsing"
OP() { jq -cn --arg v "$1" --argjson a "$2" '{v:1,id:"s/1",ts:"t",session:"s",verb:$v,args:$a}'; }
_valid()   { "$RELAY" validate --session s - >/dev/null 2>&1; echo $?; }
_err()     { "$RELAY" validate --session s - 2>/dev/null | jq -r '.errors[0] // ""'; }
_eq  "a plain claim is valid"             "$(OP claim '["x-1"]' | _valid)" 0
_eq  "no trailing newline is still valid" "$(printf '%s' "$(OP claim '["x-1"]')" | _valid)" 0
_eq  "two ops, last line unterminated"    "$(printf '%s\n%s' "$(OP claim '["x-1"]')" "$(OP note '["x-1","hi"]' | jq -c '.id="s/2"')" \
                                             | "$RELAY" validate --session s - | jq '.ops | length')" 2
_has "an interior blank line is malformed" "$(printf '%s\n\n%s\n' "$(OP claim '["x-1"]')" "$(OP claim '["x-2"]' | jq -c '.id="s/2"')" | _err)" "line 2: not a JSON object"
_has "an empty file is malformed"         "$(printf '' | _err)" "empty relay file"
_has "--description-file is refused"      "$(OP update '["x-1","--description-file","/etc/passwd"]' | _err)" "not allowed"
_has "a flag in an id position is refused" "$(OP note '["--db=/x","hi"]' | _err)" "not a valid id"
_has "a flag-shaped text value is refused" "$(OP note '["x-1","--db=/x"]' | _err)" "not a valid text"
_has "a flag-shaped label is refused"     "$(OP label-add '["x-1","--force"]' | _err)" "not a valid label"
_has "a dangling flag is refused"         "$(OP close '["x-1","--reason"]' | _err)" "no value"
_has "a missing required flag is refused" "$(OP close '["x-1"]' | _err)" "--reason is required"
_has "a doubled flag is refused"          "$(OP close '["x-1","--reason","a","--reason","b"]' | _err)" "given twice"
_has "an unknown status is refused"       "$(OP update '["x-1","--status","bogus"]' | _err)" "not a valid status"
_has "update with nothing to change is refused" "$(OP update '["x-1"]' | _err)" "at least 1 flag"
_has "non-string args are refused"        "$(OP claim '[1]' | _err)" "array of strings"
_has "a NUL in text is refused"           "$(OP note '["x-1","a\u0000b"]' | _err)" "not a valid text"
_has "a newline in a branch word is refused" "$(OP record-branch '["x-1","--branch","a\nb"]' | _err)" "not a valid word"
_has "an undefined handle is refused"     "$(OP label-add '["relay:9","x"]' | _err)" "used before any create"
_has "a create without a handle is refused" "$(OP create '["T"]' | _err)" "handle"
_has "a foreign session is refused"       "$(OP claim '["x-1"]' | jq -c '.session="t" | .id="t/1"' | _err)" "does not match its branch"
_has "an op id from another session is refused" "$(OP claim '["x-1"]' | jq -c '.id="t/1"' | _err)" "bad op id"
_has "an unknown op version is refused"   "$(OP claim '["x-1"]' | jq -c '.v=2' | _err)" "unsupported op version"
_has "duplicate op ids are refused"       "$(printf '%s\n%s\n' "$(OP claim '["x-1"]')" "$(OP claim '["x-2"]')" | _err)" "duplicate op id"
_has "a handle defined twice is refused"  "$(printf '%s\n%s\n' "$(OP create '["A"]' | jq -c '.handle="relay:1"')" "$(OP create '["B"]' | jq -c '.id="s/2" | .handle="relay:1"')" | _err)" "defined twice"

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
