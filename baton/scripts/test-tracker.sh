#!/usr/bin/env bash
# baton — tests for the tracker seam: scripts/tracker.sh, tracker/lib-registry.sh, tracker/beads.sh.
#
# Two things here decide real outcomes, so both are pinned rather than left to a careful reading:
#
#   NORMALIZATION.  Every skill now reads tasks through `get`. `bd show --json` returns a
#                   single-element ARRAY, and a bare `.status` against it makes jq exit 5 — which
#                   already happened once in baton:cleanup-worktrees, producing an empty status on
#                   every run and silently killing two of its buckets. `get` must always emit a
#                   bare object, and must never turn an unreadable status into a confident `open`.
#
#   THE FOLD.       baton:cleanup-worktrees decides whether to DELETE a worktree partly from the
#                   branch registry. The registry is an append-only log folded on read, so the
#                   fold is what a readiness signal actually comes from. A partial update that
#                   silently lands in a group of its own reads as "no readiness recorded" — which
#                   is safe, and also means the feature quietly does nothing.
#
# Hermetic: stubs `bd` with a tiny file-backed fake, so nothing touches a real tracker, the
# network, or the machine's beads install — and CI needs no `bd`. Only bash and jq are required.
# Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TRACKER="$HERE/tracker.sh"
LIB="$HERE/tracker/lib-registry.sh"
BEADS="$HERE/tracker/beads.sh"
for f in "$TRACKER" "$LIB" "$BEADS"; do [ -r "$f" ] || { echo "not found: $f" >&2; exit 2; }; done

command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }
_has() { case "$2" in *"$3"*) _ok "$1" ;; *) _bad "$1" "'$2' does not contain '$3'" ;; esac; }

# =============================================================================================
# A stub `bd`. File-backed, just enough for the verbs under test, and deliberately reproducing
# the two bd shapes the backend exists to absorb: `show --json` returns an ARRAY, and `label
# list --json` returns a bare array of strings.
# =============================================================================================
STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
STUBDB="$TMP/db"; mkdir -p "$STUBDB"
cat >"$STUBBIN/bd" <<'STUB'
#!/usr/bin/env bash
# Minimal fake bd. State lives under $STUB_DB, keyed by issue id.
set -uo pipefail
DB="${STUB_DB:?}"
# Record the BEADS_DIR each call saw, so a test can assert the seam pins it.
printf '%s\n' "${BEADS_DIR:-<unset>}" >>"$DB/.beads_dir_seen"
cmd="${1:-}"; shift || true
case "$cmd" in
  show)
    id="${1:-}"
    [ -f "$DB/$id.json" ] || { echo "issue not found: $id" >&2; exit 1; }
    jq -c '[.]' "$DB/$id.json"      # bd emits a single-element ARRAY, not an object
    ;;
  comments)
    id="${1:-}"
    if [ -f "$DB/$id.comments" ]; then jq -s -c '.' "$DB/$id.comments"; else echo '[]'; fi
    ;;
  comment)
    id="${1:-}"; shift
    [ -f "$DB/$id.json" ] || { echo "issue not found: $id" >&2; exit 1; }
    n=$(( $(wc -l <"$DB/$id.comments" 2>/dev/null || echo 0) + 1 ))
    # A stable, strictly increasing fake timestamp: real ordering is by created_at, and two
    # writes inside one wall-clock second must still fold newest-last.
    jq -cn --arg t "$1" --arg at "$(printf '2026-01-01T00:00:%02dZ' "$n")" \
      '{text:$t, created_at:$at}' >>"$DB/$id.comments"
    echo "✓ Comment added to $id"
    ;;
  label)
    sub="${1:-}"; id="${2:-}"
    case "$sub" in
      list) if [ -f "$DB/$id.labels" ]; then jq -R . "$DB/$id.labels" | jq -s -c .; else echo '[]'; fi ;;
      add)  echo "${3:-}" >>"$DB/$id.labels" ;;
      *) exit 1 ;;
    esac
    ;;
  dolt)
    # `bd dolt remote list`. $DB/.remote_out holds whatever bd would print.
    cat "$DB/.remote_out" 2>/dev/null
    ;;
  create)
    title="${1:-}"; shift || true
    desc=""; parent=""; labels=""; prio=2
    while [ $# -gt 0 ]; do
      case "$1" in
        --json) shift ;;
        -d)        desc="${2:-}";   shift 2 ;;
        --parent)  parent="${2:-}"; shift 2 ;;
        --labels)  labels="${2:-}"; shift 2 ;;
        -p)        prio="${2:-}";   shift 2 ;;
        *) shift ;;
      esac
    done
    n=$(( $(find "$DB" -maxdepth 1 -name '*.json' | wc -l) + 1 ))
    id="stub-$n"
    jq -n --arg id "$id" --arg t "$title" --arg d "$desc" --arg p "$parent" \
          --arg l "$labels" --argjson pr "$prio" \
      '{id:$id, title:$t, description:$d, acceptance_criteria:"", notes:"",
        status:"open", priority:$pr, issue_type:"task",
        labels:(if $l == "" then [] else ($l|split(",")) end),
        parent:(if $p == "" then null else $p end),
        created_at:"2026-01-01T00:00:00Z", updated_at:"2026-01-01T00:00:00Z"}' >"$DB/$id.json"
    : >"$DB/$id.comments"
    jq -c '[.]' "$DB/$id.json"
    ;;
  update)
    id="${1:-}"; shift || true
    [ -f "$DB/$id.json" ] || { echo "issue not found: $id" >&2; exit 1; }
    while [ $# -gt 0 ]; do
      case "$1" in
        --status)   jq -c --arg v "${2:-}" '.status=$v'   "$DB/$id.json" >"$DB/$id.t" && mv "$DB/$id.t" "$DB/$id.json"; shift 2 ;;
        --priority) jq -c --arg v "${2:-}" '.priority=(($v|tonumber?) // $v)' "$DB/$id.json" >"$DB/$id.t" && mv "$DB/$id.t" "$DB/$id.json"; shift 2 ;;
        --title)    jq -c --arg v "${2:-}" '.title=$v'       "$DB/$id.json" >"$DB/$id.t" && mv "$DB/$id.t" "$DB/$id.json"; shift 2 ;;
        -d|--description)
                    jq -c --arg v "${2:-}" '.description=$v' "$DB/$id.json" >"$DB/$id.t" && mv "$DB/$id.t" "$DB/$id.json"; shift 2 ;;
        --claim)    jq -c '.status="in_progress" | .assignee="stub"' "$DB/$id.json" >"$DB/$id.t" && mv "$DB/$id.t" "$DB/$id.json"; shift ;;
        *) shift ;;
      esac
    done
    ;;
  close)
    id="${1:-}"; shift || true
    [ -f "$DB/$id.json" ] || { echo "issue not found: $id" >&2; exit 1; }
    reason=""
    while [ $# -gt 0 ]; do case "$1" in --reason) reason="${2:-}"; shift 2 ;; *) shift ;; esac; done
    jq -c --arg r "$reason" '.status="closed" | .close_reason=$r' "$DB/$id.json" >"$DB/$id.t" \
      && mv "$DB/$id.t" "$DB/$id.json"
    ;;
  reopen)
    id="${1:-}"
    [ -f "$DB/$id.json" ] || { echo "issue not found: $id" >&2; exit 1; }
    jq -c '.status="open" | .close_reason=null' "$DB/$id.json" >"$DB/$id.t" && mv "$DB/$id.t" "$DB/$id.json"
    ;;
  note)
    id="${1:-}"
    [ -f "$DB/$id.json" ] || { echo "issue not found: $id" >&2; exit 1; }
    jq -c --arg n "${2:-}" '.notes = ((.notes // "") + $n)' "$DB/$id.json" >"$DB/$id.t" \
      && mv "$DB/$id.t" "$DB/$id.json"
    ;;
  link)
    # bd 1.1.0 vocabulary, and it is NOT the seam's: blocks|tracks|related|parent-child|
    # discovered-from. `relates-to` is rejected here exactly as real bd rejects it, which is what
    # makes the backend's translation testable at all.
    a="${1:-}"; b="${2:-}"; shift 2 || true
    type=blocks
    while [ $# -gt 0 ]; do case "$1" in --type) type="${2:-}"; shift 2 ;; *) shift ;; esac; done
    case "$type" in
      blocks|tracks|related|parent-child|discovered-from) ;;
      *) echo "Error: invalid dependency type \"$type\"" >&2; exit 1 ;;
    esac
    printf '%s %s %s\n' "$a" "$b" "$type" >>"$DB/.links"
    ;;
  list)
    status=""; label=""; limit=""; all=no
    while [ $# -gt 0 ]; do
      case "$1" in
        --json) shift ;;
        --all)  all=yes; shift ;;
        --status) status="${2:-}"; shift 2 ;;
        --label)  label="${2:-}";  shift 2 ;;
        --limit)  limit="${2:-}";  shift 2 ;;
        *) shift ;;
      esac
    done
    case "$status" in ""|all|open|in_progress|blocked|deferred|closed|pinned|hooked) ;;
      *) echo "Error: invalid status \"$status\" (valid: open, in_progress, blocked, deferred, closed, pinned, hooked)" >&2; exit 1 ;;
    esac
    jq -s -c --arg st "$status" --arg lb "$label" --arg all "$all" --arg lim "${limit:-0}" \
      '[ .[]
         # Some fixtures are deliberately not issue records (an `[]` for the not-found tests).
         # Real bd never lists a malformed row, so skip them rather than failing the whole read.
         | select(type == "object")
         | select($st == "" or $st == "all" or .status == $st)
         | select($lb == "" or ((.labels // []) | index($lb)))
         # Without --all (or an explicit status) bd shows only non-closed issues.
         | select($all == "yes" or $st != "" or .status != "closed") ]
       | (if ($lim|tonumber) > 0 then .[0:($lim|tonumber)] else . end)' \
      "$DB"/*.json 2>/dev/null || echo '[]'
    ;;
  ready)
    jq -s -c '[ .[] | select(type == "object") | select(.status == "open") ]' "$DB"/*.json 2>/dev/null || echo '[]'
    ;;
  children)
    id="${1:-}"
    jq -s -c --arg p "$id" '[ .[] | select(type == "object") | select(.parent == $p) ]' \
      "$DB"/*.json 2>/dev/null || echo '[]'
    ;;
  dep)
    # `bd dep list <id> [--direction=up] --json`
    id="${2:-}"
    if [ -f "$DB/$id.deps" ]; then cat "$DB/$id.deps"; else echo '[]'; fi
    ;;
  *) echo "stub bd: unhandled '$cmd'" >&2; exit 1 ;;
esac
STUB
chmod +x "$STUBBIN/bd"

_mkissue() { # $1 = id, $2 = status  — writes a bd-shaped issue record
  jq -n --arg id "$1" --arg s "$2" \
    '{id:$id, title:"t", description:"d", acceptance_criteria:"a", notes:"n",
      status:$s, priority:2, issue_type:"task", labels:["baton"], parent:null,
      created_at:"2026-01-01T00:00:00Z", updated_at:"2026-01-02T00:00:00Z"}' \
    >"$STUBDB/$1.json"
  : >"$STUBDB/$1.comments"
}

_t() { # run the seam against the stub
  STUB_DB="$STUBDB" PATH="$STUBBIN:$PATH" \
    bash "$TRACKER" --type beads --tracker "$TMP/tracker-dir" "$@"
}

# =============================================================================================
echo "dispatch"

OUT="$(BATON_TRACKER_TEST=1 bash "$TRACKER" --type nosuchtracker capabilities 2>&1)"; RC=$?
_eq  "unknown backend type exits 1" "$RC" "1"
_has "unknown backend names the implemented ones" "$OUT" "implemented backends: beads"

OUT="$(bash "$TRACKER" --type '../../etc/passwd' capabilities 2>&1)"; RC=$?
_eq  "a type that could escape the backend dir is rejected" "$RC" "1"
_has "…and says why"                                       "$OUT" "invalid task_tracking.type"

# `capabilities` must answer with NO `bd` on PATH. baton:doctor calls it to learn which tools this
# backend needs, so requiring the tool in order to report the tool is a circle — on a machine
# without bd, doctor would get nothing back and could not say what was missing. Stripping PATH to
# a shell with no bd is how these cases catch it; CI (which has no bd) found the original.
_nobd() { PATH="$TMP/emptybin:/usr/bin:/bin" bash "$TRACKER" "$@"; }
mkdir -p "$TMP/emptybin"

OUT="$(_nobd --context - --tracker "$TMP/x" capabilities 2>/dev/null \
        <<<'{"task_tracking":{"type":"beads","dir":"/nope"}}' | jq -r .type)"
_eq "type comes from the context when --type is absent" "$OUT" "beads"

# An old context.yaml with no `type` at all must keep working — that is every context written
# before this seam existed.
OUT="$(_nobd --context - --tracker "$TMP/x" capabilities 2>/dev/null \
        <<<'{"task_tracking":{"dir":"/nope"}}' | jq -r .type)"
_eq "a context with no task_tracking.type defaults to beads" "$OUT" "beads"

_eq "capabilities answers with no bd installed — doctor asks it WHICH tools it needs" \
    "$(_nobd --type beads --tracker "$TMP/x" capabilities 2>/dev/null | jq -r '.tools | join(",")')" \
    "bd,jq"
OUT="$(_nobd --type beads --tracker "$TMP/x" get anything 2>&1)"; RC=$?
_eq  "…while a verb that really needs bd still fails loudly" "$RC" "1"
_has "…naming the missing tool"                             "$OUT" "bd is not installed"

OUT="$(_t nosuchverb 2>&1)"; RC=$?
_eq "an unimplemented verb exits 4, not 1" "$RC" "4"

# =============================================================================================
echo
echo "normalization — what every skill now reads"

_mkissue task-1 in_progress
rm -f "$STUBDB/.beads_dir_seen"

OUT="$(_t get task-1)"
_eq "get returns a bare OBJECT, not bd's single-element array" "$(printf '%s' "$OUT" | jq -r type)" "object"
_eq "…with the id"                                            "$(printf '%s' "$OUT" | jq -r .id)" "task-1"
_eq "…and a normalized status"                                "$(printf '%s' "$OUT" | jq -r .status)" "in_progress"
_eq "…and every contract field present"                       \
    "$(printf '%s' "$OUT" | jq -r '[.title,.description,.acceptance_criteria,.notes,.type] | join("|")')" \
    "t|d|a|n|task"

# The seam pins BEADS_DIR per call. An ambient one silently redirects every `bd` write with no
# error — the whole reason baton:beads Step 2 exists — so this must not depend on the caller.
_eq "the backend saw the tracker dir as BEADS_DIR" \
    "$(head -1 "$STUBDB/.beads_dir_seen")" "$TMP/tracker-dir"
OUT="$(BEADS_DIR=/somewhere/else _t get task-1 >/dev/null; tail -1 "$STUBDB/.beads_dir_seen")"
_eq "an ambient BEADS_DIR cannot leak past the seam" "$OUT" "$TMP/tracker-dir"

# An unrecognized status must never round down to `open`: cleanup-verdict.sh treats "the lookup
# failed" and "the task is open" completely differently.
_mkissue task-weird wibble
_eq "an unrecognized status becomes 'unknown', never 'open'" \
    "$(_t get task-weird | jq -r .status)" "unknown"

OUT="$(_t get task-missing 2>&1)"; RC=$?
_eq "a missing id exits 3 (NOT FOUND), not 1" "$RC" "3"

# bd exits 0 for some misses. An empty record would normalize into a task with an empty id and
# status `unknown` — a confident answer about nothing.
echo '[]' >"$STUBDB/task-empty.json"
RC=0; _t get task-empty >/dev/null 2>&1 || RC=$?
_eq "an empty result is NOT FOUND too, not an empty task" "$RC" "3"

_t label-add task-1 ready-for-worktree-delete
_eq "label-list returns a JSON array, never bd's bulleted prose" \
    "$(_t label-list task-1 | jq -c .)" '["ready-for-worktree-delete"]'

# =============================================================================================
echo
echo "the branch registry — round trip through the backend"

_mkissue task-reg open
_t record-branch task-reg --repo maestro --branch feat-a --worktree /wt/a --created 2026-02-01T00:00:00Z
_t record-branch task-reg --repo maestro --branch feat-b --worktree /wt/b --created 2026-02-02T00:00:00Z

_eq "one entry per branch" "$(_t list-branches task-reg | jq -r 'map(.branch) | join(",")')" "feat-a,feat-b"
_eq "…carrying the worktree path" "$(_t list-branches task-reg | jq -r '.[0].worktree')" "/wt/a"
_eq "…and starting at status open" "$(_t list-branches task-reg | jq -r '.[0].status')" "open"

# THE BUG THIS TEST EXISTS FOR: update-branch names only the branch. Before the fold learned to
# resolve a repo-less entry onto its group, this landed in a THIRD group and the folded view
# still showed status=open — the update looked like it took and did nothing.
_t update-branch task-reg feat-a status=pr-open pr=42
E="$(_t list-branches task-reg | jq -c '.[] | select(.branch=="feat-a")')"
_eq "a repo-less update folds onto the recorded branch, not into a group of its own" \
    "$(_t list-branches task-reg | jq -r length)" "2"
_eq "…and its status took"          "$(printf '%s' "$E" | jq -r .status)" "pr-open"
_eq "…and its pr number took"       "$(printf '%s' "$E" | jq -r .pr)" "42"
_eq "…while the worktree survived"  "$(printf '%s' "$E" | jq -r .worktree)" "/wt/a"
_eq "…and so did the creation time" "$(printf '%s' "$E" | jq -r .created)" "2026-02-01T00:00:00Z"
_eq "…and the other branch is untouched" \
    "$(_t list-branches task-reg | jq -r '.[] | select(.branch=="feat-b") | .status')" "open"

_t update-branch task-reg feat-a status=merged ready=yes
E="$(_t list-branches task-reg | jq -c '.[] | select(.branch=="feat-a")')"
_eq "a later update wins"                    "$(printf '%s' "$E" | jq -r .status)" "merged"
_eq "…without dropping an earlier field"     "$(printf '%s' "$E" | jq -r .pr)" "42"
_eq "…and the branch-scoped ready signal is recorded" "$(printf '%s' "$E" | jq -r .ready)" "yes"
_eq "…with the revision count as provenance" "$(printf '%s' "$E" | jq -r .revisions)" "3"

# A tracker people also talk in must not break the registry.
STUB_DB="$STUBDB" PATH="$STUBBIN:$PATH" bd comment task-reg "looks good to me, merging" >/dev/null
STUB_DB="$STUBDB" PATH="$STUBBIN:$PATH" bd comment task-reg '{"some":"other json"}' >/dev/null
_eq "human prose and unrelated JSON comments are skipped" \
    "$(_t list-branches task-reg | jq -r length)" "2"

OUT="$(_t update-branch task-reg feat-a stauts=merged 2>&1)"; RC=$?
_eq  "a typo'd field is rejected rather than silently stored" "$RC" "1"
_has "…and the message lists the real fields"                "$OUT" "unknown branch field 'stauts'"

OUT="$(_t update-branch task-reg feat-a status=bananas 2>&1)"; RC=$?
_eq  "an out-of-vocabulary status is rejected" "$RC" "1"
_has "…and lists the vocabulary"               "$OUT" "no-change"

_mkissue task-nobr open
_eq "a task with no branches yields an empty array, not an error" \
    "$(_t list-branches task-nobr | jq -c .)" "[]"

# =============================================================================================
# `remote` is what baton:session-start consults BEFORE pulling, so a false "configured" is the
# wrong direction to fail in — it would green-light a sync against a tracker with no remote, or
# against whatever a rewritten config now points at.
echo
echo "remote — the pre-sync safety check"

printf 'No remotes configured.\n' >"$STUBDB/.remote_out"
_eq "no remote configured reports configured=false" \
    "$(_t remote | jq -r .configured)" "false"
# The specific trap: the prose's second whitespace field is "remotes", so a bare `$2` reported a
# configured remote by that name. Observed against bd 1.1.0.
_eq "…and does not name a remote called 'remotes'" \
    "$(_t remote | jq -r .remote)" ""

printf 'origin               git+ssh://git@github.com/o/r.git\n' >"$STUBDB/.remote_out"
_eq "a real remote is reported"        "$(_t remote | jq -r .configured)" "true"
_eq "…as the URL, not the column name" "$(_t remote | jq -r .remote)" "git+ssh://git@github.com/o/r.git"

printf 'origin  git@github.com:o/r.git\n' >"$STUBDB/.remote_out"
_eq "an scp-style remote is recognized too" "$(_t remote | jq -r .remote)" "git@github.com:o/r.git"

: >"$STUBDB/.remote_out"
_eq "empty output reports configured=false" "$(_t remote | jq -r .configured)" "false"

# =============================================================================================
echo
echo "the fold, directly — pure, no backend"

# Sourced so the fold can be exercised with hand-written comment streams: the shapes below are
# awkward to produce through a backend and are exactly where a wrong answer would be quiet.
BATON_TRACKER_TYPE=test
_reg_comments_json() { :; }
_reg_comment_add() { :; }
# shellcheck source=./tracker/lib-registry.sh
. "$LIB"

_fold() { printf '%s' "$1" | _reg_fold; }

# A bead with three branches over its life — the provenance case this feature was filed for.
THREE='[
 {"text":"{\"baton\":\"branch\",\"v\":1,\"repo\":\"h\",\"branch\":\"b1\",\"created\":\"2026-01-01T00:00:00Z\",\"status\":\"merged\"}","created_at":"2026-01-01T00:00:00Z"},
 {"text":"{\"baton\":\"branch\",\"v\":1,\"repo\":\"h\",\"branch\":\"b2\",\"created\":\"2026-01-02T00:00:00Z\",\"status\":\"merged\"}","created_at":"2026-01-02T00:00:00Z"},
 {"text":"{\"baton\":\"branch\",\"v\":1,\"repo\":\"h\",\"branch\":\"b3\",\"created\":\"2026-01-03T00:00:00Z\",\"status\":\"open\"}","created_at":"2026-01-03T00:00:00Z"}]'
_eq "three branches on one task stay three entries" "$(_fold "$THREE" | jq -r 'map(.branch)|join(",")')" "b1,b2,b3"
_eq "…ordered oldest first"                          "$(_fold "$THREE" | jq -r '.[0].branch')" "b1"

# Out-of-order arrival. A backend that hands back a differently ordered stream must not invert
# which entry wins — that would silently un-merge a merged branch.
OOO='[
 {"text":"{\"baton\":\"branch\",\"branch\":\"b\",\"repo\":\"r\",\"status\":\"merged\"}","created_at":"2026-01-09T00:00:00Z"},
 {"text":"{\"baton\":\"branch\",\"branch\":\"b\",\"repo\":\"r\",\"status\":\"open\",\"worktree\":\"/w\"}","created_at":"2026-01-01T00:00:00Z"}]'
_eq "the newest entry wins regardless of arrival order" "$(_fold "$OOO" | jq -r '.[0].status')" "merged"
_eq "…and the older entry's other fields survive"       "$(_fold "$OOO" | jq -r '.[0].worktree')" "/w"

# The same branch name in two repos: a repo-less update is documented to apply to both, and must
# never quietly pick one.
TWOREPO='[
 {"text":"{\"baton\":\"branch\",\"repo\":\"r1\",\"branch\":\"main-fix\",\"status\":\"open\"}","created_at":"2026-01-01T00:00:00Z"},
 {"text":"{\"baton\":\"branch\",\"repo\":\"r2\",\"branch\":\"main-fix\",\"status\":\"open\"}","created_at":"2026-01-02T00:00:00Z"},
 {"text":"{\"baton\":\"branch\",\"branch\":\"main-fix\",\"status\":\"merged\"}","created_at":"2026-01-03T00:00:00Z"}]'
_eq "one branch name in two repos stays two entries" "$(_fold "$TWOREPO" | jq -r length)" "2"
_eq "…and a repo-less update reaches both"           "$(_fold "$TWOREPO" | jq -r 'map(.status)|unique|join(",")')" "merged"

_eq "a stream with no registry entries folds to []" \
    "$(_fold '[{"text":"just talking","created_at":"2026-01-01T00:00:00Z"}]' | jq -c .)" "[]"
_eq "an entry with no branch is dropped — it names nothing to act on" \
    "$(_fold '[{"text":"{\"baton\":\"branch\",\"repo\":\"r\"}","created_at":"2026-01-01T00:00:00Z"}]' | jq -c .)" "[]"
_eq "an empty stream folds to []" "$(_fold '[]' | jq -c .)" "[]"

# =============================================================================================
echo
echo "option parsing — a value-taking flag given no value"

# These MUST terminate. `while [ $# -gt 0 ]` with `shift 2` and no `set -e` spins forever when the
# flag is the last argument: bash's `shift 2` on one positional fails and shifts NOTHING, so the
# loop never advances. Every one of these hung before the guard went in, and they are invoked
# from LLM-written snippets where an unset variable trivially produces the shape
# (`list --limit $LIMIT`). Bounded with an alarm so a regression fails the suite instead of
# hanging CI until it is killed.
if command -v perl >/dev/null 2>&1; then
  _timed() { perl -e 'alarm shift; exec @ARGV' 5 "$@" >/dev/null 2>&1; }
else
  _timed() { "$@" >/dev/null 2>&1; }   # no perl: still asserts the exit code, just unbounded
fi
_rc() { _timed "$@"; echo $?; }

_eq "tracker.sh --context with no value is a usage error, not a hang" \
    "$(_rc bash "$TRACKER" --context)" "1"
_eq "tracker.sh --tracker with no value is a usage error, not a hang" \
    "$(_rc bash "$TRACKER" --tracker)" "1"
_eq "tracker.sh --type with no value is a usage error, not a hang" \
    "$(_rc bash "$TRACKER" --type)" "1"
_eq "a backend verb flag with no value is a usage error, not a hang" \
    "$(STUB_DB="$STUBDB" PATH="$STUBBIN:$PATH" _rc bash "$TRACKER" --type beads --tracker "$TMP/d" list --limit)" "1"
_eq "update --description-file with no value is a usage error, not a hang" \
    "$(STUB_DB="$STUBDB" PATH="$STUBBIN:$PATH" _rc bash "$TRACKER" --type beads --tracker "$TMP/d" update task-1 --description-file)" "1"
_has "…and it names the offending option" \
    "$(bash "$TRACKER" --context 2>&1)" "needs a value"

# =============================================================================================
echo
echo "a resolved context is required before any bd call"

# An EMPTY tracker directory is not "use the default": bd falls back to discovering a .beads/
# under $PWD, so a failed context resolve silently redirects reads AND writes to whatever tracker
# happens to be beneath the caller. The check has to be up front — inside _bd() it is swallowed
# by `get`'s `$(...) || exit 3` and re-reported as NOT FOUND, a confident answer about a tracker
# that was never opened.
OUT="$(printf '%s' '{"task_tracking":{"type":"beads"}}' \
  | STUB_DB="$STUBDB" PATH="$STUBBIN:$PATH" bash "$TRACKER" --context - get anything 2>&1)"; RC=$?
_eq  "a context with no task_tracking.dir exits 1, not 3 (which would mean 'no such task')" "$RC" "1"
_has "…and says the tracker directory is the problem" "$OUT" "no tracker directory"

OUT="$(bash "$TRACKER" --context "$TMP/definitely-not-here.json" get x 2>&1)"; RC=$?
_eq  "an unreadable --context is a usage error" "$RC" "1"
_has "…naming the file" "$OUT" "not readable"

OUT="$(printf 'not json' | bash "$TRACKER" --context - get x 2>&1)"; RC=$?
_eq  "a non-JSON --context is a usage error" "$RC" "1"
_has "…saying so" "$OUT" "not valid JSON"

_eq "capabilities still answers with no tracker directory at all" \
    "$(printf '%s' '{"task_tracking":{"type":"beads"}}' | bash "$TRACKER" --context - capabilities | jq -r .type)" \
    "beads"

# =============================================================================================
echo
echo "status normalization covers bd's whole vocabulary"

# bd 1.1.0: open, in_progress, blocked, deferred, closed, pinned, hooked. The seam has no name
# for the last three, and `unknown` is NOT a safe place to put them — downstream it means the
# LOOKUP FAILED (cleanup-verdict.sh prints "[bead lookup FAILED — state unknown]"), so a
# deferred bead would be reported as a broken tracker on every cleanup and status run. They map
# to `open`: non-terminal, honest, and fail-safe, since an open task blocks a worktree removal.
for st in open in_progress blocked closed; do
  _mkissue "st-$st" "$st"
  _eq "status '$st' passes through" "$(_t get "st-$st" | jq -r .status)" "$st"
done
for st in deferred pinned hooked; do
  _mkissue "st-$st" "$st"
  _eq "status '$st' maps to open, never unknown" "$(_t get "st-$st" | jq -r .status)" "open"
done
_mkissue st-bogus wat
_eq "a status the seam has never heard of is still unknown" "$(_t get st-bogus | jq -r .status)" "unknown"

# =============================================================================================
echo
echo "edge types are translated to bd's vocabulary, not passed through"

# The seam says `relates-to`; bd says `related`. Passing the seam's spelling straight through was
# rejected by bd on every call, so a documented, validated verb could never once have succeeded.
_mkissue link-a open; _mkissue link-b open
_eq "link --type relates-to succeeds"       "$(_t link link-a link-b --type relates-to >/dev/null 2>&1; echo $?)" "0"
_eq "…and reaches bd as its own spelling"   "$(tail -1 "$STUBDB/.links" | awk '{print $3}')" "related"
_eq "link --type blocks is unchanged"       "$(_t link link-a link-b --type blocks >/dev/null 2>&1; tail -1 "$STUBDB/.links" | awk '{print $3}')" "blocks"
_eq "a type outside the seam vocabulary is refused" \
    "$(_t link link-a link-b --type tracks >/dev/null 2>&1; echo $?)" "1"

# The read side maps back, and anything bd grows that the seam has no name for reads as
# relates-to rather than leaking a backend-specific string to callers that filter on type.
cat >"$STUBDB/dep-src.deps" <<'DEPS'
[{"id":"d1","title":"t1","status":"open","dependency_type":"related"},
 {"id":"d2","title":"t2","status":"deferred","dependency_type":"tracks"},
 {"id":"d3","title":"t3","status":"closed","dependency_type":"blocks"}]
DEPS
_mkissue dep-src open
_eq "bd's 'related' reads back as the seam's 'relates-to'" \
    "$(_t deps dep-src | jq -r '.[0].type')" "relates-to"
_eq "an edge type the seam has no name for reads as relates-to" \
    "$(_t deps dep-src | jq -r '.[1].type')" "relates-to"
_eq "…and blocks survives, since callers filter on it" \
    "$(_t deps dep-src | jq -r '.[2].type')" "blocks"
_eq "edge status is normalized by the same rules as a task" \
    "$(_t deps dep-src | jq -r '.[1].status')" "open"

# =============================================================================================
echo
echo "write verbs"

NEW="$(_t create "a new task" --description "the WRONG body" --labels "baton,architecture" --priority 1)"
_eq "create prints ONLY an id — callers capture it straight into \$LEAF" \
    "$(printf '%s' "$NEW" | wc -l | tr -d ' ')" "0"
_has "…and it looks like an id" "$NEW" "stub-"
_eq "…with the labels it was given" "$(_t get "$NEW" | jq -r '.labels|join(",")')" "baton,architecture"

_t update "$NEW" --status in_progress
_eq "update --status takes"        "$(_t get "$NEW" | jq -r .status)" "in_progress"
_t claim "$NEW"
_eq "claim sets in_progress"       "$(_t get "$NEW" | jq -r .status)" "in_progress"
_t close "$NEW" --reason "done in a test"
_eq "close takes"                  "$(_t get "$NEW" | jq -r .status)" "closed"
_eq "…and keeps the reason"        "$(_t get "$NEW" | jq -r .close_reason)" "done in a test"
_t reopen "$NEW"
_eq "reopen takes"                 "$(_t get "$NEW" | jq -r .status)" "open"
_eq "close with no --reason is refused" "$(_t close "$NEW" >/dev/null 2>&1; echo $?)" "1"
_eq "update with nothing to change is refused" "$(_t update "$NEW" >/dev/null 2>&1; echo $?)" "1"

# REVISING TEXT. The case that forced this in: a task captured with a wrong acceptance criterion
# in its body. `note` appends, so the wrong text would have stayed above the correction; the
# only fix was a replacement task and a superseded tombstone. `update --title/--description`
# REPLACES — that is the whole point — so assert the old text is gone, not just the new present.
_t update "$NEW" --title "a corrected title"
_eq "update --title replaces the title" "$(_t get "$NEW" | jq -r .title)" "a corrected title"
_t update "$NEW" --description "the corrected body"
_eq "update --description replaces the body"        "$(_t get "$NEW" | jq -r .description)" "the corrected body"
_eq "…and the old text is gone, not appended to"    "$(_t get "$NEW" | jq -r '.description | contains("WRONG")')" "false"
printf 'line one\nline two\n' >"$TMP/body.txt"
_t update "$NEW" --description-file "$TMP/body.txt"
_eq "update --description-file reads the body from a file" \
    "$(_t get "$NEW" | jq -r .description)" "$(printf 'line one\nline two')"
printf 'from stdin\n' | _t update "$NEW" --description-file -
_eq "update --description-file - reads the body from stdin" "$(_t get "$NEW" | jq -r .description)" "from stdin"
_t update "$NEW" --title "both at once" --description "both at once body" --priority 3
_eq "text and non-text fields update in one call" \
    "$(_t get "$NEW" | jq -r '[.title, .description, (.priority|tostring)] | join("|")')" \
    "both at once|both at once body|3"
# An empty replacement is a blanked task, not a correction; refuse it before bd sees it, and
# refuse an unreadable file the same way, so a typo'd path cannot become an empty body either.
_eq "update --description '' is refused (never blank a body)" \
    "$(_t update "$NEW" --description "" >/dev/null 2>&1; echo $?)" "1"
_eq "update --description-file with an empty file is refused" \
    "$(: >"$TMP/empty.txt"; _t update "$NEW" --description-file "$TMP/empty.txt" >/dev/null 2>&1; echo $?)" "1"
_eq "update --description-file with a missing file is refused" \
    "$(_t update "$NEW" --description-file "$TMP/nope.txt" >/dev/null 2>&1; echo $?)" "1"
_eq "update --title '' is refused" "$(_t update "$NEW" --title "" >/dev/null 2>&1; echo $?)" "1"
_eq "--description and --description-file together is a usage error" \
    "$(_t update "$NEW" --description x --description-file "$TMP/body.txt" >/dev/null 2>&1; echo $?)" "1"
_eq "…and none of the refusals touched the task" \
    "$(_t get "$NEW" | jq -r '[.title, .description] | join("|")')" "both at once|both at once body"
_eq "capabilities names the fields update can change — a caller checks it before relying on one" \
    "$(_t capabilities | jq -r '.update_fields | join(",")')" "status,priority,title,description"

# `--label` needs `--all` in bd to reach past the default status filter. baton:whereami counts
# ready-for-worktree-delete across CLOSED beads, which is most of them — without --all it reports
# zero and the cleanup summary silently under-counts.
_mkissue lbl-closed closed
jq -c '.labels=["ready-for-worktree-delete"]' "$STUBDB/lbl-closed.json" >"$STUBDB/lbl-closed.tmp" \
  && mv "$STUBDB/lbl-closed.tmp" "$STUBDB/lbl-closed.json"
_eq "list --label reaches closed tasks (bd needs --all for that)" \
    "$(_t list --label ready-for-worktree-delete | jq -r 'map(.id)|join(",")')" "lbl-closed"
# `all` on an empty array is true, so assert both halves — otherwise a list that returns nothing
# passes this vacuously, which is exactly how a broken filter would look.
_eq "list --status closed returns something"  "$(_t list --status closed | jq -r 'length > 0')" "true"
_eq "…and all of it is closed"                "$(_t list --status closed | jq -r 'all(.status == "closed")')" "true"
_eq "list --limit truncates"              "$(_t list --status open --limit 2 | jq -r length)" "2"
_eq "ready returns open tasks as an array" "$(_t ready | jq -r 'type')" "array"
_eq "a verb this backend lacks exits 4, not 1" "$(_t show x >/dev/null 2>&1; echo $?)" "4"

# =============================================================================================
echo
echo "the registry does not carry a finished worktree's readiness onto a new one"

# The failure this guards: one still-open leaf, two worktrees over its life. The slug is derived
# from the bead's own title, so the second worktree plausibly regenerates the SAME branch name
# (and a naming.branch template with no slug component makes it certain). Folding the new entry
# onto the old group inherited ready=yes and keep_task_open=yes — together, exactly the signal
# baton:cleanup-worktrees auto-removes a worktree on, with no prompt, while the reason line
# claimed the readiness was recorded against THIS branch.
_mkissue task-epoch open
_t record-branch task-epoch --repo maestro --branch reused --worktree /wt/A --created 2026-03-01T00:00:00Z
_t update-branch task-epoch reused status=merged ready=yes keep_task_open=yes pr=99
E="$(_t list-branches task-epoch | jq -c '.[0]')"
_eq "the first worktree folds to ready"       "$(printf '%s' "$E" | jq -r .ready)" "yes"

_t record-branch task-epoch --repo maestro --branch reused --worktree /wt/B --created 2026-03-09T00:00:00Z
E="$(_t list-branches task-epoch | jq -c '.[0]')"
_eq "re-recording the branch keeps ONE entry"        "$(_t list-branches task-epoch | jq -r length)" "1"
_eq "…pointing at the new worktree"                  "$(printf '%s' "$E" | jq -r .worktree)" "/wt/B"
_eq "…with ready NOT inherited"                      "$(printf '%s' "$E" | jq -r '.ready // "absent"')" "absent"
_eq "…nor keep_task_open"                            "$(printf '%s' "$E" | jq -r '.keep_task_open // "absent"')" "absent"
_eq "…nor the old PR number"                         "$(printf '%s' "$E" | jq -r '.pr // "absent"')" "absent"
_eq "…back to an open status"                        "$(printf '%s' "$E" | jq -r .status)" "open"
_eq "…and the creation time is the NEW worktree's"   "$(printf '%s' "$E" | jq -r .created)" "2026-03-09T00:00:00Z"
_eq "…with revisions counted from the new epoch"     "$(printf '%s' "$E" | jq -r .revisions)" "1"

_t update-branch task-epoch reused status=merged ready=yes
_eq "a partial update after the reset still overlays" \
    "$(_t list-branches task-epoch | jq -r '.[0].ready')" "yes"
_eq "…without disturbing the worktree path" \
    "$(_t list-branches task-epoch | jq -r '.[0].worktree')" "/wt/B"

# Entries written before the epoch marker existed must keep their old meaning rather than
# silently collapsing to a single revision.
LEGACY='[
 {"text":"{\"baton\":\"branch\",\"v\":1,\"repo\":\"m\",\"branch\":\"b\",\"worktree\":\"/wt/A\",\"created\":\"2026-01-01T00:00:00Z\",\"status\":\"open\"}","created_at":"2026-01-01T00:00:00Z"},
 {"text":"{\"baton\":\"branch\",\"v\":1,\"branch\":\"b\",\"status\":\"merged\",\"ready\":\"yes\"}","created_at":"2026-01-02T00:00:00Z"}]'
_eq "a stream with no epoch marker folds whole, as before" "$(_fold "$LEGACY" | jq -r '.[0].ready')" "yes"
_eq "…keeping its full revision count"                     "$(_fold "$LEGACY" | jq -r '.[0].revisions')" "2"

# =============================================================================================
echo
printf 'tracker seam: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
