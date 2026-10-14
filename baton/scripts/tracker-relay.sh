#!/usr/bin/env bash
# baton — relay tracker writes from a session that cannot push its tracker, through a git branch.
#
#   tracker-relay.sh active
#   tracker-relay.sh record   --tracker <dir> [--result <text>] -- <verb> [args...]
#   tracker-relay.sh push     --tracker <dir>
#   tracker-relay.sh apply    [--context <file|->]
#   tracker-relay.sh validate [--session <id>] <file|->
#
# A Claude Code cloud session can push branches (refs/heads/*) and nothing else, so a tracker that
# syncs through its own refs (beads: refs/dolt/data) cannot push from there. Its writes still
# succeed locally and then vanish with the VM. In RELAY MODE, tracker.sh still applies each write
# locally (so later reads in the session see it) and also hands it to `record`, which appends it as
# one JSON op to `relay.jsonl` on the branch `tracker-relay/<session>` in the tracker's git repo.
# `push` sends that branch; tracker-sync.sh calls it in place of the tracker push. On a machine
# that can push, baton:session-start's align runs `apply`, which replays every relay branch's ops
# through tracker.sh and deletes the branch.
#
# RELAY MODE is on when BATON_TRACKER_RELAY is on|1|true, off when it is off|0|false, and
# otherwise on exactly when CLAUDE_CODE_REMOTE=true. `active` exits 0 when it is on.
#
# THE OP, one per line of relay.jsonl:
#   {"v":1, "id":"<session>/<n>", "ts":"<UTC>", "session":"<session>", "verb":"<verb>",
#    "args":[...], "handle":"relay:<n>", "local_id":"<id>"}
# `handle` and `local_id` appear on `create` only. A create has no real id until it is applied, so
# later ops refer to it as its handle; `record` rewrites the session's local id to the handle, and
# `apply` maps the handle to the id the create gets.
#
# The file comes from a VM, so `apply` treats it as data: `validate` checks the whole file against
# a whitelist of verbs, flags and value shapes before anything runs, and a file that fails is
# reported and left on its branch for a human. Ops reach the tracker only as tracker.sh verbs.
#
# APPLY, per relay:
#   1. Fetch the branch into refs/baton-relay/pending/<session> and validate it.
#   2. Claim it by deleting the remote branch with a lease on the fetched commit, so two machines
#      cannot both apply it and a relay that grew meanwhile is left for the next run.
#   3. Apply the ops in order. An op already applied, or one that conflicts with the tracker's
#      current state, is skipped with a reason and the rest carry on.
#   4. Delete the pending ref. A run that dies before this leaves the ref, and the next `apply`
#      resumes from it.
# Each applied op gets a note on its task naming the cloud session and carrying the marker
# `[relay-op:<op id>]`. The note is the provenance record, and the marker is how a re-run knows the
# op is already applied.
#
# `apply` never fails session-start: it exits 0 whatever happens to a relay, and prints one line
# per relay. With no relay branches it prints nothing.
#
# Requires: git, jq.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TRK="$HERE/tracker.sh"
BRANCH_NS="tracker-relay"
PENDING_NS="refs/baton-relay/pending"
RELAY_FILE="relay.jsonl"

_die() { echo "tracker-relay: $*" >&2; exit 1; }

command -v jq >/dev/null 2>&1 || _die "jq is not installed"

_active() {
  case "${BATON_TRACKER_RELAY:-}" in
    on|1|true) return 0 ;;
    off|0|false) return 1 ;;
  esac
  [ "${CLAUDE_CODE_REMOTE:-}" = true ]
}

_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# --- the op whitelist -------------------------------------------------------------------------
# One jq program validates a whole relay file. Input: the raw lines (jq -R). Output:
# {ok, errors:[...], ops:[...], session}. Positional and flag values are typed by kind; a text
# value may not start with '-', so no value can reach a backend tool as a flag.
_JQ_LIB='
def sid_re: "^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$";
def handle_re: "^relay:[0-9]{1,9}$";
def has_nul: index("\u0000") != null;
def valid($kind):
  if type != "string" then false
  elif $kind == "id" then test("^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$") or test(handle_re)
  elif $kind == "label" then test("^[A-Za-z0-9][A-Za-z0-9._:/+=-]{0,99}$")
  elif $kind == "labels" then (split(",") | length > 0 and all(test("^[A-Za-z0-9][A-Za-z0-9._:/+=-]{0,99}$")))
  elif $kind == "text" then (length > 0 and length <= 65536 and (startswith("-") | not) and (has_nul | not))
  elif $kind == "word" then (length > 0 and length <= 4096 and (startswith("-") | not) and (test("[\n\r]") | not) and (has_nul | not))
  elif $kind == "priority" then test("^[0-4]$")
  elif $kind == "status" then IN("open", "in_progress", "blocked", "closed")
  elif $kind == "edge" then IN("blocks", "parent-child", "relates-to", "discovered-from")
  elif $kind == "field" then (test("^[a-z_]{1,40}=") and (test("[\n\r]") | not) and (has_nul | not))
  else false end;

# verb -> positional kinds, allowed flags, required flags, and a trailing-rest kind
def spec:
  { "create":        {pos: ["text"],          flags: {"--description":"text","--parent":"id","--labels":"labels","--priority":"priority"}, req: []},
    "update":        {pos: ["id"],            flags: {"--status":"status","--priority":"priority","--title":"text","--description":"text"}, req: [], min: 1},
    "claim":         {pos: ["id"],            flags: {}, req: []},
    "reopen":        {pos: ["id"],            flags: {}, req: []},
    "close":         {pos: ["id"],            flags: {"--reason":"text"}, req: ["--reason"]},
    "label-add":     {pos: ["id","label"],    flags: {}, req: []},
    "label-remove":  {pos: ["id","label"],    flags: {}, req: []},
    "link":          {pos: ["id","id"],       flags: {"--type":"edge"}, req: []},
    "note":          {pos: ["id","text"],     flags: {}, req: []},
    "record-branch": {pos: ["id"],            flags: {"--repo":"word","--branch":"word","--worktree":"word","--status":"word","--created":"word"}, req: ["--branch"]},
    "update-branch": {pos: ["id","word"],     rest: "field"} };

# indices of args that name a task, so handles are mapped there and nowhere else
def idpos:
  if .verb == "create" then [.args | to_entries[] | select(.value == "--parent") | .key + 1]
  elif .verb == "link" then [0, 1]
  else [0] end;

def map_ids($m):
  . as $op
  | reduce (idpos[]) as $i ($op;
      (.args[$i] // null) as $a | if $a != null and ($m | has($a)) then .args[$i] = $m[$a] else . end);

# errors in one op args array against its verb spec
def arg_errors:
  . as $op | (spec[$op.verb]) as $s
  | ($s.pos | length) as $np
  | if ($op.args | length) < $np then ["needs \($np) positional argument(s)"]
    else
      ([range(0; $np) as $i | select(($op.args[$i] | valid($s.pos[$i])) | not)
         | "argument \($i + 1) is not a valid \($s.pos[$i])"])
      + ( $op.args[$np:] as $rest
          | if $s.rest then
              (if ($rest | length) == 0 then ["needs at least one \($s.rest)"] else [] end)
              + [$rest[] | select(valid($s.rest) | not) | "\(.) is not a valid \($s.rest)"]
            elif ($rest | length) % 2 != 0 then ["a flag has no value, or an extra argument"]
            else
              [range(0; $rest | length; 2) as $i | $rest[$i] as $f | $rest[$i + 1] as $v
                | if ($s.flags | has($f) | not) then "flag \($f | tostring) is not allowed for \($op.verb)"
                  elif ($v | valid($s.flags[$f]) | not) then "value for \($f) is not a valid \($s.flags[$f])"
                  else empty end]
              + ([range(0; $rest | length; 2) as $i | $rest[$i]] | group_by(.) | map(select(length > 1) | "flag \(.[0] | tostring) given twice"))
              + [$s.req[] | select(. as $r | [range(0; $rest | length; 2) as $i | $rest[$i]] | index($r) | not) | "\(.) is required"]
              + (if ($s.min // 0) > ($rest | length) / 2 then ["needs at least \($s.min) flag(s)"] else [] end)
            end )
    end;

def validate($want):
  reduce (to_entries[]) as $e ({errors: [], ops: [], ids: {}, handles: {}, session: null};
    ($e.key + 1) as $n
    | ($e.value | try fromjson catch null) as $op
    | if ($op | type) != "object" then .errors += ["line \($n): not a JSON object"]
      else
        ( [ (if $op.v != 1 then "unsupported op version \($op.v | tojson)" else empty end),
            (if ($op.session | type) != "string" or ($op.session | test(sid_re) | not) then "bad session id" else empty end),
            (if ($op.id | type) != "string" or ($op.id | startswith("\($op.session | tostring)/") | not)
                or ($op.id | test("/[0-9]{1,9}$") | not) then "bad op id \($op.id | tojson)" else empty end),
            (if ($op.ts | type) != "string" or ($op.ts | valid("word") | not) then "bad timestamp" else empty end),
            (if ($op.args | type) != "array" or ($op.args | all(type == "string") | not) then "args must be an array of strings" else empty end),
            (if (spec | has($op.verb | tostring) | not) then "verb \($op.verb | tojson) is not a relayable tracker write" else empty end)
          ] ) as $basic
        | if ($basic | length) > 0 then .errors += [$basic[] | "line \($n): \(.)"]
          else
            ( ($op | arg_errors)
              + (if ($want != "" and $op.session != $want) then ["session \($op.session) does not match its branch (\($want))"] else [] end)
              + (if (.session != null and $op.session != .session) then ["more than one session in one relay"] else [] end)
              + (if (.ids | has($op.id)) then ["duplicate op id \($op.id)"] else [] end)
              + (if $op.verb == "create" then
                   (if ($op.handle | type) != "string" or ($op.handle | test(handle_re) | not) then ["create needs a relay:<n> handle"]
                    elif (.handles | has($op.handle)) then ["handle \($op.handle) defined twice"] else [] end)
                 else [] end)
              + ( . as $st | [$op | idpos[] as $i | $op.args[$i] // empty
                  | select(test(handle_re)) | select(. as $h | $st.handles | has($h) | not)
                  | "\(.) is used before any create defines it"] )
            ) as $errs
            | if ($errs | length) > 0 then .errors += [$errs[] | "line \($n): \(.)"]
              else .ops += [$op] | .ids[$op.id] = true | .session = $op.session
                   | if $op.verb == "create" then .handles[$op.handle] = true else . end
              end
          end
      end)
  | if (.ops | length) == 0 and (.errors | length) == 0 then .errors = ["no ops"] else . end
  | {ok: ((.errors | length) == 0), errors, ops, session};
'

# $1 = relay file content, $2 = expected session ("" = any). Prints the validation object.
_validate() {
  if [ -z "$1" ]; then
    jq -n '{ok: false, errors: ["empty relay file"], ops: [], session: null}'
    return
  fi
  printf '%s\n' "$1" | jq -Rn --arg want "$2" "$_JQ_LIB"' [inputs] | validate($want)'
}

# --- the tracker's git repo -------------------------------------------------------------------
# Sets REPO and REMOTE for the git repo holding tracker dir $1. Fails when there is none.
_repo() {
  local d="$1"
  case "$d" in "~") d="$HOME" ;; "~/"*) d="$HOME/${d#\~/}" ;; esac
  [ -n "$d" ] && [ -d "$d" ] || return 1
  REPO="$(git -C "$d" rev-parse --show-toplevel 2>/dev/null)" || return 1
  [ -n "$REPO" ] || return 1
  local up
  up="$(git -C "$REPO" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null)" || up=""
  REMOTE="${up%%/*}"
  if [ -z "$REMOTE" ] || ! git -C "$REPO" remote get-url "$REMOTE" >/dev/null 2>&1; then
    if git -C "$REPO" remote get-url origin >/dev/null 2>&1; then REMOTE=origin
    else REMOTE="$(git -C "$REPO" remote | head -1)"
    fi
  fi
  return 0
}

# The session id this machine relays under: stable for the life of a session, safe in a ref name.
_session() {
  local s="${BATON_RELAY_SESSION:-${CLAUDE_CODE_REMOTE_SESSION_ID:-${CLAUDE_CODE_SESSION_ID:-}}}"
  if [ -z "$s" ]; then
    local f
    f="$(git -C "$REPO" rev-parse --git-path baton-relay-session 2>/dev/null)" || f=""
    case "$f" in /*) ;; *) f="$REPO/$f" ;; esac
    s="$(cat "$f" 2>/dev/null)"
    if [ -z "$s" ]; then
      s="s$(date -u +%Y%m%d%H%M%S)-$$-$RANDOM"
      printf '%s\n' "$s" >"$f" 2>/dev/null || true
    fi
  fi
  s="$(printf '%s' "$s" | tr -c 'A-Za-z0-9._-' '-' | cut -c1-64)"
  s="${s#[-._]}"
  printf '%s' "$s"
}

_git_ident() {
  export GIT_AUTHOR_NAME="${GIT_AUTHOR_NAME:-$(git -C "$REPO" config user.name 2>/dev/null || echo 'baton relay')}"
  export GIT_AUTHOR_EMAIL="${GIT_AUTHOR_EMAIL:-$(git -C "$REPO" config user.email 2>/dev/null || echo 'baton-relay@localhost')}"
  export GIT_COMMITTER_NAME="${GIT_COMMITTER_NAME:-$GIT_AUTHOR_NAME}"
  export GIT_COMMITTER_EMAIL="${GIT_COMMITTER_EMAIL:-$GIT_AUTHOR_EMAIL}"
}

# --- record (cloud side) ----------------------------------------------------------------------
_record() {
  local tdir="" result=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --tracker) tdir="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
      --result)  result="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
      --) shift; break ;;
      *) _die "record: unknown option '$1'" ;;
    esac
  done
  [ $# -ge 1 ] || _die "record: no verb given"
  local verb="$1"; shift
  _repo "$tdir" || _die "the tracker ($tdir) is not in a git repo, so there is no branch to relay through"
  local sid; sid="$(_session)"
  [ -n "$sid" ] || _die "could not determine a session id"
  _git_ident
  local ref="refs/heads/$BRANCH_NS/$sid" args_json attempt old content n op new blob tree commit v
  args_json="$(jq -cn '$ARGS.positional' --args -- "$@")"
  # update-ref compares against the old value, so two writes racing in one session cannot lose
  # each other's op; the loser re-reads and tries again.
  for attempt in 1 2 3 4 5; do
    old="$(git -C "$REPO" rev-parse -q --verify "$ref" 2>/dev/null)" || old=""
    content=""
    [ -n "$old" ] && content="$(git -C "$REPO" cat-file blob "$old:$RELAY_FILE" 2>/dev/null)"
    n=1
    [ -n "$content" ] && n=$(( $(printf '%s\n' "$content" | grep -c .) + 1 ))
    op="$(printf '%s\n' "$content" | jq -cRn --arg sid "$sid" --arg n "$n" --arg ts "$(_now)" \
            --arg verb "$verb" --argjson args "$args_json" --arg result "$result" "$_JQ_LIB"'
      ([inputs | select(length > 0) | fromjson | select(.verb == "create" and .local_id)
        | {(.local_id): .handle}] | add // {}) as $m
      | {v: 1, id: "\($sid)/\($n)", ts: $ts, session: $sid, verb: $verb, args: $args}
      | map_ids($m)
      | if $verb == "create" then .handle = "relay:\($n)" | .local_id = ($result | split("\n")[0]) else . end')" \
      || _die "could not build the op"
    new="$content${content:+$'\n'}$op"
    v="$(_validate "$new" "$sid")"
    if [ "$(jq -r .ok <<<"$v")" != true ]; then
      _die "this write cannot be relayed: $(jq -r '.errors | last' <<<"$v")"
    fi
    blob="$(printf '%s\n' "$new" | git -C "$REPO" hash-object -w --stdin)" || _die "git hash-object failed"
    tree="$(printf '100644 blob %s\t%s\n' "$blob" "$RELAY_FILE" | git -C "$REPO" mktree)" || _die "git mktree failed"
    commit="$(git -C "$REPO" commit-tree "$tree" ${old:+-p "$old"} -m "relay: $verb (op $sid/$n)")" \
      || _die "git commit-tree failed"
    if git -C "$REPO" update-ref "$ref" "$commit" "${old:-0000000000000000000000000000000000000000}" 2>/dev/null; then
      return 0
    fi
  done
  _die "could not update $ref (concurrent writes?)"
}

# --- push (cloud side) ------------------------------------------------------------------------
_push() {
  local tdir=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --tracker) tdir="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
      *) _die "push: unknown option '$1'" ;;
    esac
  done
  _repo "$tdir" || _die "the tracker ($tdir) is not in a git repo — nothing to relay through"
  local sid ref out
  sid="$(_session)"
  ref="refs/heads/$BRANCH_NS/$sid"
  git -C "$REPO" rev-parse -q --verify "$ref" >/dev/null 2>&1 || return 0
  [ -n "$REMOTE" ] || _die "the tracker repo has no git remote — relay $BRANCH_NS/$sid cannot leave this machine"
  out="$(git -C "$REPO" push -q "$REMOTE" "$ref:$ref" 2>&1)" \
    || _die "push of $BRANCH_NS/$sid to $REMOTE failed: $(printf '%s\n' "$out" | grep . | tail -1)"
  echo "tracker: relayed $(git -C "$REPO" cat-file blob "$ref:$RELAY_FILE" | grep -c .) write(s) on $BRANCH_NS/$sid"
}

# --- apply (the machine that can push) --------------------------------------------------------
_apply() {
  local ctx_src=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --context) ctx_src="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
      *) _die "apply: unknown option '$1'" ;;
    esac
  done
  # A session that relays its own writes is not one that may apply anyone's.
  _active && return 0
  local ctx
  if [ "$ctx_src" = - ]; then ctx="$(cat)"
  elif [ -n "$ctx_src" ]; then ctx="$(cat "$ctx_src" 2>/dev/null)" || ctx=""
  else ctx="$("$HERE/resolve-context.sh" 2>/dev/null)" || ctx=""
  fi
  jq -e 'type == "object"' >/dev/null 2>&1 <<<"$ctx" || { echo "tracker relay: no context resolved — relays not checked" >&2; return 0; }
  [ "$(jq -r '.task_tracking.sync == false' <<<"$ctx")" = true ] && return 0
  _repo "$(jq -r '.task_tracking.dir // empty' <<<"$ctx")" || return 0
  [ -n "$REMOTE" ] || return 0

  CTXF="$(mktemp)"; trap 'rm -f "$CTXF" "$CTXF.err"' EXIT
  printf '%s' "$ctx" >"$CTXF"

  local ls sid sha ref pending
  # Leftovers from a run that died part-way go first; their remote branches are already gone.
  for ref in $(git -C "$REPO" for-each-ref --format='%(refname)' "$PENDING_NS/" 2>/dev/null); do
    _apply_one "${ref#"$PENDING_NS"/}" "$ref" resumed
  done

  ls="$(git -C "$REPO" ls-remote "$REMOTE" "refs/heads/$BRANCH_NS/*" 2>&1)" \
    || { echo "tracker relay: could not list relay branches on $REMOTE — $(printf '%s\n' "$ls" | grep . | tail -1)" >&2; return 0; }
  while IFS=$'\t' read -r sha ref || [ -n "$sha" ]; do
    [ -n "$sha" ] && [ -n "$ref" ] || continue
    sid="${ref#refs/heads/$BRANCH_NS/}"
    if ! printf '%s' "$sid" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$'; then
      echo "tracker relay ${ref#refs/heads/}: not a relay branch name baton writes — left alone"
      continue
    fi
    pending="$PENDING_NS/$sid"
    if ! git -C "$REPO" fetch -q "$REMOTE" "+$ref:$pending" 2>/dev/null; then
      echo "tracker relay $sid: could not fetch — left for next time"
      continue
    fi
    if [ "$(git -C "$REPO" rev-parse "$pending" 2>/dev/null)" != "$sha" ]; then
      echo "tracker relay $sid: changed while being fetched — left for next time"
      git -C "$REPO" update-ref -d "$pending" 2>/dev/null
      continue
    fi
    _apply_one "$sid" "$pending" remote "$ref" "$sha"
  done <<<"$ls"
  return 0
}

# Look a task up once per op. Sets TASK (JSON) and returns get's exit status.
_get() { TASK="$("$TRK" --context "$CTXF" get "$1" 2>/dev/null)"; }

_T() { BATON_TRACKER_RELAY=off "$TRK" --context "$CTXF" "$@"; }

# $1 sid, $2 local ref holding the relay, $3 remote|resumed, [$4 remote ref, $5 its sha]
_apply_one() {
  local sid="$1" lref="$2" origin="$3" rref="${4:-}" rsha="${5:-}"
  local content v out
  content="$(git -C "$REPO" cat-file blob "$lref:$RELAY_FILE" 2>/dev/null)" || content=""
  v="$(_validate "$content" "$sid")"
  if [ "$(jq -r .ok <<<"$v")" != true ]; then
    echo "tracker relay $sid: MALFORMED, nothing applied, left for a human — $(jq -r '.errors[0:3] | join("; ")' <<<"$v")"
    # A remote relay stays on its branch; a resumed one goes back there so it is not lost.
    if [ "$origin" = resumed ]; then
      git -C "$REPO" push -q "$REMOTE" "$lref:refs/heads/$BRANCH_NS/$sid" 2>/dev/null \
        && git -C "$REPO" update-ref -d "$lref"
    else
      git -C "$REPO" update-ref -d "$lref"
    fi
    return 0
  fi
  if [ "$origin" = remote ]; then
    out="$(git -C "$REPO" push -q --force-with-lease="$rref:$rsha" "$REMOTE" ":$rref" 2>&1)" || {
      echo "tracker relay $sid: could not take the branch (updated or taken elsewhere?) — left for next time"
      git -C "$REPO" update-ref -d "$lref"
      return 0
    }
  fi

  local n i op verb opid ts id reason applied=0 skipped=0 skips="" hmap="{}" scan=""
  local -a args
  n="$(jq '.ops | length' <<<"$v")"
  for (( i = 0; i < n; i++ )); do
    op="$(jq -c --argjson m "$hmap" "$_JQ_LIB"' .ops['"$i"'] | map_ids($m)' <<<"$v")"
    verb="$(jq -r .verb <<<"$op")"; opid="$(jq -r .id <<<"$op")"; ts="$(jq -r .ts <<<"$op")"
    args=()
    while IFS= read -r -d '' a; do args+=("$a"); done < <(jq -j '.args[] | . + "\u0000"' <<<"$op")
    reason=""

    # An id still shaped like a handle means its create was not applied.
    id="${args[0]}"
    [ "$verb" = create ] && id=""
    for a in $(jq -r "$_JQ_LIB"' idpos[] as $i | .args[$i] // empty' <<<"$op"); do
      case "$a" in relay:*) reason="depends on $a, whose create was not applied" ;; esac
    done

    if [ -z "$reason" ] && [ "$verb" = create ]; then
      if [ -z "$scan" ]; then
        scan="$("$TRK" --context "$CTXF" list --status all 2>/dev/null)" || scan="[]"
        jq -e 'type == "array"' >/dev/null 2>&1 <<<"$scan" || scan="[]"
      fi
      id="$(jq -r --arg m "[relay-op:$opid]" 'map(select((.notes // "") | index($m))) | .[0].id // empty' <<<"$scan")"
      if [ -n "$id" ]; then
        reason="already applied (as $id)"
        hmap="$(jq -c --arg h "$(jq -r .handle <<<"$op")" --arg id "$id" '.[$h] = $id' <<<"$hmap")"
        id=""
      fi
    elif [ -z "$reason" ]; then
      _get "$id"
      case $? in
        0) ;;
        3) reason="no such task $id" ;;
        *) reason="could not read $id" ;;
      esac
      if [ -z "$reason" ] && jq -e --arg m "[relay-op:$opid]" '(.notes // "") | index($m)' >/dev/null <<<"$TASK"; then
        reason="already applied"
      fi
      if [ -z "$reason" ]; then
        local st asg
        st="$(jq -r .status <<<"$TASK")"; asg="$(jq -r '.assignee // ""' <<<"$TASK")"
        case "$verb" in
          close)  [ "$st" = closed ] && reason="$id is already closed" ;;
          reopen) [ "$st" != closed ] && reason="$id is not closed ($st)" ;;
          claim)
            if [ "$st" = closed ]; then reason="$id is closed"
            elif [ "$st" = in_progress ] && [ -n "$asg" ]; then reason="$id is already claimed by $asg"
            fi ;;
          label-add)
            jq -e --arg l "${args[1]}" '.labels | index($l)' >/dev/null <<<"$TASK" \
              && reason="$id already has label ${args[1]}" ;;
          label-remove)
            jq -e --arg l "${args[1]}" '.labels | index($l)' >/dev/null <<<"$TASK" \
              || reason="$id has no label ${args[1]}" ;;
          link)
            "$TRK" --context "$CTXF" get "${args[1]}" >/dev/null 2>&1 || reason="no such task ${args[1]}" ;;
        esac
      fi
    fi
    if [ -z "$reason" ] && [ "$verb" = create ]; then
      local par; par="$(jq -r '.args as $a | [$a | to_entries[] | select(.value == "--parent") | $a[.key + 1]] | .[0] // empty' <<<"$op")"
      if [ -n "$par" ] && ! "$TRK" --context "$CTXF" get "$par" >/dev/null 2>&1; then
        reason="no such parent $par"
      fi
    fi

    if [ -z "$reason" ]; then
      local prov="Relayed from cloud session $sid ($ts): $verb [relay-op:$opid]"
      if [ "$verb" = note ]; then
        # The text and its marker land in one write, so a crash cannot apply one without the other.
        out="$(_T note "$id" "${args[1]}"$'\n'"— $prov" 2>&1 >/dev/null)" || reason="tracker refused: $(printf '%s\n' "$out" | grep . | tail -1)"
      else
        out="$(_T "$verb" "${args[@]}" 2>"$CTXF.err")" || reason="tracker refused: $(grep . "$CTXF.err" | tail -1)"
        if [ -z "$reason" ] && [ "$verb" = create ]; then
          id="$(printf '%s\n' "$out" | grep . | head -1)"
          hmap="$(jq -c --arg h "$(jq -r .handle <<<"$op")" --arg id "$id" '.[$h] = $id' <<<"$hmap")"
        fi
        if [ -z "$reason" ]; then
          _T note "$id" "$prov" >/dev/null 2>&1 \
            || skips="$skips; $opid $verb applied but its provenance note failed (a re-run may repeat it)"
        fi
      fi
      rm -f "$CTXF.err"
    fi

    if [ -n "$reason" ]; then
      skipped=$((skipped + 1)); skips="$skips; $opid $verb: $reason"
    else
      applied=$((applied + 1))
    fi
  done
  git -C "$REPO" update-ref -d "$lref" 2>/dev/null
  local tag=""; [ "$origin" = resumed ] && tag=" (resumed)"
  echo "tracker relay $sid$tag: $applied applied, $skipped skipped${skips:+ — ${skips#; }}"
}

# --- dispatch ---------------------------------------------------------------------------------
CMD="${1:-}"; shift || true
case "$CMD" in
  active) _active ;;
  record) _record "$@" ;;
  push)   _push "$@" ;;
  apply)  _apply "$@"; exit 0 ;;
  validate)
    WANT=""
    [ "${1:-}" = --session ] && { WANT="${2:-}"; shift 2 || _die "option '--session' needs a value"; }
    SRC="${1:-}"; [ -n "$SRC" ] || _die "validate needs a file (or -)"
    if [ "$SRC" = - ]; then C="$(cat)"; else C="$(cat "$SRC" 2>/dev/null)" || _die "cannot read $SRC"; fi
    V="$(_validate "$C" "$WANT")"
    printf '%s\n' "$V"
    [ "$(jq -r .ok <<<"$V")" = true ]
    ;;
  -h|--help|"") sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'; [ -n "$CMD" ] ;;
  *) _die "unknown command '$CMD'" ;;
esac
