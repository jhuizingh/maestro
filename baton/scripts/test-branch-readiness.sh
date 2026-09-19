#!/usr/bin/env bash
# baton — tests for scripts/branch-readiness.sh: which registry entry is THIS worktree's, and
# what it says about readiness.
#
# baton:cleanup-worktrees removes a worktree partly on this answer, and baton:status reports it,
# so the rule is pinned here rather than pasted into two skills. The bug this file exists for:
# the registry stores the member repo's NAME (`baton:start` records it that way) while the
# skills held the repo's PATH, and for a whole release the paste compared one to the other —
# never matching, and silently falling through to the task-label fallback for every worktree
# started since 0.8.0 (jbh-7xb8). The round trip at the bottom is the regression test: an entry
# recorded through the real tracker seam with a NAME must be found when the reader holds a PATH.
#
# Hermetic: needs only bash and jq; the round trip stubs `bd` the same way test-tracker.sh does.
# Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BRR="$HERE/branch-readiness.sh"
TRACKER="$HERE/tracker.sh"
for f in "$BRR" "$TRACKER"; do [ -r "$f" ] || { echo "not found: $f" >&2; exit 2; }; done
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }

# $1 = registry JSON, then the script's flags -> prints "scope|labels|entry_found|branches"
_r() {
  local regs="$1"; shift
  bash "$BRR" "$@" <<<"$regs" \
    | jq -r '[.label_scope, .labels, .entry_found, (.branches|tostring)] | join("|")'
}

echo "== the key: NAME stored, PATH held"
R='[{"repo":"homebernetes","branch":"b1","status":"merged","pr":"258","ready":"yes"}]'
_eq "path on the reader side matches a name in the entry (the jbh-7xb8 shape)" \
    "$(_r "$R" --branch b1 --repo /Users/jh/code/homebernetes)" \
    "branch|ready-for-worktree-delete|yes|1"
_eq "a bare name matches too" \
    "$(_r "$R" --branch b1 --repo homebernetes)" "branch|ready-for-worktree-delete|yes|1"
_eq "a trailing slash on the path is ignored" \
    "$(_r "$R" --branch b1 --repo /Users/jh/code/homebernetes/)" "branch|ready-for-worktree-delete|yes|1"
_eq "a different repo of the same branch name does NOT match" \
    "$(_r "$R" --branch b1 --repo /Users/jh/code/maestro --task-labels "")" "bead||no|1"
_eq "a different branch does NOT match" \
    "$(_r "$R" --branch b2 --repo homebernetes)" "bead||no|1"
_eq "REPO_KEY reports the name the match was made on" \
    "$(bash "$BRR" --branch b1 --repo /x/y/homebernetes --format env <<<"$R" | grep REPO_KEY)" \
    "export REPO_KEY='homebernetes'"

echo "== transition: entries written in either form still match"
_eq "an entry that stored a PATH matches a reader holding the name" \
    "$(_r '[{"repo":"/Users/x/code/homebernetes","branch":"b1","ready":"yes","keep_task_open":"yes"}]' \
          --branch b1 --repo homebernetes)" \
    "branch|ready-for-worktree-delete keep-task-open|yes|1"
_eq "…and a reader holding a different path to the same repo" \
    "$(_r '[{"repo":"/Users/x/code/homebernetes","branch":"b1","ready":"yes"}]' \
          --branch b1 --repo /home/y/src/homebernetes)" \
    "branch|ready-for-worktree-delete|yes|1"
_eq "an entry with no repo at all (update-branch with no prior record-branch) matches" \
    "$(_r '[{"repo":null,"branch":"b1","status":"merged","ready":"yes"}]' --branch b1 --repo /a/b)" \
    "branch|ready-for-worktree-delete|yes|1"
_eq "…also when the key is simply absent" \
    "$(_r '[{"branch":"b1","ready":"yes"}]' --branch b1 --repo /a/b)" \
    "branch|ready-for-worktree-delete|yes|1"

echo "== the three scopes"
_eq "readiness recorded -> branch, labels spelled the bead way" \
    "$(_r '[{"repo":"m","branch":"b1","ready":"yes","keep_task_open":"yes","no_pr_needed":"yes"}]' \
          --branch b1 --repo m --task-labels "unrelated")" \
    "branch|ready-for-worktree-delete keep-task-open no-pr-needed|yes|1"
_eq "ready=no is still RECORDED readiness, and yields no label" \
    "$(_r '[{"repo":"m","branch":"b1","ready":"no"}]' --branch b1 --repo m --task-labels "ready-for-worktree-delete")" \
    "branch||yes|1"
_eq "entry exists but records no readiness, one branch on record -> bead, task labels pass through" \
    "$(_r '[{"repo":"m","branch":"b1","status":"open"}]' --branch b1 --repo m --task-labels "ready-for-worktree-delete keep-task-open")" \
    "bead|ready-for-worktree-delete keep-task-open|yes|1"
_eq "no entry at all, empty registry -> bead (pre-0.8.0 worktree / no registry backend)" \
    "$(_r '[]' --branch b1 --repo m --task-labels "ready-for-worktree-delete")" \
    "bead|ready-for-worktree-delete|no|0"
_eq "empty stdin reads as an empty registry" \
    "$(_r '' --branch b1 --repo m --task-labels "x")" "bead|x|no|0"
_eq "non-array stdin reads as an empty registry" \
    "$(_r '{"oops":1}' --branch b1 --repo m --task-labels "x")" "bead|x|no|0"
_eq "no readiness for THIS branch while the task owns several -> branch-unrecorded, labels NOT borrowed" \
    "$(_r '[{"repo":"m","branch":"b1","status":"open"},{"repo":"m","branch":"b0","ready":"yes","keep_task_open":"yes"}]' \
          --branch b1 --repo m --task-labels "ready-for-worktree-delete keep-task-open")" \
    "branch-unrecorded||yes|2"
_eq "…the sibling that DID record readiness still reads as branch" \
    "$(_r '[{"repo":"m","branch":"b1","status":"open"},{"repo":"m","branch":"b0","ready":"yes","keep_task_open":"yes"}]' \
          --branch b0 --repo m --task-labels "ready-for-worktree-delete keep-task-open")" \
    "branch|ready-for-worktree-delete keep-task-open|yes|2"
_eq "several branches counts across repos: same name in two repos, this one unrecorded" \
    "$(_r '[{"repo":"m","branch":"b1","status":"open"},{"repo":"n","branch":"b1","ready":"yes"}]' \
          --branch b1 --repo /code/m --task-labels "ready-for-worktree-delete")" \
    "branch-unrecorded||yes|2"
_eq "…and the other repo reads its own entry, not this one's" \
    "$(_r '[{"repo":"m","branch":"b1","status":"open"},{"repo":"n","branch":"b1","ready":"yes"}]' \
          --branch b1 --repo /code/n)" \
    "branch|ready-for-worktree-delete|yes|2"

echo "== env format"
OUT="$(bash "$BRR" --branch b1 --repo /x/m --task-labels "a b" --format env \
        <<<'[{"repo":"m","branch":"b1","status":"open"}]')"
eval "$OUT"
_eq "env: LABEL_SCOPE"  "$LABEL_SCOPE" "bead"
_eq "env: LABELS"       "$LABELS" "a b"
_eq "env: REG_FOUND"    "$REG_FOUND" "yes"
_eq "env: NBR"          "$NBR" "1"
_eq "env: REG_ENTRY is the entry as JSON" "$(jq -r .status <<<"$REG_ENTRY")" "open"
eval "$(bash "$BRR" --branch zz --repo m --format env <<<'[]')"
_eq "env: REG_ENTRY is empty when nothing matched" "$REG_ENTRY" ""
_eq "env: NBR is 0 for an empty registry" "$NBR" "0"

echo "== arguments"
bash "$BRR" --repo m <<<'[]' >/dev/null 2>&1; _eq "missing --branch exits 2" "$?" "2"
bash "$BRR" --branch b <<<'[]' >/dev/null 2>&1; _eq "missing --repo exits 2" "$?" "2"
bash "$BRR" --branch b --repo m --format yaml <<<'[]' >/dev/null 2>&1; _eq "bad --format exits 2" "$?" "2"

# =============================================================================================
# THE ROUND TRIP. record-branch through the real seam with the repo's NAME (exactly as
# baton:start does), then read it back the way baton:cleanup-worktrees does, holding the PATH.
# This is the exact pairing that was broken. Uses the same file-backed `bd` stub shape as
# test-tracker.sh, for the two verbs the trip needs.
# =============================================================================================
echo "== round trip: record-branch --repo <name> … read with --repo <path>"
STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
STUBDB="$TMP/db"; mkdir -p "$STUBDB"
cat >"$STUBBIN/bd" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail
DB="${STUB_DB:?}"
cmd="${1:-}"; shift || true
case "$cmd" in
  show)     id="${1:-}"; [ -f "$DB/$id.json" ] || { echo "not found" >&2; exit 1; }; jq -c '[.]' "$DB/$id.json" ;;
  comments) id="${1:-}"; if [ -f "$DB/$id.comments" ]; then jq -s -c . "$DB/$id.comments"; else echo '[]'; fi ;;
  comment)  id="${1:-}"; shift
            n=$(( $(wc -l <"$DB/$id.comments" 2>/dev/null || echo 0) + 1 ))
            jq -cn --arg t "$1" --arg at "$(printf '2026-01-01T00:00:%02dZ' "$n")" '{text:$t, created_at:$at}' >>"$DB/$id.comments" ;;
  *) echo "stub bd: unsupported: $cmd $*" >&2; exit 1 ;;
esac
STUB
chmod +x "$STUBBIN/bd"
jq -n '{id:"task-rt", title:"round trip", status:"open"}' >"$STUBDB/task-rt.json"

CTX='{"task_tracking":{"type":"beads","dir":"'"$TMP"'/beads"},"_workspace":"'"$TMP"'"}'
_t() { PATH="$STUBBIN:$PATH" STUB_DB="$STUBDB" bash "$TRACKER" --context - "$@" <<<"$CTX"; }

_t record-branch task-rt --repo homebernetes --branch jbh-1-thing --worktree /Users/jh/code/homebernetes-worktrees/jbh-1-thing >/dev/null 2>&1
_eq "record-branch stored the NAME" "$(_t list-branches task-rt | jq -r '.[0].repo')" "homebernetes"
_eq "a fresh entry, read by path, is found but records no readiness -> bead" \
    "$(_t list-branches task-rt | bash "$BRR" --branch jbh-1-thing --repo /Users/jh/code/homebernetes --task-labels "" \
        | jq -r '[.label_scope, .entry_found] | join("|")')" \
    "bead|yes"
_t update-branch task-rt jbh-1-thing status=merged ready=yes pr=258 >/dev/null 2>&1
_eq "after finish's update-branch, read by PATH, readiness is branch-scoped (was: bead, every time)" \
    "$(_t list-branches task-rt | bash "$BRR" --branch jbh-1-thing --repo /Users/jh/code/homebernetes \
        | jq -r '[.label_scope, .labels, .entry.pr] | join("|")')" \
    "branch|ready-for-worktree-delete|258"
_eq "…and by name" \
    "$(_t list-branches task-rt | bash "$BRR" --branch jbh-1-thing --repo homebernetes | jq -r .label_scope)" \
    "branch"

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
