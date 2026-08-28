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

OUT="$(bash "$TRACKER" --context - --tracker "$TMP/x" capabilities 2>/dev/null \
        <<<'{"task_tracking":{"type":"beads","dir":"/nope"}}' | jq -r .type)"
_eq "type comes from the context when --type is absent" "$OUT" "beads"

# An old context.yaml with no `type` at all must keep working — that is every context written
# before this seam existed.
OUT="$(bash "$TRACKER" --context - --tracker "$TMP/x" capabilities 2>/dev/null \
        <<<'{"task_tracking":{"dir":"/nope"}}' | jq -r .type)"
_eq "a context with no task_tracking.type defaults to beads" "$OUT" "beads"

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
printf 'tracker seam: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
