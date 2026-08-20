#!/usr/bin/env bash
# baton — tests for scripts/cleanup-verdict.sh and the merge-state signals it consumes.
#
# baton:cleanup-worktrees removes a worktree and deletes its branch on this verdict, so the
# rules are pinned here rather than left to a careful reading of the skill's prose. The three
# shapes that matter are indistinguishable to git and must NOT be treated alike:
#
#   c6l shape        work done, but deliberately outside git (an API against a live system):
#                    zero commits, no PR, bead closed and labeled  -> MUST be cleanable
#   fresh-unstarted  a worktree nobody has committed to yet, bead open and unlabeled
#                    -> MUST be kept; this is the regression that matters most
#   real work        commits the base does not have -> MUST never be removed, label or not
#
# The first two produce byte-identical merge-state output (asserted below). That is the whole
# problem: only the deliberate `no-pr-needed` assertion plus the bead's own state separates them,
# which is why the label exists and why it is guarded by has_work.
#
# Hermetic: builds throwaway git repos under a temp dir, stubs `gh` so nothing touches the
# network, touches nothing outside the temp dir, and needs only git and jq. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
VERDICT="$HERE/cleanup-verdict.sh"
MS="$HERE/merge-state.sh"
for f in "$VERDICT" "$MS"; do [ -r "$f" ] || { echo "not found: $f" >&2; exit 2; }; done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { # $1 = what, $2 = got, $3 = want
  if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi
}

# A `gh` that answers "this branch has no PR" — the c6l situation, and offline by construction.
# Without it merge-state.sh would shell out to the real gh against a repo with no remote, making
# the result depend on the machine's network and login state.
STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
cat >"$STUBBIN/gh" <<'EOF'
#!/usr/bin/env bash
echo "no pull requests found for branch" >&2
exit 1
EOF
chmod +x "$STUBBIN/gh"

_ms() { # $1 = repo, $2 = branch -> prints "merged/has_work"
  local out
  out="$(PATH="$STUBBIN:$PATH" bash "$MS" --repo "$1" --branch "$2" --no-fetch --format env 2>/dev/null)"
  ( eval "$out"; printf '%s/%s' "${MERGED:-?}" "${HAS_WORK:-?}" )
}

_v() { # verdict flags... -> prints "verdict|relaxed"
  bash "$VERDICT" "$@" | jq -r '.verdict + "|" + .relaxed'
}

_repo() { # $1 = name -> prints its path, with one commit and origin/main pointing at it
  local r="$TMP/$1"
  git init -q "$r"
  git -C "$r" config user.email t@t.t; git -C "$r" config user.name T
  : >"$r/f"; git -C "$r" add f; git -C "$r" commit -qm init
  git -C "$r" branch -M main          # don't inherit the machine's init.defaultBranch
  git -C "$r" update-ref refs/remotes/origin/main HEAD
  printf '%s' "$r"
}

_commit() { # $1 = repo, $2 = branch, $3 = message
  git -C "$1" checkout -q "$2"
  echo "$3" >>"$1/f"; git -C "$1" commit -qam "$3"
}

# ============================================================================================
echo "merge-state signals for the shapes that look alike"

# --- the c6l shape: branch cut, nothing ever committed on it, base moved on without it --------
R="$(_repo c6l)"
git -C "$R" branch nothing-to-merge
_commit "$R" main "someone else's work"
git -C "$R" update-ref refs/remotes/origin/main main
_eq "c6l shape: no commits of its own, base moved on -> merged=no, has_work=no" \
    "$(_ms "$R" nothing-to-merge)" "no/no"

# --- fresh-unstarted: git cannot tell it from the c6l shape ------------------------------------
git -C "$R" branch fresh-unstarted
_eq "fresh-unstarted shape: identical merge-state output (this is the ambiguity)" \
    "$(_ms "$R" fresh-unstarted)" "$(_ms "$R" nothing-to-merge)"

# --- real unmerged work -------------------------------------------------------------------------
git -C "$R" branch real-work
_commit "$R" real-work "a commit the base does not have"
git -C "$R" checkout -q main
_eq "real work: merged=no, has_work=yes" "$(_ms "$R" real-work)" "no/yes"

# --- merged by a merge commit (the ordinary path) -------------------------------------------------
git -C "$R" branch landed
_commit "$R" landed "work that will land"
git -C "$R" checkout -q main
git -C "$R" merge -q --no-ff -m "merge landed" landed
git -C "$R" update-ref refs/remotes/origin/main main
_eq "merged via merge commit: merged=yes, has_work=no" "$(_ms "$R" landed)" "yes/no"

# --- fast-forwarded with no PR: the documented limit the label exists to resolve --------------------
git -C "$R" branch ff-landed
_commit "$R" ff-landed "work that will fast-forward"
git -C "$R" checkout -q main
git -C "$R" merge -q --ff-only ff-landed
git -C "$R" update-ref refs/remotes/origin/main main
_eq "fast-forwarded, no PR: reported merged=no on purpose (bias toward not-merged)" \
    "$(_ms "$R" ff-landed)" "no/no"

# ============================================================================================
echo "cleanup-verdict — the three shapes end to end"

READY=ready-for-worktree-delete

_eq "c6l shape + closed + labeled + no-pr-needed -> confirmed-ready, merge relaxed" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work no --dirty no)" \
    "confirmed-ready|merged"

_eq "fresh-unstarted (open, unlabeled), same git signals -> KEPT" \
    "$(_v --labels "" --state open --merged no --has-work no --dirty no)" \
    "not-ready|"

_eq "fresh-unstarted, in_progress -> KEPT" \
    "$(_v --labels "" --state in_progress --merged no --has-work no --dirty no)" \
    "not-ready|"

_eq "real unmerged work + BOTH labels -> mismatch, never removable" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work yes --dirty no)" \
    "label-state-mismatch|"

_eq "real unmerged work, unlabeled -> KEPT" \
    "$(_v --labels "" --state open --merged no --has-work yes --dirty no)" \
    "not-ready|"

# ============================================================================================
echo "the guard: no-pr-needed relaxes MERGED and nothing else"

_eq "no-pr-needed + dirty tree -> mismatch (uncommitted work must not be swept)" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work no --dirty yes)" \
    "label-state-mismatch|"

_eq "no-pr-needed + has_work=unknown -> mismatch (an unknown is not a 'no')" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work unknown --dirty no)" \
    "label-state-mismatch|"

_eq "no-pr-needed + merged=unknown (helper missing) -> mismatch" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged unknown --has-work no --dirty no)" \
    "label-state-mismatch|"

# The merge cross-check is legitimately relaxed here (and `relaxed` says so), but STATE is red
# and no label covers it — proving the relaxation is scoped to MERGED alone.
_eq "no-pr-needed does NOT relax STATE: open bead, no keep-task-open -> mismatch" \
    "$(_v --labels "$READY no-pr-needed" --state open --merged no --has-work no --dirty no)" \
    "label-state-mismatch|merged"
_eq "…and the reason names the open bead as what disagreed" \
    "$(bash "$VERDICT" --labels "$READY no-pr-needed" --state open --merged no --has-work no --dirty no \
       | jq -r '.reason | test("bead is open") | tostring')" "true"

_eq "no-pr-needed + keep-task-open relaxes both, and says so" \
    "$(_v --labels "$READY no-pr-needed keep-task-open" --state open --merged no --has-work no --dirty no)" \
    "confirmed-ready|state,merged"

_eq "a bare 'no' merge with no label is still not merged" \
    "$(_v --labels "$READY" --state closed --merged no --has-work no --dirty no)" \
    "label-state-mismatch|"

# ============================================================================================
echo "pre-existing buckets are unchanged"

_eq "all four signals agree -> confirmed-ready, nothing relaxed" \
    "$(_v --labels "$READY" --state closed --merged yes --has-work no --dirty no)" \
    "confirmed-ready|"

_eq "keep-task-open covers an open bead" \
    "$(_v --labels "$READY keep-task-open" --state open --merged yes --has-work no --dirty no)" \
    "confirmed-ready|state"

_eq "closed + merged + clean but unlabeled -> looks-done-unlabeled" \
    "$(_v --labels "" --state closed --merged yes --has-work no --dirty no)" \
    "looks-done-unlabeled|"

_eq "labeled but reopened -> mismatch" \
    "$(_v --labels "$READY" --state open --merged yes --has-work no --dirty no)" \
    "label-state-mismatch|"

_eq "labeled but dirty -> mismatch" \
    "$(_v --labels "$READY" --state closed --merged yes --has-work no --dirty yes)" \
    "label-state-mismatch|"

_eq "a failed bead lookup can never be confirmed-ready" \
    "$(_v --labels "$READY" --state unknown --merged yes --has-work no --dirty no)" \
    "label-state-mismatch|"
_eq "…and it says the lookup failed rather than implying the bead is merely open" \
    "$(bash "$VERDICT" --labels "$READY" --state unknown --merged yes --has-work no --dirty no \
       | jq -r '.reason | test("bead lookup FAILED") | tostring')" "true"

# ============================================================================================
echo "input hygiene"

_eq "no flags at all -> everything unknown -> not-ready" \
    "$(_v)" "not-ready|"

_eq "a label that merely CONTAINS a known one does not count" \
    "$(_v --labels "assume-no-pr-needed $READY" --state closed --merged no --has-work no --dirty no)" \
    "label-state-mismatch|"

_eq "bd's bullet output is accepted verbatim" \
    "$(_v --labels "$(printf '🏷 Labels for jbh-c6l:\n  - cleanup\n  - %s\n  - no-pr-needed\n' "$READY")" \
         --state closed --merged no --has-work no --dirty no)" \
    "confirmed-ready|merged"

_eq "an out-of-range --merged is a usage error, not a silent pass" \
    "$(bash "$VERDICT" --merged maybe >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"

_eq "not-ready still exits 0 (a red signal is an answer, not a failure)" \
    "$(bash "$VERDICT" >/dev/null 2>&1 && echo 0 || echo nonzero)" "0"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
